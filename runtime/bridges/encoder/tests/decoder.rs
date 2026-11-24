// SPDX-License-Identifier: MIT
//! The video decoder (2026-11-24): xnuhost's "droidvm,virtio-media" device decoding, over xh-core's real transport,
//! driven by a guest of plain memory ([`oneencoder::encctl::Guest`]) replaying the V4L2 decoder sequence of the staged
//! DroidVMVTHost.cpp (startSession, setUpCapture, the worker's requeue, a dfin's drain and start): ENUM_FMT and the
//! card, ENUM_FRAMESIZES, S_FMT and REQBUFS of OUTPUT, STREAMON (the decoder opens), the SOURCE_CHANGE, G_FMT, G_CTRL,
//! REQBUFS and QBUF of CAPTURE, the frames, the pictures in the guest's CAPTURE buffers, a drain to its EOS, a start
//! after it, CLOSE, every pool buffer given back, the counters. Through the oneios-encoder helper process (the phone's
//! arrangement: the pool is a memfd the helper maps, the stand-in decoder in MediaCodec's place) and with the stand-in
//! inside the device's process (xnuhost's PC boots). Then decoder=off: the device is an encoder only.
use oneencoder::backend::Backend;
use oneencoder::backend_helper::HelperBackend;
use oneencoder::backend_stub::StubBackend;
use oneencoder::decoder::{DecodeBackend, StubDecodeBackend};
use oneencoder::encctl::{querybuf, Guest};
use oneencoder::pod::Pod;
use oneencoder::proto::*;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;
use xh_core::bytes::le;

const W: u32 = 640;
const H: u32 = 360;
const POOL: u64 = 32 << 20; // encctl's Guest pool

/// The events until `want` says one is the last: (kind, the v4l2 type or event type, index, bytesused, sec, flags).
fn events(
    g: &mut Guest,
    mut want: impl FnMut(&[(u32, u32, u32, u32, i64, u32)]) -> bool,
) -> Vec<(u32, u32, u32, u32, i64, u32)> {
    let mut got = Vec::new();
    while !want(&got) {
        let (kind, _, ev) = g.wait_event(Duration::from_secs(3)).unwrap_or_else(|| panic!("an event after {got:?}"));
        if kind == MEDIA_EVT_DQBUF {
            let b = MediaV4l2Buffer::read_from(&ev[8..]);
            got.push((kind, b.type_, b.index, b.plane.bytesused, b.sec, b.flags));
        } else {
            got.push((kind, le::u32(&ev, 8), 0, 0, 0, 0));
        }
    }
    got
}

fn captures(got: &[(u32, u32, u32, u32, i64, u32)]) -> Vec<(u32, u32, i64)> {
    got.iter()
        .filter(|e| e.0 == MEDIA_EVT_DQBUF && e.1 == MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE)
        .map(|e| (e.2, e.3, e.4))
        .collect()
}

fn eos(got: &[(u32, u32, u32, u32, i64, u32)]) -> bool {
    got.iter().any(|e| e.0 == MEDIA_EVT_EVENT && e.1 == MEDIA_EVENT_EOS)
}

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

fn card(g: &Guest) -> String {
    let bytes: Vec<u8> = (0..8).flat_map(|i| (g.dev.mmio_read(0x108 + 4 * i, 4) as u32).to_le_bytes()).collect();
    String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string()
}

/// One decoder session of `codec`, as the staged DroidVMVTHost.cpp runs it, over the guest's device.
fn run(g: &mut Guest, codec: u32) {
    assert_eq!(card(g), "oneios-encoder+decoder");
    let session = g.open().expect("OPEN");

    // ENUM_FMT: OUTPUT takes the encoder's NV12 and P010 and the four codecs; CAPTURE gives HEVC, H.264 and NV12.
    let list = |g: &mut Guest, kind: u32| {
        (0..16)
            .map_while(|index| {
                let mut fd = [0u32; 16];
                (fd[0], fd[1]) = (index, kind);
                (g.ioctl(session, MEDIA_VIDIOC_ENUM_FMT, fd.as_bytes_mut()) == 0).then_some(fd[11])
            })
            .collect::<Vec<_>>()
    };
    let outputs = list(g, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
    assert_eq!(
        outputs,
        [
            MEDIA_PIXFMT_NV12,
            MEDIA_PIXFMT_P010,
            MEDIA_PIXFMT_H264,
            MEDIA_PIXFMT_HEVC,
            MEDIA_PIXFMT_VP9,
            MEDIA_PIXFMT_AV1
        ]
    );
    assert_eq!(list(g, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE), [MEDIA_PIXFMT_HEVC, MEDIA_PIXFMT_H264, MEDIA_PIXFMT_NV12]);

    // ENUM_FRAMESIZES (decodableSize): stepwise 96..4096 both ways; the encoder's formats have none.
    let mut sizes = [0u32, codec, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_ENUM_FRAMESIZES, sizes.as_bytes_mut()), 0);
    assert_eq!(sizes[2..9], [MEDIA_FRMSIZE_TYPE_STEPWISE, 96, 4096, 2, 96, 4096, 2]);
    let mut none = [0u32, MEDIA_PIXFMT_NV12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_ne!(g.ioctl(session, MEDIA_VIDIOC_ENUM_FRAMESIZES, none.as_bytes_mut()), 0);
    let mut subscription = [MEDIA_EVENT_SOURCE_CHANGE, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_SUBSCRIBE_EVENT, subscription.as_bytes_mut()), 0);

    // S_FMT OUTPUT in the codec (the guest asks 2 MiB a buffer), REQBUFS 4, QUERYBUF and MMAP each.
    let mut out = MediaV4l2Format {
        type_: MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        width: W,
        height: H,
        pixelformat: codec,
        num_planes: 1,
        ..Default::default()
    };
    out.planes[0].sizeimage = 2 << 20;
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_S_FMT, out.as_bytes_mut()), 0);
    assert_eq!(out.planes[0].sizeimage, 2 << 20);
    let mut req = [4, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE, MEDIA_MEMORY_MMAP, 0, 0];
    assert!(g.ioctl(session, MEDIA_VIDIOC_REQBUFS, req.as_bytes_mut()) == 0 && req[0] == 4);
    let outs: Vec<u64> = (0..4)
        .map(|i| {
            let b = querybuf(g, session, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE, i);
            g.mmap(session, b.plane.m).expect("MMAP of an OUTPUT buffer").0
        })
        .collect();

    // STREAMON OUTPUT: the decoder opens, and says its pictures' layout (SOURCE_CHANGE).
    assert_eq!(g.stream_on(session, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE), 0);
    let got = events(g, |e| e.iter().any(|e| e.0 == MEDIA_EVT_EVENT));
    assert_eq!(got.last().unwrap().1, MEDIA_EVENT_SOURCE_CHANGE);

    // setUpCapture: G_FMT, G_CTRL (min buffers + 2), REQBUFS, QUERYBUF + MMAP + QBUF each, STREAMON.
    let mut f = MediaV4l2Format { type_: MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE, ..Default::default() };
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_G_FMT, f.as_bytes_mut()), 0);
    let stride = f.planes[0].bytesperline;
    assert_eq!((f.width, f.height, f.pixelformat, stride), (W, H, MEDIA_PIXFMT_NV12, 640));
    let mut control = [MEDIA_CID_MIN_BUFFERS_FOR_CAPTURE, 0];
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_G_CTRL, control.as_bytes_mut()), 0);
    let mut req = [control[1] + 2, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE, MEDIA_MEMORY_MMAP, 0, 0];
    assert!(g.ioctl(session, MEDIA_VIDIOC_REQBUFS, req.as_bytes_mut()) == 0 && req[0] == 4);
    let mut caps = Vec::new();
    for i in 0..4 {
        let mut b = querybuf(g, session, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE, i);
        let (pa, size) = g.mmap(session, b.plane.m).expect("MMAP of a CAPTURE buffer");
        assert!(size >= u64::from(stride * H * 3 / 2));
        caps.push(pa);
        assert_eq!(g.ioctl(session, MEDIA_VIDIOC_QBUF, b.as_bytes_mut()), 0);
    }
    assert_eq!(g.stream_on(session, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE), 0);

    // Three frames, their ids as the timestamps; the stand-in gives each frame's picture one frame late. The worker
    // queues each CAPTURE buffer again after its DQBUF, as the guest's does.
    let headers: &[u8] = if codec == MEDIA_PIXFMT_H264 { &[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3] } else { &[] };
    let frames: Vec<Vec<u8>> = (0..3u8)
        .map(|i| {
            let mut f = if i == 0 { headers.to_vec() } else { Vec::new() };
            if codec == MEDIA_PIXFMT_H264 {
                f.extend([0, 0, 0, 1, 0x65]); // the slice, a NAL unit of its own
            }
            f.extend((0..1000).map(|j| (j as u8).wrapping_mul(7).wrapping_add(i)));
            f
        })
        .collect();
    let mut seen = Vec::new();
    let feed = |g: &mut Guest, i: usize, frame: &[u8], id: i64| {
        g.write(outs[i % 4], frame);
        let mut b = MediaV4l2Buffer {
            type_: MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            index: (i % 4) as u32,
            sec: id,
            ..Default::default()
        };
        b.plane.bytesused = frame.len() as u32;
        assert_eq!(g.ioctl(session, MEDIA_VIDIOC_QBUF, b.as_bytes_mut()), 0);
    };
    let requeue = |g: &mut Guest, index: u32| {
        let mut b = MediaV4l2Buffer {
            type_: MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            index,
            memory: MEDIA_MEMORY_MMAP,
            ..Default::default()
        };
        assert_eq!(g.ioctl(session, MEDIA_VIDIOC_QBUF, b.as_bytes_mut()), 0);
    };
    for (i, frame) in frames.iter().enumerate() {
        feed(g, i, frame, 100 + i as i64);
    }
    let got = events(g, |e| captures(e).len() == 2);
    assert_eq!(got.iter().filter(|e| e.1 == MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE).count(), 3); // every OUTPUT back
    for (index, bytes, id) in captures(&got) {
        assert!(bytes >= stride * H * 3 / 2, "{bytes}");
        let pa = caps[index as usize];
        let frame = &frames[(id - 100) as usize];
        assert_eq!(g.read(pa, 1)[0], sum(frame), "frame {id}: the luma is its bytes summed");
        assert_eq!(g.read(pa + u64::from(stride * H), 1)[0], 128, "frame {id}: the chroma");
        seen.push(id);
        requeue(g, index);
    }
    assert_eq!(seen, [100, 101]);

    // dfin: DECODER_CMD STOP brings the last picture out, then EOS; START takes frames again, and a frame of H.264
    // after it gets the stream's headers back before it (the stand-in sums them in).
    let mut cmd = [0u32; 18];
    cmd[0] = MEDIA_DEC_CMD_STOP;
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_DECODER_CMD, cmd.as_bytes_mut()), 0);
    let got = events(g, eos);
    let last = captures(&got);
    assert_eq!(last.iter().map(|c| c.2).collect::<Vec<_>>(), [102]);
    requeue(g, last[0].0);
    cmd[0] = MEDIA_DEC_CMD_START;
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_DECODER_CMD, cmd.as_bytes_mut()), 0);
    let after = [9u8, 9, 9];
    feed(g, 3, &after, 200);
    cmd[0] = MEDIA_DEC_CMD_STOP;
    assert_eq!(g.ioctl(session, MEDIA_VIDIOC_DECODER_CMD, cmd.as_bytes_mut()), 0);
    let got = events(g, eos);
    let pictures = captures(&got);
    assert_eq!(pictures.iter().map(|c| c.2).collect::<Vec<_>>(), [200]);
    let expect = sum(headers).wrapping_add(sum(&after));
    assert_eq!(g.read(caps[pictures[0].0 as usize], 1)[0], expect);

    let st = g.stats();
    let at = decoder_index(codec).unwrap();
    assert_eq!(
        (st.dec_sessions, st.decoded[at], st.dec_fed, st.dec_dropped, st.dec_refused),
        (1, 4, 4, 0, 0),
        "{st:?}"
    );
    assert_eq!(st.dec_timed, 4, "{st:?}");
    // CLOSE: every buffer back in the pool
    assert_eq!(g.close(session), Some(0));
    assert_eq!(g.dev.lock().pool_in_use(), 0);

    // A size the decoder does not take is refused at S_FMT (VideoToolbox decodes it in software).
    let s2 = g.open().expect("OPEN");
    let mut big = MediaV4l2Format { width: 7680, height: 4320, ..out };
    assert_ne!(g.ioctl(s2, MEDIA_VIDIOC_S_FMT, big.as_bytes_mut()), 0);
    assert_eq!(g.close(s2), Some(0));
}

#[test]
fn decoding() {
    // through the helper process, its pool the guest's memfd
    for (codec, base) in [(MEDIA_PIXFMT_VP9, 0x0b00_0000), (MEDIA_PIXFMT_H264, 0x0b00_1000)] {
        let helper = HelperBackend::new(env!("CARGO_BIN_EXE_oneios-encoder"));
        let h = Arc::clone(&helper);
        let mut g = Guest::up_decoding(
            helper as Arc<dyn Backend>,
            move |_, fd| {
                h.set_pool(fd.as_raw_fd(), 0, POOL);
                h as Arc<dyn DecodeBackend>
            },
            base,
        )
        .expect("the device and its guest");
        run(&mut g, codec);
    }
    // with the stand-in in the device's process (xnuhost's PC boot)
    for (codec, base) in [(MEDIA_PIXFMT_AV1, 0x0b00_2000), (MEDIA_PIXFMT_HEVC, 0x0b00_3000)] {
        let mut g = Guest::up_decoding(Arc::new(StubBackend), |pool, _| Arc::new(StubDecodeBackend::new(pool)), base)
            .expect("the device and its guest");
        run(&mut g, codec);
    }
    // decoder=off: an encoder only (its card, its formats), and decoder-codecs= leaves a codec out
    oneencoder::switches::set_decoder(true, 0b0100);
    let mut g =
        Guest::up_decoding(Arc::new(StubBackend), |pool, _| Arc::new(StubDecodeBackend::new(pool)), 0x0b00_4000)
            .expect("the device and its guest");
    let s = g.open().unwrap();
    let mut fd = [0u32; 16];
    (fd[0], fd[1]) = (2, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
    assert!(g.ioctl(s, MEDIA_VIDIOC_ENUM_FMT, fd.as_bytes_mut()) == 0 && fd[11] == MEDIA_PIXFMT_VP9);
    fd[0] = 3;
    assert_ne!(g.ioctl(s, MEDIA_VIDIOC_ENUM_FMT, fd.as_bytes_mut()), 0);
    oneencoder::switches::set_decoder(false, oneencoder::switches::ALL_CODECS);
    assert_eq!(card(&g), "oneios-encoder");
    fd[0] = 2;
    assert_ne!(g.ioctl(s, MEDIA_VIDIOC_ENUM_FMT, fd.as_bytes_mut()), 0);
    let mut out = MediaV4l2Format {
        type_: MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        width: W,
        height: H,
        pixelformat: MEDIA_PIXFMT_VP9,
        ..Default::default()
    };
    assert_ne!(g.ioctl(s, MEDIA_VIDIOC_S_FMT, out.as_bytes_mut()), 0);
    oneencoder::switches::set_decoder(true, oneencoder::switches::ALL_CODECS);
}

#[test]
fn encctl_decodes_a_file_through_the_whole_path() {
    // `encctl decode` (the phone's test of the decoder) on a host: the stand-in decodes an IVF file's frames and an
    // Annex-B stream's access units through the device; every frame comes out as a picture, then "ok".
    let dir = std::env::temp_dir().join(format!("encctl-decode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ivf = b"DKIF\0\0\x20\0AV01\x80\x02\x68\x01\x1e\0\0\0\x01\0\0\0\x05\0\0\0\0\0\0\0".to_vec();
    for i in 0..5u8 {
        ivf.extend(300u32.to_le_bytes());
        ivf.extend(u64::from(i).to_le_bytes());
        ivf.extend(vec![i + 1; 300]);
    }
    let mut h264 = vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 0x88, 7];
    for _ in 0..3 {
        h264.extend([0, 0, 0, 1, 0x41, 0x9a, 5, 6]);
    }
    for (name, data, lines) in
        [("t.ivf", ivf, "AV1 640x360: 5 of 5 frames"), ("t.h264", h264, "H.264 320x240: 4 of 4 frames")]
    {
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        let out = dir.join(format!("{name}.nv12"));
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_encctl"));
        cmd.arg("decode").arg(&path);
        if name.ends_with("h264") {
            cmd.args(["320", "240"]);
        } else {
            cmd.args(["0", "0"]); // the IVF's own size
        }
        let run = cmd.arg(&out).output().unwrap();
        let text = String::from_utf8_lossy(&run.stdout);
        assert!(
            run.status.success() && text.contains(lines) && text.contains("; ok"),
            "{text}\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let (w, h) = if name.ends_with("h264") { (320, 240) } else { (640, 360) };
        assert_eq!(
            std::fs::metadata(&out).unwrap().len(),
            (w * h * 3 / 2 * lines.split(' ').nth(2).unwrap().parse::<usize>().unwrap()) as u64
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
