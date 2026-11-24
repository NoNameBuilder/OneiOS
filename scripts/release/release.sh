#!/bin/bash
# Build, check and publish a OneiOS release from this machine, without waiting for GitHub Actions: the runtime
# package, the phone's oneios-tool and the OneOS APK, the same files and checks as .github/workflows/release.yml.
#   scripts/release/release.sh VERSION [--build-only] [--runtime-only | --apk-only] [--skip-tests] [--metal-dir DIR | --no-metal]
#            [--metal-amd-dir DIR | --no-metal-amd]
#   scripts/bg.sh release scripts/release/release.sh 0.1.14     (it takes 10-20 minutes: run it in tmux)
# VERSION is N.N.N or N.N.N-suffix; the release is the tag vVERSION, made at HEAD (which must be on origin/main).
# Three lanes run at the same time, each with its log in out/release-local/VERSION/logs/:
#   runtime  the Metal plug-in and the capture plug-in built from HEAD (userland/metal/build.sh with METAL_OUT
#            OUT/guest, its work in out/metal, about 7 minutes after the first; userland/camera/build.sh OUT/guest/camera),
#            then runtime/package.sh VERSION with them as lib/guest/ (METAL_DIR, CAMERA_DIR, REQUIRE_METAL=1) and the
#            kernel pinned in kernel/release-kernel.txt (downloaded and checked as
#            CI does), then the package's checks: drm2kgsl in xnuhost and gpuctl, elf-check.sh, the members, the Metal
#            bundle in runtime.json, and .github/scripts/check-phone-tool.sh;
#            With them, the Galaxy A56's GPU plug-ins (docs/a56-gpu.md 6): userland/metal-amd/build.sh all and
#            air/build.sh plugin with METAL_AMD_OUT OUT/guest-a56 (Mesa radeonsi, the Metal layer, the OpenGL ES layer; about
#            10 minutes the first time, 1.5 GB), as lib/guest/OneiOSRadeonMetal.bundle and OneiOSRadeonGL.bundle beside the
#            Adreno plug-in (METAL_AMD_DIR, REQUIRE_METAL_AMD=1), runtime.json "metal_a56";
#   apk      app/ as it is checked out (its commit must be on the fork's origin/oneios): the guest app, then
#            ./gradlew test assembleFullDebug signed with secrets/oneios-debug.keystore, .github/scripts/check-apk.sh,
#            and the APK's signer must be that key;
#   tests    .github/scripts/check-tracked-files.sh and the host tests CI runs (each in its own temporary folder).
# Then every .sha256 is checked, the notes are written and the release is published: a draft with every asset first,
# then made public, which creates the tag. The tag's own CI run builds and tests again, and its release job leaves a
# release that exists alone (the plan job's "published from a machine" check), so nothing is uploaded twice.
# --build-only builds and checks, and publishes nothing. --runtime-only and --apk-only are CI's "runtime only" and
# "apk only" (an APK-only release is not marked latest). --skip-tests leaves out the tests lane.
# --metal-dir DIR takes a Metal plug-in built before (userland/metal/build.sh's output folder, with camera/ holding
# H9ISP.mediacapture when the capture plug-in goes too) instead of building one; --no-metal makes a runtime without
# them (the phone's Make iOS image then composites on the CPU; the notes say so). The Metal build needs the iOS cross
# toolchain and the guest's stubs (scripts/setup/ios-toolchain.sh says "ok"; docs/building.md section 9).
# --metal-amd-dir DIR takes the A56's GPU plug-ins built before (userland/metal-amd's output folder, out/metal-amd:
# OneiOSRadeonMetal.bundle, gl/OneiOSRadeonGL.bundle, libdrm-2.4.133/) instead of building them; --no-metal-amd leaves
# them out (an A56 then stays on the 2D path: docs/a56-gpu.md 6). Their build needs out/ios16-tbd (docs/building.md,
# "iOS 16.5 stubs"), external/mesa-droidvm and external/Jesvi-iOS.
# The debug key: secrets/oneios-debug.keystore (git-ignored; store and key password "android", alias
# androiddebugkey), the same key as the secret ONEIOS_DEBUG_KEYSTORE_B64 of akshaynexus/OneiOS and akshaynexus/OneOS,
# so an APK from here and one from CI install over each other (docs/releases.md, "The debug key").
# Needs: gh (logged in), git, zstd, keytool, the Android SDK's build-tools 37.0.0 (ANDROID_HOME, or ~/Android/Sdk),
# the NDK runtime/package.sh pins, Rust with aarch64-linux-android, JDK 21, external/virglrenderer
# (scripts/setup/get-references.sh virglrenderer).
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$ROOT"
REPO=akshaynexus/OneiOS
FORK_REPO=akshaynexus/OneOS
KEYSTORE=$ROOT/secrets/oneios-debug.keystore
die() { echo "release: $*" >&2; exit 1; }

VERSION= PUBLISH=1 RUNTIME=1 APK=1 TESTS=1 METAL=build METAL_FROM= METAL_AMD=build METAL_AMD_FROM=
want_dir=0 want_amd=0
for a in "$@"; do
  if [ "$want_dir" = 1 ]; then METAL=dir METAL_FROM=$a want_dir=0; continue; fi
  if [ "$want_amd" = 1 ]; then METAL_AMD=dir METAL_AMD_FROM=$a want_amd=0; continue; fi
  case $a in
  --metal-dir) want_dir=1 ;;
  --no-metal) METAL=none ;;
  --metal-amd-dir) want_amd=1 ;;
  --no-metal-amd) METAL_AMD=none ;;
  --build-only) PUBLISH=0 ;;
  --runtime-only) APK=0 ;;
  --apk-only) RUNTIME=0 ;;
  --skip-tests) TESTS=0 ;;
  -*) die "unknown option $a" ;;
  *) [ -z "$VERSION" ] || die "one VERSION only"; VERSION=${a#v} ;;
  esac
done
[ "$want_dir" = 0 ] || die "--metal-dir needs a folder"
[ "$want_amd" = 0 ] || die "--metal-amd-dir needs a folder"
if [ "$METAL_AMD" = dir ]; then
  METAL_AMD_FROM=$(cd "$METAL_AMD_FROM" 2>/dev/null && pwd) || die "--metal-amd-dir: no such folder"
  [ -f "$METAL_AMD_FROM/OneiOSRadeonMetal.bundle/OneiOSRadeonMetal" ] && [ -f "$METAL_AMD_FROM/gl/OneiOSRadeonGL.bundle/OneiOSRadeonGL" ] \
    || die "--metal-amd-dir $METAL_AMD_FROM holds no OneiOSRadeonMetal.bundle and gl/OneiOSRadeonGL.bundle"
fi
if [ "$METAL" = dir ]; then
  METAL_FROM=$(cd "$METAL_FROM" 2>/dev/null && pwd) || die "--metal-dir: no such folder"
  [ -f "$METAL_FROM/NativeAdrenoMetal.bundle/NativeAdrenoMetal" ] || die "--metal-dir $METAL_FROM holds no NativeAdrenoMetal.bundle"
fi
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.]*)?$ ]] \
  || die "usage: scripts/release/release.sh VERSION [--build-only] [--runtime-only | --apk-only] [--skip-tests] [--metal-dir DIR | --no-metal] [--metal-amd-dir DIR | --no-metal-amd]"
[ "$RUNTIME$APK" != 00 ] || die "--runtime-only and --apk-only together build nothing"
TAG=v$VERSION

# --- What is released: HEAD of main as origin has it, the fork's commit as its origin has it.
for t in gh git zstd keytool sha256sum; do command -v "$t" > /dev/null || die "missing $t"; done
SHA=$(git rev-parse HEAD)
git diff --quiet HEAD -- . ':!app' || die "the checkout has uncommitted changes to tracked files: commit or stash them"
git fetch -q origin main
git merge-base --is-ancestor "$SHA" origin/main || die "HEAD $SHA is not on origin/main: push it first"
if [ "$PUBLISH" = 1 ]; then
  gh auth status > /dev/null 2>&1 || die "gh is not logged in"
  ! gh release view "$TAG" --repo "$REPO" > /dev/null 2>&1 || die "release $TAG exists already"
  ! git ls-remote --exit-code --tags origin "refs/tags/$TAG" > /dev/null || die "tag $TAG exists on origin already"
fi
SDK=
for d in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Android/Sdk" "$HOME/Library/Android/sdk"; do
  [ -n "$d" ] && [ -x "$d/build-tools/37.0.0/apksigner" ] && { SDK=$d; break; }
done
# The NDK runtime/package.sh pins (it finds it there itself; check-phone-tool.sh needs it named).
if [ -z "${ANDROID_NDK_ROOT:-}" ]; then
  for d in "$SDK" "$HOME/Android/Sdk" "$HOME/Library/Android/sdk"; do
    [ -n "$d" ] && [ -f "$d/ndk/28.2.13676358/source.properties" ] && { ANDROID_NDK_ROOT=$d/ndk/28.2.13676358; break; }
  done
fi
[ "$RUNTIME" = 0 ] || [ -n "${ANDROID_NDK_ROOT:-}" ] || die "no NDK 28.2.13676358 (set ANDROID_NDK_ROOT)"
export ANDROID_NDK_ROOT
# rustup's cargo first: the Metal bridge's build runs `cargo +1.98.0`, which only rustup's proxy takes. A run under
# tmux started from ssh had /usr/bin/cargo first (the distribution's, no rustup): v0.1.24's first run stopped in the
# runtime lane with "error: no such command: `+1.98.0`".
[ ! -x "$HOME/.cargo/bin/rustup" ] || export PATH="$HOME/.cargo/bin:$PATH"
if [ "$APK" = 1 ]; then
  [ -n "$SDK" ] || die "no Android SDK with build-tools 37.0.0 (set ANDROID_HOME)"
  # Gradle finds the SDK only by ANDROID_HOME or local.properties: a run under tmux started from ssh has neither
  # (v0.1.21's first run: "SDK location not found" in the guest's build).
  export ANDROID_HOME=$SDK
  [ -f "$KEYSTORE" ] || die "no $KEYSTORE (docs/releases.md, \"The debug key\")"
  FORK_BRANCH=$(git config -f .gitmodules submodule.app.branch)
  FORK_COMMIT=$(git -C app rev-parse HEAD)
  git -C app diff --quiet HEAD || die "app/ has uncommitted changes"
  git -C app fetch -q origin "$FORK_BRANCH"
  git -C app merge-base --is-ancestor "$FORK_COMMIT" FETCH_HEAD \
    || die "app/ at $FORK_COMMIT is not on the fork's origin/$FORK_BRANCH: push it first"
  KEY_SHA=$(keytool -list -v -keystore "$KEYSTORE" -storepass android -alias androiddebugkey \
    | sed -n 's/^[[:space:]]*SHA256: //p' | tr -d ':' | tr 'A-F' 'a-f')
  [ -n "$KEY_SHA" ] || die "$KEYSTORE has no androiddebugkey with the password android"
fi

OUT=$ROOT/out/release-local/$VERSION
LOGS=$OUT/logs
rm -rf "$OUT"; mkdir -p "$OUT/files" "$LOGS"
echo "release: $TAG at $SHA${FORK_COMMIT:+, app/ $FORK_COMMIT}; logs in $LOGS"

# --- The lanes.
lane_runtime() {
  local k=$ROOT/out/release-local/kernel
  KERNEL_DIR=$OUT/no-kernel
  if [ -s kernel/release-kernel.txt ]; then
    local ktag ksha f
    read -r ktag ksha < kernel/release-kernel.txt
    f=$(ls "$k/dl/$ktag"/xnu-*.tar.zst 2>/dev/null | head -n 1 || true)
    if [ -z "$f" ] || [ "$(sha256sum "$f" | cut -d' ' -f1)" != "$ksha" ]; then
      rm -rf "$k/dl/$ktag"; mkdir -p "$k/dl/$ktag"
      gh release download "$ktag" --repo "$REPO" --pattern 'xnu-*.tar.zst' --dir "$k/dl/$ktag"
      f=$(ls "$k/dl/$ktag"/xnu-*.tar.zst)
    fi
    echo "$ksha  $f" | sha256sum -c -
    KERNEL_DIR=$OUT/kernel; mkdir -p "$KERNEL_DIR"
    zstd -dc "$f" | tar -x -C "$KERNEL_DIR"
  else
    echo "no kernel pinned in kernel/release-kernel.txt: runtime only"
  fi
  test -f external/virglrenderer/src/drm/drm2kgsl/drm2kgsl_renderer.c \
    || { echo "no external/virglrenderer with drm2kgsl (scripts/setup/get-references.sh virglrenderer)"; return 1; }
  # The guest's plug-ins as lib/guest/ (the phone's Make iOS image seeds them): built from HEAD, or --metal-dir's.
  local guest=() g=$OUT/guest
  case $METAL in
    build)
      METAL_OUT=$g userland/metal/build.sh
      userland/camera/build.sh "$g/camera"
      guest=(METAL_DIR="$g" CAMERA_DIR="$g/camera" REQUIRE_METAL=1) ;;
    dir)
      guest=(METAL_DIR="$METAL_FROM" REQUIRE_METAL=1)
      [ ! -f "$METAL_FROM/camera/H9ISP.mediacapture" ] || guest+=(CAMERA_DIR="$METAL_FROM/camera") ;;
    none) echo "--no-metal: no Metal plug-in in this runtime" ;;
  esac
  # the Galaxy A56's GPU plug-ins beside the Adreno one (docs/a56-gpu.md 6): built from HEAD, or --metal-amd-dir's
  local ga=$OUT/guest-a56
  case $METAL_AMD in
    build)
      METAL_AMD_OUT=$ga userland/metal-amd/build.sh all
      METAL_AMD_OUT=$ga userland/metal-amd/air/build.sh plugin "$ga/build-ios"
      guest+=(METAL_AMD_DIR="$ga" REQUIRE_METAL_AMD=1) ;;
    dir) guest+=(METAL_AMD_DIR="$METAL_AMD_FROM" REQUIRE_METAL_AMD=1) ;;
    none) echo "--no-metal-amd: no A56 GPU plug-ins in this runtime" ;;
  esac
  env ${guest[@]+"${guest[@]}"} OUT_DIR="$OUT/files" KERNEL_DIR="$KERNEL_DIR" runtime/package.sh "$VERSION"
  local a=$OUT/files/oneios-runtime-$VERSION-arm64.tar.zst d=$OUT/runtime-unpacked b
  rm -rf "$d"; mkdir -p "$d"; zstd -dc "$a" | tar -x -C "$d"
  for b in bin/xnuhost bin/gpuctl; do
    if strings "$d/$b" | grep -q 'this build has no drm2kgsl'; then echo "$b was built without drm2kgsl"; return 1; fi
  done
  runtime/package/elf-check.sh "$d"
  zstd -dc "$a" | tar -t > "$OUT/members.txt"
  if grep -Ev '^(runtime\.json|NOTICE|LICENSE|(bin|lib|scripts)/[^/].*|kernel/xnu\.macho(\.build\.json)?)$' "$OUT/members.txt"; then
    echo "the package has members outside the runtime's folders (above)"; return 1
  fi
  [ "$(head -n 1 "$OUT/members.txt")" = runtime.json ] || { echo "runtime.json is not the first member"; return 1; }
  # the Metal bundle as runtime.json names it (`package manifest` checked its files against its record): for the notes
  if [ "$METAL" != none ]; then
    . scripts/lib/oneios-tool.sh && oneios_tool "$ROOT"
    "$ONEIOS_TOOL" json get "$d/runtime.json" metal.target > "$OUT/metal.txt" \
      || { echo "runtime.json names no Metal plug-in"; return 1; }
    "$ONEIOS_TOOL" json get "$d/runtime.json" metal.cdhashes >> "$OUT/metal.txt"
    "$ONEIOS_TOOL" json get --default none "$d/runtime.json" camera.cdhash >> "$OUT/metal.txt"
    echo "runtime: Metal plug-in for $(head -n 1 "$OUT/metal.txt"), capture plug-in $(tail -n 1 "$OUT/metal.txt")"
  fi
  if [ "$METAL_AMD" != none ]; then
    . scripts/lib/oneios-tool.sh && oneios_tool "$ROOT"
    "$ONEIOS_TOOL" json get "$d/runtime.json" metal_a56.target > "$OUT/metal-a56.txt" \
      || { echo "runtime.json names no A56 GPU plug-ins (metal_a56)"; return 1; }
    "$ONEIOS_TOOL" json get "$d/runtime.json" metal_a56.cdhashes >> "$OUT/metal-a56.txt"
    echo "runtime: the A56's GPU plug-ins for $(head -n 1 "$OUT/metal-a56.txt")"
  fi
  OUT_DIR=$OUT/files .github/scripts/check-phone-tool.sh "$VERSION"
  echo "runtime: $(wc -l < "$OUT/members.txt") members, kernel $( [ -f "$KERNEL_DIR/xnu.macho" ] && echo in || echo none)"
}

lane_apk() {
  local bt=$SDK/build-tools/37.0.0 src=app/app-android/app/build/outputs/apk/full/debug/app-full-debug.apk
  app/scripts/build-guest-manager.sh --debug
  rm -f "$src"
  ( cd app/app-android && ./gradlew --no-daemon --max-workers=3 test assembleFullDebug \
      -Pandroid.injected.signing.store.file="$KEYSTORE" -Pandroid.injected.signing.store.password=android \
      -Pandroid.injected.signing.key.alias=androiddebugkey -Pandroid.injected.signing.key.password=android )
  [ -s "$src" ] || { echo "no $src"; return 1; }
  local badging vname vcode name signer
  badging=$("$bt/aapt2" dump badging "$src" | sed -n 1p)
  echo "$badging"
  vname=$(sed -n "s/.* versionName='\([^']*\)'.*/\1/p" <<< "$badging")
  vcode=$(sed -n "s/.* versionCode='\([0-9]*\)'.*/\1/p" <<< "$badging")
  [ -n "$vname" ] && [ -n "$vcode" ] || { echo "no versionName or versionCode in the APK"; return 1; }
  [ "$vcode" -gt 143 ] || { echo "versionCode $vcode did not grow past v0.1.10's 143"; return 1; }
  signer=$("$bt/apksigner" verify --print-certs "$src" | sed -n 's/^.*[Ss]igner[^:]*: certificate SHA-256 digest: //p; s/^Signer #1 certificate SHA-256 digest: //p' | sort -u)
  [ "$signer" = "$KEY_SHA" ] || { echo "the APK is signed by $signer, not by $KEYSTORE ($KEY_SHA)"; return 1; }
  name=OneOS-oneios-$vname-debug.apk
  cp "$src" "$OUT/files/$name"
  ( cd "$OUT/files" && sha256sum "$name" > "$name.sha256" )
  .github/scripts/check-apk.sh "$OUT/files/$name" app/app-android/packages
  printf '%s\n%s\n%s\n' "$name" "$vname" "$vcode" > "$OUT/apk.txt"
  echo "apk: $name, versionName $vname, versionCode $vcode, signer $signer"
}

lane_tests() {
  .github/scripts/check-tracked-files.sh
  git ls-files -z '*.sh' | xargs -0 -n1 bash -n
  if command -v shellcheck > /dev/null; then
    # shellcheck disable=SC2046
    shellcheck -S error $(git ls-files 'kernel/*.sh' 'runtime/*.sh' '.github/*.sh' 'scripts/release/*.sh')
  else
    echo "note: no shellcheck here (CI runs it)"
  fi
  runtime/package/elf-check-test.sh
  runtime/package/test.sh
  kernel/tests/host.sh
  local t
  for t in $(git ls-files '*/tests/host.sh' | grep -v '^kernel/'); do echo "== $t"; "$t"; done
}

declare -A PIDS=()
# Each lane with job control on (set -m): without it bash starts a background command with SIGINT and SIGQUIT
# ignored, every process of the lane inherits that, and the fork's OneiosScriptTest (xnuhost's stand-in must not ignore
# them) failed here and passes in CI's foreground steps (v0.1.20, 2026-11-23). lane_apk's Gradle runs with --no-daemon:
# a daemon left by such a run (or by anyone's background job) keeps the ignored signals, and its test workers inherit
# them (the second v0.1.20 run reused one). Stdin is /dev/null: a lane in its own process group never reads the
# terminal.
start() {
  local n=$1
  set -m
  ( set -euo pipefail; "lane_$n" ) < /dev/null > "$LOGS/$n.log" 2>&1 &
  PIDS[$n]=$!
  set +m
  echo "release: $n started (log $LOGS/$n.log)"
}
[ "$RUNTIME" = 0 ] || start runtime
[ "$APK" = 0 ] || start apk
[ "$TESTS" = 0 ] || start tests
trap 'kill "${PIDS[@]}" 2>/dev/null || true' INT TERM
failed=()
for n in "${!PIDS[@]}"; do
  if wait "${PIDS[$n]}"; then echo "release: $n ok"; else failed+=("$n"); echo "release: $n FAILED, the end of $LOGS/$n.log:"; tail -n 25 "$LOGS/$n.log" | sed 's/^/    /'; fi
done
[ ${#failed[@]} = 0 ] || die "failed: ${failed[*]}: nothing was published"

# --- The files and their checksums.
cd "$OUT/files"
files=()
if [ "$RUNTIME" = 1 ]; then
  files+=("oneios-runtime-$VERSION-arm64.tar.zst" "oneios-runtime-$VERSION-arm64.tar.zst.sha256"
          "oneios-tool-$VERSION-android-arm64" "oneios-tool-$VERSION-android-arm64.sha256")
fi
if [ "$APK" = 1 ]; then
  { read -r APK_FILE; read -r APK_NAME; read -r APK_CODE; } < "$OUT/apk.txt"
  files+=("$APK_FILE" "$APK_FILE.sha256")
fi
for f in "${files[@]}"; do case $f in *.sha256) sha256sum -c "$f" ;; esac; done
ls -l "${files[@]}"
cd "$ROOT"

# --- The notes, as CI writes them.
{
  echo "Built on a build machine by \`scripts/release/release.sh\` from OneiOS commit \`$SHA\`."
  echo
  if [ "$RUNTIME" = 1 ]; then
    echo "- **Runtime** \`oneios-runtime-$VERSION-arm64.tar.zst\` (+ \`.sha256\`): the runtime package for the OneOS fork (branch \`oneios\`). OneiOS's own code only: no Apple firmware; the iOS image set is built locally. See [docs/releases.md](https://github.com/$REPO/blob/$SHA/docs/releases.md)."
    echo "- **oneios-tool for the phone** \`oneios-tool-$VERSION-android-arm64\` (+ \`.sha256\`): the package's \`bin/oneios-tool\` by itself (OneiOS's Rust tools for Android arm64, stripped), which the OneOS app runs as root from the staged runtime."
    if [ -s "$OUT/metal.txt" ]; then
      echo "- **Metal plug-in** in the runtime (\`lib/guest/\`, built from source; its licences and sources in \`lib/guest/NOTICE\`): NativeAdrenoMetal for \`$(sed -n 1p "$OUT/metal.txt")\`, CDHashes \`$(sed -n 2p "$OUT/metal.txt")\`; the capture plug-in \`$(sed -n 3p "$OUT/metal.txt")\`. An image made on the phone with this runtime (Make iOS image) has them in its Data seed and their CDHashes in its owner trust list, so iOS composites on the GPU."
    else
      echo "- **No Metal plug-in** in this runtime (built with \`--no-metal\`): an image made on the phone with it composites on the CPU."
    fi
    if [ -s "$OUT/metal-a56.txt" ]; then
      echo "- **The Galaxy A56's GPU plug-ins** in the runtime (\`lib/guest/OneiOSRadeonMetal.bundle\`, \`OneiOSRadeonGL.bundle\`, built from source; licences and sources in \`lib/guest/NOTICE\`) for \`$(sed -n 1p "$OUT/metal-a56.txt")\`, CDHashes \`$(sed -n 2p "$OUT/metal-a56.txt")\`. On an A56 with an iOS 16.5 image, Update the kernel puts them in the image as its boot addon (\`kernel.kc.gpuaddon\`, their CDHashes in its owner trust list); xnuhost turns its GPU path on when its self-test passes ([docs/a56-gpu.md](https://github.com/$REPO/blob/$SHA/docs/a56-gpu.md) section 6). Not booted before this release."
    fi
  else
    echo "- **No runtime** in this release (built as \"apk only\"): it is not marked latest, so the app's runtime download keeps the latest release that has one."
  fi
  if [ "$APK" = 1 ]; then
    echo "- **OneOS app** \`$APK_FILE\` (+ \`.sha256\`, SHA-256 \`$(cut -d' ' -f1 "$OUT/files/$APK_FILE.sha256")\`): versionName **$APK_NAME**, versionCode **$APK_CODE**, the full edition's debug build of [$FORK_REPO](https://github.com/$FORK_REPO) branch \`$FORK_BRANCH\` at commit [\`$FORK_COMMIT\`](https://github.com/$FORK_REPO/commit/$FORK_COMMIT). Signed with the OneiOS debug key (certificate SHA-256 \`$KEY_SHA\`), the same key CI signs with: it installs over any APK of a release signed with it. It carries no system image and no package set (checked by \`.github/scripts/check-apk.sh\`): the Android sets are downloaded by the app, the iOS image set is one you build yourself."
  else
    echo "- **No APK** in this release (built as \"runtime only\")."
  fi
} > "$OUT/notes.md"
cat "$OUT/notes.md"

if [ "$PUBLISH" = 0 ]; then
  echo "release: built and checked in $OUT/files (--build-only: nothing published)"
  exit 0
fi

# --- Publish: a draft with every asset, then public (which makes the tag at $SHA).
flags=()
[[ $VERSION == *-* ]] && flags+=(--prerelease)
[ "$RUNTIME" = 1 ] || flags+=(--latest=false)
if [ "$RUNTIME" = 1 ]; then title="OneiOS runtime $VERSION"; else title="OneiOS $VERSION"; fi
[ "$APK" = 0 ] || title="$title, OneOS $APK_NAME"
paths=(); for f in "${files[@]}"; do paths+=("$OUT/files/$f"); done
gh release create "$TAG" --repo "$REPO" --draft --target "$SHA" --title "$title" "${flags[@]}" \
  --notes-file "$OUT/notes.md" "${paths[@]}"
gh release edit "$TAG" --repo "$REPO" --draft=false
have=$(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[] | "\(.name) \(.digest)"')
for f in "${files[@]}"; do
  want="sha256:$(sha256sum "$OUT/files/$f" | cut -d' ' -f1)"
  [ "$(awk -v n="$f" '$1 == n { print $2 }' <<< "$have")" = "$want" ] || die "$f on $TAG is not the file built here"
done
git fetch -q origin "refs/tags/$TAG:refs/tags/$TAG"
[ "$(git rev-parse "refs/tags/$TAG^{commit}")" = "$SHA" ] || die "tag $TAG is not at $SHA"
gh release view "$TAG" --repo "$REPO" --json url,assets --jq '.url, (.assets[] | "  \(.name) \(.size)")'
echo "release: $TAG published; its CI run builds and tests again and uploads nothing"
