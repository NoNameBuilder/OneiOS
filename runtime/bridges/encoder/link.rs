// SPDX-License-Identifier: MIT
//! The link between xnuhost's end of the video encoder (the static process: [`crate::media_device`] and
//! [`crate::backend_helper`]) and the oneios-encoder helper process ([`crate::helper`], which runs the NDK's
//! MediaCodec -- a shared library, so it cannot be in the static xnuhost, exactly as the camera and audio helpers are
//! separate processes). A length-framed message protocol over a UNIX stream socket (the C's encoder_link.h; NV12
//! frames and coded bitstream are copied through it: widget-snapshot stills are small).
use crate::pod::Pod;
use std::io::{self, Read, Write};

/// 2 (2026-11-23): the OPEN's input format, the OPEN_REPLY's coding, ENC_MSG_LOG. 3 (2026-11-24): the decoder (the
/// pool, DEC_MSG_*). xnuhost and the helper ship together; a HELLO of another version is refused.
pub const ENC_LINK_VERSION: u32 = 3;

/// helper -> device at start: a = ENC_LINK_VERSION
pub const ENC_MSG_HELLO: u32 = 1;
/// device -> helper: a = codec FourCC, b = width, c = height, payload = the input FourCC (u32 LE: NV12 or P010)
pub const ENC_MSG_OPEN: u32 = 2;
/// helper -> device: session, a = 1 opened (0 refused), b = its coding ([`crate::backend::Coding`])
pub const ENC_MSG_OPEN_REPLY: u32 = 3;
/// device -> helper: session, pts (us) in pts, payload = the frame (NV12 or P010, as opened)
pub const ENC_MSG_FEED: u32 = 4;
/// device -> helper: session
pub const ENC_MSG_DRAIN: u32 = 5;
/// device -> helper: session
pub const ENC_MSG_CLOSE: u32 = 6;
/// helper -> device: session, pts, flags (bit0 key, bit1 eos), payload = Annex-B AU
pub const ENC_MSG_CODED: u32 = 7;
/// helper -> device: session (the drain finished, no more coded buffers)
pub const ENC_MSG_EOS: u32 = 8;
/// helper -> device: session
pub const ENC_MSG_ERROR: u32 = 9;
/// helper -> device: session 0, a = 1 an error (0 news), payload = a line for xnuhost's log (the helper's own: its
/// binder threads, its warm-up, an open that takes long and where it waits)
pub const ENC_MSG_LOG: u32 = 10;

// The decoder (2026-11-24, version 3). A decoder session's id is from the same counter as an encoder's; ENC_MSG_DRAIN,
// ENC_MSG_CLOSE, ENC_MSG_OPEN_REPLY and ENC_MSG_EOS serve both.
/// device -> helper, right after the HELLO, always: the media pool, as the RAM's memfd (SCM_RIGHTS on this header;
/// none when there is no pool), its file offset in pts, its size in a and b (low, high 32 bits). The helper maps it and writes the
/// decoded pictures into it.
pub const ENC_MSG_POOL: u32 = 11;
/// device -> helper: session, a = codec FourCC (H264, HEVC, VP90, AV01), b = width, c = height; ENC_MSG_OPEN_REPLY
pub const DEC_MSG_OPEN: u32 = 12;
/// device -> helper: session, pts = the guest's frame id, payload = one compressed frame
pub const DEC_MSG_FEED: u32 = 13;
/// device -> helper: session, a = CAPTURE index, pts = its offset in the pool, b = its size
pub const DEC_MSG_CAPTURE: u32 = 14;
/// device -> helper: session (the CAPTURE buffers were taken back)
pub const DEC_MSG_RESET: u32 = 15;
/// device -> helper: session (frames again after a drain)
pub const DEC_MSG_START: u32 = 16;
/// helper -> device: session, a = width, b = height, c = stride (the pictures' layout from now on)
pub const DEC_MSG_FORMAT: u32 = 17;
/// helper -> device: session, a = CAPTURE index, b = bytes used, c = 1 an error (no picture), pts = the frame id,
/// payload = the decode time in us (u32 LE)
pub const DEC_MSG_FRAME: u32 = 18;

/// Every message: this header, then `len` payload bytes (`struct enc_msg`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncMsg {
    pub type_: u32,
    pub session: u32,
    pub pts: i64,
    /// small scalars (codec/width/height, flags)
    pub a: u32,
    pub b: u32,
    pub c: u32,
    /// payload bytes that follow
    pub len: u32,
}

// SAFETY: repr(C): two u32, an i64 at 8, four u32: no padding.
unsafe impl Pod for EncMsg {}

const _: () = assert!(std::mem::size_of::<EncMsg>() == 32);

impl EncMsg {
    pub fn new(type_: u32, session: u32) -> EncMsg {
        EncMsg { type_, session, ..Default::default() }
    }

    /// Writes the message and its payload (`len` is set from it); EINTR is retried by write_all.
    pub fn send(mut self, to: &mut impl Write, payload: &[u8]) -> io::Result<()> {
        self.len = payload.len() as u32;
        to.write_all(self.as_bytes())?;
        if !payload.is_empty() {
            to.write_all(payload)?;
        }
        Ok(())
    }

    /// Reads one header (the payload is the caller's to read: `len` bytes).
    pub fn receive(from: &mut impl Read) -> io::Result<EncMsg> {
        let mut m = EncMsg::default();
        from.read_exact(m.as_bytes_mut())?;
        Ok(m)
    }
}
