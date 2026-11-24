// SPDX-License-Identifier: MIT
//! The video decoder (2026-11-24; README.md "The decoder", docs/hardware.md 7g): the other half of xnuhost's
//! "droidvm,virtio-media" device. iOS's paravirtualized VideoToolbox decoder (DroidVMVTHost.cpp in the guest kernel,
//! its staged edit 6) drives a stateful V4L2 decoder: OUTPUT is the compressed stream (H.264 and HEVC as Annex-B access
//! units, VP9 and AV1 frames as they are), CAPTURE the decoded pictures, NV12. The decoding is a backend's
//! ([`DecodeBackend`]): Android's MediaCodec in the helper process ([`crate::decode_ndk`], through
//! [`crate::backend_helper`]), a stand-in on the PC ([`StubDecodeBackend`]).
//!
//! The pictures are written straight into the guest's CAPTURE buffers, in the media pool ([`PoolMem`]): the helper maps
//! the pool from the RAM's memfd (sent over its socket), so a picture is copied once on the host, from MediaCodec's
//! output buffer into the guest's buffer, and once in the guest, into the IOSurface.
//!
//! A decoder gives its results to a [`DecodeSink`] (the device queues them and wakes its worker: no polling), from
//! whatever thread it decodes on: the picture's layout ([`DecEvent::Format`], then the device's SOURCE_CHANGE), each
//! picture placed in a CAPTURE buffer ([`DecEvent::Frame`]), and the end of a drain ([`DecEvent::Eos`]).
//!
//! [`Core`] is the logic every backend shares, over a [`DecCodec`] (MediaCodec's buffers, or the stand-in's): the frames
//! waiting for an input buffer, the pictures waiting for a CAPTURE buffer, the stream's headers (sent again after a
//! drain, which flushes MediaCodec), the time each frame took.
use crate::proto::*;
use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

/// The CAPTURE layout the device tells the guest (G_FMT): NV12, `stride` bytes a row, `height` luma rows, then
/// `(height + 1) / 2` chroma rows at `stride * height`, as DroidVMVTHost.cpp's copyFrame reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapLayout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl CapLayout {
    /// The bytes of one picture.
    pub fn bytes(&self) -> u64 {
        u64::from(self.stride) * (u64::from(self.height) + u64::from(self.height).div_ceil(2))
    }
}

/// What a decoder gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecEvent {
    /// The pictures' layout from now on (the first, or a new one).
    Format(CapLayout),
    /// A picture in CAPTURE buffer `index`: `bytes` used (0 with `error`), the frame's pts, how long it was in the
    /// decoder (us; 0 not known).
    Frame { index: u32, bytes: u32, pts: i64, error: bool, decode_us: u32 },
    /// The drain is over: every frame fed before it came out.
    Eos,
}

/// Where a decoder's results go (any thread).
pub type DecodeSink = Arc<dyn Fn(DecEvent) + Send + Sync>;

/// One decoder session (`dec_session`). Dropping it closes it.
pub trait Decoder: Send {
    /// One compressed frame (copied: the caller may reuse the buffer at once), `pts` the guest's frame id.
    fn feed(&mut self, data: &[u8], pts: i64) -> Result<(), i32>;
    /// CAPTURE buffer `index`, `size` bytes at `offset` in the media pool, is the decoder's to fill.
    fn capture(&mut self, index: u32, offset: u64, size: u64);
    /// The CAPTURE buffers were taken back (REQBUFS): none is the decoder's.
    fn reset_capture(&mut self);
    /// Every frame fed is to come out (V4L2_DEC_CMD_STOP); then [`DecEvent::Eos`].
    fn drain(&mut self) -> Result<(), i32>;
    /// Frames again after a drain (V4L2_DEC_CMD_START).
    fn restart(&mut self) -> Result<(), i32>;
}

/// A decoder backend (`dec_backend`).
pub trait DecodeBackend: Send + Sync {
    fn name(&self) -> &'static str;
    /// A session for `codec` (one of [`DECODER_CODECS`]) at width x height, its results to `sink`; None when refused.
    fn open_decoder(&self, codec: u32, width: u32, height: u32, sink: DecodeSink) -> Option<Box<dyn Decoder>>;
}

/// The media pool as this process maps it: CAPTURE buffers are written through it.
#[derive(Clone, Copy, Debug)]
pub struct PoolMem {
    host: NonNull<u8>,
    size: u64,
}

// SAFETY: guest RAM mapped for the process's life; a decoder writes only a CAPTURE buffer the guest has queued, which the
// guest does not touch until its DQBUF.
unsafe impl Send for PoolMem {}
// SAFETY: as above.
unsafe impl Sync for PoolMem {}

impl PoolMem {
    /// # Safety
    /// `host` must point at `size` bytes mapped for as long as any copy of this lives.
    pub unsafe fn new(host: *mut u8, size: u64) -> Option<PoolMem> {
        NonNull::new(host).map(|host| PoolMem { host, size })
    }

    /// `[offset, offset + size)` of the pool, if it is inside it.
    ///
    /// # Safety
    /// The range must be a buffer nobody else writes or reads while the slice lives (a queued CAPTURE buffer).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice(&self, offset: u64, size: u64) -> Option<&mut [u8]> {
        let end = offset.checked_add(size)?;
        // SAFETY: inside the mapping (checked); exclusive by the caller's promise.
        (end <= self.size)
            .then(|| unsafe { std::slice::from_raw_parts_mut(self.host.as_ptr().add(offset as usize), size as usize) })
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

/// A decoded picture's layout in the codec's output buffer: the visible part (`left`, `top`, `width` x `height`), the
/// rows (`stride` bytes, `slice` luma rows before the chroma plane), and the sample size (1: NV12, 2: P010).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutLayout {
    pub width: u32,
    pub height: u32,
    pub left: u32,
    pub top: u32,
    pub stride: u32,
    pub slice: u32,
    pub sample: u32,
}

impl OutLayout {
    /// MediaCodec's output format (its "width", "height", "stride", "slice-height", "color-format" and "crop"; a
    /// missing stride is the width, a missing slice height the height, a missing crop all of it). None when the
    /// picture has no size or is not two-plane 4:2:0.
    #[allow(clippy::too_many_arguments)]
    pub fn from_format(
        width: i32,
        height: i32,
        stride: Option<i32>,
        slice: Option<i32>,
        color: Option<i32>,
        crop: Option<(i32, i32, i32, i32)>,
    ) -> Option<OutLayout> {
        let (w, h) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
        if w == 0 || h == 0 {
            return None;
        }
        let sample = match color {
            Some(c) if c == crate::backend_ndk::COLOR_FORMAT_P010 => 2,
            // NV12 (21), the flexible 4:2:0 (0x7f420888), or none said: two planes of 8-bit samples
            _ => 1,
        };
        let stride = stride.and_then(|s| u32::try_from(s).ok()).filter(|s| *s >= w * sample).unwrap_or(w * sample);
        let slice = slice.and_then(|s| u32::try_from(s).ok()).filter(|s| *s >= h).unwrap_or(h);
        let (left, top, right, bottom) = crop.unwrap_or((0, 0, width - 1, height - 1));
        let (left, top) = (u32::try_from(left).ok()? & !1, u32::try_from(top).ok()? & !1);
        let (right, bottom) = (u32::try_from(right).ok()?, u32::try_from(bottom).ok()?);
        if right < left || bottom < top || right >= w || bottom >= h {
            return None;
        }
        Some(OutLayout { width: right - left + 1, height: bottom - top + 1, left, top, stride, slice, sample })
    }

    /// What the guest's CAPTURE buffers get: the visible part at MediaCodec's own stride for NV12 (each plane one copy
    /// when nothing is cropped at the left or top), at the width rounded up to 64 for P010 (made 8-bit: each sample's
    /// high byte).
    pub fn capture(&self) -> CapLayout {
        let stride = if self.sample == 1 { self.stride } else { self.width.div_ceil(64) * 64 };
        CapLayout { width: self.width, height: self.height, stride }
    }

    /// The bytes an output buffer must have for this layout (its last chroma row may end at the picture's width).
    fn source_bytes(&self) -> usize {
        let chroma_rows = (self.top / 2 + self.height.div_ceil(2)) as usize;
        let last = (self.left * self.sample + self.width * self.sample) as usize;
        self.stride as usize * (self.slice as usize + chroma_rows - 1) + last
    }

    /// The picture of `src` (an output buffer) into `dst` (a CAPTURE buffer) in [`OutLayout::capture`]'s layout. The
    /// bytes used, or None when either is too small.
    pub fn place(&self, src: &[u8], dst: &mut [u8]) -> Option<usize> {
        let cap = self.capture();
        let used = cap.bytes() as usize;
        if dst.len() < used || src.len() < self.source_bytes() {
            return None;
        }
        let (stride, cstride) = (self.stride as usize, cap.stride as usize);
        let (w, h, ch) = (self.width as usize, self.height as usize, self.height.div_ceil(2) as usize);
        let (left, top) = ((self.left * self.sample) as usize, self.top as usize);
        let chroma_src = stride * self.slice as usize;
        let chroma_dst = cstride * h;
        if self.sample == 1 && left == 0 && top == 0 {
            // NV12, nothing cropped at the left or top: each plane in one copy (the last row's padding may be missing)
            let luma = stride * h;
            dst[..luma].copy_from_slice(&src[..luma]);
            let chroma = (stride * ch).min(src.len() - chroma_src);
            dst[chroma_dst..chroma_dst + chroma].copy_from_slice(&src[chroma_src..chroma_src + chroma]);
            return Some(used);
        }
        let row = |plane: usize, y: usize| plane + y * stride + left;
        let copy = |from: &[u8], to: &mut [u8]| {
            if self.sample == 1 {
                to[..w].copy_from_slice(&from[..w]);
            } else {
                for (t, s) in to[..w].iter_mut().zip(from.chunks_exact(2)) {
                    *t = s[1]; // the 10-bit sample's top 8 bits
                }
            }
        };
        for y in 0..h {
            let at = row(0, top + y);
            copy(&src[at..at + w * self.sample as usize], &mut dst[y * cstride..]);
        }
        for y in 0..ch {
            let at = row(chroma_src, top / 2 + y);
            copy(&src[at..at + w * self.sample as usize], &mut dst[chroma_dst + y * cstride..]);
        }
        Some(used)
    }
}

/// The NAL units of an Annex-B access unit, without their start codes.
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            let mut e = s - 3;
            while e > 0 && data[e - 1] == 0 {
                e -= 1; // a four-byte start code's zero, or trailing zeros
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts.into_iter().zip(ends).filter(|(s, e)| s < e).map(move |(s, e)| &data[s..e])
}

/// AV1's leb128 at `data[at..]`: the value and its length.
fn leb128(data: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for i in 0..8 {
        let b = *data.get(at + i)?;
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The stream headers a frame carries, as the stream has them (Annex-B with start codes for H.264 and HEVC; OBUs for
/// AV1): the parameter sets (H.264's SPS and PPS, HEVC's VPS, SPS and PPS) or AV1's sequence header. None when the
/// frame has none (and always for VP9, whose key frames carry their own). The decoder keeps the last ones and puts them
/// before the first frame after a drain: MediaCodec forgets them at the flush that ends the drain, and the guest sends
/// them before its first frame only.
pub fn stream_headers(codec: u32, data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match codec {
        MEDIA_PIXFMT_H264 | MEDIA_PIXFMT_HEVC => {
            for nal in nal_units(data) {
                let header = codec == MEDIA_PIXFMT_H264 && matches!(nal[0] & 0x1f, 7 | 8)
                    || codec == MEDIA_PIXFMT_HEVC && matches!((nal[0] >> 1) & 0x3f, 32..=34);
                if header {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal);
                }
            }
        }
        MEDIA_PIXFMT_AV1 => {
            let mut at = 0;
            while at < data.len() {
                let header = data[at];
                let (kind, extension, sized) = ((header >> 3) & 0xf, header & 4 != 0, header & 2 != 0);
                if !sized {
                    break; // an OBU without its size runs to the end: no headers to find after it
                }
                let (size, length) = leb128(data, at + 1 + usize::from(extension))?;
                let end = (at + 1 + usize::from(extension) + length).checked_add(size as usize)?;
                if end > data.len() {
                    break;
                }
                if kind == 1 {
                    out.extend_from_slice(&data[at..end]); // OBU_SEQUENCE_HEADER
                }
                at = end;
            }
        }
        _ => {}
    }
    (!out.is_empty()).then_some(out)
}

/// MediaCodec's flags of an input or output buffer that the decoder uses.
pub const DEC_FLAG_END_OF_STREAM: u32 = 4;

/// A codec's buffers, as [`Core`] uses them: MediaCodec's (by index), or the stand-in's.
pub trait DecCodec {
    fn input_buffer(&mut self, index: usize) -> Option<&mut [u8]>;
    /// Whether the codec took it.
    fn queue_input(&mut self, index: usize, size: usize, pts: i64, flags: u32) -> bool;
    fn output_buffer(&mut self, index: usize) -> Option<&[u8]>;
    fn release_output(&mut self, index: usize);
}

/// An output buffer's description: where its picture is, its pts, its flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutInfo {
    pub offset: usize,
    pub size: usize,
    pub pts: i64,
    pub flags: u32,
}

/// Frames kept for the decode time (their pts and when they were fed), at most this many.
const TIMED: usize = 64;

/// The decoding logic every backend shares (`dec_core`).
pub struct Core {
    codec: u32,
    pool: PoolMem,
    sink: DecodeSink,
    /// the codec's input buffers free to fill, and the frames waiting for one (data, pts, flags)
    free_inputs: VecDeque<usize>,
    pending: VecDeque<(Vec<u8>, i64, u32)>,
    /// pictures decoded and waiting for a CAPTURE buffer, and the CAPTURE buffers free (index, offset, size)
    ready: VecDeque<(usize, OutInfo)>,
    captures: VecDeque<(u32, u64, u64)>,
    layout: Option<OutLayout>,
    told: Option<CapLayout>,
    fed: VecDeque<(i64, Instant)>,
    headers: Vec<u8>,
    resend: bool,
    /// frames fed that the codec could not take (too large for its input buffer) or came out without a picture
    pub lost: u64,
}

impl Core {
    pub fn new(codec: u32, pool: PoolMem, sink: DecodeSink) -> Core {
        Core {
            codec,
            pool,
            sink,
            free_inputs: VecDeque::new(),
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            captures: VecDeque::new(),
            layout: None,
            told: None,
            fed: VecDeque::with_capacity(TIMED),
            headers: Vec::new(),
            resend: false,
            lost: 0,
        }
    }

    /// A frame fed: it waits for an input buffer. Its headers are kept; after a drain the kept ones go before it.
    pub fn feed(&mut self, c: &mut dyn DecCodec, data: &[u8], pts: i64) {
        let mut frame = Vec::with_capacity(data.len() + self.headers.len());
        match stream_headers(self.codec, data) {
            Some(h) => self.headers = h,
            None if self.resend => frame.extend_from_slice(&self.headers),
            None => {}
        }
        self.resend = false;
        frame.extend_from_slice(data);
        if self.fed.len() >= TIMED {
            self.fed.pop_front();
        }
        self.fed.push_back((pts, Instant::now()));
        self.pending.push_back((frame, pts, 0));
        self.pump_inputs(c);
    }

    /// The end of the stream is queued after the frames fed: every picture comes out, then the end's buffer.
    pub fn drain(&mut self, c: &mut dyn DecCodec) {
        self.pending.push_back((Vec::new(), 0, DEC_FLAG_END_OF_STREAM));
        self.pump_inputs(c);
    }

    /// MediaCodec was flushed (after a drain): its buffers' indices are gone; the next frame gets the headers again.
    pub fn flushed(&mut self) {
        self.free_inputs.clear();
        self.ready.clear();
        self.resend = !self.headers.is_empty();
    }

    pub fn input_available(&mut self, c: &mut dyn DecCodec, index: usize) {
        self.free_inputs.push_back(index);
        self.pump_inputs(c);
    }

    fn pump_inputs(&mut self, c: &mut dyn DecCodec) {
        while !self.free_inputs.is_empty() && !self.pending.is_empty() {
            let (index, (data, pts, flags)) =
                (self.free_inputs.pop_front().unwrap(), self.pending.pop_front().unwrap());
            let size = match c.input_buffer(index) {
                Some(buf) if buf.len() >= data.len() => {
                    buf[..data.len()].copy_from_slice(&data);
                    data.len()
                }
                _ => {
                    self.lost += 1; // larger than the codec's input buffer (max-input-size): never decoded
                    0
                }
            };
            if !c.queue_input(index, size, pts, flags) {
                self.lost += 1;
            }
        }
    }

    /// The codec's output format changed: the device is told when the guest's layout changes.
    pub fn format_changed(&mut self, layout: Option<OutLayout>) {
        self.layout = layout;
        if let Some(cap) = layout.map(|l| l.capture()).filter(|c| self.told != Some(*c)) {
            self.told = Some(cap);
            (self.sink)(DecEvent::Format(cap));
        }
    }

    pub fn output_available(&mut self, c: &mut dyn DecCodec, index: usize, info: OutInfo) {
        if info.size == 0 {
            c.release_output(index);
            if info.flags & DEC_FLAG_END_OF_STREAM != 0 {
                (self.sink)(DecEvent::Eos);
            }
            return;
        }
        self.ready.push_back((index, info));
        self.pump_outputs(c);
    }

    /// A CAPTURE buffer of the guest's is free to fill.
    pub fn capture(&mut self, c: &mut dyn DecCodec, index: u32, offset: u64, size: u64) {
        self.captures.retain(|(i, _, _)| *i != index);
        self.captures.push_back((index, offset, size));
        self.pump_outputs(c);
    }

    pub fn reset_capture(&mut self) {
        self.captures.clear();
    }

    fn decode_us(&mut self, pts: i64) -> u32 {
        match self.fed.iter().position(|(p, _)| *p == pts) {
            Some(i) => {
                let (_, at) = self.fed.remove(i).unwrap();
                u32::try_from(at.elapsed().as_micros()).unwrap_or(u32::MAX).max(1)
            }
            None => 0,
        }
    }

    fn pump_outputs(&mut self, c: &mut dyn DecCodec) {
        while !self.ready.is_empty() && !self.captures.is_empty() {
            let ((index, info), (capture, offset, size)) =
                (self.ready.pop_front().unwrap(), self.captures.pop_front().unwrap());
            // SAFETY: a CAPTURE buffer the guest queued: nobody touches it until the DQBUF this frame becomes.
            let dst = unsafe { self.pool.slice(offset, size) };
            let placed = match (self.layout, c.output_buffer(index), dst) {
                (Some(layout), Some(out), Some(dst)) => {
                    out.get(info.offset..info.offset + info.size).and_then(|src| layout.place(src, dst))
                }
                _ => None,
            };
            c.release_output(index);
            let decode_us = self.decode_us(info.pts);
            let (bytes, error) = match placed {
                Some(n) => (n as u32, false),
                None => {
                    self.lost += 1;
                    (0, true)
                }
            };
            (self.sink)(DecEvent::Frame { index: capture, bytes, pts: info.pts, error, decode_us });
            if info.flags & DEC_FLAG_END_OF_STREAM != 0 {
                (self.sink)(DecEvent::Eos);
            }
        }
    }
}

/// The stand-in decoder's codec: no decoding. Every frame comes out as a picture whose luma is the frame's bytes summed
/// (mod 256) and whose chroma is 128, one frame late (the previous frame's comes out when a new one is fed; a drain
/// brings out the last), so the device's and the guest's paths (SOURCE_CHANGE, CAPTURE, a drain) run as with a real
/// decoder. Its pictures are `width` x `height` at a stride of the width rounded up to 64, in a buffer 16 rows taller.
pub struct StubCodec {
    width: u32,
    height: u32,
    inputs: Vec<Vec<u8>>,
    queued: VecDeque<(usize, usize, i64, u32)>,
    held: Option<(i64, u8)>,
    /// pictures (pts, luma, flags) and the end (None) decoded while every output buffer was taken: they come out
    /// in order as buffers are released, as MediaCodec holds them (it never drops one)
    waiting: VecDeque<Option<(i64, u8, u32)>>,
    outputs: Vec<Option<Vec<u8>>>,
    /// output buffers ready, for the caller to hand to the core (index, info)
    pub out: VecDeque<(usize, OutInfo)>,
}

const STUB_BUFFERS: usize = 4;

impl StubCodec {
    pub fn new(width: u32, height: u32) -> StubCodec {
        StubCodec {
            width,
            height,
            inputs: vec![vec![0; 2 << 20]; STUB_BUFFERS],
            queued: VecDeque::new(),
            held: None,
            waiting: VecDeque::new(),
            outputs: vec![None; STUB_BUFFERS],
            out: VecDeque::new(),
        }
    }

    pub fn layout(&self) -> OutLayout {
        let stride = self.width.div_ceil(64) * 64;
        OutLayout {
            width: self.width,
            height: self.height,
            left: 0,
            top: 0,
            stride,
            slice: self.height + 16,
            sample: 1,
        }
    }

    /// The input buffers free at the start.
    pub fn first_inputs(&self) -> std::ops::Range<usize> {
        0..STUB_BUFFERS
    }

    /// A picture (or the end, None) comes out after those that wait for an output buffer.
    fn picture(&mut self, p: Option<(i64, u8, u32)>) {
        self.waiting.push_back(p);
        self.place_waiting();
    }

    /// The waiting pictures into the free output buffers, in order; the end needs no buffer. Before this, a picture
    /// with every buffer taken was dropped: with the end on it (the last frame of a drain), no end came out, and
    /// `encctl decode` on a host failed 1 run in 3 ("nothing from the decoder for 5 s: 4 of 5 frames out", v0.1.21's
    /// first release run): the guest had not queued its capture buffers yet when the drain came.
    fn place_waiting(&mut self) {
        while let Some(&front) = self.waiting.front() {
            let Some((pts, luma, flags)) = front else {
                self.waiting.pop_front();
                self.out.push_back((usize::MAX, OutInfo { flags: DEC_FLAG_END_OF_STREAM, ..OutInfo::default() }));
                continue;
            };
            let Some(slot) = self.outputs.iter().position(Option::is_none) else { return };
            self.waiting.pop_front();
            let l = self.layout();
            let mut buf = vec![128u8; l.source_bytes()];
            buf[..(l.stride * l.slice) as usize].fill(luma);
            let size = buf.len();
            self.outputs[slot] = Some(buf);
            self.out.push_back((slot, OutInfo { offset: 0, size, pts, flags }));
        }
    }

    /// The queued inputs decoded: their pictures (one frame late) and input buffers back. The freed input indices.
    pub fn run(&mut self) -> Vec<usize> {
        self.place_waiting();
        let mut freed = Vec::new();
        while let Some((index, size, pts, flags)) = self.queued.pop_front() {
            freed.push(index);
            if flags & DEC_FLAG_END_OF_STREAM != 0 {
                let last = self.held.take().map(|(p, luma)| (p, luma, DEC_FLAG_END_OF_STREAM));
                self.picture(last);
                continue;
            }
            let luma = self.inputs[index][..size].iter().fold(0u8, |a, b| a.wrapping_add(*b));
            if let Some((p, l)) = self.held.replace((pts, luma)) {
                self.picture(Some((p, l, 0)));
            }
        }
        freed
    }
}

impl DecCodec for StubCodec {
    fn input_buffer(&mut self, index: usize) -> Option<&mut [u8]> {
        self.inputs.get_mut(index).map(Vec::as_mut_slice)
    }

    fn queue_input(&mut self, index: usize, size: usize, pts: i64, flags: u32) -> bool {
        self.queued.push_back((index, size, pts, flags));
        true
    }

    fn output_buffer(&mut self, index: usize) -> Option<&[u8]> {
        self.outputs.get(index).and_then(Option::as_deref)
    }

    fn release_output(&mut self, index: usize) {
        if let Some(o) = self.outputs.get_mut(index) {
            *o = None;
        }
    }
}

/// A stand-in decoder session: [`Core`] over a [`StubCodec`], run at each call.
pub struct StubDecoder {
    core: Core,
    codec: StubCodec,
}

impl StubDecoder {
    pub fn new(codec: u32, width: u32, height: u32, pool: PoolMem, sink: DecodeSink) -> StubDecoder {
        let mut d = StubDecoder { core: Core::new(codec, pool, sink), codec: StubCodec::new(width, height) };
        let layout = d.codec.layout();
        for i in d.codec.first_inputs() {
            d.core.input_available(&mut d.codec, i);
        }
        d.core.format_changed(Some(layout)); // a real codec says it before its first picture
        d
    }

    fn run(&mut self) {
        loop {
            let freed = self.codec.run();
            let outs: Vec<(usize, OutInfo)> = self.codec.out.drain(..).collect();
            if freed.is_empty() && outs.is_empty() {
                break;
            }
            for (index, info) in outs {
                if index == usize::MAX {
                    (self.core.sink)(DecEvent::Eos);
                } else {
                    self.core.output_available(&mut self.codec, index, info);
                }
            }
            for i in freed {
                self.core.input_available(&mut self.codec, i);
            }
        }
    }
}

impl Decoder for StubDecoder {
    fn feed(&mut self, data: &[u8], pts: i64) -> Result<(), i32> {
        self.core.feed(&mut self.codec, data, pts);
        self.run();
        Ok(())
    }

    fn capture(&mut self, index: u32, offset: u64, size: u64) {
        self.core.capture(&mut self.codec, index, offset, size);
        self.run();
    }

    fn reset_capture(&mut self) {
        self.core.reset_capture();
    }

    fn drain(&mut self) -> Result<(), i32> {
        self.core.drain(&mut self.codec);
        self.run();
        Ok(())
    }

    fn restart(&mut self) -> Result<(), i32> {
        // a real codec is flushed here: its buffers and the held picture are gone
        self.codec.held = None;
        self.codec.waiting.clear();
        self.codec.out.clear();
        self.codec.outputs.iter_mut().for_each(|o| *o = None);
        self.core.flushed();
        for i in self.codec.first_inputs() {
            self.core.input_available(&mut self.codec, i);
        }
        Ok(())
    }
}

/// The stand-in decoder backend ("stand-in"): xnuhost's own on a PC boot (no MediaCodec), and the helper's on a host.
pub struct StubDecodeBackend {
    pool: PoolMem,
}

impl StubDecodeBackend {
    pub fn new(pool: PoolMem) -> StubDecodeBackend {
        StubDecodeBackend { pool }
    }
}

impl DecodeBackend for StubDecodeBackend {
    fn name(&self) -> &'static str {
        "stand-in"
    }

    fn open_decoder(&self, codec: u32, width: u32, height: u32, sink: DecodeSink) -> Option<Box<dyn Decoder>> {
        if decoder_index(codec).is_none() || width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return None;
        }
        Some(Box::new(StubDecoder::new(codec, width, height, self.pool, sink)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn layout(w: u32, h: u32, stride: u32, slice: u32) -> OutLayout {
        OutLayout { width: w, height: h, left: 0, top: 0, stride, slice, sample: 1 }
    }

    #[test]
    fn a_picture_is_placed_at_the_codecs_stride_plane_by_plane() {
        // 4x4 at stride 8, slice 6: luma rows 0..4 of 8 bytes, chroma at 8*6
        let l = layout(4, 4, 8, 6);
        let mut src = vec![0u8; 8 * 6 + 8 * 2];
        for (i, b) in src.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut dst = vec![0xeeu8; 8 * 6];
        assert_eq!(l.place(&src, &mut dst), Some(8 * 6));
        assert_eq!(&dst[..32], &src[..32]);
        assert_eq!(&dst[32..48], &src[48..64]);
        // too small either way
        assert_eq!(l.place(&src[..59], &mut dst), None);
        assert_eq!(l.place(&src, &mut dst[..47]), None);
        // the last chroma row may stop at the picture's width
        assert_eq!(l.place(&src[..8 * 6 + 8 + 4], &mut dst), Some(48));
    }

    #[test]
    fn a_cropped_or_ten_bit_picture_is_placed_row_by_row() {
        // crop (2, 2)-(5, 3): a 4x2 part of an 8x4 picture at stride 8, slice 4
        let l = OutLayout::from_format(8, 4, Some(8), Some(4), Some(21), Some((2, 2, 5, 3))).unwrap();
        assert_eq!((l.width, l.height, l.left, l.top), (4, 2, 2, 2));
        let src: Vec<u8> = (0..48).collect();
        let mut dst = vec![0u8; 8 * 3];
        assert_eq!(l.place(&src, &mut dst), Some(24));
        assert_eq!(&dst[..4], &[18, 19, 20, 21]); // row 2 from x 2
        assert_eq!(&dst[8..12], &[26, 27, 28, 29]);
        assert_eq!(&dst[16..20], &[42, 43, 44, 45]); // chroma row 1 (top / 2) from x 2
                                                     // P010: the high bytes, at the width rounded to 64
        let p =
            OutLayout::from_format(2, 2, Some(4), Some(2), Some(crate::backend_ndk::COLOR_FORMAT_P010), None).unwrap();
        assert_eq!((p.sample, p.capture().stride), (2, 64));
        let src = [0x40, 1, 0x40, 2, 0x40, 3, 0x40, 4, 0x40, 5, 0x40, 6];
        let mut dst = vec![0u8; 64 * 3];
        assert_eq!(p.place(&src[..12], &mut dst), Some(192));
        assert_eq!((&dst[..2], &dst[64..66], &dst[128..130]), (&[1u8, 2][..], &[3u8, 4][..], &[5u8, 6][..]));
        // no size, an odd crop, a stride too small: what MediaCodec says is taken where it holds
        assert!(OutLayout::from_format(0, 4, None, None, None, None).is_none());
        assert!(OutLayout::from_format(8, 4, None, None, None, Some((0, 0, 8, 3))).is_none());
        assert_eq!(OutLayout::from_format(8, 4, Some(4), Some(2), None, None).unwrap().stride, 8);
    }

    #[test]
    fn the_headers_of_each_codec_are_found() {
        let h264 = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9, 0];
        assert_eq!(stream_headers(MEDIA_PIXFMT_H264, &h264).unwrap(), [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
        assert_eq!(stream_headers(MEDIA_PIXFMT_H264, &[0, 0, 1, 0x41, 7]), None);
        // HEVC: VPS 32, SPS 33, PPS 34 (type in bits 1-6), then an IDR slice (19)
        let hevc = [0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x26, 1, 5];
        assert_eq!(stream_headers(MEDIA_PIXFMT_HEVC, &hevc).unwrap().len(), 3 * 6);
        // AV1: a temporal delimiter (2), a sequence header (1) of 3 bytes, a frame (6) of 2
        let av1 = [0x12, 0, 0x0a, 3, 7, 8, 9, 0x32, 2, 1, 1];
        assert_eq!(stream_headers(MEDIA_PIXFMT_AV1, &av1).unwrap(), [0x0a, 3, 7, 8, 9]);
        assert_eq!(stream_headers(MEDIA_PIXFMT_AV1, &[0x32, 2, 1, 1]), None);
        assert_eq!(stream_headers(MEDIA_PIXFMT_VP9, &h264), None);
    }

    fn pool(bytes: &mut [u8]) -> PoolMem {
        // SAFETY: the test's buffer outlives the decoder.
        unsafe { PoolMem::new(bytes.as_mut_ptr(), bytes.len() as u64) }.unwrap()
    }

    fn sink() -> (DecodeSink, Arc<Mutex<Vec<DecEvent>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = Arc::clone(&got);
        (Arc::new(move |e| g.lock().unwrap().push(e)), got)
    }

    #[test]
    fn the_stand_in_decodes_one_frame_late_drains_and_sends_its_headers_again_after_a_restart() {
        let mut mem = vec![0u8; 1 << 20];
        let (s, got) = sink();
        let mut d = StubDecodeBackend::new(pool(&mut mem)).open_decoder(MEDIA_PIXFMT_H264, 64, 32, s).unwrap();
        let cap = CapLayout { width: 64, height: 32, stride: 64 };
        assert_eq!(got.lock().unwrap()[..], [DecEvent::Format(cap)]);
        d.capture(0, 0, cap.bytes());
        d.capture(1, 65536, cap.bytes());
        d.feed(&[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3], 10).unwrap();
        d.feed(&[0, 0, 0, 1, 0x41, 4], 11).unwrap();
        {
            let g = got.lock().unwrap();
            assert_eq!(g.len(), 2); // frame 10 out once 11 came
            assert!(matches!(g[1], DecEvent::Frame { index: 0, pts: 10, error: false, .. }));
        }
        let first = [0u8, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
        assert_eq!(mem[0], first.iter().fold(0u8, |a, b| a.wrapping_add(*b))); // the luma: the frame's bytes summed
        assert_eq!(mem[64 * 32], 128);
        d.drain().unwrap();
        {
            let g = got.lock().unwrap();
            assert!(matches!(g[2], DecEvent::Frame { index: 1, pts: 11, .. }));
            assert_eq!(g[3], DecEvent::Eos);
        }
        // after the restart the kept headers go before the next frame (summed into its luma)
        d.restart().unwrap();
        d.capture(0, 0, cap.bytes());
        d.feed(&[0, 0, 0, 1, 0x41, 5], 12).unwrap();
        d.drain().unwrap();
        let g = got.lock().unwrap();
        assert!(matches!(g[4], DecEvent::Frame { index: 0, pts: 12, .. }), "{g:?}");
        let headers: u8 = [0u8, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2].iter().fold(0, |a, b| a.wrapping_add(*b));
        assert_eq!(mem[0], headers.wrapping_add(0x41 + 5 + 1));
        assert_eq!(g[5], DecEvent::Eos);
    }

    #[test]
    fn a_picture_waits_for_a_capture_buffer_and_a_frame_for_an_input_buffer() {
        let mut mem = vec![0u8; 1 << 20];
        let (s, got) = sink();
        let mut c = StubCodec::new(64, 32);
        let mut core = Core::new(MEDIA_PIXFMT_VP9, pool(&mut mem), s);
        core.format_changed(Some(c.layout()));
        core.feed(&mut c, &[1, 2, 3], 1); // no input buffer yet: waits
        assert!(c.queued.is_empty());
        core.input_available(&mut c, 0);
        assert_eq!(c.queued.len(), 1);
        core.feed(&mut c, &[4], 2);
        c.run();
        core.input_available(&mut c, 0);
        c.run();
        let (index, info) = c.out.pop_front().unwrap();
        core.output_available(&mut c, index, info); // no CAPTURE buffer: it waits
        assert_eq!(got.lock().unwrap().len(), 1);
        let cap = c.layout().capture();
        core.capture(&mut c, 3, 4096, cap.bytes());
        let g = got.lock().unwrap();
        assert!(matches!(g[1], DecEvent::Frame { index: 3, pts: 1, error: false, decode_us: 1.., .. }), "{g:?}");
        assert_eq!(mem[4096], 6);
        assert!(c.outputs.iter().all(Option::is_none)); // released
    }

    #[test]
    fn the_stand_in_keeps_a_picture_while_every_output_buffer_is_taken() {
        // v0.1.21's first release run: the last picture of a drain came while the guest had no capture buffer yet,
        // every output buffer was taken, and the picture with the end on it was dropped: no end came out.
        let mut c = StubCodec::new(64, 32);
        let frames = STUB_BUFFERS + 2;
        for i in 0..frames {
            c.queue_input(i % STUB_BUFFERS, 1, i as i64, 0);
            c.run();
        }
        c.queue_input(0, 0, 0, DEC_FLAG_END_OF_STREAM);
        c.run();
        // STUB_BUFFERS pictures out, none released: the rest (the last with the end on it) wait
        assert_eq!(c.out.len(), STUB_BUFFERS);
        assert_eq!(c.waiting.len(), frames - STUB_BUFFERS);
        let mut seen = Vec::new();
        while let Some((index, info)) = c.out.pop_front() {
            seen.push((info.pts, info.flags));
            c.release_output(index);
            c.run();
        }
        let want: Vec<(i64, u32)> =
            (0..frames as i64).map(|p| (p, if p == frames as i64 - 1 { DEC_FLAG_END_OF_STREAM } else { 0 })).collect();
        assert_eq!(seen, want);
        assert!(c.waiting.is_empty());
        // with nothing held, the end comes out after the waiting pictures, without a buffer
        let mut c = StubCodec::new(64, 32);
        c.outputs.iter_mut().for_each(|o| *o = Some(Vec::new()));
        c.queue_input(0, 1, 7, 0);
        c.run();
        c.queue_input(1, 1, 8, 0);
        c.run(); // picture 7 waits
        c.held = None; // as if picture 8 had come out with the end already
        c.queue_input(2, 0, 0, DEC_FLAG_END_OF_STREAM);
        c.run();
        assert!(c.out.is_empty() && c.waiting.len() == 2);
        c.release_output(0);
        c.run();
        let got: Vec<usize> = c.out.iter().map(|(i, _)| *i).collect();
        assert_eq!(got, vec![0, usize::MAX]);
    }
}
