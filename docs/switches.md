# Switches: every recent change, one setting each

The owner (2026-11-23): "Merge it all with switches so we can test everything at once." One build has every change.
Each change that alters what runs can be turned back by **one** setting in the image's
`overlay/userland/xnuhost.conf` (or a kernel boot-arg there, through `boot-args-add=`). One boot tests everything; a
fault is pinned by turning one switch off.

Rules for a switch (and for every helper that lands a behaviour change from now on, `docs/HANDOFF-2026-11-23.md` 5):

- One key, read once at the start, cheap where it acts (one relaxed load, or a value fixed at the start).
- The default is the new behaviour (main's). The other value is the behaviour before the change, as close as the
  code allows; the table says where it is not exact.
- A row in this table in the same commit: key, values, default, what it changes, the commit, whether a phone ran it.
- No switch for pure logging, and none for a crash fix whose "off" would be the abort again.

xnuhost logs every switch once, after the `versions:` line:

```text
switches: session-mode=hosted stop-loop=wait takeover-reclaim=1 takeover-swappiness=keep task-trim=1 timer-coalesce=1 ram-fit-terms=tasks ram-ondemand=0 uclamp-rule=work gpu-down=governor gpu-blob-entries=65532 wifi-ring-wait=1 wifi-send-wait=1 wifi-buffers=1 wifi-joined-scan=cache encoder-10bit=main10 encoder-warmup=1 display-post=apart decoder=auto decoder-codecs=h264,hevc,vp9,av1 oneios-wifi-rssi=1 oneios-telemetry-jobs=0 oneios-pool-cut=1 oneios-pressure-big=512 oneios-disable-allowed=1 oneios-nested-exact=1 oneios-upgrade-flush=0 oneios-fill-fast-fault=1 oneios-idle-spares=1 oneios-bo-cache-idle=1 oneios-dont-care=1 oneios-queue-priority=1 (docs/switches.md); all at their defaults
```

When some are changed, the end says which: `not at the default: wifi-send-wait gpu-down`. Kernel switches are read
from `boot-args` there (the kernel's default when the boot-args do not name them).

## The table

Values `0|1` take `0/1`, `no/yes`, `false/true` (xnuhost's flags). "Phone" says whether a phone session has run the
change (the default value), and whether one has run the other value.

| Key | Values | Default | What the default does / the other value | Commit | Phone |
|---|---|---|---|---|---|
| `session-mode` | `hosted\|takeover` | `hosted` | takeover: the opt-in Android takeover (docs/booter.md 2.9d) | a3077a9, fork 1d9b58f | takeover booted 2026-11-23 21:11 (kernel b5f5378a), not with this build |
| `takeover-stops` | `default\|none\|NAME,...` | `default` | takeover only: the services the switch stops (`none`: none) | a3077a9, fork 1d9b58f | 21:11 boot: default list |
| `stop-loop` | `wait\|poll` | `wait` | wait: the fork's stop loop sleeps on init's properties (`xnuhost --prop-wait`); poll: one `getprop` look a second, no `--prop-wait` (the 0.2 s loop of before is not brought back). Read by the fork's switch; xnuhost checks it | a3077a9, fork 1d9b58f + this build | wait: 21:11 boot; poll: no |
| `takeover-reclaim` | `0\|1` | `1` | takeover only: 1: after the stops, before xnuhost, the fork's switch runs `sync`, `drop_caches` 3 and each zram device's `compact`, so that the page pools and caches MemAvailable did not count are in it for ram=auto (its line measures the MiB); nothing to put back. 0: none of it (before takeover-ram). Read by the fork's switch; xnuhost checks it | takeover-ram (docs/booter.md 2.9d, "More RAM in the takeover"), fork takeover-ram | no |
| `takeover-swappiness` | `keep\|0..200` | `keep` | takeover only: N: `vm.swappiness` N for the session (One UI's saved first; every return writes it back); keep: not touched (as before). The lever for a lower headroom. Read by the fork's switch; xnuhost checks it | takeover-ram, fork takeover-ram | no |
| `task-trim` | `0\|1` | `1` | 1: an idle task's host mappings are dropped and ram=auto counts 150 tasks awake (5376 MiB on the S24 Ultra); 0: no trim, every task at 4 MiB: exactly 3b8742a's sizing (`config::tests::ram_config`: 3528 MiB fit) | 07b10a8 | no |
| `ram-ondemand` | `0\|1` | `0` | **option C, off; against the fixed-RAM rule of 2026-11-15, for the owner to choose.** 1: the guest's RAM is not prefaulted; each page is locked as iOS first touches it (`MLOCK_ONFAULT`: never swapped while held) and free pages are given back; with `ram=SIZE` above what fits, the top is backed only while iOS uses it, and when iOS fills it the valve opens or the OOM killer ends xnuhost (docs/booter.md 2.2, "On demand"). 0: the fixed, locked RAM | takeover-ram | no |
| `timer-coalesce` | `0\|1` | `1` | 1: a vCPU's host timer is armed only when the guest's deadline comes earlier; a later deadline or a cancel leaves it, and a signal before the deadline arms it for the deadline without raising the line (docs/booter.md 2.4); 0: every new deadline and cancel is a `timer_settime`, every signal raises the line (before). `perf: ... timer_set N (M to the host timer, K kept it; R early signals armed it again)` | fix/unmap-fanout | no |
| `ram-fit-terms` | `tasks\|old` | `tasks` | tasks: ram=auto's 3b8742a terms (tasks, hold share, 1536 MiB headroom locked); old: the terms before 3b8742a (headroom 768 MiB locked / 1 GiB unlocked, page tables 8 %, no task cost, no hold share; any of those keys given still wins). With `task-trim=0` too this is the v0.1.19 sizing | this build (3b8742a) | tasks: no; old: v0.1.19's sessions |
| `uclamp-rule` | `work\|all` | `work` | work: xnuhost alone sets a vCPU thread's uclamp (0 at idle, the released thread's floor, the guest's request); drm2kgsl's animation boost only for the frame's threads; background/utility 0. all: before 03d85ac: drm2kgsl's boost on every started vCPU thread and every iOS thread not background, the cpufreq device sets the vCPU thread's clamp, nothing at idle | 03d85ac + this build | no |
| `gpu-down` | `governor\|bottom` | `governor` | governor: the GPU level leaves the top when a frame fits at the governor's clock in a burst (learned ratio); bottom: when it fits at KGSL's lowest clock (before 3b36ff0: on the S24 Ultra it never came down) | 3b36ff0 + this build | bottom: every session up to 21:11 (the GPU at the top all session); governor: no |
| `gpu-blob-entries` | `16384..65532` | `65532` | the most entries of a guest 3D blob the device takes; 16384: the limit before 78c4b86 (an 80 MiB blob of single pages refused; drm2kgsl 0010 keeps that from aborting) | 78c4b86 + this build | no |
| `wifi-ring-wait` | `0\|1` | `1` | 1: a frame for iOS that finds its receive ring full waits for the guest's kick (20 ms data, 50 ms the station's own); 0: dropped at once (before cbf97ea) | cbf97ea + this build | 1: 2026-11-23 19:40 |
| `wifi-send-wait` | `0\|1` | `1` | 1: a frame of iOS's that finds wlan0's send buffer full waits (256 at most, 20 ms); 0: it fails at once (`send failed, last errno 11`, before b704922) | b704922 + this build | 0: 19:40 (the old code); 1: no |
| `wifi-buffers` | `0\|1` | `1` | 1: wlan0's AF_PACKET socket gets a 4 MiB receive and a 1 MiB send buffer; 0: the kernel's defaults (before cbf97ea) | cbf97ea, b704922 + this build | 1 (receive only): 19:40 |
| `wifi-joined-scan` | `cache\|low\|plain` | `cache` | cache: within 120 s of the last scan its networks answer, else a low-priority scan; low: every scan the radio's, low priority (cbf97ea); plain: every scan the radio's, no flags (before cbf97ea) | b704922, cbf97ea + this build | low: 19:40 (the driver took no flags: `flags 0x0`); cache: no |
| `encoder-10bit` | `main10\|8bit\|nv12` | `main10` | main10: the device takes P010 (iOS's 10-bit tiles) and the helper codes HEVC Main10 where the encoder offers it, else 8-bit of the high bytes; 8bit: P010 taken, Main10 never tried; nv12: the device lists and takes no P010, the guest sends NV12 (its high bytes). Not exact: before b3da193 the guest refused 10-bit tiles itself (stage 101), which the kernel no longer does | b3da193 + this build | no |
| `encoder-warmup` | `0\|1` | `1` | 1: the encoder helper starts at the guest's first virtio-media session and opens one HEVC encoder at once; 0: it starts at the first encode, no warm-up (its binder threads start either way: that is the fix of the hang, not a choice) | b3da193 + this build | no |
| `display-post` | `apart\|serial` | `apart` | apart: the display helper reads and answers each frame as it comes (a reader thread) while its poster waits for the layer's buffer, fills it and posts the newest picture; serial: xnuhost sends the helper `options` (`XHDO`, bit 0) and its reader reads nothing more until the last change is posted, the one loop's timing before 2026-11-24 (exact: the same order of read, answer, lock, fill, post). A helper older than this build ignores the message (it has the one loop anyway) | fix/frame-path | no |
| `decoder` | `auto\|off` | `auto` | auto (2026-11-24): the video device decodes too (card `oneios-encoder+decoder`): iOS's paravirtualized VideoToolbox decoder gets the phone's MediaCodec for H.264, HEVC, VP9 and AV1 (8-bit 4:2:0; a 10-bit stream, a size outside 96..4096 or above 4096x2304 is refused and decoded in software). Needs `encoder=auto` or `sim` (the device and media pool are the encoder's). off: an encoder only, `decs` lists nothing and iOS decodes in software (videocodecd, WebKit), as before. Exact: the guest's decoder half is the same code either way, it finds no device | fix/hw-decode | no |
| `decoder-codecs` | `h264,hevc,vp9,av1` (any of them, comma separated) | all four | the codecs the device offers (ENUM_FMT of OUTPUT, so `decs` and `dcap`); one left out is decoded in software. Pins a codec whose MediaCodec decoder misbehaves | fix/hw-decode | no |
| `boot-args-add=oneios-wifi-rssi=0` | `0\|1` | `1` | 1: the Wi-Fi driver posts RSSI_CHANGED records (the bars follow the signal); 0: none, wifid reads the signal at the join only (before cbf97ea). Kernel (net/OneiOSWifi.cpp) | cbf97ea + this build (db2eb42) | no |
| `boot-args-add=oneios-telemetry-jobs=1` | `0\|1` | `0` | 0: 0093 and 0094 disable the telemetry group (osanalyticshelper and its agent job, 16 uploaders); 1: the disable list is 0053's loops only and the enable pass enables the telemetry labels again (a store 0093 changed gets them back). Kernel, hosted 0096 | bad88c3, 30bdea6 + 0096 | 0093 alone: 20:23 (osanalyticshelper still ran: 0094) |
| `boot-args-add=oneios-pool-cut=0` | `0\|1` | `1` | 1: metal2vulkan releases a spare command pool that holds more than its last 10 s need plus 8 command buffers (the render server's small command-stream BOs go back); 0: the pools are kept as before (the plug-in sets `METAL2VULKAN_POOL_USE_WINDOW_S=0`). Metal plug-in only (the boot add-on), 624bab3 add-on only: the rt runtime reads no `METAL2VULKAN_POOL_USE_WINDOW_S` (its own reaper trims pools), docs/gpu.md "Who keeps a blob" | 50873fe | no |
| `boot-args-add=oneios-pressure-big=0` | `0\|MB` | `512` | 512: at a memory pressure rise a process of at least 512 MiB footprint not yet notified at that level is notified first, the largest first (the render server and the apps that draw hear of it in the first 250 ms steps, not after 150-300 smaller processes); 0: XNU's order (least important band first, 250 ms apart: the GPU clients last). Kernel, hosted 0097; its `oneios-wired` and `oneios-pressure` lines are logging (no switch) | 0097 (fix/wired-names) | no |
| `boot-args-add=oneios-disable-allowed=0` | `0\|1\|NAME` | `1` | 1: before launchd the kernel renames the 19 `DisableAllowed` keys of the sealed `/System/Library/LaunchDaemons/com.apple.jetsamproperties.V54.plist` (same length, nothing else changes) in a copy that 0031's union shows over LaunchDaemons, so launchd honours disabled.plist for every job (osanalyticshelper and its agent, diagnosticextensionsd, symptomsd-diag, OTATaskingAgent, softwareupdated ran although disabled); NAME: the plist of that product type; 0: no copy (an earlier boot's removed), launchd runs the marked jobs whatever disabled.plist says (before 0098). With `oneios-telemetry-jobs=1` the telemetry labels are enabled in disabled.plist, so they run either way. Kernel, hosted 0098; `oneios-kill` (0100) is logging (no switch) | 0098, 0100 (fix/disable-allowed) | no |
| `boot-args-add=oneios-nested-exact=0` | `0\|1` | `1` | 1: each task keeps a page bitmap (1 KiB per 32 MiB block it filled in) of the shared region; a shared-region flush is sent only to the tasks that have a page of its range, and forgets those pages; 0: 0045's block bits only (a flush goes to every task that filled any page of the block). Kernel, hosted 0101 | fix/unmap-fanout | no |
| `boot-args-add=oneios-upgrade-flush=1` | `0\|1` | `0` | 0: an entry changed to the same page with no less access (arm_clear_fast_fault: the reference or modified fault taken; pmap_enter of the same page again) keeps the tasks' host mappings; 1: such a change unmaps the page in every task that may have it, as before. Kernel, hosted 0101 | fix/unmap-fanout | no |
| `boot-args-add=oneios-fill-fast-fault=0` | `0\|1` | `1` | 1: a user fault whose entry waits for its reference or modified fault is resolved on the fill path (arm_fast_fault, then the fill); 0: it goes the exception path (sleh, arm_fast_fault, the return and the refill) as before. Kernel, hosted 0101 | fix/unmap-fanout | no |
| `boot-args-add=oneios-idle-spares=0` | `0\|1` | `1` | 1: the Metal plug-in's idle trim (every 2 s) also frees the images and buffers metal2vulkan's recyclers parked past their second and the spare dedicated image blocks past their 10 s (they went only as another object was parked, taken or returned, so a still process kept up to 128 + 64 MiB parked and every spare); 0: they age only as before (the plug-in sets `METAL2VULKAN_IDLE_SPARES=0`). Metal plug-in only (the boot add-on), 624bab3 add-on only: the rt runtime reads no `METAL2VULKAN_IDLE_SPARES` (it has no such spares), `userland/metal/m2v-idle-memory.patch`, docs/gpu.md "The idle memory" | this build | no |
| `boot-args-add=oneios-bo-cache-idle=0` | `0\|1` | `1` | 1: every 60 s the plug-in gives back the BOs in Turnip's cache of freed BOs that no allocation took for 30 s (`tu_darwin_bo_cache_trim_idle`); 0: the cache goes back only past its budgets or at a pressure rise, as before (the render server kept 78 MiB at a still screen). Metal plug-in only (the boot add-on), both add-ons (the same Turnip and the same row code, `CommandPoolRow.inc`), `userland/metal/turnip-idle-bo-cache.patch` | this build | no |
| `boot-args-add=oneios-dont-care=0` | `0\|1` | `1` | rt add-on only: 1: Metal's DontCare and MultisampleResolve stores are Vulkan `DONT_CARE` on Turnip (rt 0005, Droid-VM 0021's measurement: 37 ms against 9 ms a pass); 0: they store (`METAL2VULKAN_DONT_CARE_STORES=0`, `userland/metal/rt/RtSwitches.inc`). The 624bab3 add-on has Droid-VM 0021 always | port/m2v-rt | no |
| `boot-args-add=oneios-queue-priority=0` | `0\|1` | `1` | rt add-on only: 1: the render server's queues at the high global priority (rt 0006, Droid-VM 0012); 0: the driver's default (`METAL2VULKAN_QUEUE_PRIORITY=default`) | port/m2v-rt | no |

### The Metal add-on: 624bab3 or rt

Not a key of `xnuhost.conf`: which boot add-on (`image/kernel.kc.gpuaddon`) the Mac stages, with
`oneios-tool phone addon FILE` (and `phone addon --remove` for the image's own plug-in). Both carry the same Turnip and
the same camera plug-in; they differ in the bridge's runtime (`userland/metal/rt/README.md`).

| Add-on | Built by | Default | What |
|---|---|---|---|
| 624bab3 | `userland/metal/build.sh` (`METAL_RUNTIME` unset) | yes, until a phone boot of rt passes | the old runtime and its 56 patches; the fallback |
| rt | `METAL_RUNTIME=rt userland/metal/build.sh` | no | metal2vulkan `development` 6462199fa (`metal2vulkan-runtime` 0.2) with `userland/metal/rt/patches` 0001-0008 and the rewritten bridge; not booted |

Each build writes `NativeAdrenoMetal.runtime` (`624bab3` or `rt`) beside its bundle; the CDHashes of a pair the Mac
was given are in that hand-off, and `oneios-tool phone sums` shows the staged one.

Not switched, on purpose:

- drm2kgsl 0010 (78c4b86: no abort at a context's end with a refused blob) and the kill-of-pid-0 guard (df485a0): crash
  fixes; off is the abort.
- The fatal-signal lines and end records (df485a0, fork bd4bbf2), the Wi-Fi traffic line (cbf97ea), the memory
  pressure line (3b8742a), `perf: qos:`, `thermal: 30 s clocks:`, `cpu: policyN: governor`, the E/P hand-off count
  (03d85ac), the `host_wait%` column (3b36ff0), 0095's `thread calls:` line: logging only.
- The encoder helper's binder threads and its open watchdog (b3da193): the fix of the hang and its log line.
- Older keys that already were switches: `map-defer`, `gpu-level`, `gpu-priority`, `cpu-pin`, `ram-*`, `pen`,
  `encoder`, `wifi` (docs/booter.md section 1).

## A sample `xnuhost.conf` fragment

Every switch at its default (the build's behaviour). Change one line to turn one change back:

```text
# docs/switches.md: every switch at its default
session-mode=hosted
takeover-stops=default
stop-loop=wait
takeover-reclaim=1
takeover-swappiness=keep
task-trim=1
timer-coalesce=1
ram-fit-terms=tasks
ram-ondemand=0
uclamp-rule=work
gpu-down=governor
gpu-blob-entries=65532
wifi-ring-wait=1
wifi-send-wait=1
wifi-buffers=1
wifi-joined-scan=cache
encoder-10bit=main10
encoder-warmup=1
display-post=apart
decoder=auto
decoder-codecs=h264,hevc,vp9,av1
# kernel and plug-in switches (boot-args-add appends to boot-args; one line, all twelve words):
# boot-args-add=oneios-wifi-rssi=1 oneios-telemetry-jobs=0 oneios-pool-cut=1 oneios-pressure-big=512 oneios-disable-allowed=1 oneios-nested-exact=1 oneios-upgrade-flush=0 oneios-fill-fast-fault=1 oneios-idle-spares=1 oneios-bo-cache-idle=1 oneios-dont-care=1 oneios-queue-priority=1
```

## Tests

- xnuhost: `config::tests::the_switches` (each key's values and refusals, the image's conf and the command line over
  it, the switches line), `config::tests::ram_config` (`task-trim=0` 3528 MiB fit; `ram-fit-terms=old` 5688 MiB fit
  with the lock, a given key wins, 1 GiB headroom unlocked), `gpu3d_kgsl::policy::tests::down_with_a_wide_clock_range`
  (`gpu-down=bottom` stays at the top in the phone's case), `cpu::tests::release_floor` (`uclamp-rule=all`'s floor).
- Display (fix/frame-path): `config::tests::the_switches` (`display-post`), `display::tests::the_serial_post_option` (the
  options message after the hello with `serial`, none at the default), the helper's `test_run_serial` (a frame waits for
  the post before it) and `test_run_answers_before_the_post` (the default: answers within 50 ms of 100 ms locks).
- Bridges: `switches::tests::words_and_defaults`, `wifi::uplink::tests::no_wait_when_switched_off`; the encoder's
  `switches::tests::words` (with `decoder-codecs=`'s words); the decoder's `tests/decoder.rs` (`decoder=off`: the card
  and the formats of an encoder only; `decoder-codecs=vp9`: VP9 alone).
- Metal: metal2vulkan's `tests::spares` (12, fix/gpu-blobs); the plug-in of this build is made with the patch.
- Kernel: 0096 applies after 0095; the kernel of this build has both boot-args (`strings`).
- Kernel: 0097 applies after 0096 (`git apply --check` on the patched tree). Built on the laptop with `HOSTED_OUT=wired ONEIOS_CRYPTO=original` and the series less 0095 (its `recount.c:753` panic is fixed apart): `xnu.macho` `9a604844`, 412 s, releasable; prelinked to `out/kernel/24A446-wired/kernel.kc` `2890c098` (`.hashes` `cfd918a8`), audit 0 failures, stubs-check 666 good 0 bad; `kernel/tests/host.sh` 8 of 8. `config::tests::the_switches` has `oneios-pressure-big=0`. Merged after the fixed 0095 (2026-11-24) and built again with the whole series: `xnu.macho` `32f88a27` (182 patches, releasable), `kernel.kc` `e79886d9` (`.hashes` `e06b673d`), audit 0 failures, stubs-check 666 good 0 bad; `kernel/tests/host.sh` 10 of 10, `cargo test -p xnuhost config::` 16 passed. Under qemu-aarch64 (8 vCPUs, 4 GiB, the integration runtime) 20 minutes, no panic, SpringBoard at +247 s, `oneios-wired` and `oneios-pressure` every 60 s; no pressure rise there, so the default 512 itself is not exercised (kernel/patches/oneios-hosted/README.md, "0097").
- Kernel: 0098 (`oneios-disable-allowed`) and 0100 (`oneios-kill`, logging) apply after 0097; `kernel/tests/host.sh` builds 0098's rename from the patch with a made-up plist (all 11 of host.sh pass), and the same rename of the real 24A446 plist leaves a plist plistutil reads, only the 19 keys changed. Built with `HOSTED_OUT=disallow ONEIOS_CRYPTO=original` (184 patches): `xnu.macho` `7ecd6622`, releasable; `kernel.kc` `a07f5426` (`.hashes` `80688378`), audit 0 failures, stubs-check 666 good 0 bad. `config::tests::the_switches` has `oneios-disable-allowed=0` (16 passed). Not booted.
- Fork: `OneiosScriptTest.theStopLoopLooksOnceASecondWithARuntimeFromBeforeThePropertyWait` (`stop-loop=poll`: no
  `--prop-wait` call, its line).
- Kernel: 0101 (fix/unmap-fanout) applies after 0100 (`git apply --check` on the patched tree). Built on the laptop with
  `HOSTED_OUT=unmap ONEIOS_CRYPTO=original` and the merged series (185 patches): `xnu.macho` `346e8b67`, 415 s,
  releasable; prelinked to `out/kernel/24A446-unmap/kernel.kc` `a041bc20` (`.hashes` `c81e4ea7`), audit 0 failures,
  stubs-check 666 good 0 bad; `kernel/tests/host.sh` 10 of 10. xnuhost `timer-coalesce`: `config::tests::the_switches`
  (its values, a refusal, the line with the three kernel switches); `runtime/host-tests.sh` (no qemu part): lint,
  lint-android, port, links and package pass; workspace 100 of 101 in onebridges, the one failure
  `audio::test_audio_helper::audio_helper` (`underruns` 1, a test that needs the host to run its threads on time; it
  passed run alone, and this change touches no bridge). Before the owner's no-qemu rule, two partial qemu boots of
  the same code (all four off against the defaults): kernel/patches/oneios-hosted/README.md "0101". Not booted on the
  phone.
- The integration build of 2026-11-24 (main d3b802f: takeover-ram, m2v-update, frame-path, m2v-rt, hw-decode, with
  main's 0098, 0100 and 0101; fork e00fb92), on the build laptop: `runtime/host-tests.sh` passes (361 cargo results, 0
  failed; qemu boot skipped), `kernel/tests/host.sh` 11 of 11, `config::` 16 passed, the fork's `./gradlew test
  --rerun-tasks` 734, 0 failures. Kernel `HOSTED_OUT=i2 ONEIOS_CRYPTO=original` (185 patches, the decoder's codec
  edit): `xnu.macho` `4fb1cf6a`, releasable; `kernel.kc` `4db993fc` (`.hashes` `e1b3ee7e`), audit 0 failures,
  stubs-check 666 good 0 bad. Add-ons with the camera plug-in (`fda818f2`): 624bab3 (NativeAdrenoMetal `080821ef`,
  Turnip `5652c89d`; 12,330,152 bytes) and rt (`7a398d78`, `bbe6c736`; 9,538,920 bytes). Runtime 0.1.21-dev
  `0fc186a8` (xnuhost `7f33cd68`, oneios-encoder `79bc4a70`). APK 0.5.87 (216) `b5d72170`, the shared debug key.
  Handed to the Mac as `out/handoff/integration2-d3b802f/`. Not booted.
- Not tested on a phone: any non-default value, and most defaults (column "Phone").

## Found while building the Metal plug-in (2026-11-23)

- `userland/metal/patches/metal2vulkan/0101-0103` are copied into the staged tree as `90101-...` but **never applied**:
  the base's `build-adreno-metal-bridge.py` applies a fixed list of names, not every `*.patch`. No plug-in built here
  has them. Left so on purpose (they change shader translation and no phone has run them); applying them is a change
  for its own commit, behind a switch. `userland/metal/build.sh` says so now (its comment and its `stage` line).
- `build-turnip-darwin.py` re-uses an existing `build/mesa-tu-ios27` with `meson configure` (options only), so a
  changed cross file never reached it (the helper of fix/gpu-blobs removed the directory by hand). `build.sh turnip`
  now keeps the cross file's inputs (the SDK, the four tools `xcrun --find` gives, the script's `cksum`) in
  `build/mesa-tu-ios27.cross-inputs` and removes the directory when they changed; a directory with no record is kept
  and recorded.
