// SPDX-License-Identifier: MIT
//! Bluetooth back end: the phone's Bluetooth HAL, spoken to directly over /dev/hwbinder.
//!
//! On the Galaxy S24 Ultra the controller is a Qualcomm WCN785x ("hamilton", DT node bt_kiwi) behind the HIDL service
//! android.hardware.bluetooth@1.1::IBluetoothHci/default (init service vendor.bluetooth-1-1-qti). The HAL owns power,
//! the UART and the firmware download; its only client is Android's Bluetooth app, which is gone in a session (it
//! is a zygote child), so the HAL is free. This is that client: IServiceManager::get, initialize(callbacks),
//! sendHciCommand/AclData/ScoData/IsoData, and a callback object the HAL calls with events and data. Two threads:
//! the looper answers the HAL's calls into us; the caller makes ours, in order, from a queue the guest fills.
//! ONEIOS_HWBINDER names another binder device (the PC test's binderfs).
//!
//! No Android library is linked (xnuhost is static): the binder protocol is [`crate::hal::binder`] and the parcels
//! [`super::hidl_parcel`]. Needs root (KernelSU's domain): hwservicemanager checks the caller's SELinux context for
//! "find", and the HAL must be allowed to call back. C: bt/hci_hidl.c.
//!
//! AIDL: a phone with the stable AIDL HAL instead (the Galaxy A56: android.hardware.bluetooth.IBluetoothHci/default,
//! process android.hardware.bluetooth-service.default, on servicemanager) has no HIDL service. Only when
//! hwservicemanager has none does [`probe`] ask servicemanager on /dev/binder (ONEIOS_BINDER names another device);
//! [`open_hidl`] then speaks the AIDL parcels ([`super::aidl_parcel`]) on the same two threads, the callback object
//! answering IBluetoothHciCallbacks. A phone with the HIDL HAL (the S24 Ultra) never reaches that code: its probe
//! returns before /dev/binder is opened. Ported from the A56 fork's bt/hci_hidl.c (Musicalista/OneiOS-A56 26a2276).

use super::aidl_parcel as ap;
use super::hci::{Hci, HciBackend, HciState, RxFn, StateFn};
use super::hidl_parcel::*;
use crate::hal::aidl::{self, Object};
use crate::hal::binder::{self, Binder, Commands, Handler, Incoming, Looper, OpenError};
use crate::hal::hidl::{HidlParcel, HidlReader};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// initialize() to initializationComplete: power-up and the firmware download.
const INIT_TIMEOUT_MS: u64 = 15000;
const QUEUE_PACKETS: usize = 512;
const DEATH_COOKIE: u64 = 0x4f6e_6942;
/// The probe's answer for the AIDL HAL (11 and 10: HIDL 1.1 and 1.0).
pub const PROBE_AIDL: i32 = 20;

/// The last probe found the AIDL HAL and no HIDL one: [`open_hidl`] speaks AIDL. Otherwise (no probe, as with
/// bluetooth=hidl, or a HIDL HAL found) the open is the HIDL one.
static FOUND_AIDL: AtomicBool = AtomicBool::new(false);

/// Work for the caller thread, in order.
enum Work {
    Open,
    Close,
    Send(u8, Vec<u8>),
    Quit,
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Work>,
    service: Option<u32>,
    waiting_init: bool, // initialize() returned, initializationComplete not yet
}

/// The client: the hwbinder, the HAL's handle, the work queue. The HAL's calls into us come to it as a Handler.
struct HidlBt {
    binder: Arc<Binder>,
    hci: Weak<Hci>, // empty for the probe
    shared: Mutex<Shared>,
    more: Condvar,
    v1_1: AtomicBool,
    dropped: AtomicU64,
    events: AtomicU64,
    aidl: bool,         // the AIDL HAL on /dev/binder, not HIDL on /dev/hwbinder
    header: AtomicU32,  // AIDL: the interface token's header the HAL took ('SYST', else 'VNDR')
    version: AtomicI32, // AIDL: the HAL's interface version (0: not told)
}

fn init_timeout_ms() -> u64 {
    // the PC test's short wait
    match std::env::var("ONEIOS_BT_INIT_TIMEOUT_MS").ok().and_then(|t| t.trim().parse::<i64>().ok()) {
        Some(v) if v > 0 && v < 600_000 => v as u64,
        _ => INIT_TIMEOUT_MS,
    }
}

fn hwbinder_path() -> String {
    Binder::path_from_env("ONEIOS_HWBINDER", "/dev/hwbinder") // the PC test's binderfs; the phone's /dev/hwbinder
}

fn why(rc: i32) -> String {
    if rc == -libc::ETIMEDOUT {
        "no answer in 5 s".to_owned()
    } else {
        binder::strerror(rc)
    }
}

fn not_found_hint(rc: i32) -> &'static str {
    if rc == -libc::ENOENT {
        " (not registered, or SELinux denies find to this domain)"
    } else {
        ""
    }
}

thread_local! {
    /// The parcel of this thread's calls and replies, kept between them (a packet is a call: no allocation each).
    static PARCEL: RefCell<HidlParcel<'static>> = RefCell::new(HidlParcel::new());
    /// The AIDL callbacks' reply parcel, kept as the HIDL one.
    static AIDL_REPLY: RefCell<aidl::Parcel> = RefCell::new(aidl::Parcel::new());
}

impl Handler for HidlBt {
    /// A transaction for our callback object (the HAL's events, data, initializationComplete, IBase's methods).
    fn transaction(&self, tr: Incoming<'_>) {
        if self.aidl {
            return self.aidl_transaction(tr);
        }
        let code = tr.code();
        PARCEL.with_borrow_mut(|reply| {
            let answer = {
                let (data, offsets) = tr.parts();
                // SAFETY: a transaction the driver delivered: its buffers are in our mapping until tr is finished.
                let mut r = unsafe { HidlReader::new(data, offsets) };
                let answer = callback_transact(code, &mut r, reply);
                if let Ok(Callback::Packet(kind, packet)) = answer {
                    if self.events.fetch_add(1, Ordering::Relaxed) == 0 {
                        crate::br_info!(
                            "bluetooth: first packet from the controller (H4 type {kind}, {} bytes)",
                            packet.len()
                        );
                    }
                    if let Some(h) = self.hci.upgrade() {
                        h.deliver(kind, packet); // before the buffer is freed
                    }
                }
                answer.map(|c| match c {
                    Callback::InitComplete(status) => Some(status),
                    _ => None,
                })
            };
            match answer {
                Err(rc) if code < 0x0f00_0000 => crate::br_error!(
                    "bluetooth: the HAL called back with code {code} and a parcel not understood: answered {rc}"
                ),
                Ok(Some(status)) => {
                    let expected = std::mem::replace(&mut self.lock().waiting_init, false);
                    crate::br_log!(
                        if status != 0 { crate::log::Level::Error } else { crate::log::Level::Info },
                        "bluetooth: HAL initialisation {} (status {status}){}",
                        if status != 0 { "failed" } else { "complete: the controller is on" },
                        if expected { "" } else { " (not waited for)" }
                    );
                    self.set_state(if status == 0 { HciState::On } else { HciState::Failed });
                }
                _ => {}
            }
            tr.finish(answer.map(|_| &*reply as &dyn binder::Outgoing));
        });
    }

    fn dead(&self, _cookie: u64) {
        {
            let mut s = self.lock();
            s.service = None;
            s.waiting_init = false;
        }
        crate::br_error!("bluetooth: the HAL process died (init restarts it; bluetoothd's next start opens it again)");
        self.set_state(HciState::Failed); // not during the probe: no controller then
    }
}

impl HidlBt {
    fn new(binder: Arc<Binder>, hci: Weak<Hci>, aidl: bool) -> HidlBt {
        HidlBt {
            binder,
            hci,
            shared: Mutex::new(Shared::default()),
            more: Condvar::new(),
            v1_1: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            events: AtomicU64::new(0),
            aidl,
            header: AtomicU32::new(aidl::HEADER_SYST),
            version: AtomicI32::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_state(&self, state: HciState) {
        if let Some(h) = self.hci.upgrade() {
            h.set_state(state);
        }
    }

    /// The hwbinder (or, `aidl`, the binder), or why not (the text after "bluetooth: off: ").
    fn open_binder(aidl: bool) -> Result<Arc<Binder>, String> {
        let path = if aidl { aidl::default_device() } else { hwbinder_path() };
        match Binder::open(Path::new(&path)) {
            Ok(b) => Ok(Arc::new(b)),
            Err(OpenError::Open(e)) => {
                Err(format!("{path}: {}", binder::strerror(e.raw_os_error().unwrap_or(libc::EIO))))
            }
            Err(e @ OpenError::Setup { protocol, .. }) => Err(format!(
                "{} setup (protocol {protocol}): {}",
                if aidl { path.as_str() } else { "hwbinder" },
                binder::strerror(e.errno())
            )),
        }
    }

    /// A synchronous call, made only by the caller thread (or the probe). `on_reply` sees the reply after its
    /// status before its buffer is released. A status-code reply gives that code.
    fn call(
        &self,
        handle: u32,
        code: u32,
        p: &HidlParcel<'_>,
        on_reply: impl FnOnce(&mut HidlReader<'_>) -> Result<(), i32>,
    ) -> Result<(), i32> {
        let reply = self.binder.transact(handle, code, p, self)?;
        if let Some(status) = reply.status_code() {
            return Err(if status != 0 { status } else { -libc::EIO });
        }
        let (data, offsets) = reply.parts();
        // SAFETY: the reply the driver delivered: its buffers are in our mapping until `reply` is dropped.
        let mut r = unsafe { HidlReader::new(data, offsets) };
        let exception = r.i32();
        if r.is_bad() || exception != 0 {
            return Err(-libc::EPROTO);
        }
        on_reply(&mut r)
    }

    /// Our own references (taken before the reply buffer, which holds the transaction's, is freed) and a death
    /// notification.
    fn take_service(&self, r: &mut HidlReader<'_>) -> Result<(), i32> {
        let handle = r.handle().ok_or(-libc::ENOENT)?;
        self.take_handle(handle)
    }

    fn take_handle(&self, handle: u32) -> Result<(), i32> {
        self.binder.write_cmds(&Commands::new().acquire(handle).watch(handle, DEATH_COOKIE))?;
        self.lock().service = Some(handle);
        Ok(())
    }

    fn drop_service(&self) {
        let Some(handle) = self.lock().service.take() else { return };
        let _ = self.binder.write_cmds(&Commands::new().unwatch(handle, DEATH_COOKIE).release(handle));
    }

    /// IServiceManager::get of the 1.1 name, then the 1.0 one: a 1.1 service is registered under both, a 1.0 one
    /// only under the second.
    fn find_service(&self, p: &mut HidlParcel<'_>) -> Result<(), i32> {
        self.v1_1.store(true, Ordering::Relaxed);
        get_service(p, BT_HCI_1_1, c"default");
        let first = self.call(0, 1 /* IServiceManager::get */, p, |r| self.take_service(r));
        if first.is_ok() {
            return first;
        }
        self.v1_1.store(false, Ordering::Relaxed);
        get_service(p, BT_HCI_1_0, c"default");
        self.call(0, 1, p, |r| self.take_service(r))
    }

    fn connect_hal(&self, p: &mut HidlParcel<'_>) {
        if let Err(rc) = self.find_service(p) {
            crate::br_error!(
                "bluetooth: off: no android.hardware.bluetooth@1.x::IBluetoothHci/default from hwservicemanager: {}{}",
                why(rc),
                not_found_hint(rc)
            );
            self.set_state(HciState::Failed);
            return;
        }
        let v1_1 = self.v1_1.load(Ordering::Relaxed);
        let service = {
            let mut s = self.lock();
            s.waiting_init = true;
            s.service.unwrap_or(0)
        };
        let ours = self as *const HidlBt as u64; // the callback object: its id is our address, as the C's
        initialize(p, v1_1, ours);
        if let Err(rc) = self.call(service, if v1_1 { BT_INITIALIZE_1_1 } else { BT_INITIALIZE }, p, |_| Ok(())) {
            self.lock().waiting_init = false;
            crate::br_error!(
                "bluetooth: off: IBluetoothHci::initialize{}: {}",
                if v1_1 { "_1_1" } else { "" },
                why(rc)
            );
            self.drop_service();
            self.set_state(HciState::Failed);
            return;
        }
        crate::br_info!(
            "bluetooth: HAL {} found, initialising (power and firmware are the HAL's)",
            if v1_1 { "1.1" } else { "1.0" }
        );
    }

    fn disconnect_hal(&self, p: &mut HidlParcel<'_>) {
        let service = {
            let mut s = self.lock();
            s.waiting_init = false;
            s.service
        };
        let Some(handle) = service else { return };
        p.clear();
        p.put_token(BT_HCI_1_0);
        let rc = self.call(handle, BT_CLOSE, p, |_| Ok(()));
        self.drop_service();
        let (level, text) = match rc {
            Ok(()) => (crate::log::Level::Info, String::new()),
            Err(rc) => (crate::log::Level::Error, format!(": {}", why(rc))),
        };
        crate::br_log!(
            level,
            "bluetooth: HAL closed{text} ({} packets from the controller, {} dropped: queue full)",
            self.events.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed)
        );
    }

    fn send_packet(&self, p: &mut HidlParcel<'_>, kind: u8, packet: &[u8]) {
        let code = match kind {
            1 => BT_SEND_COMMAND,
            2 => BT_SEND_ACL,
            3 => BT_SEND_SCO,
            5 if self.v1_1.load(Ordering::Relaxed) => BT_SEND_ISO,
            _ => return,
        };
        let Some(handle) = self.lock().service else { return };
        // A parcel of this call's own lifetime, on the kept parcel's memory.
        let mut q = std::mem::take(p).recycle();
        send(&mut q, code, packet);
        if let Err(rc) = self.call(handle, code, &q, |_| Ok(())) {
            crate::br_error!("bluetooth: sending an H4 type {kind} packet to the HAL: {}", why(rc));
        }
        *p = q.recycle();
    }

    /// Our calls into the HAL, one at a time and in the guest's order. The guest's threads only queue (they hold
    /// the bridge device's lock); a HAL that is slow to answer delays Bluetooth, not iOS.
    fn caller(&self) {
        let mut p = HidlParcel::new();
        let mut a = aidl::Parcel::new(); // the AIDL calls' parcel, kept between them as the HIDL one
        a.set_limit(ap::PARCEL_MAX);
        let mut init_deadline: Option<Instant> = None;
        loop {
            let work = {
                let mut s = self.lock();
                loop {
                    if let Some(w) = s.queue.pop_front() {
                        break w;
                    }
                    if !s.waiting_init {
                        init_deadline = None;
                        s = self.more.wait(s).unwrap_or_else(PoisonError::into_inner);
                        continue;
                    }
                    let deadline =
                        *init_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(init_timeout_ms()));
                    let now = Instant::now();
                    if now < deadline {
                        s = self.more.wait_timeout(s, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
                        if Instant::now() < deadline || !s.waiting_init || !s.queue.is_empty() {
                            continue;
                        }
                    }
                    s.waiting_init = false;
                    drop(s);
                    crate::br_error!(
                        "bluetooth: off: no initializationComplete from the HAL in {} ms (is the HAL allowed to call into this domain? \
                         look for avc denials with scontext=...hal_bluetooth in dmesg)",
                        init_timeout_ms()
                    );
                    self.set_state(HciState::Failed);
                    s = self.lock();
                }
            };
            match work {
                Work::Open => {
                    init_deadline = None;
                    if self.aidl {
                        self.connect_hal_aidl(&mut a);
                    } else {
                        self.connect_hal(&mut p);
                    }
                }
                Work::Close if self.aidl => self.disconnect_hal_aidl(&mut a),
                Work::Close => self.disconnect_hal(&mut p),
                Work::Send(kind, packet) if self.aidl => self.send_packet_aidl(&mut a, kind, &packet),
                Work::Send(kind, packet) => self.send_packet(&mut p, kind, &packet),
                Work::Quit => {
                    if self.aidl {
                        self.disconnect_hal_aidl(&mut a);
                    } else {
                        self.disconnect_hal(&mut p);
                    }
                    break;
                }
            }
        }
    }

    // ---- AIDL (android.hardware.bluetooth.IBluetoothHci on /dev/binder) ----

    /// The callback object's AIDL transactions: IBluetoothHciCallbacks and IBinder's own.
    fn aidl_transaction(&self, tr: Incoming<'_>) {
        let code = tr.code();
        AIDL_REPLY.with_borrow_mut(|reply| {
            let answer = {
                let (data, offsets) = tr.parts();
                let mut r = aidl::Reader::new(data, offsets);
                let answer = ap::callback_transact(code, &mut r, reply);
                if let Ok(Callback::Packet(kind, packet)) = answer {
                    if self.events.fetch_add(1, Ordering::Relaxed) == 0 {
                        crate::br_info!(
                            "bluetooth: first packet from the controller (H4 type {kind}, {} bytes)",
                            packet.len()
                        );
                    }
                    if let Some(h) = self.hci.upgrade() {
                        h.deliver(kind, packet); // before the buffer is freed
                    }
                }
                answer.map(|c| match c {
                    Callback::InitComplete(status) => Some(status),
                    _ => None,
                })
            };
            match answer {
                Err(rc) => crate::br_error!(
                    "bluetooth: the AIDL HAL called back with code {code} and a parcel not understood: answered {rc}"
                ),
                Ok(Some(status)) => {
                    let expected = std::mem::replace(&mut self.lock().waiting_init, false);
                    crate::br_log!(
                        if status != 0 { crate::log::Level::Error } else { crate::log::Level::Info },
                        "bluetooth: HAL initialisation {} (status {status}){}",
                        if status != 0 { "failed" } else { "complete: the controller is on" },
                        if expected { "" } else { " (not waited for)" }
                    );
                    self.set_state(if status == 0 { HciState::On } else { HciState::Failed });
                }
                Ok(None) => {}
            }
            tr.finish(answer.map(|_| &*reply as &dyn binder::Outgoing));
        });
    }

    /// An AIDL call (as hal::aidl::Connection::call, on this client's binder and handler): `on_reply` sees the
    /// reply after its status header (none for INTERFACE and PING). -ENOSYS: the HAL does not know the call;
    /// -EPROTO: a reply not understood, or libbinder's own status (BAD_TYPE: the token refused); -EREMOTEIO: an
    /// exception.
    fn call_aidl<T>(
        &self,
        handle: u32,
        code: u32,
        p: &aidl::Parcel,
        on_reply: impl FnOnce(&mut aidl::Reader<'_>) -> Result<T, i32>,
    ) -> Result<T, i32> {
        let reply = self.binder.transact(handle, code, p, self)?;
        if let Some(status) = reply.status_code() {
            return Err(match if status < 0 { status } else { -libc::EIO } {
                aidl::UNKNOWN_TRANSACTION => -libc::ENOSYS,
                rc if rc < -4095 => -libc::EPROTO,
                rc => rc,
            });
        }
        let (data, offsets) = reply.parts();
        let mut r = aidl::Reader::new(data, offsets);
        if code != aidl::INTERFACE && code != aidl::PING {
            match r.status() {
                _ if r.is_bad() => return Err(-libc::EPROTO),
                Err(_) => return Err(-libc::EREMOTEIO),
                Ok(()) => {}
            }
        }
        on_reply(&mut r)
    }

    /// IServiceManager.getService of the HAL, its descriptor checked, its version asked (0 when it does not say).
    fn find_service_aidl(&self, p: &mut aidl::Parcel) -> Result<(), i32> {
        ap::get_service(p);
        self.call_aidl(0, crate::hal::SM_GET_SERVICE, p, |r| match r.binder() {
            Object::Handle(h) => self.take_handle(h),
            _ if r.is_bad() => Err(-libc::EPROTO),
            _ => Err(-libc::ENOENT),
        })?;
        let handle = self.lock().service.unwrap_or(0);
        p.clear();
        match self.call_aidl(handle, aidl::INTERFACE, p, |r| r.string(95).ok_or(-libc::EPROTO)) {
            Ok(name) if name == ap::HCI => {}
            Ok(name) => {
                crate::br_error!("bluetooth: {} is a {name}, not an {}", ap::HCI_INSTANCE, ap::HCI);
                self.drop_service();
                return Err(-libc::EPROTO);
            }
            Err(rc) => {
                self.drop_service();
                return Err(rc);
            }
        }
        p.clear();
        p.put_token(ap::HCI);
        let version = self.call_aidl(handle, aidl::GET_VERSION, p, aidl::take_i32).unwrap_or(0);
        self.version.store(version, Ordering::Relaxed);
        Ok(())
    }

    fn connect_hal_aidl(&self, p: &mut aidl::Parcel) {
        if let Err(rc) = self.find_service_aidl(p) {
            crate::br_error!(
                "bluetooth: off: no {} from servicemanager: {}",
                ap::HCI_INSTANCE,
                crate::hal::error_text(rc)
            );
            self.set_state(HciState::Failed);
            return;
        }
        let service = {
            let mut s = self.lock();
            s.waiting_init = true;
            s.service.unwrap_or(0)
        };
        let ours = self as *const HidlBt as u64; // the callback object: its id is our address, as the HIDL one's
        let mut header = aidl::HEADER_SYST;
        ap::initialize(p, header, ours);
        let mut rc = self.call_aidl(service, ap::HCI_INITIALIZE, p, |_| Ok(()));
        // A HAL on the vendor copy of libbinder refuses a 'SYST' token: libbinder_ndk with BAD_TYPE (-EPROTO here),
        // the cpp backend with PERMISSION_DENIED (-EPERM). Not called then: the same call again with 'VNDR'.
        if let Some(e) = rc.err().filter(|&e| e == -libc::EPROTO || e == -libc::EPERM) {
            crate::br_info!(
                "bluetooth: the AIDL HAL refused initialize with the system token header ({}): trying 'VNDR'",
                binder::strerror(e)
            );
            header = aidl::HEADER_VNDR;
            ap::initialize(p, header, ours);
            rc = self.call_aidl(service, ap::HCI_INITIALIZE, p, |_| Ok(()));
        }
        self.header.store(header, Ordering::Relaxed);
        if let Err(rc) = rc {
            self.lock().waiting_init = false;
            crate::br_error!("bluetooth: off: IBluetoothHci::initialize (AIDL): {}", crate::hal::error_text(rc));
            self.drop_service();
            self.set_state(HciState::Failed);
            return;
        }
        crate::br_info!(
            "bluetooth: HAL AIDL (version {}, token header {}) found, initialising (power and firmware are the HAL's)",
            self.version.load(Ordering::Relaxed),
            if header == aidl::HEADER_VNDR { "VNDR" } else { "SYST" }
        );
    }

    fn disconnect_hal_aidl(&self, p: &mut aidl::Parcel) {
        let service = {
            let mut s = self.lock();
            s.waiting_init = false;
            s.service
        };
        let Some(handle) = service else { return };
        ap::close(p, self.header.load(Ordering::Relaxed));
        let rc = self.call_aidl(handle, ap::HCI_CLOSE, p, |_| Ok(()));
        self.drop_service();
        let (level, text) = match rc {
            Ok(()) => (crate::log::Level::Info, String::new()),
            Err(rc) => (crate::log::Level::Error, format!(": {}", crate::hal::error_text(rc))),
        };
        crate::br_log!(
            level,
            "bluetooth: HAL closed{text} ({} packets from the controller, {} dropped: queue full)",
            self.events.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed)
        );
    }

    fn send_packet_aidl(&self, p: &mut aidl::Parcel, kind: u8, packet: &[u8]) {
        let Some(code) = ap::send_code(kind) else { return };
        let Some(handle) = self.lock().service else { return };
        ap::send(p, self.header.load(Ordering::Relaxed), packet);
        if let Err(rc) = self.call_aidl(handle, code, p, |_| Ok(())) {
            crate::br_error!("bluetooth: sending an H4 type {kind} packet to the HAL: {}", crate::hal::error_text(rc));
        }
    }

    fn enqueue(&self, work: Work) -> Result<(), i32> {
        if let Work::Send(_, packet) = &work {
            if packet.len() > 0xffff {
                return Err(-libc::ENOMEM);
            }
        }
        let mut s = self.lock();
        if s.queue.len() == QUEUE_PACKETS {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(-libc::EAGAIN);
        }
        s.queue.push_back(work);
        self.more.notify_one();
        Ok(())
    }
}

/// The back end the controller holds: the client and its two threads. Dropped, it closes the HAL and joins them.
struct HidlBackend {
    bt: Arc<HidlBt>,
    caller: Option<JoinHandle<()>>,
    looper: Option<Looper>,
}

impl HciBackend for HidlBackend {
    fn open(&self) -> Result<(), i32> {
        self.bt.enqueue(Work::Open)
    }
    fn send(&self, kind: u8, packet: &[u8]) -> Result<(), i32> {
        self.bt.enqueue(Work::Send(kind, packet.to_vec()))
    }
    fn close(&self) {
        let _ = self.bt.enqueue(Work::Close);
    }
}

impl Drop for HidlBackend {
    fn drop(&mut self) {
        if let Some(caller) = self.caller.take() {
            self.bt.lock().queue.push_back(Work::Quit); // past the queue's bound: the stop always goes in
            self.bt.more.notify_one();
            if caller.thread().id() != std::thread::current().id() {
                let _ = caller.join();
            }
        }
        self.looper.take();
    }
}

/// The phone's HAL as a controller (the bridge device's hci_factory): the HIDL one, or the AIDL one when the last
/// [`probe`] found only that. None: no binder device or no threads (logged).
pub fn open_hidl(rx: RxFn, state: StateFn) -> Option<Arc<Hci>> {
    let aidl = FOUND_AIDL.load(Ordering::Relaxed);
    let binder = match HidlBt::open_binder(aidl) {
        Ok(b) => b,
        Err(why) => {
            crate::br_error!("bluetooth: off: {why}");
            return None;
        }
    };
    let mut failed = false;
    let hci = Hci::new(
        |hci| {
            let bt = Arc::new(HidlBt::new(binder, hci, aidl));
            let looper = Looper::start(Arc::clone(&bt.binder), bt.clone()).ok();
            let caller = {
                let bt = Arc::clone(&bt);
                std::thread::Builder::new().spawn(move || bt.caller()).ok()
            };
            failed = looper.is_none() || caller.is_none();
            HidlBackend { bt, caller, looper }
        },
        Some(rx),
        Some(state),
    );
    if failed {
        crate::br_error!("bluetooth: off: no binder threads");
        return None;
    }
    Some(hci)
}

/// hwservicemanager's answer: 11 (1.1) or 10 (1.0), else -errno and why (-ENODEV: no hwbinder).
fn probe_hidl() -> Result<i32, (i32, String)> {
    let binder = HidlBt::open_binder(false).map_err(|why| (-libc::ENODEV, why))?;
    let bt = HidlBt::new(binder, Weak::new(), false);
    let mut p = HidlParcel::new();
    let rc = match bt.find_service(&mut p) {
        Ok(()) if bt.v1_1.load(Ordering::Relaxed) => Ok(11),
        Ok(()) => Ok(10),
        Err(rc) => Err((rc, format!("{}{}", why(rc), not_found_hint(rc)))),
    };
    bt.drop_service();
    rc // the binder closes: the driver drops what is left
}

/// servicemanager's answer for the AIDL HAL: its interface version, else -errno and why (-ENODEV: no binder).
fn probe_aidl() -> Result<i32, (i32, String)> {
    let binder = HidlBt::open_binder(true).map_err(|why| (-libc::ENODEV, why))?;
    let bt = HidlBt::new(binder, Weak::new(), true);
    let mut p = aidl::Parcel::new();
    let rc = match bt.find_service_aidl(&mut p) {
        Ok(()) => Ok(bt.version.load(Ordering::Relaxed)),
        Err(rc) => Err((rc, crate::hal::error_text(rc))),
    };
    bt.drop_service();
    rc
}

/// Asks for the HAL once and lets it go: hwservicemanager first (11: HIDL 1.1, 10: 1.0; a phone with it, the S24
/// Ultra, stops there), then, only when it has none, servicemanager on /dev/binder for the stable AIDL HAL
/// ([`PROBE_AIDL`]: [`open_hidl`] then speaks AIDL). Else -errno (the HIDL one's, or the AIDL one's when there is no
/// hwbinder; -ENODEV: no binder device at all) and one line naming both.
pub fn probe() -> i32 {
    let (hidl_rc, hidl_why) = match probe_hidl() {
        Ok(v) => {
            FOUND_AIDL.store(false, Ordering::Relaxed);
            return v;
        }
        Err(e) => e,
    };
    match probe_aidl() {
        Ok(version) => {
            FOUND_AIDL.store(true, Ordering::Relaxed);
            crate::br_info!(
                "bluetooth: the AIDL HAL {} (version {version}) on servicemanager: no HIDL HAL on hwservicemanager ({hidl_why})",
                ap::HCI_INSTANCE
            );
            PROBE_AIDL
        }
        Err((aidl_rc, aidl_why)) => {
            FOUND_AIDL.store(false, Ordering::Relaxed);
            crate::br_error!(
                "bluetooth: no Bluetooth HAL: no android.hardware.bluetooth@1.x::IBluetoothHci/default from \
                 hwservicemanager ({hidl_why}), no {} from servicemanager ({aidl_why})",
                ap::HCI_INSTANCE
            );
            if hidl_rc == -libc::ENODEV {
                aidl_rc
            } else {
                hidl_rc
            }
        }
    }
}
