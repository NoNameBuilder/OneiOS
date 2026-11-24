/*
 * Host test of the Darwin vdrm backend (Mesa src/virtio/vdrm/vdrm_darwin.c, userland/metal-amd/patches/mesa)
 * together with Mesa's amdgpu virtio context (src/amd/common/virtio), on Linux, against a fake of the guest
 * kernel's user client: the IOKit calls of tests/fake-iokit/IOKit/IOKitLib.h are implemented here. The fake
 * answers as DroidVMVirtioGpu3D.cpp does (the tunnel, GET_CAPS, CONTEXT_INIT, CREATE_BLOB, MAP, EXECBUFFER
 * with ring fences, WAIT_FENCE, GEM_CLOSE) and plays a minimal amdgpu host for the ccmds it receives
 * (QUERY_INFO, CREATE_CTX, BO_VA_OP, CS_SUBMIT, CS_QUERY_FENCE_STATUS), writing their responses into the
 * shmem. Every record the backend sends is kept and checked: method numbers, record sizes, the open type,
 * the capset, the K2 blob rules (shmem HOST3D blob 0; every other BO HOST3D_GUEST + CREATE_GUEST_HANDLE
 * with its GEM_NEW ccmd), fences on the right rings, syncobjs, waits.
 *
 * Built and run by userland/metal-amd/host-test.sh. Prints "ok N - ..." lines; exit status = failures.
 *
 * Copyright 2026 OneiOS contributors
 * SPDX-License-Identifier: MIT
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <IOKit/IOKitLib.h>

#include "amdgpu.h"
#include "drm-uapi/amdgpu_drm.h"
#include "drm-uapi/drm.h"
#include "util/os_time.h"
#include "util/u_sync_provider.h"
#include "virtio/vdrm/vdrm.h"
#include "virtio/vdrm/vdrm_darwin.h"
#include "virtio/vdrm/DroidVMVirtgpuABI.h"
#include "virtio/amdgpu_virtio.h"
#include "virtio/amdgpu_virtio_proto.h"
#include "virtio/amdgpu_virtio_private.h"
#include "virtio/virtio-gpu/drm_hw.h"

/* ---- the test's bookkeeping ---- */

static int failures, checks;

static void
check(bool ok, const char *format, ...)
{
   va_list args;
   va_start(args, format);
   checks++;
   printf("%s %d - ", ok ? "ok" : "not ok", checks);
   vprintf(format, args);
   printf("\n");
   va_end(args);
   if (!ok)
      failures++;
}

/* ---- the fake kernel ---- */

#define FAKE_SELECTOR 5
#define FAKE_RINGS 64
#define FAKE_SHMEM_RSP_OFFSET 256

enum { MAX_RECORDS = 512, MAX_RES = 64 };

struct record {
   uint32_t op;
   uint32_t in_size;
   uint8_t in[512];   /* the first bytes of the record */
};

static struct {
   uint32_t open_type;
   uint32_t context_type;   /* what GET_CAPS answers */
   bool opened, closed;
   uint32_t probes;
   struct record records[MAX_RECORDS];
   unsigned num_records;
   struct {
      bool used;
      uint32_t res_id;
      uint64_t size;
      void *memory;
      unsigned maps;
   } res[MAX_RES];
   uint32_t next_handle, next_res;
   uint8_t *shmem;
   uint64_t fences[FAKE_RINGS];
   uint64_t waits_ring[32], waits_fence[32];
   unsigned num_waits;
   /* what the fake host saw in ccmds */
   unsigned gem_new, cs_submit, query_info;
   uint64_t last_gem_new_blob_id, last_gem_new_size;
   uint32_t last_gem_new_heap;
   uint32_t last_cs_ring;
} k = {.context_type = VIRTGPU_DRM_CONTEXT_AMDGPU, .next_handle = 1, .next_res = 100};

const mach_port_t kIOMainPortDefault = 0;
mach_port_t mach_task_self(void) { return 1; }
CFMutableDictionaryRef IOServiceMatching(const char *name) { return (CFMutableDictionaryRef)name; }

io_service_t
IOServiceGetMatchingService(mach_port_t main_port, CFDictionaryRef matching)
{
   (void)main_port;
   return strcmp((const char *)matching, "IOAcceleratorES") == 0 ? 77 : 0;
}

kern_return_t
IOServiceOpen(io_service_t service, task_port_t task, uint32_t type, io_connect_t *connect)
{
   (void)task;
   k.open_type = type;
   if (service != 77 || type != DROIDVM_VIRTGPU_OPEN_TYPE)
      return kIOReturnBadArgument;
   k.opened = true;
   k.closed = false;
   *connect = 99;
   return KERN_SUCCESS;
}

kern_return_t IOServiceClose(io_connect_t connect) { (void)connect; k.closed = true; return KERN_SUCCESS; }
kern_return_t IOObjectRelease(io_object_t object) { (void)object; return KERN_SUCCESS; }

static int
fake_handle(uint32_t handle)
{
   for (int i = 0; i < MAX_RES; i++)
      if (k.res[i].used && i + 1 == (int)handle)
         return i;
   return -1;
}

/* The minimal amdgpu host: one ccmd. */
static void
host_ccmd(const struct vdrm_ccmd_req *req)
{
   struct vdrm_shmem *shmem = (struct vdrm_shmem *)k.shmem;
   uint8_t *rsp = k.shmem + FAKE_SHMEM_RSP_OFFSET + req->rsp_off;
   struct amdgpu_ccmd_rsp *r = (struct amdgpu_ccmd_rsp *)rsp;
   switch (req->cmd) {
   case AMDGPU_CCMD_QUERY_INFO: {
      const struct amdgpu_ccmd_query_info_req *q = (const void *)req;
      struct amdgpu_ccmd_query_info_rsp *out = (void *)rsp;
      k.query_info++;
      memset(out->payload, 0, q->info.return_size);
      if (q->info.query == AMDGPU_INFO_DEV_INFO) {
         struct drm_amdgpu_info_device *d = (void *)out->payload;
         d->family = AMDGPU_FAMILY_GC_11_0_0;
         d->device_id = 0x7480;
         d->virtual_address_offset = 0x200000;
         d->virtual_address_max = 0x800000000000ull;
         d->virtual_address_alignment = 4096;
         d->high_va_offset = 0xffff800000000000ull;
         d->high_va_max = 0xffffffffffffffffull;
      } else if (q->info.query == AMDGPU_INFO_HW_IP_INFO) {
         struct drm_amdgpu_info_hw_ip *ip = (void *)out->payload;
         if (q->info.query_hw_ip.type == AMDGPU_HW_IP_GFX || q->info.query_hw_ip.type == AMDGPU_HW_IP_COMPUTE) {
            ip->available_rings = 1;
            ip->hw_ip_version_major = 11;
         }
      }
      r->ret = 0;
      break;
   }
   case AMDGPU_CCMD_GEM_NEW: {
      const struct amdgpu_ccmd_gem_new_req *g = (const void *)req;
      k.gem_new++;
      k.last_gem_new_blob_id = g->blob_id;
      k.last_gem_new_size = g->r.alloc_size;
      k.last_gem_new_heap = g->r.preferred_heap;
      break;
   }
   case AMDGPU_CCMD_CREATE_CTX: {
      struct amdgpu_ccmd_create_ctx_rsp *c = (void *)rsp;
      c->hdr.ret = 0;
      c->ctx_id = 7;
      break;
   }
   case AMDGPU_CCMD_CS_SUBMIT: {
      const struct amdgpu_ccmd_cs_submit_req *s = (const void *)req;
      k.cs_submit++;
      k.last_cs_ring = s->ring_idx;
      break;
   }
   case AMDGPU_CCMD_CS_QUERY_FENCE_STATUS: {
      struct amdgpu_ccmd_cs_query_fence_status_rsp *f = (void *)rsp;
      f->hdr.ret = 0;
      f->expired = 1;
      break;
   }
   default:
      r->ret = 0;
      break;
   }
   shmem->seqno = req->seqno;
}

static void
host_commands(const uint8_t *commands, uint32_t size)
{
   for (uint32_t off = 0; off + sizeof(struct vdrm_ccmd_req) <= size;) {
      const struct vdrm_ccmd_req *req = (const void *)(commands + off);
      if (!req->len)
         break;
      host_ccmd(req);
      off += req->len;
   }
}

kern_return_t
IOConnectCallStructMethod(mach_port_t connection, uint32_t selector, const void *input, size_t input_size, void *output,
                          size_t *output_size)
{
   (void)connection;
   const uint32_t *header = input;
   if (input_size < 8 || header[0] != 0x4e544e4cu)
      return kIOReturnBadArgument;
   if (header[1] == 0xffffffffu) {
      k.probes++;
      return selector == FAKE_SELECTOR ? KERN_SUCCESS : kIOReturnUnsupported;
   }
   if (selector != FAKE_SELECTOR || header[1] < DROIDVM_VIRTGPU_METHOD || header[1] >= DROIDVM_VIRTGPU_METHOD + 16)
      return kIOReturnBadArgument;
   const uint32_t op = header[1] - DROIDVM_VIRTGPU_METHOD;
   const uint8_t *in = (const uint8_t *)input + 8;
   const size_t in_size = input_size - 8;
   const size_t out_size = output_size ? *output_size : 0;
   if (k.num_records < MAX_RECORDS) {
      struct record *r = &k.records[k.num_records++];
      r->op = op;
      r->in_size = (uint32_t)in_size;
      memcpy(r->in, in, in_size < sizeof(r->in) ? in_size : sizeof(r->in));
   }

   switch (op) {
   case DROIDVM_VIRTGPU_GETPARAM: {
      const struct droidvm_virtgpu_getparam *p = (const void *)in;
      struct droidvm_virtgpu_getparam_reply *o = output;
      if (in_size != sizeof(*p) || out_size != sizeof(*o))
         return kIOReturnBadArgument;
      o->value = p->param == DROIDVM_VIRTGPU_PARAM_ABI_VERSION ? 1 :
                 p->param == DROIDVM_VIRTGPU_PARAM_CREATE_GUEST_HANDLE ? 1 : 0;
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_GET_CAPS: {
      const struct droidvm_virtgpu_get_caps *c = (const void *)in;
      if (in_size != sizeof(*c) || c->capset_id != 6 || !out_size)
         return kIOReturnBadArgument;
      struct virgl_renderer_capset_drm caps = {0};
      caps.wire_format_version = 1;
      caps.version_major = 3;
      caps.version_minor = 61;
      caps.context_type = k.context_type;
      caps.u.amdgpu.address32_hi = 0xffff8000;
      snprintf(caps.u.amdgpu.marketing_name, sizeof(caps.u.amdgpu.marketing_name), "Xclipse 540 (fake host)");
      memcpy(output, &caps, out_size < sizeof(caps) ? out_size : sizeof(caps));
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_CONTEXT_INIT: {
      struct droidvm_virtgpu_context_init_reply *o = output;
      if (in_size != sizeof(struct droidvm_virtgpu_context_init) || out_size != sizeof(*o))
         return kIOReturnBadArgument;
      o->context_id = 16;
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_CREATE_BLOB:
   case DROIDVM_VIRTGPU_CREATE_BLOB_USER: {
      uint64_t size;
      uint32_t cmd_size;
      const uint8_t *ccmd;
      if (op == DROIDVM_VIRTGPU_CREATE_BLOB) {
         const struct droidvm_virtgpu_create_blob *b = (const void *)in;
         if (in_size < sizeof(*b) || in_size != sizeof(*b) + b->cmd_size)
            return kIOReturnBadArgument;
         size = b->size;
         cmd_size = b->cmd_size;
         ccmd = in + sizeof(*b);
         if (b->blob_mem != DROIDVM_VIRTGPU_BLOB_MEM_HOST3D && b->blob_mem != DROIDVM_VIRTGPU_BLOB_MEM_HOST3D_GUEST)
            return kIOReturnBadArgument;
      } else {
         const struct droidvm_virtgpu_create_blob_user *b = (const void *)in;
         if (in_size < sizeof(*b) || in_size != sizeof(*b) + b->cmd_size)
            return kIOReturnBadArgument;
         size = b->size;
         cmd_size = b->cmd_size;
         ccmd = in + sizeof(*b);
      }
      if (out_size != sizeof(struct droidvm_virtgpu_create_blob_reply) || (cmd_size & 3))
         return kIOReturnBadArgument;
      if (cmd_size)
         host_commands(ccmd, cmd_size);
      int i = (int)k.next_handle - 1;
      if (i >= MAX_RES)
         return kIOReturnNoResources;
      k.res[i].used = true;
      k.res[i].res_id = k.next_res++;
      k.res[i].size = size;
      k.res[i].maps = 0;
      k.res[i].memory = op == DROIDVM_VIRTGPU_CREATE_BLOB ? calloc(1, size) : NULL;
      if (!k.shmem && op == DROIDVM_VIRTGPU_CREATE_BLOB) {
         /* The first blob is the shmem: the host sets where its responses go. */
         k.shmem = k.res[i].memory;
         ((struct vdrm_shmem *)k.shmem)->rsp_mem_offset = FAKE_SHMEM_RSP_OFFSET;
      }
      struct droidvm_virtgpu_create_blob_reply *o = output;
      o->handle = k.next_handle++;
      o->resource_id = k.res[i].res_id;
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_MAP: {
      const struct droidvm_virtgpu_handle *h = (const void *)in;
      struct droidvm_virtgpu_map_reply *o = output;
      int i = in_size == sizeof(*h) ? fake_handle(h->handle) : -1;
      if (i < 0 || out_size != sizeof(*o) || !k.res[i].memory)
         return kIOReturnBadArgument;
      k.res[i].maps++;
      o->address = (uintptr_t)k.res[i].memory;
      o->size = k.res[i].size;
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_EXECBUFFER: {
      const struct droidvm_virtgpu_execbuffer *e = (const void *)in;
      struct droidvm_virtgpu_execbuffer_reply *o = output;
      if (in_size < sizeof(*e) || in_size != sizeof(*e) + e->size || out_size != sizeof(*o) || !e->size ||
          (e->size & 3) || e->size > DROIDVM_VIRTGPU_MAX_COMMAND)
         return kIOReturnBadArgument;
      host_commands(in + sizeof(*e), e->size);
      o->fence = 0;
      if (e->flags & DROIDVM_VIRTGPU_EXEC_FENCE) {
         if (e->ring_idx >= FAKE_RINGS)
            return kIOReturnBadArgument;
         o->fence = ++k.fences[e->ring_idx];
      }
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_WAIT_FENCE: {
      const struct droidvm_virtgpu_wait_fence *w = (const void *)in;
      if (in_size != sizeof(*w) || out_size)
         return kIOReturnBadArgument;
      if (k.num_waits < 32) {
         k.waits_ring[k.num_waits] = w->ring_idx;
         k.waits_fence[k.num_waits++] = w->fence;
      }
      /* The fake host retires everything at once; a fence never issued is done, as in the kernel. */
      return KERN_SUCCESS;
   }
   case DROIDVM_VIRTGPU_GEM_CLOSE: {
      const struct droidvm_virtgpu_handle *h = (const void *)in;
      int i = in_size == sizeof(*h) ? fake_handle(h->handle) : -1;
      if (i < 0 || out_size)
         return kIOReturnBadArgument;
      if (k.res[i].memory != k.shmem)
         free(k.res[i].memory);
      k.res[i].used = false;
      return KERN_SUCCESS;
   }
   default:
      return kIOReturnUnsupported;
   }
}

/* ---- helpers over the records ---- */

static unsigned
count_op(uint32_t op, unsigned from)
{
   unsigned n = 0;
   for (unsigned i = from; i < k.num_records; i++)
      n += k.records[i].op == op;
   return n;
}

static const struct record *
last_op(uint32_t op)
{
   for (unsigned i = k.num_records; i-- > 0;)
      if (k.records[i].op == op)
         return &k.records[i];
   return NULL;
}

static const struct record *
nth_op(uint32_t op, unsigned n)
{
   for (unsigned i = 0; i < k.num_records; i++)
      if (k.records[i].op == op && n-- == 0)
         return &k.records[i];
   return NULL;
}

/* ---- the tests ---- */

static void
test_wrong_context(void)
{
   k.context_type = VIRTGPU_DRM_CONTEXT_MSM;
   int fd = open("/dev/null", O_RDWR);
   struct vdrm_device *vdev = vdrm_device_connect(fd, VIRTGPU_DRM_CONTEXT_AMDGPU);
   check(!vdev, "an msm capset gives no amdgpu device");
   check(strstr(vdrm_darwin_last_error(), "context type 1") != NULL, "and says why: \"%s\"", vdrm_darwin_last_error());
   check(k.closed, "the connection is closed again");
   check(count_op(DROIDVM_VIRTGPU_CONTEXT_INIT, 0) == 0, "no CONTEXT_INIT for a capset of another driver");
   close(fd);
   k.context_type = VIRTGPU_DRM_CONTEXT_AMDGPU;
   k.num_records = 0;
   k.probes = 0;
}

int
main(void)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   test_wrong_context();

   /* The device: open, tunnel, capset, context, shmem; then the amdgpu queries. */
   int fd = open("/dev/null", O_RDWR);
   uint32_t major = 0, minor = 0;
   amdvgpu_device_handle dev = NULL;
   struct util_sync_provider *sync = NULL;
   int r = amdvgpu_device_initialize(fd, &major, &minor, &dev, &sync);
   check(r == 0 && dev && sync, "amdvgpu_device_initialize over the Darwin backend (%d)", r);
   if (r)
      return 1;
   check(k.open_type == 0x4149, "IOServiceOpen of IOAcceleratorES with open type 0x4149 (0x%x)", k.open_type);
   check(k.probes == FAKE_SELECTOR + 1, "the tunnel's selector probe stops at the selector that answers (%u probes)",
         k.probes);
   check(major == 3 && minor == 61, "drm version from the capset (%u.%u)", major, minor);
   const struct record *caps = nth_op(DROIDVM_VIRTGPU_GET_CAPS, 0);
   check(caps && caps->in_size == sizeof(struct droidvm_virtgpu_get_caps) &&
         ((const struct droidvm_virtgpu_get_caps *)caps->in)->capset_id == 6, "GET_CAPS of capset 6");
   const struct record *init = nth_op(DROIDVM_VIRTGPU_CONTEXT_INIT, 0);
   check(init && ((const struct droidvm_virtgpu_context_init *)init->in)->capset_id == 6 &&
         ((const struct droidvm_virtgpu_context_init *)init->in)->num_rings == 64, "CONTEXT_INIT capset 6, 64 rings");
   const struct record *shmem = nth_op(DROIDVM_VIRTGPU_CREATE_BLOB, 0);
   const struct droidvm_virtgpu_create_blob *sb = shmem ? (const void *)shmem->in : NULL;
   check(sb && sb->blob_mem == DROIDVM_VIRTGPU_BLOB_MEM_HOST3D && sb->blob_flags == DROIDVM_VIRTGPU_BLOB_FLAG_MAPPABLE &&
         sb->size == 0x4000 && sb->blob_id == 0 && sb->cmd_size == 0 && shmem->in_size == sizeof(*sb),
         "K2: the shmem is a HOST3D blob 0, mappable, 16 KiB, no ccmd");
   check(k.res[0].maps == 1, "the shmem is MAPped once");
   check(k.query_info >= 2, "the amdgpu queries reached the host as QUERY_INFO ccmds (%u)", k.query_info);
   const struct record *first_exec = nth_op(DROIDVM_VIRTGPU_EXECBUFFER, 0);
   const struct droidvm_virtgpu_execbuffer *fe = first_exec ? (const void *)first_exec->in : NULL;
   check(fe && fe->ring_idx == 0 && (fe->flags & DROIDVM_VIRTGPU_EXEC_FENCE),
         "a sync ccmd is a fenced EXECBUFFER on ring 0");
   check(k.num_waits >= 1 && k.waits_ring[0] == 0 && k.waits_fence[0] == 1, "and its WAIT_FENCE is ring 0 fence 1");
   check(!strcmp(amdvgpu_get_marketing_name(dev), "Xclipse 540 (fake host)"), "the marketing name comes from the capset");

   /* K2: a BO is a guest blob, its GEM_NEW in the CREATE_BLOB. */
   unsigned before = k.num_records;
   struct amdgpu_bo_alloc_request request = {
      .alloc_size = 65536, .phys_alignment = 4096, .preferred_heap = AMDGPU_GEM_DOMAIN_GTT,
   };
   amdvgpu_bo_handle bo = NULL;
   r = amdvgpu_bo_alloc(dev, &request, &bo);
   check(r == 0 && bo, "amdvgpu_bo_alloc of 64 KiB GTT (%d)", r);
   const struct record *blob = last_op(DROIDVM_VIRTGPU_CREATE_BLOB);
   const struct droidvm_virtgpu_create_blob *b = blob ? (const void *)blob->in : NULL;
   const struct amdgpu_ccmd_gem_new_req *gem = b ? (const void *)(blob->in + sizeof(*b)) : NULL;
   check(b && b->blob_mem == DROIDVM_VIRTGPU_BLOB_MEM_HOST3D_GUEST, "K2: blob_mem HOST3D_GUEST (%u)", b ? b->blob_mem : 0);
   check(b && (b->blob_flags & DROIDVM_VIRTGPU_BLOB_FLAG_CREATE_GUEST_HANDLE) &&
         (b->blob_flags & DROIDVM_VIRTGPU_BLOB_FLAG_MAPPABLE), "K2: flags CREATE_GUEST_HANDLE | MAPPABLE (0x%x)",
         b ? b->blob_flags : 0);
   check(b && b->size == 65536 && b->blob_id != 0, "K2: size 65536, blob_id %" PRIu64 " != 0", b ? b->blob_id : 0);
   check(gem && b->cmd_size == sizeof(*gem) && gem->hdr.cmd == AMDGPU_CCMD_GEM_NEW && gem->blob_id == b->blob_id &&
         gem->r.alloc_size == 65536 && gem->r.preferred_heap == AMDGPU_GEM_DOMAIN_GTT,
         "K2: the CREATE_BLOB carries the GEM_NEW ccmd (same blob_id, alloc_size, heap)");
   check(k.gem_new == 1 && k.last_gem_new_blob_id == b->blob_id, "the host saw the GEM_NEW before the blob");
   check(count_op(DROIDVM_VIRTGPU_RESOURCE_INFO, before) == 0, "no RESOURCE_INFO: CREATE_BLOB's reply names the resource");

   /* A VRAM BO with no CPU access: still guest pages, not mappable. */
   struct amdgpu_bo_alloc_request vram = {
      .alloc_size = 8192, .phys_alignment = 4096, .preferred_heap = AMDGPU_GEM_DOMAIN_VRAM,
      .flags = AMDGPU_GEM_CREATE_NO_CPU_ACCESS,
   };
   amdvgpu_bo_handle bo2 = NULL;
   r = amdvgpu_bo_alloc(dev, &vram, &bo2);
   b = (const void *)last_op(DROIDVM_VIRTGPU_CREATE_BLOB)->in;
   check(r == 0 && b->blob_mem == DROIDVM_VIRTGPU_BLOB_MEM_HOST3D_GUEST &&
         b->blob_flags == DROIDVM_VIRTGPU_BLOB_FLAG_CREATE_GUEST_HANDLE,
         "K2: a NO_CPU_ACCESS VRAM BO is HOST3D_GUEST, CREATE_GUEST_HANDLE only (0x%x)", b->blob_flags);
   check(k.last_gem_new_heap == AMDGPU_GEM_DOMAIN_VRAM, "and its GEM_NEW keeps the VRAM heap for the host to place");

   /* The CPU view: one MAP, kept across unmap and map again. */
   void *cpu = NULL, *again = NULL;
   r = amdvgpu_bo_cpu_map(dev, bo, &cpu);
   amdvgpu_bo_cpu_unmap(dev, bo);
   r |= amdvgpu_bo_cpu_map(dev, bo, &again);
   check(r == 0 && cpu && cpu == again, "map, unmap, map gives the same CPU view (%p %p)", cpu, again);
   check(k.res[1].maps == 1, "with one MAP (the kernel's mapping lives until GEM_CLOSE)");
   amdvgpu_bo_cpu_unmap(dev, bo);

   uint32_t res_id = 0;
   amdvgpu_bo_export(dev, bo, amdgpu_bo_handle_type_kms, &res_id);
   check(res_id == k.res[1].res_id, "the BO's resource id is the CREATE_BLOB reply's (%u)", res_id);

   /* A GFX submission with an out-syncobj, then a compute one that depends on it. */
   uint32_t ctx = 0;
   r = amdvgpu_cs_ctx_create2(dev, AMDGPU_CTX_PRIORITY_NORMAL, &ctx);
   check(r == 0 && ctx == 7, "CREATE_CTX answered by the host (ctx %u)", ctx);
   uint32_t s1 = 0, s2 = 0, s3 = 0;
   r = sync->create(sync, 0, &s1) | sync->create(sync, 0, &s2) | sync->create(sync, DRM_SYNCOBJ_CREATE_SIGNALED, &s3);
   check(r == 0 && s1 && s2 && s3 && s1 != s2, "three syncobjs (%u %u %u)", s1, s2, s3);

   struct drm_amdgpu_bo_list_entry entry = {.bo_handle = res_id};
   struct drm_amdgpu_bo_list_in list = {
      .operation = ~0u, .list_handle = ~0u, .bo_number = 1, .bo_info_size = sizeof(entry), .bo_info_ptr = (uintptr_t)&entry,
   };
   struct drm_amdgpu_cs_chunk_ib ib = {.ip_type = AMDGPU_HW_IP_GFX, .va_start = 0x400000, .ib_bytes = 32};
   struct drm_amdgpu_cs_chunk_sem out1 = {.handle = s1};
   struct drm_amdgpu_cs_chunk chunks[3] = {
      {.chunk_id = AMDGPU_CHUNK_ID_BO_HANDLES, .length_dw = sizeof(list) / 4, .chunk_data = (uintptr_t)&list},
      {.chunk_id = AMDGPU_CHUNK_ID_IB, .length_dw = sizeof(ib) / 4, .chunk_data = (uintptr_t)&ib},
      {.chunk_id = AMDGPU_CHUNK_ID_SYNCOBJ_OUT, .length_dw = sizeof(out1) / 4, .chunk_data = (uintptr_t)&out1},
   };
   uint64_t seqno = 0;
   const unsigned waits_before = k.num_waits;
   r = amdvgpu_cs_submit_raw2(dev, ctx, 0, 3, chunks, &seqno);
   const struct droidvm_virtgpu_execbuffer *e = (const void *)last_op(DROIDVM_VIRTGPU_EXECBUFFER)->in;
   check(r == 0 && k.cs_submit == 1, "a GFX CS_SUBMIT reached the host (%d)", r);
   check(e->ring_idx == 1 && (e->flags & DROIDVM_VIRTGPU_EXEC_FENCE) && k.last_cs_ring == 1,
         "on the GFX ring (virtio ring 1), fenced (ring %u, flags 0x%x, ccmd ring %u)", e->ring_idx, e->flags,
         k.last_cs_ring);
   uint64_t point = 9;
   sync->query(sync, &s1, &point, 1, 0);
   check(point == 0, "the out-syncobj is not signalled before a wait has seen the fence retire");

   ib.ip_type = AMDGPU_HW_IP_COMPUTE;
   struct drm_amdgpu_cs_chunk_sem in1 = {.handle = s1}, out2 = {.handle = s2};
   struct drm_amdgpu_cs_chunk chunks2[4] = {
      chunks[0], chunks[1],
      {.chunk_id = AMDGPU_CHUNK_ID_SYNCOBJ_IN, .length_dw = sizeof(in1) / 4, .chunk_data = (uintptr_t)&in1},
      {.chunk_id = AMDGPU_CHUNK_ID_SYNCOBJ_OUT, .length_dw = sizeof(out2) / 4, .chunk_data = (uintptr_t)&out2},
   };
   r = amdvgpu_cs_submit_raw2(dev, ctx, 0, 4, chunks2, &seqno);
   e = (const void *)last_op(DROIDVM_VIRTGPU_EXECBUFFER)->in;
   check(r == 0 && e->ring_idx == 2 && k.last_cs_ring == 2, "a compute CS_SUBMIT on virtio ring 2 (%u)", e->ring_idx);
   check(k.num_waits == waits_before + 1 && k.waits_ring[waits_before] == 1 && k.waits_fence[waits_before] == 1,
         "its in-syncobj on the GFX ring was waited for first (WAIT_FENCE ring 1 fence 1)");

   /* Waits. */
   const unsigned w0 = k.num_waits;
   r = sync->wait(sync, &s2, 1, os_time_get_absolute_timeout(1000000000ull), DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, NULL);
   check(r == 0 && k.num_waits == w0 + 1 && k.waits_ring[w0] == 2 && k.waits_fence[w0] == 1,
         "a syncobj wait is a WAIT_FENCE of its ring and fence (ring 2 fence 1)");
   r = sync->wait(sync, &s2, 1, os_time_get_absolute_timeout(1000000000ull), DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, NULL);
   check(r == 0 && k.num_waits == w0 + 1, "a second wait of a retired fence asks the kernel nothing");
   sync->query(sync, &s2, &point, 1, 0);
   check(point == 1, "and the syncobj reads signalled");
   r = sync->wait(sync, &s3, 1, 0, 0, NULL);
   check(r == 0 && k.num_waits == w0 + 1, "a syncobj created signalled is signalled");
   uint32_t s4 = 0;
   sync->create(sync, 0, &s4);
   r = sync->wait(sync, &s4, 1, 0, 0, NULL);
   check(r == -EINVAL, "a syncobj with no fence, waited without WAIT_FOR_SUBMIT, is -EINVAL as in DRM (%d)", r);
   r = sync->wait(sync, &s4, 1, os_time_get_absolute_timeout(1000000ull), DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, NULL);
   check(r == -ETIME, "with WAIT_FOR_SUBMIT it times out (-ETIME) when nothing submits it (%d)", r);
   uint32_t pair[2] = {s4, s2}, first = 99;
   r = sync->wait(sync, pair, 2, os_time_get_absolute_timeout(1000000ull), DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, &first);
   check(r == 0 && first == 1, "WAIT_ANY returns the signalled one (first_signaled %u)", first);
   check(sync->timeline_wait == NULL, "no timeline syncobjs (has_timeline_syncobj stays false)");
   int fdout = 0;
   check(sync->export_sync_file(sync, s2, &fdout) == -ENOTSUP, "no sync files on Darwin");

   uint32_t expired = 0;
   r = amdvgpu_cs_query_fence_status(dev, ctx, AMDGPU_HW_IP_GFX, 0, 0, 1, 0, 0, &expired);
   check(r == 0 && expired == 1, "CS_QUERY_FENCE_STATUS answered by the host");

   /* bo_wait waits for everything submitted, on every ring. */
   const unsigned w1 = k.num_waits;
   r = amdvgpu_bo_wait_for_idle(dev, bo, 0);
   check(r == 0, "bo wait for idle (%d) asks the kernel %u WAIT_FENCEs (rings with work not yet seen retired)",
         r, k.num_waits - w1);

   /* CREATE_BLOB_USER: the caller's own pages. */
   struct amdgpu_ccmd_gem_new_req user_gem = {
      .hdr = AMDGPU_CCMD(GEM_NEW, sizeof(user_gem)),
      .blob_id = 4242,
   };
   user_gem.r.alloc_size = 16384;
   void *pages = aligned_alloc(4096, 16384);
   uint32_t uh = vdrm_darwin_bo_create_user(dev->vdev, pages, 16384, DROIDVM_VIRTGPU_BLOB_FLAG_MAPPABLE,
                                            4242, &user_gem.hdr);
   const struct record *ur = last_op(DROIDVM_VIRTGPU_CREATE_BLOB_USER);
   const struct droidvm_virtgpu_create_blob_user *ub = ur ? (const void *)ur->in : NULL;
   check(uh && ub && ub->address == (uintptr_t)pages && ub->size == 16384 && ub->blob_id == 4242 &&
         (ub->blob_flags & DROIDVM_VIRTGPU_BLOB_FLAG_CREATE_GUEST_HANDLE) && ub->cmd_size == sizeof(user_gem),
         "CREATE_BLOB_USER over the caller's pages, with its GEM_NEW");
   check(vdrm_darwin_bo_create_user(dev->vdev, (uint8_t *)pages + 1, 16384, 0, 1, NULL) == 0,
         "CREATE_BLOB_USER refuses an unaligned address");
   vdrm_bo_close(dev->vdev, uh);
   free(pages);

   /* Close. */
   const unsigned closes = count_op(DROIDVM_VIRTGPU_GEM_CLOSE, 0);
   amdvgpu_bo_free(dev, bo2);
   amdvgpu_bo_free(dev, bo);
   check(count_op(DROIDVM_VIRTGPU_GEM_CLOSE, 0) == closes + 2, "each BO free is a GEM_CLOSE");
   sync->destroy(sync, s1);
   sync->destroy(sync, s2);
   sync->destroy(sync, s3);
   sync->destroy(sync, s4);
   amdvgpu_cs_ctx_free(dev, ctx);
   sync->finalize(sync);
   amdvgpu_device_deinitialize(dev);
   check(k.closed, "device deinit closes the connection");
   check(!k.res[0].used, "and GEM_CLOSEs the shmem");
   close(fd);

   printf("%s: %d of %d checks failed\n", failures ? "FAIL" : "PASS", failures, checks);
   return failures;
}
