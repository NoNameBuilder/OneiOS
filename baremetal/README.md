# baremetal: the last step between Linux and XNU

`stub/entry.S` is the code that leaves the running Android kernel and enters an XNU-shaped payload at
EL1 with the MMU and the caches off. It is the one piece of the [pa1q bare-metal track](../docs/pa1q.md)
with no prior art on this phone: Pagani reaches the same point from UEFI at EL2, and this phone has no
UEFI and reaches it from Linux at EL1.

It is not a kexec. The phone's own kernel has `CONFIG_KEXEC` off, no `/dev/mem` and no `/proc/kcore`
(measured in `docs/pa1q.md`), so the jump is whatever code execution the root already has: a module
that calls it, or an arbitrary-write injector that jumps to it.

## The contract

| Register | Meaning |
|---|---|
| `x0` | `boot_args`, physical |
| `x1` | payload entry, physical |
| `x2` | the window the payload owns, physical |
| `x3` | its size in bytes |
| `x4` | framebuffer, physical (0 for none) |
| `x5` | framebuffer bytes |

At the branch: EL1, `SCTLR_EL1` M/C/I clear, DAIF masked, one core running. Only `x0` is meaningful to
XNU. `boot_args` is the Darwin 23+ ABI (`stub/handoff.inc`): 0x480 bytes, revision and version 2, the
video at 0x28, the ADT at 0x60, the command line at 0x6c, `MemorySizeActual` at 0x478.

The caller must have parked the other CPUs, quiesced the DMA masters that write memory (the display's
scanout and UFS are the two that matter), copied the payload, the args, the ADT, the ramdisk and the
trust cache into the window, and flushed the blob's own code out of the D-cache if it wrote it with the
MMU on (a module's text is already coherent).

The stub masks interrupts, paints a blue bar over the top sixteenth of the framebuffer (so a black
screen is distinguishable from a stub that never ran), cleans the bar and the window to the point of
coherency, invalidates the I-cache, clears M/C/I and branches. Pass `x4 = 0` if the framebuffer is
mapped Device: cache maintenance on Device memory is UNPREDICTABLE.

## The harness

`test.sh` builds one image with five islands: a host at 0x40000000 (an EL1 "Linux" that turns the MMU
and the caches on, identity-maps RAM and the UART, builds `boot_args`, writes a magic into the window
with the caches on, and calls the stub exactly as a module would), a fake framebuffer at 0x47000000,
the payload in the window at 0x48000000, the args and the magic after it, and the stub at 0x49000000.
The payload checks the exception level, that M/C/I are clear, every `boot_args` field it will rely on,
the magic and the bar, then switches QEMU off.

```sh
baremetal/test.sh
```

What it proves: the register ABI, the `boot_args` layout, the branch, that the payload runs with the
MMU off and can still use a device the host left initialized, and that a stub which forgets to clear M
or C is caught — comment either `bic` out and the payload names the failure.

What it cannot prove: QEMU does not model the data cache, so `dc civac` is a no-op there and the
"window was cleaned" check passes trivially. On the phone that clean is what makes the check true.

## Still to come

- **The injector.** A module calling the stub, or the R/W path that writes it and jumps. Both use the
  same stub; the difference is how the window is allocated and who parks the CPUs.
- **The Mach-O side.** Laying out a real `xnu.macho` in the window (segments, `boot_args`, the ADT,
  the ramdisk, the trust cache). Pagani's `DarwinBoot.c` is the reference, and its parser is a harness
  of its own.
- **The ADT.** The tree XNU actually reads (Apple's flat format), generated from the board's DTS.
