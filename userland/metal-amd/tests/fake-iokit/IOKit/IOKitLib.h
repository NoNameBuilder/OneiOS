/* A stand-in for Darwin's IOKitLib.h, so that vdrm_darwin.c compiles on Linux for the host test
 * (userland/metal-amd/tests/vdrm_darwin_test.c implements the functions as a fake kernel user client).
 * The error codes are IOKit's (IOReturn.h). SPDX-License-Identifier: MIT */
#ifndef ONEIOS_FAKE_IOKITLIB_H
#define ONEIOS_FAKE_IOKITLIB_H
#include <stddef.h>
#include <stdint.h>

typedef int kern_return_t;
typedef unsigned int mach_port_t;
typedef mach_port_t io_object_t, io_service_t, io_connect_t, task_port_t;
typedef void *CFMutableDictionaryRef;
typedef const void *CFDictionaryRef;

#define KERN_SUCCESS 0
#define kIOReturnError ((kern_return_t)0xe00002bc)
#define kIOReturnNoMemory ((kern_return_t)0xe00002bd)
#define kIOReturnNoResources ((kern_return_t)0xe00002be)
#define kIOReturnBadArgument ((kern_return_t)0xe00002c2)
#define kIOReturnUnsupported ((kern_return_t)0xe00002c7)
#define kIOReturnTimeout ((kern_return_t)0xe00002d6)
#define kIOReturnNotReady ((kern_return_t)0xe00002d8)

extern const mach_port_t kIOMainPortDefault;
mach_port_t mach_task_self(void);
CFMutableDictionaryRef IOServiceMatching(const char *name);
io_service_t IOServiceGetMatchingService(mach_port_t main_port, CFDictionaryRef matching);
kern_return_t IOServiceOpen(io_service_t service, task_port_t task, uint32_t type, io_connect_t *connect);
kern_return_t IOServiceClose(io_connect_t connect);
kern_return_t IOObjectRelease(io_object_t object);
kern_return_t IOConnectCallStructMethod(mach_port_t connection, uint32_t selector, const void *input, size_t input_size,
                                        void *output, size_t *output_size);
#endif
