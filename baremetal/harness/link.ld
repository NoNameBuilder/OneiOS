/* SPDX-License-Identifier: BSD-2-Clause-Patent
 *
 * One image, five islands: the host's code and data at 0x40000000, a fake
 * framebuffer at 0x47000000, the payload in the window at 0x48000000, the
 * boot_args and the magic after it, and the stub at 0x49000000. QEMU loads
 * every PT_LOAD at its physical address, so what runs is what the linker said.
 */
ENTRY(_start)
SECTIONS
{
    . = 0x40000000;
    .text   : { *(.text) }
    .rodata : { *(.rodata*) }
    .data   : { *(.data) }
    .bss    : { *(.bss) *(COMMON) }

    . = 0x47000000;
    .fb : { *(.fb) }

    . = 0x48000000;
    .payload : { *(.payload) }

    . = 0x48070000;
    .args : { *(.args) }

    . = 0x48080000;
    .magic : { *(.magic) }

    . = 0x49000000;
    .stub : { *(.stub) }
}
