// SPDX-License-Identifier: BSD-2-Clause-Patent
//
// The handoff ABI, as XNU takes it (Darwin 23+ pexpert/pexpert/arm64/boot.h).
// Every field is little-endian; the offsets are the ones Pagani's loader
// asserts, and the harness checks them byte for byte.
//
// In registers:
//   x0 = boot_args, physical          x1 = payload entry, physical
//   x2 = the window, physical         x3 = its size in bytes
//   x4 = framebuffer, physical (0)    x5 = its size in bytes
// At the branch: EL1, MMU off, D-cache off, I-cache off, DAIF masked, one
// core running. Nothing else is promised; only x0 is meaningful to XNU.

.equ BA_REVISION,          0x000   // u16
.equ BA_VERSION,           0x002   // u16
.equ BA_VIRTUALBASE,       0x008   // u64
.equ BA_PHYSICALBASE,      0x010   // u64
.equ BA_MEMORYSIZE,        0x018   // u64
.equ BA_TOP_OF_KERNEL_DATA,0x020   // u64
.equ BA_VIDEO,             0x028   // u64[6]: base, row-pixels?, row-bytes, width, height, depth
.equ BA_MACHINETYPE,       0x058   // u32
.equ BA_DEVICETREE,        0x060   // u64, the ADT
.equ BA_DEVICETREELENGTH,  0x068   // u32
.equ BA_COMMANDLINE,       0x06c   // char[1024]
.equ BA_BOOTFLAGS,         0x470   // u64
.equ BA_MEMORYSIZEACTUAL,  0x478   // u64
.equ BA_SIZE,              0x480

// The window this harness hands over. On the phone the injector picks it; the
// numbers here are only the harness's.
.equ WINDOW_PA,            0x48000000
.equ WINDOW_SIZE,          0x00100000
.equ ARGS_PA,              0x48070000
.equ MAGIC_PA,             0x48080000
.equ FB_PA,                0x47000000
.equ FB_BYTES,             0x00010000
.equ FB_ROW_BYTES,         0x00000200
.equ FB_WIDTH,             128
.equ FB_HEIGHT,            128
