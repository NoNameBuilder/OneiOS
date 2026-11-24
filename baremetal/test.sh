#!/bin/sh
# Build and run the handoff harness.
#
# A small EL1 image ("the host", standing in for Linux) turns the MMU and the
# caches on, builds the boot_args, writes a magic into the window with the
# caches on, and calls the stub like a module would. The stub drops the MMU and
# the caches and branches into the payload, which checks every promise and
# switches QEMU off.
#
#   baremetal/test.sh            build, run, assert the payload's PASS line
#   baremetal/test.sh --keep     also keep out/ when it fails (it always does)
#
# Needs aarch64-linux-gnu-gcc, aarch64-linux-gnu-ld and qemu-system-aarch64.
# Nothing here touches a phone.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
out=${OUT:-"$here/out"}
cc=${CC:-aarch64-linux-gnu-gcc}
ld=${LD:-aarch64-linux-gnu-ld}
qemu=${QEMU:-qemu-system-aarch64}

for tool in "$cc" "$ld" "$qemu"; do
    command -v "$tool" >/dev/null 2>&1 || { echo "baremetal: missing $tool" >&2; exit 1; }
done

mkdir -p "$out"
flags="-c -nostdlib -ffreestanding -fno-pic -fno-stack-protector"
"$cc" $flags -o "$out/entry.o" "$here/stub/entry.S"
"$cc" $flags -I"$here/stub" -o "$out/host.o" "$here/harness/host.S"
"$cc" $flags -I"$here/stub" -o "$out/payload.o" "$here/harness/payload.S"
"$ld" -T "$here/harness/link.ld" -o "$out/pa1q-handoff.elf" \
    "$out/host.o" "$out/payload.o" "$out/entry.o"

# -smp 1 is the contract's "one core running"; QEMU parks nothing else for us,
# so parking is the injector's job, not the stub's.
set +e
timeout 30 "$qemu" -M virt -cpu cortex-a76 -m 1024 -smp 1 -nographic -no-reboot \
    -kernel "$out/pa1q-handoff.elf" >"$out/serial.log" 2>&1
status=$?
set -e

if grep -q "PA1Q PASS" "$out/serial.log"; then
    grep "PA1Q" "$out/serial.log"
    echo "baremetal: pass (qemu exit $status)"
else
    cat "$out/serial.log"
    echo "baremetal: FAIL (qemu exit $status); log in $out/serial.log" >&2
    exit 1
fi
