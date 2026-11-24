// SPDX-License-Identifier: MIT
//! The Android decoder backend (2026-11-24; README.md "The decoder"): the NDK's AMediaCodec in asynchronous mode, in the
//! oneios-encoder helper process. "video/avc", "video/hevc", "video/x-vnd.on2.vp9" and "video/av01" reach the phone's
//! own decoders (c2.qti.* on the S24 Ultra: [I] the codec store's first decoder of the type, `createDecoderByType`; the
//! open's line names it). ByteBuffer output asked as NV12 (COLOR_FormatYUV420SemiPlanar), with no surface: each picture
//! is copied from MediaCodec's output buffer straight into a CAPTURE buffer of the guest's, in the media pool the helper
//! maps ([`crate::decoder::Core`] does the copy, the format's stride, slice height and crop as MediaCodec says them; a
//! P010 picture of a 10-bit stream comes out 8-bit, its high bytes).
//!
//! No thread of ours waits for MediaCodec: its callbacks (onAsyncInputAvailable, onAsyncOutputAvailable,
//! onAsyncFormatChanged, onAsyncError, on the NDK's own looper thread) take the session's lock and run the core, which
//! feeds the waiting frames and places the waiting pictures; the helper's main thread does the same for the device's
//! messages. A drain queues the end of the stream; the start after it flushes and starts the codec again (MediaCodec
//! wants both in asynchronous mode), and the core puts the stream's headers before the next frame.
//!
//! [I] not yet run on the phone: that c2.qti's decoders take in-band parameter sets in the first frame (the guest sends
//! them so), give NV12 in ByteBuffer mode, and keep the pts. `encctl decode` runs the whole path on the phone.
#[cfg(target_os = "android")]
pub use ffi::NdkDecodeBackend;

/// The log line a decoder writes (an error when the first is true): the helper's link.
pub type DecodeLog = std::sync::Arc<dyn Fn(bool, &str) + Send + Sync>;

#[cfg(target_os = "android")]
mod ffi {
    use super::DecodeLog;
    use crate::backend_ndk::{set_open_step, BufferInfo, COLOR_FORMAT_NV12};
    use crate::decoder::{Core, DecCodec, DecodeBackend, DecodeSink, Decoder, OutInfo, OutLayout, PoolMem};
    use crate::proto::*;
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;
    use xh_core::thread::lock;

    #[repr(C)]
    struct AMediaCodec {
        _p: [u8; 0],
    }
    #[repr(C)]
    struct AMediaFormat {
        _p: [u8; 0],
    }
    const AMEDIA_OK: i32 = 0;

    /// AMediaCodecOnAsyncNotifyCallback (API 28), passed by value.
    #[repr(C)]
    struct Callbacks {
        input: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, i32),
        output: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, i32, *mut BufferInfo),
        format: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, *mut AMediaFormat),
        error: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, i32, i32, *const c_char),
    }

    extern "C" {
        static AMEDIAFORMAT_KEY_MIME: *const c_char;
        static AMEDIAFORMAT_KEY_WIDTH: *const c_char;
        static AMEDIAFORMAT_KEY_HEIGHT: *const c_char;
        static AMEDIAFORMAT_KEY_COLOR_FORMAT: *const c_char;
        static AMEDIAFORMAT_KEY_MAX_INPUT_SIZE: *const c_char;
        fn AMediaCodec_createDecoderByType(mime: *const c_char) -> *mut AMediaCodec;
        fn AMediaCodec_setAsyncNotifyCallback(c: *mut AMediaCodec, cb: Callbacks, user: *mut c_void) -> i32;
        fn AMediaCodec_configure(
            c: *mut AMediaCodec,
            f: *const AMediaFormat,
            surface: *mut c_void,
            crypto: *mut c_void,
            flags: u32,
        ) -> i32;
        fn AMediaCodec_start(c: *mut AMediaCodec) -> i32;
        fn AMediaCodec_stop(c: *mut AMediaCodec) -> i32;
        fn AMediaCodec_flush(c: *mut AMediaCodec) -> i32;
        fn AMediaCodec_delete(c: *mut AMediaCodec) -> i32;
        fn AMediaCodec_getName(c: *mut AMediaCodec, name: *mut *mut c_char) -> i32;
        fn AMediaCodec_releaseName(c: *mut AMediaCodec, name: *mut c_char);
        fn AMediaCodec_getOutputFormat(c: *mut AMediaCodec) -> *mut AMediaFormat;
        fn AMediaCodec_getInputBuffer(c: *mut AMediaCodec, idx: usize, size: *mut usize) -> *mut u8;
        fn AMediaCodec_queueInputBuffer(
            c: *mut AMediaCodec,
            idx: usize,
            offset: libc::off_t,
            size: usize,
            time: u64,
            flags: u32,
        ) -> i32;
        fn AMediaCodec_getOutputBuffer(c: *mut AMediaCodec, idx: usize, size: *mut usize) -> *mut u8;
        fn AMediaCodec_releaseOutputBuffer(c: *mut AMediaCodec, idx: usize, render: bool) -> i32;
        fn AMediaFormat_new() -> *mut AMediaFormat;
        fn AMediaFormat_delete(f: *mut AMediaFormat) -> i32;
        fn AMediaFormat_setString(f: *mut AMediaFormat, name: *const c_char, value: *const c_char);
        fn AMediaFormat_setInt32(f: *mut AMediaFormat, name: *const c_char, value: i32);
        fn AMediaFormat_getInt32(f: *mut AMediaFormat, name: *const c_char, out: *mut i32) -> bool;
        fn AMediaFormat_getRect(
            f: *mut AMediaFormat,
            name: *const c_char,
            left: *mut i32,
            top: *mut i32,
            right: *mut i32,
            bottom: *mut i32,
        ) -> bool;
    }

    fn mime(codec: u32) -> &'static CStr {
        match codec {
            MEDIA_PIXFMT_HEVC => c"video/hevc",
            MEDIA_PIXFMT_VP9 => c"video/x-vnd.on2.vp9",
            MEDIA_PIXFMT_AV1 => c"video/av01",
            _ => c"video/avc",
        }
    }

    /// MediaCodec's buffers by index, as the core uses them (any thread: the NDK's calls are thread-safe).
    struct NdkCodec(*mut AMediaCodec);

    impl DecCodec for NdkCodec {
        fn input_buffer(&mut self, index: usize) -> Option<&mut [u8]> {
            let mut cap = 0;
            // SAFETY: an input buffer MediaCodec gave us (onAsyncInputAvailable): ours until it is queued.
            unsafe {
                let p = AMediaCodec_getInputBuffer(self.0, index, &mut cap);
                (!p.is_null()).then(|| std::slice::from_raw_parts_mut(p, cap))
            }
        }

        fn queue_input(&mut self, index: usize, size: usize, pts: i64, flags: u32) -> bool {
            // SAFETY: an input buffer of ours.
            unsafe { AMediaCodec_queueInputBuffer(self.0, index, 0, size, pts as u64, flags) == AMEDIA_OK }
        }

        fn output_buffer(&mut self, index: usize) -> Option<&[u8]> {
            let mut cap = 0;
            // SAFETY: an output buffer MediaCodec gave us (onAsyncOutputAvailable): readable until released.
            unsafe {
                let p = AMediaCodec_getOutputBuffer(self.0, index, &mut cap);
                (!p.is_null()).then(|| std::slice::from_raw_parts(p, cap))
            }
        }

        fn release_output(&mut self, index: usize) {
            // SAFETY: an output buffer of ours.
            unsafe { AMediaCodec_releaseOutputBuffer(self.0, index, false) };
        }
    }

    /// One session: its codec, its core, whether a flush runs (the callbacks then wait for the next start).
    struct Shared {
        codec: *mut AMediaCodec,
        core: Mutex<Core>,
        flushing: AtomicBool,
        log: DecodeLog,
        what: String,
        errors: Mutex<u32>,
    }

    /// The callbacks' userdata.
    ///
    /// # Safety
    /// `user` is a session's `Shared`, alive until the codec is deleted (the decoder's drop, before it frees it).
    unsafe fn shared<'a>(user: *mut c_void) -> &'a Shared {
        // SAFETY: the caller's promise.
        unsafe { &*(user as *const Shared) }
    }

    unsafe extern "C" fn on_input(codec: *mut AMediaCodec, user: *mut c_void, index: i32) {
        // SAFETY: the session's userdata (set_async's).
        let s = unsafe { shared(user) };
        if s.flushing.load(Ordering::Acquire) || index < 0 {
            return;
        }
        lock(&s.core).input_available(&mut NdkCodec(codec), index as usize);
    }

    unsafe extern "C" fn on_output(codec: *mut AMediaCodec, user: *mut c_void, index: i32, info: *mut BufferInfo) {
        // SAFETY: the session's userdata; MediaCodec's description of the buffer, valid during the call.
        let (s, info) = unsafe { (shared(user), *info) };
        if s.flushing.load(Ordering::Acquire) || index < 0 {
            return;
        }
        let info = OutInfo {
            offset: usize::try_from(info.offset).unwrap_or(0),
            size: usize::try_from(info.size).unwrap_or(0),
            pts: info.presentation_time_us,
            flags: info.flags,
        };
        lock(&s.core).output_available(&mut NdkCodec(codec), index as usize, info);
    }

    /// The output format as the core takes it (the callback's own format is MediaCodec's to free: it is read again).
    unsafe extern "C" fn on_format(codec: *mut AMediaCodec, user: *mut c_void, _format: *mut AMediaFormat) {
        // SAFETY: the session's userdata; a format of ours, deleted here.
        let (s, layout, said) = unsafe {
            let s = shared(user);
            let f = AMediaCodec_getOutputFormat(codec);
            if f.is_null() {
                return;
            }
            let int = |key: &CStr| {
                let mut v = 0;
                AMediaFormat_getInt32(f, key.as_ptr(), &mut v).then_some(v)
            };
            let (w, h) = (int(c"width").unwrap_or(0), int(c"height").unwrap_or(0));
            let (stride, slice, color) = (int(c"stride"), int(c"slice-height"), int(c"color-format"));
            let (mut l, mut t, mut r, mut b) = (0, 0, 0, 0);
            let crop =
                AMediaFormat_getRect(f, c"crop".as_ptr(), &mut l, &mut t, &mut r, &mut b).then_some((l, t, r, b));
            AMediaFormat_delete(f);
            let layout = OutLayout::from_format(w, h, stride, slice, color, crop);
            let said = format!(
                "decoder: {}: output {w}x{h}, stride {}, slice height {}, color format {:#x}, crop {:?}: {}",
                s.what,
                stride.unwrap_or(-1),
                slice.unwrap_or(-1),
                color.unwrap_or(-1),
                crop,
                if layout.is_some() { "taken" } else { "NOT TAKEN (no picture comes out)" }
            );
            (s, layout, said)
        };
        (s.log)(layout.is_none(), &said);
        lock(&s.core).format_changed(layout);
    }

    unsafe extern "C" fn on_error(
        _codec: *mut AMediaCodec,
        user: *mut c_void,
        error: i32,
        action: i32,
        detail: *const c_char,
    ) {
        // SAFETY: the session's userdata; MediaCodec's C string, valid during the call.
        let (s, detail) = unsafe {
            let detail = if detail.is_null() { "".into() } else { CStr::from_ptr(detail).to_string_lossy() };
            (shared(user), detail.into_owned())
        };
        let mut n = lock(&s.errors);
        *n += 1;
        if *n <= 3 {
            (s.log)(true, &format!("decoder: {}: MediaCodec error {error} (action {action}) {detail}", s.what));
        }
    }

    /// A started decoder; stopped and deleted on drop, its callbacks' userdata freed after.
    struct NdkDecoder {
        shared: *mut Shared,
    }

    // SAFETY: the codec and the core are used under the core's lock or by the NDK's thread-safe calls.
    unsafe impl Send for NdkDecoder {}

    impl NdkDecoder {
        fn shared(&self) -> &Shared {
            // SAFETY: ours until drop.
            unsafe { &*self.shared }
        }
    }

    impl Decoder for NdkDecoder {
        fn feed(&mut self, data: &[u8], pts: i64) -> Result<(), i32> {
            let s = self.shared();
            lock(&s.core).feed(&mut NdkCodec(s.codec), data, pts);
            Ok(())
        }

        fn capture(&mut self, index: u32, offset: u64, size: u64) {
            let s = self.shared();
            lock(&s.core).capture(&mut NdkCodec(s.codec), index, offset, size);
        }

        fn reset_capture(&mut self) {
            lock(&self.shared().core).reset_capture();
        }

        fn drain(&mut self) -> Result<(), i32> {
            let s = self.shared();
            lock(&s.core).drain(&mut NdkCodec(s.codec));
            Ok(())
        }

        /// The flush and the start MediaCodec wants after the end of a stream, in asynchronous mode. The core's lock
        /// is not held across them: MediaCodec's flush waits for its looper, whose callback may wait for that lock.
        fn restart(&mut self) -> Result<(), i32> {
            let s = self.shared();
            s.flushing.store(true, Ordering::Release);
            // SAFETY: our started codec.
            let flushed = unsafe { AMediaCodec_flush(s.codec) };
            lock(&s.core).flushed();
            s.flushing.store(false, Ordering::Release);
            // SAFETY: as above.
            let started = unsafe { AMediaCodec_start(s.codec) };
            if flushed != AMEDIA_OK || started != AMEDIA_OK {
                (s.log)(
                    true,
                    &format!("decoder: {}: the start after a drain failed (flush {flushed}, start {started})", s.what),
                );
                return Err(libc::EIO);
            }
            Ok(())
        }
    }

    impl Drop for NdkDecoder {
        fn drop(&mut self) {
            // SAFETY: our codec, stopped and deleted once (no callback runs after the delete); then its userdata.
            unsafe {
                let codec = (*self.shared).codec;
                AMediaCodec_stop(codec);
                AMediaCodec_delete(codec);
                drop(Box::from_raw(self.shared));
            }
        }
    }

    /// The backend ("mediacodec").
    pub struct NdkDecodeBackend {
        pool: PoolMem,
        log: DecodeLog,
    }

    impl NdkDecodeBackend {
        pub fn new(pool: PoolMem, log: DecodeLog) -> NdkDecodeBackend {
            NdkDecodeBackend { pool, log }
        }
    }

    impl DecodeBackend for NdkDecodeBackend {
        fn name(&self) -> &'static str {
            "mediacodec"
        }

        fn open_decoder(&self, codec: u32, width: u32, height: u32, sink: DecodeSink) -> Option<Box<dyn Decoder>> {
            decoder_index(codec)?;
            let start = Instant::now();
            let what = format!("{} {width}x{height}", codec_name(codec));
            // SAFETY: NDK calls with valid C strings; every object made here is deleted here or by the decoder's drop.
            unsafe {
                set_open_step(2);
                let c = AMediaCodec_createDecoderByType(mime(codec).as_ptr());
                if c.is_null() {
                    set_open_step(0);
                    (self.log)(true, &format!("decoder: no MediaCodec decoder for {what}"));
                    return None;
                }
                let shared = Box::into_raw(Box::new(Shared {
                    codec: c,
                    core: Mutex::new(Core::new(codec, self.pool, sink)),
                    flushing: AtomicBool::new(false),
                    log: self.log.clone(),
                    what: what.clone(),
                    errors: Mutex::new(0),
                }));
                let fail = |step: &str, rc: i32| {
                    AMediaCodec_delete(c);
                    drop(Box::from_raw(shared));
                    set_open_step(0);
                    (self.log)(true, &format!("decoder: {what}: {step} failed ({rc})"));
                    None
                };
                let callbacks = Callbacks { input: on_input, output: on_output, format: on_format, error: on_error };
                let rc = AMediaCodec_setAsyncNotifyCallback(c, callbacks, shared.cast());
                if rc != AMEDIA_OK {
                    return fail("AMediaCodec_setAsyncNotifyCallback", rc);
                }
                let f = AMediaFormat_new();
                AMediaFormat_setString(f, AMEDIAFORMAT_KEY_MIME, mime(codec).as_ptr());
                AMediaFormat_setInt32(f, AMEDIAFORMAT_KEY_WIDTH, width as i32);
                AMediaFormat_setInt32(f, AMEDIAFORMAT_KEY_HEIGHT, height as i32);
                AMediaFormat_setInt32(f, AMEDIAFORMAT_KEY_COLOR_FORMAT, COLOR_FORMAT_NV12);
                // the guest's OUTPUT buffers hold up to 4 MiB (media_device DEC_OUTPUT_MAX)
                let input = if u64::from(width) * u64::from(height) > 1920 * 1088 { 4 << 20 } else { 2 << 20 };
                AMediaFormat_setInt32(f, AMEDIAFORMAT_KEY_MAX_INPUT_SIZE, input);
                set_open_step(3);
                let rc = AMediaCodec_configure(c, f, std::ptr::null_mut(), std::ptr::null_mut(), 0);
                AMediaFormat_delete(f);
                if rc != AMEDIA_OK {
                    return fail("AMediaCodec_configure", rc);
                }
                set_open_step(4);
                let rc = AMediaCodec_start(c);
                if rc != AMEDIA_OK {
                    return fail("AMediaCodec_start", rc);
                }
                set_open_step(0);
                let mut name: *mut c_char = std::ptr::null_mut();
                let named = if AMediaCodec_getName(c, &mut name) == AMEDIA_OK && !name.is_null() {
                    let n = CStr::from_ptr(name).to_string_lossy().into_owned();
                    AMediaCodec_releaseName(c, name);
                    n
                } else {
                    "?".into()
                };
                (self.log)(false, &format!("decoder: {what}: {named}, opened in {} ms", start.elapsed().as_millis()));
                Some(Box::new(NdkDecoder { shared }))
            }
        }
    }
}
