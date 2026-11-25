#!/bin/sh
# runtime/xnuhost/check-arm64.sh [smoke|check|bench|parallel|rpc|all]...: the qemu boots of the test kernel that the C
# Makefile's check-arm64, bench-arm64, parallel-arm64 and rpc-arm64 targets made, for the Rust xnuhost. With no
# argument: check. Under qemu-aarch64 on a PC (the times are qemu's: compare runs on the same host only), natively on
# an arm64 Linux host.
#   smoke     the vCPU machinery alone (examples/machine_smoke.rs): no kernel image, no session; seconds
#   check     the five boots of testkernel/check-arm64.sh: the normal boot ('TESTKERNEL: all passed'), ram=auto with
#             a made-up meminfo, the restart loop (exit 3 after 9 'TESTKERNEL: restart'), the last words (a
#             SIGUSR1 from outside: its 'fatal signal' line, status 138), the task trim (task-trim-idle=1)
#   bench     the trap path's costs (docs/booter.md 2.6a): the test kernel's benchmarks, then xnuhost's "perf:" report
#   parallel  N threads of one task, one per vCPU (docs/booter.md 2.6b): the speed-up for 1, 2, 4 and 8 threads
#   rpc       the Mach RPC round trip (docs/booter.md 2.6d), once per interrupt path (RPC_MODES)
#   ab        two binaries compared: the bench and rpc boots of XNUHOST and XNUHOST_B in turn, AB_ROUNDS rounds, all
#             pinned to the same cores, then each BENCH line's mean over the rounds for both and B's difference in %.
#             One run of each is not a comparison: a boot's numbers move 10-30% with the host cores it lands on (a
#             hybrid PC's fast and slow cores) and the load; alternate rounds on the same cores move by about 3%.
#               XNUHOST_B=FILE  the other binary (required: e.g. a build of another commit)
#               AB_ROUNDS=N     rounds (default 4); logs in WORK/ab (A.N.txt, B.N.txt)
#               PIN=CPUS        the host CPUs (taskset -c); for ab the default is one thread of each fastest core
#                               (cpufreq's cpuinfo_max_freq within 10% of the top, the first of its siblings)
# It builds, unless given: the static xnuhost (cargo, aarch64-linux-android, crt-static, target dir target/machine-static),
# the test kernel (testkernel/build.sh) and its image (the loader crate's mkkernel: kernel.kc, os.img).
#   XNUHOST=FILE   boot this binary instead (e.g. a Rust xnuhost of another commit)
#   IMAGE=DIR      this test image (kernel.kc, os.img) instead of building one
#   WORK=DIR       the data store, the logs and the built image (default runtime/target/machine-check)
#   QEMU=FILE      another qemu-aarch64
#   BENCH_SCALE=N, BENCH_SET="--set spin-host=5 ..."      the bench's scale and xnuhost settings
#   PAR_CPUS=8, PAR_SCALE=N, PAR_SET="--set cpu-pin=2"   the parallel test's vCPUs, scale, settings
#   RPC_MODES="0 1 2", RPC_SCALE=N, RPC_SET=...          the rpc test's modes (0 ArmCpu's trapped GIC, 1 host calls,
#                                                         2 one vCPU, no IPI: the floor), scale, settings
set -eu
here=$(cd "$(dirname "$0")" && pwd)
runtime=$(dirname "$here")
work=${WORK:-$runtime/target/machine-check}
mkdir -p "$work/testdata"
[ $# -gt 0 ] || set -- check
for what in "$@"; do
  case $what in smoke|check|bench|parallel|rpc|ab) ;; all) set -- smoke check bench parallel rpc; break ;;
    *) echo "usage: check-arm64.sh [smoke|check|bench|parallel|rpc|ab|all]..." >&2; exit 2 ;; esac
done

case "$(uname -s)/$(uname -m)" in
  Linux/aarch64|Linux/arm64) run= ;;
  # QEMU is a path here, but callers that only mean "run the boot part" set QEMU=1: an
  # unexecutable value is not a qemu binary, so fall back to the one on PATH.
  *) if [ -n "${QEMU:-}" ] && [ -x "${QEMU}" ]; then run=$QEMU; else run=$(command -v qemu-aarch64 || true); fi
     [ -n "$run" ] || { echo "check-arm64: qemu-aarch64 not installed: skipped"; exit 0; } ;;
esac
# One thread of each of the fastest cores: cpuinfo_max_freq within 10% of the top, the first CPU of its siblings.
fast_cpus() {
  top=0 n=0
  while [ -d /sys/devices/system/cpu/cpu$n ]; do
    f=$(cat /sys/devices/system/cpu/cpu$n/cpufreq/cpuinfo_max_freq 2>/dev/null || echo 0)
    [ "$f" -gt "$top" ] && top=$f
    n=$((n + 1))
  done
  list= n=0
  while [ -d /sys/devices/system/cpu/cpu$n ]; do
    d=/sys/devices/system/cpu/cpu$n
    f=$(cat "$d/cpufreq/cpuinfo_max_freq" 2>/dev/null || echo 0)
    first=$(cut -d, -f1 "$d/topology/thread_siblings_list" 2>/dev/null | cut -d- -f1)
    [ "$f" -ge $((top - top / 10)) ] && [ "${first:-$n}" = "$n" ] && list=$list${list:+,}$n
    n=$((n + 1))
  done
  echo "$list"
}
for what in "$@"; do [ "$what" = ab ] && [ -z "${PIN:-}" ] && PIN=$(fast_cpus); done
if [ -n "${PIN:-}" ]; then
  command -v taskset > /dev/null || { echo "check-arm64: PIN needs taskset (util-linux)" >&2; exit 2; }
  run="taskset -c $PIN${run:+ $run}"
fi

# ---- what is booted -----------------------------------------------------------------------------------------------
static_build() {
  (cd "$runtime" && CARGO_TARGET_DIR=target/machine-static cargo build --quiet --release --target aarch64-linux-android \
     -p xnuhost "$@" --config 'target.aarch64-linux-android.rustflags=["-C","target-feature=+crt-static"]')
}
for what in "$@"; do
  [ "$what" = smoke ] || continue
  static_build --example machine-smoke
  # shellcheck disable=SC2086 # $run is a word list
  timeout 60 $run "$runtime/target/machine-static/aarch64-linux-android/release/examples/machine-smoke"
done
for what in "$@"; do [ "$what" = smoke ] || need=1; done
[ -n "${need:-}" ] || exit 0
if [ -z "${XNUHOST:-}" ]; then
  static_build --bin xnuhost
  XNUHOST=$runtime/target/machine-static/aarch64-linux-android/release/xnuhost
fi
if [ -z "${IMAGE:-}" ]; then
  IMAGE=$work/testimage
  mkdir -p "$IMAGE"
  sh "$here/testkernel/build.sh" "$work/testkernel.elf"
  (cd "$runtime" && CARGO_TARGET_DIR=target/machine cargo run --quiet -p xnuloader --bin mkkernel -- \
     "$work/testkernel.elf" "$IMAGE/kernel.kc" "$IMAGE/os.img")
fi
common="--mode bench --image $IMAGE --data $work/testdata --display none --no-input --width 64 --height 64"
# key=value boot arguments: "testkernel-bench" or "testkernel-bench=N"
scaled() { if [ -n "$2" ]; then echo "$1=$2"; else echo "$1"; fi; }

for what in "$@"; do
  rm -f "$work/testdata/data.img"
  case $what in
  check)
    sh "$here/testkernel/check-arm64.sh" "$XNUHOST" "$IMAGE" "$work" ;;
  bench)
    # shellcheck disable=SC2086 # $run, $common and the *_SET settings are word lists
    timeout 120 $run "$XNUHOST" $common --cpus 3 --ram 256M --set ram-giveback=1 \
      --boot-args "$(scaled testkernel-bench "${BENCH_SCALE:-}")" ${BENCH_SET:-} > "$work/bench.log" 2>&1 || true
    grep -E 'BENCH|perf:|TESTKERNEL: (all|FAIL)|hand-off' "$work/bench.log" ;;
  parallel)
    # shellcheck disable=SC2086
    timeout 300 $run "$XNUHOST" $common --cpus "${PAR_CPUS:-8}" --ram 256M --set ram-giveback=1 \
      --boot-args "$(scaled testkernel-parallel "${PAR_SCALE:-}")" ${PAR_SET:-} > "$work/parallel.log" 2>&1 || true
    grep -E 'PARALLEL|perf:   queue|TESTKERNEL: (all|FAIL)' "$work/parallel.log" ;;
  rpc)
    # Each mode is its own run, so xnuhost's perf report and `emulated:` lines at the stop are that mode's.
    for m in ${RPC_MODES:-0 1 2}; do
      rm -f "$work/testdata/data.img"
      # shellcheck disable=SC2086
      timeout 120 $run "$XNUHOST" $common --cpus 2 --ram 256M --set ram-giveback=1 \
        --boot-args "$(scaled testkernel-rpc "${RPC_SCALE:-}") testkernel-rpc-mode=$m" ${RPC_SET:-} > "$work/rpc-$m.log" 2>&1 || true
      grep -E 'RPC|BENCH|perf: [0-9.]+ s since|perf:   (up|down|queue|release)|emulated: insn|TESTKERNEL: (all|FAIL)' "$work/rpc-$m.log"
    done ;;
  ab)
    b=${XNUHOST_B:-}
    [ -n "$b" ] && [ -x "$b" ] || { echo "check-arm64 ab: set XNUHOST_B=FILE, the binary to compare with (no default)" >&2; exit 2; }
    echo "ab: A $XNUHOST"
    echo "ab: B $b"
    echo "ab: ${AB_ROUNDS:-4} rounds, each binary in turn, on host CPUs $PIN"
    mkdir -p "$work/ab"
    rm -f "$work/ab"/A.*.txt "$work/ab"/B.*.txt # only this run's rounds go into the means
    i=1
    while [ "$i" -le "${AB_ROUNDS:-4}" ]; do
      for side in A B; do
        bin=$XNUHOST; [ $side = B ] && bin=$b
        echo "ab: round $i $side (load $(cut -d' ' -f1 /proc/loadavg))"
        # PIN is set: the boots below run on the same cores
        WORK=$work/ab/work XNUHOST=$bin IMAGE=$IMAGE PIN=$PIN sh "$0" bench rpc > "$work/ab/$side.$i.txt" 2>&1 || true
      done
      i=$((i + 1))
    done
    # Each BENCH line's mean (ns) over the rounds; the rpc lines are keyed by their mode ("RPC: mode N").
    for side in A B; do
      cat "$work/ab/$side".*.txt | awk -v side=$side '
        /RPC: mode/ { mode = $0; sub(/.*RPC: mode /, "", mode); sub(/,.*/, "", mode) }
        /BENCH/ && / mean / {
          l = $0; sub(/.*BENCH +/, "", l); k = l; sub(/:.*/, "", k); if (l ~ /^(rpc|IPI)/) k = "rpc " mode ": " k
          m = l; sub(/.* mean /, "", m); sub(/ ns.*/, "", m); print side "\t" k "\t" m }'
    done | awk -F'\t' '
      { if (!(($2) in seen)) { seen[$2] = 1; order[++n] = $2 } s[$1, $2] += $3; c[$1, $2]++ }
      END {
        printf "ab: %-74s %9s %9s %7s\n", "mean ns over the rounds", "A", "B", "B vs A"
        for (i = 1; i <= n; i++) {
          k = order[i]; a = c["A", k] ? s["A", k] / c["A", k] : 0; b = c["B", k] ? s["B", k] / c["B", k] : 0
          printf "ab: %-74s %9.0f %9.0f %+6.1f%%\n", substr(k, 1, 74), a, b, a ? (b - a) * 100 / a : 0
        }
      }' ;;
  esac
done
