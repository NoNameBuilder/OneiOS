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

use super::hci::{Hci, HciBackend, HciState, RxFn, StateFn};
use super::hidl_parcel::*;
use crate::hal::binder::{self, Binder, Commands, Handler, Incoming, Looper, OpenError};
use crate::hal::hidl::{HidlParcel, HidlReader};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// initialize() to initializationComplete: power-up and the firmware download.
const INIT_TIMEOUT_MS: u64 = 15000;
const QUEUE_PACKETS: usize = 512;
const DEATH_COOKIE: u64 = 0x4f6e_6942;

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
}

impl Handler for HidlBt {
    /// A transaction for our callback object (the HAL's events, data, initializationComplete, IBase's methods).
    fn transaction(&self, tr: Incoming<'_>) {
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
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_state(&self, state: HciState) {
        if let Some(h) = self.hci.upgrade() {
            h.set_state(state);
        }
    }

    /// The hwbinder, with its failure logged.
    fn open_binder() -> Option<Arc<Binder>> {
        HidlBt::open_binder_logging(true)
    }

    fn open_binder_logging(log: bool) -> Option<Arc<Binder>> {
        let path = hwbinder_path();
        match Binder::open(Path::new(&path)) {
            Ok(b) => Some(Arc::new(b)),
            Err(OpenError::Open(e)) => {
                if log {
                    crate::br_error!(
                        "bluetooth: off: {path}: {}",
                        binder::strerror(e.raw_os_error().unwrap_or(libc::EIO))
                    );
                }
                None
            }
            Err(e @ OpenError::Setup { protocol, .. }) => {
                if log {
                    crate::br_error!(
                        "bluetooth: off: hwbinder setup (protocol {protocol}): {}",
                        binder::strerror(e.errno())
                    );
                }
                None
            }
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
                    self.connect_hal(&mut p);
                }
                Work::Close => self.disconnect_hal(&mut p),
                Work::Send(kind, packet) => self.send_packet(&mut p, kind, &packet),
                Work::Quit => {
                    self.disconnect_hal(&mut p);
                    break;
                }
            }
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

/// The phone's HAL as a controller (the bridge device's hci_factory). None: no hwbinder or no threads (logged).
pub fn open_hidl(rx: RxFn, state: StateFn) -> Option<Arc<Hci>> {
    let binder = HidlBt::open_binder()?;
    let mut failed = false;
    let hci = Hci::new(
        |hci| {
            let bt = Arc::new(HidlBt {
                binder,
                hci,
                shared: Mutex::new(Shared::default()),
                more: Condvar::new(),
                v1_1: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                events: AtomicU64::new(0),
            });
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

/// Asks hwservicemanager for the HAL once and lets it go: 11 (1.1) or 10 (1.0) when it is registered and this
/// domain may find it, else -errno (-ENODEV: no binder device). Logs why when it is not found.
pub fn probe() -> i32 {
    probe_logging(true)
}

/// [`probe`] without the "not found" lines: the caller then tries the AIDL HAL, so a phone without the HIDL one is
/// not an error to report.
pub fn probe_quiet() -> i32 {
    probe_logging(false)
}

fn probe_logging(log: bool) -> i32 {
    let Some(binder) = HidlBt::open_binder_logging(log) else { return -libc::ENODEV };
    let bt = HidlBt {
        binder,
        hci: Weak::new(),
        shared: Mutex::new(Shared::default()),
        more: Condvar::new(),
        v1_1: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
        events: AtomicU64::new(0),
    };
    let mut p = HidlParcel::new();
    let rc = match bt.find_service(&mut p) {
        Ok(()) if bt.v1_1.load(Ordering::Relaxed) => 11,
        Ok(()) => 10,
        Err(rc) => {
            if log {
                crate::br_error!(
                    "bluetooth: no android.hardware.bluetooth@1.x::IBluetoothHci/default from hwservicemanager: {}{}",
                    why(rc),
                    not_found_hint(rc)
                );
            }
            rc
        }
    };
    bt.drop_service();
    rc // the binder closes: the driver drops what is left
}
