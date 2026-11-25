// SPDX-License-Identifier: MIT
//
// OneiOS: the room metal2vulkan keeps ahead in its pools, from a boot argument. Appended to the Metal plug-in's
// NativeAdrenoMetalPlugin.mm by userland/metal/build.sh (stage), as DisplayWide.inc is.
//
// Why (docs/s24-fps-heat-camera.md, the 2026-11-24 gpumem session): the guest's free memory fell to 1-36 MiB with 2-3 GiB
// of it in GPU blobs (every blob is wired guest RAM), and that pressure drove the pageout, the compressor and the unmap
// storm that kept every host CPU at its top clock. backboardd's GM row: `p 13/440M u 264M`, 176 MiB of its pooled blocks
// unplaced. The runtime's room keeper (memory.rs keep_room) keeps one free range of half the pool's next block (32 MiB
// once a pool has 64 MiB) in each pool the encodings place in, the main pool of images among them, and makes another
// 64 MiB block whenever no block has that range in one piece, however many bytes the blocks hold free in pieces.
// METAL2VULKAN_KEEP_ROOM_MIB caps that range; it was an A/B file only (/private/var/ngpu/debug/keep-room-mib, read by
// the plug-in when the device opens, which still wins). A placement past the room makes its block when it is placed,
// as any placement that finds no room does.
//
// Switch: boot argument oneios-keep-room=MIB (boot-args-add=oneios-keep-room=MIB in the image set's xnuhost.conf), 8
// without it; oneios-keep-room=0 leaves the runtime's half block (docs/switches.md). The proof is the GM row
// (CommandPoolRow.inc): `p` MiB nearer `u`.
//
// What it logs (iOS's log): nothing; the GM row and `oneios-gpu-kinds` show the bytes.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysctl.h>

__attribute__((constructor)) static void oneiosKeepRoom() {
    unsigned mib = 8;
    char args[1024] = {};
    size_t size = sizeof(args) - 1;
    if (!sysctlbyname("kern.bootargs", args, &size, nullptr, 0)) {
        if (const char *word = strstr(args, "oneios-keep-room=")) mib = (unsigned)strtoul(word + strlen("oneios-keep-room="), nullptr, 10);
    }
    if (!mib) return;
    char text[16];
    snprintf(text, sizeof text, "%u", mib);
    setenv("METAL2VULKAN_KEEP_ROOM_MIB", text, 0);   // a process's own setting, and the A/B file read later, win
}
