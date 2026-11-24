# pa1q bare metal: XNU on the SM-S931B's own hardware

> **Status: scoping and host-side bring-up. Nothing in this file has run on the phone.** The hosted session
> ([s25.md](s25.md)) is still the one that reaches SpringBoard, and it stays the daily driver until this track
> reaches M3. The owner's call (2026-11-24): bare-metal XNU, by a **kernel-only late load** — no UEFI, no
> fastboot, no partition writes, and a reboot is the way back to One UI.

## 1. The model

XNU does not need UEFI. Apple's own boot chain is not UEFI either: iBoot hands the kernel a device tree, a memory
map and boot arguments, and `PE_state` is built from that. UniXNU needs a UEFI environment because it is a generic
loader for phones that have one; a kernel-only load can hand XNU the same three things directly, which is closer to
iBoot than UniXNU is [I].

What the phone-side loader must therefore provide, and nothing else:

| Thing | For XNU |
|---|---|
| Entry | EL1, MMU off, D-cache off, one core running; the other seven parked (PSCI `CPU_OFF`, or spinning on a known address) |
| Memory | one contiguous window XNU owns (Pagani runs a full system in 4 GiB), and the rest of RAM described as reserved so XNU never touches Android's remains |
| Device tree | an FDT in Apple's shape, built from the phone's own DT plus the board overlay — the same tree `runtime/loader/src/machine.rs` already emits for the hosted board, so that code is the seed |
| Boot args | `-v`, `debug=`, `serial=` as today |
| Console | none from the firmware: the first output is the panel (M1) or the GENI debug UART (`0x894000`, below) as the fallback |
| Return | nothing. The image lives in RAM, no flash is written, a power cycle brings One UI back |

There is no boot service, no runtime service and no GOP to emulate, because XNU never had them: everything the
kernel needs is in the tree and in the memory map. That is also why the payload can be load-path agnostic — a raw
arm64 `Image`, the FDT and the arguments at a fixed offset, whatever gets them into RAM.

## 2. What is already proven on SM8750

This is not a from-zero port any more. The kernel, the drivers and the GPU have public, device-verified results on
this SoC:

| Work | SoC / board | State |
|---|---|---|
| `IssacL04/UniXNU-Pagani_SM8750` (OnePlus 13T) | SM8750 | build 63: **system startup and touch confirmed on the phone**, 8 of 8 CPUs, 4 GiB, read-only UFS root; `fastboot boot`, no flash. Display and input `device-verified`, storage `host-verified`; GPU, network and audio still `planned` |
| `RigoLigoRLC/unixnu-sm-t733` (Galaxy Tab S7 FE) | SM7325, Samsung | desktop, touch, keys, brightness, shutdown, USB diagnostics, persistent Data, Adreno6xx through `AdrenoMetal`; its catalog record is marked invalid for a components/area mismatch, not for a boot result |
| `superboyyy/hakios-arm64e-dada` (Xiaomi 15) | SM8750 | iOS 26 arm64e on `xnu-12377.121.6` (our kernel line): home screen, touch, flash read/write, **Adreno 830 Metal** via metal2vulkan → Turnip → their kernel driver, speaker, battery, Bluetooth; Wi-Fi firmware flaky; camera and modem "not started" |
| `iola1999/hackios-redmi-k90-annibale` (Redmi K90) | SM8750 | iOS 26.6.2 at 120 Hz, Adreno 830 Metal, Wi-Fi + Bluetooth, UFS dual boot; roughly 90–100 fps on home-screen swipes |

Two of those four boot through UEFI (dada, annibale), which is the part this phone does not have. What transfers is
the part that matters: the SM8750 platform layer, the panel/touch/UFS drivers, and the A830 GPU path.

## 3. What this tree already vendors

`external/` holds the pieces, so this is assembly rather than a rewrite:

- `UniXNU/`, `Droid-VM-UniXNU/` — the build system and the device catalog the boards above are written against
  (schema 2: profiles → SoC layer → board).
- `NativeMobileDisplay/`, `NativeHidInput/`, `QcomGeniSpi` users — display and touch for a Qualcomm panel.
- `AdrenoPM4/`, `ArmSmmu4K/`, `NativeAdreno/`, `NativeMetal/`, `metal2vulkan/`, `mesa-droidvm/`,
  `virglrenderer/` — the Adreno command stream, the SMMU, Metal and its Vulkan translation.
- `userland/metal/patches/metal2vulkan/` — the dada patches, rebased on our pin (s25.md, "Metal translation").

The hosted kernel is built by `kernel/` with the same patch series and `XnuPrelink`; a bare-metal board takes the
same kernel and replaces only the platform layer.

## 4. The phone's own numbers

Read from the dump's `scratch_czf1/dtb_main.dtb` (356,665 bytes, FDT 17, 2,748 nodes), with `dtc -I dtb -O dts`:

| What | Value in the phone's DT |
|---|---|
| Root | `model = "Qualcomm Technologies, Inc. Kera IoT SoC"`, `compatible = "qcom,kera"`, `qcom,msm-id = <0x2db 0x10000>` |
| CPUs | 8 × `qcom,kryo`, `enable-method = "psci"` (the Oryon rule already plans 2 E + 6 P vCPUs, s25.md) |
| Interrupts | GICv3 `0x17100000` + redistributors `0x17180000` (2 MiB), ITS `0x17140000`; arch timer present |
| Storage | `ufshc@1d84000` (`qcom,ufshc`), PHY `0x1d80000` |
| Console fallback | `qup_uart@894000`, `qcom,geni-debug-uart`, IRQ 587, `/aliases/serial0` |
| PMIC | SPMI `0xc42d000` with `pm8550`, `pmxr2230`, `pm8550vs` (GDSCs included) |
| Remote processors | adsp `0x03000000`, cdsp `0x32300000`, mss `0x04080000`, wpss `0x97000000` (`qcom,kera-*-pas`) |
| Memory | `memory { reg = <0 0 0 0> }` — a placeholder; the loader fills the window |

Board facts the base DT does not carry, from the port record (docs/forks.md and s25.md): panel
`PA1_S6E3FAE_AMB616FL03`, touch `stm,stm_ts_spi`, haptics `cirrus,cs40l26a`.

**Gap:** the dump has no `dtbo.img`, so the panel timings, the touch SPI lines/rails and the regulator names are
not measured here yet. They are in the phone's `dtbo` partition; pulling it read-only is the first task below.

## 5. Milestones

| Milestone | Where | Done when |
|---|---|---|
| **M0** — board and payload on the host | PC only | a `pa1q` board on the SM8750 layer builds a kernel and a payload; every measured input is in the board file; the Apple DT it emits has the panel, GIC, timer, UFS and memory window, and a host check reads it back |
| **M1** — the load and the panel | phone, one boot | `-v` output on the panel (or the UART), no panic, ten minutes stable. This is "screen on the first step" |
| **M2** — touch and keys | phone | taps land on the probe screen; volume/power keys report |
| **M3** — UFS root and Data | phone | SpringBoard's home screen on a real root. From here this track can replace the hosted session |
| **M4** — GPU | phone | Adreno 830 through `AdrenoPM4` + `ArmSmmu4K` + `NativeAdreno` + metal2vulkan; 120 Hz, no frame over budget |
| **M5** — the rest | phone | audio (WCD9385/WSA8835), Bluetooth and Wi-Fi (WCN7850), camera, cellular last |

Each milestone is its own boot, and each boot ends in a power cycle, not a flash.

## 6. Limits, stated plainly

- **Cellular** is not done on any SM8750 XNU port. The modem is a remoteproc (`mss@04080000`) that Android brings
  up with PIL and then speaks QMI to; that has to be reimplemented before a call is possible.
- **Camera** is "not started" on every SM8750 port found.
- **GPU after Linux dies:** kgsl's firmware load and the zap handshake live in the Android kernel today. They have
  to be reimplemented, and the dada/annibale A830 work is the reference, but their code is not ours.
- **App Store / Apple ID** needs the Secure Enclave. No non-Apple port has it, and the hosted model does not either.
- **Thermals and battery:** 8 cores and an Adreno at 120 Hz with no Android thermal daemon is a phone that gets hot.
- **The load path is the one unknown I cannot test from here:** whether this Android kernel allows `kexec` /
  `kexec_load` (or a module that does the same) under KernelSU. If it does, M1 is one boot away; if not, the jump
  is the first thing to port.

## 7. Where the work starts

1. Pull `dtbo` from the phone (read-only) and read the panel, touch and regulator nodes into the board file.
2. Fetch and pin the SM8750 layer and the Pagani board as the base (`external/UniXNU`, schema 2), and write the
   `pa1q` board over it: SoC `sm8750`, 1080x2340 panel, `stm,stm_ts_spi`, UFS LUNs, the memory window.
3. Build the kernel and payload on the host; check the emitted DT and the ADT against the numbers above.
4. One phone boot: M1.
