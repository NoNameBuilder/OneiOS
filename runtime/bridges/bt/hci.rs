// SPDX-License-Identifier: MIT
//! Bluetooth bridge, the part every back end shares: one HCI controller with packet checks and a state the guest's
//! transport (drivers/bluetooth) follows. The back end for the phone is [`super::hci_hidl`] (HIDL, the S24 Ultra) or
//! [`super::hci_aidl`] (stable AIDL, Android 16+); [`super::hci_sim`] stands in for it on a PC.
//! Packet types are the H4 indicators: 1 command, 2 ACL, 3 SCO, 4 event, 5 ISO. C: bt/hci.c.

use super::le_scan::{LeScan, Verdict};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

pub const H4_COMMAND: u8 = 1;
pub const H4_ACL: u8 = 2;
pub const H4_SCO: u8 = 3;
pub const H4_EVENT: u8 = 4;
pub const H4_ISO: u8 = 5;

/// The controller's state as the guest's transport sees it (OB_HCI_STATE_*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum HciState {
    #[default]
    Off = 0,
    On = 1,
    Failed = 2,
}

/// A packet for the guest: its H4 type and bytes.
pub type RxFn = Box<dyn Fn(u8, &[u8]) + Send + Sync>;
/// The controller's state changed.
pub type StateFn = Box<dyn Fn(HciState) + Send + Sync>;
/// What makes a controller when the guest first opens it (the bridge device's hci_factory).
pub type HciFactory = fn(RxFn, StateFn) -> Option<Arc<Hci>>;

/// A back end: the phone's HAL, the stand-in, a test's fake. Errors are negative errnos.
pub trait HciBackend: Send + Sync {
    /// Starts initialisation; the state reports the result.
    fn open(&self) -> Result<(), i32>;
    fn send(&self, kind: u8, packet: &[u8]) -> Result<(), i32>;
    fn close(&self);
}

#[derive(Default)]
struct Inner {
    state: HciState,
    open: bool,
    scan: LeScan, // extended LE scan as legacy commands, if the controller refuses them (le_scan.rs)
}

pub struct Hci {
    backend: Box<dyn HciBackend>,
    rx: Option<RxFn>,
    state_fn: Option<StateFn>,
    inner: Mutex<Inner>,
    sent: AtomicU64,
    received: AtomicU64,
    dropped: AtomicU64,
    reports: AtomicU64, // advertising and inquiry reports: devices the controller heard
}

/// One HCI packet's own length field against its length.
pub fn packet_ok(kind: u8, p: &[u8]) -> bool {
    let n = p.len();
    match kind {
        H4_COMMAND | H4_SCO => n >= 3 && n == 3 + p[2] as usize, // opcode, length
        H4_ACL => n >= 4 && n == 4 + (p[2] as usize | (p[3] as usize) << 8), // handle, length
        H4_EVENT => n >= 2 && n == 2 + p[1] as usize,            // code, length
        H4_ISO => n >= 4 && n == 4 + ((p[2] as usize | (p[3] as usize) << 8) & 0x3fff),
        _ => false,
    }
}

impl Hci {
    /// A controller on the back end `make` builds; the back end gets a weak reference to deliver and report with.
    pub fn new<B: HciBackend + 'static>(
        make: impl FnOnce(Weak<Hci>) -> B,
        rx: Option<RxFn>,
        state: Option<StateFn>,
    ) -> Arc<Hci> {
        Arc::new_cyclic(|me| Hci {
            backend: Box::new(make(me.clone())),
            rx,
            state_fn: state,
            inner: Mutex::new(Inner::default()),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            reports: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn open(&self) -> Result<(), i32> {
        let was = std::mem::replace(&mut self.lock().open, true);
        if was {
            return Ok(());
        }
        let rc = self.backend.open();
        if rc.is_err() {
            self.lock().open = false;
            self.set_state(HciState::Failed);
        }
        rc
    }

    /// A packet from the guest. -EINVAL: malformed or an event; -ENOTCONN: the controller is not on.
    pub fn send(&self, kind: u8, packet: &[u8]) -> Result<(), i32> {
        if kind == H4_EVENT || !packet_ok(kind, packet) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(-libc::EINVAL);
        }
        let ready = {
            let i = self.lock();
            i.open && i.state == HciState::On
        };
        if !ready {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(-libc::ENOTCONN);
        }
        self.sent.fetch_add(1, Ordering::Relaxed);
        if kind == H4_COMMAND {
            let instead = self.lock().scan.command(packet);
            if let Some(commands) = instead {
                return commands.iter().try_for_each(|c| self.backend.send(H4_COMMAND, c));
            }
        }
        self.backend.send(kind, packet)
    }

    pub fn close(&self) {
        let was = {
            let mut i = self.lock();
            i.scan.reset(); // what the controller refused stays known; nothing is in flight
            std::mem::replace(&mut i.open, false)
        };
        if was {
            self.backend.close();
        }
        self.set_state(HciState::Off);
    }

    fn rx(&self, kind: u8, packet: &[u8]) {
        if let Some(rx) = &self.rx {
            rx(kind, packet)
        }
    }

    /// For back ends: a packet from the controller.
    pub fn deliver(&self, kind: u8, packet: &[u8]) {
        if !packet_ok(kind, packet) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.received.fetch_add(1, Ordering::Relaxed);
        if kind == H4_EVENT {
            let (step, first) = {
                let mut i = self.lock();
                let switched = i.scan.switched;
                let step = i.scan.event(packet);
                (step, switched == 0 && i.scan.switched != 0)
            };
            if first {
                crate::br_info!(
                    "bluetooth: the controller refused LE Set Extended Scan Parameters: LE scans go out as the legacy commands"
                );
            }
            for c in &step.to_controller {
                let _ = self.backend.send(H4_COMMAND, c);
            }
            match step.verdict {
                Verdict::Drop => return,
                Verdict::Replace => {
                    step.to_guest.iter().for_each(|up| self.rx(H4_EVENT, up));
                    return;
                }
                Verdict::Pass => {}
            }
        }
        // A device nearby, heard by the controller: an LE advertising report (LE Meta 0x3e, subevents 0x02 and 0x0d)
        // or an inquiry result (0x02, 0x22, 0x2f). One line for the first and for each power of two after: a scan
        // that finds devices can be read in the log.
        if kind == H4_EVENT
            && packet.len() >= 3
            && ((packet[0] == 0x3e && (packet[2] == 0x02 || packet[2] == 0x0d))
                || matches!(packet[0], 0x02 | 0x22 | 0x2f))
        {
            let n = self.reports.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                crate::br_info!(
                    "bluetooth: the controller hears devices: {n} report(s) so far ({})",
                    if packet[0] == 0x3e { "LE advertising" } else { "inquiry result" }
                );
            }
        }
        self.rx(kind, packet);
    }

    /// For back ends: the controller's state; the owner hears of a change.
    pub fn set_state(&self, state: HciState) {
        let changed = std::mem::replace(&mut self.lock().state, state) != state;
        if changed {
            if let Some(f) = &self.state_fn {
                f(state)
            }
        }
    }

    pub fn state(&self) -> HciState {
        self.lock().state
    }

    /// Packets sent to the controller, received from it, dropped (malformed, or while it was not on).
    pub fn counters(&self) -> (u64, u64, u64) {
        (self.sent.load(Ordering::Relaxed), self.received.load(Ordering::Relaxed), self.dropped.load(Ordering::Relaxed))
    }
}

impl Drop for Hci {
    fn drop(&mut self) {
        self.close(); // C: br_hci_destroy
    }
}

/// The controller's address as the phone stores it: text "AA:BB:.." (twelve hex digits with or without separators,
/// ended by the file's end or a NUL: the S24 Ultra's /mnt/vendor/efs/bluetooth/bt_addr is 18 bytes, the text and a
/// NUL), else exactly 6 raw bytes. None for an unreadable file or an all-zero address.
pub fn bt_address(path: &str) -> Option<[u8; 6]> {
    let raw = std::fs::read(path).ok()?;
    let raw = &raw[..raw.len().min(63)];
    let n = raw.len();
    let end = if n == 6 { n } else { raw.iter().position(|&b| b == 0).unwrap_or(n) };
    let mut hex = Vec::new();
    let mut text = true;
    for &c in &raw[..end] {
        match c {
            b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => hex.push((c as char).to_digit(16).expect("a hex digit") as u8),
            b':' | b'-' | b'\n' | b'\r' | b' ' => {}
            _ => {
                text = false;
                break;
            }
        }
    }
    let out: [u8; 6] = if text && hex.len() == 12 {
        std::array::from_fn(|i| hex[2 * i] << 4 | hex[2 * i + 1])
    } else if n == 6 {
        raw.try_into().expect("6 bytes")
    } else {
        return None;
    };
    (out != [0; 6]).then_some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// tests/test_bridges.c's fake_radio: a back end that records what it got.
    #[derive(Default)]
    pub struct FakeRadio {
        pub r: Mutex<Radio>,
    }
    #[derive(Default, Clone)]
    pub struct Radio {
        pub opens: u32,
        pub closes: u32,
        pub sends: u32,
        pub last_type: u8,
        pub last: Vec<u8>,
    }
    impl HciBackend for Arc<FakeRadio> {
        fn open(&self) -> Result<(), i32> {
            self.r.lock().unwrap().opens += 1;
            Ok(())
        }
        fn send(&self, kind: u8, packet: &[u8]) -> Result<(), i32> {
            let mut r = self.r.lock().unwrap();
            r.sends += 1;
            r.last_type = kind;
            r.last = packet.to_vec();
            Ok(())
        }
        fn close(&self) {
            self.r.lock().unwrap().closes += 1;
        }
    }

    /// What the owner got: packets and states.
    #[derive(Default)]
    pub struct Record {
        pub packets: u32,
        pub kind: u8,
        pub last: Vec<u8>,
        pub states: u32,
        pub state: Option<HciState>,
        pub reports: u32,
    }
    pub fn recorder() -> (Arc<Mutex<Record>>, RxFn, StateFn) {
        let rec: Arc<Mutex<Record>> = Arc::default();
        let (a, b) = (rec.clone(), rec.clone());
        let rx: RxFn = Box::new(move |kind, p| {
            let mut r = a.lock().unwrap();
            r.packets += 1;
            r.kind = kind;
            r.last = p.to_vec();
            if kind == H4_EVENT && p.len() >= 3 && p[0] == 0x3e && (p[2] == 0x02 || p[2] == 0x0d) {
                r.reports += 1;
            }
        });
        let state: StateFn = Box::new(move |s| {
            let mut r = b.lock().unwrap();
            r.states += 1;
            r.state = Some(s);
        });
        (rec, rx, state)
    }

    fn fake() -> (Arc<FakeRadio>, Arc<Mutex<Record>>, Arc<Hci>) {
        let radio = Arc::new(FakeRadio::default());
        let (rec, rx, state) = recorder();
        let r = radio.clone();
        let h = Hci::new(move |_| r, Some(rx), Some(state));
        (radio, rec, h)
    }

    // tests/test_bridges.c test_bt: the packet checks, the controller's state, the address file.
    #[test]
    fn controller() {
        let reset = [0x03u8, 0x0c, 0x00];
        let complete = [0x0eu8, 0x04, 0x01, 0x03, 0x0c, 0x00];
        let acl = [0x40u8, 0x20, 0x03, 0x00, 1, 2, 3];
        let iso = [0x01u8, 0x00, 0x02, 0x40, 9, 9];
        assert!(packet_ok(1, &reset) && !packet_ok(1, &reset[..2]) && !packet_ok(1, &complete));
        assert!(packet_ok(4, &complete) && !packet_ok(4, &complete[..5]));
        assert!(
            packet_ok(2, &acl)
                && !packet_ok(2, &[&acl[..], &[0]].concat())
                && packet_ok(5, &iso)
                && !packet_ok(9, &acl)
        );

        let (radio, rec, h) = fake();
        assert_eq!(h.send(1, &reset), Err(-libc::ENOTCONN)); // not open
        assert!(h.open().is_ok() && radio.r.lock().unwrap().opens == 1 && h.open().is_ok());
        assert_eq!(radio.r.lock().unwrap().opens, 1);
        assert_eq!(h.send(1, &reset), Err(-libc::ENOTCONN)); // open, the HAL has not finished
        h.set_state(HciState::On);
        assert!(rec.lock().unwrap().states == 1 && rec.lock().unwrap().state == Some(HciState::On));
        assert!(h.send(1, &reset).is_ok());
        assert!(radio.r.lock().unwrap().sends == 1 && radio.r.lock().unwrap().last_type == 1);
        assert_eq!(h.send(4, &complete), Err(-libc::EINVAL)); // a guest does not send events
        assert_eq!(h.send(2, &acl[..6]), Err(-libc::EINVAL));
        assert_eq!(radio.r.lock().unwrap().sends, 1);
        h.deliver(4, &complete);
        h.deliver(4, &complete[..3]); // malformed: dropped
        {
            let r = rec.lock().unwrap();
            assert!(r.packets == 1 && r.kind == 4 && r.last.len() == 6);
        }
        h.close();
        assert!(radio.r.lock().unwrap().closes == 1 && rec.lock().unwrap().state == Some(HciState::Off));
    }

    #[test]
    fn address_file() {
        let dir = std::env::temp_dir().join(format!("onebridges-bt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bt_addr");
        let p = path.to_str().unwrap();
        std::fs::write(&path, "A4:75:B9:01:02:03\n").unwrap();
        assert_eq!(bt_address(p), Some([0xa4, 0x75, 0xb9, 1, 2, 3]));
        std::fs::write(&path, b"A4:75:B9:01:02:03\0").unwrap(); // the S24 Ultra's file: 17 characters and a NUL
        assert_eq!(bt_address(p), Some([0xa4, 0x75, 0xb9, 1, 2, 3]));
        std::fs::write(&path, "a475b9010203").unwrap();
        assert_eq!(bt_address(p).map(|a| a[5]), Some(3));
        std::fs::write(&path, [0x10u8, 0x20, 0x30, 0x40, 0x50, 0x60]).unwrap();
        assert_eq!(bt_address(p).map(|a| a[0]), Some(0x10));
        std::fs::write(&path, "00:00:00:00:00:00").unwrap();
        assert_eq!(bt_address(p), None);
        assert_eq!(bt_address("/nonexistent"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // tests/test_bridges.c test_le_scan
    #[test]
    fn le_scan_through_the_controller() {
        let mask = [0x01u8, 0x20, 8, 0x10, 0x10, 0, 0, 0, 0, 0, 0]; // ext adv report on, legacy off
        let params = [0x41u8, 0x20, 8, 0x01, 0x00, 0x01, 0x01, 0x60, 0x00, 0x30, 0x00]; // own 1, filter 0, 1M: active 0x60/0x30
        let enable = [0x42u8, 0x20, 6, 0x01, 0x02, 0, 0, 0, 0];
        let refused = [0x0eu8, 4, 1, 0x41, 0x20, 0x01];
        let ok_200b = [0x0eu8, 4, 1, 0x0b, 0x20, 0x00];
        let ok_2041 = [0x0eu8, 4, 1, 0x41, 0x20, 0x00];
        let ok_2001 = [0x0eu8, 4, 1, 0x01, 0x20, 0x00];
        let ok_200c = [0x0eu8, 4, 1, 0x0c, 0x20, 0x00];
        // one ADV_IND from 11:22:33:44:55:66 (public), 3 bytes of data, RSSI -60
        let report = [0x3eu8, 15, 0x02, 1, 0x00, 0x00, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 3, 0x02, 0x01, 0x06, 0xc4];

        // A controller that takes the extended commands: nothing changes.
        let (radio, rec, h) = fake();
        assert!(h.open().is_ok());
        h.set_state(HciState::On);
        assert!(h.send(1, &mask).is_ok() && radio.r.lock().unwrap().last == mask);
        assert!(h.send(1, &params).is_ok());
        assert!(radio.r.lock().unwrap().sends == 2 && radio.r.lock().unwrap().last == params);
        h.deliver(4, &ok_2041);
        assert!(rec.lock().unwrap().packets == 1 && rec.lock().unwrap().last == ok_2041);
        h.deliver(4, &report);
        assert!(rec.lock().unwrap().packets == 2 && rec.lock().unwrap().last.len() == report.len()); // a legacy report stays one
        drop(h);

        // A controller that refuses them (the Mi A3's WCN3950 in iOSPlus10's log).
        let (radio, rec, h) = fake();
        assert!(h.open().is_ok());
        h.set_state(HciState::On);
        assert!(h.send(1, &mask).is_ok() && h.send(1, &params).is_ok() && radio.r.lock().unwrap().sends == 2);
        h.deliver(4, &refused);
        assert_eq!(rec.lock().unwrap().packets, 0); // the refusal is not the guest's
        {
            let r = radio.r.lock().unwrap();
            assert!(r.sends == 4 && r.last.len() == 10); // the mask with legacy reports on, then 0x200b
            assert_eq!(r.last, b"\x0b\x20\x07\x01\x60\x00\x30\x00\x01\x00");
        }
        h.deliver(4, &ok_2001);
        assert_eq!(rec.lock().unwrap().packets, 0); // the mask's answer: ours
        h.deliver(4, &ok_200b);
        assert!(rec.lock().unwrap().packets == 1 && rec.lock().unwrap().last == ok_2041); // answered as 0x2041
        assert!(h.send(1, &enable).is_ok() && radio.r.lock().unwrap().last == b"\x0c\x20\x02\x01\x01");
        h.deliver(4, &ok_200c);
        {
            let r = rec.lock().unwrap();
            assert!(r.packets == 2 && r.last[3] == 0x42 && r.last[4] == 0x20 && r.last[5] == 0);
        }
        h.deliver(4, &report);
        {
            let r = rec.lock().unwrap();
            let l = &r.last;
            assert!(r.packets == 3 && l.len() == 2 + 2 + 24 + 3); // one extended report
            assert!(l[0] == 0x3e && l[1] == 2 + 24 + 3 && l[2] == 0x0d && l[3] == 1 && l[4] == 0x13 && l[5] == 0);
            assert!(l[7..13] == *b"\x66\x55\x44\x33\x22\x11" && l[13] == 1 && l[16] == 0x7f && l[17] == 0xc4);
            assert!(l[27] == 3 && l[28..31] == *b"\x02\x01\x06");
        }
        // After a close the controller is still known to refuse: the next 0x2041 goes out as 0x200b at once.
        h.close();
        assert!(h.open().is_ok());
        h.set_state(HciState::On);
        let sends = radio.r.lock().unwrap().sends;
        assert!(h.send(1, &params).is_ok());
        assert!(radio.r.lock().unwrap().sends == sends + 1 && radio.r.lock().unwrap().last[0] == 0x0b);
    }
}
