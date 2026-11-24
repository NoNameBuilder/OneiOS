// SPDX-License-Identifier: MIT
//! `encctl decode` (2026-11-24): the phone test of the video decoder. It runs the whole host path of a decoder
//! session -- the "droidvm,virtio-media" device decoding ([`crate::media_device`]), MediaCodec in asynchronous mode
//! ([`crate::decode_ndk`]; the stand-in on a host), the pictures written into the guest's CAPTURE buffers -- with the
//! plain-memory guest of [`crate::encctl::Guest`] doing what the staged DroidVMVTHost.cpp does: the frames fed as they
//! come, CAPTURE set up at the SOURCE_CHANGE, each picture's buffer queued again, a drain at the end.
//!
//! ```text
//!   encctl decode FILE [WIDTH HEIGHT [OUT]]
//! ```
//!
//! FILE is an IVF file (VP9 or AV1: its header names the codec and the size) or an Annex-B stream (`.h264`/`.264`,
//! `.hevc`/`.265`/`.h265`; WIDTH x HEIGHT, default 1920x1080 (an IVF's own; 0 0 keeps it), is what the decoder is opened at, as the guest opens it at
//! the description's size). OUT gets the pictures as raw NV12 (`ffmpeg -f rawvideo -pix_fmt nv12 -s WxH -i OUT
//! out.png` shows one). On the Mac: `ffmpeg -f lavfi -i testsrc2=s=1280x720:r=30 -t 2 -c:v libvpx-vp9 t.ivf` (or
//! `-c:v libaom-av1 -cpu-used 8`, or `-c:v libx264 -bsf:v h264_mp4toannexb t.h264`), then `adb push`.
//!
//! The last line: `encctl: decode FILE: VP9 1280x720: N of N frames decoded, pictures WxH at a stride of S, decode
//! time avg A ms, max M ms, dropped 0; ok`, or `error:` with what failed.
use crate::encctl::{querybuf, Guest};
use crate::pod::Pod;
use crate::proto::*;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use xh_core::bytes::le;

/// An IVF file's codec, size and frames (None: not IVF, or a codec other than VP9 and AV1).
pub fn read_ivf(data: &[u8]) -> Option<(u32, u32, u32, Vec<&[u8]>)> {
    if data.len() < 32 || &data[..4] != b"DKIF" {
        return None;
    }
    let codec = match &data[8..12] {
        b"VP90" => MEDIA_PIXFMT_VP9,
        b"AV01" => MEDIA_PIXFMT_AV1,
        _ => return None,
    };
    let (w, h) =
        (u32::from(u16::from_le_bytes([data[12], data[13]])), u32::from(u16::from_le_bytes([data[14], data[15]])));
    let header = usize::from(u16::from_le_bytes([data[6], data[7]])).max(32);
    let mut frames = Vec::new();
    let mut at = header;
    while at + 12 <= data.len() {
        let size = le::u32(data, at) as usize;
        let end = (at + 12).checked_add(size)?;
        if end > data.len() {
            break;
        }
        frames.push(&data[at + 12..end]);
        at = end;
    }
    Some((codec, w, h, frames))
}

/// The access units of an Annex-B stream: a new one at a parameter set, an AUD or an SEI after a picture's slices, or
/// at a slice that starts a picture (H.264's first_mb_in_slice 0, HEVC's first_slice_segment_in_pic_flag).
pub fn split_annexb(codec: u32, data: &[u8]) -> Vec<&[u8]> {
    let hevc = codec == MEDIA_PIXFMT_HEVC;
    let mut starts = Vec::new(); // (start code's first byte, NAL's first byte)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let first = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((first, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    let (mut begin, mut vcl) = (None::<usize>, false);
    for &(code, nal) in &starts {
        let Some(&h0) = data.get(nal) else { continue };
        let kind = if hevc { (h0 >> 1) & 0x3f } else { h0 & 0x1f };
        let is_vcl = if hevc { kind < 32 } else { (1..=5).contains(&kind) };
        let first_slice = data.get(nal + if hevc { 2 } else { 1 }).is_some_and(|b| b & 0x80 != 0);
        let prefix = if hevc { matches!(kind, 32..=35 | 39) } else { matches!(kind, 6..=9) };
        if vcl && ((is_vcl && first_slice) || prefix) {
            units.push(&data[begin.unwrap()..code]);
            (begin, vcl) = (None, false);
        }
        begin.get_or_insert(code);
        vcl |= is_vcl;
    }
    if let Some(b) = begin {
        units.push(&data[b..]);
    }
    units
}

fn fail(text: &str) -> i32 {
    eprintln!("error: {text}");
    1
}

fn decode_backend(pool: crate::decoder::PoolMem) -> Arc<dyn crate::decoder::DecodeBackend> {
    #[cfg(target_os = "android")]
    {
        let log: crate::decode_ndk::DecodeLog =
            Arc::new(|error, line| eprintln!("{} {line}", if error { "error:" } else { "encctl:" }));
        Arc::new(crate::decode_ndk::NdkDecodeBackend::new(pool, log))
    }
    #[cfg(not(target_os = "android"))]
    Arc::new(crate::decoder::StubDecodeBackend::new(pool)) // no MediaCodec on a host: the stand-in
}

/// `encctl decode FILE [WIDTH HEIGHT [OUT]]`: its exit code.
pub fn decode(args: &[String]) -> i32 {
    let Some(path) = args.first() else { return fail("usage: encctl decode FILE [WIDTH HEIGHT [OUT]]") };
    let Ok(data) = std::fs::read(path) else { return fail(&format!("cannot read {path}")) };
    let num = |i: usize| args.get(i).map(|a| onecamera::util::atoi(a) as u32);
    let (codec, w, h, frames) = match read_ivf(&data) {
        Some(ivf) => ivf,
        None => {
            let lower = path.to_lowercase();
            let codec = if [".hevc", ".265", ".h265"].iter().any(|e| lower.ends_with(e)) {
                MEDIA_PIXFMT_HEVC
            } else if [".h264", ".264"].iter().any(|e| lower.ends_with(e)) {
                MEDIA_PIXFMT_H264
            } else {
                return fail(&format!("{path}: neither IVF (VP9, AV1) nor .h264/.hevc"));
            };
            (codec, num(1).unwrap_or(1920), num(2).unwrap_or(1080), split_annexb(codec, &data))
        }
    };
    let (w, h) = (num(1).filter(|v| *v != 0).unwrap_or(w), num(2).filter(|v| *v != 0).unwrap_or(h)); // 0: the file's own
    let name = codec_name(codec);
    if frames.is_empty() {
        return fail(&format!("{path}: no frames"));
    }
    let mut out = match args.get(3) {
        Some(p) => match std::fs::File::create(p) {
            Ok(f) => Some(f),
            Err(_) => return fail(&format!("cannot write {p}")),
        },
        None => None,
    };
    println!("encctl: decode {path}: {name} {w}x{h}, {} frames", frames.len());
    let backend: Arc<dyn crate::backend::Backend> = Arc::new(crate::backend_stub::StubBackend); // no encoding here
    let mut g = match Guest::up_decoding(backend, |pool, _| decode_backend(pool), 0x0a00_0000) {
        Ok(g) => g,
        Err(e) => return fail(e),
    };
    let Some(session) = g.open() else { return fail("OPEN") };
    let mut f = MediaV4l2Format {
        type_: MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        width: w,
        height: h,
        pixelformat: codec,
        num_planes: 1,
        ..Default::default()
    };
    f.planes[0].sizeimage = 2 << 20;
    if g.ioctl(session, MEDIA_VIDIOC_S_FMT, f.as_bytes_mut()) != 0 {
        return fail("S_FMT output (a size or codec the device does not take)");
    }
    let size = f.planes[0].sizeimage as usize;
    let mut req = [4, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE, MEDIA_MEMORY_MMAP, 0, 0];
    if g.ioctl(session, MEDIA_VIDIOC_REQBUFS, req.as_bytes_mut()) != 0 {
        return fail("REQBUFS output");
    }
    let mut outs = Vec::new();
    for i in 0..req[0] {
        let b = querybuf(&mut g, session, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE, i);
        match g.mmap(session, b.plane.m) {
            Ok((pa, _)) => outs.push(pa),
            Err(e) => return fail(&e.to_string()),
        }
    }
    let opened = Instant::now();
    if g.stream_on(session, MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE) != 0 {
        return fail("STREAMON output: the decoder did not open (its line says why)");
    }
    println!("encctl: the decoder opened in {} ms", opened.elapsed().as_millis());
    let mut free: Vec<u32> = (0..outs.len() as u32).collect();
    let (mut next, mut caps, mut layout) = (0usize, Vec::new(), (0u32, 0u32, 0u32));
    let (mut decoded, mut errors, mut drained, mut eos) = (0usize, 0usize, false, false);
    let started = Instant::now();
    let mut quiet = Instant::now();
    while !eos {
        // feed what there is room for (the guest's worker does the same as dfrm comes)
        while next < frames.len() && !free.is_empty() {
            let frame = frames[next];
            if frame.len() > size {
                return fail(&format!("frame {next}: {} bytes, the OUTPUT buffers hold {size}", frame.len()));
            }
            let i = free.remove(0);
            g.write(outs[i as usize], frame);
            let mut b = MediaV4l2Buffer {
                type_: MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                index: i,
                sec: next as i64,
                ..Default::default()
            };
            b.plane.bytesused = frame.len() as u32;
            if g.ioctl(session, MEDIA_VIDIOC_QBUF, b.as_bytes_mut()) != 0 {
                return fail(&format!("QBUF of frame {next}"));
            }
            next += 1;
        }
        if next == frames.len() && !drained {
            let mut cmd = [0u32; 18];
            cmd[0] = MEDIA_DEC_CMD_STOP;
            g.ioctl(session, MEDIA_VIDIOC_DECODER_CMD, cmd.as_bytes_mut());
            drained = true;
        }
        let Some((kind, _, ev)) = g.wait_event(Duration::from_millis(100)) else {
            if quiet.elapsed() > Duration::from_secs(5) {
                return fail(&format!("nothing from the decoder for 5 s: {decoded} of {} frames out", frames.len()));
            }
            continue;
        };
        quiet = Instant::now();
        if kind == MEDIA_EVT_EVENT {
            match le::u32(&ev, 8) {
                MEDIA_EVENT_EOS => eos = true,
                MEDIA_EVENT_SOURCE_CHANGE if caps.is_empty() => {
                    // setUpCapture
                    let mut f = MediaV4l2Format { type_: MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE, ..Default::default() };
                    if g.ioctl(session, MEDIA_VIDIOC_G_FMT, f.as_bytes_mut()) != 0 {
                        return fail("G_FMT capture");
                    }
                    layout = (f.width, f.height, f.planes[0].bytesperline);
                    println!("encctl: pictures {}x{} at a stride of {}", layout.0, layout.1, layout.2);
                    let mut control = [MEDIA_CID_MIN_BUFFERS_FOR_CAPTURE, 0];
                    g.ioctl(session, MEDIA_VIDIOC_G_CTRL, control.as_bytes_mut());
                    let mut req = [control[1] + 2, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE, MEDIA_MEMORY_MMAP, 0, 0];
                    if g.ioctl(session, MEDIA_VIDIOC_REQBUFS, req.as_bytes_mut()) != 0 {
                        return fail("REQBUFS capture (the pool is too small for these pictures)");
                    }
                    for i in 0..req[0] {
                        let mut b = querybuf(&mut g, session, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE, i);
                        match g.mmap(session, b.plane.m) {
                            Ok((pa, _)) => caps.push(pa),
                            Err(e) => return fail(&e.to_string()),
                        }
                        g.ioctl(session, MEDIA_VIDIOC_QBUF, b.as_bytes_mut());
                    }
                    g.stream_on(session, MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
                }
                _ => {}
            }
            continue;
        }
        if kind != MEDIA_EVT_DQBUF {
            continue;
        }
        let b = MediaV4l2Buffer::read_from(&ev[8..]);
        if b.type_ == MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE {
            free.push(b.index);
            continue;
        }
        if b.flags & MEDIA_BUF_FLAG_ERROR != 0 || b.plane.bytesused == 0 {
            errors += 1;
        } else {
            decoded += 1;
            if let (Some(file), Some(&pa)) = (out.as_mut(), caps.get(b.index as usize)) {
                let (pw, ph, stride) = layout;
                let rows = ph + ph.div_ceil(2);
                for row in 0..rows {
                    let line = g.read(pa + u64::from(row * stride), pw as usize);
                    if file.write_all(&line).is_err() {
                        return fail("cannot write the pictures");
                    }
                }
            }
        }
        let mut q = MediaV4l2Buffer {
            type_: MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            index: b.index,
            memory: MEDIA_MEMORY_MMAP,
            ..Default::default()
        };
        g.ioctl(session, MEDIA_VIDIOC_QBUF, q.as_bytes_mut());
    }
    let took = started.elapsed().as_secs_f64();
    g.close(session);
    let st = g.stats();
    let avg = if st.dec_timed == 0 { 0.0 } else { st.dec_us as f64 / st.dec_timed as f64 / 1000.0 };
    let line = format!(
        "decode {path}: {name} {w}x{h}: {decoded} of {} frames decoded in {took:.2} s ({:.0} a second), pictures {}x{} at a \
         stride of {}, decode time avg {avg:.1} ms, dropped {}",
        frames.len(),
        decoded as f64 / took.max(1e-6),
        layout.0,
        layout.1,
        layout.2,
        st.dec_dropped
    );
    if decoded == frames.len() && errors == 0 && st.dec_dropped == 0 {
        println!("encctl: {line}; ok");
        0
    } else {
        fail(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivf_frames_are_read() {
        let mut f = b"DKIF\0\0\x20\0VP90\x00\x05\xd0\x02\x1e\0\0\0\x01\0\0\0\x02\0\0\0\0\0\0\0".to_vec();
        for (i, n) in [3usize, 5].iter().enumerate() {
            f.extend((*n as u32).to_le_bytes());
            f.extend((i as u64).to_le_bytes());
            f.extend(vec![i as u8; *n]);
        }
        let (codec, w, h, frames) = read_ivf(&f).unwrap();
        assert_eq!((codec, w, h), (MEDIA_PIXFMT_VP9, 1280, 720));
        assert_eq!(frames, [&[0u8; 3][..], &[1u8; 5][..]]);
        assert!(read_ivf(b"RIFF").is_none());
    }

    #[test]
    fn annex_b_is_split_into_access_units() {
        // H.264: SPS PPS IDR(first) | slice(first) | slice(first) slice(not first)
        let s = [
            0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 0x88, 0, 0, 1, 0x41, 0x9a, 0, 0, 1, 0x41, 0x9a,
            0, 0, 1, 0x41, 0x1a,
        ];
        let units = split_annexb(MEDIA_PIXFMT_H264, &s);
        assert_eq!(units.len(), 3, "{units:?}");
        assert_eq!(units[0], &s[..18]);
        assert_eq!(units[2], &s[23..]);
        // HEVC: VPS SPS PPS IDR | TRAIL (first slice bit in the byte after the two-byte header)
        let h = [0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x26, 1, 0xaf, 0, 0, 1, 0x02, 1, 0xd0];
        let units = split_annexb(MEDIA_PIXFMT_HEVC, &h);
        assert_eq!(units.len(), 2);
        assert_eq!(units[1], &h[21..]);
    }
}
