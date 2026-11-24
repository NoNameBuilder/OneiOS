/*
 * Copyright © 2026 OneiOS contributors
 * SPDX-License-Identifier: MIT
 *
 * The one call the AIR Metal library (air_metal.c) makes of the A56's GPU driver: contract K3 of
 * the Galaxy A56 GPU port (docs/a56-gpu.md). The guest Mesa stage defines it, in the Mesa tree it
 * builds for Darwin (radeonsi over Mesa's amdgpu winsys and the virtio amdgpu native context, its
 * Darwin vdrm opening the A56 accelerator with open type 0x4149). This header is only its
 * declaration, so the Metal layer does not depend on where the guest Mesa stage puts it.
 *
 * air_metal.c calls it only when Mesa was configured with -Doneios-air-screen=radeonsi, which
 * userland/metal-amd/air/build.sh sets when the tree it is given defines the function; a tree
 * without it builds a library whose rwa_device_open fails with a sentence instead.
 */
#ifndef ONEIOS_RADEONSI_SCREEN_H
#define ONEIOS_RADEONSI_SCREEN_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

struct pipe_screen;

/*
 * The A56's radeonsi screen, made once per call (the Metal layer makes one per Metal device, one
 * per process). NULL when there is no GPU, with the reason in `why` (at most why_size bytes,
 * NUL-terminated): no accelerator published (a kernel without oneios-gpu-amd=1), no AMDGPU capset,
 * the open refused, radeonsi declining the device, ...
 */
struct pipe_screen *oneios_radeonsi_screen_create(char *why, size_t why_size);

#ifdef __cplusplus
}
#endif

#endif
