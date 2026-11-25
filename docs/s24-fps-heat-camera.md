# S24 Ultra: GPU memory, animation rate and camera

This local backport is based on upstream commit `e84a46e` and retains this branch's existing Metal shader-fix patch
staging. The changes are individually switchable in [switches.md](switches.md). They are aimed at the Galaxy S24 Ultra
running iOS 27.0.1 (24A446).

## Evidence and changes

The 2026-11-24 `gpumem` session recorded guest free memory falling to 1–36 MiB while GPU blobs used about 2–3 GiB.
The pageout/compressor activity and repeated page faults kept the host CPUs busy. In the render server, the Metal
memory census reported 440 MiB of pooled blocks with 264 MiB placed. The pool-room cap lowers unused reserve per pool.

The same session showed animations requesting 20–120 updates/s with 30 preferred while the virtual display advertised
24–120 Hz. The display-link hook raises only links owned by UIKit/AnimationKit's in-process animation manager to the
display's rate, and the display switch raises its advertised lower bound to 60 Hz. Other links and the fixed-refresh
switch keep their existing behavior.

ASTC is exposed in the guest product feature list so CoreUI and Core Animation can send ASTC textures through the
Adreno Metal path. SpringBoard's snapshot hook keeps opaque snapshots JPEG and transparent snapshots PNG, avoiding
the CPU ASTC snapshot path. The A56 host entry keeps ASTC disabled.

The camera's observed Metal pipeline failures were `Compile(Budget(Time))` at the runtime's 20-second deadline. A
120-second deadline is given to Metal clients except `backboardd`, whose pipeline creation is on the frame path. The
capture stream also posts its Start/Stop lifecycle notifications behind its own switch.

## Camera names checked against iOS 27

The names already used by the capture plug-in match 24A446's CMCaptureCore: `VideoOutputConfigurations`,
`VideoOutputsEnabled`, `VideoOutputHandlers`, nested `PixelBufferHandler`, `BufferPool`, and the outputs
`PrimaryScaler`, `SecondaryScaler`, and `StillImage`. No guessed aliases were added. iOS 27 also names additional
outputs such as `IntermediateTap`, `FrontEndPyramidScale0`, `StillImageRaw`, and `PointCloud`; unsupported output names
continue to be logged once and skipped.

## Validation state

No tests, builds, or phone boots were run for this local backport. Upstream `e84a46e` reports host validation for its
own tree, but that does not validate this branch's combined changes or prove the phone behavior. The switches remain
unverified on-device; the proof lines for the next boot are listed in [switches.md](switches.md), `docs/gpu.md` 2.5b,
and `docs/hardware.md` 7.
