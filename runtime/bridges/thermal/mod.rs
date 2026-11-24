// SPDX-License-Identifier: MIT
//! The thermal bridge: the phone's temperatures and a thermal pressure level for iOS, and the step by which xnuhost
//! holds itself back. Reads Linux's thermal zones (/sys/class/thermal), Samsung's board thermistors
//! (/sys/class/sec/sec-*-thermistor) and the battery's own sensors (/sys/class/power_supply/battery), read only;
//! nothing outside the process is changed. README.md here has the sensors, the levels and why.
//!
//! In a session One UI's system_server is gone, and with it Samsung's skin-temperature policy (SSRM). The kernel's own
//! trips (the CPU and GPU junction at 90-110 C, LMh in hardware) keep running and cap every process, xnuhost's threads
//! too. Nothing else acts on the case temperature: this bridge does, inside xnuhost only.
//!
//! Two backends for the sensors (hw/, hw-backend-thermal): samsung, the files above (unchanged); treble, the thermal
//! HAL android.hardware.thermal.IThermal/default (getTemperatures: CPU, GPU, SOC, BATTERY, SKIN). The policy
//! ([`decide`], the step) is the same for both; the load line reads the same standard files in both.

mod clocks;
mod load;
mod session;
#[cfg(test)]
mod tests;

pub use clocks::{cpu_list, cpu_ticks, Clocks, PolicyClocks};
pub use load::{stat_ticks, HostLoad, LOAD_MS};
pub use session::{Listener, Options, Thermal};

use crate::hal::{self, Temperature};

/// A reading that is missing (all readings are tenths of a degree C; drivers/bridge/oneios_bridge_proto.h's
/// OB_THERMAL_UNKNOWN).
pub const UNKNOWN: i32 = i32::MIN;
/// How often the sensors are read while someone listens.
pub const POLL_MS: u32 = 5000;
/// A sample is sent at least this often, and at once on a new level.
pub const REPORT_MS: u32 = 30000;
pub const MAX_ZONES: usize = 96;
/// The SoC rule may read the lowest of the last N SoC readings (Limits::soc_samples, at most this many): heat that
/// lasts N polls, not one core's spike. 1 (the default) is one reading, as before. The hottest of about 30 junction
/// sensors swings by 20-35 C between two polls under load (the S25 Ultra, YumikoHayamato/OneiOS, 2026-11-19: 68.3 ->
/// 103.8 -> 68.3 C in 10 s), and on a phone whose first passive trip is at the S24's TRAPPING threshold each swing was
/// a new level for iOS (20 level changes in 8 minutes, 5 into TRAPPING: iOS's "too hot" screen).
pub const SOC_SAMPLES_MAX: usize = 4;

// iOS's thermal pressure levels (OSThermalNotification.h of the iPhoneOS SDK: kOSThermalPressureLevel*).
pub const NOMINAL: u32 = 0;
pub const LIGHT: u32 = 10;
pub const MODERATE: u32 = 20;
pub const HEAVY: u32 = 30;
pub const TRAPPING: u32 = 40;
pub const SLEEPING: u32 = 50;
// Which rule set the level (State::reason; the channel's OB_THERMAL_BY_*).
pub const BY_SKIN: u32 = 1;
pub const BY_SOC: u32 = 2;
pub const BY_BATTERY: u32 = 4;
/// xnuhost's one step: only the slowest P domain and the E cores ([`cap_for`]).
pub const CAP_CRITICAL: u32 = 2;

/// One reading of every sensor, tenths of a degree C, [`UNKNOWN`] when missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raw {
    /// The hottest CPU core or cluster zone, GPU zone, the DDR zone; soc = the hottest of the three.
    pub cpu: i32,
    pub gpu: i32,
    pub ddr: i32,
    pub soc: i32,
    /// The zone that gave soc.
    pub soc_zone: String,
    /// xo-therm: the crystal, near the SoC on the board.
    pub xo: i32,
    /// sec-ap-thermistor: Samsung's board thermistor by the SoC.
    pub ap: i32,
    /// power_supply/battery/temp
    pub battery: i32,
    /// power_supply/battery/lrp: Samsung's case sensor, by which its charger limits current.
    pub lrp: i32,
    /// The case: lrp, or ap when there is no lrp.
    pub skin: i32,
}

impl Default for Raw {
    /// Every reading missing.
    fn default() -> Raw {
        Raw {
            cpu: UNKNOWN,
            gpu: UNKNOWN,
            ddr: UNKNOWN,
            soc: UNKNOWN,
            soc_zone: String::new(),
            xo: UNKNOWN,
            ap: UNKNOWN,
            battery: UNKNOWN,
            lrp: UNKNOWN,
            skin: UNKNOWN,
        }
    }
}

/// Thresholds, tenths of a degree C. skin[k] turns on LIGHT, MODERATE, HEAVY, TRAPPING, SLEEPING for k = 0..4; soc[k]
/// and battery[k] turn on HEAVY and TRAPPING. A level stays until its reading is `*_hyst` below its threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub skin: [i32; 5],
    pub skin_hyst: i32,
    pub soc: [i32; 2],
    pub soc_hyst: i32,
    pub battery: [i32; 2],
    pub battery_hyst: i32,
    /// The SoC rule reads the lowest of the last soc_samples readings (0 or 1: one).
    pub soc_samples: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            // The case (Samsung's lrp sensor): its charger already limits current from 37-39 C with the screen on and
            // acts again at 46 C (the phone's device tree, battery,temp_table_LRP_* and battery,mix_v2_lrp_cond 460).
            // Light from 39, serious (HEAVY) from 44, critical (TRAPPING) from 46, SLEEPING from 50: a guess on that
            // scale, not Samsung's own policy, which is not readable (it lived in system_server).
            skin: [390, 420, 440, 460, 500],
            skin_hyst: 20,
            // The SoC: the X4's first trip is 90 C (cpufreq-cpu7), the GPU's 95 C, the cores pause at 108-110 C (the
            // phone's thermal zones). HEAVY at 95 C, TRAPPING at 105 C: the kernel acts first, iOS is told. Not
            // lower: a boot's first minute reaches 94.6 C on the cores for some seconds (the phone, 2026-11-12), and a
            // step then would only slow the boot.
            soc: [950, 1050],
            soc_hyst: 50,
            // One reading: a phone whose junction sensors swing past these between polls sets
            // ONEIOS_THERMAL_SOC_SAMPLES (the host-device table does it for the SM8750 phones, docs/hardware.md 7h).
            soc_samples: 1,
            // The cell: charging stops near 50 C on Samsung phones [inference]; HEAVY at 45 C, TRAPPING at 50 C.
            battery: [450, 500],
            battery_hyst: 20,
        }
    }
}

/// C's strtod on a prefix: an optional sign, digits, a point and digits, an exponent. The value and the rest.
fn strtod(text: &str) -> Option<(f64, &str)> {
    let b = text.as_bytes();
    let t = text.len() - text.trim_start().len();
    let mut i = t;
    if matches!(b.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let int = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    i += int;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        frac = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if int + frac > 0 {
            i += 1 + frac;
        }
    }
    if int + frac == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let exp = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        if exp > 0 {
            i = j + exp;
        }
    }
    Some((text[t..i].parse().ok()?, &text[i..]))
}

/// "390,420,440,460,500"-like text (degrees, "39,42,44,46,50") as N ascending thresholds in tenths. None when it is
/// not N ascending numbers from -50 to 200.
pub fn parse_limits<const N: usize>(text: &str) -> Option<[i32; N]> {
    let mut v = [0i32; N];
    let mut p = text;
    for (k, slot) in v.iter_mut().enumerate() {
        let (d, rest) = strtod(p)?;
        if !(-50.0..=200.0).contains(&d) {
            return None;
        }
        *slot = (d * 10.0 + if d < 0.0 { -0.5 } else { 0.5 }) as i32;
        p = rest.trim_start_matches(' ');
        if k + 1 < N {
            p = p.strip_prefix(',')?;
        }
    }
    if !p.trim_start_matches([' ', '\n']).is_empty() {
        return None;
    }
    // ascending, or the hysteresis has no meaning
    v.windows(2).all(|w| w[1] > w[0]).then_some(v)
}

impl Limits {
    /// The SoC rule on the phone's own trips (tenths of a degree C, UNKNOWN when the kernel has none): HEAVY 10 C over
    /// the kernel's first passive trip (the kernel throttles from there: a SoC at its first trip is the kernel doing
    /// its job, not heat for iOS), TRAPPING 5 C under the hot trip (or 15 C over the first passive one without a hot
    /// trip); never lower than what self has. The S25 Ultra (SM8750, YumikoHayamato/OneiOS sessions 3-7): first
    /// passive 105 C, hot 125 C, so 115 and 120 C, where the S24's 95 and 105 C had iOS in TRAPPING at every
    /// throttle. Returns whether soc changed. Applied by Thermal::open only with ONEIOS_THERMAL_SOC_TRIPS=1 (a
    /// host-device default: docs/hardware.md 7h). Ported from YumikoHayamato/OneiOS (s25-ultra-sm8750, 876419b).
    pub fn from_trips(&mut self, first_passive: i32, hot: i32) -> bool {
        if first_passive == UNKNOWN {
            return false;
        }
        let heavy = (first_passive + 100).max(self.soc[0]);
        let mut trapping =
            if hot != UNKNOWN && hot - 50 > first_passive + 100 { hot - 50 } else { first_passive + 150 };
        trapping = trapping.max(self.soc[1]);
        if trapping <= heavy {
            trapping = heavy + 50; // ascending, or the hysteresis has no meaning
        }
        let changed = [heavy, trapping] != self.soc;
        self.soc = [heavy, trapping];
        changed
    }
}

/// What the bridge decided from a reading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// NOMINAL .. SLEEPING (the highest of the three rules)
    pub level: u32,
    /// BY_* of the rules at that level
    pub reason: u32,
    /// Each rule's own level (its hysteresis follows it).
    pub skin_level: u32,
    pub soc_level: u32,
    pub battery_level: u32,
    /// xnuhost's step: cap_for (0, or CAP_CRITICAL)
    pub cap: u32,
    pub raw: Raw,
    /// The last SoC readings that were not missing, newest first.
    pub soc_seen: [i32; SOC_SAMPLES_MAX],
    /// How many of soc_seen are filled (at most the limits' soc_samples).
    pub soc_n: usize,
    /// What the SoC rule read: the lowest of soc_seen (UNKNOWN: none yet).
    pub soc_held: i32,
}

/// The level of one rule: the highest whose threshold the value reaches; a level at or below the previous one is kept
/// down to `hyst` below its threshold. A missing reading keeps the previous level.
fn rule(value: i32, on: &[i32], levels: &[u32], prev: u32, hyst: i32) -> u32 {
    if value == UNKNOWN {
        return prev;
    }
    on.iter()
        .zip(levels)
        .filter(|&(&on, &level)| value >= on - if level <= prev { hyst } else { 0 })
        .map(|(_, &level)| level)
        .next_back()
        .unwrap_or(NOMINAL)
}

/// The next state from a reading and the previous state (None: nothing before). The SoC rule reads soc_held, the
/// lowest of the last l.soc_samples SoC readings (prev's and this one; one reading when soc_samples is 0 or 1).
pub fn decide(l: &Limits, raw: &Raw, prev: Option<&State>) -> State {
    const SKIN_LEVELS: [u32; 5] = [LIGHT, MODERATE, HEAVY, TRAPPING, SLEEPING];
    const TWO_LEVELS: [u32; 2] = [HEAVY, TRAPPING];
    let mut s = State { raw: raw.clone(), soc_held: UNKNOWN, ..State::default() };
    // The SoC's last readings, newest first; a missing reading keeps the window (and, below, the rule's level).
    let window = l.soc_samples.clamp(1, SOC_SAMPLES_MAX);
    let newest = (raw.soc != UNKNOWN).then_some(raw.soc);
    let older = prev.map_or(&[][..], |p| &p.soc_seen[..p.soc_n]);
    for v in newest.into_iter().chain(older.iter().copied()).take(window) {
        s.soc_seen[s.soc_n] = v;
        s.soc_n += 1;
    }
    if let Some(&lowest) = s.soc_seen[..s.soc_n].iter().min() {
        s.soc_held = lowest;
    }
    let was = |f: fn(&State) -> u32| prev.map_or(NOMINAL, f);
    s.skin_level = rule(raw.skin, &l.skin, &SKIN_LEVELS, was(|p| p.skin_level), l.skin_hyst);
    let soc = if raw.soc == UNKNOWN { UNKNOWN } else { s.soc_held };
    s.soc_level = rule(soc, &l.soc, &TWO_LEVELS, was(|p| p.soc_level), l.soc_hyst);
    s.battery_level = rule(raw.battery, &l.battery, &TWO_LEVELS, was(|p| p.battery_level), l.battery_hyst);
    s.level = s.skin_level.max(s.soc_level).max(s.battery_level);
    if s.level != NOMINAL {
        for (rule_level, by) in [(s.skin_level, BY_SKIN), (s.soc_level, BY_SOC), (s.battery_level, BY_BATTERY)] {
            if rule_level == s.level {
                s.reason |= by;
            }
        }
    }
    s.cap = cap_for(&s);
    s
}

/// xnuhost's step (since fix-ram-locked, 2026-11-15): 0, all CPU cores, unless the cell is critical: the battery
/// rule at TRAPPING (50 C); then CAP_CRITICAL, only the slowest P domain and the E cores. The SoC is the kernel's (its
/// own trips and LMh). cpu's Topology::thermal_mask turns a step into CPUs (it still knows steps 1 and 3: thermalctl
/// prints them).
///
/// One step, at a critical temperature only (the owner's rule: all CPU cores, no caps of ours beyond a true
/// over-temperature guard). Until then the steps began at "heavy" (the case at 44 C, the SoC at 95 C, the cell at
/// 45 C): step 1 left out the X4, step 2 kept the slowest P domain and the E cores, step 3 the E cores alone. The heat
/// of the 2026-11-14/15 sessions came mostly from the host swapping iOS's RAM (host sys 200 %, swap-outs 2.4 MB/s at
/// SoC 83.8 C), which the lock removes. What stays:
/// - the phone's own throttling for the SoC (the kernel's trips on the X4 at 90 C, the GPU at 95 C, the cores pausing
///   at 108-110 C, LMh in hardware): the SoC rule informs iOS and takes no step;
/// - iOS's own reaction to the level it is told (its thermal state), as One UI's SSRM reacts on the phone;
/// - this step, where nothing else acts in a session: the cell at 50 C ("trapping" of the battery rule), down to its
///   hysteresis (2 C).
///
/// The case no longer takes a step (2026-11-24, the owner: heat is acceptable, the battery is not). At 50 C the case
/// rule still tells iOS "sleeping" (its own thermal state then slows the guest, as SSRM does on the phone) and the
/// phone's kernel caps the SoC by its trips; pinning all six P vCPUs onto four host CPUs at a 50 C skin only made the
/// guest slower: in the 0.1.24 session the two steps took 30 s each and the session stalled around the return to
/// "trapping" (case 50.7 C, battery 46.7 C, SoC 103 C). The cell's own 50 C is the guard that stays, because that is
/// the battery's limit rather than the skin's comfort. Step 2 (the slowest P domain and the E cores, cpu0-1 and 5-6
/// on this phone) and not step 3: the E cores alone put eight vCPUs on two cores, the crowding behind the hang of
/// 2026-11-13 (a preempted vCPU holding a kernel lock holds up the others).
pub fn cap_for(s: &State) -> u32 {
    if s.battery_level >= TRAPPING {
        CAP_CRITICAL
    } else {
        0
    }
}

pub fn level_name(level: u32) -> &'static str {
    match level {
        NOMINAL => "nominal",
        LIGHT => "light",
        MODERATE => "moderate",
        HEAVY => "heavy",
        TRAPPING => "trapping",
        SLEEPING => "sleeping",
        _ => "?",
    }
}

impl State {
    /// Whether `now` is worth a message after `self` was sent: a new level or cap at once, otherwise after REPORT_MS.
    pub fn changed(&self, now: &State, ms_since_sent: u32) -> bool {
        self.level != now.level || self.cap != now.cap || ms_since_sent >= REPORT_MS
    }
}

/// "41.0 C", "-0.5 C", or "-" for a missing reading.
pub(crate) fn deci_text(v: i32) -> String {
    if v == UNKNOWN {
        "-".to_owned()
    } else {
        format!("{}{}.{} C", if v < 0 { "-" } else { "" }, v.unsigned_abs() / 10, v.unsigned_abs() % 10)
    }
}

// ---- sysfs --------------------------------------------------------------------------------------

/// Up to `max` bytes of a file without its trailing newlines and spaces; None when it cannot be read or is empty.
fn read_line(path: &str, max: usize) -> Option<String> {
    let buf = xh_core::fs::read_bytes(path, max).ok()?;
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).trim_end_matches(['\n', ' ']).to_owned())
}

/// C's strtoll of base 10 on a prefix: leading white space, a sign, the digits; None without digits or past 64 bits.
pub fn strtoll(text: &str) -> Option<i64> {
    xh_core::parse::leading_i64(text, 10).map(|(v, _)| v)
}

fn read_int(path: &str) -> Option<i64> {
    strtoll(&read_line(path, 31)?)
}

/// Tenths of a degree from a node in tenths; implausible values (a sensor that is off reads -273 C here) are missing.
fn read_deci(root: &str, rel: &str) -> i32 {
    match read_int(&format!("{root}{rel}")) {
        Some(v) if (-400..=2000).contains(&v) => v as i32,
        _ => UNKNOWN,
    }
}

/// The zones' groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Cpu = 0,
    Gpu = 1,
    Ddr = 2,
    Xo = 3,
}

impl Group {
    pub fn name(self) -> &'static str {
        ["cpu", "gpu", "ddr", "xo"][self as usize]
    }

    fn of(kind: &str) -> Option<Group> {
        // SM8850 advertises cpu-hw-trip-0/1 as thermal zones at a fixed 105000 millidegrees even while real CPU
        // sensors are ~35-37 C. These are hardware trip/threshold pseudo-zones, NOT live CPU temperatures. Exclude by
        // exact family before the broad cpu-* sensor match: otherwise iOS falsely receives TRAPPING pressure at idle
        // (and hides real spikes).
        if kind.starts_with("cpu-hw-trip-") {
            None
        } else if kind.starts_with("cpu-") || kind.starts_with("cpuss-") {
            Some(Group::Cpu)
        } else if kind.starts_with("gpuss-") {
            Some(Group::Gpu)
        } else if kind == "ddr" {
            Some(Group::Ddr)
        } else if kind == "xo-therm" {
            Some(Group::Xo)
        } else {
            None
        }
    }
}

/// A zone of interest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    /// Its temp file.
    pub path: String,
    /// Its type, at most 23 bytes.
    pub kind: String,
    pub group: Group,
}

/// The sensors found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sensors {
    /// "" on the phone; a fixture's directory in tests (prefixed to every path).
    pub root: String,
    pub zones: Vec<Zone>,
    /// The kernel's own trips on the SoC's zones (cpu, gpu, ddr), tenths of a degree C, UNKNOWN when none: the median
    /// over those zones of each zone's lowest passive trip (60-129.9 C) and of its lowest hot or critical one
    /// (60-149.9 C). Read once by find; logged at the bridge's start on every phone.
    pub trip_first: i32,
    pub trip_hot: i32,
    /// Zones with a passive trip in that range.
    pub trip_zones: usize,
}

/// One zone's lowest passive trip from 60 to 129.9 C and its lowest hot or critical trip from 60 to 149.9 C, tenths
/// of a degree (UNKNOWN: none). Qualcomm's zones carry a 135 C passive trip that is a placeholder (on every CPU and GPU
/// zone of the S25 Ultra, beside the real ones at 105, 118, 120 and the hot one at 125 C), hence the range.
fn zone_trips(zone_dir: &str) -> (i32, i32) {
    let (mut passive, mut hot) = (UNKNOWN, UNKNOWN);
    let lower = |best: &mut i32, v: i32| {
        if *best == UNKNOWN || v < *best {
            *best = v;
        }
    };
    for i in 0..16 {
        // the kernel numbers its trips from 0 without gaps
        let Some(kind) = read_line(&format!("{zone_dir}/trip_point_{i}_type"), 23) else { break };
        let Some(v) = read_int(&format!("{zone_dir}/trip_point_{i}_temp")) else { continue };
        let deci = (v / 100) as i32;
        if kind == "passive" && (60000..130000).contains(&v) {
            lower(&mut passive, deci);
        } else if (kind == "hot" || kind == "critical") && (60000..150000).contains(&v) {
            lower(&mut hot, deci);
        }
    }
    (passive, hot)
}

/// The median of the values (the lower of the two middle ones for an even count); UNKNOWN for none.
fn median(mut v: Vec<i32>) -> i32 {
    if v.is_empty() {
        return UNKNOWN;
    }
    v.sort_unstable();
    v[(v.len() - 1) / 2]
}

impl Sensors {
    /// Finds the zones of interest under root/sys/class/thermal (cpu-*, cpuss-*, gpuss-*, ddr, xo-therm). None when no
    /// sensor of any kind can be read (no zone, no thermistor, no battery temperature).
    pub fn find(root: &str) -> Option<Sensors> {
        let dir = format!("{root}/sys/class/thermal");
        let (mut passive, mut hot, mut zones) = (Vec::new(), Vec::new(), Vec::new());
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if zones.len() >= MAX_ZONES {
                break;
            }
            let Ok(name) = e.file_name().into_string() else { continue };
            if !name.strip_prefix("thermal_zone").is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit())) {
                continue;
            }
            let Some(kind) = read_line(&format!("{dir}/{name}/type"), 63) else { continue };
            let Some(group) = Group::of(&kind) else { continue };
            zones.push(Zone {
                path: format!("{dir}/{name}/temp"),
                kind: crate::log::truncate(&kind, 23).to_owned(),
                group,
            });
            if group != Group::Xo {
                // the SoC's zones: cpu, gpu, ddr
                let (p, h) = zone_trips(&format!("{dir}/{name}"));
                if p != UNKNOWN {
                    passive.push(p);
                }
                if h != UNKNOWN {
                    hot.push(h);
                }
            }
        }
        let s = Sensors {
            root: root.to_owned(),
            zones,
            trip_zones: passive.len(),
            trip_first: median(passive),
            trip_hot: median(hot),
        };
        let raw = s.read();
        [raw.soc, raw.ap, raw.battery, raw.lrp].iter().any(|&v| v != UNKNOWN).then_some(s)
    }

    /// One reading.
    pub fn read(&self) -> Raw {
        let mut best: [Option<(i32, &str)>; 4] = [None; 4];
        for z in &self.zones {
            // millidegrees; a zone that is off reads -273000, a broken one huge numbers
            let Some(v) = read_int(&z.path).filter(|v| (-40000..=200000).contains(v)) else { continue };
            let deci = (v / 100) as i32;
            let slot = &mut best[z.group as usize];
            if slot.is_none_or(|(b, _)| deci > b) {
                *slot = Some((deci, &z.kind));
            }
        }
        let value = |g: Group| best[g as usize].map_or(UNKNOWN, |(v, _)| v);
        let mut raw = Raw {
            cpu: value(Group::Cpu),
            gpu: value(Group::Gpu),
            ddr: value(Group::Ddr),
            xo: value(Group::Xo),
            ..Raw::default()
        };
        for (v, name) in best[..3].iter().flatten() {
            if raw.soc == UNKNOWN || *v > raw.soc {
                raw.soc = *v;
                raw.soc_zone = (*name).to_owned();
            }
        }
        raw.ap = read_deci(&self.root, "/sys/class/sec/sec-ap-thermistor/temperature");
        raw.battery = read_deci(&self.root, "/sys/class/power_supply/battery/temp");
        raw.lrp = read_deci(&self.root, "/sys/class/power_supply/battery/lrp");
        raw.skin = if raw.lrp != UNKNOWN { raw.lrp } else { raw.ap };
        raw
    }
}

/// The treble backend (hw-backend-thermal): the thermal HAL's temperatures as a reading: cpu, gpu the hottest of their
/// type; soc the hottest of CPU, GPU and SOC (its name in soc_zone); battery; skin (the case) the hottest SKIN. ddr,
/// xo, ap and lrp (Samsung's) stay unknown. Implausible values (below -40 C, above 200 C) are skipped.
pub fn from_hal(temps: &[Temperature]) -> Raw {
    let mut raw = Raw::default();
    let hotter = |into: &mut i32, v: i32| {
        if *into == UNKNOWN || v > *into {
            *into = v;
        }
    };
    for t in temps {
        let c = t.celsius;
        if !(-40.0..=200.0).contains(&c) {
            continue; // NaN too
        }
        let deci = (c * 10.0 + if c < 0.0 { -0.5 } else { 0.5 }) as i32;
        match t.kind {
            hal::TEMP_CPU => hotter(&mut raw.cpu, deci),
            hal::TEMP_GPU => hotter(&mut raw.gpu, deci),
            hal::TEMP_BATTERY => hotter(&mut raw.battery, deci),
            hal::TEMP_SKIN => hotter(&mut raw.skin, deci),
            _ => {}
        }
        if matches!(t.kind, hal::TEMP_CPU | hal::TEMP_GPU | hal::TEMP_SOC) && (raw.soc == UNKNOWN || deci > raw.soc) {
            raw.soc = deci;
            raw.soc_zone = crate::log::truncate(&t.name, 23).to_owned();
        }
    }
    raw
}
