#!/bin/sh
# runtime/xnuhost/testkernel/check-arm64.sh XNUHOST IMAGE WORK: five boots (the first four the Makefile's
# `make check-arm64`), for any xnuhost binary (the C one, the Rust one) and any test image (IMAGE/kernel.kc and
# IMAGE/os.img, as mkkernel writes them from the test kernel's ELF). WORK holds the data store and the logs (check.log,
# ramauto.log, lastwords.log, restart.log, trim.log). Under qemu-aarch64 on a PC, natively on an arm64 Linux host;
# QEMU=... names another qemu.
#   1. the normal boot: 3 vCPUs, 256 MiB, ram-giveback=1: 'TESTKERNEL: all passed'
#   2. ram=auto on a host of 4 GiB, 3.5 GiB available (ram-meminfo), ram-tasks=64: 1024 or 1280 MiB, held and prefaulted
#   3. a guest that keeps restarting: xnuhost starts again in place 8 times, then exit 3 (9 'TESTKERNEL: restart')
#   4. the last words: SIGUSR1 from outside to xnuhost in the test kernel's hold (testkernel-hold): its line
#      'xnuhost !! fatal signal 10 (SIGUSR1) ...: code 0, sent by pid <this shell>', then status 138 (it dies of the signal)
#   5. the trim (task-trim-idle=1, testkernel-trim): an idle task's host mappings are dropped and filled again
#      ('TESTKERNEL: all passed' and xnuhost's 'tasks: trim: the first' line)
set -u
[ $# -eq 3 ] || { echo "usage: check-arm64.sh XNUHOST IMAGE WORK" >&2; exit 2; }
xnuhost=$1 image=$2 work=$3
case "$(uname -s)/$(uname -m)" in
  Linux/aarch64|Linux/arm64) run= ;;
  # QEMU is a path here; a caller that only means "run the boot part" sets QEMU=1, which is
  # not a binary, so fall back to the one on PATH.
  *) if [ -n "${QEMU:-}" ] && [ -x "${QEMU}" ]; then run=$QEMU; else run=$(command -v qemu-aarch64 || true); fi
     [ -n "$run" ] || { echo "check-arm64: qemu-aarch64 not installed: skipped"; exit 0; } ;;
esac
mkdir -p "$work/testdata"
common="--mode bench --image $image --data $work/testdata --display none --no-input --width 64 --height 64"
rm -f "$work/testdata/data.img"
echo "check-arm64: booting the test kernel under ${run:-this arm64 Linux kernel}"
# shellcheck disable=SC2086 # $run and $common are word lists
timeout 30 $run "$xnuhost" $common --cpus 3 --ram 256M --set ram-giveback=1 2>&1 | tee "$work/check.log"
grep -q 'TESTKERNEL: all passed' "$work/check.log" || { echo "check-arm64: the test kernel did not pass"; exit 1; }

echo "check-arm64: ram=auto on a host of 4 GiB, 3.5 GiB available (ram-meminfo), sized for 64 tasks (ram-tasks=64: the test kernel runs none): the guest gets what fits, 1024 MiB (1280 where the lock can work: a smaller headroom), held fixed and locked by default (ram-giveback=0; where mlock fails, a clean fallback)"
printf 'MemTotal:        4194304 kB\nMemAvailable:    3670016 kB\n' > "$work/meminfo-3g"
rm -f "$work/testdata/data.img"
# shellcheck disable=SC2086
timeout 30 $run "$xnuhost" $common --cpus 3 --set "ram-meminfo=$work/meminfo-3g" --set ram-tasks=64 > "$work/ramauto.log" 2>&1
if ! { grep -Eq 'ram=auto: host MemTotal 4096 MiB, MemAvailable 3584 MiB; the guest gets (1024|1280) MiB' "$work/ramauto.log" &&
       grep -Eq 'memory: the guest.s RAM is held for the session: (1024|1280) MiB, prefaulted' "$work/ramauto.log" &&
       grep -Eq 'memory: the guest.s RAM is held: [0-9]+ MiB prefaulted in [0-9]+ ms, (locked \(mlock|not locked \(mlock: .*so not prefaulted)' "$work/ramauto.log" &&
       grep -q 'TESTKERNEL: all passed' "$work/ramauto.log"; }; then
  grep -E 'memory|FAIL|TESTKERNEL' "$work/ramauto.log" | tail -8
  echo "check-arm64: ram=auto failed"
  exit 1
fi

echo "check-arm64: a guest that keeps restarting: xnuhost starts again in place 8 times, then exit 3"
# shellcheck disable=SC2086
XNUHOST_RUNNER="$run" timeout 60 $run "$xnuhost" $common --cpus 1 --ram 256M --boot-args testkernel-restart > "$work/restart.log" 2>&1
code=$?
starts=$(grep -c 'TESTKERNEL: restart' "$work/restart.log")
if [ "$code" != 3 ] || [ "$starts" != 9 ]; then
  tail -5 "$work/restart.log"
  echo "check-arm64: restart: exit $code"
  exit 1
fi

echo "check-arm64: last words: SIGUSR1 from outside to a running xnuhost: its fatal signal line, then it dies of SIGUSR1"
# (SIGUSR1, not SIGABRT: qemu-aarch64 11 ends itself on a SIGABRT from outside, before the guest's handler runs.
# An abort inside xnuhost and SIGUSR1 take the same handler: cpu.rs LAST_WORDS.)
rm -f "$work/testdata/data.img"
# Not under timeout(1): the signal goes to xnuhost (or the qemu that runs it) itself. The hold is ended below.
# shellcheck disable=SC2086
$run "$xnuhost" $common --cpus 1 --ram 256M --boot-args testkernel-hold > "$work/lastwords.log" 2>&1 &
pid=$!
n=0
while [ $n -lt 600 ] && kill -0 "$pid" 2>/dev/null && ! grep -q 'TESTKERNEL: holding' "$work/lastwords.log"; do
  sleep 0.1; n=$((n + 1))
done
if ! grep -q 'TESTKERNEL: holding' "$work/lastwords.log"; then
  kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  tail -5 "$work/lastwords.log"
  echo "check-arm64: last words: the test kernel did not get to its hold"
  exit 1
fi
kill -USR1 "$pid"
wait "$pid"
code=$?
if [ "$code" != 138 ] ||
   ! grep -Eq "^xnuhost !! fatal signal 10 \(SIGUSR1\) on thread '[^']*' \(tid [0-9]+\): code 0, sent by pid $$ uid [0-9]+$" "$work/lastwords.log"; then
  grep -E 'xnuhost !!|TESTKERNEL' "$work/lastwords.log" | tail -8
  echo "check-arm64: last words: status $code (138: killed by SIGUSR1), or no fatal signal line"
  exit 1
fi
grep 'fatal signal' "$work/lastwords.log"

echo "check-arm64: the trim: a task idle 1 s (task-trim-idle=1) loses its host mappings; its next run faults and is filled again"
rm -f "$work/testdata/data.img"
# shellcheck disable=SC2086
timeout 60 $run "$xnuhost" $common --cpus 1 --ram 256M --set ram-giveback=1 --set task-trim-idle=1 \
  --boot-args testkernel-trim > "$work/trim.log" 2>&1
if ! { grep -q 'TESTKERNEL: all passed' "$work/trim.log" &&
       grep -Eq 'tasks: trim: the first: pid [0-9]+ after [0-9]+ s with no release' "$work/trim.log"; }; then
  grep -E 'trim|FAIL|TESTKERNEL' "$work/trim.log" | tail -8
  echo "check-arm64: the trim failed"
  exit 1
fi
grep -E 'tasks: trim: the first|PASS trim' "$work/trim.log"
echo "check-arm64: all five boots passed"
