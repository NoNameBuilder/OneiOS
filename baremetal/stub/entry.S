// SPDX-License-Identifier: BSD-2-Clause-Patent
//
// pa1q_handoff: the last few hundred instructions between a running Linux and
// a payload that believes iBoot just handed it the phone.
//
// Position independent on purpose: no globals, no literal pool, no stack, no
// return. The injector copies this blob anywhere executable, passes the six
// registers below and calls it like a function (a module can; an
// arbitrary-write injector jumps to it after cleaning its own code).
//
// In:  x0 = boot_args, physical      x1 = payload entry, physical
//      x2 = window, physical         x3 = window bytes
//      x4 = framebuffer, physical    x5 = framebuffer bytes
// Out: never. x0 still holds boot_args at the branch.
//
// What the caller must have done already, because none of it can be done from
// here once the MMU is off:
//   - parked the other CPUs (PSCI CPU_OFF, or a spin with IRQs masked);
//   - stopped or quiesced the DMA masters that write memory (the display's
//     scanout and UFS are the two that matter);
//   - copied the payload, the boot_args, the ADT, the ramdisk and the trust
//     cache into the window;
//   - cleaned the blob's own code out of the D-cache if it wrote it with the
//     MMU on (a module's text is already coherent).
//
// What it does: masks the interrupts, leaves a blue bar over the top sixteenth
// of the framebuffer (so a black screen is distinguishable from a stub that
// never ran), cleans the bar and the window to the point of coherency,
// invalidates the I-cache, clears SCTLR_EL1.M/C/I, and branches.

    .section .stub, "ax", %progbits
    .globl pa1q_handoff
    .type  pa1q_handoff, %function
pa1q_handoff:
    msr     daifset, #0xf

    // The breadcrumb: BGRA 0xFF0000FF is opaque blue. Fill FB_BYTES/16, the
    // same fraction the UEFI loader used, rounded down to a pair of words.
    cbz     x4, 2f
    lsr     x9, x5, #4
    bic     x9, x9, #7
    movz    w10, #0x00FF
    movk    w10, #0xFF00, lsl #16
    mov     x11, x4
1:  stp     w10, w10, [x11], #8
    subs    x9, x9, #8
    b.hi    1b
    // The panel reads memory, not our cache: clean the bar it must see.
    mov     x8, x4
    add     x9, x4, x5, lsr #4
    bl      pa1q_clean
2:
    // The window: the payload's code and the args the caller built with the
    // MMU and the caches on.
    mov     x8, x2
    add     x9, x2, x3
    bl      pa1q_clean

    // No instruction we are about to fetch may come from a stale I-cache line.
    ic      iallu
    dsb     sy
    isb

    // MMU, D-cache and I-cache off. The RES1 bits stay as the caller left them.
    mrs     x9, sctlr_el1
    bic     x9, x9, #1              // M
    bic     x9, x9, #(1 << 2)       // C
    bic     x9, x9, #(1 << 12)      // I
    msr     sctlr_el1, x9
    isb

    // Every translation we made is gone; the branch is physical.
    tlbi    vmalle1
    dsb     sy
    isb

    br      x1
    .size pa1q_handoff, . - pa1q_handoff

// Clean the D-cache over [x8, x9) to the point of coherency. x8 is rounded
// down to a cache line; x9 is exclusive and rounded up by the loop. Clobbers
// x10, x11 only, so x0..x7 survive.
    .globl pa1q_clean
    .type  pa1q_clean, %function
pa1q_clean:
    mrs     x10, ctr_el0
    ubfx    x10, x10, #16, #4
    mov     x11, #4
    lsl     x10, x11, x10           // DminLine bytes
    sub     x11, x10, #1
    bic     x8, x8, x11
1:  dc      civac, x8
    add     x8, x8, x10
    cmp     x8, x9
    b.lo    1b
    dsb     sy
    ret
    .size pa1q_clean, . - pa1q_clean
