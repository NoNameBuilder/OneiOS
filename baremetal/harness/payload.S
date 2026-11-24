// SPDX-License-Identifier: BSD-2-Clause-Patent
//
// The harness's stand-in for XNU: the first code to run after the stub. It
// checks the state the stub promised and the data the host left in the window,
// then switches the machine off. Every check is one the real payload depends
// on, and every failure names itself on the UART.

    .include "handoff.inc"

    .section .payload, "ax", %progbits
    .globl payload_entry
payload_entry:
    mov     x20, x0

    mrs     x9, CurrentEL
    lsr     x9, x9, #2
    cmp     x9, #1
    b.ne    fail_el
    mrs     x9, sctlr_el1
    tst     x9, #1
    b.ne    fail_mmu
    tst     x9, #4
    b.ne    fail_dcache
    tst     x9, #(1 << 12)
    b.ne    fail_icache

    ldr     x9, =ARGS_PA
    cmp     x20, x9
    b.ne    fail_args
    ldrh    w9, [x20, #BA_REVISION]
    cmp     w9, #2
    b.ne    fail_revision
    ldrh    w9, [x20, #BA_VERSION]
    cmp     w9, #2
    b.ne    fail_version
    ldr     x9, [x20, #BA_PHYSICALBASE]
    ldr     x10, =WINDOW_PA
    cmp     x9, x10
    b.ne    fail_base
    ldr     x9, [x20, #BA_MEMORYSIZE]
    ldr     x10, =WINDOW_SIZE
    cmp     x9, x10
    b.ne    fail_size
    ldr     x9, [x20, #BA_MEMORYSIZEACTUAL]
    cmp     x9, x10
    b.ne    fail_size_actual
    ldr     x9, [x20, #BA_DEVICETREE]
    cbnz    x9, fail_tree
    ldr     w9, [x20, #BA_COMMANDLINE]
    ldr     w10, =0x0000762d         // "-v"
    cmp     w9, w10
    b.ne    fail_cmdline

    // The window: written with the caches on, so it is here only if the stub
    // cleaned it to the point of coherency.
    ldr     x9, =MAGIC_PA
    ldr     x10, [x9]
    ldr     x11, =0x004b4f2d51314150 // "PA1Q-OK\0"
    cmp     x10, x11
    b.ne    fail_magic

    // The breadcrumb: the bar is in memory, and the word after it is not.
    ldr     x9, =FB_PA
    ldr     w10, [x9]
    ldr     w11, =0xFF0000FF
    cmp     w10, w11
    b.ne    fail_bar
    add     x9, x9, #(FB_BYTES / 16)
    ldr     w10, [x9]
    cbnz    w10, fail_bar_past

    ldr     x0, =msg_pass
    bl      puts
    b       poweroff

fail_el:         ldr x0, =tag_el;         b fail
fail_mmu:        ldr x0, =tag_mmu;        b fail
fail_dcache:     ldr x0, =tag_dcache;     b fail
fail_icache:     ldr x0, =tag_icache;     b fail
fail_args:       ldr x0, =tag_args;       b fail
fail_revision:   ldr x0, =tag_revision;   b fail
fail_version:    ldr x0, =tag_version;    b fail
fail_base:       ldr x0, =tag_base;       b fail
fail_size:       ldr x0, =tag_size;       b fail
fail_size_actual: ldr x0, =tag_size_actual; b fail
fail_tree:       ldr x0, =tag_tree;       b fail
fail_cmdline:    ldr x0, =tag_cmdline;    b fail
fail_magic:      ldr x0, =tag_magic;      b fail
fail_bar:        ldr x0, =tag_bar;        b fail
fail_bar_past:   ldr x0, =tag_bar_past;   b fail

fail:
    mov     x21, x0
    ldr     x0, =msg_fail
    bl      puts
    mov     x0, x21
    bl      puts
    mov     w0, #10
    bl      putc

poweroff:
    ldr     x0, =0x84000008          // PSCI SYSTEM_OFF
    hvc     #0
1:  wfi
    b       1b

// puts: x0 = NUL-terminated string, physical. Clobbers x9, x10, x11.
puts:
    ldr     x9, =0x09000000
1:  ldrb    w10, [x0], #1
    cbz     w10, 2f
3:  ldr     w11, [x9, #0x18]
    tbnz    w11, #5, 3b
    str     w10, [x9]
    b       1b
2:  ret

// putc: w0 = character. Clobbers x9, x11.
putc:
    ldr     x9, =0x09000000
1:  ldr     w11, [x9, #0x18]
    tbnz    w11, #5, 1b
    str     w0, [x9]
    ret

    .section .rodata
msg_pass: .asciz "PA1Q PASS: EL1, MMU and caches off, boot_args, window clean, breadcrumb\n"
msg_fail: .asciz "PA1Q FAIL: "
tag_el:          .asciz "not at EL1"
tag_mmu:         .asciz "MMU still on"
tag_dcache:      .asciz "D-cache still on"
tag_icache:      .asciz "I-cache still on"
tag_args:        .asciz "x0 is not boot_args"
tag_revision:    .asciz "revision"
tag_version:     .asciz "version"
tag_base:        .asciz "PhysicalBase"
tag_size:        .asciz "MemorySize"
tag_size_actual: .asciz "MemorySizeActual"
tag_tree:        .asciz "DeviceTree"
tag_cmdline:     .asciz "command line"
tag_magic:       .asciz "the window was not cleaned"
tag_bar:         .asciz "the breadcrumb is not in memory"
tag_bar_past:    .asciz "the breadcrumb wrote past its bar"
