#!/bin/bash
# Host tests of runtime/package.sh: the package it makes, and what it must refuse. Needs the NDK (see package.sh).
#   runtime/package/test.sh
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
export OUT_DIR=$T/out KERNEL_DIR=$T/no-kernel
V=0.0.0-test
fail() { echo "FAIL: $*" >&2; exit 1; }
pass=0
if command -v sha256sum >/dev/null; then SHA256=(sha256sum); else SHA256=(shasum -a 256); fi
ok() { pass=$((pass + 1)); echo "ok $pass: $*"; }
# The NDK, found as package.sh finds it (runtime/ndk-cc links the faulty library of the refusals with its clang).
if [ "$(uname -s)" = Darwin ]; then SDK_DEFAULT=$HOME/Library/Android/sdk; else SDK_DEFAULT=$HOME/Android/Sdk; fi
export NDK=${NDK:-${ANDROID_NDK_ROOT:-${ANDROID_SDK_ROOT:-$SDK_DEFAULT}/ndk/28.2.13676358}}
[ -d "$HOME/.cargo/bin" ] && PATH=$HOME/.cargo/bin:$PATH

# runtime/build.sh refuses a stage without external/virglrenderer; a tree without that checkout (a fresh worktree) tests
# the package with the 2D-only xnuhost, on purpose. CI's release job fetches the checkout and checks it is there.
[ -f "$ROOT/external/virglrenderer/src/drm/drm2kgsl/drm2kgsl_renderer.c" ] || export XNUHOST_2D_ONLY=1

# 1. A plain package: verifies, runtime.json first, manifest equals the content.
"$ROOT/runtime/package.sh" $V > "$T/log" 2>&1 || { cat "$T/log"; fail "package.sh"; }
P=$OUT_DIR/oneios-runtime-$V-arm64.tar.zst
( cd "$OUT_DIR" && "${SHA256[@]}" -c "$(basename "$P").sha256" > /dev/null ) || fail "sha256 file"
[ "$(wc -l < "$P.sha256" | tr -d ' ')" = 1 ] || fail ".sha256 is not one line"
[ "$(zstd -dc "$P" | tar -t | head -1)" = runtime.json ] || fail "runtime.json is not the first member"
mkdir "$T/x" && zstd -dc "$P" | tar -x -C "$T/x"
# The manifest (oneios-tool json: runtime/package.sh builds or finds the tool the same way, ONEIOS_TOOL=PATH another).
. "$ROOT/scripts/lib/oneios-tool.sh"
oneios_tool "$ROOT"
J=("$ONEIOS_TOOL" json)
M=$T/x/runtime.json
manifest_check() {
  local path size sha mode want on_disk listed
  [ "$("${J[@]}" get "$M" format)/$("${J[@]}" get "$M" name)/$("${J[@]}" get "$M" version)/$("${J[@]}" get "$M" arch)/$("${J[@]}" get "$M" session_abi)" = "1/oneios-runtime/$V/arm64/1" ] || { echo "format, name, version, arch, session_abi: $(cat "$M")"; return 1; }
  [ "$("${J[@]}" get "$M" xnuhost)" = bin/xnuhost ] || { echo "xnuhost"; return 1; }
  [ "$("${J[@]}" get "$M" git.commit | tr -d '\n' | wc -c | tr -d ' ')" = 40 ] || { echo "git.commit"; return 1; }
  [ -n "$("${J[@]}" get "$M" min_oneos_fork)" ] || { echo "min_oneos_fork"; return 1; }
  listed=$("${J[@]}" lines "$M" files path | LC_ALL=C sort)
  on_disk=$(cd "$T/x" && find . ! -type d ! -path ./runtime.json | sed 's#^\./##' | LC_ALL=C sort)
  [ "$listed" = "$on_disk" ] || { echo "listed and on disk differ:"; diff <(echo "$listed") <(echo "$on_disk"); return 1; }
  while IFS="$(printf '\t')" read -r path size sha mode; do
    [ "$(wc -c < "$T/x/$path" | tr -d ' ')" = "$size" ] && [ "$("${SHA256[@]}" "$T/x/$path" | cut -d' ' -f1)" = "$sha" ] || { echo "size or sha256: $path"; return 1; }
    if [ -x "$T/x/$path" ]; then want=0755; else want=0644; fi
    [ "$mode" = "$want" ] || { echo "mode $mode, not $want: $path"; return 1; }
  done < <("${J[@]}" lines "$M" files path size sha256 mode)
  # the phone checks the OneOS fork's Hardware checks screen runs (runtime/build.sh stages them)
  for tool in xnuhost gpuctl bridgectl wifictl brightctl displayctl camctl oneios-camera oneios-audio oneios-display oneios-encoder encctl; do
    [ "$("${J[@]}" get "$M" "files[path=bin/$tool].mode")" = 0755 ] || { echo "bin/$tool is not 0755"; return 1; }
  done
}
manifest_check || fail "manifest does not match the content"
ok "package, checksum, member order, manifest"
zstd -dc "$P" | tar -tv | awk '$1 !~ /^-/ {bad=1} END {exit bad}' || fail "a member is not a regular file"
ok "regular files only"
# The phone's oneios-tool: in the package (0755, listed in runtime.json), and beside it with its .sha256, the same bytes.
TA=$OUT_DIR/oneios-tool-$V-android-arm64
[ -x "$T/x/bin/oneios-tool" ] || fail "no bin/oneios-tool in the package"
grep -q '"path": "bin/oneios-tool"' "$T/x/runtime.json" || fail "bin/oneios-tool is not in runtime.json"
( cd "$OUT_DIR" && "${SHA256[@]}" -c "$(basename "$TA").sha256" > /dev/null ) || fail "oneios-tool's .sha256"
cmp -s "$TA" "$T/x/bin/oneios-tool" || fail "oneios-tool beside the package is not the package's bin/oneios-tool"
[ "$(od -An -tx1 -N20 "$TA" | tr -d ' \n')" = 7f454c460201010000000000000000000300b700 ] || fail "oneios-tool is not an arm64 PIE"
ok "bin/oneios-tool and $(basename "$TA") ($(wc -c < "$TA" | tr -d ' ') bytes, stripped)"
cmp -s "$T/x/lib/profiles/ios-27.0.1-iPhone18-2.toml" "$ROOT/profiles/ios-27.0.1-iPhone18,2.toml" || fail "lib/profiles/ is not profiles/"
grep -q '"path": "lib/profiles/ios-27.0.1-iPhone18-2.toml"' "$T/x/runtime.json" || fail "lib/profiles/ is not in runtime.json"
# the Galaxy A56's iOS 16.5 profile (docs/a56.md): the OneOS fork's Make iOS image sends it on an A56
cmp -s "$T/x/lib/profiles/ios-16.5-a56.toml" "$ROOT/profiles/ios-16.5-a56.toml" || fail "lib/profiles/ has no A56 profile"
ok "lib/profiles/: the IPSW profiles for the phone's fetch"
# Its dynamic ELF files were checked against the NDK's API 30 stubs (elf-check.sh), twice: runtime/build.sh on its
# stage, then package.sh on the package's content.
[ "$(grep -c '^elf-check: .* 0 failures$' "$T/log")" = 2 ] || { cat "$T/log"; fail "package.sh did not run elf-check.sh twice"; }
grep -q '^elf-check: bin/liboneios-display.so: NEEDED .*libnativewindow.so' "$T/log" || { cat "$T/log"; fail "bin/liboneios-display.so was not checked"; }
ok "NEEDED and symbol check: $(grep -m1 '^elf-check: .* 0 failures$' "$T/log" | sed 's/^elf-check: //')"

# 2. The same inputs give the same bytes.
first=$(cut -d' ' -f1 "$P.sha256")
"$ROOT/runtime/package.sh" $V > "$T/log" 2>&1 || fail "second run"
[ "$first" = "$(cut -d' ' -f1 "$P.sha256")" ] || fail "not reproducible"
ok "reproducible"

# 3. Refusals: exit 3 and no package left behind for that version.
refuse() { # NAME, then a command that fills $T/extra
  local name=$1; shift
  rm -rf "$T/extra" "$OUT_DIR"; mkdir -p "$T/extra"
  "$@"
  local rc=0
  PACKAGE_EXTRA_DIR=$T/extra "$ROOT/runtime/package.sh" $V > "$T/log" 2>&1 || rc=$?
  [ "$rc" = 3 ] || { cat "$T/log"; fail "$name: exit $rc, expected the refusal (3)"; }
  [ ! -e "$OUT_DIR/oneios-runtime-$V-arm64.tar.zst" ] || fail "$name: a package was written"
  ok "refused: $name ($(grep -m1 'refused' "$T/log" | cut -c1-110))"
}
touch_file() { mkdir -p "$(dirname "$T/extra/$1")"; printf 'x' > "$T/extra/$1"; }
macho() { # PATH FILETYPE: a minimal arm64 Mach-O header (mach_header_64, little-endian), then 64 zero bytes
  mkdir -p "$(dirname "$T/extra/$1")"
  { printf '\317\372\355\376\014\000\000\001\002\000\000\000'; printf "\\$(printf %03o "$2")\\000\\000\\000"; dd if=/dev/zero bs=80 count=1 2> /dev/null; } > "$T/extra/$1"
}
record() { # PATH [KEY=JSON...]: a build record for the Mach-O, with fields replaced
  local r=$T/extra/$1.build.json pair
  printf '{"builder": "oneios-kernel", "build_id": "test-1", "sha256": "%s", "prelinked": false, "kernelcache_kexts_added": false, "apple_binary_inputs": [], "restricted_sources": [], "xnu": "xnu-0", "profile": "test"}\n' \
    "$("${SHA256[@]}" "$T/extra/$1" | cut -d' ' -f1)" > "$r"
  for pair in "${@:2}"; do "${J[@]}" set "$r" "${pair%%=*}" "${pair#*=}" || fail "record: $pair"; done
}
refuse "kernelcache by name" touch_file firmware/kernelcache.release.iphone18
refuse "a path the fork's unpacker rejects (comma)" touch_file notes,v2.txt
refuse ".ipsw" touch_file restore.ipsw
refuse ".dmg" touch_file os/090-12345-678.dmg
refuse ".aea" touch_file os.dmg.aea
refuse ".img" touch_file image/os.img
refuse "cryptex" touch_file cryptex1-systemos
im4p() { mkdir -p "$T/extra/bin"; printf '\x30\x83\x12\x34\x56\x16\x04IM4P\x16\x04krnl' > "$T/extra/bin/blob"; }
refuse "IM4P content under another name" im4p
m1() { macho kernel/xnu.macho 2; }
refuse "Mach-O without a build record" m1
m2() { macho kernel/xnu.macho 2; record kernel/xnu.macho 'kernelcache_kexts_added=true'; }
refuse "kernel with kernelcache kexts" m2
m3() { macho kernel/xnu.macho 2; record kernel/xnu.macho 'sha256="00"'; }
refuse "record for another file" m3
m4() { macho kernel/xnu.macho 12; record kernel/xnu.macho; }
refuse "kernel collection (MH_FILESET)" m4
m5() { macho kernel/xnu.macho 2; record kernel/xnu.macho 'builder="someone"'; }
refuse "record of another builder" m5
m6() { macho kernel/xnu.macho 2; record kernel/xnu.macho 'restricted_sources=["corecrypto-2023"]'; }
refuse "kernel that links a non-redistributable source" m6
link() { ln -s /etc/passwd "$T/extra/link"; }
refuse "symbolic link" link
# A library that Android cannot load, in the display helper's place: what liboneios-display.so of 1f7dd1f was.
# elf-fault.rs calls libnativewindow's dataspace functions and is linked against libandroid only, without -z defs.
faulty_library() {
  mkdir -p "$T/extra/bin"
  rustc --edition 2021 --target aarch64-linux-android -C opt-level=2 -C linker="$ROOT/runtime/ndk-cc" -A linker_messages \
    --crate-type cdylib "$ROOT/runtime/package/elf-fault.rs" -o "$T/extra/bin/liboneios-display.so"
}
refuse "a shared object with a symbol that none of its NEEDED libraries has" faulty_library
for symbol in ANativeWindow_getBuffersDataSpace ANativeWindow_setBuffersDataSpace; do
  grep -qxF "bin/liboneios-display.so: symbol $symbol is in none of its NEEDED libraries (NEEDED: libdl.so, libc.so); it is in libnativewindow.so" "$T/log" ||
    { cat "$T/log"; fail "the refusal does not name $symbol and libnativewindow.so"; }
done
ok "the refusal names it: $(grep -m1 -o 'symbol ANativeWindow_getBuffersDataSpace.*' "$T/log")"

# 4. Our own kernel with its record is accepted and named in the manifest.
rm -rf "$T/extra" "$OUT_DIR" "$T/k"; mkdir -p "$T/k" "$T/extra/kernel"
macho kernel/xnu.macho 2; record kernel/xnu.macho
mv "$T/extra/kernel/xnu.macho" "$T/extra/kernel/xnu.macho.build.json" "$T/k/"
KERNEL_DIR=$T/k "$ROOT/runtime/package.sh" $V > "$T/log" 2>&1 || { cat "$T/log"; fail "package with a kernel"; }
zstd -dc "$P" | tar -xO runtime.json > "$T/m.json"
[ "$("${J[@]}" get "$T/m.json" kernel.build_id)/$("${J[@]}" get "$T/m.json" kernel.path)" = test-1/kernel/xnu.macho ] || fail "kernel not in the manifest"
zstd -dc "$P" | tar -xO NOTICE | grep -q 'Apple Public Source License' || fail "no APSL notice"
ok "own kernel accepted, APSL notice present"

# 5. The boot addon: ONEIOS_GPUADDON's pair (an `ios addon` sidecar and its .cdhashes) is packed as kernel/gpuaddon
#    beside the kernel, listed in runtime.json; a file that is no boot addon and a list with no CDHash are refused.
rm -rf "$OUT_DIR" "$T/a"; mkdir -p "$T/a"
addon() { # PATH: the NGPUADD2 header as `ios addon` writes it (no files; the packager checks the magic, the kernel the entries)
  printf 'NGPUADD2\000\000\000\000\000\000\000\000\030\000\000\000\000\000\000\000' > "$1"
}
addon "$T/a/plug.gpuaddon"
printf '4baf7dbb8b7faf1312e87d5b81ea830b5578b3fd addons/NativeAdrenoMetal.bundle/NativeAdrenoMetal\n' > "$T/a/plug.gpuaddon.cdhashes"
KERNEL_DIR=$T/k ONEIOS_GPUADDON=$T/a/plug.gpuaddon "$ROOT/runtime/package.sh" $V > "$T/log" 2>&1 ||
  { cat "$T/log"; fail "package with a boot addon"; }
zstd -dc "$P" | tar -xO runtime.json > "$T/m.json"
[ "$("${J[@]}" get "$T/m.json" "files[path=kernel/gpuaddon].mode")" = 0644 ] || fail "kernel/gpuaddon is not in runtime.json (0644)"
[ "$("${J[@]}" get "$T/m.json" "files[path=kernel/gpuaddon.cdhashes].size")" = "$(wc -c < "$T/a/plug.gpuaddon.cdhashes" | tr -d ' ')" ] || fail "kernel/gpuaddon.cdhashes is not in runtime.json"
zstd -dc "$P" | tar -xO kernel/gpuaddon | cmp -s - "$T/a/plug.gpuaddon" || fail "kernel/gpuaddon is not the addon"
zstd -dc "$P" | tar -xO kernel/gpuaddon.cdhashes | cmp -s - "$T/a/plug.gpuaddon.cdhashes" || fail "the list is not the addon's"
grep -q '^package: boot addon 24 bytes, 1 CDHashes (kernel/gpuaddon)$' "$T/log" || { cat "$T/log"; fail "the addon line"; }
ok "boot addon packed as kernel/gpuaddon with its CDHashes"
addon_refuse() { # NAME EXPECTED: ONEIOS_GPUADDON set by the caller; exit 1, that refusal, and no package
  local name=$1 expected=$2 rc=0
  rm -rf "$OUT_DIR"
  KERNEL_DIR=$T/k ONEIOS_GPUADDON=$T/a/plug.gpuaddon "$ROOT/runtime/package.sh" $V > "$T/log" 2>&1 || rc=$?
  [ "$rc" = 1 ] || { cat "$T/log"; fail "$name: exit $rc, expected 1"; }
  [ ! -e "$OUT_DIR/oneios-runtime-$V-arm64.tar.zst" ] || fail "$name: a package was written"
  grep -q "^package: .*$expected" "$T/log" || { cat "$T/log"; fail "$name: the refusal does not say '$expected'"; }
  ok "refused: $name ($(grep -m1 "^package: .*$expected" "$T/log" | cut -c1-100))"
}
printf 'not an addon' > "$T/a/plug.gpuaddon"
addon_refuse "a file that is no boot addon" "is not a boot addon (NGPUADD2)"
addon "$T/a/plug.gpuaddon"
printf '# no CDHash here\n' > "$T/a/plug.gpuaddon.cdhashes"
addon_refuse "a CDHash list with no CDHash" "lists no CDHash"
echo "all $pass tests passed"
