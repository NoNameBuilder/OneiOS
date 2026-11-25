/*
 * SPDX-License-Identifier: MIT
 *
 * OneiOSCaptureDevice: the FigCaptureDevice of the phone's cameras for iOS 27's capture server.
 *
 * Ported from iOSPlus10 userland/camera/camera_device.c, which is ported from HakiOS userland/src/camera_device.c
 * (MIT License, Copyright (c) 2025-2026 the HakiOS authors). What is ours: the iOS 27 entry point, the cameras the
 * kernel lists, the formats read from the device's own AVCaptureSession.plist, finished NV12 frames in place of
 * the raw sensor and its software ISP, the user client in place of a /dev node.
 *
 * How it is reached. CMCapture 27 probes for a camera driver; for AppleH9CamIn (drivers/camera/OneiOSCameraService.cpp)
 * it dlopen()s /System/Library/MediaCapture/H9ISP.mediacapture (this dylib, shown there by hosted patch 0056) and
 * hands H9ISPCaptureDeviceCreate to BWFigCaptureDeviceVendor, which calls it for every device it makes
 * (-[BWFigCaptureDeviceVendor _createDevice:reason:clientPID:figCaptureDevice:]: x0 the allocator, x1 the options,
 * x2 where the device goes; read in the 24A446 cache). No method of CMCapture is replaced.
 *
 * The objects are CoreMedia derived objects. Their layout, read in H16ISP.mediacapture of the 24A446 cache
 * (kH16ISPCaptureDeviceBaseClass, kH16ISPCaptureDeviceClass, kH16ISPCaptureStreamBaseClass, kH16ISPCaptureStreamClass),
 * is the one HakiOS used on iOS 16.5:
 *   base    version 1, storage size, equal, invalidate, finalize, copyDebugDescription, copyProperty, setProperty, ...
 *   device  version 3: ProcessingSessionCreate, RequestStreams, RelinquishStreams, PerformPearlCalibration
 *   stream  version 2: Start, Stop, AddAttachmentsToSampleBuffer
 * The property names are HakiOS's (iOS 16); which of them iOS 27 asks and which it asks that are not answered is
 * written to the log once per name ("ONEIOS CAMERA: ios: ... no answer for ..."): that list is the next step.
 *
 * A frame: the kernel's newest (NV12, at most 1920x1080, BT.601 full range) is scaled into a buffer of the pool
 * the graph gave each output (VideoOutputConfigurations' BufferPool) and handed to that output's
 * PixelBufferHandler with the host time of its arrival. 420f, 420v, x420, x422 and BGRA buffers are filled
 * (oneios_camera_scale.c); others are said once and skipped. The camera is opened by the graph's Start only. The
 * stream posts StreamStarting, StreamStarted, StreamStopping and StreamStopped as H10ISP's does (from Jesvi-iOS).
 *
 * The stream formats are the plist's named formats and their companions with the keys CMCapture's resolver compares
 * (oneios_camera_formats.c), and each stream answers ModuleInfo with its sensor ID (CameraSetup.plist DefaultSensorIDs).
 * Run in iOS on the phone since 2026-11-22 (userland/camera/README.md has what each boot showed); the Mac test
 * (tests/host-mac.sh) runs it against macOS 27's own CMCapture.
 */
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysctl.h>
#include <unistd.h>
#include "OneiOSCamera.h"
#include "oneios_camera_formats.h"
#include "oneios_camera_proto.h"
#include "oneios_camera_reader.h"

/* ---- CoreMedia's derived objects, as CMCapture 27 lays them out (see the top) ---- */
typedef struct OpaqueCMBaseObject *CMBaseObjRef;
typedef const void *CMBaseClassIDRef;

struct base_class {
	uint64_t version;
	uint64_t storage_size;
	Boolean (*equal)(CMBaseObjRef, CMBaseObjRef);
	OSStatus (*invalidate)(CMBaseObjRef);
	void (*finalize)(CMBaseObjRef);
	CFStringRef (*copy_debug_description)(CMBaseObjRef);
	OSStatus (*copy_property)(CMBaseObjRef, CFStringRef, CFAllocatorRef, void *);
	OSStatus (*set_property)(CMBaseObjRef, CFStringRef, CFTypeRef);
	OSStatus (*notification_barrier)(CMBaseObjRef);
	const void *reserved[3];
};
struct device_class {
	uint64_t version;               /* 3 */
	void *processing_session_create;
	OSStatus (*request_streams)(CMBaseObjRef, CFArrayRef, CFDictionaryRef);
	OSStatus (*relinquish_streams)(CMBaseObjRef, CFArrayRef, CFDictionaryRef);
	void *pearl_calibration;
	void *reserved[3];
};
struct stream_class {
	uint64_t version;               /* 2 */
	OSStatus (*start)(CMBaseObjRef);
	OSStatus (*stop)(CMBaseObjRef);
	OSStatus (*add_attachments)(CMBaseObjRef, CMSampleBufferRef, CFDictionaryRef);
	void *reserved[2];
};
struct vtable {
	const void *reserved;
	const struct base_class *base;
	const void *derived;
};

#define ERR_PARAM               (-12780)
#define ERR_UNSUPPORTED         (-12782)
#define ERR_NOT_AVAILABLE       (-12783)

static OSStatus (*derived_create)(CFAllocatorRef, const struct vtable *, CMBaseClassIDRef, CMBaseObjRef *);
static void *(*derived_storage)(CMBaseObjRef);
static CMBaseClassIDRef (*device_class_id)(void);
static CMBaseClassIDRef (*stream_class_id)(void);
static CFStringRef port_back, port_front;
static CFStringRef md_attachment;       /* kFigCaptureSampleBufferAttachmentKey_MetadataDictionary */
static CFStringRef md_port;             /* kFigCaptureStreamMetadata_PortType */
static CFStringRef md_sensor_id;        /* kFigCapturePropertyValue_SensorID ("sensorID"): ModuleInfo's key */
static CFStringRef wb_keys[3];          /* kFigCaptureStreamWhiteBalanceGainKey_Red, _Green, _Blue (CMCaptureCore) */

static CFStringRef
symbol_string(const char *name)
{
	CFStringRef *p = dlsym(RTLD_DEFAULT, name);

	return p != NULL ? *p : NULL;
}

static bool
find_functions(void)
{
	derived_create = dlsym(RTLD_DEFAULT, "CMDerivedObjectCreate");
	derived_storage = dlsym(RTLD_DEFAULT, "CMBaseObjectGetDerivedStorage");
	device_class_id = dlsym(RTLD_DEFAULT, "FigCaptureDeviceGetClassID");
	stream_class_id = dlsym(RTLD_DEFAULT, "FigCaptureStreamGetClassID");
	port_back = symbol_string("kFigCapturePortType_BackFacingCamera");
	/* The iPhone 17 Pro's front camera is a super-wide one: V54's AVCaptureSession.plist builds "Front Camera" and "Front Ultra
	 * Wide Camera" from a PortTypeFrontSuperWide stream (iOS's log, 2026-11-23: "unable to find a stream with port type
	 * PortTypeFrontSuperWide" with our PortTypeFront stream there). */
	port_front = symbol_string("kFigCapturePortType_FrontFacingSuperWideCamera");
	md_attachment = symbol_string("kFigCaptureSampleBufferAttachmentKey_MetadataDictionary");
	md_port = symbol_string("kFigCaptureStreamMetadata_PortType");
	md_sensor_id = symbol_string("kFigCapturePropertyValue_SensorID");
	wb_keys[0] = symbol_string("kFigCaptureStreamWhiteBalanceGainKey_Red");
	wb_keys[1] = symbol_string("kFigCaptureStreamWhiteBalanceGainKey_Green");
	wb_keys[2] = symbol_string("kFigCaptureStreamWhiteBalanceGainKey_Blue");
	if (port_back == NULL) {
		port_back = CFSTR("PortTypeBack");
	}
	if (port_front == NULL) {
		port_front = CFSTR("PortTypeFrontSuperWide");
	}
	oneios_camera_say("CoreMedia: CMDerivedObjectCreate %s, FigCaptureDeviceGetClassID %s, FigCaptureStreamGetClassID %s, "
	    "metadata key %s", derived_create ? "found" : "MISSING", device_class_id ? "found" : "MISSING",
	    stream_class_id ? "found" : "MISSING", md_attachment ? "found" : "missing");
	return derived_create && derived_storage && device_class_id && stream_class_id;
}

/* ---- small helpers ---- */
static CFNumberRef
number_i(int64_t v)
{
	return CFNumberCreate(NULL, kCFNumberSInt64Type, &v);
}

static CFNumberRef
number_f(double v)
{
	return CFNumberCreate(NULL, kCFNumberDoubleType, &v);
}

static CFMutableDictionaryRef
new_dict(void)
{
	return CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
}

/* Puts `value` under `key` and releases it. */
static void
put(CFMutableDictionaryRef d, const char *key, CFTypeRef value)
{
	CFStringRef k = CFStringCreateWithCString(NULL, key, kCFStringEncodingUTF8);

	if (value != NULL) {
		CFDictionarySetValue(d, k, value);
		CFRelease(value);
	}
	CFRelease(k);
}

static bool
key_is(CFStringRef key, const char *name)
{
	char buffer[160];

	return key != NULL && CFGetTypeID(key) == CFStringGetTypeID() &&
	    CFStringGetCString(key, buffer, sizeof(buffer), kCFStringEncodingUTF8) && strcmp(buffer, name) == 0;
}

static double
number_of(CFTypeRef v, double otherwise)
{
	double n = otherwise;

	if (v != NULL && CFGetTypeID(v) == CFNumberGetTypeID()) {
		CFNumberGetValue(v, kCFNumberDoubleType, &n);
	} else if (v != NULL && CFGetTypeID(v) == CFBooleanGetTypeID()) {
		n = CFBooleanGetValue(v);
	}
	return n;
}

static CFMutableSetRef said_names;
static pthread_mutex_t said_lock = PTHREAD_MUTEX_INITIALIZER;

/* Says `what key = value` the first time this pair of what and key comes. */
static void
say_once(const char *what, CFStringRef key, CFTypeRef value)
{
	CFStringRef tag = CFStringCreateWithFormat(NULL, NULL, CFSTR("%s %@"), what, key);
	char name[160] = "?", text[240] = "";
	bool first;

	pthread_mutex_lock(&said_lock);
	if (said_names == NULL) {
		said_names = CFSetCreateMutable(NULL, 0, &kCFTypeSetCallBacks);
	}
	first = !CFSetContainsValue(said_names, tag);
	CFSetAddValue(said_names, tag);
	pthread_mutex_unlock(&said_lock);
	CFRelease(tag);
	if (!first) {
		return;
	}
	if (key != NULL) {
		CFStringGetCString(key, name, sizeof(name), kCFStringEncodingUTF8);
	}
	if (value != NULL) {
		CFStringRef d = CFCopyDescription(value);

		if (d != NULL) {
			CFStringGetCString(d, text, sizeof(text), kCFStringEncodingUTF8);
			CFRelease(d);
		}
		for (char *c = text; *c; c++) {
			*c = *c == '\n' ? ' ' : *c;
		}
	}
	oneios_camera_say("%s %s%s%s", what, name, value ? " = " : "", text);
}

/* ---- the properties each object lists (SupportedPropertiesDictionary: { name: { PropertyType, ReadWriteStatus } }) ---- */
struct prop {
	const char *name, *type, *access;
};

static const struct prop stream_props[] = {
	/* ModuleInfo must be listed: BWFigCaptureStream asks it with requireSupported, and answers -12787 itself for a name
	 * that is not here (phone 2026-11-23: "failed to copy ModuleInfo (err=-12787)") */
	{ "PortType", "String", "ReadOnly" }, { "ModuleInfo", "Collection", "ReadOnly" }, { "UniqueID", "String", "ReadOnly" }, { "NonLocalizedName", "String", "ReadOnly" },
	/* asked with requireSupported too, when CMCapture makes the source (its DefaultWhiteBalanceGains): phone 2026-11-23, "failed to
	 * copy WhiteBalanceGain (err=-12787)" for both streams */
	{ "WhiteBalanceGain", "Collection", "ReadOnly" },
	{ "SupportedFormatsArray", "Collection", "ReadOnly" }, { "FocalLength", "Number", "ReadOnly" },
	{ "LensFNumber", "Number", "ReadOnly" }, { "PixelSize", "Number", "ReadOnly" },
	{ "MaxZoomFactor", "Number", "ReadOnly" }, { "SupportedColorSpaces", "Collection", "ReadOnly" },
	{ "HasAutoFocusPositionSensor", "Boolean", "ReadOnly" }, { "Clock", "Clock", "ReadOnly" },
	{ "FormatIndex", "Number", "ReadWrite" }, { "MinimumFrameRate", "Number", "ReadWrite" }, { "MaximumFrameRate", "Number", "ReadWrite" },
	{ "MaximumAllowedFrameRate", "Number", "ReadWrite" }, { "VideoOutputConfigurations", "Collection", "ReadWrite" },
	{ "VideoOutputsEnabled", "Collection", "ReadWrite" }, { "VideoOutputHandlers", "Collection", "WriteOnly" },
	{ "DeferAdditionOfAttachments", "Boolean", "ReadWrite" }, { "TorchLevel", "Number", "ReadWrite" },
	{ "HighCurrentTorchEnabled", "Boolean", "ReadWrite" }, { "ColorSpace", "Number", "ReadWrite" }, { "ZoomFactor", "Number", "ReadWrite" },
	{ "ExposureBias", "Number", "ReadWrite" }, { "DeviceOrientation", "Number", "ReadWrite" }, { "FlashMode", "Number", "ReadWrite" },
	{ "StillImageCaptureNow", "Collection", "WriteOnly" }, { "LockAENow", "Boolean", "WriteOnly" }, { "UnlockAENow", "Boolean", "WriteOnly" },
	{ "LockAWBNow", "Boolean", "WriteOnly" }, { "UnlockAWBNow", "Boolean", "WriteOnly" }, { "MaxIntegrationTime", "Number", "ReadWrite" },
	{ "AEMaxGain", "Number", "ReadWrite" }, { "AEMinGain", "Number", "ReadWrite" }, { "FaceDetectionEnabled", "Boolean", "ReadWrite" },
	{ "VideoRecordingInProgress", "Boolean", "ReadWrite" }, { "MemoryPoolID", "Number", "ReadWrite" },
	{ "ChromaNoiseReductionEnabled", "Boolean", "ReadWrite" }, { "TemporalNoiseReductionEnabled", "Boolean", "ReadWrite" },
	{ "MultipleOutputSupportEnabled", "Boolean", "ReadWrite" }, { "VideoOutputsColorInfo", "Collection", "ReadWrite" },
	{ "RegionOfInterestForCameraControls", "Collection", "ReadWrite" }, { "AEMeteringMode", "Number", "ReadWrite" },
	{ "MaxFrameCoalescingDuration", "Number", "ReadWrite" }, { "LimitAERange", "Boolean", "ReadWrite" },
	{ "TimeMachineSuspendNow", "Boolean", "WriteOnly" }, { "TimeMachineResumeNow", "Boolean", "WriteOnly" },
	{ "AutoFocusNow", "Collection", "WriteOnly" }, { "ContinuousAutoFocusNow", "Collection", "WriteOnly" },
	{ "FocusNow", "Collection", "WriteOnly" }, { "LockAFNow", "Boolean", "WriteOnly" }, { "AFWindowParams", "Collection", "ReadWrite" },
	{ "AutoFocusMode", "Number", "ReadWrite" },
}, device_props[] = {
	{ "StreamArray", "Collection", "ReadOnly" }, { "Clock", "Clock", "ReadOnly" }, { "ClientIsActive", "Boolean", "ReadWrite" },
	{ "SupportedSynchronizedStreamsGroups", "Collection", "ReadOnly" }, { "MemoryPoolID", "Number", "ReadWrite" },
};

static CFDictionaryRef
supported(const struct prop *p, size_t n)
{
	CFMutableDictionaryRef all = new_dict();

	for (size_t i = 0; i < n; i++) {
		CFMutableDictionaryRef one = new_dict();

		put(one, "PropertyType", CFStringCreateWithCString(NULL, p[i].type, kCFStringEncodingUTF8));
		put(one, "ReadWriteStatus", CFStringCreateWithCString(NULL, p[i].access, kCFStringEncodingUTF8));
		put(all, p[i].name, one);
	}
	return all;
}

/* ---- the cameras ---- */
#define PF_420F 0x34323066u     /* '420f' */
#define PF_420V 0x34323076u     /* '420v' */
#define PF_X420 0x78343230u     /* 'x420': 10-bit 4:2:0 video range (V54's -10Bit device formats) */
#define PF_X422 0x78343232u     /* 'x422': 10-bit 4:2:2 video range (V54's -422-10Bit device formats) */
#define PF_BGRA 0x42475241u     /* 'BGRA' */
#define MAX_STREAMS 2
#define OUTPUTS 3
#define HELD 3                  /* pictures kept after they are given, so the pool hands out others (HakiOS) */
static const char *const output_names[OUTPUTS] = { "PrimaryScaler", "SecondaryScaler", "StillImage" };

struct camera {
	uint32_t role;                  /* ONEIOS_CAMERA_WIDE, ONEIOS_CAMERA_FRONT */
	const char *name;               /* the device's uniqueName in AVCaptureSession.plist */
	const char *unique_id;
	bool front;
	double focal_length, f_number, pixel_size;
	int sensor_id;                  /* when CameraSetup.plist's DefaultSensorIDs has none for the port */
	const char *devices[4];         /* the plist's devices on this stream's port, whose named formats it must have */
};

/*
 * The back wide camera and the front camera, named as V54's plist names the devices whose stream formats they take.
 * The sensor IDs are V54's own (CameraSetup.plist DefaultSensorIDs: PortTypeBack 0x0903, PortTypeFrontSuperWide 0x0914):
 * CMCapture keys every camera table by port type and "0x%04x" of the stream's ModuleInfo sensorID.
 *
 * Each device of the plist on a stream's port resolves its own named formats against that one stream's formats (phone
 * 2026-11-23: "Resolving formats for Front Ultra Wide Camera, using 8 stream formats", then the same 8 for "Front Camera",
 * which is the plist's "Front Virtual Camera", and for "Front Ultra Wide Metadata Camera"). So the front stream has the
 * formats of all three; the Camera app's front camera is "Front Camera", which had none ("does not support
 * AVCaptureSessionPresetHigh", no input, no picture).
 *
 * No telephoto and no ultra-wide stream (yet: README "Not done" 5): CMCapture disables each device of the plist whose port
 * has no stream ("Failed to open streams for device Back Telephoto Camera, this device will be disabled"; the same for the
 * ultra-wide and the dual and triple cameras made of them), and goes on with the others: that is V54's device list without
 * them, no plist of ours needed. A telephoto stream fed by the wide camera would publish a 4x lens that is not there; the
 * phone's own 5x needs a second camera open at once in drivers/camera (one client, one camera now), and the dual and triple
 * cameras need synchronized stream groups (SupportedSynchronizedStreamsGroups is empty here).
 */
static const struct camera known[MAX_STREAMS] = {
	{ ONEIOS_CAMERA_WIDE, "Back Camera", "com.apple.avfoundation.avcapturedevice.built-in_video:0", false, 6.3, 1.7, 1.0, 0x0903,
	  { "Back Camera" } },
	{ ONEIOS_CAMERA_FRONT, "Front Ultra Wide Camera", "com.apple.avfoundation.avcapturedevice.built-in_video:1", true, 3.3, 2.2, 1.0, 0x0914,
	  { "Front Ultra Wide Camera", "Front Virtual Camera", "Front Ultra Wide Metadata Camera" } },
};

#ifndef ONEIOS_CAMERA_SESSION_PLIST
#define ONEIOS_CAMERA_SESSION_PLIST "/System/Library/PrivateFrameworks/CMCapture.framework/V54/AVCaptureSession.plist"
#endif
#ifndef ONEIOS_CAMERA_SETUP_PLIST
#define ONEIOS_CAMERA_SETUP_PLIST "/System/Library/PrivateFrameworks/CMCapture.framework/V54/CameraSetup.plist"
#endif

static CFPropertyListRef session_plist, setup_plist;

static CFPropertyListRef
read_plist(const char *path)
{
	CFStringRef s = CFStringCreateWithCString(NULL, path, kCFStringEncodingUTF8);
	CFURLRef url = CFURLCreateWithFileSystemPath(NULL, s, kCFURLPOSIXPathStyle, false);
	CFReadStreamRef in = url ? CFReadStreamCreateWithFile(NULL, url) : NULL;
	CFPropertyListRef p = NULL;

	if (in != NULL && CFReadStreamOpen(in)) {
		p = CFPropertyListCreateWithStream(NULL, in, 0, kCFPropertyListImmutable, NULL, NULL);
		CFReadStreamClose(in);
	}
	if (in) {
		CFRelease(in);
	}
	if (url) {
		CFRelease(url);
	}
	CFRelease(s);
	return p;
}

/* The sensor ID of a port: CameraSetup.plist's DefaultSensorIDs ("0x0903"), else the camera's own. */
static int
sensor_id_for(CFStringRef port, const struct camera *c)
{
	CFDictionaryRef ids = setup_plist != NULL && CFGetTypeID(setup_plist) == CFDictionaryGetTypeID() ?
	    CFDictionaryGetValue(setup_plist, CFSTR("DefaultSensorIDs")) : NULL;
	CFStringRef text = ids != NULL && CFGetTypeID(ids) == CFDictionaryGetTypeID() ? CFDictionaryGetValue(ids, port) : NULL;
	char buffer[24];

	if (text != NULL && CFGetTypeID(text) == CFStringGetTypeID() && CFStringGetCString(text, buffer, sizeof(buffer), kCFStringEncodingUTF8)) {
		long id = strtol(buffer, NULL, 16);

		if (id > 0 && id < 0x10000) {
			return (int)id;
		}
	}
	return c->sensor_id;
}

static void
copy_entry(const void *key, const void *value, void *context)
{
	CFDictionarySetValue(context, key, value);
}

/* A device's (or a device info base's) MultiStream FigCaptureStreamFormats, or NULL. */
static CFDictionaryRef
stream_formats_in(CFTypeRef d)
{
	CFDictionaryRef ms = d != NULL && CFGetTypeID(d) == CFDictionaryGetTypeID() ? CFDictionaryGetValue(d, CFSTR("MultiStream")) : NULL;
	CFDictionaryRef f = ms != NULL && CFGetTypeID(ms) == CFDictionaryGetTypeID() ? CFDictionaryGetValue(ms, CFSTR("FigCaptureStreamFormats")) : NULL;

	return f != NULL && CFGetTypeID(f) == CFDictionaryGetTypeID() ? f : NULL;
}

static CFMutableDictionaryRef base_plists;     /* the device info bases read so far: file name -> plist (kCFNull: none) */
static pthread_mutex_t base_lock = PTHREAD_MUTEX_INITIALIZER;

/* A device's deviceInfoBase file of the session plist's folder ("FrontUltraWideCamera-DeviceInfoBase.plist"), read once. */
static CFTypeRef
device_info_base(CFStringRef file)
{
	const char *session = ONEIOS_CAMERA_SESSION_PLIST;
	char name[128], path[600];
	const char *slash = session != NULL ? strrchr(session, '/') : NULL;
	CFTypeRef p;

	if (slash == NULL || file == NULL || CFGetTypeID(file) != CFStringGetTypeID() ||
	    !CFStringGetCString(file, name, sizeof(name), kCFStringEncodingUTF8) || strchr(name, '/') != NULL ||
	    (size_t)(slash - session) + 2 + strlen(name) > sizeof(path)) {
		return NULL;
	}
	pthread_mutex_lock(&base_lock);
	if (base_plists == NULL) {
		base_plists = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
	}
	p = CFDictionaryGetValue(base_plists, file);
	if (p == NULL) {
		snprintf(path, sizeof(path), "%.*s/%s", (int)(slash - session), session, name);
		p = read_plist(path);
		oneios_camera_say("%s: %s", path, p != NULL ? "read" : "not read");
		CFDictionarySetValue(base_plists, file, p != NULL ? p : kCFNull);
		if (p != NULL) {
			CFRelease(p);
		}
		p = CFDictionaryGetValue(base_plists, file);
	}
	pthread_mutex_unlock(&base_lock);
	return p != kCFNull ? p : NULL;
}

/*
 * The FigCaptureStreamFormats of the plist's device with this uniqueName, with those of its deviceInfoBase: V54's front
 * devices keep theirs there (FrontUltraWideCamera-DeviceInfoBase.plist has 22 named formats, the session plist's entry 8 more;
 * CMCapture asks both, phone 2026-11-23). The device's own entry wins a name both have. A new dictionary, or NULL.
 */
static CFDictionaryRef
device_stream_formats(const char *unique_name)
{
	CFArrayRef devices = session_plist != NULL && CFGetTypeID(session_plist) == CFDictionaryGetTypeID() ?
	    CFDictionaryGetValue(session_plist, CFSTR("AVCaptureDevices")) : NULL;

	for (CFIndex i = 0; devices != NULL && CFGetTypeID(devices) == CFArrayGetTypeID() && i < CFArrayGetCount(devices); i++) {
		CFDictionaryRef d = CFArrayGetValueAtIndex(devices, i), own, base;
		CFStringRef name = CFGetTypeID(d) == CFDictionaryGetTypeID() ? CFDictionaryGetValue(d, CFSTR("uniqueName")) : NULL;
		CFMutableDictionaryRef all;
		char text[80] = "";

		if (name == NULL || CFGetTypeID(name) != CFStringGetTypeID() || !CFStringGetCString(name, text, sizeof(text), kCFStringEncodingUTF8) ||
		    strcmp(text, unique_name) != 0) {
			continue;
		}
		own = stream_formats_in(d);
		base = stream_formats_in(device_info_base(CFDictionaryGetValue(d, CFSTR("deviceInfoBase"))));
		if ((own == NULL || CFDictionaryGetCount(own) == 0) && (base == NULL || CFDictionaryGetCount(base) == 0)) {
			return NULL;
		}
		all = base != NULL ? CFDictionaryCreateMutableCopy(NULL, 0, base) :
		    CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
		if (own != NULL) {
			CFDictionaryApplyFunction(own, copy_entry, all);
		}
		return all;
	}
	return NULL;
}

/* Without the camera's devices in the plist (another model): the first back or front device's formats. */
static CFDictionaryRef
first_stream_formats(const struct camera *c, char *name, size_t room)
{
	CFArrayRef devices = session_plist != NULL && CFGetTypeID(session_plist) == CFDictionaryGetTypeID() ?
	    CFDictionaryGetValue(session_plist, CFSTR("AVCaptureDevices")) : NULL;

	for (CFIndex i = 0; devices != NULL && CFGetTypeID(devices) == CFArrayGetTypeID() && i < CFArrayGetCount(devices); i++) {
		CFDictionaryRef d = CFArrayGetValueAtIndex(devices, i);
		CFStringRef n = CFGetTypeID(d) == CFDictionaryGetTypeID() ? CFDictionaryGetValue(d, CFSTR("uniqueName")) : NULL;
		CFDictionaryRef f;

		if (n != NULL && CFGetTypeID(n) == CFStringGetTypeID() && CFStringGetCString(n, name, (CFIndex)room, kCFStringEncodingUTF8) &&
		    strncmp(name, c->front ? "Front" : "Back", c->front ? 5 : 4) == 0 && (f = device_stream_formats(name)) != NULL) {
			return f;
		}
	}
	return NULL;
}

static CFDictionaryRef
output_info(int width, int height)
{
	CFMutableDictionaryRef o = new_dict();
	CFNumberRef pf[5] = { number_i(PF_420F), number_i(PF_420V), number_i(PF_X420), number_i(PF_X422), number_i(PF_BGRA) };

	put(o, "SupportedPixelFormats", CFArrayCreate(NULL, (const void **)pf, 5, &kCFTypeArrayCallBacks));
	for (int i = 0; i < 5; i++) {
		CFRelease(pf[i]);
	}
	put(o, "NativeWidth", number_i(width));
	put(o, "NativeHeight", number_i(height));
	put(o, "MaxDownscaleFactor", number_f(32));
	put(o, "MaxScaleFactor", number_f(1));
	put(o, "MinScaleFactor", number_f(1.0 / 32));
	put(o, "OutputWidthAlignmentRequirement", number_i(16));
	return o;
}

static int
int_at(CFDictionaryRef d, const char *key)
{
	CFStringRef k = CFStringCreateWithCString(NULL, key, kCFStringEncodingUTF8);
	double v = number_of(d != NULL ? CFDictionaryGetValue(d, k) : NULL, 0);

	CFRelease(k);
	return (int)v;
}

/* Which of the keys the resolver reads the entry has (ocf_params.has). */
static void
mark_key(const void *key, const void *value, void *context)
{
	char name[64];

	(void)value;
	if (CFGetTypeID(key) == CFStringGetTypeID() && CFStringGetCString(key, name, sizeof(name), kCFStringEncodingUTF8)) {
		*(unsigned *)context |= ocf_key_bit(name);
	}
}

/* A named stream format of the plist as CMCapture's resolver reads it (oneios_camera_formats.h). */
static void
params_of(CFDictionaryRef e, struct ocf_params *p)
{
	*p = (struct ocf_params){
		.sensor_w = int_at(e, "SensorWidth"), .sensor_h = int_at(e, "SensorHeight"), .fes = int_at(e, "FES") != 0,
		.fes_w = int_at(e, "FESWidth"), .fes_h = int_at(e, "FESHeight"), .binned = int_at(e, "Binned") != 0,
		.hdr = int_at(e, "HDR") != 0, .max_fps = int_at(e, "MaxFrameRate"), .sifr = int_at(e, "SIFREnabled") != 0,
		.sifr_bf = int_at(e, "SIFRBinningFactor"), .qbf = int_at(e, "QuadraBinningFactor"),
		.mid_sync = int_at(e, "OptimizedForMidFrameSynchronization") != 0,
		.signing = int_at(e, "SecureSigningFaceIDSupported") != 0, .unsupported = int_at(e, "Unsupported") != 0,
	};
	if (e != NULL) {
		CFDictionaryApplyFunction(e, mark_key, &p->has);
	}
}

/* The plist entry's own scalar keys (numbers, booleans, strings): a companion's and a stream format's start. */
static void
copy_scalar(const void *key, const void *value, void *context)
{
	CFTypeID t = CFGetTypeID(value);

	if (t == CFNumberGetTypeID() || t == CFBooleanGetTypeID() || t == CFStringGetTypeID()) {
		CFDictionarySetValue(context, key, value);
	}
}

/*
 * One stream format for a named format of the plist: the entry's scalar keys, then the keys CMCapture's resolver
 * compares (ocf_format_for: without FrontEndScalerOutputWidth/Height and SIFRBinningFactor no V54 format resolved,
 * phone 2026-11-23), and the keys a stream's own format carries.
 */
static CFDictionaryRef
format(CFStringRef name, CFDictionaryRef entry, struct ocf_format *k)
{
	CFMutableDictionaryRef f = new_dict(), outputs = new_dict();
	struct ocf_params p = { 0 };
	CMVideoFormatDescriptionRef fd = NULL;
	int w, h, native_w, native_h, crop_w, crop_h;

	if (entry != NULL) {
		params_of(entry, &p);
		CFDictionaryApplyFunction(entry, copy_scalar, f);
	}
	ocf_format_for(&p, k);
	w = k->sensor_w;
	h = k->sensor_h;
	native_w = k->fes_out_w > 0 ? k->fes_out_w : w;
	native_h = k->fes_out_h > 0 ? k->fes_out_h : h;
	crop_w = int_at(entry, "SensorCropWidth") > 0 ? int_at(entry, "SensorCropWidth") : w;
	crop_h = int_at(entry, "SensorCropHeight") > 0 ? int_at(entry, "SensorCropHeight") : h;
	if (name != NULL) {
		CFDictionarySetValue(f, CFSTR("FormatName"), name);
	}
	if (CMVideoFormatDescriptionCreate(NULL, PF_420F, w, h, NULL, &fd) == 0 && fd != NULL) {
		put(f, "FormatDescription", fd);
	}
	/* what the resolver compares (kFigCaptureStreamVideoFormatKey_*, kFigSupportedFormat_*: their CMCaptureCore names) */
	put(f, "SensorWidth", number_i(w));
	put(f, "SensorHeight", number_i(h));
	if (k->fes_out_w > 0) {
		put(f, "FrontEndScalerOutputWidth", number_i(k->fes_out_w));
		put(f, "FrontEndScalerOutputHeight", number_i(k->fes_out_h));
	}
	put(f, "VideoIsBinned", CFRetain(k->binned ? kCFBooleanTrue : kCFBooleanFalse));
	if (k->hdr_mode > 0) {
		put(f, "SensorHDRMode", number_i(k->hdr_mode));
	}
	put(f, "VideoMaxFrameRate", number_i(k->max_fps));
	put(f, "SIFREnabled", CFRetain(k->sifr ? kCFBooleanTrue : kCFBooleanFalse));
	if (k->sifr_bf > 0) {
		put(f, "SIFRBinningFactor", number_i(k->sifr_bf));
	}
	if (k->qbf > 0) {
		put(f, "QuadraBinningFactor", number_i(k->qbf));
	}
	put(f, "OptimizedForMidFrameSynchronization", CFRetain(k->mid_sync ? kCFBooleanTrue : kCFBooleanFalse));
	put(f, "SensorSigningEnabled", CFRetain(k->signing ? kCFBooleanTrue : kCFBooleanFalse));
	put(f, "Experimental", CFRetain(kCFBooleanFalse));
	/* the rest of a stream format */
	put(f, "VideoMinFrameRate", number_f(2));
	put(f, "VideoBinningFactorHorizontal", number_i(k->binned ? 2 : 1));
	put(f, "VideoBinningFactorVertical", number_i(k->binned ? 2 : 1));
	put(f, "VideoFieldOfView", number_f(78));
	put(f, "VideoMinIntegrationTime", number_i(100));
	put(f, "VideoMaxIntegrationTime", number_i(1000000 / 15));
	put(f, "VideoRawBitDepth", number_i(10));
	put(f, "VideoMaxWidth", number_i(native_w));
	put(f, "VideoMaxHeight", number_i(native_h));
	put(f, "ISOBase", number_f(32));
	put(f, "DefaultAEMaxGain", number_f(64));
	put(f, "SensorCropWidth", number_i(crop_w));
	put(f, "SensorCropHeight", number_i(crop_h));
	put(f, "PreGDCSensorCropWidth", number_i(crop_w));
	put(f, "PreGDCSensorCropHeight", number_i(crop_h));
	put(f, "PostGDCCropWidth", number_i(native_w));
	put(f, "PostGDCCropHeight", number_i(native_h));
	put(f, "GeometricDistortionCorrectedNativeWidth", number_i(native_w));
	put(f, "GeometricDistortionCorrectedNativeHeight", number_i(native_h));
	put(outputs, "PrimaryScaler", output_info(native_w, native_h));
	put(outputs, "SecondaryScaler", output_info(native_w, native_h));
	put(outputs, "StillImage", output_info(w, h));
	put(f, "SupportedOutputs", outputs);
	return f;
}

static CFComparisonResult
compare_names(const void *a, const void *b, void *context)
{
	(void)context;
	return CFStringCompare(a, b, 0);
}

/* What formats_for builds: the stream formats with their compared keys, and every name of the plist with its params. */
struct format_list {
	CFMutableArrayRef out;
	struct ocf_format *keys;
	int n, room;
	struct ocf_params *asked;
	CFMutableArrayRef asked_names;
	int n_asked, room_asked, companions;
};

static void
list_ask(struct format_list *l, CFStringRef name, CFDictionaryRef entry)
{
	struct ocf_format k;
	CFDictionaryRef f = format(name, entry, &k);
	bool known = false;

	if (l->n_asked == l->room_asked) {
		l->room_asked = l->room_asked ? 2 * l->room_asked : 64;
		l->asked = realloc(l->asked, (size_t)l->room_asked * sizeof(*l->asked));
	}
	params_of(entry, &l->asked[l->n_asked++]);
	CFArrayAppendValue(l->asked_names, name);
	/* one stream format for each distinct set of compared keys: the resolver takes the first match anyway */
	for (int i = 0; i < l->n && !known; i++) {
		known = ocf_same(&l->keys[i], &k);
	}
	if (!known && !l->asked[l->n_asked - 1].unsupported) {
		if (l->n == l->room) {
			l->room = l->room ? 2 * l->room : 64;
			l->keys = realloc(l->keys, (size_t)l->room * sizeof(*l->keys));
		}
		l->keys[l->n++] = k;
		CFArrayAppendValue(l->out, f);
	}
	CFRelease(f);
}

struct companion_context {
	struct format_list *list;
	CFStringRef parent_name;
	CFDictionaryRef parent;
};

/* CMCapture's steps of a companion (ocf_companion_rule) on the parent's mutable copy, with the companion's entry. */
static void
apply_steps(const struct ocf_companion *rule, CFMutableDictionaryRef merged, CFDictionaryRef value)
{
	for (unsigned i = 0; i < sizeof(rule->steps) / sizeof(rule->steps[0]) && rule->steps[i].key != NULL; i++) {
		CFStringRef k = CFStringCreateWithCString(NULL, rule->steps[i].key, kCFStringEncodingUTF8);
		CFTypeRef v = value != NULL ? CFDictionaryGetValue(value, k) : NULL;
		CFNumberRef two;

		switch (rule->steps[i].op) {
		case OCF_TAKE_IF_THERE:
			if (v != NULL) {
				CFDictionarySetValue(merged, k, v);
			}
			break;
		case OCF_TAKE:
			if (v != NULL) {
				CFDictionarySetValue(merged, k, v);
			} else {
				CFDictionaryRemoveValue(merged, k);
			}
			break;
		case OCF_SET_TRUE:
		case OCF_SET_FALSE:
			CFDictionarySetValue(merged, k, rule->steps[i].op == OCF_SET_TRUE ? kCFBooleanTrue : kCFBooleanFalse);
			break;
		case OCF_REMOVE:
			CFDictionaryRemoveValue(merged, k);
			break;
		case OCF_SET_TWO:
			two = number_i(2);
			CFDictionarySetValue(merged, k, two);
			CFRelease(two);
			break;
		}
		CFRelease(k);
	}
}

/*
 * A companion's named format, made as CMCapture 24A446 makes it (ocf_companion_rule: phone 2026-11-23, "failed to resolve
 * format 'Photo[FESCompanionFormat]'" and the two MidFrameSynchronization ones, when the plug-in only laid the companion's
 * keys over its parent's: an FES companion names no FES and a mid-frame one no OptimizedForMidFrameSynchronization, CMCapture
 * sets both). A companion CMCapture does not resolve (UltraHighResCompanionFormat) keeps the old way: its keys over the
 * parent's, named without the "s".
 */
static void
companion(CFStringRef key, CFDictionaryRef value, struct companion_context *c)
{
	CFMutableDictionaryRef merged;
	CFStringRef name;
	const struct ocf_companion *rule;
	char text[96] = "";

	CFStringGetCString(key, text, sizeof(text), kCFStringEncodingUTF8);
	rule = ocf_companion_rule(text);
	if (rule != NULL && rule->skip_unsupported && value != NULL && number_of(CFDictionaryGetValue(value, CFSTR("Unsupported")), 0) != 0) {
		return;
	}
	merged = new_dict();
	CFDictionaryApplyFunction(c->parent, copy_scalar, merged);
	CFDictionaryRemoveValue(merged, CFSTR("Unsupported"));
	if (rule != NULL) {
		apply_steps(rule, merged, value);
		name = CFStringCreateWithFormat(NULL, NULL, CFSTR("%@%s"), c->parent_name, rule->suffix);
	} else {
		if (text[0] != '\0' && text[strlen(text) - 1] == 's') {
			text[strlen(text) - 1] = '\0';
		}
		if (value != NULL) {
			CFDictionaryApplyFunction(value, copy_scalar, merged);
		}
		name = CFStringCreateWithFormat(NULL, NULL, CFSTR("%@[%s]"), c->parent_name, text);
	}
	list_ask(c->list, name, merged);
	c->list->companions++;
	CFRelease(name);
	CFRelease(merged);
}

static void
companions_of(const void *key, const void *value, void *context)
{
	struct companion_context *c = context;
	const struct ocf_companion *rule;
	char text[96] = "";

	if (CFGetTypeID(key) != CFStringGetTypeID() || CFStringFind(key, CFSTR("CompanionFormat"), 0).location == kCFNotFound) {
		return;
	}
	if (CFGetTypeID(value) == CFBooleanGetTypeID()) {
		/* HasSensorHDRCompanionFormat: a flag of the parent, the companion is the parent with HDR */
		CFStringGetCString(key, text, sizeof(text), kCFStringEncodingUTF8);
		rule = ocf_companion_rule(text);
		if (rule != NULL && rule->flag && CFBooleanGetValue(value)) {
			companion(key, NULL, c);
		}
	} else if (CFGetTypeID(value) == CFDictionaryGetTypeID()) {
		companion(key, value, c);
	} else if (CFGetTypeID(value) == CFArrayGetTypeID()) {
		for (CFIndex i = 0; i < CFArrayGetCount(value); i++) {
			CFDictionaryRef one = CFArrayGetValueAtIndex(value, i);

			if (CFGetTypeID(one) == CFDictionaryGetTypeID()) {
				companion(key, one, c);
			}
		}
	}
}

/*
 * The stream's formats: one for each distinct named format of the plist's stream formats of the devices on this camera's
 * port (camera.devices, with their device info bases) and of their companions (so that every AVCaptureDevice format of the
 * plist resolves to one), the named ones in name order first; without the plist, 1920x1080 at 30. Then CMCapture's own
 * rule is run over them for every name (a lasting check, one line per camera): "all resolve", or the names that do not.
 */
static CFArrayRef
sorted_names(CFDictionaryRef list)
{
	CFIndex n = CFDictionaryGetCount(list);
	const void **keys = calloc((size_t)n + 1, sizeof(void *));
	CFMutableArrayRef names = CFArrayCreateMutable(NULL, n, &kCFTypeArrayCallBacks);

	CFDictionaryGetKeysAndValues(list, keys, NULL);
	for (CFIndex i = 0; i < n; i++) {
		CFArrayAppendValue(names, keys[i]);
	}
	CFArraySortValues(names, CFRangeMake(0, n), compare_names, NULL);
	free(keys);
	return names;
}

static CFArrayRef
formats_for(const struct camera *c)
{
	struct format_list l = { .out = CFArrayCreateMutable(NULL, 0, &kCFTypeArrayCallBacks),
		.asked_names = CFArrayCreateMutable(NULL, 0, &kCFTypeArrayCallBacks) };
	CFDictionaryRef lists[4] = { NULL };
	int named = 0, resolved = 0, n_lists = 0;
	char missing[200] = "", from[200] = "";

	/* every device of the plist on this stream's port, each with its device info base's formats */
	for (unsigned k = 0; k < sizeof(c->devices) / sizeof(c->devices[0]) && c->devices[k] != NULL; k++) {
		if ((lists[n_lists] = device_stream_formats(c->devices[k])) != NULL) {
			n_lists++;
			strlcat(from, from[0] ? ", " : "", sizeof(from));
			strlcat(from, c->devices[k], sizeof(from));
		}
	}
	if (n_lists == 0) {
		char name[80] = "";

		if ((lists[0] = first_stream_formats(c, name, sizeof(name))) != NULL) {
			n_lists = 1;
			strlcpy(from, name, sizeof(from));
		}
	}
	/* the named formats first (each device's in name order), then the companions */
	for (int pass = 0; pass < 2; pass++) {
		for (int k = 0; k < n_lists; k++) {
			CFArrayRef names = sorted_names(lists[k]);

			for (CFIndex i = 0; i < CFArrayGetCount(names); i++) {
				CFStringRef name = CFArrayGetValueAtIndex(names, i);
				CFDictionaryRef entry = CFDictionaryGetValue(lists[k], name);
				struct companion_context cc = { &l, name, entry };

				if (CFGetTypeID(entry) != CFDictionaryGetTypeID()) {
					continue;
				}
				if (pass == 0) {
					list_ask(&l, name, entry);
					named++;
				} else {
					CFDictionaryApplyFunction(entry, companions_of, &cc);
				}
			}
			CFRelease(names);
		}
	}
	for (int k = 0; k < n_lists; k++) {
		CFRelease(lists[k]);
	}
	if (CFArrayGetCount(l.out) == 0) {
		struct ocf_format k;
		CFDictionaryRef f = format(NULL, NULL, &k);

		CFArrayAppendValue(l.out, f);
		CFRelease(f);
	}
	for (int i = 0; i < l.n_asked; i++) {
		if (l.asked[i].unsupported || ocf_resolve(&l.asked[i], l.keys, l.n) >= 0) {
			resolved++;
		} else if (strlen(missing) < sizeof(missing) - 40) {
			char text[80] = "";

			CFStringGetCString(CFArrayGetValueAtIndex(l.asked_names, i), text, sizeof(text), kCFStringEncodingUTF8);
			strlcat(missing, missing[0] ? ", " : ": ", sizeof(missing));
			strlcat(missing, text, sizeof(missing));
		}
	}
	oneios_camera_say("stream %s: %d stream formats for %d named formats and %d companions of the plist's %s; %d of %d resolve by "
	    "CMCapture's rule%s%s", c->name, (int)CFArrayGetCount(l.out), named, l.companions, from[0] ? from : "(no device)", resolved,
	    l.n_asked, resolved == l.n_asked ? " (all)" : ", these do not", missing);
	free(l.keys);
	free(l.asked);
	CFRelease(l.asked_names);
	return l.out;
}

/* ---- the stream ---- */
typedef void (^pixel_buffer_handler)(CVPixelBufferRef, CMTime);

struct output {
	const char *name;
	CFDictionaryRef config;         /* from VideoOutputConfigurations */
	bool enabled;
	void *handler;                  /* a block ^(CVPixelBufferRef, CMTime) */
	uint64_t delivered, refused;
	bool said_pool, said_format;
	CVPixelBufferRef held[HELD];
	unsigned held_next;
};

struct stream {
	const struct camera *camera;
	CFStringRef port;
	CFArrayRef formats;
	CFDictionaryRef supported, colour_info;
	int sensor_id;                  /* ModuleInfo sensorID (CameraSetup.plist DefaultSensorIDs) */
	int format_index;
	double min_fps, max_fps, zoom;
	unsigned stills_wanted;
	struct output out[OUTPUTS];
	pthread_mutex_t lock;
	pthread_t worker;
	volatile bool running, graph_running;
	bool worker_started;
	uint64_t frames;
};

static CMBaseObjRef all_streams[MAX_STREAMS];

static struct stream *
stream_of(CMBaseObjRef o)
{
	return derived_storage(o);
}

static int
output_index(CFTypeRef name)
{
	for (int i = 0; i < OUTPUTS; i++) {
		if (key_is(name, output_names[i])) {
			return i;
		}
	}
	return -1;
}

static void
let_go(struct output *out)
{
	for (unsigned i = 0; i < HELD; i++) {
		if (out->held[i] != NULL) {
			CVPixelBufferRelease(out->held[i]);
			out->held[i] = NULL;
		}
	}
}

static OSStatus
stream_copy_property(CMBaseObjRef o, CFStringRef key, CFAllocatorRef allocator, void *out)
{
	struct stream *s = stream_of(o);
	CFTypeRef value = NULL;

	(void)allocator;
	if (key_is(key, "PortType")) {
		value = CFRetain(s->port);
	} else if (key_is(key, "ModuleInfo")) {
		/* CMCapture reads the sensor ID out of it (+[FigCaptureCameraParameters sensorIDStringFromModuleInfo:]: "0x%04x" of
		 * kFigCapturePropertyValue_SensorID) and finds the camera's tables by port type and that string; without it every
		 * named format failed ("Unexpected sensor ID ((null)) found for PortTypeBack", phone 2026-11-23). */
		CFMutableDictionaryRef m = new_dict();
		CFNumberRef id = number_i(s->sensor_id);

		CFDictionarySetValue(m, md_sensor_id != NULL ? md_sensor_id : CFSTR("sensorID"), id);
		CFRelease(id);
		value = m;
	} else if (key_is(key, "WhiteBalanceGain")) {
		/* { RedGain, GreenGain, BlueGain }, integers with 12 fraction bits (oneios_camera_formats.h): 1 each, the phone's
		 * picture comes white-balanced */
		static const char *const names[3] = { "RedGain", "GreenGain", "BlueGain" };
		CFMutableDictionaryRef g = new_dict();
		CFNumberRef one = number_i(OCF_WB_GAIN_ONE);

		for (int i = 0; i < 3; i++) {
			CFStringRef k = wb_keys[i] != NULL ? CFRetain(wb_keys[i]) : CFStringCreateWithCString(NULL, names[i], kCFStringEncodingUTF8);

			CFDictionarySetValue(g, k, one);
			CFRelease(k);
		}
		CFRelease(one);
		value = g;
	} else if (key_is(key, "UniqueID") || key_is(key, "ModelID")) {
		value = CFStringCreateWithCString(NULL, s->camera->unique_id, kCFStringEncodingUTF8);
	} else if (key_is(key, "NonLocalizedName") || key_is(key, "LocalizedName")) {
		value = CFStringCreateWithCString(NULL, s->camera->name, kCFStringEncodingUTF8);
	} else if (key_is(key, "Manufacturer")) {
		value = CFRetain(CFSTR("Apple Inc."));
	} else if (key_is(key, "SupportedFormatsArray")) {
		value = CFRetain(s->formats);
	} else if (key_is(key, "FormatIndex")) {
		value = number_i(s->format_index);
	} else if (key_is(key, "MinimumFrameRate")) {
		value = number_f(s->min_fps);
	} else if (key_is(key, "MaximumFrameRate") || key_is(key, "MaximumAllowedFrameRate")) {
		value = number_f(s->max_fps);
	} else if (key_is(key, "Clock")) {
		value = CFRetain(CMClockGetHostTimeClock());
	} else if (key_is(key, "TorchLevel") || key_is(key, "ExposureBias")) {
		value = number_f(0);
	} else if (key_is(key, "VideoOutputConfigurations") || key_is(key, "VideoOutputsEnabled")) {
		/* Read back by the graph while it builds (HakiOS: a failed read tears the graph down). */
		bool configs = key_is(key, "VideoOutputConfigurations");
		CFMutableDictionaryRef d = new_dict();

		pthread_mutex_lock(&s->lock);
		for (int i = 0; i < OUTPUTS; i++) {
			CFStringRef name = CFStringCreateWithCString(NULL, output_names[i], kCFStringEncodingUTF8);

			if (configs && s->out[i].config != NULL) {
				CFDictionarySetValue(d, name, s->out[i].config);
			} else if (!configs) {
				CFDictionarySetValue(d, name, s->out[i].enabled ? kCFBooleanTrue : kCFBooleanFalse);
			}
			CFRelease(name);
		}
		pthread_mutex_unlock(&s->lock);
		value = d;
	} else if (key_is(key, "VideoOutputsColorInfo")) {
		pthread_mutex_lock(&s->lock);
		value = s->colour_info != NULL ? CFRetain(s->colour_info) : new_dict();
		pthread_mutex_unlock(&s->lock);
	} else if (key_is(key, "SupportedPropertiesDictionary")) {
		value = CFRetain(s->supported);
	} else if (key_is(key, "FocalLength")) {
		value = number_f(s->camera->focal_length);
	} else if (key_is(key, "LensFNumber")) {
		value = number_f(s->camera->f_number);
	} else if (key_is(key, "PixelSize")) {
		value = number_f(s->camera->pixel_size);     /* required by the graph (HakiOS) */
	} else if (key_is(key, "MaxZoomFactor")) {
		value = number_f(8);
	} else if (key_is(key, "ZoomFactor")) {
		value = number_f(s->zoom);
	} else if (key_is(key, "SupportedColorSpaces")) {
		CFNumberRef srgb = number_i(0);

		value = CFArrayCreate(NULL, (const void **)&srgb, 1, &kCFTypeArrayCallBacks);
		CFRelease(srgb);
	} else if (key_is(key, "ColorSpace") || key_is(key, "DeviceOrientation") || key_is(key, "FlashMode")) {
		value = number_i(0);
	} else if (key_is(key, "HasAutoFocusPositionSensor") || key_is(key, "HighCurrentTorchEnabled") ||
	    key_is(key, "DeferAdditionOfAttachments")) {
		value = CFRetain(kCFBooleanFalse);
	} else {
		say_once("stream: no answer for", key, NULL);
		return ERR_NOT_AVAILABLE;
	}
	*(CFTypeRef *)out = value;
	return 0;
}

static OSStatus
stream_set_property(CMBaseObjRef o, CFStringRef key, CFTypeRef value)
{
	struct stream *s = stream_of(o);

	if (key_is(key, "FormatIndex")) {
		int64_t i = (int64_t)number_of(value, -1);

		if (i < 0 || i >= CFArrayGetCount(s->formats)) {
			return ERR_PARAM;
		}
		s->format_index = (int)i;
		oneios_camera_say("stream %s: format %lld", s->camera->name, (long long)i);
		return 0;
	}
	if (key_is(key, "ZoomFactor")) {
		double zoom = number_of(value, 1);

		s->zoom = zoom < 1 ? 1 : zoom > 8 ? 8 : zoom;
		return 0;
	}
	if (key_is(key, "MinimumFrameRate") || key_is(key, "MaximumFrameRate")) {
		*(key_is(key, "MinimumFrameRate") ? &s->min_fps : &s->max_fps) = number_of(value, 30);
		say_once("stream: set", key, value);
		return 0;
	}
	if (key_is(key, "VideoOutputConfigurations") || key_is(key, "VideoOutputsEnabled") || key_is(key, "VideoOutputHandlers")) {
		bool configs = key_is(key, "VideoOutputConfigurations"), enabled = key_is(key, "VideoOutputsEnabled");
		CFIndex n;
		const void **keys, **values;

		say_once("stream: set", key, value);
		if (value == NULL || CFGetTypeID(value) != CFDictionaryGetTypeID()) {
			return 0;
		}
		n = CFDictionaryGetCount(value);
		keys = calloc((size_t)n + 1, sizeof(void *));
		values = calloc((size_t)n + 1, sizeof(void *));
		CFDictionaryGetKeysAndValues(value, keys, values);
		pthread_mutex_lock(&s->lock);
		for (CFIndex i = 0; i < n; i++) {
			int k = output_index(keys[i]);
			struct output *out;

			if (k < 0) {
				say_once("stream: an output not here:", keys[i], NULL);
				continue;
			}
			out = &s->out[k];
			if (configs) {
				if (out->config != NULL) {
					CFRelease(out->config);
				}
				let_go(out);
				out->config = CFGetTypeID(values[i]) == CFDictionaryGetTypeID() ? CFRetain(values[i]) : NULL;
			} else if (enabled) {
				out->enabled = number_of(values[i], 1) != 0;
			} else {
				CFTypeRef h = CFGetTypeID(values[i]) == CFDictionaryGetTypeID() ?
				    CFDictionaryGetValue(values[i], CFSTR("PixelBufferHandler")) : NULL;

				if (out->handler != NULL) {
					CFRelease(out->handler);
				}
				out->handler = h != NULL ? (void *)CFRetain(h) : NULL;
			}
		}
		pthread_mutex_unlock(&s->lock);
		free(keys);
		free(values);
		return 0;
	}
	if (key_is(key, "StillImageCaptureNow")) {
		pthread_mutex_lock(&s->lock);
		s->stills_wanted++;
		pthread_mutex_unlock(&s->lock);
		say_once("stream: a photograph asked for", key, value);
		return 0;
	}
	if (key_is(key, "VideoOutputsColorInfo")) {
		pthread_mutex_lock(&s->lock);
		if (s->colour_info != NULL) {
			CFRelease(s->colour_info);
		}
		s->colour_info = value != NULL && CFGetTypeID(value) == CFDictionaryGetTypeID() ? CFRetain(value) : NULL;
		pthread_mutex_unlock(&s->lock);
		return 0;
	}
	say_once("stream: set (taken, not used)", key, value);
	return 0;
}

/* ---- the frames ---- */
static CVPixelBufferRef
output_buffer(struct output *out)
{
	CFTypeRef value = out->config != NULL ? CFDictionaryGetValue(out->config, CFSTR("BufferPool")) : NULL;
	CVPixelBufferRef pb = NULL;

	if (value == NULL || CFGetTypeID(value) != CVPixelBufferPoolGetTypeID()) {
		if (!out->said_pool) {
			out->said_pool = true;
			oneios_camera_say("%s: no pool of the graph's in the configuration: no pictures", out->name);
		}
		return NULL;
	}
	for (unsigned i = 0; CVPixelBufferPoolCreatePixelBuffer(NULL, (CVPixelBufferPoolRef)value, &pb) != kCVReturnSuccess && i < HELD; i++) {
		unsigned oldest = (out->held_next + i) % HELD;

		if (out->held[oldest] != NULL) {
			CVPixelBufferRelease(out->held[oldest]);
			out->held[oldest] = NULL;
		}
	}
	if (pb == NULL && out->refused++ % 300 == 0) {
		oneios_camera_say("%s: the graph's pool has no buffer to spare (%llu frames so)", out->name,
		    (unsigned long long)out->refused);
	}
	return pb;
}

static void
deliver(struct stream *s, struct output *out, const struct oneios_camera_frame *f, const uint8_t *picture)
{
	CVPixelBufferRef pb = output_buffer(out);
	uint32_t pf, width, height;
	int e = -EINVAL;

	if (pb == NULL) {
		return;
	}
	pf = CVPixelBufferGetPixelFormatType(pb);
	width = (uint32_t)CVPixelBufferGetWidth(pb);
	height = (uint32_t)CVPixelBufferGetHeight(pb);
	if (CVPixelBufferLockBaseAddress(pb, 0) == kCVReturnSuccess) {
		if ((pf == PF_420F || pf == PF_420V) && CVPixelBufferGetPlaneCount(pb) == 2) {
			e = oneios_camera_scale_nv12(f, picture, CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
			    CVPixelBufferGetBaseAddressOfPlane(pb, 1), CVPixelBufferGetBytesPerRowOfPlane(pb, 1), width, height, pf == PF_420V);
		} else if ((pf == PF_X420 || pf == PF_X422) && CVPixelBufferGetPlaneCount(pb) == 2) {
			e = oneios_camera_scale_10bit(f, picture, CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
			    CVPixelBufferGetBaseAddressOfPlane(pb, 1), CVPixelBufferGetBytesPerRowOfPlane(pb, 1), width, height, pf == PF_X422);
		} else if (pf == PF_BGRA) {
			e = oneios_camera_scale_bgra(f, picture, CVPixelBufferGetBaseAddress(pb), CVPixelBufferGetBytesPerRow(pb), width, height);
		}
		CVPixelBufferUnlockBaseAddress(pb, 0);
	}
	if (e != 0) {
		if (!out->said_format) {
			out->said_format = true;
			oneios_camera_say("%s: %ux%u '%c%c%c%c' buffers are not filled here", out->name, width, height, (char)(pf >> 24),
			    (char)(pf >> 16), (char)(pf >> 8), (char)pf);
		}
		CVPixelBufferRelease(pb);
		return;
	}
	CVBufferSetAttachment(pb, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVAttachmentMode_ShouldPropagate);
	CVBufferSetAttachment(pb, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVAttachmentMode_ShouldPropagate);
	CVBufferSetAttachment(pb, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, kCVAttachmentMode_ShouldPropagate);
	if (md_attachment != NULL) {
		CFMutableDictionaryRef m = new_dict();

		if (md_port != NULL) {
			CFDictionarySetValue(m, md_port, s->port);
		}
		CVBufferSetAttachment(pb, md_attachment, m, kCVAttachmentMode_ShouldPropagate);
		CFRelease(m);
	}
	((pixel_buffer_handler)out->handler)(pb, CMClockMakeHostTimeFromSystemUnits(f->time));
	if (strcmp(out->name, "StillImage") != 0) {
		if (out->held[out->held_next] != NULL) {
			CVPixelBufferRelease(out->held[out->held_next]);
		}
		out->held[out->held_next] = pb;
		out->held_next = (out->held_next + 1) % HELD;
	} else {
		CVPixelBufferRelease(pb);
	}
	if (out->delivered++ == 0) {
		oneios_camera_say("%s %s: the first picture given (%ux%u '%c%c%c%c' from %ux%u)", s->camera->name, out->name, width, height,
		    (char)(pf >> 24), (char)(pf >> 16), (char)(pf >> 8), (char)pf, f->width, f->height);
	}
}

static void *
worker(void *context)
{
	CMBaseObjRef o = context;
	struct stream *s = stream_of(o);
	oneios_camera_reader reader;
	unsigned quiet = 0;
	int e;

	pthread_setname_np("oneios-camera");
	if ((e = oneios_camera_reader_open(&reader, NULL)) != 0 ||
	    (e = oneios_camera_reader_select(&reader, s->camera->role, 1920, 1080, 30)) != 0) {
		oneios_camera_say("stream %s: the kernel's camera: %s", s->camera->name, strerror(-e));
		if (reader.scratch) {
			oneios_camera_reader_close(&reader);
		}
		s->running = false;
		return NULL;
	}
	oneios_camera_say("stream %s: reading frames through %s", s->camera->name, reader.connect ? "AppleH9CamIn" : ONEIOS_CAMERA_DEVICE);
	while (s->running) {
		struct oneios_camera_frame header;
		const uint8_t *picture;

		if ((e = oneios_camera_reader_next(&reader, &header, &picture)) != 0) {
			if (quiet++ % 30 == 0) {
				oneios_camera_say("stream %s: no frame: %s", s->camera->name, strerror(-e));
			}
			usleep(e == -EAGAIN ? 10000 : 100000);       /* the kernel's EAGAIN comes after a 1 s wait */
			continue;
		}
		quiet = 0;
		if (s->frames++ == 0) {
			oneios_camera_say("stream %s: the first frame: %ux%u, camera %u, turned %u", s->camera->name, header.width, header.height,
			    header.sensor, header.orientation);
		}
		pthread_mutex_lock(&s->lock);
		for (int i = 0; s->graph_running && i < OUTPUTS; i++) {
			struct output *out = &s->out[i];
			bool still = i == 2;

			if (out->handler == NULL || out->config == NULL || !(out->enabled || i == 0) || (still && s->stills_wanted == 0)) {
				continue;
			}
			deliver(s, out, &header, picture);
			if (still) {
				s->stills_wanted--;
				oneios_camera_say("stream %s: the photograph is given", s->camera->name);
			}
		}
		pthread_mutex_unlock(&s->lock);
	}
	oneios_camera_reader_close(&reader);
	oneios_camera_say("stream %s: the frames stopped (%llu)", s->camera->name, (unsigned long long)s->frames);
	return NULL;
}

static OSStatus stream_stop(CMBaseObjRef o);

/*
 * The stream's own notifications, as H10ISP's stream posts them (CoreMedia's notification center, the stream as the
 * object): "StreamStarting" and "StreamStarted" around a Start, "StreamStopping" and "StreamStopped" around a Stop.
 * Ported from Jesvi-iOS userland/src/camera_device.c post() (a13e406; MIT, the HakiOS authors): there
 * BWFigVideoCaptureDevice waited for StreamStopped (captureStreamWillStopStreaming:, _deviceDidStopStreaming), and
 * without it every stop ended in "Timed out waiting for the capture graph to stop" and mediaserverd's SIGTRAP. Ours
 * posted none (2026-11-25), and the 2026-11-24 boot's graph never reached a Start. Boot argument
 * oneios-camera-notify=0 posts none (docs/switches.md). Each posted name is said once: "stream: posted StreamStarted".
 */
static bool
notify_on(void)
{
	static int on = -1;

	if (on < 0) {
		char args[1024] = { 0 };
		size_t size = sizeof(args) - 1;

		on = sysctlbyname("kern.bootargs", args, &size, NULL, 0) != 0 || strstr(args, "oneios-camera-notify=0") == NULL;
	}
	return on != 0;
}

static void
post(CMBaseObjRef o, CFStringRef name)
{
	static void *(*center)(void);
	static OSStatus (*post_notification)(void *, CFStringRef, const void *, CFTypeRef, uint32_t);
	static bool looked;
	static unsigned said;

	if (!notify_on()) {
		return;
	}
	if (!looked) {
		looked = true;
		center = dlsym(RTLD_DEFAULT, "CMNotificationCenterGetDefaultLocalCenter");
		post_notification = dlsym(RTLD_DEFAULT, "CMNotificationCenterPostNotification");
		if (center == NULL || post_notification == NULL) {
			oneios_camera_say("stream: no CMNotificationCenter: the stream's notifications are not posted");
		}
	}
	if (center != NULL && post_notification != NULL) {
		(void)post_notification(center(), name, o, NULL, 0);
		if (said++ < 8) {
			char text[32] = "";

			(void)CFStringGetCString(name, text, sizeof(text), kCFStringEncodingUTF8);
			oneios_camera_say("stream: posted %s", text);
		}
	}
}

/* Frames are read from the graph's Start on and given out while it runs (stream_stop ends both). */
static OSStatus
stream_begin(CMBaseObjRef o, bool graph)
{
	struct stream *s = stream_of(o);

	if (graph) {
		pthread_mutex_lock(&s->lock);
		s->graph_running = true;
		pthread_mutex_unlock(&s->lock);
		oneios_camera_say("stream %s: the graph starts the stream (format %d)", s->camera->name, s->format_index);
	}
	if (s->running) {
		return 0;
	}
	if (s->worker_started) {
		pthread_join(s->worker, NULL);
		s->worker_started = false;
	}
	for (int i = 0; i < MAX_STREAMS; i++) {         /* one camera at a time: the kernel has one client */
		if (all_streams[i] != NULL && all_streams[i] != o && stream_of(all_streams[i])->running) {
			(void)stream_stop(all_streams[i]);
		}
	}
	s->running = true;
	s->frames = 0;
	if (pthread_create(&s->worker, NULL, worker, o) != 0) {
		s->running = false;
		return ERR_UNSUPPORTED;
	}
	s->worker_started = true;
	return 0;
}

static OSStatus
stream_start(CMBaseObjRef o)
{
	OSStatus st;

	post(o, CFSTR("StreamStarting"));
	st = stream_begin(o, true);
	if (st == 0) {
		post(o, CFSTR("StreamStarted"));
	}
	return st;
}

static OSStatus
stream_stop(CMBaseObjRef o)
{
	struct stream *s = stream_of(o);

	post(o, CFSTR("StreamStopping"));   /* every Stop, as Jesvi-iOS posts them: the graph waits for StreamStopped */
	pthread_mutex_lock(&s->lock);
	s->graph_running = false;
	for (int i = 0; i < OUTPUTS; i++) {
		let_go(&s->out[i]);
	}
	pthread_mutex_unlock(&s->lock);
	s->running = false;
	if (s->worker_started) {
		pthread_join(s->worker, NULL);
		s->worker_started = false;
		oneios_camera_say("stream %s: stopped (%llu pictures given)", s->camera->name, (unsigned long long)s->out[0].delivered);
	}
	post(o, CFSTR("StreamStopped"));
	return 0;
}

static OSStatus
stream_add_attachments(CMBaseObjRef o, CMSampleBufferRef sbuf, CFDictionaryRef options)
{
	struct stream *s = stream_of(o);
	CVImageBufferRef image = sbuf != NULL ? CMSampleBufferGetImageBuffer(sbuf) : NULL;
	CFTypeRef ours = image != NULL && md_attachment != NULL ? CVBufferCopyAttachment(image, md_attachment, NULL) : NULL;

	(void)options;
	(void)s;
	if (ours != NULL) {
		if (CMGetAttachment(sbuf, md_attachment, NULL) == NULL) {
			CMSetAttachment(sbuf, md_attachment, ours, kCMAttachmentMode_ShouldPropagate);
		}
		CFRelease(ours);
	}
	return 0;
}

static OSStatus
stream_invalidate(CMBaseObjRef o)
{
	return stream_stop(o);
}

static void
stream_finalize(CMBaseObjRef o)
{
	struct stream *s = stream_of(o);

	(void)stream_stop(o);
	for (int i = 0; i < MAX_STREAMS; i++) {
		if (all_streams[i] == o) {
			all_streams[i] = NULL;
		}
	}
	for (int i = 0; i < OUTPUTS; i++) {
		if (s->out[i].config) {
			CFRelease(s->out[i].config);
		}
		if (s->out[i].handler) {
			CFRelease(s->out[i].handler);
		}
	}
	if (s->formats) {
		CFRelease(s->formats);
	}
	if (s->supported) {
		CFRelease(s->supported);
	}
	if (s->colour_info) {
		CFRelease(s->colour_info);
	}
	pthread_mutex_destroy(&s->lock);
}

static CFStringRef
stream_description(CMBaseObjRef o)
{
	return CFStringCreateWithFormat(NULL, NULL, CFSTR("<OneiOSCaptureStream: %s, the phone's camera %u>"), stream_of(o)->camera->name,
	    stream_of(o)->camera->role);
}

static const struct base_class stream_base = {
	1, sizeof(struct stream), NULL, stream_invalidate, stream_finalize, stream_description, stream_copy_property, stream_set_property,
	NULL, { NULL, NULL, NULL }
};
static const struct stream_class stream_derived = { 2, stream_start, stream_stop, stream_add_attachments, { NULL, NULL } };
static const struct vtable stream_vtable = { NULL, &stream_base, &stream_derived };

static CMBaseObjRef
stream_create(CFAllocatorRef allocator, const struct camera *camera)
{
	CMBaseObjRef o = NULL;
	struct stream *s;

	if (derived_create(allocator, &stream_vtable, stream_class_id(), &o) != 0 || o == NULL) {
		oneios_camera_say("the stream of %s cannot be made", camera->name);
		return NULL;
	}
	s = stream_of(o);
	memset(s, 0, sizeof(*s));
	pthread_mutex_init(&s->lock, NULL);
	s->camera = camera;
	s->port = camera->front ? port_front : port_back;
	s->sensor_id = sensor_id_for(s->port, camera);
	s->min_fps = 2;
	s->max_fps = 30;
	s->zoom = 1;
	for (int i = 0; i < OUTPUTS; i++) {
		s->out[i].name = output_names[i];
	}
	s->formats = formats_for(camera);
	s->supported = supported(stream_props, sizeof(stream_props) / sizeof(stream_props[0]));
	return o;
}

/* ---- the device ---- */
struct device {
	CFArrayRef streams;
	CFDictionaryRef supported;
};

static OSStatus
device_copy_property(CMBaseObjRef o, CFStringRef key, CFAllocatorRef allocator, void *out)
{
	struct device *d = derived_storage(o);
	CFTypeRef value;

	(void)allocator;
	if (key_is(key, "StreamArray")) {
		value = CFRetain(d->streams);
	} else if (key_is(key, "Clock")) {
		value = CFRetain(CMClockGetHostTimeClock());
	} else if (key_is(key, "SupportedPropertiesDictionary")) {
		value = CFRetain(d->supported);
	} else if (key_is(key, "SupportedSynchronizedStreamsGroups")) {
		value = CFArrayCreate(NULL, NULL, 0, &kCFTypeArrayCallBacks);
	} else if (key_is(key, "ClientIsActive")) {
		value = CFRetain(kCFBooleanTrue);
	} else {
		say_once("device: no answer for", key, NULL);
		return ERR_NOT_AVAILABLE;
	}
	*(CFTypeRef *)out = value;
	return 0;
}

static OSStatus
device_set_property(CMBaseObjRef o, CFStringRef key, CFTypeRef value)
{
	(void)o;
	say_once("device: set (taken, not used)", key, value);
	return 0;
}

static OSStatus
device_request_streams(CMBaseObjRef o, CFArrayRef streams, CFDictionaryRef options)
{
	(void)o;
	(void)options;
	oneios_camera_say("device: streams asked for (%ld)", streams ? (long)CFArrayGetCount(streams) : 0L);
	/*
	 * The vendor reserves the streams for each device it publishes too (every start of cameracaptured), not only for a
	 * capture session: HakiOS opened the camera here, so each start of cameracaptured opened the phone's camera for
	 * frames nobody took ("reading frames through AppleH9CamIn", then "no frame", phone 2026-11-23). The camera is
	 * opened by the graph's Start (stream_start) only.
	 */
	return 0;
}

static OSStatus
device_relinquish_streams(CMBaseObjRef o, CFArrayRef streams, CFDictionaryRef options)
{
	static unsigned said;

	(void)o;
	(void)options;
	if (said++ < 5) {
		oneios_camera_say("device: streams given back (%ld)", streams ? (long)CFArrayGetCount(streams) : 0L);
	}
	/* a stream given back is not read any more: the phone's camera closes with it */
	for (CFIndex i = 0; streams != NULL && i < CFArrayGetCount(streams); i++) {
		CMBaseObjRef stream = (CMBaseObjRef)CFArrayGetValueAtIndex(streams, i);

		for (int k = 0; k < MAX_STREAMS; k++) {
			if (all_streams[k] == stream && stream_of(stream)->running) {
				(void)stream_stop(stream);
			}
		}
	}
	return 0;
}

static OSStatus
device_invalidate(CMBaseObjRef o)
{
	(void)o;
	return 0;
}

static void
device_finalize(CMBaseObjRef o)
{
	struct device *d = derived_storage(o);

	if (d->streams) {
		CFRelease(d->streams);
	}
	if (d->supported) {
		CFRelease(d->supported);
	}
}

static CFStringRef
device_description(CMBaseObjRef o)
{
	(void)o;
	return CFRetain(CFSTR("<OneiOSCaptureDevice: the Galaxy S24 Ultra's cameras through xnuhost>"));
}

static const struct base_class device_base = {
	1, sizeof(struct device), NULL, device_invalidate, device_finalize, device_description, device_copy_property, device_set_property,
	NULL, { NULL, NULL, NULL }
};
static const struct device_class device_derived = { 3, NULL, device_request_streams, device_relinquish_streams, NULL, { NULL, NULL, NULL } };
static const struct vtable device_vtable = { NULL, &device_base, &device_derived };

/* Which of the known cameras the kernel has: from its list, or (a reader without the list) the back one. */
static unsigned
cameras_there(const struct camera **out)
{
	oneios_camera_reader r;
	struct oc_camera list[OC_MAX_CAMERAS];
	unsigned n = 0, have = 0;
	int e = oneios_camera_reader_open(&r, NULL);

	if (e == 0) {
		have = oneios_camera_reader_cameras(&r, list, OC_MAX_CAMERAS);
		oneios_camera_reader_close(&r);
	} else if (e != -EBUSY) {
		oneios_camera_say("the kernel's camera: %s", strerror(-e));
		return 0;
	}
	for (unsigned k = 0; k < MAX_STREAMS; k++) {
		bool listed = have == 0 && known[k].role == ONEIOS_CAMERA_WIDE;

		for (unsigned i = 0; i < have; i++) {
			listed = listed || list[i].role == known[k].role;
		}
		if (listed) {
			out[n++] = &known[k];
		}
	}
	return n;
}

OSStatus H9ISPCaptureDeviceCreate(CFAllocatorRef allocator, CFDictionaryRef options, CMBaseObjRef *out);

__attribute__((visibility("default"))) OSStatus
H9ISPCaptureDeviceCreate(CFAllocatorRef allocator, CFDictionaryRef options, CMBaseObjRef *out)
{
	static bool usable;
	static unsigned calls;
	const struct camera *cams[MAX_STREAMS];
	CMBaseObjRef device = NULL, streams[MAX_STREAMS];
	struct device *d;
	unsigned n;

	if (out == NULL) {
		return ERR_PARAM;
	}
	*out = NULL;
	if (calls++ < 3) {
		say_once("device create: options", CFSTR("(the vendor's)"), options);
	}
	if (oneios_camera_off()) {
		return ERR_NOT_AVAILABLE;
	}
	if (!usable) {
		static bool tried;

		if (!tried) {
			tried = true;
			usable = find_functions();
			session_plist = read_plist(ONEIOS_CAMERA_SESSION_PLIST);
			setup_plist = read_plist(ONEIOS_CAMERA_SETUP_PLIST);
			oneios_camera_say("%s: %s; %s: %s", ONEIOS_CAMERA_SESSION_PLIST, session_plist ? "read" : "not read: formats of our own",
			    ONEIOS_CAMERA_SETUP_PLIST, setup_plist ? "read" : "not read: V54's sensor IDs");
		}
		if (!usable) {
			return ERR_UNSUPPORTED;
		}
	}
	if ((n = cameras_there(cams)) == 0) {
		oneios_camera_say("no device: the kernel has no camera for iOS");
		return ERR_NOT_AVAILABLE;
	}
	for (unsigned i = 0; i < n; i++) {
		if ((streams[i] = stream_create(allocator, cams[i])) == NULL) {
			for (unsigned k = 0; k < i; k++) {
				CFRelease(streams[k]);
			}
			return ERR_UNSUPPORTED;
		}
	}
	if (derived_create(allocator, &device_vtable, device_class_id(), &device) != 0 || device == NULL) {
		for (unsigned k = 0; k < n; k++) {
			CFRelease(streams[k]);
		}
		oneios_camera_say("the device cannot be made");
		return ERR_UNSUPPORTED;
	}
	d = derived_storage(device);
	memset(d, 0, sizeof(*d));
	d->streams = CFArrayCreate(NULL, (const void **)streams, (CFIndex)n, &kCFTypeArrayCallBacks);
	d->supported = supported(device_props, sizeof(device_props) / sizeof(device_props[0]));
	for (unsigned i = 0; i < MAX_STREAMS; i++) {
		all_streams[i] = i < n ? streams[i] : NULL;
	}
	for (unsigned i = 0; i < n; i++) {
		oneios_camera_say("the device made: stream %u %s (%ld formats)", i, cams[i]->name, (long)CFArrayGetCount(stream_of(streams[i])->formats));
		CFRelease(streams[i]);
	}
	*out = device;
	return 0;
}
