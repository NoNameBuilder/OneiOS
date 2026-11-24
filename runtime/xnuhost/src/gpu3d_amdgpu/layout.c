// SPDX-License-Identifier: MIT
// Host builds only (xnuhost's build.rs): the sizes, offsets and numbers the amdgpu renderer mirrors in Rust
// (gpu3d_amdgpu/uapi.rs), taken from the headers the guest's Mesa is built with: Mesa cecc0e9's include/drm-uapi
// (amdgpu_drm.h, drm.h), src/virtio/virtio-gpu/drm_hw.h, src/amd/common/virtio/amdgpu_virtio_proto.h, and libdrm's
// amdgpu.h (struct amdgpu_gpu_info, amdgpu_heap_info, amdgpu_buffer_size_alignments). tests.rs lists the Rust values
// in this order.
#include <assert.h>
#include <stdalign.h>
#include <stddef.h>
#include <stdint.h>
#define ENABLE_DRM_AMDGPU 1
#include "drm.h"
#include "amdgpu_drm.h"
#include <amdgpu.h>
#include "drm_hw.h"
#include "amdgpu_virtio_proto.h"

#define CAP(f) offsetof(struct virgl_renderer_capset_drm, f)
#define INFO(f) offsetof(struct amdgpu_gpu_info, f)

static const uint64_t layout[] = {
  // the kernel's structures: sizes
  sizeof(struct drm_version), sizeof(struct drm_prime_handle), sizeof(struct drm_gem_close),
  sizeof(struct drm_syncobj_create), sizeof(struct drm_syncobj_destroy), sizeof(struct drm_syncobj_handle),
  sizeof(struct drm_amdgpu_info), sizeof(union drm_amdgpu_ctx), sizeof(union drm_amdgpu_bo_list),
  sizeof(struct drm_amdgpu_bo_list_in), sizeof(struct drm_amdgpu_bo_list_entry), sizeof(struct drm_amdgpu_cs_chunk),
  sizeof(union drm_amdgpu_cs), sizeof(struct drm_amdgpu_cs_chunk_ib), sizeof(struct drm_amdgpu_cs_chunk_fence),
  sizeof(struct drm_amdgpu_cs_chunk_dep), sizeof(struct drm_amdgpu_cs_chunk_sem), sizeof(union drm_amdgpu_wait_cs),
  sizeof(struct drm_amdgpu_gem_metadata), sizeof(union drm_amdgpu_vm), sizeof(struct drm_amdgpu_info_device),
  sizeof(struct drm_amdgpu_info_hw_ip), sizeof(struct drm_amdgpu_info_vram_gtt),
  // offsets
  offsetof(struct drm_version, name_len), offsetof(struct drm_version, desc),
  offsetof(struct drm_syncobj_handle, fd), offsetof(struct drm_amdgpu_info, query),
  offsetof(struct drm_amdgpu_info, query_hw_ip.type), offsetof(struct drm_amdgpu_info, read_mmr_reg.instance),
  offsetof(struct drm_amdgpu_gem_va, operation), offsetof(struct drm_amdgpu_gem_va, va_address),
  offsetof(struct drm_amdgpu_gem_va, offset_in_bo), offsetof(struct drm_amdgpu_gem_va, map_size),
  offsetof(union drm_amdgpu_ctx, in.priority), offsetof(union drm_amdgpu_ctx, out.alloc.ctx_id),
  offsetof(union drm_amdgpu_ctx, out.pstate.flags), offsetof(union drm_amdgpu_bo_list, out.list_handle),
  offsetof(struct drm_amdgpu_bo_list_in, bo_info_ptr), offsetof(union drm_amdgpu_cs, in.chunks),
  offsetof(union drm_amdgpu_cs, out.handle), offsetof(struct drm_amdgpu_cs_chunk_ib, va_start),
  offsetof(struct drm_amdgpu_cs_chunk_ib, ring), offsetof(union drm_amdgpu_wait_cs, in.ctx_id),
  offsetof(union drm_amdgpu_wait_cs, out.status), offsetof(struct drm_amdgpu_gem_metadata, data.flags),
  offsetof(struct drm_amdgpu_gem_metadata, data.data_size_bytes), offsetof(struct drm_amdgpu_gem_metadata, data.data),
  offsetof(struct drm_amdgpu_info_device, family), offsetof(struct drm_amdgpu_info_device, cu_bitmap),
  offsetof(struct drm_amdgpu_info_device, ids_flags), offsetof(struct drm_amdgpu_info_device, virtual_address_offset),
  offsetof(struct drm_amdgpu_info_device, virtual_address_max),
  offsetof(struct drm_amdgpu_info_device, virtual_address_alignment),
  offsetof(struct drm_amdgpu_info_device, pte_fragment_size), offsetof(struct drm_amdgpu_info_device, gart_page_size),
  offsetof(struct drm_amdgpu_info_device, vram_type), offsetof(struct drm_amdgpu_info_device, high_va_offset),
  offsetof(struct drm_amdgpu_info_device, high_va_max), offsetof(struct drm_amdgpu_info_device, tcc_disabled_mask),
  offsetof(struct drm_amdgpu_info_device, userq_ip_mask), offsetof(struct drm_amdgpu_info_hw_ip, available_rings),
  // the requests' numbers
  DRM_IOCTL_VERSION, DRM_IOCTL_GEM_CLOSE, DRM_IOCTL_PRIME_FD_TO_HANDLE, DRM_IOCTL_SYNCOBJ_CREATE,
  DRM_IOCTL_SYNCOBJ_DESTROY, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, DRM_IOCTL_AMDGPU_CTX, DRM_IOCTL_AMDGPU_BO_LIST,
  DRM_IOCTL_AMDGPU_CS, DRM_IOCTL_AMDGPU_INFO, DRM_IOCTL_AMDGPU_GEM_METADATA, _IOC_NR(DRM_IOCTL_AMDGPU_GEM_VA),
  _IOC_DIR(DRM_IOCTL_AMDGPU_GEM_VA), DRM_IOCTL_AMDGPU_WAIT_CS, DRM_IOCTL_AMDGPU_VM,
  // constants
  DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE, AMDGPU_GEM_DOMAIN_GTT, AMDGPU_GEM_DOMAIN_VRAM,
  AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED, AMDGPU_GEM_CREATE_VM_ALWAYS_VALID, AMDGPU_BO_LIST_OP_CREATE,
  AMDGPU_BO_LIST_OP_DESTROY, AMDGPU_CTX_OP_ALLOC_CTX, AMDGPU_CTX_OP_FREE_CTX, AMDGPU_CTX_OP_GET_STABLE_PSTATE,
  AMDGPU_CTX_OP_SET_STABLE_PSTATE, AMDGPU_VM_OP_RESERVE_VMID, AMDGPU_VM_OP_UNRESERVE_VMID,
  AMDGPU_GEM_METADATA_OP_SET_METADATA, AMDGPU_VA_OP_MAP, AMDGPU_VA_OP_UNMAP, AMDGPU_VA_OP_CLEAR, AMDGPU_VA_OP_REPLACE,
  AMDGPU_VM_PAGE_READABLE, AMDGPU_VM_PAGE_EXECUTABLE, AMDGPU_VM_PAGE_PRT, AMDGPU_HW_IP_GFX, AMDGPU_HW_IP_NUM,
  AMDGPU_CHUNK_ID_IB, AMDGPU_CHUNK_ID_FENCE, AMDGPU_CHUNK_ID_DEPENDENCIES, AMDGPU_CHUNK_ID_SYNCOBJ_OUT,
  AMDGPU_CHUNK_ID_BO_HANDLES, AMDGPU_INFO_HW_IP_INFO, AMDGPU_INFO_VRAM_USAGE, AMDGPU_INFO_GTT_USAGE,
  AMDGPU_INFO_VRAM_GTT, AMDGPU_INFO_READ_MMR_REG, AMDGPU_INFO_DEV_INFO, AMDGPU_INFO_VIS_VRAM_USAGE, AMDGPU_FAMILY_AI,
  AMDGPU_FAMILY_CI, AMDGPU_TIMEOUT_INFINITE, AMDGPU_QUERY_FENCE_TIMEOUT_IS_ABSOLUTE,
  // the guest's protocol: the commands, then each request's size and fields
  AMDGPU_CCMD_QUERY_INFO, AMDGPU_CCMD_GEM_NEW, AMDGPU_CCMD_BO_VA_OP, AMDGPU_CCMD_CS_SUBMIT, AMDGPU_CCMD_SET_METADATA,
  AMDGPU_CCMD_BO_QUERY_INFO, AMDGPU_CCMD_CREATE_CTX, AMDGPU_CCMD_RESERVE_VMID, AMDGPU_CCMD_SET_PSTATE,
  AMDGPU_CCMD_CS_QUERY_FENCE_STATUS,
  sizeof(struct amdgpu_ccmd_query_info_req), sizeof(struct amdgpu_ccmd_gem_new_req),
  sizeof(struct amdgpu_ccmd_bo_va_op_req), sizeof(struct amdgpu_ccmd_cs_submit_req),
  sizeof(struct amdgpu_ccmd_set_metadata_req), sizeof(struct amdgpu_ccmd_bo_query_info_req),
  sizeof(struct amdgpu_ccmd_create_ctx_req), sizeof(struct amdgpu_ccmd_reserve_vmid_req),
  sizeof(struct amdgpu_ccmd_set_pstate_req), sizeof(struct amdgpu_ccmd_cs_query_fence_status_req),
  sizeof(struct vdrm_ccmd_req), sizeof(struct amdgpu_ccmd_rsp), offsetof(struct amdgpu_ccmd_rsp, ret),
  offsetof(struct amdgpu_ccmd_query_info_req, info), offsetof(struct amdgpu_ccmd_query_info_rsp, payload),
  offsetof(struct amdgpu_ccmd_gem_new_req, blob_id), offsetof(struct amdgpu_ccmd_gem_new_req, r.alloc_size),
  offsetof(struct amdgpu_ccmd_gem_new_req, r.phys_alignment), offsetof(struct amdgpu_ccmd_gem_new_req, r.preferred_heap),
  offsetof(struct amdgpu_ccmd_gem_new_req, r.__pad), offsetof(struct amdgpu_ccmd_gem_new_req, r.flags),
  offsetof(struct amdgpu_ccmd_bo_va_op_req, va), offsetof(struct amdgpu_ccmd_bo_va_op_req, vm_map_size),
  offsetof(struct amdgpu_ccmd_bo_va_op_req, flags), offsetof(struct amdgpu_ccmd_bo_va_op_req, flags2),
  offsetof(struct amdgpu_ccmd_bo_va_op_req, offset), offsetof(struct amdgpu_ccmd_bo_va_op_req, res_id),
  offsetof(struct amdgpu_ccmd_bo_va_op_req, op), AMDGPU_CCMD_BO_VA_OP_SPARSE_BO,
  offsetof(struct amdgpu_ccmd_cs_submit_req, ctx_id), offsetof(struct amdgpu_ccmd_cs_submit_req, num_chunks),
  offsetof(struct amdgpu_ccmd_cs_submit_req, ring_idx), offsetof(struct amdgpu_ccmd_cs_submit_req, payload),
  AMDGPU_CCMD_CS_SUBMIT_MAX_NUM_CHUNKS,
  offsetof(struct amdgpu_ccmd_set_metadata_req, flags), offsetof(struct amdgpu_ccmd_set_metadata_req, tiling_info),
  offsetof(struct amdgpu_ccmd_set_metadata_req, res_id), offsetof(struct amdgpu_ccmd_set_metadata_req, size_metadata),
  offsetof(struct amdgpu_ccmd_set_metadata_req, umd_metadata),
  offsetof(struct amdgpu_ccmd_bo_query_info_req, res_id), offsetof(struct amdgpu_ccmd_bo_query_info_req, pad),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.alloc_size),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.phys_alignment),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.preferred_heap),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.alloc_flags),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.metadata.flags),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.metadata.tiling_info),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.metadata.size_metadata),
  offsetof(struct amdgpu_ccmd_bo_query_info_rsp, info.metadata.umd_metadata),
  sizeof(struct amdgpu_ccmd_bo_query_info_rsp),
  offsetof(struct amdgpu_ccmd_create_ctx_req, priority), offsetof(struct amdgpu_ccmd_create_ctx_req, flags),
  AMDGPU_CCMD_CREATE_CTX_DESTROY, offsetof(struct amdgpu_ccmd_create_ctx_rsp, ctx_id),
  sizeof(struct amdgpu_ccmd_create_ctx_rsp),
  offsetof(struct amdgpu_ccmd_reserve_vmid_req, flags), AMDGPU_CCMD_RESERVE_VMID_UNRESERVE,
  offsetof(struct amdgpu_ccmd_set_pstate_req, ctx_id), offsetof(struct amdgpu_ccmd_set_pstate_req, op),
  offsetof(struct amdgpu_ccmd_set_pstate_req, flags), offsetof(struct amdgpu_ccmd_set_pstate_req, pad),
  sizeof(struct amdgpu_ccmd_set_pstate_rsp),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, ctx_id),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, ip_type),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, ip_instance),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, ring),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, fence),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, timeout_ns),
  offsetof(struct amdgpu_ccmd_cs_query_fence_status_req, flags), sizeof(struct amdgpu_ccmd_cs_query_fence_status_rsp),
  // the shmem
  offsetof(struct amdvgpu_shmem, base.seqno), offsetof(struct amdvgpu_shmem, base.rsp_mem_offset),
  offsetof(struct amdvgpu_shmem, async_error), offsetof(struct amdvgpu_shmem, gtt), offsetof(struct amdvgpu_shmem, vram),
  offsetof(struct amdvgpu_shmem, vis_vram), sizeof(struct amdvgpu_shmem), sizeof(struct amdgpu_heap_info),
  // the capset
  CAP(version_major), CAP(version_minor), CAP(version_patchlevel), CAP(context_type), CAP(u.amdgpu.address32_hi),
  CAP(u.amdgpu.alignments.size_local), CAP(u.amdgpu.alignments.size_remote), CAP(u.amdgpu.gpu_info),
  CAP(u.amdgpu.marketing_name), sizeof(((struct virgl_renderer_capset_drm *)0)->u.amdgpu.marketing_name),
  sizeof(struct virgl_renderer_capset_drm), VIRTGPU_DRM_CONTEXT_AMDGPU,
  INFO(asic_id), INFO(chip_rev), INFO(chip_external_rev), INFO(family_id), INFO(ids_flags), INFO(max_engine_clk),
  INFO(max_memory_clk), INFO(num_shader_engines), INFO(num_shader_arrays_per_engine), INFO(num_hw_gfx_contexts),
  INFO(rb_pipes), INFO(enabled_rb_pipes_mask), INFO(gpu_counter_freq), INFO(backend_disable), INFO(mc_arb_ramcfg),
  INFO(gb_addr_cfg), INFO(gb_tile_mode), INFO(gb_macro_tile_mode), INFO(pa_sc_raster_cfg), INFO(pa_sc_raster_cfg1),
  INFO(cu_active_number), INFO(cu_ao_mask), INFO(cu_bitmap), INFO(vram_type), INFO(vram_bit_width), INFO(ce_ram_size),
  INFO(vce_harvest_config), INFO(pci_rev_id), sizeof(struct amdgpu_gpu_info),
};

// The bitfield has_vm_always_valid: the word at u.amdgpu + 4 with only it set.
static uint32_t vm_always_valid_word(void)
{
  struct virgl_renderer_capset_drm c = {0};
  c.u.amdgpu.has_vm_always_valid = 1;
  uint32_t w;
  __builtin_memcpy(&w, (const char *)&c + offsetof(struct virgl_renderer_capset_drm, u.amdgpu.address32_hi) + 4, 4);
  return w;
}

size_t oneios_amdgpu_layout(const uint64_t **out, uint32_t *bitfield);
size_t oneios_amdgpu_layout(const uint64_t **out, uint32_t *bitfield)
{
  *out = layout;
  *bitfield = vm_always_valid_word();
  return sizeof layout / sizeof *layout;
}
