// SPDX-License-Identifier: MIT
//! The wire layout of xnuhost's "droidvm,virtio-media" device (the C's media_proto.h; both ends are Rust now,
//! checked by tests/encoder.rs's round trips): a stateful V4L2 video encoder over virtio-mmio, as UniXNU-droidvm's
//! guest transport DroidVMVirtioMedia.c drives it and DroidVMVTHost.cpp's session logic expects. It stands in for
//! crosvm's --virtio-media device: the guest kernel's transport is unchanged, so the device must answer the same
//! commands and events.
//!
//! Two queues:
//! - commandq (0, driver -> device): OPEN -> session id; CLOSE; IOCTL(session, the _IOC_NR of a VIDIOC_*) with the
//!   ioctl payload out (for _IOW/_IOWR) and back (for _IOR/_IOWR), a response header in front; MMAP(session, offset)
//!   -> an offset into the host pool; MUNMAP. A response status is 0 or a Linux errno.
//! - eventq (1, device -> driver): the guest posts writable event buffers; the device fills one per event: DQBUF (a
//!   struct v4l2_buffer and one struct v4l2_plane), EVENT (a struct v4l2_event), ERROR (the session is gone).
//!
//! Config space (offset 0 of the virtio config, i.e. MMIO 0x100): device_caps (u32), device_type (u32), card (32
//! bytes). All little-endian (the guest's own byte order); every structure matches the 64-bit Linux UAPI
//! DroidVMVTHost reads.
use crate::pod::Pod;

/// VIRTIO_ID_MEDIA, as DroidVMVirtioMedia.c checks at MMIO 0x08.
pub const MEDIA_VIRTIO_DEVICE_ID: u32 = 48;

pub const MEDIA_QUEUE_COMMAND: u32 = 0;
pub const MEDIA_QUEUE_EVENT: u32 = 1;
pub const MEDIA_NQUEUES: u32 = 2;

// commandq command codes (cmd[0]); the response's status is at response[0].
/// -> response[2] = session id
pub const MEDIA_CMD_OPEN: u32 = 1;
/// cmd[2] = session
pub const MEDIA_CMD_CLOSE: u32 = 2;
/// cmd[2] = session, cmd[3] = ioctl nr; out payload, then in payload back
pub const MEDIA_CMD_IOCTL: u32 = 3;
/// cmd[2] = session, cmd[3] = writable, cmd[4] = offset; -> at(u64)@8, size(u64)@16
pub const MEDIA_CMD_MMAP: u32 = 4;
/// cmd[2..3] = the pool offset (lo, hi)
pub const MEDIA_CMD_MUNMAP: u32 = 5;

// eventq event kinds (event[0]); event[1] = session.
pub const MEDIA_EVT_ERROR: u32 = 0;
/// event[2..] = struct v4l2_buffer + one struct v4l2_plane
pub const MEDIA_EVT_DQBUF: u32 = 1;
/// event[2..] = struct v4l2_event (its first u32 is the type)
pub const MEDIA_EVT_EVENT: u32 = 2;
/// >= 8 + sizeof(v4l2_buffer) + 8*64 (DroidVMVirtioMedia.c MEDIA_EVENT_SIZE)
pub const MEDIA_EVENT_BYTES: u32 = 640;

// V4L2 ioctl numbers, by _IOC_NR, the values DroidVMVTHost.cpp passes (and DroidVMVirtioMedia.c's ENUM_FMT).
pub const MEDIA_VIDIOC_ENUM_FMT: u32 = 2;
pub const MEDIA_VIDIOC_G_FMT: u32 = 4;
pub const MEDIA_VIDIOC_S_FMT: u32 = 5;
pub const MEDIA_VIDIOC_REQBUFS: u32 = 8;
pub const MEDIA_VIDIOC_QUERYBUF: u32 = 9;
pub const MEDIA_VIDIOC_QBUF: u32 = 15;
pub const MEDIA_VIDIOC_STREAMON: u32 = 18;
pub const MEDIA_VIDIOC_STREAMOFF: u32 = 19;
pub const MEDIA_VIDIOC_G_CTRL: u32 = 27;
pub const MEDIA_VIDIOC_ENUM_FRAMESIZES: u32 = 74;
pub const MEDIA_VIDIOC_ENCODER_CMD: u32 = 77;
pub const MEDIA_VIDIOC_SUBSCRIBE_EVENT: u32 = 90;
pub const MEDIA_VIDIOC_DECODER_CMD: u32 = 96;

// V4L2 buffer types and memory.
pub const MEDIA_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const MEDIA_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
pub const MEDIA_MEMORY_MMAP: u32 = 1;

// V4L2 device_caps bits (config space).
pub const MEDIA_CAP_VIDEO_M2M_MPLANE: u32 = 0x4000;
pub const MEDIA_CAP_STREAMING: u32 = 0x0400_0000;

// V4L2 buffer flags (struct v4l2_buffer .flags).
pub const MEDIA_BUF_FLAG_KEYFRAME: u32 = 0x8;
pub const MEDIA_BUF_FLAG_ERROR: u32 = 0x40;
pub const MEDIA_BUF_FLAG_LAST: u32 = 0x0010_0000;

// V4L2 events and encoder commands.
pub const MEDIA_EVENT_EOS: u32 = 2;
pub const MEDIA_EVENT_SOURCE_CHANGE: u32 = 5;
pub const MEDIA_ENC_CMD_START: u32 = 0;
pub const MEDIA_ENC_CMD_STOP: u32 = 1;
/// V4L2_DEC_CMD_START and _STOP (VIDIOC_DECODER_CMD; 2026-11-24: the decoder's drain and its start again).
pub const MEDIA_DEC_CMD_START: u32 = 0;
pub const MEDIA_DEC_CMD_STOP: u32 = 1;
/// V4L2_CID_MIN_BUFFERS_FOR_CAPTURE (VIDIOC_G_CTRL: the decoder's CAPTURE buffers the guest adds two to).
pub const MEDIA_CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0927;
/// V4L2_FRMSIZE_TYPE_STEPWISE (VIDIOC_ENUM_FRAMESIZES).
pub const MEDIA_FRMSIZE_TYPE_STEPWISE: u32 = 3;

/// A FourCC pixel format (little-endian, as V4L2 keeps them).
pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}
pub const MEDIA_PIXFMT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
/// V4L2_PIX_FMT_P010: NV12's layout with 16-bit little-endian samples, the 10 bits in the high bits (2026-11-23: iOS's
/// 10-bit surfaces x420 and xf20, as the guest copies them; coded as HEVC Main10 when the encoder offers it).
pub const MEDIA_PIXFMT_P010: u32 = fourcc(b'P', b'0', b'1', b'0');

/// The bytes of one tightly packed 4:2:0 picture of `input` (NV12 or P010) at width x height.
pub fn picture_bytes(input: u32, width: u32, height: u32) -> u64 {
    let samples = u64::from(width) * u64::from(height) * 3 / 2;
    if input == MEDIA_PIXFMT_P010 {
        samples * 2
    } else {
        samples
    }
}
pub const MEDIA_PIXFMT_H264: u32 = fourcc(b'H', b'2', b'6', b'4');
pub const MEDIA_PIXFMT_HEVC: u32 = fourcc(b'H', b'E', b'V', b'C');
/// V4L2_PIX_FMT_VP9 and xnuhost's own 'AV01' (2026-11-24): the decoder's two other OUTPUT formats (DroidVMVTHost.cpp's
/// staged edit 6 asks for them as VideoToolbox's 'vp09' and 'av01').
pub const MEDIA_PIXFMT_VP9: u32 = fourcc(b'V', b'P', b'9', b'0');
pub const MEDIA_PIXFMT_AV1: u32 = fourcc(b'A', b'V', b'0', b'1');
/// The decoder's codecs in the order of its switch bits and counters (decoder-codecs=; DroidVMVTHost.cpp codecIndex).
pub const DECODER_CODECS: [u32; 4] = [MEDIA_PIXFMT_H264, MEDIA_PIXFMT_HEVC, MEDIA_PIXFMT_VP9, MEDIA_PIXFMT_AV1];

/// A codec's place in [`DECODER_CODECS`].
pub fn decoder_index(codec: u32) -> Option<usize> {
    DECODER_CODECS.iter().position(|c| *c == codec)
}

/// A decoder codec's name in log lines.
pub fn codec_name(codec: u32) -> &'static str {
    match codec {
        MEDIA_PIXFMT_H264 => "H.264",
        MEDIA_PIXFMT_HEVC => "HEVC",
        MEDIA_PIXFMT_VP9 => "VP9",
        MEDIA_PIXFMT_AV1 => "AV1",
        _ => "?",
    }
}

/// struct v4l2_plane (one, after the buffer).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaV4l2Plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: u64,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

/// struct v4l2_buffer followed by one struct v4l2_plane (DroidVMVTHost.cpp V4l2Buffer, 88 + 64 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaV4l2Buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub pad0: u32,
    pub sec: i64,
    pub usec: i64,
    pub timecode: [u8; 16],
    pub sequence: u32,
    pub memory: u32,
    pub m: u64,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: u32,
    pub pad1: u32,
    pub plane: MediaV4l2Plane,
}

/// One plane's format of v4l2_pix_format_mplane.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaPlaneFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

/// struct v4l2_format with v4l2_pix_format_mplane (DroidVMVTHost.cpp V4l2Format, 208 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaV4l2Format {
    pub type_: u32,
    pub pad: u32,
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub planes: [MediaPlaneFormat; 8],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr: u8,
    pub quantization: u8,
    pub xfer: u8,
    pub reserved: [u8; 7],
    pub rest: [u8; 200 - 192],
}

// SAFETY: repr(C), integers only, no padding (the asserts below and tests/headers.rs).
unsafe impl Pod for MediaV4l2Plane {}
// SAFETY: as above.
unsafe impl Pod for MediaV4l2Buffer {}
// SAFETY: as above.
unsafe impl Pod for MediaPlaneFormat {}
// SAFETY: as above.
unsafe impl Pod for MediaV4l2Format {}

const _: () = assert!(std::mem::size_of::<MediaV4l2Buffer>() == 88 + 64, "v4l2_buffer + v4l2_plane");
const _: () = assert!(std::mem::size_of::<MediaV4l2Format>() == 208, "v4l2_format");
