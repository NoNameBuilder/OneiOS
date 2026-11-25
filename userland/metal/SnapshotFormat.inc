// SPDX-License-Identifier: MIT
//
// OneiOS: SpringBoard's app snapshots as JPEG or PNG, never ATX (an ASTC texture). Appended to the Metal plug-in's
// NativeAdrenoMetalPlugin.mm by userland/metal/build.sh (stage), as DisplayWide.inc is; it acts in SpringBoard only.
//
// Why: +[XBApplicationSnapshotManifestImpl _outputFormatForSnapshot:] (SplashBoard) chooses the file format of the
// picture SpringBoard takes of an app that leaves the screen (the app switcher, the next launch): 0 PNG, 1 and 2 ATX,
// 3 JPEG. A device with a wide-colour display that answers DeviceSupportsASTC gets ATX, and iOS compresses it on the
// CPU: fast with the vector code of Apple's cores, seconds a picture here, while the app waits for the answer until the
// scene-update watchdog (10 s) kills it. Ported from ios-garnet (rafsanbasunia/ios-garnet, userland/src/inject_sb.c
// snapshot_output_format, f6276db; MIT, the HakiOS authors). With xnuhost's astc=1 (the default since 2026-11-25:
// runtime/loader machine.rs set_featuresets keeps APPLE2 and MTL2,2) the device answers DeviceSupportsASTC, so this
// keeps the snapshots what they were with ASTC off: an opaque picture as JPEG, one that needs transparency as PNG.
//
// Switch: boot argument oneios-snapshot-astc=1 keeps iOS's own choice (docs/switches.md).
//
// What it logs (iOS's log, SpringBoard): once, "OneiOS snapshots: JPEG or PNG, not ATX (hooked)"; or "... no
// XBApplicationSnapshotManifestImpl" while SplashBoard is not loaded (tried again as each image loads).
#include <mach-o/dyld.h>
#include <objc/message.h>
#include <objc/runtime.h>
#include <os/log.h>
#include <string.h>
#include <sys/sysctl.h>

namespace oneios_snapshots {
static bool hooked;

static long format(id self, SEL selector, id snapshot) {
    (void)self;
    (void)selector;
    SEL opaque = sel_registerName("isImageOpaque");
    if (snapshot && class_getInstanceMethod(object_getClass(snapshot), opaque) &&
        ((BOOL (*)(id, SEL))objc_msgSend)(snapshot, opaque))
        return 3;   // JPEG
    return 0;       // PNG
}

static bool hook() {
    if (hooked) return true;
    Class manifest = objc_getClass("XBApplicationSnapshotManifestImpl");
    Method method = manifest ? class_getClassMethod(manifest, sel_registerName("_outputFormatForSnapshot:")) : nullptr;
    if (!method) return false;
    method_setImplementation(method, (IMP)format);
    hooked = true;
    os_log(OS_LOG_DEFAULT, "OneiOS snapshots: JPEG or PNG, not ATX (hooked)");
    return true;
}

static void added(const struct mach_header *, intptr_t) { (void)hook(); }
}   // namespace oneios_snapshots

__attribute__((constructor)) static void oneiosSnapshotFormat() {
    if (strcmp(getprogname(), "SpringBoard")) return;
    char args[1024] = {};
    size_t size = sizeof(args) - 1;
    if (!sysctlbyname("kern.bootargs", args, &size, nullptr, 0) && strstr(args, "oneios-snapshot-astc=1")) return;
    if (oneios_snapshots::hook()) return;
    os_log(OS_LOG_DEFAULT, "OneiOS snapshots: no XBApplicationSnapshotManifestImpl yet: tried again as images load");
    _dyld_register_func_for_add_image(oneios_snapshots::added);   // calls it for every loaded image too
}
