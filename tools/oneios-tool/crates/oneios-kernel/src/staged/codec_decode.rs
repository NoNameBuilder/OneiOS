// Written with `oneios-tool kernel gen codec` (2026-11-24): no Python original.
/// DroidVMVTHost.cpp's decoder lines that change (edit 6 of the codec archive; edits 1 to 5, the encoder's, are
/// staged/codec.rs, applied first).
///
/// 6. The hardware decoder (2026-11-24). YouTube in Safari decoded on the CPU (videocodecd and WebKit's processes,
///    `out/session-logs/2026-11-24-mac-gpumem`): xnuhost's device was an encoder only, so `decs` listed nothing. Since
///    then the device is an encoder and a decoder (`oneios-encoder+decoder`, xnuhost's `decoder=auto`; the decoder is
///    the phone's MediaCodec). What upstream's decoder half lacks, read in 24A446's VideoToolbox
///    (`out/ios27-cache`, `ipsw dyld extract`, 2026-11-24):
///    - VP9 and AV1. The guest's paravirtualized decoder takes any codec type the host lists (`vtLoadParavirtualizedVideo
///      Decoders` registers each `decs` entry; `VTIsHardwareDecodeSupported` creates an instance with a hardware decoder
///      required, VP9 behind `VTIsVP9DecodeEnabledForCurrentProcess`). `decs` lists 'vp09' and 'av01' too when the
///      device's OUTPUT queue offers V4L2_PIX_FMT_VP9 ('VP90') and 'AV01' (VIDIOC_ENUM_FMT, asked once). A VP9 or AV1
///      sample is the codec's own frame (no length prefixes): copied as it is; AV1's configOBUs (the av1C's bytes after
///      its first four: the sequence header) go before the first frame, as an avcC's parameter sets do. Only 8-bit
///      4:2:0 is taken (VP9 profile 0 by its vpcC, AV1 Main 8-bit by its av1C): the CAPTURE queue is NV12 and the guest
///      is asked for 420v; a 10-bit stream is refused at dsta and VideoToolbox decodes it in software, as before.
///    - `dcap` (`vtCreateDecoderCapabilitiesDictionary` in a guest: `VTCopyAV1DecoderCapabilitiesDictionary`, `VTCopyHEVC
///      DecoderCapabilitiesDictionary`, which WebCore asks before it offers a codec): `,ver` and `,cod`, the answer
///      `,cda`, the dictionary 24A446's own host makes: VTSupportedProfiles (an array of profile numbers),
///      VTPerProfileSupport (by the profile's number as a string: VTIsHardwareAccelerated, VTMaxDecodeLevel,
///      VTMaxPlaybackLevel). Upstream answered the unknown type's -12900, so the guest had no capabilities.
///    - `dcan` (`ParavirtualizedVideoDecoder_CanAcceptFormatDescription`, `,fds`, the answer `,can`): yes for the same
///      codec and size in a format the device decodes, its new parameter sets before the next frame; else no, and
///      VideoToolbox makes a new session. Upstream's -12900 made every format change a new session.
///    - The surface's kernel mapping is kept for the session (at most 16 surfaces): upstream mapped every surface into
///      the kernel and unmapped it again for every frame (`copyFrame`), a mapping of about 3 MB per 1080p frame. The
///      pages are wired only while the mapping is made (`complete()` at once), so a process that frees its surface never
///      waits for the session.
///    - A SOURCE_CHANGE while CAPTURE streams takes the new geometry (G_FMT) when it fits the buffers.
///    - Counters for OneiOSCodec.c's `decoded:` line: frames handed over decoded and dropped, by codec (avc1, hvc1,
///      vp09, av01), and the time from a frame's dfrm to its Demt (sum, count, max; the codec's reordering is in it).
pub const CODEC_DECODER: &[(&str, &str)] = &[
    // The counters.
    ("volatile uint32_t droidvm_vthost_trace_count;\n}\n",
     r#"volatile uint32_t droidvm_vthost_trace_count;
// OneiOS (kernel gen codec, 2026-11-24): the decoder's frames by codec (avc1, hvc1, vp09, av01), handed over decoded and
// dropped; the time from a frame's dfrm to its Demt, in microseconds (sum, count, the largest since OneiOSCodec.c's last
// line, which sets it back to 0).
volatile uint32_t oneios_decode_frames[4], oneios_decode_dropped[4];
volatile uint64_t oneios_decode_us_sum;
volatile uint32_t oneios_decode_us_count, oneios_decode_us_max;
}
"#),
    // The decoder session's state.
    ("    bool replyHasSurface = false;\n    SurfaceView replySurface;\n};\n",
     r#"    bool replyHasSurface = false;
    SurfaceView replySurface;
    // OneiOS (kernel gen codec, 2026-11-24): a decoder session's configuration record (the avcC, hvcC, vpcC or av1C body
    // dsta took, for dcan), when each frame was given (dfrm, for the time to its Demt), and the surfaces mapped into the
    // kernel for it (kept until the session ends).
    uint8_t record[512] = {};
    uint32_t recordSize = 0;
    struct { int64_t frame; uint64_t at; } given[64] = {};
    unsigned givenNext = 0;
    struct { uint32_t id; uint64_t base; IOMemoryDescriptor *memory; IOMemoryMap *map; uint8_t *bytes; } mappedSurfaces[16] = {};
    unsigned mappedCount = 0;
};
"#),
    // The codecs, before the first use (emitFrame's counters).
    ("int ioctl(Session *s, uint32_t code, void *payload, uint32_t out, uint32_t in) {\n",
     r#"// OneiOS (kernel gen codec, 2026-11-24): the decoder's codecs, in the order of the counters and of the device's bits:
// avc1, hvc1 (hev1 too), vp09, av01; -1 for another type.
int codecIndex(uint32_t codec) {
    if (codec == cc("avc1")) return 0;
    if (codec == cc("hvc1") || codec == cc("hev1")) return 1;
    if (codec == cc("vp09")) return 2;
    if (codec == cc("av01")) return 3;
    return -1;
}

// The V4L2 format of each (little endian): V4L2_PIX_FMT_H264, _HEVC, _VP9 ('VP90'), and 'AV01' (xnuhost's device).
uint32_t decoderFourcc(uint32_t codec) {
    static const uint32_t fourcc[4] = {cc("462H"), cc("CVEH"), cc("09PV"), cc("10VA")};
    const int index = codecIndex(codec);
    return index < 0 ? 0 : fourcc[index];
}

// A VP9 or AV1 sample is the codec's frame as it is, with no NAL length prefixes.
bool rawStream(uint32_t codec) { return codecIndex(codec) >= 2; }

// The codecs the decoder device offers (bit i: codecIndex i): its OUTPUT queue's formats (VIDIOC_ENUM_FMT, asked once in a
// session of its own); H.264 and HEVC, upstream's two, when it cannot be asked.
uint32_t decoderCodecs() {
    static uint32_t codecs;
    static bool asked;
    const unsigned device = decoderDevice();
    if (device == ~0u) return 0;
    if (asked) return codecs;
    uint32_t found = 0, session = 0;
    if (droidvm_media_open(device, &session) == 0) {
        for (uint32_t index = 0; index < 16; ++index) {
            uint32_t desc[16] = {index, 10 /* TypeOutputMplane */};   // struct v4l2_fmtdesc; pixelformat at 44
            if (droidvm_media_ioctl(device, session, 2 /* VIDIOC_ENUM_FMT */, desc, sizeof desc, sizeof desc)) break;
            for (int i = 0; i < 4; ++i) {
                static const uint32_t fourcc[4] = {cc("462H"), cc("CVEH"), cc("09PV"), cc("10VA")};
                if (desc[11] == fourcc[i]) found |= 1u << i;
            }
        }
        droidvm_media_close(device, session);
    }
    codecs = found ? found : 3;
    asked = true;
    trace("decoder codecs %#x (avc1 hvc1 vp09 av01)", codecs);
    return codecs;
}

int ioctl(Session *s, uint32_t code, void *payload, uint32_t out, uint32_t in) {
"#),
    // The surface's kernel mapping, kept.
    (r#"void copyFrame(Session *s, const Mapped &from, const SurfaceView &to) {
    IOMemoryDescriptor *memory = IOMemoryDescriptor::withAddressRange(to.base, to.size, kIODirectionInOut, s->client->task);
    if (!memory || memory->prepare() != kIOReturnSuccess) { OSSafeReleaseNULL(memory); failed(60, 0); return; }
    IOMemoryMap *map = memory->createMappingInTask(kernel_task, 0, kIOMapAnywhere);
    uint8_t *surface = map ? reinterpret_cast<uint8_t *>(map->getVirtualAddress()) : nullptr;
    if (surface) {
"#,
     r#"// OneiOS (kernel gen codec, 2026-11-24): a decoded picture into its surface through the surface's kernel mapping, made at
// the surface's first picture and kept for the session (the guest's pool hands the same few surfaces out again and again;
// upstream mapped and unmapped the whole surface for every frame). The pages are wired only while the mapping is made: a
// process that frees the surface must not wait for this session. A 17th surface is mapped for its picture alone.
void copyFrame(Session *s, const Mapped &from, const SurfaceView &to) {
    IOMemoryDescriptor *memory = nullptr;
    IOMemoryMap *map = nullptr;
    uint8_t *surface = nullptr;
    for (unsigned i = 0; i < s->mappedCount && !surface; ++i)
        if (s->mappedSurfaces[i].id == to.id && s->mappedSurfaces[i].base == to.base) surface = s->mappedSurfaces[i].bytes;
    if (!surface) {
        memory = IOMemoryDescriptor::withAddressRange(to.base, to.size, kIODirectionInOut, s->client->task);
        if (!memory || memory->prepare() != kIOReturnSuccess) { OSSafeReleaseNULL(memory); failed(60, 0); return; }
        map = memory->createMappingInTask(kernel_task, 0, kIOMapAnywhere);
        memory->complete();
        surface = map ? reinterpret_cast<uint8_t *>(map->getVirtualAddress()) : nullptr;
        if (surface && s->mappedCount < 16) {
            s->mappedSurfaces[s->mappedCount++] = {to.id, to.base, memory, map, surface};
            memory = nullptr;
            map = nullptr;
        }
    }
    if (surface) {
"#),
    (r#"    } else {
        failed(61, 0);
    }
    OSSafeReleaseNULL(map);
    memory->complete();
    memory->release();
}
"#,
     r#"    } else {
        failed(61, 0);
    }
    OSSafeReleaseNULL(map);
    OSSafeReleaseNULL(memory);
}

// OneiOS (kernel gen codec, 2026-11-24): the time from the frame's dfrm (the lock held).
uint32_t givenMicroseconds(Session *s, int64_t frame) {
    for (auto &g : s->given) {
        if (g.at && g.frame == frame) {
            uint64_t ns = 0;
            absolutetime_to_nanoseconds(mach_absolute_time() - g.at, &ns);
            g.at = 0;
            return uint32_t(ns / 1000 > 0xffffffffull ? 0xffffffffull : ns / 1000);
        }
    }
    return 0;
}

void countDecoded(Session *s, int64_t frame, bool decoded) {
    const int index = codecIndex(s->codec);
    if (index < 0) return;
    IOLockLock(s->lock);
    const uint32_t us = givenMicroseconds(s, frame);
    IOLockUnlock(s->lock);
    if (!decoded) { oneios_decode_dropped[index] = oneios_decode_dropped[index] + 1; return; }
    oneios_decode_frames[index] = oneios_decode_frames[index] + 1;
    if (!us) return;
    __atomic_fetch_add(&oneios_decode_us_sum, uint64_t(us), __ATOMIC_RELAXED);
    __atomic_fetch_add(&oneios_decode_us_count, 1u, __ATOMIC_RELAXED);
    if (us > oneios_decode_us_max) oneios_decode_us_max = us;
}
"#),
    // A Dbuf the guest did not answer: the frame is dropped.
    ("    if (!hostRequest(s, w, message, request) || s->replyError || !s->replyHasSurface) {\n        failed(31, s->replyError);\n        return;\n    }\n",
     "    if (!hostRequest(s, w, message, request) || s->replyError || !s->replyHasSurface) {\n        failed(31, s->replyError);\n        countDecoded(s, frame, false);   // OneiOS: counted\n        return;\n    }\n"),
    ("    if (hostRequest(s, e, message, emit)) droidvm_vthost_frames_out = droidvm_vthost_frames_out + 1;\n}\n",
     "    if (hostRequest(s, e, message, emit)) droidvm_vthost_frames_out = droidvm_vthost_frames_out + 1;\n    countDecoded(s, frame, bytesused != 0);   // OneiOS (2026-11-24): the decoder's counters\n}\n"),
    ("        e.u32(cc(\",inf\"), 2);   // kVTDecodeInfo_FrameDropped\n",
     "        e.u32(cc(\",inf\"), 2);   // kVTDecodeInfo_FrameDropped\n        countDecoded(s, frame, false);   // OneiOS: counted\n"),
    // A new geometry while CAPTURE streams.
    ("void workerMain(void *argument, wait_result_t) {\n",
     r#"// OneiOS (kernel gen codec, 2026-11-24): a SOURCE_CHANGE while CAPTURE streams: the device's new geometry, when its
// pictures fit the buffers already mapped (upstream kept the first one; a picture larger than the buffers is not taken).
void refreshCapture(Session *s) {
    V4l2Format f = {};
    f.type = TypeCaptureMplane;
    const int r = ioctl(s, IocGFmt, &f, sizeof f, sizeof f);
    if (r) { failed(57, r); return; }
    const uint32_t stride = f.planes[0].bytesperline ? f.planes[0].bytesperline : f.width;
    uint64_t smallest = ~0ull;
    for (unsigned i = 0; i < s->captureCount; ++i) if (s->capture[i].length < smallest) smallest = s->capture[i].length;
    if (uint64_t(stride) * f.height * 3 / 2 > smallest) { failed(58, int32_t(f.width)); return; }
    s->captureWidth = f.width;
    s->captureHeight = f.height;
    s->captureStride = stride;
    trace("decoder capture now %ux%u stride %u", f.width, f.height, stride);
}

void workerMain(void *argument, wait_result_t) {
"#),
    ("            if (!s->captureStreaming) setUpCapture(s);\n",
     "            if (!s->captureStreaming) setUpCapture(s);\n            else refreshCapture(s);   // OneiOS (2026-11-24)\n"),
    // The decoder list: the codecs the device offers.
    (r#"        // H.264 and HEVC: what the virtio-media decoder takes (ENUM_FMT) and VideoToolbox can describe for it.
        static const char *const codecs[2][3] = {
            {"avc1", "com.droidvm.videodecoder.virtio-media.avc", "DroidVM Video Decoder - AVC (virtio-media)"},
            {"hvc1", "com.droidvm.videodecoder.virtio-media.hevc", "DroidVM Video Decoder - HEVC (virtio-media)"},
        };
        int entries[2];
        for (int i = 0; i < 2; ++i) {
"#,
     r#"        // OneiOS (kernel gen codec, 2026-11-24): H.264, HEVC, VP9 and AV1, those the virtio-media decoder takes (its
        // OUTPUT formats, decoderCodecs).
        static const char *const codecs[4][3] = {
            {"avc1", "com.droidvm.videodecoder.virtio-media.avc", "DroidVM Video Decoder - AVC (virtio-media)"},
            {"hvc1", "com.droidvm.videodecoder.virtio-media.hevc", "DroidVM Video Decoder - HEVC (virtio-media)"},
            {"vp09", "com.oneios.videodecoder.virtio-media.vp9", "OneiOS Video Decoder - VP9 (virtio-media)"},
            {"av01", "com.oneios.videodecoder.virtio-media.av1", "OneiOS Video Decoder - AV1 (virtio-media)"},
        };
        const uint32_t offered = decoderCodecs();
        int entries[4];
        uint32_t count = 0;
        for (int i = 0; i < 4; ++i) {
            if (!(offered & (1u << i))) continue;
"#),
    ("            entries[i] = p->dict(keys, values, 5);\n        }\n        const int list = p->array(entries, decoderDevice() == ~0u ? 0 : 2);\n",
     "            entries[count++] = p->dict(keys, values, 5);\n        }\n        const int list = p->array(entries, count);\n"),
    // VP9 and AV1: 8-bit 4:2:0 only.
    ("    return false;\n}\n\nbool decodableSize(Session *s, uint32_t pixelformat) {\n",
     r#"    // OneiOS (kernel gen codec, 2026-11-24): VP9 profile 0 (8-bit 4:2:0: the vpcC, a full box, its profile, level, then
    // bit depth (4 bits), chroma subsampling (3) and full range (1)) and AV1 Main 8-bit 4:2:0 (the av1C: marker and
    // version, seq_profile (3 bits) and seq_level_idx_0 (5), then tier, high_bitdepth, twelve_bit, monochrome,
    // chroma_subsampling_x, _y, chroma_sample_position (2)). A description without its record is not taken.
    if (find(d.extensions, d.extensionsSize, cc("vpcC"), record)) {
        if (record.size < 12) return false;
        const uint8_t profile = record.body[4], depth = record.body[6] >> 4, chroma = (record.body[6] >> 1) & 7;
        return profile == 0 && depth == 8 && chroma <= 1;   // 4:2:0 vertical or colocated
    }
    if (find(d.extensions, d.extensionsSize, cc("av1C"), record)) {
        if (record.size < 4 || record.body[0] != 0x81) return false;
        const uint8_t profile = record.body[1] >> 5, flags = record.body[2];
        const bool high = flags & 0x40, mono = flags & 0x10, x = flags & 0x08, y = flags & 0x04;
        return profile == 0 && !high && !mono && x && y;
    }
    return false;
}

// OneiOS (kernel gen codec, 2026-11-24): the configuration record of a description (the avcC, hvcC, vpcC or av1C).
bool configurationAtom(const ImageDescription &d, Atom &record) {
    const uint8_t *e = d.extensions;
    const uint32_t n = d.extensionsSize;
    return find(e, n, cc("avcC"), record) || find(e, n, cc("hvcC"), record) || find(e, n, cc("vpcC"), record) || find(e, n, cc("av1C"), record);
}

// A VP9 or AV1 stream's headers: none for VP9 (every key frame carries its own); for AV1 the av1C's configOBUs (its sequence
// header), which go before the first frame. No length prefixes (nalLength 0).
bool streamHeaders(Session *s, const ImageDescription &d) {
    Atom record;
    s->nalLength = 0;
    s->parameterSetsSize = 0;
    if (d.codec != cc("av01")) return true;
    if (!find(d.extensions, d.extensionsSize, cc("av1C"), record) || record.size < 4) return false;
    const uint32_t n = record.size - 4;
    if (n > sizeof s->parameterSets) return false;
    memcpy(s->parameterSets, record.body + 4, n);
    s->parameterSetsSize = n;
    return true;
}

bool decodableSize(Session *s, uint32_t pixelformat) {
"#),
    // The session's start for any of the four.
    ("    if (d.codec != cc(\"avc1\") && d.codec != cc(\"hvc1\") && d.codec != cc(\"hev1\")) return -12906;\n",
     "    // OneiOS (kernel gen codec, 2026-11-24): any codec the device offers (H.264, HEVC, VP9, AV1)\n    if (codecIndex(d.codec) < 0 || !(decoderCodecs() & (1u << codecIndex(d.codec)))) return -12906;\n"),
    ("    if (!parameterSets(s, d)) return -12909;\n",
     r#"    if (!(rawStream(d.codec) ? streamHeaders(s, d) : parameterSets(s, d))) return -12909;   // OneiOS: VP9 and AV1 have no NAL units
    {   // OneiOS: the record, for dcan
        Atom record;
        s->recordSize = configurationAtom(d, record) && record.size <= sizeof s->record ? record.size : 0;
        if (s->recordSize) memcpy(s->record, record.body, s->recordSize);
    }
"#),
    ("    if (!decodableSize(s, d.codec == cc(\"avc1\") ? cc(\"462H\") : cc(\"CVEH\"))) { teardown(s); return -12910; }\n",
     "    if (!decodableSize(s, decoderFourcc(d.codec))) { teardown(s); return -12910; }   // OneiOS: the codec's V4L2 format\n"),
    ("    f.pixelformat = d.codec == cc(\"avc1\") ? cc(\"462H\") : cc(\"CVEH\");   // V4L2_PIX_FMT_H264 'H264', _HEVC 'HEVC' (little endian)\n",
     "    f.pixelformat = decoderFourcc(d.codec);   // OneiOS: V4L2_PIX_FMT_H264 'H264', _HEVC 'HEVC', _VP9 'VP90', 'AV01' (little endian)\n"),
    ("    IOLog(\"DroidVMVTHost: session %u %ux%u %c%c%c%c\\n\", s->v4l2, s->width, s->height, d.codec >> 24, d.codec >> 16, d.codec >> 8, d.codec);\n",
     "    IOLog(\"DroidVMVTHost: session %u %ux%u %c%c%c%c\\n\", s->v4l2, s->width, s->height, d.codec >> 24, d.codec >> 16, d.codec >> 8, d.codec);\n    trace(\"decoder session %u %ux%u %c%c%c%c\", s->v4l2, s->width, s->height, d.codec >> 24, d.codec >> 16, d.codec >> 8, d.codec);   // OneiOS\n"),
    // A VP9 or AV1 frame as it is.
    ("    const uint8_t *p = sdat.body, *end = sdat.body + sdat.size;\n    while (uint32_t(end - p) > s->nalLength) {\n",
     r#"    const uint8_t *p = sdat.body, *end = sdat.body + sdat.size;
    if (!s->nalLength && at + sdat.size <= capacity) {   // OneiOS (2026-11-24): a VP9 or AV1 frame, as it is
        memcpy(bytes + at, sdat.body, sdat.size);
        at += sdat.size;
    }
    while (s->nalLength && uint32_t(end - p) > s->nalLength) {
"#),
    ("    if (s->outstandingCount < 64) s->outstanding[s->outstandingCount++] = frame;\n",
     "    if (s->outstandingCount < 64) s->outstanding[s->outstandingCount++] = frame;\n    s->given[s->givenNext++ % 64] = {frame, mach_absolute_time()};   // OneiOS: for the time to its Demt\n"),
    // dcap and dcan, after queueFrame (which they follow in the file).
    ("}  // namespace\n\nextern \"C\" {\n\n// Whether crosvm gave the guest",
     r#"// OneiOS (kernel gen codec, 2026-11-24): dcap, a codec's capabilities as 24A446's own host answers them (,cda): the profiles
// the device decodes, each hardware accelerated, with the highest level it plays (the media pool's size bounds it: 4K).
// H.264 Baseline, Main, High at level 5.1 (51); HEVC Main and Main 10 at 5.1 (general_level_idc 153); VP9 profile 0 at 5.1
// (51, VideoToolbox's own figure); AV1 Main at 5.1 (seq_level_idx 13).
void decoderCapabilities(Client *c, const Header &h, const uint8_t *m, uint32_t n) {
    int32_t codec = 0;
    paramS32(m, n, cc(",cod"), codec);
    const int index = codecIndex(uint32_t(codec));
    if (index < 0 || !(decoderCodecs() & (1u << index))) { replyStatus(c, h, -12906); return; }   // kVTCouldNotFindVideoDecoderErr
    static const struct { uint8_t count; uint8_t profiles[3]; const char *names[3]; int level; } table[4] = {
        {3, {66, 77, 100}, {"66", "77", "100"}, 51},
        {2, {1, 2}, {"1", "2"}, 153},
        {1, {0}, {"0"}, 51},
        {1, {0}, {"0"}, 13},
    };
    Plist *p = new Plist();
    uint8_t *buffer = static_cast<uint8_t *>(IOMallocZero(2048));
    if (p && buffer) {
        const auto &t = table[index];
        int profiles[3], names[3], entries[3];
        for (unsigned i = 0; i < t.count; ++i) {
            profiles[i] = p->integer(t.profiles[i]);
            names[i] = p->string(t.names[i]);
            const int keys[3] = {p->string("VTIsHardwareAccelerated"), p->string("VTMaxDecodeLevel"), p->string("VTMaxPlaybackLevel")};
            const int values[3] = {p->boolean(true), p->integer(t.level), p->integer(t.level)};
            entries[i] = p->dict(keys, values, 3);
        }
        const int keys[2] = {p->string("VTSupportedProfiles"), p->string("VTPerProfileSupport")};
        const int values[2] = {p->array(profiles, t.count), p->dict(names, entries, t.count)};
        const int capabilities = p->dict(keys, values, 2);
        Writer w(buffer, 2048);
        w.header(h.type, FlagReply, h.uuid, h.request);
        w.s32(cc(",err"), 0);
        w.plist(cc(",cda"), *p, capabilities);
        sendToGuest(c, w, buffer);
    } else {
        replyStatus(c, h, -12904);
    }
    delete p;
    if (buffer) IOFree(buffer, 2048);
}

// dcan: whether the session takes the new description (,fds) as it is: the same codec and size, in a format the device
// decodes. Its parameter sets (an AV1 sequence header) go before the next frame. ,can 0: VideoToolbox makes a new session.
int32_t acceptFormat(Session *s, const uint8_t *m, uint32_t n) {
    Param fds;
    ImageDescription d;
    if (!s || !s->outputStreaming || !param(m, n, cc(",fds"), fds) || !imageDescription(fds.value, fds.size, d)) return 0;
    if (codecIndex(d.codec) != codecIndex(s->codec) || d.width != s->width || d.height != s->height || !decodableFormat(d)) return 0;
    Atom record;
    if (!configurationAtom(d, record) || record.size > sizeof s->record) return 0;
    if (record.size == s->recordSize && !memcmp(record.body, s->record, record.size)) return 1;
    if (!(rawStream(d.codec) ? streamHeaders(s, d) : parameterSets(s, d))) return 0;
    memcpy(s->record, record.body, record.size);
    s->recordSize = record.size;
    s->parametersSent = false;
    trace("decoder session %u takes a new description", s->v4l2);
    return 1;
}

}  // namespace

extern "C" {

// Whether crosvm gave the guest"#),
    ("    case cc(\"+dec\"): createDecoder(c, h, m, n); break;\n",
     r#"    case cc("+dec"): createDecoder(c, h, m, n); break;
    // OneiOS (kernel gen codec, 2026-11-24): the decoder's capabilities (WebCore asks them) and a new description.
    case cc("dcap"): decoderCapabilities(c, h, m, n); break;
    case cc("dcan"): {
        uint8_t buffer[128];
        Writer w(buffer, sizeof buffer);
        w.header(h.type, FlagReply, h.uuid, h.request);
        w.s32(cc(",err"), 0);
        w.s32(cc(",can"), acceptFormat(s, m, n));
        sendToGuest(c, w, buffer);
        break;
    }
"#),
    // The kept mappings go with the session.
    ("    OSSafeReleaseNULL(s->holder);\n}\n",
     r#"    OSSafeReleaseNULL(s->holder);
    // OneiOS (kernel gen codec, 2026-11-24): the surfaces' kernel mappings (the worker has stopped).
    for (unsigned i = 0; i < s->mappedCount; ++i) {
        OSSafeReleaseNULL(s->mappedSurfaces[i].map);
        OSSafeReleaseNULL(s->mappedSurfaces[i].memory);
    }
    s->mappedCount = 0;
    memset(s->given, 0, sizeof s->given);   // a new session's frame IDs start again
}
"#),
];
