# OneiOS — Galaxy S25 (SM-S931B)

> iOS 27 next to One UI on a rooted Samsung Galaxy S25 (base model, **SM-S931B**, `pa1q`, SM8750) — switched to and
> back the way OneOS switches Androids. Nothing flashed, the image lives in a folder, One UI's kernel keeps running.

This is the base-S25 port fork of [akshaynexus/OneiOS](https://github.com/akshaynexus/OneiOS), pinned to EUX firmware
**`S931BXXUCDZIF`** (Android 17 / OS17, kernel **6.6.127-android15-8**). Upstream's on-device results are from the
S24 Ultra; the base S25 has not been run on a phone yet. The exact device record is [docs/s25.md](docs/s25.md).

**Status: source-level port done and host-tested; no on-device run yet.** The S25 host row, the Oryon CPU rule, the
Adreno 830 fence default and the PA1Q panel/touch/haptics/thermistor record are in. `runtime/host-tests.sh` passes
9/9, and the S25 harnesses (`run-harnesses.sh`, the A830 KGSL stand-in, the bridge fixtures) pass. The phone itself
is unavailable (adb showed a Galaxy A71, not the S25), so the probe and first session are still pending.

OneOS's guests are Android systems that run unchanged on One UI's Linux kernel. iOS's userland needs Apple's kernel (XNU), so OneiOS runs a source-built XNU **as a process on One UI's kernel** (`runtime/xnuhost`): iOS's processes are Linux processes whose system calls and faults go to it. How it fits together: [docs/architecture.md](docs/architecture.md).

## Quick start

Prerequisites: Linux x86_64 PC, the pinned Apple IPSW pieces (fetched, never committed), NDK 28.2, Rust 1.95+, `qemu-aarch64`. Full flow: [docs/building.md](docs/building.md).

```sh
P=iphone18,2-ios27.0.1-24A446
scripts/fetch/fetch.sh --list "$P"          # compare the profile with Apple's IPSW (~1 MiB)
scripts/bg.sh fetch scripts/fetch/fetch.sh "$P"   # the IPSW members from Apple, into inputs/
kernel/fetch.sh                             # the kernel's sources at the commits of kernel/pins.sh
scripts/bg.sh image scripts/image/build.sh "$P" --kernel out/kernel/24A446/kernel.kc
scripts/bg.sh kernel-hosted env HOSTED_OUT=me kernel/build-hosted.sh
scripts/bg.sh prelink env KERNEL=out/kernel/current-hosted-me/xnu.macho kernel/prelink/build.sh
runtime/build.sh STAGE                       # -> STAGE/bin/xnuhost and helpers
```

Long jobs run in tmux via `scripts/bg.sh NAME COMMAND...`; check them with `scripts/bg.sh -s NAME`.

The base S25's host record is `profiles/host-galaxy-s25-sm8750.toml`. The runtime package is runtime-only:
`kernel.kc` belongs to the image set and still needs the Apple IPSW, the hosted XNU build and the prelink
([docs/s25.md](docs/s25.md)).

## What works

The S25 column is this fork's state; the S24 Ultra column is upstream's on-device result, kept for reference. The
base S25 has not been run on a phone yet.

| Area | S25 base (this fork) | S24 Ultra (upstream) | PC (qemu-aarch64) | Notes |
|---|---|---|---|---|
| Boot to Home Screen | Not run on device | Yes — lock screen, swipe unlock, apps shown (hosted 0035–0037) | Yes — launchd, SpringBoard, backboardd | [docs/s25.md](docs/s25.md) |
| GPU compositing | Host stand-in: A830, 192 GiB VA, 128 GiB VBO rung, timestamp fence | Yes, 40–60 fps via virtio-gpu | Yes, frames captured | [docs/gpu.md](docs/gpu.md) |
| Touch + buttons | Host-tested: `sec_touchscreen` acceptance, `tsp/enabled`; dtbo `stm,stm_ts_spi` | Yes | Scripts (`--input-script`) | Axes [U] until the probe |
| Battery | Host-tested: max77775 gauge + cps4038 `wireless` tree | Published to iOS | Stand-in tested | [docs/hardware.md](docs/hardware.md) 7c |
| Wi-Fi | Host-tested | Joins; scan rate cut 20→5/min (reuse works); late-enable merged, phone proof pending | Sim radio (`wifi=sim`) | [docs/hardware.md](docs/hardware.md) |
| Haptics / keyboard | Host-tested: cs40l26a FF map (4–10 ms, 40–255) | Fix merged, phone proof pending | Host-tested | [docs/hardware.md](docs/hardware.md) §5 |
| Interrupts / Mach RPC | Same code, PC-verified | — | Verified: 41k host-call IPIs vs 110 trapped | `oneios-perf` `rpc:`/`0081:` lines |
| Return to One UI | Not run on device | Yes — chord, SIGKILL backstop | n/a | Never touches side keys, realtime, or `/sys/power` |
| Host test suites | All green: 9/9; the device harnesses pass in the working copy | n/a | All green (loader, xnuhost, bridges, display, prelink audit 0 failures) | `runtime/host-tests.sh` |

## Known issues

| Issue | State | Tracking |
|---|---|---|
| On-device probe and first session | Not started: adb showed a Galaxy A71 (TWRP), not the S25 | [docs/s25.md](docs/s25.md) |
| Hosted XNU build on this PC | `kernel/build-hosted.sh` stops at `setup-toolchain: libdispatch not installed`; pacman cannot install it here | [docs/s25.md](docs/s25.md) |
| Image set / `kernel.kc` | Not built; needs the Apple IPSW (~10.6 GiB) + the hosted XNU prelink. The runtime package is runtime-only | [docs/s25.md](docs/s25.md) |
| Exact 6.6.127 GPL source | Not published by Samsung OSRC; the shipped `Image` + IKCONFIG are the authority, and the local 6.6.77 clone is not it | [docs/s25.md](docs/s25.md) |
| Adreno 830 on the base S25 | [I] until `gpuctl` runs on it; the host stand-in uses the S25 Ultra's reported values | [docs/gpu.md](docs/gpu.md) |
| Touch axes, haptics input name, Samsung bridge node names | [U] until the probe; the dtbo gives the drivers, not the runtime names | [docs/s25.md](docs/s25.md) |
| Never goals | Passcode, Face ID, Wallet, calls, SMS | — |

Upstream's S24 Ultra issues (typing overheat, Wi-Fi scan latency, audio MMAP start gap, HEVC encode, camera capture,
auto-brightness) remain in the upstream README; they have not been re-run on the S25.
| Shared-region protect no-op count still 0 | Optimization unproven | kernel README "0080" |
| Mac kernel panics in first 2 s | Old news, Mac runtime boots with Linux `kernel.kc` | `kernel/prelink/README.md` |
| Kernels built from `main` between the A56 merge (`b2fa083`) and 2026-11-21 died at launchd (`mach_ports_lookup() failed: -300`, panic `initproc exited`) | Fixed: hosted 0068 is opt-in (`oneios-legacy-registered-ports=1`); PC boot to SpringBoard again | `kernel/patches/oneios-hosted/README.md` (A56 section) |
| Never goals | Passcode, Face ID, Wallet, calls, SMS | — |

## Devices

Primary target: the rooted base Galaxy S25 (SM-S931B, `pa1q`, SM8750) — this fork. Upstream targets the S24 Ultra;
S25 Ultra, S26 Ultra and Galaxy A-series run in other forks. Full map: [docs/forks.md](docs/forks.md).

| | Device | Status | Best result |
|---|---|---|---|
| | Galaxy S25 (SM-S931B, SM8750) — **this fork** | Source-level port, host harnesses green; no on-device run | S25 host row, A830 stand-in, PA1Q dtbo record |
| ![Galaxy S24 Ultra](docs/images/s24-ultra.jpg) | Galaxy S24 Ultra (SM-S928, SM8650) — [upstream](https://github.com/akshaynexus/OneiOS) | Boots to Home Screen, returns to One UI | Daily driver target upstream |
| ![Galaxy S25 Ultra](docs/images/s25-ultra.jpg) | Galaxy S25 Ultra (SM-S938, SM8750) — [s25ver snapshot](https://github.com/xaxaapple/s25ver), denis fork (mirror) | Ported: Oryon topology fix, probe tool, metal fixes | Big-only-SoC bug fixed for all |
| ![Galaxy S26 Ultra](docs/images/s26-ultra.jpg) | Galaxy S26 Ultra (SM-S988, SM8850) — [YMGPwcca/OneiOS](https://github.com/YMGPwcca/OneiOS) | ~600 s iOS run on device | Haptics + backend tests ported |
| ![Galaxy A56](docs/images/a56.jpg) | Galaxy A56 (SM-A566E, Exynos 1580) — [Musicalista/OneiOS-A56](https://github.com/Musicalista/OneiOS-A56) | iOS 16.5 to lock screen on device | GPU template, bootstrap, triage tools |

Device photos: official Samsung product images ([samsung.com](https://www.samsung.com/us/smartphones/galaxy-s25-ultra/), Samsung Newsroom press kits), © Samsung Electronics — used for device identification. A56 photo: [Captainmorlypogi1959 via Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Samsung_Galaxy_A56_5G_2025_(cropped).jpg) (CC BY-SA 4.0). The base S25 row has no photo in this tree.

## Layout

```
docs/architecture.md  how the parts fit; docs/building.md  how to build and test; docs/*.md  each part in detail
docs/forks.md         device forks: what runs where, what was ported back, the multi-device pattern
docs/s25.md           the base-S25 record: firmware, kernel/config, PA1Q dtbo facts, [F]/[U] gaps
profiles/             the pinned IPSW profile (iPhone18,2 iOS 27.0.1 24A446); host-galaxy-s25-sm8750.toml is the S25 host record
scripts/fetch/        fetch only the needed IPSW members from Apple's CDN (never from here)
scripts/image/        build the image set (manifest, SHA-256, .tar.zst)
userland/             iOS 27 root and Data tweaks (the image hook), guest helpers, the camera plug-in
kernel/               XNU 12377 (UniXNU / Droid-VM fork) built on Linux; oneios-hosted/ patches; prelink/ (kernel.kc)
runtime/              xnuhost and its loader, the display helper, the hardware bridges, the runtime package
drivers/              iOS-side drivers for the bridges (CPU, network, Bluetooth, haptics, audio, brightness, camera)
oneos/                the contract with the OneOS fork (akshaynexus/OneOS, branch oneios)
```

## Rules

- **No Apple files in git or in releases.** IPSWs, cryptexes, kernelcaches, kexts, `kernel.kc` and every image built
  from them stay in `out/` and `inputs/` (git-ignored). Scripts fetch them from Apple and build them on the owner's
  machine. Releases carry only this project's own code (scripts, runtime binaries, the OneOS patches). The default
  kernel is built locally (it links Apple's corecrypto); a release carries only the hosted `ONEIOS_CRYPTO=original`
  `xnu.macho` (no Apple code in it), which each phone prelinks into its own `kernel.kc` ([docs/releases.md](docs/releases.md)).
- Long jobs run in tmux: `scripts/bg.sh NAME COMMAND...`.
- Every change updates the docs that describe it: [CLAUDE.md](CLAUDE.md).

## License

No license file yet — all rights reserved until the owner adds one.

## Credits

[OneOS](https://github.com/fahadalijaved/OneOS) (fahadalijaved) and
[OneLineage](https://github.com/WeWaffles/OneLineage) (WeWaffles): the container hot-switch.
[MysticGSI](https://github.com/MysticGSI/mysticgsi): the image build model.
[UniXNU](https://github.com/UniXNU/UniXNU) and [Droid-VM](https://github.com/Droid-VM): XNU on non-Apple
ARM64, drm2kgsl, the guest drivers. [iOSPlus10](https://github.com/akshaynexus/iOSPlus10): XNU and iOS userland on
Qualcomm phones, haptics and brightness. [Darling](https://github.com/darlinghq/darling): Mach and XNU on Linux.
