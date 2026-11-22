# Devices and forks (2026-11-20)

One project, many phones. This file maps every known OneiOS device fork: what runs where,
what already flowed back into `main`, and what is still fork-only. Status as of 2026-11-21;
fork contents move fast and this file will go stale — the dates say when it was true.

Our own target in this fork is the rooted base Galaxy S25 (SM-S931B, `pa1q`, SM8750); upstream
`akshaynexus/OneiOS` targets the S24 Ultra. Everything below is judged by what it teaches upstream
or what runs on more phones without breaking the S24 Ultra.

| Device | Fork(s) | Best result claimed | Ported to `main` | Still fork-only |
|---|---|---|---|---|
| Galaxy S25 (base, SM-S931B pa1q) — **this fork** | `NoNameBuilder/OneiOS` (fork of `akshaynexus/OneiOS`, branch `main`) | Source-level port and host harnesses green; no on-device run yet (adb saw a Galaxy A71, not the S25) | nothing yet (the work is in this fork) | S25 host row (`host-galaxy-s25-sm8750.toml`), A830 KGSL stand-in, PA1Q dtbo record, S25 battery/thermal fixtures; on-device probe and first session; the device harnesses live in the working copy |
| Galaxy S24 Ultra (upstream) | `akshaynexus/OneiOS` (upstream) | iOS 27 booted, SpringBoard, sessions with return | — (upstream home) | — |
| Galaxy S25 Ultra (SM-S938, SM8750 — denis branch; base S25 SM-S931B pa1q — s25ver snapshot) | `denis0001-dev/OneiOS-S25-Ultra` branch `device/s25-ultra` — 2 commits (Adreno 830 fences, SM8750 host profile; fork claims SpringBoard on SM-S938B, no return/frame-rate/app-switch evidence; fork `main` is a pure upstream mirror plus ~40 upstream-mirror `fix-*` branches, not S25 work). [s25ver snapshot](https://github.com/xaxaapple/s25ver) — single-commit base-S25 layer plus `xaxaapple-patch-1` trivia | fork-claimed SpringBoard sessions with a fork-built `xnuhost` (not re-run here) | Adreno 830 wait-timestamp default (`0007-a830`), `virgl_fence_table_init`, host profile, verify/ssh scripts, dropbear/zsh builders, S25 record (ported 2026-11-21) | its `topology.c`/`test_bridges.c` (predate our Oryon rule), `ONEIOS_XNUHOST` dead line, Makefile deletions, README framing; OneOS phone gates (other repo); on-device proof |
| Galaxy S25 (base, SM-S931B pa1q — s25ver snapshot) | `xaxaapple/s25ver` — not a GitHub fork, a Nov-16 snapshot copy (~386 files) preserving S25 work our tree never had | S25 layer (probe, Oryon topology, metal patches, port record) | Oryon CPU rule + E-lending (`topology.c`), SM8750 fixture, `tools/probe/s25-probe.sh`, `docs/s25.md` + hardware/building notes (merged 2026-11-20) | `panel-fallback=` opt-in and the S25 table row (ported 2026-11-21; `verify-host.sh` accepts SM-S931), `s25-phone-kit.yml` and metal2vulkan 0104 (NEEDS-REBASE) not taken, SM-S931 phone gates (live in the OneOS fork, not here), Adreno 830 unknowns, bare-metal/Xiaomi parts (rejected in `s25.md` itself) |
| Galaxy S26 Ultra (SM-S948B, SM8850) | `YMGPwcca/OneiOS` — iOS 27 guest boot, first frame, ~600 s run on real S26 hardware (their `docs/SM8850_LIVE_STATUS_2026-11-20.md`), via a root-only Framework-takeover experiment; OneOS app switch still blocked; return/watchdog handback explicitly NOT verified. Open PRs: #1 (SM8850 input/brightness/display diagnostics, on-phone PASSes) and #8 (thermal P0.1 preflight evidence, draft, not accepted for another run). 2026-11-21: a 90 s Framework-stop probe ran xnuhost and ended about 1 s later when their own root thermal guard cut out at an experimental 55 °C limit (reboot return seen); a 300 s request aborted on CPU zone 10 at 75.2 °C before xnuhost launched; a CPU 90 °C repeat was inconclusive; 600 s still blocked (the guard is in their OneOS app fork, PR #6) | 600 s on-device run | SM8850 input/brightness/display/thermal diagnostics (branch `port-sm8850`, 2026-11-21): native-touchscreen priority + wearable guard (`xh_input_accept_touch`, host-tested), opt-in bench+SHM `touch-exclusive` grab (default off, takeover refused), `brightctl inspect` read-only probe + `test_inspect.sh` + 2300-cap test, `cpu-hw-trip-*` exclusion + trip/recovery tests, `displayctl` test-mode guard + `--socket` + union-flush + flush-count tests, `frame_perf` stats lib + host test (helper log wiring open); port-sm8850-2 (2026-11-21): bridgectl haptic checks (`haptic-inspect`, `haptic-hal-inspect`, `haptic-hal-primitives`, `haptic-native-inspect`; `haptic-hal-click`/`heavy-click` behind `ONEIOS_HAPTIC_ACTIVE_TEST=1`), `bridgectl haptic` fails when no effect played, stand-in tests +12, and the `haptics-native-impacts` session option (default off, treble + no fallback, appends `oneios_native_impacts=1`, no setenv); native haptics impact path, multi-phone hw-backend stand-in tests, Metal/Vulkan trace patches (merged from `port-metal-haptics` 2026-11-21; haptics delivery itself NOT physically verified — needs a safety-qualified session) | Framework-takeover experiment, S26-only GPU/display-direct refactor (their tree deleted our 0076/0078/0080/0081 and `display-direct`, `ram-view`, `blk-late-flush`, Oryon rule, wifi IFF_UP, adreno qemu reporter — all kept here; do NOT take as-is), oneios-tool transfer/remotezip rewrite, encoder/audio/hal-compose haptics stack, `cpu-affinity-bench` (superseded by main's default `cpu-pin=1`), the audio prefill/xrun-coalescing chain and the encoder binder-pool/EOS fixes (general fixes, candidates for a separate port with an S24 check), sm8850 `scripts/phone/*` Python tooling + thermal preflight scripts (phone-gated), their docs tree |
| Pixel 11 Pro (Tensor, claim) | `mangosaretasty/PixeliOS` — zero unique commits (Nov 14); README still S24 Ultra. Stale mirror. | — | nothing (mirror) | — |
| Galaxy A56 (SM-A566E, Exynos 1580) | `Musicalista/OneiOS-A56` — iOS 16.5 userland "starts to its UI" on device (Nov 18) by mixing 24A446 kexts + hosted patches 0067–0073 (fork's words: UI start on the panel; touch NOT working; their README screenshots still say boot stops at the logo — inconsistent). Also ships an `ios-27.0.1-a56.toml` profile and a Manager APK with GitHub download | UI start on A56 (our gloss: lock-screen-class; imprecise — fork doesn't claim more) | kernel 0068/0072–0074/0077 + 0004/0029 deltas, `ONEIOS_CRYPTO=original` (opt-in), version-gated `RAMDisk1`, roottweak 20F66 boot-args, amdgpu staged-GPU3D + golden tests, `emit.rs` no-seal, A56 profiles (reference), `tools/test_a56_gpu.c`, `docs/a56.md` + `docs/a56-oneos-fork.md` (ported 2026-11-21, host-tested, no phone/kernel proof yet); 0068 is opt-in since f75f2dd (`oneios-legacy-registered-ports=1`, which roottweak writes for 20F66; fork sessions that write their own boot arguments need that flag: on by default it broke launchd on every iOS 27 boot); audit 2026-11-21 (`port-forks-gap`): the clang-wrapper PAC flags, `verify-host.sh` SM-A566 row, the A56 table row (`gpu=2d`, `panel-fallback=1080x2340@120`) | switch script + Manager app (OneOS-fork proposal, `docs/a56-oneos-fork.md`), touch (no fix in the fork's tree; needs the 20F66/24A446 IOHIDEventServiceUserClient tables), the Xclipse 3D host backend (binary `tools/xnuhost_a56` only, source unpublished: ask the fork), S24-untested `original` crypto |
| Galaxy A53 | — | no A53 fork found (searched 2026-11-20; re-checked 2026-11-21) | — | — |
| (mirror, no device) | `gageconnor/OneiOS` — pure upstream mirror, 0 unique commits | — | nothing | — |

## What the forks already made better (in `main`)

- **Oryon CPU topology** (s25ver): Qualcomm implementer `0x51` is never E; all-performance hosts lend the slowest
  cores as E vCPUs. A latent wrong-topology bug on every big-only SoC, not just the S25; S24 behavior unchanged.
- **Multi-phone backend tests** (YMGPwcca via the hw-backend merge): samsung vs treble backends against stand-in
  sysfs/binderfs, the harness every new phone reuses.
- **Native haptics impact path** (YMGPwcca, merged 2026-11-21 — guest→HAL delivery NOT physically
  verified upstream): discrete taps instead of chopped VIB segments, negotiated (`features=0` keeps old hosts working).
- **Phone probe tooling** (`tools/probe/s25-probe.sh`): read-only fact-gatherer; the first step of any new port.
- **Adreno 830 fence default** (denis S25 Ultra): `WAITTIMESTAMP_CTXTID` everywhere until the FENCE path is
  proven on SM8750 (`NCTX_NO_WAITTS` for A/B); plus the missing `virgl_fence_table_init` and a `--forward`
  patch-loop re-run. S24 behavior unchanged (FENCE path was already unused there by default... on non-A830
  the default was chip-gated; now uniform — same effective path for the S24's Adreno 750).
- **S25 Ultra host record + session scripts** (denis S25 Ultra): `profiles/host-galaxy-s25-ultra-sm8750.toml`,
  `scripts/port/verify-host.sh` (read-only preflight), `scripts/phone/ssh-forward.sh` (guest shell relay),
  in-tree arm64e `build-dropbear.sh`/`build-zsh.sh`.
- **Lasting trace tooling** (metal/Vulkan traces, sandbox-exec triage 0073): off-by-default debug aids, per the
  fix-plus-tool rule.
- **iOS 16 compat shims, version-gated** (A56, `docs/a56.md`): the 16.5 user ABI repairs (0072 swap/surface-info
  rewrite, 0073 mac-exec triage, 0077 legacy bootstrap replies, 0074 shared-cache check, 0068 registered ports),
  the loader's `RAMDisk1` for 20F66 images only, roottweak's 20F66 boot-args, and the amdgpu staged-GPU3D branch.
  All default-off or size-triggered; the 27 path is unchanged.

## How one tree supports many phones (the pattern)

- **Host device table** (2026-11-21, `host-device-detect`): xnuhost detects the host phone at start (`ro.product.model`, `ro.product.device`, `ro.board.platform`, `ro.soc.model`, `ro.soc.manufacturer`; soc0 and the device tree as fallbacks) and takes its defaults from one table, `runtime/bridges/hw/host_device.c`: S24 Ultra (none, today's), S25 Ultra (none yet: its values are [I]/[U]), S25 (`panel-fallback=1080x2340@120`), S26 Ultra (`ONEIOS_BRIGHT_MAX=2300`), A56 (`gpu=2d`, `panel-fallback=1080x2340@120`). The command line and the image set win over it; `host-device=none` turns it off, `host-device=NAME` forces a row. A port adds a table row (existing options only) plus its record, instead of a fork. `docs/hardware.md` 7h.
- **Backends, not forks**: `runtime/bridges/hw` (`hw-backend=samsung|treble`, per-bridge overrides) plus the
  stand-in tests above. A new phone starts by proving its backend against fakes, not by forking the repo.
- **Per-device records**: `docs/s25.md` is the template (F/I/U-marked device table, verdict table, probe→decision
  matrix). A port lands as a record plus its profile/config, not as a parallel universe.
- **Per-device config**: `userland/config/device-iphone18,2.toml` is the guest-side pattern; the host side follows
  the same shape as backends grow.
- **What a new port still needs each time**: panel mode/refresh (display helper XHDP report), input/evdev
  capabilities, sensor/HAL paths (backend stand-ins first), GPU backend notes (AGX gate pattern in `docs/gpu.md`),
  kernel notes only where the SoC differs (the Oryon rule is the template: describe, test, keep old phones unchanged).

Guesses, marked: fork "best results" are the forks' own claims (log/session references inside their docs where
given); nothing here was re-run on our side except the ported items' host tests. The A53 row is empty — point us at
a fork if one exists.
