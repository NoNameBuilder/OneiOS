// SPDX-License-Identifier: MIT
//! bridgectl (C: tools/bridgectl.c): tries each host bridge on the phone, without xnuhost or a guest. Run as root (su):
//!   bridgectl host                   the host phone: the properties read, the table entry they match and why, the table
//!                                    (read only; ONEIOS_HOST_PROPS=FILE: a getprop dump of another phone)
//!   bridgectl cpu [NVCPU]            the host's cores and the vCPU plan made from them
//!   bridgectl haptic-inspect              O_RDONLY evdev queries only: name, sine/periodic/gain bits, max effects
//!   bridgectl haptic-hal-inspect          read-only AIDL getCapabilities/version: never on, off or amplitude
//!   bridgectl haptic-hal-primitives       read-only AIDL composition/effect support and primitive durations
//!   bridgectl haptic-native-inspect       passive, opt-in production Treble impact preflight; never actuate
//!   bridgectl haptic-hal-click            ACTIVE one-shot IVibrator.perform(CLICK, MEDIUM); explicit opt-in required
//!   bridgectl haptic-hal-heavy-click      ACTIVE one-shot IVibrator.perform(HEAVY_CLICK, STRONG); explicit opt-in required
//!   bridgectl haptic [MS [INTENSITY [HZ]]]   one vibration (default 200 ms, 1000, resonance)
//!   bridgectl haptic-pattern         three taps and a ramp: what an iOS pattern feels like
//! Every command first says the host phone (a "host: ..." line; ONEIOS_HOST_DEVICE=auto|none|NAME, hw/host_device.rs).
//! The backend: ONEIOS_HW_BACKEND, ONEIOS_HW_BACKEND_<BRIDGE>, ONEIOS_HW_FALLBACK (hw/hw_backend.rs): haptic's is the
//! evdev device (samsung) or the vibrator HAL (treble). Off the phone: ONEIOS_INPUT_DIR, ONEIOS_BINDER.
//!   bridgectl bt                     opens the Bluetooth HAL, sends HCI Reset and Read BD_ADDR, prints the events
//!   bridgectl net                    opens the tap, answers the first DHCP request it sees, counts frames for 10 s
//!   bridgectl audio HELPER [S [record]]  a quiet 440 Hz tone for S s (2) through the helper (path of oneios-audio),
//!                                    then 0.5 s later a 660 Hz one for 0.5 s; with record, then 2 s of the microphone
//!                                    (frames, peak, RMS); first the helper's probe (the speaker's latency xnuhost would
//!                                    publish to iOS)
//!   bridgectl audio-loop HELPER [N]  N clicks (10), 0.5 s apart, each 20 ms of 1 kHz at -30 dBFS, while the microphone
//!                                    records: the round trip of each (handed to the helper to heard by the microphone:
//!                                    the speaker's latency, the air and the microphone's), and their median
//! Each prints what it found and ends with "ok" or the reason it failed.

use onebridges::audio::{AudioBackend, AudioHelper};
use onebridges::bt::{hci, hci_hidl};
use onebridges::cpu::{CpuPlan, Topology};
use onebridges::hal::{self, aidl, Hal};
use onebridges::haptics::Haptic;
use onebridges::hw::{self, Backend, HwSettings};
use onebridges::net::{Net, NetConfig};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xh_core::evdev::{
    self,
    codes::{EV_FF, FF_CUSTOM, FF_GAIN, FF_MAX, FF_PERIODIC, FF_SINE},
};
use xh_core::parse::atoi;
use xh_core::sys::{error_text, strerror};

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// `bridgectl host`: the host phone as xnuhost sees it, read only: what was read, which entry it matched and why, and
/// the table (each phone's match strings and defaults).
fn host_cmd() -> i32 {
    let var = |n: &str| std::env::var(n).unwrap_or_default();
    let (e, f, why) =
        match hw::host_choose(&var("ONEIOS_HOST_DEVICE"), &var("ONEIOS_HOST_PROPS"), &var("ONEIOS_HOST_ROOT")) {
            Ok(chosen) => chosen,
            Err(why) => {
                println!("host: {why}");
                return 1;
            }
        };
    println!("read: {}", hw::host_describe(&f));
    println!("host: {} ({why})", e.map_or("none", |e| e.name));
    for t in hw::host_table() {
        let mut line = format!(
            "{} {:<17} {}; model",
            if e.is_some_and(|e| std::ptr::eq(e, t)) { '*' } else { ' ' },
            t.name,
            t.label
        );
        t.models.iter().for_each(|m| line += &format!(" {m}*"));
        for (label, list) in [("device", t.devices), ("soc", t.socs), ("platform", t.platforms)] {
            if !list.is_empty() {
                line += &format!(", {label} {}", list.join(" "));
            }
        }
        line += "; defaults:";
        let defaults: Vec<&str> = t.config.iter().chain(t.env).copied().collect();
        if defaults.is_empty() {
            line += " none";
        }
        defaults.iter().for_each(|d| line += &format!(" {d}"));
        println!("{line}");
    }
    0
}

fn cpu(args: &[String]) -> i32 {
    let nvcpu = if args.len() > 2 { atoi(&args[2]) as usize } else { 8 };
    let Some(t) = Topology::read("/sys") else {
        println!("no CPUs under /sys/devices/system/cpu");
        return 1;
    };
    println!("host: {} CPUs, {} P, {} E", t.ncpu(), t.n_p, t.n_e);
    for (i, c) in t.cpu.iter().enumerate().filter(|(_, c)| c.present) {
        println!(
            "  cpu{i}: {} capacity {} part 0x{:03x} policy {} {}..{} kHz ({} steps) L1d {} L2 {} L3 {}",
            c.kind.letter(),
            c.capacity,
            c.part,
            c.policy,
            c.min_khz,
            c.max_khz,
            c.freq_khz.len(),
            c.l1d_bytes,
            c.l2_bytes,
            c.l3_bytes
        );
    }
    let Some(plan) = CpuPlan::make(&t, nvcpu, false) else {
        println!("no plan for {nvcpu} vCPUs");
        return 1;
    };
    println!("plan: {} vCPUs, {} E then {} P", plan.nvcpu(), plan.n_e, plan.n_p);
    for (i, v) in plan.v.iter().enumerate() {
        println!(
            "  vcpu{i}: {} cluster {} host mask 0x{:x} capacity {} max {} kHz",
            v.kind.letter(),
            v.cluster_id,
            v.affinity(),
            v.capacity,
            v.max_khz
        );
    }
    let last = plan.nvcpu() - 1;
    let done = |r: std::io::Result<()>| r.map_or_else(|e| error_text(&e), |()| "done".to_owned());
    println!("placing this thread as vcpu{last}: {}", done(plan.place(0, last)));
    let top = plan.v[last].max_khz;
    let rc = plan.request(0, last, top);
    println!("uclamp.min {} for the top frequency: {}", plan.util_for(last, top), done(rc));
    0
}

// ---- haptics: the evdev node's bits, read only ---------------------------------------------------

/// Strictly passive probe. Unlike Haptic::open_as, this does NOT send FF_GAIN, upload a waveform, create a worker or
/// play an effect.
fn haptic_inspect() -> i32 {
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            println!("haptic-inspect: cannot open /dev/input: {}", error_text(&e));
            return 1;
        }
    };
    let mut found = 0;
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let path = format!("/dev/input/{}", onebridges::log::truncate(&name, 160));
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(file) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path) else {
            continue;
        };
        let fd = file.as_raw_fd();
        let mut bits = [0u8; FF_MAX / 8 + 1];
        let ok = evdev::bits(fd, EV_FF, &mut bits);
        let bit = |n: u16| ok && evdev::bit(&bits, n.into());
        if bit(FF_PERIODIC) && bit(FF_SINE) {
            found += 1;
            let dev = evdev::name(fd, 128).unwrap_or_default();
            let (effects, effects_ok) = match evdev::effects(fd) {
                Some(n) => (n, true),
                None => (-1, false),
            };
            println!(
                "haptic-inspect: {path} \"{}\" FF_PERIODIC=1 FF_SINE=1 FF_CUSTOM={} FF_GAIN={} max_effects={effects}{}",
                dev,
                u8::from(bit(FF_CUSTOM)),
                u8::from(bit(FF_GAIN)),
                if !effects_ok { " (query unavailable)" } else { "" }
            );
        }
    }
    if found == 0 {
        println!("haptic-inspect: no readable FF_PERIODIC + FF_SINE device");
        return 1;
    }
    println!("haptic-inspect: ok (O_RDONLY; no FF_GAIN, EVIOCSFF, or playback)");
    0
}

// ---- haptics: the vibrator HAL, read only ----------------------------------------------------------
fn vib_call(h: &Hal, code: u32) -> Result<i32, i32> {
    h.call(code, &hal::parcel(hal::VIBRATOR), aidl::take_i32)
}

/// An int[] reply (at most 32).
fn take_list(r: &mut hal::Reader<'_>) -> Result<Vec<i32>, i32> {
    let n = r.i32();
    if r.is_bad() || !(0..=32).contains(&n) {
        return Err(-libc::EPROTO);
    }
    let list: Vec<i32> = (0..n).map(|_| r.i32()).collect();
    if r.is_bad() {
        Err(-libc::EPROTO)
    } else {
        Ok(list)
    }
}

fn open_vibrator(cmd: &str) -> Option<Hal> {
    match Hal::open(None, hal::VIBRATOR_INSTANCE, hal::VIBRATOR) {
        Ok(h) => Some(h),
        Err(why) => {
            println!("{cmd}: no usable {}: {why}", hal::VIBRATOR_INSTANCE);
            None
        }
    }
}

/// The active Samsung evdev FF_SINE upload fails with EINVAL on SM8850, while One UI can play FF_CUSTOM. Before
/// considering HAL playback, verify whether the stock AIDL vibrator is accessible with *read-only* calls. Opening and
/// closing a HAL handle here does not call IVibrator.off()/on().
fn haptic_hal_inspect() -> i32 {
    let Some(h) = open_vibrator("haptic-hal-inspect") else { return 1 };
    let version = h.version();
    let caps = vib_call(&h, hal::VIB_GET_CAPABILITIES);
    drop(h);
    let caps = match caps {
        Ok(c) => c,
        Err(rc) => {
            println!("haptic-hal-inspect: getCapabilities failed: {}", hal::error_text(rc));
            return 1;
        }
    };
    println!(
        "haptic-hal-inspect: {} version {version} capabilities 0x{:x} (amplitude={})",
        hal::VIBRATOR_INSTANCE,
        caps as u32,
        if caps & hal::VIB_CAP_AMPLITUDE != 0 { "yes" } else { "no" }
    );
    println!("haptic-hal-inspect: ok (getCapabilities only; no on, off, amplitude or FF events)");
    0
}

fn primitive_name(id: i32) -> &'static str {
    match id {
        0 => "NOOP",
        1 => "CLICK",
        2 => "THUD",
        3 => "SPIN",
        4 => "QUICK_RISE",
        5 => "SLOW_RISE",
        6 => "QUICK_FALL",
        7 => "LIGHT_TICK",
        8 => "LOW_TICK",
        _ => "unknown",
    }
}

/// Only AIDL getters. No IVibrator.on/off/compose/perform/setAmplitude, no sysfs writes, no opening the evdev node for
/// playback. This query establishes vendor-supported primitive IDs before any physical trial.
fn haptic_hal_primitives() -> i32 {
    let Some(h) = open_vibrator("haptic-hal-primitives") else { return 1 };
    let caps = match vib_call(&h, hal::VIB_GET_CAPABILITIES) {
        Ok(c) => c,
        Err(rc) => {
            println!("haptic-hal-primitives: getCapabilities failed: {}", hal::error_text(rc));
            return 1;
        }
    };
    println!(
        "haptic-hal-primitives: AIDL version {} capabilities 0x{:x} compose={}",
        h.version(),
        caps as u32,
        if caps & hal::VIB_CAP_COMPOSE != 0 { "yes" } else { "no" }
    );
    if caps & hal::VIB_CAP_COMPOSE == 0 {
        println!("haptic-hal-primitives: composition not advertised; no primitive queries");
        return 0;
    }
    let limits =
        vib_call(&h, hal::VIB_COMPOSE_DELAY_MAX).and_then(|d| Ok((d, vib_call(&h, hal::VIB_COMPOSE_SIZE_MAX)?)));
    let (max_delay, max_size) = match limits {
        Ok(l) => l,
        Err(rc) => {
            println!("haptic-hal-primitives: composition limits query failed: {}", hal::error_text(rc));
            return 1;
        }
    };
    println!("haptic-hal-primitives: max delay {max_delay} ms, max composition entries {max_size}");
    let primitives = match h.call(hal::VIB_SUPPORTED_PRIMITIVES, &hal::parcel(hal::VIBRATOR), take_list) {
        Ok(p) => p,
        Err(rc) => {
            println!("haptic-hal-primitives: getSupportedPrimitives failed: {}", hal::error_text(rc));
            return 1;
        }
    };
    println!("haptic-hal-primitives: {} supported primitive(s)", primitives.len());
    for &id in &primitives {
        let mut p = hal::parcel(hal::VIBRATOR);
        p.put_i32(id);
        match h.call(hal::VIB_PRIMITIVE_DURATION, &p, aidl::take_i32) {
            Ok(d) => println!("  primitive {id} {:<12} duration {d} ms", primitive_name(id)),
            Err(rc) => {
                println!("  primitive {id} {:<12} duration unavailable: {}", primitive_name(id), hal::error_text(rc))
            }
        }
    }
    match h.call(hal::VIB_SUPPORTED_EFFECTS, &hal::parcel(hal::VIBRATOR), take_list) {
        Err(rc) => println!("haptic-hal-primitives: getSupportedEffects unavailable: {}", hal::error_text(rc)),
        Ok(effects) => {
            let list: String = effects.iter().map(|e| format!(" {e}")).collect();
            println!("haptic-hal-primitives: {} supported preset effect(s):{list}", effects.len());
        }
    }
    drop(h);
    println!("haptic-hal-primitives: ok (read-only AIDL getters; no on, off, compose, perform, amplitude or FF)");
    0
}

/// Passive physical integration probe for the REAL production haptic bridge. The native opt-in causes HAL
/// getCapabilities/getSupportedEffects only, and the bridge is closed unused. No off()/perform()/on()/setAmplitude(),
/// even when the required preset set is unavailable. Missing HAL is fail-closed.
fn haptic_native_inspect(args: &[String]) -> i32 {
    if args.len() != 2 {
        return 2;
    }
    if std::env::var("ONEIOS_HAPTIC_NATIVE_IMPACTS").map_or(true, |v| v != "1") {
        println!(
            "haptic-native-inspect: blocked; set ONEIOS_HAPTIC_NATIVE_IMPACTS=1 to permit read-only native preflight"
        );
        return 1;
    }
    if hw::fallback_allowed() {
        println!("haptic-native-inspect: blocked; disable ONEIOS_HW_FALLBACK to prevent evdev fallback");
        return 1;
    }
    let Some(h) = Haptic::open_as(None, Backend::Treble, None) else {
        println!("haptic-native-inspect: Treble bridge unavailable; no evdev fallback");
        return 1;
    };
    let supported = h.impacts_available();
    drop(h); // no on/off/gain/perform when native was requested
    if !supported {
        println!("haptic-native-inspect: native preset preflight FAILED (no actuation)");
        return 1;
    }
    println!("haptic-native-inspect: ready (native impact preflight PASS; no on, off, gain, perform or evdev)");
    0
}

/// Bounded, opt-in ACTIVE test of a vendor-advertised AOSP preset. Exactly one AIDL perform() request: no FF fallback,
/// manual waveform, amplitude/gain setting, on/off, loop, external control or compose. Do not use while the Android
/// framework is already vibrating.
fn haptic_hal_preset(args: &[String], heavy: bool) -> i32 {
    let cmd = if heavy { "haptic-hal-heavy-click" } else { "haptic-hal-click" };
    let (effect_name, strength_name) = if heavy { ("HEAVY_CLICK", "STRONG") } else { ("CLICK", "MEDIUM") };
    let (effect, strength) = if heavy {
        (hal::VIB_EFFECT_HEAVY_CLICK, hal::VIB_STRENGTH_STRONG)
    } else {
        (hal::VIB_EFFECT_CLICK, hal::VIB_STRENGTH_MEDIUM)
    };
    if args.len() != 2 {
        println!("usage: ONEIOS_HAPTIC_ACTIVE_TEST=1 bridgectl {cmd}");
        return 2;
    }
    if std::env::var("ONEIOS_HAPTIC_ACTIVE_TEST").map_or(true, |v| v != "1") {
        println!("{cmd}: blocked; set ONEIOS_HAPTIC_ACTIVE_TEST=1 to authorize one active {effect_name}");
        return 1;
    }
    let Some(h) = open_vibrator(cmd) else { return 1 };
    let effects = match h.call(hal::VIB_SUPPORTED_EFFECTS, &hal::parcel(hal::VIBRATOR), take_list) {
        Ok(e) => e,
        Err(rc) => {
            println!("{cmd}: getSupportedEffects failed: {}", hal::error_text(rc));
            return 1;
        }
    };
    if !effects.contains(&effect) {
        println!("{cmd}: {effect_name} not in supported effects; no actuation attempted");
        return 1;
    }
    let mut p = hal::parcel(hal::VIBRATOR);
    p.put_i32(effect);
    p.put_i32(strength);
    p.put_null_binder(); // S26 CAP_PERFORM_CALLBACK not advertised
    let duration_ms = match h.call(hal::VIB_PERFORM, &p, aidl::take_i32) {
        Ok(d) => d,
        Err(rc) => {
            println!("{cmd}: perform({effect_name}, {strength_name}) rejected: {}", hal::error_text(rc));
            return 1;
        }
    };
    println!("{cmd}: HAL accepted one {effect_name} {strength_name} (reported duration {duration_ms} ms)");
    if duration_ms <= 0 || duration_ms > 1000 {
        println!("{cmd}: HAL returned an unexpected duration; no follow-up effects");
        return 1;
    }
    // Let the HAL finish its own effect; off() could cancel unrelated Android vibration starting after our one-shot
    // preset.
    sleep_ms(duration_ms as u64 + 80);
    drop(h);
    println!("{cmd}: done (one perform; no off, gain, compose or fallback; tactile verification required)");
    0
}

fn haptic(args: &[String], pattern: bool) -> i32 {
    let dir = std::env::var("ONEIOS_INPUT_DIR").ok();
    let Some(h) = Haptic::open_as(dir.as_deref(), Backend::Unset, None) else { return 1 }; // the backend: hw-backend-haptics
    if pattern {
        for _ in 0..3 {
            h.vib(15, 1000, 0);
            sleep_ms(150);
        }
        for level in (100..=1000).step_by(100) {
            h.vib(200, level, 0);
            sleep_ms(60);
        }
        sleep_ms(300);
    } else {
        let arg = |i: usize, default: u32| if args.len() > i { atoi(&args[i]) as u32 } else { default };
        let ms = arg(2, 200);
        h.vib(ms, arg(3, 1000), arg(4, 0));
        sleep_ms(u64::from(ms) + 100);
    }
    // The worker is asynchronous: an earlier "ok" only meant the CLI completed, even if the driver rejected every
    // effect (EINVAL).
    h.flush();
    let stats = h.stats();
    let failed = stats.failed > 0 || stats.played == 0;
    if failed {
        println!("bridgectl: haptic failed (effects played={}, failed={})", stats.played, stats.failed);
    }
    drop(h);
    i32::from(failed)
}

fn bt() -> i32 {
    const RESET: [u8; 3] = [0x03, 0x0c, 0x00];
    const READ_ADDRESS: [u8; 3] = [0x09, 0x10, 0x00];
    if let Some(a) = hci::bt_address("/mnt/vendor/efs/bluetooth/bt_addr") {
        println!("address file: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", a[0], a[1], a[2], a[3], a[4], a[5]);
    }
    let hal = hci_hidl::probe();
    if hal < 0 {
        println!("no Bluetooth HAL found ({hal})");
        return 1;
    }
    println!(
        "HAL {} registered",
        match hal {
            11 => "1.1",
            hci_hidl::PROBE_AIDL => "AIDL (android.hardware.bluetooth.IBluetoothHci/default)",
            _ => "1.0",
        }
    );
    let state = Arc::new(AtomicI32::new(-1));
    let events = Arc::new(AtomicU32::new(0));
    let rx: hci::RxFn = {
        let events = Arc::clone(&events);
        Box::new(move |kind, packet: &[u8]| {
            let bytes: String = packet.iter().take(24).map(|b| format!(" {b:02x}")).collect();
            println!("  <- type {kind}, {} bytes:{bytes}", packet.len());
            events.fetch_add(1, Ordering::Relaxed);
        })
    };
    let changed: hci::StateFn = {
        let state = Arc::clone(&state);
        Box::new(move |s| {
            state.store(s as i32, Ordering::Relaxed);
            println!("  state {}", s as i32);
        })
    };
    let Some(h) = hci_hidl::open_hidl(rx, changed) else { return 1 };
    if h.open().is_err() {
        return 1;
    }
    // the firmware download takes seconds: the state callback says when it is done
    for _ in 0..200 {
        if state.load(Ordering::Relaxed) >= 1 {
            break;
        }
        sleep_ms(100);
    }
    if state.load(Ordering::Relaxed) != 1 {
        println!("the HAL did not become ready (state {})", state.load(Ordering::Relaxed));
        return 1;
    }
    let mut rc = h.send(1, &RESET);
    sleep_ms(500);
    if rc.is_ok() {
        rc = h.send(1, &READ_ADDRESS);
    }
    sleep_ms(500);
    h.close();
    sleep_ms(200);
    let n = events.load(Ordering::Relaxed);
    if rc.is_err() || n < 2 {
        println!("send {}, {n} events (2 expected)", rc.err().unwrap_or(0));
        return 1;
    }
    println!("bt: ok: {n} events (Command Complete of HCI Reset and Read BD_ADDR expected)");
    0
}

fn net() -> i32 {
    let c = NetConfig::default();
    let frames = Arc::new(AtomicU32::new(0));
    let count = Arc::clone(&frames);
    let Some(n) = Net::open_tap(
        &c,
        Arc::new(move |_: &[u8]| {
            count.fetch_add(1, Ordering::Relaxed);
            true
        }),
    ) else {
        return 1;
    };
    println!(
        "tap {} is open; run `oneios-net.sh up` in another shell, then `ping -I {} 192.168.77.2` shows ARP requests here",
        c.ifname, c.ifname
    );
    sleep_ms(10_000);
    println!("{} frames from the tap in 10 s", frames.load(Ordering::Relaxed));
    drop(n);
    0
}

// ---- audio ------------------------------------------------------------------------------------------
/// 5 ms periods on the clock, as the guest's engine paces them.
struct Pacer(f64);
impl Pacer {
    fn new() -> Pacer {
        Pacer(xh_core::time::monotonic_s())
    }
    fn next(&mut self) {
        self.0 += 0.005;
        let t = libc::timespec {
            tv_sec: self.0 as libc::time_t,
            tv_nsec: ((self.0 - self.0.floor()) * 1e9) as libc::c_long,
        };
        // SAFETY: a valid timespec; an absolute sleep.
        unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &t, std::ptr::null_mut()) };
    }
}

/// One stereo period of a tone at -30 dBFS (1000 of 32768) from `phase`, which moves on.
fn tone(period: &mut [i16; 480], phase: &mut f64, hz: f64) {
    for i in 0..240 {
        let v = (1000.0 * phase.sin()) as i16;
        period[2 * i] = v;
        period[2 * i + 1] = v;
        *phase += 2.0 * std::f64::consts::PI * hz / 48000.0;
    }
}

fn db(v: f64) -> f64 {
    if v > 0.0 {
        20.0 * (v / 32768.0).log10()
    } else {
        -999.0
    }
}

/// A quiet tone (-30 dBFS: never loud, whatever the media volume) paced by the clock as the guest's engine paces its
/// periods; then, with `record`, two seconds of the microphone: frames, peak and RMS. The helper's own lines (its
/// stream's mode, the ring's report) are on stderr.
fn audio(args: &[String]) -> i32 {
    let mut seconds = if args.len() > 3 { atoi(&args[3]) as u32 } else { 2 };
    let record = args.get(4).is_some_and(|a| a == "record");
    if !(1..=30).contains(&seconds) {
        seconds = 2;
    }
    let Some(b) = AudioHelper::spawn(&args[2]) else { return 1 };
    match b.probe_latency(2000) {
        Ok(p) => println!(
            "audio: probe: speaker latency {p} frames ({:.1} ms): oneios-audio-latency={p} in a session",
            f64::from(p) / 48.0
        ),
        Err(e) => println!("audio: probe: not measured ({})", strerror(e)),
    }
    if b.start(true, 48000, 2).is_err() {
        return 1;
    }
    let mut period = [0i16; 480];
    let mut phase = 0f64;
    let mut pace = Pacer::new();
    let mut sent = 0;
    for _ in 0..seconds * 200 {
        tone(&mut period, &mut phase, 440.0);
        if b.write(&period) == 240 {
            sent += 1;
        }
        pace.next();
    }
    println!("audio: {sent} of {} periods of a 440 Hz tone at -30 dBFS sent", seconds * 200);
    let _ = b.stop(true);
    // A second, short tone 0.5 s later, as iOS stops and starts its device around each sound: the helper's stream
    // lingers, so this one starts without the track's stall (its report says so).
    sleep_ms(500);
    if b.start(true, 48000, 2).is_err() {
        return 1;
    }
    let mut pace = Pacer::new();
    for _ in 0..100 {
        tone(&mut period, &mut phase, 660.0);
        let _ = b.write(&period);
        pace.next();
    }
    let _ = b.stop(true);
    let mut rc = i32::from(sent == 0);
    if record {
        if b.start(false, 48000, 1).is_err() {
            return 1;
        }
        let mut pcm = [0i16; 240];
        let (mut frames, mut counted, mut sum, mut peak) = (0u64, 0u64, 0f64, 0i32);
        let t0 = Instant::now();
        loop {
            let n = b.read(&mut pcm);
            if n < 0 {
                break;
            }
            // The first 250 ms are the input's start (a full-scale click on the phone): not counted in the peak and RMS.
            for (i, &s) in pcm.iter().take(n as usize).enumerate() {
                if frames + i as u64 >= 12000 {
                    peak = peak.max(i32::from(s).abs());
                    sum += f64::from(s) * f64::from(s);
                    counted += 1;
                }
            }
            frames += n as u64;
            if t0.elapsed() >= Duration::from_secs(2) {
                break;
            }
        }
        let _ = b.stop(false);
        let rms = if counted != 0 { (sum / counted as f64).sqrt() } else { 0.0 };
        println!(
            "audio: microphone: {frames} frames in 2 s (96000 expected), peak {peak} ({:.1} dBFS), RMS {:.1} dBFS{}",
            if peak != 0 { db(f64::from(peak)) } else { -999.0 },
            db(rms),
            if frames != 0 && peak == 0 { " (all zero: silenced?)" } else { "" }
        );
        if frames == 0 {
            rc = 1;
        }
    }
    sleep_ms(300); // the helper's last report
    drop(b);
    sleep_ms(100);
    rc
}

/// audio-loop: the microphone's frames, each with the time it came in here (a reader thread), against the clicks, each
/// with the time its first period was handed to the helper. A frame's time is its period's arrival less the frames
/// after it in that period: it includes the microphone's own path (capture, AAudio, the helper, the socket), so a round
/// trip is the speaker's latency plus the air plus the microphone's latency. The helper's speaker report (stderr) says
/// the speaker's part from AAudio's timestamps; the difference is the microphone's and the air's.
fn audio_loop(args: &[String]) -> i32 {
    let mut clicks = if args.len() > 3 { atoi(&args[3]) as usize } else { 10 };
    if !(1..=20).contains(&clicks) {
        clicks = 10;
    }
    let Some(b) = AudioHelper::spawn(&args[2]) else { return 1 };
    let b = Arc::new(b);
    let cap = 48000 * 14;
    let mic: Arc<Mutex<(Vec<i16>, Vec<f64>)>> = Arc::default();
    let run = Arc::new(AtomicBool::new(true));
    let started = b.start(false, 48000, 1).is_ok() && b.start(true, 48000, 2).is_ok();
    let reader = started.then(|| {
        let (b, mic, run) = (Arc::clone(&b), Arc::clone(&mic), Arc::clone(&run));
        std::thread::spawn(move || {
            let mut pcm = [0i16; 240];
            while run.load(Ordering::Acquire) && mic.lock().unwrap().0.len() + 240 <= cap {
                let n = b.read(&mut pcm);
                if n < 0 {
                    break;
                }
                if n == 0 {
                    continue;
                }
                let now = xh_core::time::monotonic_s();
                let mut m = mic.lock().unwrap();
                m.0.extend_from_slice(&pcm[..n as usize]);
                (0..n).for_each(|i| m.1.push(now - f64::from(n - 1 - i) / 48000.0));
            }
        })
    });
    let Some(reader) = reader else {
        println!("audio-loop: the speaker or the microphone did not start");
        return 1;
    };
    // 1 s of silence (the microphone's start click, the noise floor), then a click every 0.5 s: 4 periods of 1 kHz.
    let mut period = [0i16; 480];
    let mut sent = [0f64; 20];
    let mut pace = Pacer::new();
    let periods = 200 + clicks * 100 + 60;
    let mut k = 0;
    for p in 0..periods {
        let on = p >= 200 && (p - 200) % 100 < 4 && (p - 200) / 100 < clicks;
        for i in 0..240 {
            let v =
                if on { (1000.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48000.0).sin()) as i16 } else { 0 };
            period[2 * i] = v;
            period[2 * i + 1] = v;
        }
        if on && (p - 200) % 100 == 0 {
            sent[k] = xh_core::time::monotonic_s();
            k += 1;
        }
        let _ = b.write(&period);
        pace.next();
    }
    let _ = b.stop(true);
    sleep_ms(200);
    run.store(false, Ordering::Release);
    let _ = reader.join();
    let _ = b.stop(false);
    let (pcm, at) = std::mem::take(&mut *mic.lock().unwrap());
    let rms = |f: usize| ((f..f + 48).map(|i| f64::from(pcm[i]) * f64::from(pcm[i])).sum::<f64>() / 48.0).sqrt();
    // The noise: RMS of 1 ms windows from 0.4 s in to the first click; the threshold 8 times their median (18 dB), at
    // least -56 dBFS.
    let mut windows: Vec<f64> = (0..pcm.len().saturating_sub(47))
        .step_by(48)
        .filter(|&f| at[f] >= sent[0] - 0.6 && at[f + 47] < sent[0])
        .take(1000)
        .map(rms)
        .collect();
    windows.sort_by(f64::total_cmp);
    let noise = windows.get(windows.len() / 2).copied().unwrap_or(0.0);
    let threshold = (noise * 8.0).max(50.0);
    let mut heard = Vec::new();
    for (k, &s) in sent.iter().take(clicks).enumerate() {
        let found = (0..pcm.len().saturating_sub(47))
            .step_by(8)
            .skip_while(|&f| at[f] < s)
            .take_while(|&f| at[f] <= s + 0.4)
            .find(|&f| rms(f) > threshold)
            .map(|f| (at[f] - s) * 1e3);
        match found {
            Some(ms) => {
                println!("audio-loop: click {}: heard {ms:.1} ms after it was handed to the helper", k + 1);
                heard.push(ms);
            }
            None => println!("audio-loop: click {}: not heard within 400 ms", k + 1),
        }
    }
    heard.sort_by(f64::total_cmp);
    println!(
        "audio-loop: {} frames from the microphone, noise {:.1} dBFS, threshold {:.1} dBFS",
        pcm.len(),
        db(noise),
        20.0 * (threshold / 32768.0).log10()
    );
    if let (Some(first), Some(last)) = (heard.first(), heard.last()) {
        println!(
            "audio-loop: round trip (the speaker + the air + the microphone) median {:.1} ms, {first:.1}..{last:.1}, {} of \
             {clicks} clicks",
            heard[heard.len() / 2],
            heard.len()
        );
    }
    sleep_ms(300); // the helper's last report
    drop(b);
    sleep_ms(100);
    i32::from(heard.len() * 2 < clicks)
}

fn main() {
    // As the C tool: output into a closed pipe (`bridgectl cpu | head`) ends it quietly, not with a panic.
    // SAFETY: setting a signal's disposition to its default, before any thread starts.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().collect();
    // The bridges' own lines (why a HAL was not found, the HAL's state), as xnuhost logs them.
    onebridges::log::set_sink(onebridges::log::Sink::Custom(Arc::new(|level, line: &str| {
        println!("  {}{line}", if level == onebridges::log::Level::Error { "error: " } else { "" });
    })));
    let mut settings = HwSettings::default();
    if let Err(bad) = settings.env() {
        eprintln!("bridgectl: bad {bad} (samsung or treble; ONEIOS_HW_FALLBACK 0 or 1)");
        std::process::exit(2);
    }
    // The host phone: its environment defaults (ONEIOS_BRIGHT_MAX on the S26 Ultra) and hw-backend defaults under the
    // environment's own, as xnuhost takes them; ONEIOS_HOST_DEVICE=none: today's defaults.
    match hw::host_tool(&mut settings) {
        Ok((_, line)) => onebridges::br_info!("{line}"),
        Err(line) => {
            eprintln!("bridgectl: {line}");
            std::process::exit(2);
        }
    }
    hw::hw_configure(&settings);
    if args.len() < 2 {
        eprintln!(
            "usage: bridgectl host|cpu|haptic-inspect|haptic-hal-inspect|haptic-hal-primitives|haptic-native-inspect|\
             haptic-hal-click|haptic-hal-heavy-click|haptic|haptic-pattern|bt|net|audio|audio-loop ... (see \
             tools/bridgectl.rs)"
        );
        std::process::exit(2);
    }
    let rc = match (args[1].as_str(), args.len() > 2) {
        ("host", _) => host_cmd(),
        ("cpu", _) => cpu(&args),
        ("haptic-inspect", _) => haptic_inspect(),
        ("haptic-hal-inspect", _) => haptic_hal_inspect(),
        ("haptic-hal-primitives", _) => haptic_hal_primitives(),
        ("haptic-native-inspect", _) => haptic_native_inspect(&args),
        ("haptic-hal-click", _) => haptic_hal_preset(&args, false),
        ("haptic-hal-heavy-click", _) => haptic_hal_preset(&args, true),
        ("haptic", _) => haptic(&args, false),
        ("haptic-pattern", _) => haptic(&args, true),
        ("bt", _) => bt(),
        ("net", _) => net(),
        ("audio", true) => audio(&args),
        ("audio-loop", true) => audio_loop(&args),
        _ => 2,
    };
    println!(
        "{}",
        if rc == 0 {
            "ok"
        } else if rc == 2 {
            "unknown command"
        } else {
            "failed"
        }
    );
    std::process::exit(rc);
}
