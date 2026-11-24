#!/bin/sh
# Host tests of the A56's guest Mesa (userland/metal-amd, docs/a56-gpu.md section 3), on the Linux build laptop:
#
#   userland/metal-amd/host-test.sh       patches, the Linux build, the Darwin vdrm test (about 6 minutes the first time)
#   METAL_AMD_OUT=DIR ...                 another output folder (default out/metal-amd; this uses DIR/host)
#
# 1. patches   every userland/metal-amd/patches/mesa/*.patch applies, in name order, to a fresh clone of
#              external/mesa-droidvm at cecc0e9 (DIR/host/mesa; made again only when the patch set changes)
# 2. linux     the same Mesa builds for Linux with radeonsi and the amdgpu virtio context, ACO alone
#              (-Dllvm=disabled -Damd-use-llvm=false -Doneios-radeonsi-screen=true): liboneios_radeonsi.so, which must
#              export oneios_radeonsi_screen_create and nothing else, and oneios-amdgpu-probe (DIR/host/build)
# 3. vdrm      tests/vdrm_darwin_test.c: the Darwin vdrm backend (vdrm.c, vdrm_darwin.c) and Mesa's amdgpu virtio
#              context (amdgpu_virtio*.c, with HAVE_VDRM_DARWIN) compiled for Linux, the IOKit calls a fake of the guest
#              kernel's user client that keeps and checks every record (open type, tunnel, capset 6, context, the K2
#              blob rules, fences on their rings, syncobj waits, closes)
# One line each at the end (pass or FAIL); the exit status is the number of FAILs. Needs: the Linux libdrm (2.4.133 or
# newer, with libdrm_amdgpu), meson, ninja, bison, flex, python3 with mako and PyYAML (Mesa's build), cc.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT" || exit 1
HERE=$ROOT/userland/metal-amd
OUT=${METAL_AMD_OUT:-$ROOT/out/metal-amd}/host
EXT=${ONEIOS_EXTERNAL:-$ROOT/external}
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN)}
MESA_PIN=cecc0e96d7193c72356a31abb0cc589e4fc0415e
SRC=$OUT/mesa
BUILD=$OUT/build
mkdir -p "$OUT"
results=""
fails=0
result() { results="$results$1 $2
"; [ "$2" = pass ] || fails=$((fails + 1)); }

patches() {
  want=$(ls "$HERE"/patches/mesa/*.patch | LC_ALL=C sort | while read -r p; do printf '%s ' "$(basename "$p")"; cksum < "$p"; done)
  if [ -d "$SRC/.git" ] && [ "$(cat "$OUT/mesa.patches" 2>/dev/null)" = "$want" ]; then
    echo "patches: $SRC has this patch set already"
    return 0
  fi
  rm -rf "${SRC:?}" "${BUILD:?}"
  git clone -q "$EXT/mesa-droidvm" "$SRC" && git -C "$SRC" checkout -q "$MESA_PIN" || return 1
  for p in $(ls "$HERE"/patches/mesa/*.patch | LC_ALL=C sort); do
    git -C "$SRC" apply --check "$p" && git -C "$SRC" apply "$p" || { echo "patches: $(basename "$p") does not apply"; return 1; }
    echo "patches: $(basename "$p") applies"
  done
  printf '%s\n' "$want" > "$OUT/mesa.patches"
}

linux() {
  if [ ! -f "$BUILD/build.ninja" ]; then
    meson setup "$BUILD" "$SRC" --wrap-mode=nodownload -Dbuildtype=debugoptimized \
      -Dgallium-drivers=radeonsi -Damdgpu-virtio=true -Dllvm=disabled -Damd-use-llvm=false -Doneios-radeonsi-screen=true \
      -Dvulkan-drivers= -Dplatforms= -Dglx=disabled -Degl=disabled -Dgbm=disabled -Dopengl=false -Dgles1=disabled \
      -Dgles2=disabled -Dxmlconfig=disabled -Dtools= -Dbuild-tests=false -Dvideo-codecs= -Dvalgrind=disabled \
      -Dlibunwind=disabled -Dlmsensors=disabled -Dzstd=disabled -Dexpat=disabled -Dgallium-va=disabled \
      -Dgallium-rusticl=false -Dshader-cache=disabled -Dvulkan-layers= -Dperfetto=false \
      > "$OUT/meson-setup.log" 2>&1 || { tail -20 "$OUT/meson-setup.log"; return 1; }
  fi
  t=src/gallium/targets/oneios-radeonsi
  ninja -C "$BUILD" -j "$JOBS" $t/liboneios_radeonsi.so $t/oneios-amdgpu-probe src/util/libmesa_util.a \
    > "$OUT/ninja.log" 2>&1 || { grep -m 20 -E "error|undefined" "$OUT/ninja.log"; return 1; }
  exports=$(nm -D --defined-only "$BUILD/$t/liboneios_radeonsi.so" | awk '$2 == "T" { print $3 }')
  [ "$exports" = oneios_radeonsi_screen_create ] || { echo "linux: liboneios_radeonsi.so exports: $exports"; return 1; }
  echo "linux: liboneios_radeonsi.so exports oneios_radeonsi_screen_create alone; oneios-amdgpu-probe built"
}

vdrm() {
  # The flags Mesa's own build gives amdgpu_virtio_device.c (its defines and include folders), then the Darwin backend.
  obj=src/amd/common/libamd_common.a.p/virtio_amdgpu_virtio_device.c.o
  cmd=$(cd "$BUILD" && ninja -t commands "$obj" | tail -1)
  flags=$(printf '%s\n' "$cmd" | tr ' ' '\n' | grep -E '^-(I|D|std=|f)' | grep -v -e '-fdiagnostics' | sed "s|^-I\([^/]\)|-I$BUILD/\1|")
  t=$OUT/vdrm-test
  mkdir -p "$t"
  # shellcheck disable=SC2086
  cc -o "$t/vdrm_darwin_test" -g -O1 -Wall -Wno-unused-parameter -Werror=implicit-function-declaration \
    $flags -DHAVE_VDRM_DARWIN -I"$HERE/tests/fake-iokit" -I"$SRC/src" -I"$SRC/src/amd/common" -I"$SRC/src/virtio" \
    "$HERE/tests/vdrm_darwin_test.c" "$SRC/src/virtio/vdrm/vdrm.c" "$SRC/src/virtio/vdrm/vdrm_darwin.c" \
    "$SRC/src/amd/common/virtio/amdgpu_virtio.c" "$SRC/src/amd/common/virtio/amdgpu_virtio_bo.c" \
    "$SRC/src/amd/common/virtio/amdgpu_virtio_device.c" \
    "$BUILD/src/util/libmesa_util.a" "$BUILD/src/c11/impl/libmesa_util_c11.a" \
    $(pkg-config --libs libdrm_amdgpu libdrm) -lpthread -lm -ldl > "$t/build.log" 2>&1 || { head -30 "$t/build.log"; return 1; }
  "$t/vdrm_darwin_test"
}

echo "== patches"; if patches; then result patches pass; else result patches FAIL; fi
if [ "${fails}" = 0 ]; then
  echo "== linux"; if linux; then result linux pass; else result linux FAIL; fi
  echo "== vdrm"; if vdrm; then result vdrm pass; else result vdrm FAIL; fi
else
  result linux skipped; result vdrm skipped
fi
printf '%s' "$results"
exit "$fails"
