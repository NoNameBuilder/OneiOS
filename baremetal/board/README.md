# The pa1q board overlay

`pa1q-overlay.dts` is the part of the SM-S931B's board description that the Sun base tree does
not carry: the panel (`PA1_S6E3FAE_AMB616FL03`), its DSI rails, and the touch controller. It is
the input the bare-metal track needs before a board can build, and every value in it is measured
from the phone's own firmware rather than copied from the port record.

## Where the values come from

The phone's `dtbo` partition is not in the dump, but the overlay is not lost with it. The Sun
*graphics* DTB lives in `partitions/vm-bootsys_a`, and it carries the panel, touch and rail nodes
for this board. Two trees in that partition have the panel (the MTP and MTP-V8 variants); this
file was taken from the first.

- Base SoC tree: `dtb_9` ("Sun SoC", `qcom,msm-id = <0x26a 0x10000>`) of the `vendor_boot` DTB
  section. Its numbers are in [docs/pa1q.md](../../docs/pa1q.md) section 4.
- Panel, touch and rails: the Sun DTB in `partitions/vm-bootsys_a`, extracted with:

```sh
# every FDT in the partition, through dtc, then the one that has the panel
for off in $(grep -aob $'\xd0\x0d\xfe\xed' partitions/vm-bootsys_a | cut -d: -f1); do
    size=$(od -An -tu4 --endian=big -j $((off+4)) -N4 partitions/vm-bootsys_a | tr -d ' \n')
    dd if=partitions/vm-bootsys_a bs=1 skip=$off count=$size status=none \
        | dtc -I dtb -O dts - 2>/dev/null | grep -q PA1_S6E3FAE && echo "$off $size"
done
```

No live `/proc/device-tree` pull is needed: the dump already has everything the board file uses.

## What was measured

| Fact | Value |
|---|---|
| Panel | `PA1_S6E3FAE_AMB616FL03`, Samsung S6E3FAE, AMB616FL03 module |
| Resolution | 1080x2340 (`0x438` x `0x924`) |
| Modes | 120, 80, 60, 48, 30, 24, 10 Hz (one timing node each) |
| Transport | DSI command mode, DSC, 2 encoders, 540-pixel slices, 60-row slices |
| Reset | `qcom,platform-reset-gpio = <&tlmm 98 0>` |
| Pixels | `rgb_swap_rgb`, 30 bpp, DMA trigger `trigger_sw` |
| Touch | `stm_spi` on QUPv3 SE4 (`spi@a90000`), CS gpio 51, irq gpio 162, reset gpio 161, 4096x4096 |
| Rails | vddio 1.8 V, vdd 1.0-1.1 V, vci 3.0 V, tvdd 1.8 V |

The panel is DSI **command** mode, which matters for the display driver: the panel keeps its own
frame memory, so XNU can leave it lit across a slow path and the first screen does not depend on a
continuous video stream.

## The check

```sh
baremetal/board/check.sh
baremetal/board/check.sh --firmware /path/to/partitions/vm-bootsys_a
```

The first form compiles the overlay and asserts every number above (panel name, geometry, all
seven modes, DSC geometry, the reset line, the touch lines, the four rails). The second form also
finds the Sun DTB inside the firmware partition and proves the overlay's panel facts come from it.
Both run in about a second and need only `dtc`.

## What is still not here

- **Memory and reserved regions.** These come from the bootloader at runtime (`/memory` and
  `/reserved-memory`), not from a DTS, so they are not in this file; the window XNU owns is chosen
  by the loader, not by the board.
- **The ADT.** This DTS is the hardware description; the Apple device tree XNU reads is generated
  from it by the same code path as the hosted board (`runtime/loader/src/machine.rs`).
- **The injector.** See [../README.md](../README.md).
