// SPDX-License-Identifier: MIT
//! Bluetooth back end: the phone's stable-AIDL Bluetooth HAL
//! (`android.hardware.bluetooth.IBluetoothHci/default`) over /dev/binder.
//!
//! Android 16 vendors move the vendor Bluetooth HAL from HIDL (`android.hardware.bluetooth@1.x`, hwservicemanager)
//! to stable AIDL (servicemanager); Samsung's SM8750 images do: the Galaxy S25's vendor image declares
//! `android.hardware.bluetooth.IBluetoothHci/default` ([`AIDL_HCI_INSTANCE`]) and starts it as
//! `vendor.bluetooth-aidl-qti`. [`super::hci_hidl`] is the HIDL client for the S24 Ultra; this is the same client on
//! [`crate::hal::aidl`], chosen by [`probe`] when no HIDL service answers.
//!
//! The interface is the frozen version 1 of `IBluetoothHci.aidl` and `IBluetoothHciCallbacks.aidl`:
//! `initialize(callbacks)`, `sendHciCommand`, `sendAclData`, `sendScoData`, `sendIsoData`, `close`; the HAL calls the
//! callbacks with events, ACL, SCO and ISO data and `initializationComplete(status)`. Two threads, as in the HIDL
//! client: the binder looper only answers the HAL's calls into us, and one caller thread makes our calls in order
//! from a queue of up to 512 packets, so a vCPU never waits on the HAL. `initialize` is tried with the 'SYST' token
//! header first and 'VNDR' second (a vendor partition's libbinder writes 'VNDR'; the HAL accepts either, but a
//! vendor build may check it). `ONEIOS_BINDER` names another binder device (the PC test).
//!
//! Needs root (KernelSU's domain): servicemanager checks the caller's SELinux context, and the HAL must be allowed
//! to call back. C: bt/hci_hidl.c (the C build's one AIDL/HIDL client), bt/hidl_parcel.c's AIDL part.

use super::hci::{Hci, HciBackend, HciState, RxFn, StateFn};
use crate::hal::aidl::{self, Connection, Parcel, Reader};
use crate::hal::binder::{self, Commands};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// initialize() to initializationComplete: power-up and the firmware download.
const INIT_TIMEOUT_MS: u64 = 15000;
const QUEUE_PACKETS: usize = 512;
const DEATH_COOKIE: u64 = 0x4f6e_6944;

pub const AIDL_HCI: &str = "android.hardware.bluetooth.IBluetoothHci";
pub const AIDL_HCI_INSTANCE: &str = "android.hardware.bluetooth.IBluetoothHci/default";
pub const AIDL_CALLBACKS: &str = "android.hardware.bluetooth.IBluetoothHciCallbacks";
/// The frozen interfaces' version (the .aidl files have not changed since Android 13).
pub const AIDL_VERSION: i32 = 1;
/// The `getInterfaceHash` answer for the callback object. Nothing the HAL asks (its `fromBinder` reads the
/// descriptor and calls the methods); the frozen hash only matters to a client that verifies it.
const CALLBACK_HASH: &str = "";

/// The token's partition header: 'SYST' (a system libbinder) and 'VNDR' (a vendor one).
const HEADER_SYST: u32 = aidl::HEADER_SYST;
const HEADER_VNDR: u32 = 0x564e_4452;

// IBluetoothHci, declaration order in the frozen version 1 (Android 13+; methods only append).
pub const BT_CLOSE: u32 = 1;
pub const BT_INITIALIZE: u32 = 2;
pub const BT_SEND_ACL: u32 = 3;
pub const BT_SEND_COMMAND: u32 = 4;
pub const BT_SEND_ISO: u32 = 5;
pub const BT_SEND_SCO: u32 = 6;
// IBluetoothHciCallbacks, likewise.
pub const CB_ACL: u32 = 1;
pub const CB_EVENT: u32 = 2;
pub const CB_INIT_COMPLETE: u32 = 3;
pub const CB_ISO: u32 = 4;
pub const CB_SCO: u32 = 5;

/// Work for the caller thread, in order.
enum Work {
    Open,
    Close,
    Send(u8, Vec<u8>),
    Quit,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Work>,
    service: Option<u32>,
    header: u32,        // the token header the HAL accepted at initialize
    waiting_init: bool, // initialize() returned, initializationComplete not yet
}

/// What the client and the HAL's calls into us share: the queue, the service, the controller, the counters.
struct Shared {
    inner: Mutex<Inner>,
    more: Condvar,
    hci: Mutex<Weak<Hci>>, // empty for a probe
    dropped: AtomicU64,
    events: AtomicU64,
}

impl Default for Shared {
    fn default() -> Shared {
        Shared {
            inner: Mutex::new(Inner::default()),
            more: Condvar::new(),
            hci: Mutex::new(Weak::new()),
            dropped: AtomicU64::new(0),
            events: AtomicU64::new(0),
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn hci(&self) -> Option<Arc<Hci>> {
        self.hci.lock().unwrap_or_else(PoisonError::into_inner).upgrade()
    }

    fn set_state(&self, state: HciState) {
        if let Some(h) = self.hci() {
            h.set_state(state);
        }
    }
}

/// A call's parcel with a chosen token header (libbinder's writeInterfaceToken: strict-mode policy, no work source,
/// the partition header, a String16).
fn token_parcel(descriptor: &str, header: u32) -> Parcel {
    let mut p = Parcel::new();
    p.put_u32(0x8000_0000);
    p.put_i32(-1);
    p.put_u32(header);
    p.put_str(descriptor);
    p
}

fn init_timeout_ms() -> u64 {
    // the PC test's short wait
    match std::env::var("ONEIOS_BT_INIT_TIMEOUT_MS").ok().and_then(|t| t.trim().parse::<i64>().ok()) {
        Some(v) if v > 0 && v < 600_000 => v as u64,
        _ => INIT_TIMEOUT_MS,
    }
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

/// One callback from the HAL, after its token is checked. The packet borrows the transaction's buffer.
#[derive(Debug, PartialEq, Eq)]
enum Callback<'a> {
    Packet(u8, &'a [u8]), // its H4 type and bytes
    InitComplete(i32),    // Status: 0 SUCCESS
}

/// What a callback transaction carries: the callback object's methods (IBluetoothHciCallbacks v1).
fn callback_transact<'a>(code: u32, r: &mut Reader<'a>) -> Result<Callback<'a>, i32> {
    if !r.token(AIDL_CALLBACKS) {
        return Err(aidl::BAD_TYPE);
    }
    match code {
        CB_ACL | CB_EVENT | CB_ISO | CB_SCO => {
            let packet = r.bytes().ok_or(aidl::UNKNOWN_TRANSACTION)?;
            let h4 = match code {
                CB_ACL => super::hci::H4_ACL,
                CB_EVENT => super::hci::H4_EVENT,
                CB_ISO => super::hci::H4_ISO,
                _ => super::hci::H4_SCO,
            };
            Ok(Callback::Packet(h4, packet))
        }
        CB_INIT_COMPLETE => {
            let status = r.i32();
            if r.is_bad() {
                return Err(aidl::UNKNOWN_TRANSACTION);
            }
            Ok(Callback::InitComplete(status))
        }
        _ => Err(aidl::UNKNOWN_TRANSACTION),
    }
}

/// A call into our callback object: the standard methods ([`aidl::standard`]), then the HAL's callbacks.
fn callback(shared: &Arc<Shared>, code: u32, one_way: bool, r: &mut Reader<'_>) -> Result<Parcel, i32> {
    let _ = one_way;
    if let Some(reply) = aidl::standard(code, AIDL_CALLBACKS, AIDL_VERSION, CALLBACK_HASH) {
        return Ok(reply);
    }
    match callback_transact(code, r)? {
        Callback::Packet(kind, packet) => {
            if shared.events.fetch_add(1, Ordering::Relaxed) == 0 {
                crate::br_info!("bluetooth: first packet from the controller (H4 type {kind}, {} bytes)", packet.len());
            }
            if let Some(h) = shared.hci() {
                h.deliver(kind, packet); // before the transaction's buffer goes back
            }
        }
        Callback::InitComplete(status) => {
            let expected = std::mem::replace(&mut shared.lock().waiting_init, false);
            shared.more.notify_all();
            crate::br_log!(
                if status != 0 { crate::log::Level::Error } else { crate::log::Level::Info },
                "bluetooth: HAL initialisation {} (status {status}){}",
                if status != 0 { "failed" } else { "complete: the controller is on" },
                if expected { "" } else { " (not waited for)" }
            );
            shared.set_state(if status == 0 { HciState::On } else { HciState::Failed });
        }
    }
    let mut reply = Parcel::new();
    reply.put_ok();
    Ok(reply)
}

/// The client: the binder connection, the service, the work queue. The HAL's calls into us come to it as the
/// connection's incoming function.
struct AidlBt {
    conn: Connection,
    shared: Arc<Shared>,
}

impl AidlBt {
    fn set_state(&self, state: HciState) {
        self.shared.set_state(state);
    }

    /// The binder device, with its failure logged.
    fn open_connection(shared: &Arc<Shared>, with_callback: bool) -> Result<Connection, binder::OpenError> {
        let incoming = {
            let shared = Arc::clone(shared);
            Box::new(move |_object, code, one_way, r: &mut Reader<'_>| callback(&shared, code, one_way, r))
                as Box<aidl::IncomingFn>
        };
        let death = {
            let shared = Arc::clone(shared);
            Box::new(move |_cookie| {
                {
                    let mut i = shared.lock();
                    i.service = None;
                    i.waiting_init = false;
                }
                shared.more.notify_all();
                crate::br_error!(
                    "bluetooth: the HAL process died (init restarts it; bluetoothd's next start opens it again)"
                );
                shared.set_state(HciState::Failed); // not during the probe: no controller then
            }) as Box<aidl::DeathFn>
        };
        if with_callback {
            Connection::open(None, Some(incoming), Some(death))
        } else {
            Connection::open(None, None, None)
        }
    }

    /// Asks servicemanager for the instance and takes the handle. A later death turns the state to failed.
    fn find_service(&self, p: &mut Parcel) -> Result<(), i32> {
        p.clear();
        p.put_token(crate::hal::SERVICE_MANAGER);
        p.put_str(AIDL_HCI_INSTANCE);
        let handle =
            self.conn.call(0, crate::hal::SM_GET_SERVICE, p, |r| aidl::take_handle(&self.conn, r)).map_err(|e| e.rc)?;
        if let Err(rc) = self.conn.watch(handle, DEATH_COOKIE) {
            self.conn.release(handle);
            return Err(rc);
        }
        self.shared.lock().service = Some(handle);
        Ok(())
    }

    fn drop_service(&self) {
        let Some(handle) = self.shared.lock().service.take() else { return };
        // Unwatch then release; both go in one write.
        let _ = self.conn.binder().write_cmds(&Commands::new().unwatch(handle, DEATH_COOKIE).release(handle));
    }

    fn connect_hal(&self, p: &mut Parcel) {
        if let Err(rc) = self.find_service(p) {
            crate::br_error!(
                "bluetooth: off: no {AIDL_HCI_INSTANCE} from servicemanager: {}{}",
                why(rc),
                not_found_hint(rc)
            );
            self.set_state(HciState::Failed);
            return;
        }
        let service = {
            let mut i = self.shared.lock();
            i.waiting_init = true;
            i.service.unwrap_or(0)
        };
        // 'SYST' first; 'VNDR' when the HAL refuses it. A timeout or a dead HAL is not retried.
        let mut rc: Result<(), i32> = Ok(());
        for header in [HEADER_SYST, HEADER_VNDR] {
            self.shared.lock().header = header;
            let mut parcel = token_parcel(AIDL_HCI, header);
            parcel.put_local(Arc::as_ptr(&self.shared) as u64); // our callback object
            rc = self.conn.call_void(service, BT_INITIALIZE, &parcel).map_err(|e| e.rc);
            let stop = rc.is_ok() || rc == Err(-libc::ETIMEDOUT) || rc == Err(-libc::EPIPE);
            if stop {
                break;
            }
            crate::br_info!(
                "bluetooth: AIDL initialize with header {}: {}{}",
                if header == HEADER_SYST { "SYST" } else { "VNDR" },
                why(rc.err().unwrap_or(0)),
                if header == HEADER_SYST { "; trying VNDR" } else { "" }
            );
        }
        if let Err(rc) = rc {
            self.shared.lock().waiting_init = false;
            crate::br_error!("bluetooth: off: IBluetoothHci::initialize (AIDL): {}", why(rc));
            self.drop_service();
            self.set_state(HciState::Failed);
            return;
        }
        let header = self.shared.lock().header;
        crate::br_info!(
            "bluetooth: HAL {} found, initialising (power and firmware are the HAL's)",
            if header == HEADER_VNDR { "AIDL (VNDR)" } else { "AIDL (SYST)" }
        );
    }

    fn disconnect_hal(&self, p: &mut Parcel) {
        let (service, header) = {
            let mut i = self.shared.lock();
            i.waiting_init = false;
            (i.service, i.header)
        };
        let Some(handle) = service else { return };
        *p = token_parcel(AIDL_HCI, header);
        let rc = self.conn.call_void(handle, BT_CLOSE, p).map_err(|e| e.rc);
        self.drop_service();
        let (level, text) = match rc {
            Ok(()) => (crate::log::Level::Info, String::new()),
            Err(rc) => (crate::log::Level::Error, format!(": {}", why(rc))),
        };
        crate::br_log!(
            level,
            "bluetooth: HAL closed{text} ({} packets from the controller, {} dropped: queue full)",
            self.shared.events.load(Ordering::Relaxed),
            self.shared.dropped.load(Ordering::Relaxed)
        );
    }

    fn send_packet(&self, p: &mut Parcel, kind: u8, packet: &[u8]) {
        let code = match kind {
            super::hci::H4_COMMAND => BT_SEND_COMMAND,
            super::hci::H4_ACL => BT_SEND_ACL,
            super::hci::H4_SCO => BT_SEND_SCO,
            super::hci::H4_ISO => BT_SEND_ISO,
            _ => return,
        };
        let (service, header) = {
            let i = self.shared.lock();
            (i.service, i.header)
        };
        let Some(handle) = service else { return };
        *p = token_parcel(AIDL_HCI, header);
        p.put_bytes(packet);
        if let Err(e) = self.conn.call_void(handle, code, p) {
            crate::br_error!("bluetooth: sending an H4 type {kind} packet to the HAL: {}", why(e.rc));
        }
    }

    /// Our calls into the HAL, one at a time and in the guest's order. The guest's threads only queue (they hold
    /// the bridge device's lock); a HAL that is slow to answer delays Bluetooth, not iOS.
    fn caller(&self) {
        let mut p = Parcel::new();
        let mut init_deadline: Option<Instant> = None;
        loop {
            let work = {
                let mut i = self.shared.lock();
                loop {
                    if let Some(w) = i.queue.pop_front() {
                        break w;
                    }
                    if !i.waiting_init {
                        init_deadline = None;
                        i = self.shared.more.wait(i).unwrap_or_else(PoisonError::into_inner);
                        continue;
                    }
                    let deadline =
                        *init_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(init_timeout_ms()));
                    let now = Instant::now();
                    if now < deadline {
                        i = self.shared.more.wait_timeout(i, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
                        if Instant::now() < deadline || !i.waiting_init || !i.queue.is_empty() {
                            continue;
                        }
                    }
                    i.waiting_init = false;
                    drop(i);
                    crate::br_error!(
                        "bluetooth: off: no initializationComplete from the HAL in {} ms (is the HAL allowed to call into this domain? \
                         look for avc denials with scontext=...hal_bluetooth in dmesg)",
                        init_timeout_ms()
                    );
                    self.set_state(HciState::Failed);
                    i = self.shared.lock();
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
        let mut i = self.shared.lock();
        if i.queue.len() == QUEUE_PACKETS {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(-libc::EAGAIN);
        }
        i.queue.push_back(work);
        self.shared.more.notify_one();
        Ok(())
    }
}

/// The back end the controller holds: the client and its caller thread. Dropped, it closes the HAL and joins it.
struct AidlBackend {
    bt: Arc<AidlBt>,
    caller: Option<JoinHandle<()>>,
}

impl HciBackend for AidlBackend {
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

impl Drop for AidlBackend {
    fn drop(&mut self) {
        if let Some(caller) = self.caller.take() {
            self.bt.shared.lock().queue.push_back(Work::Quit); // past the queue's bound: the stop always goes in
            self.bt.shared.more.notify_one();
            if caller.thread().id() != std::thread::current().id() {
                let _ = caller.join();
            }
        }
        // The connection drops with the back end: its looper ends, then the driver drops our callback object.
    }
}

/// The phone's AIDL HAL as a controller (the bridge device's hci_factory). None: no binder or no thread (logged).
pub fn open_aidl(rx: RxFn, state: StateFn) -> Option<Arc<Hci>> {
    let shared = Arc::new(Shared::default());
    let conn = match AidlBt::open_connection(&shared, true) {
        Ok(conn) => conn,
        Err(open_error) => {
            let path = aidl::default_device();
            crate::br_error!("bluetooth: off: {path}: {}", binder::strerror(open_error.errno()));
            return None;
        }
    };
    let mut failed = false;
    let hci = Hci::new(
        |hci| {
            *shared.hci.lock().unwrap_or_else(PoisonError::into_inner) = hci;
            let bt = Arc::new(AidlBt { conn, shared });
            let caller = {
                let bt = Arc::clone(&bt);
                std::thread::Builder::new().spawn(move || bt.caller()).ok()
            };
            failed = caller.is_none();
            AidlBackend { bt, caller }
        },
        Some(rx),
        Some(state),
    );
    if failed {
        crate::br_error!("bluetooth: off: no binder thread");
        return None;
    }
    Some(hci)
}

/// Asks servicemanager for the AIDL HAL once and lets it go: 20 when it is registered and this domain may find it
/// (after checking its descriptor), else -errno (-ENODEV: no binder device). Logs why when it is not found.
pub fn probe() -> i32 {
    let shared = Arc::new(Shared::default());
    let conn = match AidlBt::open_connection(&shared, false) {
        Ok(conn) => conn,
        Err(e) => {
            crate::br_error!(
                "bluetooth: no AIDL HAL: no binder device ({}: {})",
                aidl::default_device(),
                binder::strerror(e.errno())
            );
            return -libc::ENODEV;
        }
    };
    let mut p = Parcel::with_token(crate::hal::SERVICE_MANAGER);
    p.put_str(AIDL_HCI_INSTANCE);
    let found = conn.call(0, crate::hal::SM_GET_SERVICE, &p, |r| aidl::take_handle(&conn, r));
    let handle = match found {
        Ok(handle) => handle,
        Err(e) => {
            crate::br_error!(
                "bluetooth: no {AIDL_HCI_INSTANCE} from servicemanager: {}{}",
                why(e.rc),
                not_found_hint(e.rc)
            );
            return e.rc;
        }
    };
    let named = conn.call(handle, aidl::INTERFACE, &Parcel::new(), |r| r.string(95).ok_or(-libc::EPROTO));
    let version = conn.call(handle, aidl::GET_VERSION, &Parcel::with_token(AIDL_HCI), aidl::take_i32).unwrap_or(0);
    conn.release(handle);
    match named {
        Ok(name) if name == AIDL_HCI => {
            crate::br_info!("bluetooth: the HAL is the AIDL one ({AIDL_HCI_INSTANCE}, version {version})");
            20
        }
        Ok(_) => {
            crate::br_error!("bluetooth: {AIDL_HCI_INSTANCE} is not an {AIDL_HCI} service");
            -libc::ENODEV
        }
        Err(e) => {
            crate::br_error!("bluetooth: {AIDL_HCI_INSTANCE}: {}", why(e.rc));
            e.rc
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::aidl::Object;

    #[test]
    fn parcels_and_callbacks() {
        // A send parcel: the token, then the byte[] (length, bytes padded to 4).
        let reset = [0x03u8, 0x0c, 0x00];
        let mut p = token_parcel(AIDL_HCI, HEADER_SYST);
        p.put_bytes(&reset);
        let mut r = p.reader();
        assert!(r.token(AIDL_HCI));
        assert_eq!(r.bytes(), Some(&reset[..]));
        assert!(!r.is_bad());

        // initialize: a strong binder with its stability level; 'VNDR' in the token is not compared.
        let mut p = token_parcel(AIDL_HCI, HEADER_VNDR);
        p.put_local(0x1234);
        let mut r = p.reader();
        assert!(r.token(AIDL_HCI));
        assert_eq!(r.binder(), Object::Local(0x1234));

        // The callback object's standard answers: the descriptor, and version 1 with the correct stable-AIDL codes.
        let reply = aidl::standard(aidl::INTERFACE, AIDL_CALLBACKS, AIDL_VERSION, CALLBACK_HASH).unwrap();
        assert_eq!(reply.reader().string(95).as_deref(), Some(AIDL_CALLBACKS));
        let reply = aidl::standard(aidl::GET_VERSION, AIDL_CALLBACKS, AIDL_VERSION, CALLBACK_HASH).unwrap();
        let mut r = reply.reader();
        assert_eq!(r.status(), Ok(()));
        assert_eq!(r.i32(), AIDL_VERSION);

        // initializationComplete(Status::SUCCESS), then an event and ACL data.
        let mut p = Parcel::with_token(AIDL_CALLBACKS);
        p.put_i32(0);
        assert_eq!(callback_transact(CB_INIT_COMPLETE, &mut p.reader()), Ok(Callback::InitComplete(0)));
        let event = [0x0eu8, 0x04, 0x01, 0x03, 0x0c, 0x00];
        let mut p = Parcel::with_token(AIDL_CALLBACKS);
        p.put_bytes(&event);
        assert_eq!(callback_transact(CB_EVENT, &mut p.reader()), Ok(Callback::Packet(4, &event[..])));
        let acl = [0x05u8, 0x20, 0x02, 0x00];
        let mut p = Parcel::with_token(AIDL_CALLBACKS);
        p.put_bytes(&acl);
        assert_eq!(callback_transact(CB_ACL, &mut p.reader()), Ok(Callback::Packet(2, &acl[..])));
        let mut p = Parcel::with_token(AIDL_CALLBACKS);
        p.put_bytes(&acl);
        assert_eq!(callback_transact(CB_SCO, &mut p.reader()), Ok(Callback::Packet(3, &acl[..])));
        let mut p = Parcel::with_token(AIDL_CALLBACKS);
        p.put_bytes(&acl);
        assert_eq!(callback_transact(CB_ISO, &mut p.reader()), Ok(Callback::Packet(5, &acl[..])));

        // Another interface's token, an unknown method and a truncated byte[] are refused.
        let mut p = Parcel::with_token(AIDL_HCI);
        p.put_bytes(&acl);
        assert_eq!(callback_transact(CB_EVENT, &mut p.reader()), Err(aidl::BAD_TYPE));
        let p = Parcel::with_token(AIDL_CALLBACKS);
        assert_eq!(callback_transact(0x7777, &mut p.reader()), Err(aidl::UNKNOWN_TRANSACTION));
        let mut p = Parcel::with_token(AIDL_CALLBACKS);
        p.put_bytes(&event);
        let size = p.size();
        let mut r = Reader::new(&p.bytes()[..size - 4], &[]);
        assert_eq!(callback_transact(CB_EVENT, &mut r), Err(aidl::UNKNOWN_TRANSACTION));
    }
}
