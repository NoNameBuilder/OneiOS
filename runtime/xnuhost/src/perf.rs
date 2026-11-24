// SPDX-License-Identifier: MIT
//! Port of perf.c. What one trip of an iOS thread through the kernel costs, step by step (docs/booter.md 2.6a).
//! Counters and histograms that each vCPU thread keeps for itself without locks (task.rs and cpu.rs add to them),
//! merged and printed by the main thread: every perf-log seconds (default 30; 0 = never on its own), at the stop and
//! on SIGQUIT.
//!   perf: 30.0 s: 112340 task traps (3745/s): syscall 80000, fault 20000, interrupt 9000, other 3340; ...
//!   perf:   up        n ... mean ... p50 ... p90 ... p99 ... max ... (microseconds)
//!   perf: cpu0: 56% of a core; of its wall time 31% kernel, 40% an iOS thread's turn, 29% idle; 3700 traps/s; last on host
//!     CPU 0; waited for a host CPU 3% (runnable, not running: schedstat)
//! A step is a histogram of nanoseconds with four buckets per power of two, so a quantile is good to 19%.

use crate::cpu::{self, Vcpu, MAX_CPUS};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use xh_core::logi;
use xnuhost_abi::{XH_TRAP_FAULT, XH_TRAP_INTERRUPT, XH_TRAP_SYSCALL};

/// A count with one writer (its vCPU's thread): an add is a plain load and store, as the C's `++`; others read it.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.store(self.0.load(Relaxed).wrapping_add(n), Relaxed);
    }
    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
    #[inline]
    pub fn set(&self, v: u64) {
        self.0.store(v, Relaxed);
    }
}

/// Histograms of durations in nanoseconds: 4 buckets per power of two, up to 2^32 ns.
pub const HIST_BUCKETS: usize = 128;

/// One histogram (`xh_hist`), as a plain value: a snapshot, a sum, the tests' own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hist {
    pub n: u64,
    pub sum_ns: u64,
    pub max_ns: u64,
    pub b: [u64; HIST_BUCKETS],
}

impl Default for Hist {
    fn default() -> Self {
        Hist { n: 0, sum_ns: 0, max_ns: 0, b: [0; HIST_BUCKETS] }
    }
}

fn bucket_of(ns: u64) -> usize {
    if ns < 4 {
        return ns as usize;
    }
    let top = 63 - ns.leading_zeros(); // >= 2
    let b = (top * 4) as usize + ((ns >> (top - 2)) & 3) as usize; // 2 mantissa bits
    (b - 4).min(HIST_BUCKETS - 1)
}

/// The largest ns that falls in bucket b.
fn bucket_top(b: usize) -> u64 {
    if b < 4 {
        return b as u64;
    }
    let (top, m) = ((b + 4) / 4, ((b + 4) % 4) as u64);
    ((4 + m + 1) << (top - 2)) - 1
}

impl Hist {
    pub fn add(&mut self, ns: u64) {
        self.n += 1;
        self.sum_ns += ns;
        self.max_ns = self.max_ns.max(ns);
        self.b[bucket_of(ns)] += 1;
    }

    /// ns (the upper bound of the bucket it falls in, never above the maximum).
    pub fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let want = (q * self.n as f64) as u64;
        let mut seen = 0;
        for (b, &count) in self.b.iter().enumerate() {
            seen += count;
            if seen > want {
                return bucket_top(b).min(self.max_ns);
            }
        }
        self.max_ns
    }

    /// Adds now - then (then: None for since the start); the maximum is since the start.
    fn add_delta(&mut self, now: &Hist, then: Option<&Hist>) {
        let then = then.map_or((0, 0), |t| (t.n, t.sum_ns));
        self.n += now.n - then.0;
        self.sum_ns += now.sum_ns - then.1;
        self.max_ns = self.max_ns.max(now.max_ns);
    }
}

/// A vCPU's live histogram: one writer.
#[derive(Debug)]
pub struct LiveHist {
    n: Counter,
    sum_ns: Counter,
    max_ns: Counter,
    b: [Counter; HIST_BUCKETS],
}

impl LiveHist {
    const fn new() -> LiveHist {
        LiveHist {
            n: Counter::new(),
            sum_ns: Counter::new(),
            max_ns: Counter::new(),
            b: [const { Counter::new() }; HIST_BUCKETS],
        }
    }

    #[inline]
    pub fn add(&self, ns: u64) {
        self.n.add(1);
        self.sum_ns.add(ns);
        if ns > self.max_ns.get() {
            self.max_ns.set(ns);
        }
        self.b[bucket_of(ns)].add(1);
    }

    fn snapshot(&self) -> Hist {
        Hist {
            n: self.n.get(),
            sum_ns: self.sum_ns.get(),
            max_ns: self.max_ns.get(),
            b: std::array::from_fn(|i| self.b[i].get()),
        }
    }
}

/// The steps a trip is timed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Step {
    /// the stub's handler entered -> the vCPU has the trap (register copy, the hand-off)
    Up,
    /// the vCPU released the thread -> the thread's handler has seen it (the hand-off back)
    Down,
    /// the kernel's work after a trap of that kind: from thread_run's return to this vCPU's next thread_run, the time
    /// it idled taken out
    KSyscall,
    KFault,
    KIrq,
    KOther,
    /// one order to a task's service thread, there and back
    OrderMap,
    OrderUnmap,
    OrderProtect,
    OrderClone,
    /// deferred maps carried out by the released thread itself (task.rs, map-defer)
    Drain,
    /// a running thread was signalled (the vCPU's interrupt line rose; by the raiser itself since 2026-11-23,
    /// task::kick, else by the vCPU) -> the vCPU has the thread's trap
    Kick,
}
pub const STEPS: usize = 12;
const STEP_NAMES: [&str; STEPS] = [
    "up (trap->vCPU)",
    "down (vCPU->task)",
    "kernel: syscall",
    "kernel: fault",
    "kernel: interrupt",
    "kernel: other",
    "order: map",
    "order: unmap",
    "order: protect",
    "order: clone",
    "maps by release",
    "kick",
];

/// The counts a vCPU keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Count {
    /// futex waits and wakes the hand-offs made
    HostSleeps,
    TaskSleeps,
    HostWakes,
    TaskWakes,
    /// task_map calls carried by a release / sent to the service thread
    MapsDeferred,
    MapsOrdered,
    /// task_unmap calls queued (no thread of the task ran) / sent as an order
    UnmapsDeferred,
    UnmapsOrdered,
    /// the kernel's timer_set calls; those that reached timer_settime
    TimerSets,
    TimerCalls,
    /// counter ticks spent inside thread_run (the iOS thread's turn) and cpu_idle
    TicksUser,
    TicksIdle,
    /// task_map/task_unmap calls joined to the queue's last entry (xnuhost-abi's stub.rs, xs_queue_join)
    OpsJoined,
    /// sched_setattr calls that gave an iOS thread its vCPU's uclamp.min (task.rs)
    UclampSets,
    /// raises of another vCPU's line that signalled or woke it; those it had up already
    Kicks,
    KicksSkipped,
    /// kicks of an iOS thread sent again: it was not back 200 ms after the first (task.rs)
    KicksAgain,
    /// raises that kicked the iOS thread a vCPU waits for straight away (task::kick)
    KicksDirect,
    /// fix-mach-rpc: sched_setaffinity calls that put an iOS thread with its vCPU (thread_run), and the ticks
    /// thread_run spent before the release (placement, uclamp, the queue)
    Places,
    PrepTicks,
    /// 2026-11-23: sched_setattr calls that set a vCPU thread's own uclamp.min (cpu::own_uclamp); of `Places`, those
    /// that moved the iOS thread to the other kind of core (E <-> P); and the iOS threads' turns (counter ticks of
    /// thread_run, as TicksUser) by the kernel's class of the thread (cpu::hint_class): realtime, UI, default, background
    VcpuUclampSets,
    PlacesAcross,
    TicksRt,
    TicksUi,
    TicksDefault,
    TicksBg,
    /// timer-coalesce (fix/unmap-fanout, 2026-11-24): timer_set calls that left the host timer as it was (armed for an
    /// earlier time, or a cancel: its signal raises nothing), and timer signals that came before the deadline and
    /// armed the host timer for it (cpu::timer_signal)
    TimerKept,
    TimerRearms,
}
const COUNTS: usize = 28;

/// `xh_perf`: a vCPU's histograms and counts, and thread_run's bookkeeping between two calls.
#[derive(Debug)]
pub struct Perf {
    h: [LiveHist; STEPS],
    /// by xh_trap kind
    traps: [Counter; 16],
    counts: [Counter; COUNTS],
    pub ret_at: Counter,
    pub ret_idle: Counter,
    pub ret_kind: Counter,
}

impl Default for Perf {
    fn default() -> Self {
        Self::new()
    }
}

impl Perf {
    pub const fn new() -> Perf {
        Perf {
            h: [const { LiveHist::new() }; STEPS],
            traps: [const { Counter::new() }; 16],
            counts: [const { Counter::new() }; COUNTS],
            ret_at: Counter::new(),
            ret_idle: Counter::new(),
            ret_kind: Counter::new(),
        }
    }

    #[inline]
    pub fn step(&self, s: Step, ns: u64) {
        self.h[s as usize].add(ns);
    }
    #[inline]
    pub fn count(&self, c: Count, n: u64) {
        self.counts[c as usize].add(n);
    }
    pub fn get(&self, c: Count) -> u64 {
        self.counts[c as usize].get()
    }
    #[inline]
    pub fn trap(&self, kind: u32) {
        self.traps[(kind & 15) as usize].add(1);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            h: std::array::from_fn(|i| self.h[i].snapshot()),
            traps: std::array::from_fn(|i| self.traps[i].get()),
            counts: std::array::from_fn(|i| self.counts[i].get()),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    h: [Hist; STEPS],
    traps: [u64; 16],
    counts: [u64; COUNTS],
}

impl Snapshot {
    fn count(&self, c: Count) -> u64 {
        self.counts[c as usize]
    }
}

/// For host calls outside a vCPU thread (none today).
static SHARED: Perf = Perf::new();

/// The calling vCPU's, else a shared one.
pub fn current() -> &'static Perf {
    cpu::current().map_or(&SHARED, |v| &v.perf)
}

/// Counter ticks to nanoseconds.
pub fn ticks_ns(ticks: u64) -> u64 {
    let hz = crate::hostcalls::info().timebase_hz;
    cpu::ticks_to_ns(ticks, if hz != 0 { hz } else { cpu::counter_hz() })
}

struct Reporter {
    every: u32,
    /// counter
    last_report: u64,
    start: u64,
    /// the previous report's values, for the deltas
    last: [Snapshot; MAX_CPUS],
    last_cpu_ns: [u64; MAX_CPUS],
    /// each vCPU thread's time runnable but not running (schedstat), at the previous report
    last_wait_ns: [u64; MAX_CPUS],
}

static REPORTER: Mutex<Option<Reporter>> = Mutex::new(None);

fn reporter() -> std::sync::MutexGuard<'static, Option<Reporter>> {
    REPORTER.lock().unwrap_or_else(|e| e.into_inner())
}

/// perf-log (None: 30).
pub fn init(perf_log: Option<u32>) {
    let now = cpu::counter();
    *reporter() = Some(Reporter {
        every: perf_log.unwrap_or(30),
        last_report: now,
        start: now,
        last: Default::default(),
        last_cpu_ns: [0; MAX_CPUS],
        last_wait_ns: [0; MAX_CPUS],
    });
}

fn hist_line(name: &str, h: &Hist) {
    if h.n == 0 {
        return;
    }
    let us = |ns: u64| ns as f64 / 1000.0;
    logi!(
        "perf:   {:<18} n {:8}  mean {:7.2}  p50 {:7.2}  p90 {:7.2}  p99 {:8.2}  max {:9.1} us",
        name,
        h.n,
        h.sum_ns as f64 / h.n as f64 / 1000.0,
        us(h.quantile(0.5)),
        us(h.quantile(0.9)),
        us(h.quantile(0.99)),
        us(h.max_ns)
    );
}

fn thread_cpu_ns(v: &Vcpu) -> u64 {
    if !v.started.load(Relaxed) || v.pthread() <= 1 {
        return 0;
    }
    let mut id: libc::clockid_t = 0;
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: the clock of a live pthread (the vCPU threads never end), then a read of it.
    let ok = unsafe {
        libc::pthread_getcpuclockid(v.pthread() as libc::pthread_t, &mut id) == 0
            && libc::clock_gettime(id, &mut ts) == 0
    };
    if ok {
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    } else {
        0
    }
}

/// How long a thread of this process waited runnable for a host CPU, in ns (/proc/self/task/TID/schedstat: run time,
/// wait time, timeslices; CONFIG_SCHED_INFO), 0 if unknown. One named file per vCPU per report, as [`thread_psr`].
fn thread_wait_ns(tid: i32) -> u64 {
    if tid <= 0 {
        return 0;
    }
    let Ok(text) = std::fs::read(format!("/proc/self/task/{tid}/schedstat")) else { return 0 };
    schedstat_wait(&String::from_utf8_lossy(&text[..text.len().min(127)]))
}

/// The wait time (the second field) of a schedstat line.
fn schedstat_wait(line: &str) -> u64 {
    line.split_whitespace().nth(1).and_then(|f| f.parse().ok()).unwrap_or(0)
}

/// The host CPU a thread last ran on (/proc/self/task/TID/stat field 39, "processor"), -1 if unknown. A named file
/// per known thread: no directory walk (the phone's rule).
fn thread_psr(tid: i32) -> i32 {
    let Ok(line) = std::fs::read(format!("/proc/self/task/{tid}/stat")) else { return -1 };
    let line = String::from_utf8_lossy(&line[..line.len().min(1023)]).into_owned();
    let Some(close) = line.rfind(')') else { return -1 };
    // after ") ": field 3 (state) onwards; the processor is field 39
    line[close + 1..].split(' ').nth(37).and_then(|f| f.trim().parse().ok()).unwrap_or(-1)
}

/// The report now (`xh_perf_log`): since the start (the stop, SIGQUIT), else since the last report.
pub fn log(since_start: bool) {
    let mut guard = reporter();
    let Some(r) = guard.as_mut() else { return };
    let now = cpu::counter();
    let wall = now - if since_start { r.start } else { r.last_report };
    let seconds = ticks_ns(wall) as f64 / 1e9;
    if seconds <= 0.0 {
        return;
    }
    let vcpus = cpu::vcpus();
    let snaps: Vec<Snapshot> = vcpus.iter().map(|v| v.perf.snapshot()).collect();
    let mut sum = Snapshot::default();
    for (i, p) in snaps.iter().enumerate() {
        let then = (!since_start).then(|| &r.last[i]);
        for k in 0..STEPS {
            sum.h[k].add_delta(&p.h[k], then.map(|t| &t.h[k]));
            for b in 0..HIST_BUCKETS {
                sum.h[k].b[b] += p.h[k].b[b] - then.map_or(0, |t| t.h[k].b[b]);
            }
        }
        for k in 0..16 {
            sum.traps[k] += p.traps[k] - then.map_or(0, |t| t.traps[k]);
        }
        for k in 0..COUNTS {
            sum.counts[k] += p.counts[k] - then.map_or(0, |t| t.counts[k]);
        }
    }
    let traps: u64 = sum.traps.iter().sum();
    let (sys, fault, irq) =
        (sum.traps[XH_TRAP_SYSCALL as usize], sum.traps[XH_TRAP_FAULT as usize], sum.traps[XH_TRAP_INTERRUPT as usize]);
    use Count::*;
    logi!(
        "perf: {:.1} s{}: {} task traps ({:.0}/s): syscall {}, fault {}, interrupt {}, other {}; futex sleeps: vCPU {}, \
         iOS thread {} (of {} hand-offs each way); futex wakes {} + {}; task_map {} carried by a release, {} ordered; \
         task_unmap {} queued, {} ordered; timer_set {} ({} to the host timer, {} kept it; {} early signals armed it again)",
        seconds,
        if since_start { " since the start" } else { "" },
        traps,
        traps as f64 / seconds,
        sys,
        fault,
        irq,
        traps - sys - fault - irq,
        sum.count(HostSleeps),
        sum.count(TaskSleeps),
        sum.h[Step::Up as usize].n,
        sum.count(HostWakes),
        sum.count(TaskWakes),
        sum.count(MapsDeferred),
        sum.count(MapsOrdered),
        sum.count(UnmapsDeferred),
        sum.count(UnmapsOrdered),
        sum.count(TimerSets),
        sum.count(TimerCalls),
        sum.count(TimerKept),
        sum.count(TimerRearms)
    );
    // host-overhead (2026-11-15): what the queue joined, the clamps given to iOS threads, the cross-vCPU signals.
    logi!(
        "perf:   queue: {} maps and unmaps joined to the one before (one host call less each); uclamp: {} given to iOS \
         threads, {} to vCPU threads (their own: 0 at each guest idle); interrupt lines raised on other vCPUs: {} signalled or woken ({} of them by a kick of the iOS thread \
         they wait for, sent by the raiser), {} already up (no signal); kicks of iOS threads sent again {} (not back 200 \
         ms after the first)",
        sum.count(OpsJoined),
        sum.count(UclampSets),
        sum.count(VcpuUclampSets),
        sum.count(Kicks),
        sum.count(KicksDirect),
        sum.count(KicksSkipped),
        sum.count(KicksAgain)
    );
    // fix-mach-rpc (2026-11-15): what a release costs on the host before its hand-off (the `down` line starts after
    // it), and how often an iOS thread was put on another vCPU's core (one sched_setaffinity each).
    let releases = sum.h[Step::Down as usize].n.max(1);
    logi!(
        "perf:   release: {} iOS threads put with another vCPU's core (sched_setaffinity), {} of them across E and P; the \
         host's work before each release {:.2} us on average (placement, uclamp, the queue)",
        sum.count(Places),
        sum.count(PlacesAcross),
        ticks_ns(sum.count(PrepTicks)) as f64 / releases as f64 / 1000.0
    );
    for (name, h) in STEP_NAMES.iter().zip(&sum.h) {
        hist_line(name, h);
    }
    // 2026-11-23: what ran where. The iOS threads' turns on each vCPU by the kernel's class of the thread, against the
    // vCPU's wall time: background and utility work belongs on the E vCPUs, the UI's and the render server's on P.
    let mut qos = String::new();
    for (i, (v, p)) in vcpus.iter().zip(&snaps).enumerate() {
        let then = (!since_start).then(|| &r.last[i]);
        let at = |c: Count| p.count(c) - then.map_or(0, |t| t.count(c));
        let pct = |c: Count| (at(c) as f64 * 100.0 / wall.max(1) as f64).round() as u64;
        if v.started.load(Relaxed) {
            let kind = match v.kind.load(Relaxed) {
                0 => '?',
                k => k as char,
            };
            let sep = if qos.is_empty() { "" } else { ", " };
            let (rt, ui, def, bg) = (pct(TicksRt), pct(TicksUi), pct(TicksDefault), pct(TicksBg));
            qos += &format!("{sep}cpu{i} {kind} {rt}/{ui}/{def}/{bg}");
        }
    }
    if !qos.is_empty() {
        logi!("perf: qos: the iOS threads' turns, % of each vCPU's wall time (realtime/UI/default/background): {qos}");
    }
    for (i, (v, p)) in vcpus.iter().zip(&snaps).enumerate() {
        let then = (!since_start).then(|| &r.last[i]);
        let cpu_ns = thread_cpu_ns(v);
        let cpu_then = if since_start { 0 } else { r.last_cpu_ns[i] };
        let wait_ns = thread_wait_ns(v.tid.load(Relaxed));
        let wait_then = if since_start { 0 } else { r.last_wait_ns[i] };
        let user = p.count(TicksUser) - then.map_or(0, |t| t.count(TicksUser));
        let idle = p.count(TicksIdle) - then.map_or(0, |t| t.count(TicksIdle));
        let t: u64 = (0..16).map(|k| p.traps[k] - then.map_or(0, |t| t.traps[k])).sum();
        let w = wall as f64;
        let kern = w - user as f64 - idle as f64;
        if v.started.load(Relaxed) && w > 0.0 {
            logi!(
                "perf: cpu{}: {:.0}% of a core; of its wall time {:.0}% kernel, {:.0}% an iOS thread's turn, {:.0}% idle; \
                 {:.0} traps/s; last on host CPU {}; waited for a host CPU {:.0}% (runnable, not running)",
                i,
                if cpu_ns >= cpu_then { (cpu_ns - cpu_then) as f64 / 1e7 / seconds } else { 0.0 },
                if kern > 0.0 { 100.0 * kern / w } else { 0.0 },
                100.0 * user as f64 / w,
                100.0 * idle as f64 / w,
                t as f64 / seconds,
                thread_psr(v.tid.load(Relaxed)),
                if wait_ns >= wait_then { (wait_ns - wait_then) as f64 / 1e7 / seconds } else { 0.0 }
            );
        }
        if !since_start {
            r.last[i] = p.clone();
            r.last_cpu_ns[i] = cpu_ns;
            r.last_wait_ns[i] = wait_ns;
        }
    }
    if !since_start {
        r.last_report = now;
    }
}

/// Main loop: a report every perf-log seconds.
pub fn tick() {
    let due = {
        let g = reporter();
        let Some(r) = g.as_ref() else { return };
        r.every > 0 && ticks_ns(cpu::counter() - r.last_report) >= u64::from(r.every) * 1_000_000_000
    };
    if due {
        log(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hist() {
        let mut h = Hist::default();
        assert_eq!(h.quantile(0.5), 0);
        for ns in 1..=1000u64 {
            h.add(ns * 1000); // 1 us .. 1 ms, evenly
        }
        assert!(h.n == 1000 && h.max_ns == 1_000_000 && h.sum_ns == 500_500_000);
        let (p50, p99) = (h.quantile(0.5), h.quantile(0.99));
        assert!((500_000..=500_000 * 5 / 4).contains(&p50)); // a bucket is a quarter of a power of two
        assert!((990_000..=1_000_000).contains(&p99)); // never above the maximum
        for ns in (0..70000).step_by(7) {
            // every value lands at or below its bucket's top
            let mut h = Hist::default();
            h.add(ns);
            assert_eq!(h.quantile(0.0), ns);
        }
        let mut h = Hist::default();
        h.add(!0u64 >> 1); // past 2^32 ns: the last bucket
        assert_eq!(h.b[HIST_BUCKETS - 1], 1);
        // the live one keeps the same buckets
        let live = LiveHist::new();
        for ns in [3, 900, 5000, 1 << 40] {
            live.add(ns);
        }
        let mut plain = Hist::default();
        [3, 900, 5000, 1 << 40].into_iter().for_each(|ns| plain.add(ns));
        assert_eq!(live.snapshot(), plain);
    }

    #[test]
    fn report_lines() {
        // A report with two vCPUs' counts (no thread: no cpu lines): the summary sums both, the step lines name the
        // steps that have samples.
        let _m = cpu::tests::machine();
        cpu::init(2).unwrap();
        init(Some(0));
        let p = &cpu::vcpu(1).perf;
        p.trap(XH_TRAP_SYSCALL);
        p.trap(XH_TRAP_SYSCALL);
        p.trap(XH_TRAP_FAULT);
        p.count(Count::OpsJoined, 5);
        p.step(Step::Up, 2500);
        let ((), lines) = xh_core::log::capture(|| log(true));
        let text: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert!(text.iter().any(|l| l.contains(" since the start: 3 task traps (")
            && l.contains("syscall 2, fault 1, interrupt 0, other 0")));
        assert!(text.iter().any(|l| l.contains("perf:   queue: 5 maps and unmaps joined")));
        assert!(text.iter().any(|l| l.contains("perf:   up (trap->vCPU)    n        1  mean    2.50")), "{text:?}");
        assert!(!text.iter().any(|l| l.contains("kernel: fault")));
    }

    #[test]
    fn schedstat_wait_time() {
        // run time, wait time, timeslices (Linux's proc_pid_schedstat)
        assert_eq!(schedstat_wait("123456789 98765 42\n"), 98765);
        assert_eq!(schedstat_wait(""), 0);
        assert_eq!(thread_wait_ns(0), 0);
        // this thread's own file: a number (0 without CONFIG_SCHED_INFO), never a panic
        // SAFETY: gettid has no arguments and cannot fail.
        let me = unsafe { libc::gettid() };
        let _ = thread_wait_ns(me);
    }
}
