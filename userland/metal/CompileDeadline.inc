// SPDX-License-Identifier: MIT
//
// OneiOS: a longer compile deadline for the Metal clients that are not the render server. Appended to the Metal
// plug-in's NativeAdrenoMetalPlugin.mm by userland/metal/build.sh (stage), as DisplayWide.inc is; the runtime reads it
// through userland/metal/m2v-compile-deadline.patch (Budget::standard).
//
// Why (docs/s24-fps-heat-camera.md, the 2026-11-24 gpumem session): with the Camera app open, cameracaptured's compute
// pipelines failed `oneios-gpu-text: cameracaptured[62] problem compute pipeline ...: Compile(Budget(Time))` five times
// and more (function constants kMotionThreshold, kDirection_float, kIIRUpdateCoefficient): metal2vulkan's 20 s deadline
// for the translation on the creating thread, on a phone whose every core was at its top clock. The capture graph
// never started its stream (no `camera#0 q0` past the device's creation): the preview stayed black. A client that is
// not the render server waits for its pipeline at its own creation, off the frame path, so a longer wait costs a slower
// first use and nothing else; backboardd keeps the 20 s (a frame waits for its compiles).
//
// Switch: boot argument oneios-compile-s=SECONDS (boot-args-add=oneios-compile-s=S in the image set's xnuhost.conf), 120
// without it, 1-600; oneios-compile-s=0 keeps the runtime's 20 s everywhere (docs/switches.md).
//
// What it logs: nothing of its own; the proof is no `Compile(Budget(Time))` among the oneios-gpu-text lines.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysctl.h>

__attribute__((constructor)) static void oneiosCompileDeadline() {
    if (!strcmp(getprogname(), "backboardd")) return;
    unsigned seconds = 120;
    char args[1024] = {};
    size_t size = sizeof(args) - 1;
    if (!sysctlbyname("kern.bootargs", args, &size, nullptr, 0)) {
        if (const char *word = strstr(args, "oneios-compile-s=")) seconds = (unsigned)strtoul(word + strlen("oneios-compile-s="), nullptr, 10);
    }
    if (!seconds || seconds > 600) return;
    char text[16];
    snprintf(text, sizeof text, "%u", seconds);
    setenv("METAL2VULKAN_COMPILE_DEADLINE_S", text, 0);
}
