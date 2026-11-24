// SPDX-License-Identifier: MIT
//! A stable-AIDL binder client (and, for the tests, server) with no Android library: the vendor's Wi-Fi supplicant
//! and the HALs are AIDL services on /dev/binder (docs/hardware.md section 3).
//!   [`Parcel`], [`Reader`]  the parcels: the interface token, int, String (UTF-16 on the wire), byte[], binder
//!                           objects with their stability word, the status header of a reply. Pure.
//!   [`Connection`]          calls over [`super::binder`], one looper thread for the calls into our objects,
//!                           references, death notifications.
//! Written from libbinder's Parcel.cpp, Status.cpp, Stability.cpp and IPCThreadState.cpp (AOSP, Apache 2.0) as the
//! wire format of Android 13 and later: the stability word is the level alone (VINTF 0x3f).
//! C: wifi/aidl.h, wifi/aidl_parcel.c, wifi/aidl_binder.c.

use super::binder::{self, Binder, Handler, Incoming, Looper, OpenError, Outgoing};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use xh_core::bytes::ne;

pub const TYPE_BINDER: u32 = binder::uapi::TYPE_BINDER;
pub const TYPE_HANDLE: u32 = binder::uapi::TYPE_HANDLE;
/// 'SYST': the interface token's marker of a system libbinder.
pub const HEADER_SYST: u32 = 0x5359_5354;
/// 'VNDR': the marker of the vendor copy of libbinder (Parcel.cpp's kHeader under __ANDROID_VNDK__). A stable-AIDL
/// HAL on libbinder_ndk (an LLNDK library: the system's libbinder in a vendor process) expects 'SYST'.
pub const HEADER_VNDR: u32 = 0x564e_4452;
/// The stability a vendor process may call.
pub const STABILITY_VINTF: i32 = 0x3f;
/// Our objects' flags: accepts descriptors, SCHED_NORMAL at nice 19 or better: never realtime.
pub const LOCAL_FLAGS: u32 = 0x113;

// Transaction codes every binder object answers.
pub const FIRST_CALL: u32 = 1;
pub const PING: u32 = 0x5f50_4e47; // '_PNG'
pub const INTERFACE: u32 = 0x5f4e_5446; // '_NTF': the reply is the descriptor
pub const GET_VERSION: u32 = 0x00ff_ffff; // stable AIDL: getInterfaceVersion
pub const GET_HASH: u32 = 0x00ff_fffe; // stable AIDL: getInterfaceHash
/// TF_ONE_WAY in an incoming call's flags.
pub const ONE_WAY: u32 = 1;

// android::status_t values a transaction can be answered with.
pub const UNKNOWN_TRANSACTION: i32 = binder::UNKNOWN_TRANSACTION;
pub const BAD_TYPE: i32 = -2_147_483_647;
// Exception codes of a reply's status header.
pub const EX_SERVICE_SPECIFIC: i32 = -8;
pub const EX_HAS_NOTED_APPOPS: i32 = -127;
pub const EX_HAS_REPLY_HEADER: i32 = -128;

pub const PARCEL_DATA: usize = 4096;
pub const PARCEL_OBJECTS: usize = 8;

/// The kernel's flat_binder_object (24 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlatObject {
    pub kind: u32,
    pub flags: u32,
    pub binder_or_handle: u64,
    pub cookie: u64,
}

impl FlatObject {
    pub const SIZE: usize = 24;
    pub fn to_bytes(self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..4].copy_from_slice(&self.kind.to_ne_bytes());
        b[4..8].copy_from_slice(&self.flags.to_ne_bytes());
        b[8..16].copy_from_slice(&self.binder_or_handle.to_ne_bytes());
        b[16..24].copy_from_slice(&self.cookie.to_ne_bytes());
        b
    }
    pub fn parse(b: &[u8]) -> FlatObject {
        let w = |at: usize| ne::u32(b, at);
        let q = |at: usize| ne::u64(b, at);
        FlatObject { kind: w(0), flags: w(4), binder_or_handle: q(8), cookie: q(16) }
    }
}

/// An outgoing parcel: at most [`PARCEL_DATA`] bytes (or the limit [`Parcel::set_limit`] gave) and
/// [`PARCEL_OBJECTS`] objects; what does not fit sets `overflow` and the parcel is never sent.
#[derive(Clone, Default)]
pub struct Parcel {
    data: Vec<u8>,
    offsets: Vec<u64>,
    overflow: bool,
    limit: usize, // 0: PARCEL_DATA
}

impl std::fmt::Debug for Parcel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parcel").field("size", &self.data.len()).field("objects", &self.offsets.len()).finish()
    }
}

impl Parcel {
    pub fn new() -> Parcel {
        Parcel::default()
    }
    /// A call's parcel: the interface token (writeInterfaceToken).
    pub fn with_token(descriptor: &str) -> Parcel {
        let mut p = Parcel::new();
        p.put_token(descriptor);
        p
    }
    /// Empties it (C's ab_init).
    pub fn clear(&mut self) {
        self.data.clear();
        self.offsets.clear();
        self.overflow = false;
    }
    /// Zeroes and empties it: a parcel that carried a key.
    pub fn wipe(&mut self) {
        for b in self.data.iter_mut() {
            // SAFETY: a valid &mut u8 of the vector; volatile so the zeroing is not optimised away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        self.clear();
    }
    pub fn size(&self) -> usize {
        self.data.len()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
    pub fn object_offsets(&self) -> &[u64] {
        &self.offsets
    }
    pub fn is_overflow(&self) -> bool {
        self.overflow
    }
    /// A reader over this parcel's bytes and objects (the tests read their own parcels).
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.data, binder::offsets_bytes(&self.offsets))
    }

    /// Room for `length` bytes, padded to 4 and zeroed: its start, None (and overflow) when it does not fit.
    fn room(&mut self, length: usize) -> Option<usize> {
        let padded = (length + 3) & !3;
        if self.overflow || padded > self.limit() - self.data.len() {
            self.overflow = true;
            return None;
        }
        let at = self.data.len();
        self.data.resize(at + padded, 0);
        Some(at)
    }

    pub fn put_i32(&mut self, value: i32) {
        if let Some(at) = self.room(4) {
            self.data[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }
    pub fn put_u32(&mut self, value: u32) {
        self.put_i32(value as i32)
    }
    pub fn put_f32(&mut self, value: f32) {
        self.put_u32(value.to_bits())
    }
    /// An int64: the low word first.
    pub fn put_i64(&mut self, value: i64) {
        self.put_u32(value as u32);
        self.put_u32((value as u64 >> 32) as u32);
    }
    /// A String: UTF-16; None is a null string. The text is ASCII (each byte one unit).
    pub fn put_string(&mut self, text: Option<&str>) {
        let Some(text) = text else {
            self.put_i32(-1);
            return;
        };
        let n = text.len();
        self.put_i32(n as i32);
        if let Some(at) = self.room((n + 1) * 2) {
            for (i, b) in text.bytes().enumerate() {
                self.data[at + 2 * i] = b; // the units and a terminator
            }
        }
    }
    pub fn put_str(&mut self, text: &str) {
        self.put_string(Some(text))
    }
    /// writeInterfaceToken: strict mode policy (none, penalties gathered), no work source, 'SYST', the descriptor.
    pub fn put_token(&mut self, descriptor: &str) {
        self.put_token_header(descriptor, HEADER_SYST);
    }
    /// writeInterfaceToken with another libbinder's marker: [`HEADER_VNDR`] for a service built on the vendor copy
    /// of libbinder, whose enforceInterface refuses any other header.
    pub fn put_token_header(&mut self, descriptor: &str, header: u32) {
        self.put_u32(0x8000_0000);
        self.put_i32(-1);
        self.put_u32(header);
        self.put_str(descriptor);
    }
    /// A parcel that may grow past [`PARCEL_DATA`] to `bytes` (a Bluetooth packet's byte[]: up to 64 KiB). Kept by
    /// [`Parcel::clear`].
    pub fn set_limit(&mut self, bytes: usize) {
        self.limit = bytes;
    }
    fn limit(&self) -> usize {
        if self.limit == 0 {
            PARCEL_DATA
        } else {
            self.limit
        }
    }
    /// A byte[] (also byte[N]).
    pub fn put_bytes(&mut self, bytes: &[u8]) {
        self.put_i32(bytes.len() as i32);
        if let Some(at) = self.room(bytes.len()) {
            self.data[at..at + bytes.len()].copy_from_slice(bytes);
        }
    }
    /// listed: in the offsets array, for the kernel to translate. A null binder is not (libbinder's writeObject).
    fn put_object(&mut self, o: FlatObject, stability: i32, listed: bool) {
        if listed && self.offsets.len() == PARCEL_OBJECTS {
            self.overflow = true;
            return;
        }
        let Some(at) = self.room(FlatObject::SIZE) else { return };
        self.data[at..at + FlatObject::SIZE].copy_from_slice(&o.to_bytes());
        if listed {
            self.offsets.push(at as u64);
        }
        self.put_i32(stability);
    }
    /// One of our objects (the id comes back with its calls).
    pub fn put_local(&mut self, object: u64) {
        let o = FlatObject { kind: TYPE_BINDER, flags: LOCAL_FLAGS, binder_or_handle: object, cookie: object };
        self.put_object(o, STABILITY_VINTF, true);
    }
    /// Someone's object we hold.
    pub fn put_handle(&mut self, handle: u32) {
        let o = FlatObject { kind: TYPE_HANDLE, flags: 0x100 | 0x13, binder_or_handle: handle as u64, cookie: 0 };
        self.put_object(o, STABILITY_VINTF, true);
    }
    pub fn put_null_binder(&mut self) {
        self.put_object(FlatObject { kind: TYPE_BINDER, flags: 0x13, ..Default::default() }, 0, false);
    }
    /// A reply's status header: no exception.
    pub fn put_ok(&mut self) {
        self.put_i32(0);
    }
    /// A reply's status header: a service-specific error (no message, no remote stack trace).
    pub fn put_service_error(&mut self, code: i32) {
        self.put_i32(EX_SERVICE_SPECIFIC);
        self.put_str("");
        self.put_i32(0);
        self.put_i32(code);
    }
    /// A non-null parcelable: its marker and size, the body from `body`, the size written after.
    pub fn put_parcelable(&mut self, body: impl FnOnce(&mut Parcel)) {
        self.put_i32(1);
        let at = self.data.len();
        self.put_i32(0);
        body(self);
        if !self.overflow {
            let size = (self.data.len() - at) as i32;
            self.data[at..at + 4].copy_from_slice(&size.to_ne_bytes());
        }
    }
}

impl Outgoing for Parcel {
    fn data(&self) -> &[u8] {
        &self.data
    }
    fn offsets(&self) -> &[u64] {
        &self.offsets
    }
    fn overflow(&self) -> bool {
        self.overflow
    }
}

/// An incoming parcel. A malformed one sets `bad`: every read after gives 0, "" or None.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    offsets: &'a [u8],
    position: usize,
    bad: bool,
}

/// A binder object read from a parcel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    /// Someone's object (a handle of ours to it).
    Handle(u32),
    /// One of the reader's own objects (its cookie).
    Local(u64),
    Null,
}

impl<'a> Reader<'a> {
    /// `offsets`: the offsets array as bytes (8 per object).
    pub fn new(data: &'a [u8], offsets: &'a [u8]) -> Reader<'a> {
        Reader { data, offsets, position: 0, bad: false }
    }
    pub fn position(&self) -> usize {
        self.position
    }
    pub fn set_position(&mut self, position: usize) {
        self.position = position;
    }
    pub fn size(&self) -> usize {
        self.data.len()
    }
    pub fn is_bad(&self) -> bool {
        self.bad
    }
    pub fn mark_bad(&mut self) {
        self.bad = true;
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let padded = length.checked_add(3).map(|n| n & !3);
        match padded {
            Some(padded) if !self.bad && padded <= self.data.len() - self.position => {
                let at = &self.data[self.position..self.position + length];
                self.position += padded;
                Some(at)
            }
            _ => {
                self.bad = true;
                None
            }
        }
    }

    pub fn i32(&mut self) -> i32 {
        self.take(4).map_or(0, |b| i32::from_ne_bytes(b.try_into().expect("4 bytes")))
    }
    pub fn u32(&mut self) -> u32 {
        self.i32() as u32
    }
    pub fn f32(&mut self) -> f32 {
        f32::from_bits(self.u32())
    }
    pub fn i64(&mut self) -> i64 {
        let low = self.u32() as u64;
        let high = self.u32() as u64;
        (high << 32 | low) as i64
    }

    /// A String as ASCII (other characters become '?'), cut to `max` characters (C: a buffer of max + 1). None
    /// (and bad) on a malformed parcel; a null string gives "".
    pub fn string(&mut self, max: usize) -> Option<String> {
        let n = self.i32();
        if self.bad {
            return None;
        }
        if n == -1 {
            return Some(String::new()); // a null string
        }
        if n < 0 || n as usize > (self.data.len() - self.position) / 2 {
            self.bad = true;
            return None;
        }
        let at = self.take((n as usize + 1) * 2)?;
        Some(
            (0..(n as usize).min(max))
                .map(|i| {
                    let unit = u16::from(at[2 * i]) | u16::from(at[2 * i + 1]) << 8;
                    if (0x20..0x7f).contains(&unit) {
                        unit as u8 as char
                    } else {
                        '?'
                    }
                })
                .collect(),
        )
    }

    /// A byte[]: a null array gives an empty slice. None (and bad) on a malformed parcel.
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.i32();
        if self.bad {
            return None;
        }
        if n == -1 {
            return Some(&[]);
        }
        if n < 0 {
            self.bad = true;
            return None;
        }
        self.take(n as usize)
    }

    /// A binder object and its stability word. The kernel translated only the objects of the offsets array: an
    /// object not in it is bad, unless it is a null binder.
    pub fn binder(&mut self) -> Object {
        let offset = self.position;
        let Some(at) = self.take(FlatObject::SIZE) else { return Object::Null };
        let o = FlatObject::parse(at);
        let _stability = self.i32();
        if self.bad {
            return Object::Null;
        }
        let listed = (0..self.offsets.len() / 8).any(|i| binder::offset_at(self.offsets, i) == Some(offset as u64));
        match o.kind {
            TYPE_HANDLE if listed => Object::Handle(o.binder_or_handle as u32),
            TYPE_BINDER if o.binder_or_handle == 0 && o.cookie == 0 => Object::Null,
            TYPE_BINDER if listed => Object::Local(o.cookie),
            _ => {
                self.bad = true;
                Object::Null
            }
        }
    }

    /// The interface token of a call: true when it names `descriptor` ('SYST' or a vendor's 'VNDR': not compared).
    pub fn token(&mut self, descriptor: &str) -> bool {
        self.token_header(descriptor).is_some()
    }
    /// As [`Reader::token`], with the token's header ('SYST', 'VNDR') when it names `descriptor` (the tests' fake
    /// services check what a client sends).
    pub fn token_header(&mut self, descriptor: &str) -> Option<u32> {
        let _policy = self.i32();
        let _work_source = self.i32();
        let header = self.u32();
        self.string(127).filter(|name| name == descriptor).map(|_| header)
    }

    /// A reply's status header: Ok(()), or Err((exception, specific)) with specific the service's error
    /// (EX_SERVICE_SPECIFIC) or the exception itself. A malformed header: Err((BAD_TYPE, ..)) and bad.
    pub fn status(&mut self) -> Result<(), (i32, i32)> {
        let mut exception = self.i32();
        if exception == EX_HAS_NOTED_APPOPS || exception == EX_HAS_REPLY_HEADER {
            // a header to skip, then the code
            let start = self.position;
            let size = self.i32();
            if self.bad || size < 4 || size as usize > self.data.len() - start {
                self.bad = true;
                return Err((BAD_TYPE, 0));
            }
            self.position = start + size as usize;
            exception = if exception == EX_HAS_REPLY_HEADER { 0 } else { self.i32() };
        }
        if self.bad {
            return Err((BAD_TYPE, 0));
        }
        if exception == 0 {
            return Ok(());
        }
        let _message = self.string(3);
        let start = self.position;
        let trace = self.i32(); // the remote stack trace's size, itself included; 0: none
        if !self.bad && trace > 4 && trace as usize <= self.data.len() - start {
            self.position = start + trace as usize;
        }
        let code = if exception == EX_SERVICE_SPECIFIC { self.i32() } else { exception };
        Err((exception, if self.bad { exception } else { code }))
    }

    /// The start of a parcelable (its non-null marker and size): the position after it, None for a null or
    /// malformed one.
    pub fn parcelable(&mut self) -> Option<usize> {
        if self.i32() != 1 || self.bad {
            return None;
        }
        let start = self.position;
        let size = self.i32();
        if self.bad || size < 4 || size as usize > self.data.len() - start {
            self.bad = true;
            return None;
        }
        Some(start + size as usize)
    }

    /// An int[]: its values (at most `max` kept), None when malformed. A null array is empty.
    pub fn i32s(&mut self, max: usize) -> Option<Vec<i32>> {
        let count = self.i32();
        if self.bad || count < 0 {
            return if count == -1 && !self.bad { Some(Vec::new()) } else { None };
        }
        let mut out = Vec::new();
        for _ in 0..count {
            let v = self.i32();
            if self.bad {
                return None;
            }
            if out.len() < max {
                out.push(v);
            }
        }
        Some(out)
    }
}

/// What every object answers: the descriptor, a ping, the interface's version and hash. Some(reply) when `code`
/// was one of them.
pub fn standard(code: u32, descriptor: &str, version: i32, hash: &str) -> Option<Parcel> {
    let mut reply = Parcel::new();
    match code {
        INTERFACE => reply.put_str(descriptor), // no status header: BBinder's own answer
        PING => {}
        GET_VERSION => {
            reply.put_ok();
            reply.put_i32(version);
        }
        GET_HASH => {
            reply.put_ok();
            reply.put_str(hash);
        }
        _ => return None,
    }
    Some(reply)
}

/// A failed call: `rc` a negative errno (-ETIMEDOUT: no reply in 5 s; -EPIPE: the object's process died;
/// -ENOSYS: it does not know the call; -EPROTO: a reply this client cannot read; -EREMOTEIO: an exception, with
/// `specific` the service's error code, or the exception's when it is not a service-specific one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallError {
    pub rc: i32,
    pub specific: i32,
}

impl From<i32> for CallError {
    fn from(rc: i32) -> CallError {
        CallError { rc, specific: 0 }
    }
}

/// A call into one of our objects: `object` its id (0 with a context manager: a call to handle 0), `one_way`
/// whether the caller waits. Ok(reply) (not sent for a one-way call), or Err(the status to answer with:
/// UNKNOWN_TRANSACTION, BAD_TYPE).
pub type IncomingFn = dyn Fn(u64, u32, bool, &mut Reader<'_>) -> Result<Parcel, i32> + Send + Sync;
/// The process of an object we watch died: the cookie given to [`Connection::watch`].
pub type DeathFn = dyn Fn(u64) + Send + Sync;

struct AidlHandler {
    incoming: Option<Box<IncomingFn>>,
    death: Option<Box<DeathFn>>,
}

impl Handler for AidlHandler {
    fn transaction(&self, tr: Incoming<'_>) {
        let (data, offsets) = tr.parts();
        let mut r = Reader::new(data, offsets);
        let answer = match &self.incoming {
            Some(f) => f(tr.cookie(), tr.code(), tr.one_way(), &mut r),
            None => Err(UNKNOWN_TRANSACTION),
        };
        match answer {
            Ok(reply) => tr.finish(Ok(&reply)),
            Err(status) => tr.finish(Err(status)),
        }
    }
    fn dead(&self, cookie: u64) {
        if let Some(f) = &self.death {
            f(cookie)
        }
    }
}

/// A connection to /dev/binder: our objects' calls on its looper thread, our calls one at a time from any thread.
pub struct Connection {
    looper: Option<Looper>, // first: joined before the binder closes
    handler: Arc<AidlHandler>,
    binder: Arc<Binder>,
    call_lock: Mutex<()>,
}

impl Connection {
    /// Opens the binder device (None: ONEIOS_BINDER, else /dev/binder) and starts the looper thread.
    pub fn open(
        device: Option<&str>,
        incoming: Option<Box<IncomingFn>>,
        death: Option<Box<DeathFn>>,
    ) -> Result<Connection, OpenError> {
        let path = device.map_or_else(default_device, str::to_owned);
        let binder = Arc::new(Binder::open(Path::new(&path))?);
        let handler = Arc::new(AidlHandler { incoming, death });
        // A looper that did not start: calls still work, our objects are not served (as the C's looping flag).
        let looper = Looper::start(Arc::clone(&binder), handler.clone()).ok();
        Ok(Connection { looper, handler, binder, call_lock: Mutex::new(()) })
    }

    pub fn binder(&self) -> &Binder {
        &self.binder
    }

    /// The tests' service manager.
    pub fn context_manager(&self) -> Result<(), i32> {
        self.binder.context_manager()
    }

    /// A call and its reply. `on_reply` sees the reply after its status header (none for INTERFACE and PING), before
    /// its buffer is given back: it takes its references to the handles it keeps ([`Connection::acquire`]) there.
    pub fn call<T>(
        &self,
        handle: u32,
        code: u32,
        p: &Parcel,
        on_reply: impl FnOnce(&mut Reader<'_>) -> Result<T, i32>,
    ) -> Result<T, CallError> {
        if p.is_overflow() {
            return Err((-libc::E2BIG).into());
        }
        let _one = self.call_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let reply = self.binder.transact(handle, code, p, &*self.handler)?;
        if let Some(status) = reply.status_code() {
            let rc = if status < 0 { status } else { -libc::EIO };
            return Err(match rc {
                UNKNOWN_TRANSACTION => -libc::ENOSYS,
                rc if rc < -4095 => -libc::EPROTO, // android's own status values
                rc => rc,
            }
            .into());
        }
        let (data, offsets) = reply.parts();
        let mut r = Reader::new(data, offsets);
        if code != INTERFACE && code != PING {
            match r.status() {
                _ if r.is_bad() => return Err((-libc::EPROTO).into()),
                Err((_, specific)) => return Err(CallError { rc: -libc::EREMOTEIO, specific }),
                Ok(()) => {}
            }
        }
        on_reply(&mut r).map_err(CallError::from)
    }

    /// A call whose reply carries nothing this client reads.
    pub fn call_void(&self, handle: u32, code: u32, p: &Parcel) -> Result<(), CallError> {
        self.call(handle, code, p, |_| Ok(()))
    }

    pub fn oneway(&self, handle: u32, code: u32, p: &Parcel) -> Result<(), i32> {
        let _one = self.call_lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.binder.oneway(handle, code, p)
    }
    pub fn acquire(&self, handle: u32) -> Result<(), i32> {
        self.binder.acquire(handle)
    }
    pub fn release(&self, handle: u32) {
        self.binder.release(handle)
    }
    /// The death function gets `cookie` when the object's process dies.
    pub fn watch(&self, handle: u32, cookie: u64) -> Result<(), i32> {
        self.binder.watch(handle, cookie)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.looper.take(); // the thread ends first; then the driver drops our objects: their holders see them die
    }
}

/// ONEIOS_BINDER (the PC test's binderfs), else the phone's /dev/binder.
pub fn default_device() -> String {
    Binder::path_from_env("ONEIOS_BINDER", "/dev/binder")
}

/// A handle from a reply (a lookup's IBinder): ours once acquired, before the reply's buffer goes back.
/// -ENOENT: a null binder (not registered); -EPROTO: a malformed reply.
pub fn take_handle(c: &Connection, r: &mut Reader<'_>) -> Result<u32, i32> {
    match r.binder() {
        Object::Handle(h) => c.acquire(h).map(|()| h),
        _ if r.is_bad() => Err(-libc::EPROTO),
        _ => Err(-libc::ENOENT),
    }
}

/// An int from a reply.
pub fn take_i32(r: &mut Reader<'_>) -> Result<i32, i32> {
    let v = r.i32();
    if r.is_bad() {
        Err(-libc::EPROTO)
    } else {
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // tests/test_wifi.c test_parcels: the bytes of each piece, as libbinder writes them.
    #[test]
    fn parcels_byte_for_byte() {
        let mut p = Parcel::with_token("a.B");
        let want: &[u8] = &[
            0, 0, 0, 0x80, 0xff, 0xff, 0xff, 0xff, 0x54, 0x53, 0x59, 0x53, 3, 0, 0, 0, b'a', 0, b'.', 0, b'B', 0, 0, 0,
        ];
        assert_eq!(p.bytes(), want);
        p.clear();
        p.put_string(None);
        p.put_str("");
        assert_eq!(p.bytes(), &[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0]);
        p.clear();
        p.put_bytes(&[1, 2, 3, 4, 5]);
        assert_eq!(p.bytes(), &[5, 0, 0, 0, 1, 2, 3, 4, 5, 0, 0, 0]);
        p.clear();
        p.put_local(0x4f57_434b);
        assert_eq!(p.size(), 28);
        assert_eq!(p.object_offsets(), &[0]);
        let o = FlatObject::parse(&p.bytes()[..24]);
        assert_eq!(
            o,
            FlatObject { kind: TYPE_BINDER, flags: LOCAL_FLAGS, binder_or_handle: 0x4f57_434b, cookie: 0x4f57_434b }
        );
        assert_eq!(&p.bytes()[24..28], &[0x3f, 0, 0, 0]);
        p.clear();
        p.put_null_binder();
        assert_eq!(p.size(), 28);
        assert!(p.object_offsets().is_empty());
        assert_eq!(p.reader().binder(), Object::Null);
        p.clear();
        p.put_service_error(4);
        let mut r = p.reader();
        assert_eq!(r.status(), Err((EX_SERVICE_SPECIFIC, 4)));
    }

    #[test]
    fn reader_strings_and_objects() {
        let mut p = Parcel::new();
        p.put_str("caf\u{7f}e");
        p.put_handle(3);
        p.put_local(9);
        let mut r = p.reader();
        assert_eq!(r.string(95).as_deref(), Some("caf?e"));
        assert_eq!(r.binder(), Object::Handle(3));
        assert_eq!(r.binder(), Object::Local(9));
        assert!(!r.is_bad());
        // An object not in the offsets array is refused.
        let mut q = Parcel::new();
        q.put_handle(3);
        let mut r = Reader::new(q.bytes(), &[]);
        assert_eq!(r.binder(), Object::Null);
        assert!(r.is_bad());
        // A string cut to its buffer, as the C did.
        let mut p = Parcel::new();
        p.put_str("abcdef");
        assert_eq!(p.reader().string(3).as_deref(), Some("abc"));
        // A string longer than the parcel.
        let mut p = Parcel::new();
        p.put_i32(100);
        let mut r = p.reader();
        assert!(r.string(10).is_none() && r.is_bad());
    }

    #[test]
    fn status_headers() {
        let mut p = Parcel::new();
        p.put_i32(EX_HAS_REPLY_HEADER);
        p.put_i32(8);
        p.put_i32(0x55);
        p.put_i32(7);
        let mut r = p.reader();
        assert_eq!(r.status(), Ok(()));
        assert_eq!(r.i32(), 7);
        let mut p = Parcel::new();
        p.put_i32(-3); // EX_ILLEGAL_ARGUMENT
        p.put_str("no");
        p.put_i32(0);
        let mut r = p.reader();
        assert_eq!(r.status(), Err((-3, -3)));
        let mut p = Parcel::new();
        p.put_i32(EX_HAS_NOTED_APPOPS);
        p.put_i32(2);
        assert_eq!(p.reader().status(), Err((BAD_TYPE, 0)));
    }

    #[test]
    fn overflow_and_wipe() {
        let mut p = Parcel::new();
        for _ in 0..9 {
            p.put_local(1);
        }
        assert!(p.is_overflow());
        let mut p = Parcel::new();
        p.put_bytes(&[0u8; PARCEL_DATA]);
        assert!(p.is_overflow());
        let mut p = Parcel::new();
        p.put_bytes(b"secret");
        p.wipe();
        assert_eq!(p.size(), 0);
        assert!(!p.is_overflow());
    }

    #[test]
    fn token_headers_and_large_parcels() {
        let mut p = Parcel::new();
        p.put_token_header("a.B", HEADER_VNDR);
        assert_eq!(&p.bytes()[8..12], b"RDNV"); // B_PACK_CHARS('V','N','D','R') as a little-endian word
        assert_eq!(p.reader().token_header("a.B"), Some(HEADER_VNDR));
        assert_eq!(Parcel::with_token("a.B").reader().token_header("a.B"), Some(HEADER_SYST));
        assert_eq!(p.reader().token_header("a.C"), None);
        assert!(p.reader().token("a.B") && !p.reader().token("a.C"));
        // A Bluetooth packet past PARCEL_DATA: refused by a plain parcel, carried by one with a larger limit, which
        // clear() keeps.
        let big = vec![0x5au8; 9000];
        let mut p = Parcel::new();
        p.put_bytes(&big);
        assert!(p.is_overflow());
        let mut p = Parcel::new();
        p.set_limit(0x1_0000 + 256);
        p.put_token("a.B");
        p.put_bytes(&big);
        assert!(!p.is_overflow());
        let mut r = p.reader();
        assert!(r.token("a.B") && r.bytes() == Some(&big[..]));
        p.clear();
        p.put_bytes(&[0u8; 0x1_0000]);
        assert!(!p.is_overflow());
        p.put_bytes(&[0u8; 256]);
        assert!(p.is_overflow());
    }

    #[test]
    fn standard_answers() {
        assert_eq!(standard(INTERFACE, "x.Y", 2, "h").unwrap().reader().string(95).as_deref(), Some("x.Y"));
        assert_eq!(standard(PING, "x.Y", 2, "h").unwrap().size(), 0);
        let v = standard(GET_VERSION, "x.Y", 2, "h").unwrap();
        let mut r = v.reader();
        assert!(r.status().is_ok() && r.i32() == 2);
        assert!(standard(5, "x.Y", 2, "h").is_none());
    }

    #[test]
    fn parcelables_and_numbers() {
        let mut p = Parcel::new();
        p.put_parcelable(|p| {
            p.put_i32(1);
            p.put_i64(-2);
            p.put_f32(0.5);
        });
        p.put_i32(0x5a5a);
        let mut r = p.reader();
        let end = r.parcelable().unwrap();
        assert_eq!(end, 4 + 4 + 4 + 8 + 4);
        assert_eq!((r.i32(), r.i64(), r.f32()), (1, -2, 0.5));
        assert_eq!(r.position(), end);
        assert_eq!(r.i32(), 0x5a5a);
    }
}
