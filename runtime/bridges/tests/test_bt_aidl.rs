// SPDX-License-Identifier: MIT
//! The AIDL Bluetooth HAL client (`bt::hci_aidl`) against a real binder driver: a private binderfs on the build host
//! (the test runs itself in a user namespace with one: hal::binderfs). A child process plays servicemanager (the
//! context manager) and android.hardware.bluetooth.IBluetoothHci/default: it checks the client's callback object
//! (descriptor, version), answers initialize with initializationComplete(SUCCESS), an HCI event and ACL data, records
//! the client's command and its close.
//!
//! Scenarios: the probe finds the AIDL service (20); initialize's callback object and its token; the state reaches
//! on; frames both ways keep their H4 type; close. Both ends use hal::aidl, so what this proves is the kernel
//! protocol and this client's parcels; the real vendor HAL is `bridgectl bt` on the phone. C: the C build had no
//! AIDL test (the A56 fork's client was untested: docs/forks.md).

mod common;

use onebridges::bt::hci::{self, HciState};
use onebridges::bt::hci_aidl;
use onebridges::hal::aidl::{self, Connection, Object, Parcel, Reader};
use onebridges::hal::binderfs::{self, Entered};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

// ================================ the fake servicemanager and HAL (child) ================================
const OBJ_MANAGER: u64 = 0;
const OBJ_HCI: u64 = 1;

struct Fake {
    log: i32, // what the client did, a line each, for the parent
    closed: bool,
}

static FAKE: OnceLock<Connection> = OnceLock::new();
static STATE: Mutex<Option<Fake>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Fake) -> T) -> T {
    f(STATE.lock().unwrap().as_mut().expect("the fake's state"))
}

fn note(line: &str) {
    let fd = with(|s| s.log);
    let text = format!("{line}\n");
    // SAFETY: our pipe's write end and our text.
    unsafe { libc::write(fd, text.as_ptr().cast(), text.len()) };
}

macro_rules! scheck {
    ($cond:expr) => {{
        if !($cond) {
            note(&format!("FAKE FAIL {}:{}: {}", file!(), line!(), stringify!($cond)));
        }
    }};
}

fn conn() -> &'static Connection {
    FAKE.get().expect("the fake's binder")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn fake_incoming(object: u64, code: u32, _one_way: bool, r: &mut Reader<'_>) -> Result<Parcel, i32> {
    let mut reply = Parcel::new();
    if object == OBJ_MANAGER {
        let name = if code == onebridges::hal::SM_GET_SERVICE && r.token(onebridges::hal::SERVICE_MANAGER) {
            r.string(95)
        } else {
            None
        };
        let name = name.ok_or(aidl::UNKNOWN_TRANSACTION)?;
        note("getService");
        reply.put_ok();
        if name == hci_aidl::AIDL_HCI_INSTANCE {
            reply.put_local(OBJ_HCI);
        } else {
            reply.put_null_binder();
        }
        return Ok(reply);
    }
    if object == OBJ_HCI {
        // The base methods (descriptor, ping, version, hash) answer themselves.
        if let Some(answer) = aidl::standard(code, hci_aidl::AIDL_HCI, hci_aidl::AIDL_VERSION, "") {
            return Ok(answer);
        }
        if !r.token(hci_aidl::AIDL_HCI) {
            return Err(aidl::BAD_TYPE);
        }
        match code {
            hci_aidl::BT_INITIALIZE => {
                let cb = match r.binder() {
                    Object::Handle(h) => h,
                    _ => 0,
                };
                scheck!(cb != 0);
                scheck!(conn().acquire(cb).is_ok() && conn().watch(cb, 1).is_ok());
                // As the HAL's fromBinder asks: the callback's descriptor and version, before the answer.
                let descriptor =
                    conn().call(cb, aidl::INTERFACE, &Parcel::new(), |r| r.string(95).ok_or(-libc::EPROTO));
                scheck!(descriptor.as_deref() == Ok(hci_aidl::AIDL_CALLBACKS));
                let version =
                    conn().call(cb, aidl::GET_VERSION, &Parcel::with_token(hci_aidl::AIDL_CALLBACKS), aidl::take_i32);
                scheck!(version == Ok(hci_aidl::AIDL_VERSION));
                note("initialize");
                // initializationComplete(SUCCESS), then an event and ACL data: the HAL's order, before its reply.
                let mut p = Parcel::with_token(hci_aidl::AIDL_CALLBACKS);
                p.put_i32(0);
                scheck!(conn().call_void(cb, hci_aidl::CB_INIT_COMPLETE, &p).is_ok());
                let event = [0x0eu8, 0x04, 0x01, 0x03, 0x0c, 0x00];
                let mut p = Parcel::with_token(hci_aidl::AIDL_CALLBACKS);
                p.put_bytes(&event);
                scheck!(conn().call_void(cb, hci_aidl::CB_EVENT, &p).is_ok());
                let acl = [0x05u8, 0x20, 0x02, 0x00, 0xaa, 0xbb];
                let mut p = Parcel::with_token(hci_aidl::AIDL_CALLBACKS);
                p.put_bytes(&acl);
                scheck!(conn().call_void(cb, hci_aidl::CB_ACL, &p).is_ok());
                note("callbacks");
                reply.put_ok();
            }
            hci_aidl::BT_SEND_COMMAND | hci_aidl::BT_SEND_ACL | hci_aidl::BT_SEND_SCO | hci_aidl::BT_SEND_ISO => {
                let bytes = r.bytes().ok_or(aidl::UNKNOWN_TRANSACTION)?.to_vec();
                note(&format!("send {code}: {}", hex(&bytes)));
                reply.put_ok();
            }
            hci_aidl::BT_CLOSE => {
                scheck!(!with(|s| s.closed));
                with(|s| s.closed = true);
                note("close");
                reply.put_ok();
            }
            _ => return Err(aidl::UNKNOWN_TRANSACTION),
        }
        return Ok(reply);
    }
    Err(aidl::UNKNOWN_TRANSACTION)
}

fn run_fake(log: i32) {
    *STATE.lock().unwrap() = Some(Fake { log, closed: false });
    let c = Connection::open(
        None,
        Some(Box::new(|object, code, one_way, r: &mut Reader<'_>| fake_incoming(object, code, one_way, r))),
        Some(Box::new(|_cookie| note("the client died"))),
    );
    let Ok(c) = c else {
        println!("fake: no binder");
        common::exit(91);
    };
    if c.context_manager().is_err() {
        println!("fake: not the context manager");
        common::exit(91);
    }
    let _ = FAKE.set(c);
    note("ready");
    loop {
        std::thread::park();
    }
}

// ================================ the client (parent) ================================
fn start_fake() -> (libc::pid_t, i32) {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two fds.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let pid = common::fork(move || {
        // SAFETY: the read end is the parent's.
        unsafe { libc::close(fds[0]) };
        run_fake(fds[1]);
    });
    // SAFETY: the write end is the child's.
    unsafe { libc::close(fds[1]) };
    let mut line = [0u8; 6];
    // SAFETY: our pipe and buffer.
    let got = unsafe { libc::read(fds[0], line.as_mut_ptr().cast(), 6) };
    check!(got == 6 && &line == b"ready\n");
    (pid, fds[0])
}

/// Everything the fake noted so far (it is stopped first, so the pipe ends).
fn stop_fake(pid: libc::pid_t, log: i32) -> String {
    common::kill(pid);
    let mut text = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: our pipe and buffer.
        let n = unsafe { libc::read(log, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        text.extend_from_slice(&buf[..n as usize]);
    }
    // SAFETY: our pipe's read end.
    unsafe { libc::close(log) };
    String::from_utf8_lossy(&text).into_owned()
}

#[derive(Default)]
struct Got {
    packets: Vec<(u8, Vec<u8>)>,
    state: Option<HciState>,
}

/// What the client hands the controller: the packets and state changes, waited for, not polled.
#[derive(Default)]
struct Client {
    got: Mutex<Got>,
    cond: Condvar,
}

impl Client {
    fn rx(self: &Arc<Self>) -> hci::RxFn {
        let c = Arc::clone(self);
        Box::new(move |kind, packet: &[u8]| {
            let mut g = c.got.lock().unwrap();
            g.packets.push((kind, packet.to_vec()));
            c.cond.notify_all();
        })
    }

    fn changed(self: &Arc<Self>) -> hci::StateFn {
        let c = Arc::clone(self);
        Box::new(move |s| {
            let mut g = c.got.lock().unwrap();
            g.state = Some(s);
            c.cond.notify_all();
        })
    }

    fn wait(&self, ms: u64, done: impl Fn(&Got) -> bool) -> bool {
        let g = self.got.lock().unwrap();
        let (g, _) = self.cond.wait_timeout_while(g, Duration::from_millis(ms), |g| !done(g)).unwrap();
        done(&g)
    }
}

fn main() {
    let device = match binderfs::enter("ONEIOS_BINDER", "binder") {
        Entered::Device(path) => path,
        Entered::Skipped(why) => {
            println!("test_bt_aidl: skipped ({why})");
            return;
        }
        Entered::Ran(code) => std::process::exit(code),
    };
    let _ = device;
    common::log_to_stdout();
    let (pid, log) = start_fake();

    check!(hci_aidl::probe() == 20);

    let c = Arc::new(Client::default());
    let Some(h) = hci_aidl::open_aidl(c.rx(), c.changed()) else {
        let text = stop_fake(pid, log);
        println!("no client; the fake's log:\n{text}");
        std::process::exit(common::finish("bt aidl"));
    };
    check!(h.open().is_ok());
    check!(c.wait(3000, |g| g.state == Some(HciState::On)));
    check!(c.wait(3000, |g| g.packets.len() >= 2));
    let got = c.got.lock().unwrap().packets.clone();
    check!(got.iter().any(|(k, p)| *k == hci::H4_EVENT && p == &[0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]));
    check!(got.iter().any(|(k, p)| *k == hci::H4_ACL && p == &[0x05, 0x20, 0x02, 0x00, 0xaa, 0xbb]));
    check!(h.send(hci::H4_COMMAND, &[0x03, 0x0c, 0x00]).is_ok());
    h.close();
    drop(h);

    let text = stop_fake(pid, log);
    for want in ["getService", "initialize", "callbacks", "send 4: 03 0c 00", "close"] {
        check!(text.contains(want));
        if !text.contains(want) {
            println!("  missing after the last match: {want}  the fake's log:\n{text}");
        }
    }
    check!(!text.contains("FAKE FAIL"));
    if text.contains("FAKE FAIL") {
        println!("  the fake's log:\n{text}");
    }
    std::process::exit(common::finish("bt aidl"));
}
