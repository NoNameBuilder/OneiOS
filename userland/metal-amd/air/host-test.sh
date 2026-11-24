#!/bin/sh
# The A56 Metal layer's host test: Jesvi-iOS's AIR frontend and air_metal (userland/metal-amd/air) on Mesa cecc0e9 with
# patches/mesa/5*.patch, driven through air_metal.h alone by air_metal_test, on softpipe (no GPU), on this PC.
#
#   userland/metal-amd/air/host-test.sh       builds (build.sh host: out/metal-amd/host-build, about 4 minutes the first
#                                             time, then only what changed) and runs the checks; exit 0 when all pass
#   AIR_HOST_BUILD=DIR userland/metal-amd/air/host-test.sh   the checks against another build directory
#
# The inputs are hand-written LLVM IR (air/tests/*.ll: four of Jesvi-iOS's, tid_store.ll ours), assembled here with
# llvm-as; no Apple metallib, shared-cache extract or capture is read. The checks:
#   frontend      air2nir on phiconst.ll: the constant on the phi edge survives (`phi b1: ..., b2: ... (0xffff)`)
#   compute       a dispatch of tid_store.ll (4x4 threads) writes 5, 8, 11, 14, 17, 20 into its buffer
#   render        fragpos.ll drawn into an RGBA32Float target: pixel (0,0) is 0.5 0.5 0 1 (Metal's top-left origin)
#   argbufs       argbuf_set.ll's argument buffer table, and --draw-argbuf: the same picture through an
#                 MTLArgumentEncoder's records as through direct arguments, and every wrong record refused (20 checks)
#   surface       --surface: what a texture over an IOSurface is refused for, each answer named (softpipe imports none)
#   no-gpu        RWA_SOFTWARE=0: this build has no GPU screen (-Doneios-air-screen=none), and the device open says so,
#                 which is what the A56's plug-in logs when its Mesa has no radeonsi screen
# Needs: what build.sh host needs, and llvm-as.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
B=${AIR_HOST_BUILD:-${METAL_AMD_OUT:-$ROOT/out/metal-amd}/host-build}
command -v llvm-as >/dev/null || { echo "host-test: no llvm-as"; exit 1; }
if [ -z "${AIR_HOST_BUILD:-}" ]; then
  bash "$HERE/build.sh" host || { echo "host-test: FAIL: build.sh host"; exit 1; }
fi
TEST=$B/src/gallium/targets/air-metal/air_metal_test
AIR2NIR=$B/src/compiler/air/air2nir
[ -x "$TEST" ] && [ -x "$AIR2NIR" ] || { echo "host-test: FAIL: no $TEST or $AIR2NIR"; exit 1; }
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
for m in phiconst tid_store fragpos argbuf_set argbuf_direct; do
  llvm-as -o "$W/$m.bc" "$HERE/tests/$m.ll" || { echo "host-test: FAIL: llvm-as $m.ll"; exit 1; }
done

pass=0 fail=0
check() { # NAME OK(0|1) WHAT
  if [ "$2" = 0 ]; then pass=$((pass + 1)); echo "host-test: pass: $1: $3"
  else fail=$((fail + 1)); echo "host-test: FAIL: $1: $3"; fi
}
has() { grep -qE "$2" "$1"; echo $?; }

RWA_SOFTWARE=1 "$AIR2NIR" "$W/phiconst.bc" > "$W/phiconst.out" 2>&1
check frontend "$(has "$W/phiconst.out" 'phi b1: %[0-9]+, b2: %[0-9]+ \(0xffff\)')" "phiconst's edge constant is 0xffff, not undef"

RWA_SOFTWARE=1 "$TEST" "$W/tid_store.bc" > "$W/tid.out" 2>&1
check compute "$(has "$W/tid.out" 'buffer 0 was written; first six words 00000005 00000008 0000000b 0000000e 00000011 00000014')" \
  "tid_store wrote tid * 3 + 5"
check compute "$(has "$W/tid.out" 'execution width 32')" "threadExecutionWidth 32"

RWA_SOFTWARE=1 "$TEST" --draw "$W/fragpos.bc" fragPos 125 > "$W/fragpos.out" 2>&1
check render "$(grep -A4 'pixel at (0, 0) of attachment 0' "$W/fragpos.out" | tr -s ' \n' ' ' | grep -q 'came back as: 0.5 0.5 0 1'; echo $?)" \
  "fragPos at (0,0) is 0.5 0.5 0 1"

RWA_SOFTWARE=1 "$TEST" --argbufs "$W/argbuf_set.bc" > "$W/argbufs.out" 2>&1
check argbufs "$(has "$W/argbufs.out" 'encodedLength 40 +alignment 8 +5 members')" "argbuf_set declares 5 members, 40 bytes"
RWA_SOFTWARE=1 "$TEST" --draw-argbuf "$W/argbuf_set.bc" "$W/argbuf_direct.bc" > "$W/drawargbuf.out" 2>&1
rc=$?
check argbufs "$( [ $rc = 0 ] && has "$W/drawargbuf.out" '^draw-argbuf: 0 of [0-9]+ checks failed' || echo 1)" \
  "$(grep -E 'checks failed' "$W/drawargbuf.out" | sed 's/^draw-argbuf: //')"

RWA_SOFTWARE=1 "$TEST" --surface > "$W/surface.out" 2>&1
check surface "$(has "$W/surface.out" 'surface sweep: 0 answers wrong')" "every IOSurface refusal named"

RWA_SOFTWARE=0 "$TEST" "$W/tid_store.bc" > "$W/nogpu.out" 2>&1
check no-gpu "$(has "$W/nogpu.out" 'has no GPU screen in it')" "without radeonsi the device open fails with its sentence"

echo "host-test: $pass passed, $fail failed"
[ "$fail" = 0 ] || { for f in "$W"/*.out; do echo "---- $f"; tail -5 "$f"; done; exit 1; }
