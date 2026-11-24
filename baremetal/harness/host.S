// SPDX-License-Identifier: BSD-2-Clause-Patent
//
// The harness's stand-in for Linux: an EL1 image with the MMU and the caches
// on, an identity map, a fake framebuffer, and a payload it copies into the
// window. It builds the boot_args the way an injector would, writes a magic
// into the window with the caches on (so only a correct clean makes it
// visible), and calls the stub exactly as a module would: six registers, a
// normal call.
//
// QEMU -M virt: RAM at 0x40000000, PL011 at 0x09000000, entry at EL1 with the
// MMU off. Everything here is linked at its physical address.

    .include "handoff.inc"

    .section .text
    .globl _start
_start:
    ldr     x0, =stack_top
    mov     sp, x0
    bl      uart_init
    ldr     x0, =msg_start
    bl      puts

    bl      mmu_on
    ldr     x0, =msg_mmu
    bl      puts

    // boot_args: zero it, then fill the fields the payload checks.
    ldr     x0, =ARGS_PA
    mov     x1, #0
    mov     x2, #(BA_SIZE / 16)
1:  stp     x1, x1, [x0], #16
    subs    x2, x2, #1
    b.ne    1b

    ldr     x0, =ARGS_PA
    mov     w1, #2
    strh    w1, [x0, #BA_REVISION]
    strh    w1, [x0, #BA_VERSION]
    ldr     x1, =WINDOW_PA
    str     x1, [x0, #BA_VIRTUALBASE]
    str     x1, [x0, #BA_PHYSICALBASE]
    ldr     x1, =WINDOW_SIZE
    str     x1, [x0, #BA_MEMORYSIZE]
    str     x1, [x0, #BA_MEMORYSIZEACTUAL]
    ldr     x1, =MAGIC_PA
    str     x1, [x0, #BA_TOP_OF_KERNEL_DATA]
    ldr     x1, =FB_PA
    str     x1, [x0, #BA_VIDEO]
    mov     w1, #1
    str     x1, [x0, #BA_VIDEO + 8]
    ldr     w1, =FB_ROW_BYTES
    str     w1, [x0, #BA_VIDEO + 16]
    ldr     w1, =FB_WIDTH
    str     w1, [x0, #BA_VIDEO + 24]
    ldr     w1, =FB_HEIGHT
    str     w1, [x0, #BA_VIDEO + 32]
    mov     w1, #32
    str     w1, [x0, #BA_VIDEO + 40]
    mov     w1, #0x762d             // "-v\0\0"
    str     w1, [x0, #BA_COMMANDLINE]
    mov     w1, #7                  // verbose
    str     x1, [x0, #BA_BOOTFLAGS]

    // The magic: a dirty cache line, not memory, until the stub cleans it.
    ldr     x1, =MAGIC_PA
    ldr     x2, =0x004b4f2d51314150 // "PA1Q-OK\0"
    str     x2, [x1]

    ldr     x0, =msg_call
    bl      puts

    ldr     x0, =ARGS_PA
    ldr     x1, =payload_entry
    ldr     x2, =WINDOW_PA
    ldr     x3, =WINDOW_SIZE
    ldr     x4, =FB_PA
    ldr     x5, =FB_BYTES
    ldr     x9, =pa1q_handoff
    blr     x9

    ldr     x0, =msg_returned
    bl      puts
2:  wfi
    b       2b

// Identity map: L1[0] is 0x00000000 as Device-nGnRnE (the UART), L1[1] is
// 0x40000000 as Normal WB inner-shareable (RAM). 1 GiB blocks, 39-bit VA.
mmu_on:
    ldr     x0, =l1_table
    mov     x1, #0
    mov     x2, #(4096 / 16)
1:  stp     x1, x1, [x0], #16
    subs    x2, x2, #1
    b.ne    1b

    ldr     x0, =l1_table
    mov     x1, #0x401              // 0x00000000, block, AF, AttrIdx 0
    str     x1, [x0]
    ldr     x1, =0x40000705         // 0x40000000, block, AF, SH, AttrIdx 1
    str     x1, [x0, #8]

    ldr     x1, =0xFF00             // MAIR: 0 = Device nGnRnE, 1 = Normal WB
    msr     mair_el1, x1
    ldr     x1, =0x803519           // T0SZ 25 (39-bit), WB/WB, inner, 4K, EPD1
    msr     tcr_el1, x1
    msr     ttbr0_el1, x0
    dsb     sy
    tlbi    vmalle1
    dsb     sy
    isb
    ic      iallu
    dsb     sy
    isb
    mrs     x1, sctlr_el1
    orr     x1, x1, #1              // M
    orr     x1, x1, #4              // C
    orr     x1, x1, #(1 << 12)      // I
    msr     sctlr_el1, x1
    isb
    ret

uart_init:
    ldr     x1, =0x09000000
    mov     w2, #0x301              // UARTEN | TXE | RXE
    str     w2, [x1, #0x30]
    mov     w2, #0x70               // 8N1, FIFOs
    str     w2, [x1, #0x2c]
    ret

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

    .section .rodata
msg_start:    .asciz "PA1Q host: EL1, MMU off, about to turn it on\n"
msg_mmu:      .asciz "PA1Q host: MMU and caches on, building boot_args\n"
msg_call:     .asciz "PA1Q host: calling the stub\n"
msg_returned: .asciz "PA1Q host: FAIL, the stub returned\n"

    .section .data
    .balign 4096
l1_table:
    .space 4096

    .section .fb, "aw", %progbits
    .space FB_BYTES

    .section .args, "aw", %progbits
    .space BA_SIZE

    .section .magic, "aw", %progbits
    .space 16

    .section .bss
    .balign 16
    .space 4096
stack_top:
