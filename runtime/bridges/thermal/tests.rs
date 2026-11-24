// SPDX-License-Identifier: MIT
//! Host tests of the thermal bridge (test_thermal.c): the limits parser, the rules and their hysteresis, the step's
//! CPUs on the S24 Ultra's topology, the sysfs reader on a fake tree, the worker's two listeners, the load line, and
//! the HAL's temperatures as a reading.

use super::*;
use crate::cpu::{CoreKind, CpuPlan, HostCpu, Topology};
use crate::log::Capture;
use std::fs;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

/// A fake root of its own for each test, removed at the end.
struct Base(String);

impl Base {
    fn new(name: &str) -> Base {
        let dir = std::env::temp_dir().join(format!("test_thermal.{}.{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Base(dir.display().to_string())
    }

    /// Written whole, then renamed over the old file: the worker reads these files while a test changes them, and a
    /// truncated file read as a missing lrp made the case fall back to the ap thermistor for one poll: one more
    /// report than the test counts (test_worker, about 1 run in 6 before 2026-11-22).
    fn put(&self, rel: &str, text: &str) {
        let path = format!("{}{rel}", self.0);
        fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        let tmp = format!("{path}.new");
        fs::write(&tmp, text).unwrap();
        fs::rename(&tmp, &path).unwrap();
    }

    fn zone(&self, n: u32, kind: &str, milli: i64) {
        self.put(&format!("/sys/class/thermal/thermal_zone{n}/type"), &format!("{kind}\n"));
        self.put(&format!("/sys/class/thermal/thermal_zone{n}/temp"), &format!("{milli}\n"));
    }

    /// trip_point_<i>_temp and _type of zone n
    fn trip(&self, n: u32, i: u32, milli: i64, kind: &str) {
        self.put(&format!("/sys/class/thermal/thermal_zone{n}/trip_point_{i}_temp"), &format!("{milli}\n"));
        self.put(&format!("/sys/class/thermal/thermal_zone{n}/trip_point_{i}_type"), &format!("{kind}\n"));
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn raw_with(skin: i32, soc: i32, battery: i32) -> Raw {
    Raw { skin, soc, battery, ..Raw::default() }
}

fn ms(n: u64) {
    sleep(Duration::from_millis(n));
}

#[test]
fn parse() {
    assert_eq!(parse_limits::<5>("39,42,44,46,50"), Some([390, 420, 440, 460, 500]));
    let v = parse_limits::<5>("38.5, 41,43,45,49\n").unwrap();
    assert!(v[0] == 385 && v[1] == 410);
    assert_eq!(parse_limits::<5>("44,42,46,48,50"), None); // not ascending
    assert_eq!(parse_limits::<5>("39,42"), None);
    assert_eq!(parse_limits::<2>("90,100,110"), None);
    assert_eq!(parse_limits::<2>("x"), None);
    assert_eq!(parse_limits::<2>("90,100"), Some([900, 1000]));
    assert_eq!(parse_limits::<2>("nan,100"), None); // C took NaN (no range check catches it): a bug, fixed
}

#[test]
fn rules() {
    let l = Limits::default();
    let s = decide(&l, &raw_with(300, 500, 300), None);
    assert!(s.level == NOMINAL && s.cap == 0 && s.reason == 0);
    let prev = decide(&l, &raw_with(395, 500, 300), Some(&s));
    assert!(prev.level == LIGHT && prev.reason == BY_SKIN && prev.cap == 0);
    let s = decide(&l, &raw_with(445, 500, 300), Some(&prev));
    assert!(s.level == HEAVY && s.cap == 0); // heavy: no step of ours (all cores)
                                             // hysteresis: 43.0 C keeps HEAVY (on at 44, off below 42), 41.9 C drops to MODERATE (whose own threshold is 42,
                                             // held to 40)
    let prev = decide(&l, &raw_with(430, 500, 300), Some(&s));
    assert_eq!(prev.level, HEAVY);
    let s = decide(&l, &raw_with(419, 500, 300), Some(&prev));
    assert!(s.level == MODERATE && s.cap == 0);
    // rising again needs the full threshold: 43.9 C is not HEAVY from MODERATE
    let prev = decide(&l, &raw_with(439, 500, 300), Some(&s));
    assert_eq!(prev.level, MODERATE);
    // the SoC rule alone
    let s = decide(&l, &raw_with(350, 955, 300), Some(&prev));
    assert!(s.level == HEAVY && s.reason == BY_SOC && s.skin_level == NOMINAL);
    let prev = decide(&l, &raw_with(350, 1055, 300), Some(&s));
    assert!(prev.level == TRAPPING && prev.cap == 0); // the SoC at 105 C: the kernel's own trips, no step
    let s = decide(&l, &raw_with(350, 1010, 300), Some(&prev));
    assert_eq!(s.level, TRAPPING); // held to 100 C
    let prev = decide(&l, &raw_with(350, 990, 300), Some(&s));
    assert_eq!(prev.level, HEAVY); // below 100, above 90
                                   // two rules at one level: both named
    let s = decide(&l, &raw_with(445, 950, 300), Some(&prev));
    assert!(s.level == HEAVY && s.reason == (BY_SKIN | BY_SOC));
    // the battery
    let s = decide(&l, &raw_with(350, 500, 505), None);
    assert!(s.level == TRAPPING && s.reason == BY_BATTERY && s.cap == CAP_CRITICAL); // the cell at 50 C: the step
    let prev = decide(&l, &raw_with(350, 500, 485), Some(&s));
    assert_eq!(prev.cap, CAP_CRITICAL); // held to 48 C
    let s = decide(&l, &raw_with(350, 500, 475), Some(&prev));
    assert!(s.level == HEAVY && s.cap == 0);
    // a missing reading keeps its rule's level
    let prev = s;
    let r = raw_with(UNKNOWN, UNKNOWN, 300);
    let s = decide(&l, &r, Some(&prev));
    assert!(s.skin_level == prev.skin_level && s.soc_level == prev.soc_level);
    assert_eq!(decide(&l, &r, None).level, NOMINAL);
    // sleeping: iOS is told, and nothing is pinned (the case is not the battery)
    let s = decide(&l, &raw_with(505, 500, 300), None);
    assert!(s.level == SLEEPING && s.cap == 0);
    let prev = decide(&l, &raw_with(485, 500, 300), Some(&s));
    assert!(prev.level == SLEEPING && prev.cap == 0); // held to 48 C
    let s = decide(&l, &raw_with(475, 1060, 300), Some(&prev)); // the case at 47.5 C, the SoC at 106 C
    assert!(s.level == TRAPPING && s.cap == 0); // all cores: the SoC is the kernel's
    let mut at = State { skin_level: TRAPPING, soc_level: TRAPPING, battery_level: HEAVY, ..State::default() };
    assert_eq!(cap_for(&at), 0);
    at.skin_level = SLEEPING;
    assert_eq!(cap_for(&at), 0); // the case at 50 C: no cap of ours
    at.skin_level = NOMINAL;
    at.battery_level = TRAPPING;
    assert_eq!(cap_for(&at), CAP_CRITICAL);
    assert!(level_name(TRAPPING) == "trapping" && level_name(7) == "?");
    // what is worth a message
    let (a, mut b) = (s.clone(), s.clone());
    assert!(!a.changed(&b, 1000));
    assert!(a.changed(&b, REPORT_MS));
    b.level = HEAVY;
    assert!(a.changed(&b, 0));
}

/// The S24 Ultra: cpu0-1 A520 (policy0, 2.27 GHz), cpu2-4 A720 (policy2, 3.15), cpu5-6 A720 (policy5, 2.96), cpu7 X4
/// (policy7, 3.40).
fn s24_topology() -> Topology {
    const POLICY: [u32; 8] = [0, 0, 2, 2, 2, 5, 5, 7];
    const CAP: [u32; 8] = [325, 325, 870, 870, 870, 870, 870, 1024];
    const KHZ: [u32; 8] = [2265600, 2265600, 3148800, 3148800, 3148800, 2956800, 2956800, 3398400];
    let cpu = (0..8)
        .map(|i| HostCpu {
            present: true,
            policy: POLICY[i],
            capacity: CAP[i],
            max_khz: KHZ[i],
            kind: if i < 2 { CoreKind::E } else { CoreKind::P },
            ..HostCpu::default()
        })
        .collect();
    Topology { cpu, n_e: 2, n_p: 6 }
}

#[test]
fn masks() {
    let t = s24_topology();
    assert!(t.thermal_mask(CoreKind::P, 0) == 0xfc && t.thermal_mask(CoreKind::E, 0) == 0x03);
    assert_eq!(t.thermal_mask(CoreKind::P, 1), 0x7c); // without the X4
    assert_eq!(t.thermal_mask(CoreKind::P, 2), 0x63); // cpu5-6 (the slowest top clock) and the E cores
    assert_eq!(t.thermal_mask(CoreKind::P, 3), 0x03);
    assert_eq!(t.thermal_mask(CoreKind::E, 3), 0x03);
    let base = CpuPlan::make(&t, 8, false).unwrap();
    let plan = base.clone();
    plan.cap(&base, &t, 1);
    assert!(plan.affinity(0) == 0x03 && plan.affinity(2) == 0x7c && plan.affinity(7) == 0x7c);
    // step 2: the P vCPUs on cpu5-6 and the E cores (they lost the E cores before 2026-11-13: 0x60), the E vCPUs on
    // theirs
    plan.cap(&base, &t, 2);
    assert!(
        plan.affinity(0) == 0x03 && plan.affinity(1) == 0x03 && plan.affinity(2) == 0x63 && plan.affinity(7) == 0x63
    );
    plan.cap(&base, &t, 3);
    assert!(plan.affinity(0) == 0x03 && plan.affinity(7) == 0x03);
    // cpu-pin: one core each while the kind's vCPUs fit the mask, else the whole mask (never three to a core)
    plan.cap(&base, &t, 0);
    assert_eq!(plan.affinity(7), 0xfc);
    assert!(plan.pin_mask(&base, 0) == 0x01 && plan.pin_mask(&base, 1) == 0x02);
    assert!(plan.pin_mask(&base, 2) == 0x04 && plan.pin_mask(&base, 7) == 0x80);
    let seen = (2..8).fold(0, |m, i| m | plan.pin_mask(&base, i));
    assert_eq!(seen, 0xfc); // six P vCPUs, six P cores, each its own
    plan.cap(&base, &t, 1); // five P cores for six P vCPUs
    assert!(plan.pin_mask(&base, 2) == 0x7c && plan.pin_mask(&base, 0) == 0x01);
    plan.cap(&base, &t, 2);
    assert!(plan.pin_mask(&base, 7) == 0x63 && plan.pin_mask(&base, 1) == 0x02);
    assert_eq!(plan.pin_mask(&base, 8), 0);
    plan.cap(&base, &t, 0);
    // pinned: each vCPU's own core when the step allows it, the step's CPUs when it does not
    let base = CpuPlan::make(&t, 8, true).unwrap();
    let plan = base.clone();
    plan.cap(&base, &t, 1);
    assert!(base.affinity(2) == 0x80 && plan.affinity(2) == 0x7c); // the X4's vCPU moves to the other P cores
    assert_eq!(plan.affinity(3), base.affinity(3));
    // one kind of core: the steps still leave CPUs
    let cpu = (0..4)
        .map(|_| HostCpu { present: true, policy: 0, max_khz: 2_000_000, kind: CoreKind::P, ..HostCpu::default() })
        .collect();
    let same = Topology { cpu, n_p: 4, ..Topology::default() };
    assert!(same.thermal_mask(CoreKind::P, 1) == 0x0f && same.thermal_mask(CoreKind::P, 3) == 0x0f);
}

/// The SoC rule on the kernel's trips and on the lowest of the last readings (YumikoHayamato/OneiOS,
/// s25-ultra-sm8750): opt-in here, so the default (one reading, 95/105 C) is checked to be today's as well.
#[test]
fn trips() {
    let mut l = Limits::default();
    assert!(l.soc_samples == 1 && l.soc == [950, 1050]);
    // the S25 Ultra (SM8750, the fork's session 3): first passive trip 105 C, hot 125 C: HEAVY 115, TRAPPING 120 C
    assert!(l.from_trips(1050, 1250) && l.soc == [1150, 1200] && l.soc_hyst == 50);
    // no trips: the defaults stay
    let mut l = Limits::default();
    assert!(!l.from_trips(UNKNOWN, 1250) && l.soc == [950, 1050]);
    // never lower than what was there: a first trip at 80 C keeps 95 and 105 C
    assert!(!l.from_trips(800, UNKNOWN) && l.soc == [950, 1050]);
    // no hot trip: TRAPPING 15 C over the first passive one
    let mut l = Limits::default();
    assert!(l.from_trips(1000, UNKNOWN) && l.soc == [1100, 1150]);
    // a hot trip too close to the first passive one: 15 C over the first passive one, still above HEAVY
    let mut l = Limits::default();
    assert!(l.from_trips(1050, 1100) && l.soc == [1150, 1200]);

    // One reading (the default): a single 105.5 C reading is TRAPPING at once, as before this port.
    let mut l = Limits::default();
    let s = decide(&l, &raw_with(350, 1055, 300), None);
    assert!(s.level == TRAPPING && s.soc_n == 1 && s.soc_held == 1055);
    // Three readings: one core's spike to 105.5 C after two at 50 C is nothing for iOS; the third in a row is.
    l.soc_samples = 3;
    let cool = raw_with(350, 500, 300);
    let prev = decide(&l, &cool, None);
    let s = decide(&l, &cool, Some(&prev));
    let hot = raw_with(350, 1055, 300);
    let prev = decide(&l, &hot, Some(&s));
    assert!(prev.level == NOMINAL && prev.soc_held == 500 && prev.soc_n == 3);
    let s = decide(&l, &hot, Some(&prev));
    assert!(s.level == NOMINAL && s.soc_held == 500);
    let prev = decide(&l, &hot, Some(&s));
    assert!(prev.level == TRAPPING && prev.reason == BY_SOC && prev.soc_held == 1055 && prev.cap == 0);
    // falling needs one reading: 70 C ends it at once
    let s = decide(&l, &raw_with(350, 700, 300), Some(&prev));
    assert!(s.level == NOMINAL && s.soc_held == 700);
    // a missing SoC reading keeps the window and the level
    let prev = decide(&l, &raw_with(350, UNKNOWN, 300), Some(&s));
    assert!(prev.soc_level == NOMINAL && prev.soc_n == 3 && prev.soc_seen[0] == 700);
    // more than the most: the window stops at SOC_SAMPLES_MAX
    l.soc_samples = 99;
    let r = raw_with(350, 900, 300);
    let mut s = decide(&l, &r, None);
    for _ in 0..6 {
        s = decide(&l, &r, Some(&s));
    }
    assert_eq!(s.soc_n, SOC_SAMPLES_MAX);

    // The fork's session 2 on the S25 Ultra (the SoC readings of its thermal lines, in order; the CPU floors pinned at
    // their tops by a hand-made switch script, the case at 37-46 C): with the S24's 95/105 C and one reading the level
    // changed 20 times in 8 minutes, 5 of them into TRAPPING (iOS's "too hot" screen); on the phone's trips and 3
    // readings the SoC rule stays NOMINAL.
    const S2: [i32; 33] = [
        915, 1019, 1035, 1042, 1050, 1054, 1046, 872, 795, 977, 892, 861, 923, 776, 1035, 791, 1038, 683, 1038, 1042,
        1054, 961, 795, 1038, 1035, 1050, 977, 1035, 1038, 1054, 961, 1054, 1042,
    ];
    let count_changes = |l: &Limits, check: bool| {
        let mut prev: Option<State> = None;
        let mut changes = 0;
        for soc in S2 {
            let s = decide(l, &raw_with(400, soc, 320), prev.as_ref());
            changes += usize::from(prev.as_ref().is_some_and(|p| s.soc_level != p.soc_level));
            if check {
                assert!(s.soc_level == NOMINAL && s.cap == 0);
            }
            prev = Some(s);
        }
        changes
    };
    let mut l = Limits::default();
    let changes_today = count_changes(&l, false);
    l.from_trips(1050, 1250);
    l.soc_samples = 3;
    let changes = count_changes(&l, true);
    assert!(changes_today >= 10 && changes == 0);
}

/// The S25 Ultra's trips in sysfs (the fork's phone): a CPU zone (a 135 C placeholder, 105, 118, 125 hot), a zone
/// with the hot trip only, the GPU (105, 125 hot), the DDR (100, 125 hot); the crystal's trips are not the SoC's. Then
/// the bridge's start: the trips line on every phone, the limits raised only with soc_trips
/// (ONEIOS_THERMAL_SOC_TRIPS=1).
#[test]
fn trip_sysfs() {
    let hw = crate::hw::settings_lock();
    let b = Base::new("pa3q");
    b.zone(1, "cpu-1-1-1", 61500);
    b.zone(2, "cpuss-0", 40000);
    b.zone(3, "gpuss-0", 55000);
    b.zone(4, "ddr", 45000);
    b.zone(5, "xo-therm", 33204);
    for (n, i, milli, kind) in [
        (1, 0, 135000, "passive"),
        (1, 1, 105000, "passive"),
        (1, 2, 118000, "passive"),
        (1, 3, 125000, "hot"),
        (2, 0, 135000, "passive"),
        (2, 1, 125000, "hot"),
        (3, 0, 135000, "passive"),
        (3, 1, 105000, "passive"),
        (3, 2, 125000, "hot"),
        (4, 0, 100000, "passive"),
        (4, 1, 125000, "hot"),
        (5, 0, 70000, "passive"),
    ] {
        b.trip(n, i, milli, kind);
    }
    b.put("/sys/class/power_supply/battery/temp", "331\n");
    let s = Sensors::find(&b.0).unwrap();
    assert!(s.trip_first == 1050 && s.trip_hot == 1250 && s.trip_zones == 3); // the median of 105, 105, 100 C

    let log = Capture::start();
    let mut o = Options { root: Some(b.0.clone()), poll_ms: 1000, ..Options::default() };
    let t = Thermal::open(&o).unwrap();
    assert!(log.has(
        "thermal: the SoC rule: heavy from 95.0 C, trapping from 105.0 C, on 1 reading; the kernel's trips: first \
         passive 105.0 C, hot 125.0 C (the median of 3 SoC zones)"
    ));
    t.close();
    log.clear();
    o.soc_trips = true;
    let t = Thermal::open(&o).unwrap();
    assert!(log.has("heavy from 115.0 C, trapping from 120.0 C, on 1 reading, raised to the kernel's trips"));
    t.close();
    drop(log);
    drop(hw);

    // A tree without trips (the S24 Ultra's stand-in has none): unknown, and nothing is raised.
    let plain = Base::new("plain");
    plain.zone(1, "cpu-1-2-1", 61500);
    let s = Sensors::find(&plain.0).unwrap();
    assert!(s.trip_first == UNKNOWN && s.trip_hot == UNKNOWN && s.trip_zones == 0);
}

#[test]
fn sysfs() {
    let b = Base::new("sysfs");
    b.zone(0, "aoss-0", 30000);
    b.zone(1, "cpu-1-2-1", 61500);
    b.zone(2, "cpuss-0", 40000);
    b.zone(3, "gpuss-0", 55000);
    b.zone(4, "ddr", 45000);
    b.zone(5, "xo-therm", 33204);
    b.zone(6, "epm0", -273000);
    b.zone(7, "cpu-0-0-0", -273000); // an off sensor is not the hottest
                                     // SM8850: advertised by the kernel at 105 C while CPU sensors read 35-37 C. Trip/threshold pseudo-zones must not
                                     // count as live measurements.
    b.zone(8, "cpu-hw-trip-0", 105000);
    b.zone(9, "cpu-hw-trip-1", 105000);
    b.put("/sys/class/sec/sec-ap-thermistor/temperature", "362\n");
    b.put("/sys/class/power_supply/battery/temp", "331\n");
    let s = Sensors::find(&b.0).unwrap();
    assert_eq!(s.zones.len(), 6); // the two hardware trips are excluded
    assert!(s.zones.iter().all(|z| !z.kind.starts_with("cpu-hw-trip-")));
    let r = s.read();
    assert!(r.cpu == 615 && r.gpu == 550 && r.ddr == 450 && r.xo == 332 && r.soc == 615 && r.soc_zone == "cpu-1-2-1");
    assert!(r.ap == 362 && r.battery == 331 && r.lrp == UNKNOWN && r.skin == 362); // no lrp: the board thermistor
    let limits = Limits::default();
    let state = decide(&limits, &r, None);
    assert!(state.level == NOMINAL && state.cap == 0 && state.soc_level == NOMINAL);
    // An actual CPU sensor at 105.5 C must still produce the real SoC TRAPPING pressure: rejecting trip zones may NOT
    // mask legitimate high temperatures.
    b.zone(1, "cpu-1-2-1", 105500);
    let r = s.read();
    let state = decide(&limits, &r, None);
    assert!(r.cpu == 1055 && r.soc == 1055 && state.level == TRAPPING && state.reason == BY_SOC && state.cap == 0);
    b.zone(1, "cpu-1-2-1", 61500);
    b.put("/sys/class/power_supply/battery/lrp", "401\n");
    let r = s.read();
    assert!(r.lrp == 401 && r.skin == 401);
    b.zone(3, "gpuss-0", 71000);
    let r = s.read();
    assert!(r.soc == 710 && r.soc_zone == "gpuss-0");
    // The user's real SM8850 snapshot: real CPU 37.4 C / GPU 35.9 C / DDR 35.5 C / case 32.1 C / battery 30.1 C,
    // despite 105 C trip nodes.
    b.zone(1, "cpu-1-2-1", 37400);
    b.zone(2, "cpuss-0", 35900);
    b.zone(3, "gpuss-0", 35900);
    b.zone(4, "ddr", 35500);
    b.put("/sys/class/sec/sec-ap-thermistor/temperature", "322\n");
    b.put("/sys/class/power_supply/battery/temp", "301\n");
    b.put("/sys/class/power_supply/battery/lrp", "321\n");
    let r = s.read();
    assert!(r.cpu == 374 && r.gpu == 359 && r.ddr == 355 && r.soc == 374 && r.soc_zone == "cpu-1-2-1");
    assert!(r.ap == 322 && r.battery == 301 && r.lrp == 321 && r.skin == 321);
    let state = decide(&limits, &r, None);
    assert!(state.level == NOMINAL && state.cap == 0 && state.soc_level == NOMINAL);
    // nothing at all
    fs::create_dir_all(format!("{}/none", b.0)).unwrap();
    assert!(Sensors::find(&format!("{}/none", b.0)).is_none());
}

/// The base S25's PA1Q tree: five sec-thermistors (ap, cf, lp, pa, wf), the max77775's battery temp and, when the
/// charger reports it, battery/lrp. The bridge reads ap for the board, battery for the cell, and lrp for the case;
/// the other four thermistors are present but are not readings it uses. Failure paths: an off or out-of-range sensor
/// is missing, and with no ap, battery or lrp the bridge stays off.
#[test]
fn s25_sensors() {
    let b = Base::new("s25");
    for (name, v) in [("ap", "338"), ("cf", "320"), ("lp", "345"), ("pa", "360"), ("wf", "315")] {
        b.put(&format!("/sys/class/sec/sec-{name}-thermistor/temperature"), &format!("{v}\n"));
    }
    b.put("/sys/class/power_supply/battery/temp", "305\n");
    let s = Sensors::find(&b.0).unwrap();
    let r = s.read();
    assert!(r.ap == 338 && r.battery == 305 && r.lrp == UNKNOWN && r.skin == 338);
    // The gauge's case sensor wins over the board thermistor, as on the S24 Ultra.
    b.put("/sys/class/power_supply/battery/lrp", "347\n");
    let r = s.read();
    assert!(r.lrp == 347 && r.skin == 347);
    // An off sensor (-273.0 C) or an out-of-range one is missing, so the skin falls back to ap.
    b.put("/sys/class/power_supply/battery/lrp", "-2730\n");
    let r = s.read();
    assert!(r.lrp == UNKNOWN && r.skin == 338);
    b.put("/sys/class/power_supply/battery/lrp", "4000\n");
    let r = s.read();
    assert!(r.lrp == UNKNOWN && r.skin == 338);
    // The other thermistors do not become the case, and do not keep the bridge on without ap/battery/lrp.
    b.put("/sys/class/sec/sec-lp-thermistor/temperature", "600\n");
    b.put("/sys/class/sec/sec-pa-thermistor/temperature", "610\n");
    let r = s.read();
    assert!(r.ap == 338 && r.skin == 338);
    b.put("/sys/class/sec/sec-ap-thermistor/temperature", "cold\n");
    b.put("/sys/class/power_supply/battery/temp", "-2730\n");
    assert!(Sensors::find(&b.0).is_none());
}

#[test]
fn worker() {
    let _hw = crate::hw::settings_lock();
    let b = Base::new("worker");
    b.put("/sys/class/power_supply/battery/lrp", "350\n");
    let o = Options { root: Some(b.0.clone()), poll_ms: 20, limits: Some(Limits::default()), ..Options::default() };
    let t = Thermal::open(&o).unwrap();
    let st = t.get();
    assert!(st.level == NOMINAL && st.raw.skin == 350);
    let (caps, reports) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (last_cap, last_level) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let on_cap: Listener = {
        let (caps, last_cap) = (Arc::clone(&caps), Arc::clone(&last_cap));
        Arc::new(move |s: &State| {
            caps.fetch_add(1, Ordering::Relaxed);
            last_cap.store(s.cap, Ordering::Relaxed);
        })
    };
    let on_report: Listener = {
        let (reports, last_level) = (Arc::clone(&reports), Arc::clone(&last_level));
        Arc::new(move |s: &State| {
            reports.fetch_add(1, Ordering::Relaxed);
            last_level.store(s.level, Ordering::Relaxed);
        })
    };
    let n = |a: &AtomicUsize| a.load(Ordering::Relaxed);
    let v = |a: &AtomicU32| a.load(Ordering::Relaxed);
    t.on_cap(Some(Arc::clone(&on_cap)));
    t.on_report(Some(on_report));
    ms(150);
    assert!(n(&caps) == 0 && n(&reports) == 0); // nothing changed: no message before REPORT_MS
    b.put("/sys/class/power_supply/battery/lrp", "447\n");
    ms(200);
    assert_eq!(n(&caps), 0); // heavy: iOS is told, no step of ours (all cores)
    assert!(n(&reports) == 1 && v(&last_level) == HEAVY);
    b.put("/sys/class/power_supply/battery/lrp", "462\n");
    ms(200);
    assert!(n(&caps) == 0 && n(&reports) == 2 && v(&last_level) == TRAPPING);
    b.put("/sys/class/power_supply/battery/lrp", "505\n");
    ms(200);
    assert!(n(&caps) == 0 && n(&reports) == 3 && v(&last_level) == SLEEPING); // the case at 50 C: iOS is told, no step
    b.put("/sys/class/power_supply/battery/temp", "505\n");
    ms(200);
    assert!(n(&caps) == 1 && v(&last_cap) == CAP_CRITICAL); // the cell at 50 C: the one step
    b.put("/sys/class/power_supply/battery/temp", "475\n");
    b.put("/sys/class/power_supply/battery/lrp", "300\n");
    ms(200);
    assert!(n(&caps) == 2 && v(&last_cap) == 0 && v(&last_level) == HEAVY);
    t.on_cap(None);
    t.on_report(None);
    b.put("/sys/class/power_supply/battery/lrp", "510\n");
    ms(150);
    assert!(n(&caps) == 2 && n(&reports) == 5);
    assert_eq!(t.get().level, SLEEPING); // still read with no listener: the next listener hears it
    b.put("/sys/class/power_supply/battery/temp", "510\n");
    ms(150);
    assert!(n(&caps) == 2); // no listener: nothing sent
    t.on_cap(Some(on_cap));
    ms(150);
    assert!(n(&caps) == 3 && v(&last_cap) == CAP_CRITICAL);
    t.close();
}

/// The load line from fake /proc and battery files: two samples 30 s apart on an 8-CPU host.
#[test]
fn load() {
    let b = Base::new("load");
    let stat_files = |user: u64, system: u64, idle: u64, irq: u64, swpin: u64, swpout: u64, me: u64| {
        let mut text = format!("cpu  {user} 0 {system} {idle} 0 {irq} 0 0 0 0\n");
        for i in 0..8 {
            text += &format!("cpu{i} 1 2 3 4 5 6 7 0 0 0\n");
        }
        text += "intr 1 2 3\nctxt 99\n";
        b.put("/proc/stat", &text);
        b.put("/proc/vmstat", &format!("nr_free_pages 10\npswpin {swpin}\npswpout {swpout}\npgfault 7\n"));
        // comm with a space and a parenthesis: the fields are counted after its last ')'
        b.put(
            "/proc/self/stat",
            &format!(
                "4242 (xnu host) S 1 4242 4242 0 -1 4194560 100 0 0 0 {} {} 0 0 20 0 30 0 1 2 3\n",
                me / 2,
                me - me / 2
            ),
        );
    };
    // 30 s at 100 ticks a second on 8 CPUs: 24000 ticks. sys 6780 (226%), user 5010 (167%), irq 360 (12%), the rest
    // idle.
    stat_files(1000, 2000, 50000, 100, 500, 400, 700);
    b.put("/sys/class/power_supply/battery/current_now", "1600\n");
    b.put("/sys/class/power_supply/battery/status", "Charging\n");
    let mut a = HostLoad::read(&b.0);
    assert!(a.have_cpu && a.ncpu == 8 && a.user == 1000 && a.system == 2000 && a.idle == 50000 && a.irq == 100);
    assert!(a.have_swap && a.pswpin == 500 && a.pswpout == 400 && a.have_self && a.self_ticks == 700);
    assert!(a.have_current && a.current == 1600 && a.status == "Charging");
    stat_files(1000 + 5010, 2000 + 6780, 50000 + 11850, 100 + 360, 500 + 70350, 400 + 59400, 700 + 4560);
    let mut later = HostLoad::read(&b.0);
    a.ms = 1000;
    later.ms = 31000;
    let raw = Raw { soc_zone: "cpu-1-2-1".into(), ..raw_with(410, 780, 403) };
    // swap: 70350 pages of 4 KiB in 30 s = 9380 KiB/s, 59400 = 7920 KiB/s; xnuhost 4560 of 3000 ticks of one CPU = 152%
    assert_eq!(
        HostLoad::line(&a, &later, Some(&raw), 4096),
        "30 s: host CPU 405% of 800 (sys 226%, user 167%, irq 12%), swap in 9380 out 7920 KiB/s, xnuhost 152%; \
         battery 40.3 C, Charging 1600 mA; case 41.0 C, SoC 78.0 C (cpu-1-2-1)"
    );
    // Missing files: each part says "-", nothing breaks.
    let none = HostLoad { ms: 31000, ..HostLoad::default() };
    assert_eq!(
        HostLoad::line(&a, &none, None, 4096),
        "30 s: host CPU -, swap -, xnuhost -; battery -, -; case -, SoC - (-)"
    );
    assert_eq!(deci_text(-5), "-0.5 C");

    // The worker writes the line at its period.
    let _hw = crate::hw::settings_lock();
    b.put("/sys/class/power_supply/battery/lrp", "350\n");
    let log = Capture::start();
    let t =
        Thermal::open(&Options { root: Some(b.0.clone()), poll_ms: 20, load_ms: 50, ..Options::default() }).unwrap();
    ms(250);
    t.close();
    let last = log.lines().into_iter().map(|(_, l)| l).rfind(|l| l.starts_with("thermal: ") && l.contains("host CPU"));
    let last = last.unwrap();
    assert!(last.contains("host CPU ") && last.contains("; case 35.0 C"), "{last}");
}

/// The clocks line from a fake S24 Ultra's cpufreq stats: four policies, two samples 30 s apart.
#[test]
fn clocks() {
    assert_eq!(cpu_list("0-1\n"), [0, 1]);
    assert_eq!(cpu_list("2 3 4"), [2, 3, 4]);
    assert_eq!(cpu_list("5-6,7"), [5, 6, 7]);
    assert!(cpu_list("x").is_empty());
    assert_eq!(cpu_ticks("cpu  9 9 9 9\ncpu0 10 0 20 60 10 0 0 0\ncpu2 1 1 1 1\n"), [(30, 100), (0, 0), (3, 4)]);
    let b = Base::new("clocks");
    let pol = |n: u32, cpus: &str, max: u64, time: &[(u64, u64)]| {
        let d = format!("/sys/devices/system/cpu/cpufreq/policy{n}");
        b.put(&format!("{d}/related_cpus"), cpus);
        b.put(&format!("{d}/cpuinfo_max_freq"), &format!("{max}\n"));
        let text: String = time.iter().map(|(k, t)| format!("{k} {t}\n")).collect();
        b.put(&format!("{d}/stats/time_in_state"), &text);
    };
    let stat = |busy: u64| {
        let mut text = String::from("cpu  1 1 1 1\n");
        for i in 0..8 {
            // user, nice, system, idle, iowait: busy of every 100 ticks
            text += &format!("cpu{i} {busy} 0 0 {} 0 0 0 0\n", 100 - busy);
        }
        b.put("/proc/stat", &text);
    };
    pol(0, "0-1\n", 2265600, &[(364800, 100), (1804800, 0), (2265600, 0)]);
    pol(2, "2 3 4\n", 3148800, &[(499200, 0), (3148800, 0)]);
    pol(5, "5-6\n", 2956800, &[(499200, 0), (2956800, 0)]);
    // the X4: the boost frequency listed above cpuinfo_max_freq
    pol(7, "7\n", 3302400, &[(480000, 0), (3398400, 0)]);
    stat(10);
    let mut a = Clocks::read(&b.0);
    assert_eq!(a.policies.len(), 4);
    assert!(a.policies[3].policy == 7 && a.policies[3].top_khz == 3398400 && a.policies[1].cpus == [2, 3, 4]);
    // 30 s (3000 units of 10 ms): policy0 a third at each clock; policy2 all at the top; policy5 all at the bottom;
    // policy7 nothing counted. The CPUs 10 -> 50 busy of 100 -> 200 ticks: 40%.
    pol(0, "0-1\n", 2265600, &[(364800, 1100), (1804800, 1000), (2265600, 1000)]);
    pol(2, "2 3 4\n", 3148800, &[(499200, 0), (3148800, 3000)]);
    pol(5, "5-6\n", 2956800, &[(499200, 3000), (2956800, 0)]);
    stat(50);
    let mut later = Clocks::read(&b.0);
    for c in &mut later.cpu {
        c.1 += 100; // the second sample's ticks: 200 in all
    }
    (a.ms, later.ms) = (1000, 31000);
    assert_eq!(
        Clocks::line(&a, &later).unwrap(),
        "30 s clocks: policy0 (cpu0-1) busy 40%, mean 1478 of 2265 MHz, at the top 33%, under half 33%; \
         policy2 (cpu2-4) busy 40%, mean 3148 of 3148 MHz, at the top 100%, under half 0%; \
         policy5 (cpu5-6) busy 40%, mean 499 of 2956 MHz, at the top 0%, under half 100%; \
         policy7 (cpu7) busy 40%, no time counted"
    );
    assert_eq!(Clocks::line(&Clocks::default(), &later), None);
}

/// The treble backend's reading: the thermal HAL's Temperature[] made a reading (the parcel round trips are hal/'s
/// tests).
#[test]
fn from_hal_temperatures() {
    let t = |kind: i32, name: &str, celsius: f32, throttling: i32| Temperature {
        kind,
        name: name.into(),
        celsius,
        throttling,
    };
    let input = [
        t(hal::TEMP_CPU, "cpu-0-0", 51.24, 0),
        t(hal::TEMP_CPU, "cpu-1-3", 63.06, 0),
        t(hal::TEMP_GPU, "gpu", 58.0, 0),
        t(hal::TEMP_SOC, "soc", 66.5, 1),
        t(hal::TEMP_BATTERY, "battery", 31.0, 0),
        t(hal::TEMP_SKIN, "skin", 38.96, 0),
        t(hal::TEMP_SKIN, "skin-2", 36.0, 0),
        t(4, "usb", 30.0, 0),
        t(hal::TEMP_CPU, "cpu-off", -273.0, 0),
        t(hal::TEMP_CPU, "cpu-nan", f32::NAN, 0),
    ];
    let raw = from_hal(&input);
    assert!(raw.cpu == 631 && raw.gpu == 580 && raw.soc == 665 && raw.soc_zone == "soc" && raw.battery == 310);
    assert_eq!(raw.skin, 390);
    assert!(raw.ddr == UNKNOWN && raw.xo == UNKNOWN && raw.ap == UNKNOWN && raw.lrp == UNKNOWN);
    // The policy is the same: a case at 39.0 C is LIGHT on the default limits, as lrp at 39.0 C is.
    let l = Limits::default();
    let st = decide(&l, &raw, None);
    let samsung = Raw { lrp: 390, ..raw.clone() };
    let st2 = decide(&l, &samsung, None);
    assert!(st.level == st2.level && st.skin_level == st2.skin_level);
    let none = from_hal(&[]);
    assert!(none.soc == UNKNOWN && none.skin == UNKNOWN && none.battery == UNKNOWN);
}

/// The channel's constants in the guest's header: the unknown reading, the reasons, the levels' scale.
#[test]
fn guest_header() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../drivers/bridge/oneios_bridge_proto.h");
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("#define OB_THERMAL_UNKNOWN INT32_MIN") && UNKNOWN == i32::MIN);
    assert!(text.contains("#define OB_BATTERY_UNKNOWN INT32_MIN") && crate::battery::UNKNOWN == i32::MIN);
    assert!(text.contains(&format!(
        "enum {{ OB_THERMAL_BY_CASE = {BY_SKIN}u, OB_THERMAL_BY_SOC = {BY_SOC}u, OB_THERMAL_BY_BATTERY = {BY_BATTERY}u }};"
    )));
    assert!(text.contains("0 nominal, 10 light") && text.contains("20 moderate, 30 heavy, 40 trapping, 50 sleeping"));
    assert!(
        text.contains("int32_t  minutes;                      // to empty, or to full while charging; -1 = unknown")
    );
    assert_eq!(crate::battery::MINUTES_UNKNOWN, -1);
}
