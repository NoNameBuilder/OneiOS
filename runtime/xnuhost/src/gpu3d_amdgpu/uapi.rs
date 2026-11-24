// SPDX-License-Identifier: MIT
//! What the amdgpu renderer says to the kernel and hears from the guest, mirrored by hand:
//! - the kernel's: drm.h (VERSION, GEM_CLOSE, PRIME_FD_TO_HANDLE, the syncobj ioctls) and amdgpu_drm.h (INFO, GEM_VA,
//!   CTX, BO_LIST, CS and its chunks, WAIT_CS, GEM_METADATA, VM), as Mesa cecc0e9 has them in include/drm-uapi
//!   (MIT, Linux's UAPI). GEM_VA is the 40-byte struct of every amdgpu kernel: a newer kernel's longer one reads its
//!   extra timeline fields as 0 (drm_ioctl zero-fills what the caller's size leaves out);
//! - the guest's: Mesa cecc0e9's src/amd/common/virtio/amdgpu_virtio_proto.h (the AMDGPU_CCMD_* requests and their
//!   responses, the shmem) and src/virtio/virtio-gpu/drm_hw.h (vdrm_ccmd_req, the DRM capset with its amdgpu part),
//!   read as offsets into the request's bytes; the capset's gpu_info is libdrm's struct amdgpu_gpu_info.
//!
//! The layout test (tests.rs `the_layout_is_the_headers`) compiles layout.c against those headers and checks every
//! size, offset and number here.
use std::mem::size_of;
use xh_core::ioctl::{iow, iowr};

// ---- drm.h ------------------------------------------------------------------------------------------------------------
#[repr(C)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: usize,
    pub name: *mut u8,
    pub date_len: usize,
    pub date: *mut u8,
    pub desc_len: usize,
    pub desc: *mut u8,
}

#[repr(C)]
#[derive(Default)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

/// drm_syncobj_create and drm_syncobj_destroy (the same two words).
#[repr(C)]
#[derive(Default)]
pub struct DrmSyncobj {
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
}

pub const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1;

pub const DRM_IOCTL_VERSION: u32 = iowr(b'd', 0x00, size_of::<DrmVersion>());
pub const DRM_IOCTL_GEM_CLOSE: u32 = iow(b'd', 0x09, size_of::<DrmGemClose>());
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = iowr(b'd', 0x2e, size_of::<DrmPrimeHandle>());
pub const DRM_IOCTL_SYNCOBJ_CREATE: u32 = iowr(b'd', 0xbf, size_of::<DrmSyncobj>());
pub const DRM_IOCTL_SYNCOBJ_DESTROY: u32 = iowr(b'd', 0xc0, size_of::<DrmSyncobj>());
pub const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u32 = iowr(b'd', 0xc1, size_of::<DrmSyncobjHandle>());

// ---- amdgpu_drm.h -----------------------------------------------------------------------------------------------------
const BASE: u8 = 0x40; // DRM_COMMAND_BASE

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct AmdgpuInfo {
    pub return_pointer: u64,
    pub return_size: u32,
    pub query: u32,
    /// the union: query_hw_ip {type, ip_instance}, read_mmr_reg {dword_offset, count, instance, flags}, ...
    pub arg: [u32; 4],
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GemVa {
    pub handle: u32,
    pub pad: u32,
    pub operation: u32,
    pub flags: u32,
    pub va_address: u64,
    pub offset_in_bo: u64,
    pub map_size: u64,
}

/// union drm_amdgpu_ctx: in {op, flags, ctx_id, priority}; out alloc {ctx_id}, pstate {flags} (word 0).
#[repr(C)]
#[derive(Default)]
pub struct Ctx {
    pub op: u32,
    pub flags: u32,
    pub ctx_id: u32,
    pub priority: i32,
}

/// union drm_amdgpu_bo_list: in (drm_amdgpu_bo_list_in), out {list_handle} (word 0).
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct BoListIn {
    pub operation: u32,
    pub list_handle: u32,
    pub bo_number: u32,
    pub bo_info_size: u32,
    pub bo_info_ptr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoListEntry {
    pub bo_handle: u32,
    pub bo_priority: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct CsChunk {
    pub chunk_id: u32,
    pub length_dw: u32,
    pub chunk_data: u64,
}

/// union drm_amdgpu_cs: in {ctx_id, bo_list_handle, num_chunks, flags, chunks}; out {handle} (the first 8 bytes).
#[repr(C)]
#[derive(Default)]
pub struct Cs {
    pub ctx_id: u32,
    pub bo_list_handle: u32,
    pub num_chunks: u32,
    pub flags: u32,
    pub chunks: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsChunkIb {
    pub pad: u32,
    pub flags: u32,
    pub va_start: u64,
    pub ib_bytes: u32,
    pub ip_type: u32,
    pub ip_instance: u32,
    pub ring: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsChunkFence {
    pub handle: u32,
    pub offset: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct CsChunkSem {
    pub handle: u32,
}

/// union drm_amdgpu_wait_cs: in {handle, timeout, ip_type, ip_instance, ring, ctx_id}; out {status} (the first 8).
#[repr(C)]
#[derive(Default)]
pub struct WaitCs {
    pub handle: u64,
    pub timeout: u64,
    pub ip_type: u32,
    pub ip_instance: u32,
    pub ring: u32,
    pub ctx_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GemMetadata {
    pub handle: u32,
    pub op: u32,
    pub flags: u64,
    pub tiling_info: u64,
    pub data_size_bytes: u32,
    pub data: [u32; 64],
}

impl Default for GemMetadata {
    fn default() -> Self {
        GemMetadata { handle: 0, op: 0, flags: 0, tiling_info: 0, data_size_bytes: 0, data: [0; 64] }
    }
}

/// union drm_amdgpu_vm: in {op, flags}.
#[repr(C)]
#[derive(Default)]
pub struct Vm {
    pub op: u32,
    pub flags: u32,
}

/// struct drm_amdgpu_info_device, whole (the kernel writes at most return_size of it).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InfoDevice {
    pub device_id: u32,
    pub chip_rev: u32,
    pub external_rev: u32,
    pub pci_rev: u32,
    pub family: u32,
    pub num_shader_engines: u32,
    pub num_shader_arrays_per_engine: u32,
    pub gpu_counter_freq: u32,
    pub max_engine_clock: u64,
    pub max_memory_clock: u64,
    pub cu_active_number: u32,
    pub cu_ao_mask: u32,
    pub cu_bitmap: [[u32; 4]; 4],
    pub enabled_rb_pipes_mask: u32,
    pub num_rb_pipes: u32,
    pub num_hw_gfx_contexts: u32,
    pub pcie_gen: u32,
    pub ids_flags: u64,
    pub virtual_address_offset: u64,
    pub virtual_address_max: u64,
    pub virtual_address_alignment: u32,
    pub pte_fragment_size: u32,
    pub gart_page_size: u32,
    pub ce_ram_size: u32,
    pub vram_type: u32,
    pub vram_bit_width: u32,
    pub vce_harvest_config: u32,
    pub gc_double_offchip_lds_buf: u32,
    pub prim_buf_gpu_addr: u64,
    pub pos_buf_gpu_addr: u64,
    pub cntl_sb_buf_gpu_addr: u64,
    pub param_buf_gpu_addr: u64,
    pub prim_buf_size: u32,
    pub pos_buf_size: u32,
    pub cntl_sb_buf_size: u32,
    pub param_buf_size: u32,
    pub wave_front_size: u32,
    pub num_shader_visible_vgprs: u32,
    pub num_cu_per_sh: u32,
    pub num_tcc_blocks: u32,
    pub gs_vgt_table_depth: u32,
    pub gs_prim_buffer_depth: u32,
    pub max_gs_waves_per_vgt: u32,
    pub pcie_num_lanes: u32,
    pub cu_ao_bitmap: [[u32; 4]; 4],
    pub high_va_offset: u64,
    pub high_va_max: u64,
    pub pa_sc_tile_steering_override: u32,
    pub tcc_disabled_mask: u64,
    pub min_engine_clock: u64,
    pub min_memory_clock: u64,
    pub tcp_cache_size: u32,
    pub num_sqc_per_wgp: u32,
    pub sqc_data_cache_size: u32,
    pub sqc_inst_cache_size: u32,
    pub gl1c_cache_size: u32,
    pub gl2c_cache_size: u32,
    pub mall_size: u64,
    pub enabled_rb_pipes_mask_hi: u32,
    pub shadow_size: u32,
    pub shadow_alignment: u32,
    pub csa_size: u32,
    pub csa_alignment: u32,
    pub userq_ip_mask: u32,
    pub pad: u32,
}

impl Default for InfoDevice {
    fn default() -> Self {
        // SAFETY: every field is an integer or an array of them: all zeros is a valid value.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct InfoHwIp {
    pub hw_ip_version_major: u32,
    pub hw_ip_version_minor: u32,
    pub capabilities_flags: u64,
    pub ib_start_alignment: u32,
    pub ib_size_alignment: u32,
    pub available_rings: u32,
    pub ip_discovery_version: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct InfoVramGtt {
    pub vram_size: u64,
    pub vram_cpu_accessible_size: u64,
    pub gtt_size: u64,
}

pub const DRM_IOCTL_AMDGPU_CTX: u32 = iowr(b'd', BASE + 0x02, size_of::<Ctx>());
pub const DRM_IOCTL_AMDGPU_BO_LIST: u32 = iowr(b'd', BASE + 0x03, size_of::<BoListIn>());
pub const DRM_IOCTL_AMDGPU_CS: u32 = iowr(b'd', BASE + 0x04, size_of::<Cs>());
pub const DRM_IOCTL_AMDGPU_INFO: u32 = iow(b'd', BASE + 0x05, size_of::<AmdgpuInfo>());
pub const DRM_IOCTL_AMDGPU_GEM_METADATA: u32 = iowr(b'd', BASE + 0x06, size_of::<GemMetadata>());
pub const DRM_IOCTL_AMDGPU_GEM_VA: u32 = iow(b'd', BASE + 0x08, size_of::<GemVa>());
pub const DRM_IOCTL_AMDGPU_WAIT_CS: u32 = iowr(b'd', BASE + 0x09, size_of::<WaitCs>());
pub const DRM_IOCTL_AMDGPU_VM: u32 = iowr(b'd', BASE + 0x13, size_of::<Vm>());

pub const AMDGPU_GEM_DOMAIN_GTT: u32 = 0x2;
pub const AMDGPU_GEM_DOMAIN_VRAM: u32 = 0x4;
pub const AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED: u64 = 1 << 0;
pub const AMDGPU_GEM_CREATE_VM_ALWAYS_VALID: u64 = 1 << 6;
pub const AMDGPU_BO_LIST_OP_CREATE: u32 = 0;
pub const AMDGPU_BO_LIST_OP_DESTROY: u32 = 1;
pub const AMDGPU_CTX_OP_ALLOC_CTX: u32 = 1;
pub const AMDGPU_CTX_OP_FREE_CTX: u32 = 2;
pub const AMDGPU_CTX_OP_GET_STABLE_PSTATE: u32 = 5;
pub const AMDGPU_CTX_OP_SET_STABLE_PSTATE: u32 = 6;
pub const AMDGPU_VM_OP_RESERVE_VMID: u32 = 1;
pub const AMDGPU_VM_OP_UNRESERVE_VMID: u32 = 2;
pub const AMDGPU_GEM_METADATA_OP_SET_METADATA: u32 = 1;
pub const AMDGPU_VA_OP_MAP: u32 = 1;
pub const AMDGPU_VA_OP_UNMAP: u32 = 2;
pub const AMDGPU_VA_OP_CLEAR: u32 = 3;
pub const AMDGPU_VA_OP_REPLACE: u32 = 4;
pub const AMDGPU_VM_PAGE_READABLE: u32 = 1 << 1;
pub const AMDGPU_VM_PAGE_EXECUTABLE: u32 = 1 << 3;
pub const AMDGPU_VM_PAGE_PRT: u32 = 1 << 4;
pub const AMDGPU_HW_IP_GFX: u32 = 0;
pub const AMDGPU_HW_IP_NUM: u32 = 10;
pub const AMDGPU_CHUNK_ID_IB: u32 = 0x01;
pub const AMDGPU_CHUNK_ID_FENCE: u32 = 0x02;
pub const AMDGPU_CHUNK_ID_DEPENDENCIES: u32 = 0x03;
pub const AMDGPU_CHUNK_ID_SYNCOBJ_OUT: u32 = 0x05;
pub const AMDGPU_CHUNK_ID_BO_HANDLES: u32 = 0x06;
pub const AMDGPU_INFO_HW_IP_INFO: u32 = 0x02;
pub const AMDGPU_INFO_VRAM_USAGE: u32 = 0x10;
pub const AMDGPU_INFO_GTT_USAGE: u32 = 0x11;
pub const AMDGPU_INFO_VRAM_GTT: u32 = 0x14;
pub const AMDGPU_INFO_READ_MMR_REG: u32 = 0x15;
pub const AMDGPU_INFO_DEV_INFO: u32 = 0x16;
pub const AMDGPU_INFO_VIS_VRAM_USAGE: u32 = 0x17;
pub const AMDGPU_FAMILY_AI: u32 = 141;
pub const AMDGPU_FAMILY_CI: u32 = 120;
/// libdrm's amdgpu.h and amdgpu_internal.h
pub const AMDGPU_TIMEOUT_INFINITE: u64 = u64::MAX;
pub const AMDGPU_QUERY_FENCE_TIMEOUT_IS_ABSOLUTE: u64 = 1;
pub const AMDGPU_CS_MAX_RINGS: u32 = 8;

// ---- the guest's protocol (amdgpu_virtio_proto.h, drm_hw.h): offsets into a request's bytes -------------------------
/// enum amdgpu_ccmd
pub mod ccmd {
    pub const QUERY_INFO: u32 = 1;
    pub const GEM_NEW: u32 = 2;
    pub const BO_VA_OP: u32 = 3;
    pub const CS_SUBMIT: u32 = 4;
    pub const SET_METADATA: u32 = 5;
    pub const BO_QUERY_INFO: u32 = 6;
    pub const CREATE_CTX: u32 = 7;
    pub const RESERVE_VMID: u32 = 8;
    pub const SET_PSTATE: u32 = 9;
    pub const CS_QUERY_FENCE_STATUS: u32 = 10;
    /// the name of each, by number (the error lines)
    pub const NAMES: [&str; 11] = [
        "?",
        "QUERY_INFO",
        "GEM_NEW",
        "BO_VA_OP",
        "CS_SUBMIT",
        "SET_METADATA",
        "BO_QUERY_INFO",
        "CREATE_CTX",
        "RESERVE_VMID",
        "SET_PSTATE",
        "CS_QUERY_FENCE_STATUS",
    ];
    /// sizeof each request (a shorter one is zero-extended to it, as drm_context.c does)
    pub const REQ_SIZE: [usize; 11] = [0, 48, 56, 64, 32, 40, 24, 24, 24, 32, 56];
}

/// vdrm_ccmd_req: cmd, len, seqno, rsp_off
pub const HDR: usize = 16;
/// vdrm_ccmd_rsp {len} + amdgpu_ccmd_rsp's ret
pub const RSP: usize = 8;
pub const RSP_RET: usize = 4;

// amdgpu_ccmd_query_info_req: hdr, struct drm_amdgpu_info info
pub const QUERY_INFO_INFO: usize = 16;
// amdgpu_ccmd_gem_new_req: hdr, blob_id, r {alloc_size, phys_alignment, preferred_heap, __pad, flags}
pub const GEM_NEW_BLOB_ID: usize = 16;
pub const GEM_NEW_ALLOC_SIZE: usize = 24;
pub const GEM_NEW_PHYS_ALIGNMENT: usize = 32;
pub const GEM_NEW_PREFERRED_HEAP: usize = 40;
pub const GEM_NEW_PAD: usize = 44;
pub const GEM_NEW_FLAGS: usize = 48;
// amdgpu_ccmd_bo_va_op_req: hdr, va, vm_map_size, flags, flags2, offset, res_id, op
pub const VA_OP_VA: usize = 16;
pub const VA_OP_SIZE: usize = 24;
pub const VA_OP_FLAGS: usize = 32;
pub const VA_OP_FLAGS2: usize = 40;
pub const VA_OP_OFFSET: usize = 48;
pub const VA_OP_RES_ID: usize = 56;
pub const VA_OP_OP: usize = 60;
pub const VA_OP_SPARSE_BO: u64 = 1;
// amdgpu_ccmd_cs_submit_req: hdr, ctx_id, num_chunks, pad, ring_idx, payload (descriptors {u16 chunk_id, u16
// length_dw, u32 offset}, then the chunks' data)
pub const CS_CTX_ID: usize = 16;
pub const CS_NUM_CHUNKS: usize = 20;
pub const CS_RING_IDX: usize = 28;
pub const CS_PAYLOAD: usize = 32;
pub const CS_DESC: usize = 8;
pub const CS_MAX_NUM_CHUNKS: u32 = 128;
// amdgpu_ccmd_set_metadata_req: hdr, flags, tiling_info, res_id, size_metadata, umd_metadata[]
pub const MD_FLAGS: usize = 16;
pub const MD_TILING: usize = 24;
pub const MD_RES_ID: usize = 32;
pub const MD_SIZE: usize = 36;
pub const MD_DATA: usize = 40;
// amdgpu_ccmd_bo_query_info_req: hdr, res_id, pad; its response: hdr (8), info {alloc_size, phys_alignment,
// preferred_heap, __pad, alloc_flags, metadata {flags, tiling_info, size_metadata, umd_metadata[64], __pad}}
pub const BQ_RES_ID: usize = 16;
pub const BQ_PAD: usize = 20;
pub const BQ_RSP_ALLOC_SIZE: usize = 8;
pub const BQ_RSP_PHYS_ALIGNMENT: usize = 16;
pub const BQ_RSP_PREFERRED_HEAP: usize = 24;
pub const BQ_RSP_ALLOC_FLAGS: usize = 32;
pub const BQ_RSP_MD_FLAGS: usize = 40;
pub const BQ_RSP_MD_TILING: usize = 48;
pub const BQ_RSP_MD_SIZE: usize = 56;
pub const BQ_RSP_MD_DATA: usize = 60;
pub const BQ_RSP_SIZE: usize = 320;
// amdgpu_ccmd_create_ctx_req: hdr, priority|id, flags; its response: hdr, ctx_id, pad
pub const CTX_PRIORITY: usize = 16;
pub const CTX_FLAGS: usize = 20;
pub const CTX_DESTROY: u32 = 1;
pub const CTX_RSP_ID: usize = 8;
pub const CTX_RSP_SIZE: usize = 16;
// amdgpu_ccmd_reserve_vmid_req: hdr, flags
pub const VMID_FLAGS: usize = 16;
pub const VMID_UNRESERVE: u64 = 1;
// amdgpu_ccmd_set_pstate_req: hdr, ctx_id, op, flags, pad; its response: hdr, out_flags, pad
pub const PSTATE_CTX_ID: usize = 16;
pub const PSTATE_OP: usize = 20;
pub const PSTATE_FLAGS: usize = 24;
pub const PSTATE_PAD: usize = 28;
pub const PSTATE_RSP_SIZE: usize = 16;
// amdgpu_ccmd_cs_query_fence_status_req: hdr, ctx_id, ip_type, ip_instance, ring, fence, timeout_ns, flags; its
// response: hdr, expired, pad
pub const QF_CTX_ID: usize = 16;
pub const QF_IP_TYPE: usize = 20;
pub const QF_IP_INSTANCE: usize = 24;
pub const QF_RING: usize = 28;
pub const QF_FENCE: usize = 32;
pub const QF_TIMEOUT: usize = 40;
pub const QF_FLAGS: usize = 48;
pub const QF_RSP_SIZE: usize = 16;

/// struct amdvgpu_shmem: base {seqno, rsp_mem_offset}, async_error, __pad, gtt, vram, vis_vram (each struct
/// amdgpu_heap_info {heap_size, heap_usage, max_allocation}); the responses start after it.
pub const SHMEM_SEQNO: usize = 0;
pub const SHMEM_RSP_MEM_OFFSET: usize = 4;
pub const SHMEM_ASYNC_ERROR: usize = 8;
pub const SHMEM_GTT: usize = 16;
pub const SHMEM_VRAM: usize = 40;
pub const SHMEM_VIS_VRAM: usize = 64;
pub const SHMEM_SIZE: usize = 88;

/// struct virgl_renderer_capset_drm with ENABLE_DRM_AMDGPU: the header (wire_format_version, version_major,
/// version_minor, version_patchlevel, context_type, pad), then u.amdgpu {address32_hi, has_vm_always_valid:1,
/// alignments {size_local, size_remote}, gpu_info (libdrm's struct amdgpu_gpu_info), marketing_name[128]}.
pub mod cap {
    pub const VERSION_MAJOR: usize = 4;
    pub const VERSION_MINOR: usize = 8;
    pub const VERSION_PATCHLEVEL: usize = 12;
    pub const CONTEXT_TYPE: usize = 16;
    pub const ADDRESS32_HI: usize = 24;
    pub const FLAGS: usize = 28;
    pub const SIZE_LOCAL: usize = 32;
    pub const SIZE_REMOTE: usize = 40;
    pub const GPU_INFO: usize = 48;
    pub const MARKETING_NAME: usize = 464;
    pub const MARKETING_NAME_LEN: usize = 128;
    pub const SIZE: usize = 592;
    pub const CONTEXT_AMDGPU: u32 = 2;
    /// struct amdgpu_gpu_info's fields, from GPU_INFO
    pub mod info {
        pub const ASIC_ID: usize = 0;
        pub const CHIP_REV: usize = 4;
        pub const CHIP_EXTERNAL_REV: usize = 8;
        pub const FAMILY_ID: usize = 12;
        pub const IDS_FLAGS: usize = 16;
        pub const MAX_ENGINE_CLK: usize = 24;
        pub const MAX_MEMORY_CLK: usize = 32;
        pub const NUM_SHADER_ENGINES: usize = 40;
        pub const NUM_SHADER_ARRAYS_PER_ENGINE: usize = 44;
        pub const NUM_HW_GFX_CONTEXTS: usize = 60;
        pub const RB_PIPES: usize = 64;
        pub const ENABLED_RB_PIPES_MASK: usize = 68;
        pub const GPU_COUNTER_FREQ: usize = 72;
        pub const BACKEND_DISABLE: usize = 76;
        pub const MC_ARB_RAMCFG: usize = 92;
        pub const GB_ADDR_CFG: usize = 96;
        pub const GB_TILE_MODE: usize = 100;
        pub const GB_MACRO_TILE_MODE: usize = 228;
        pub const PA_SC_RASTER_CFG: usize = 292;
        pub const PA_SC_RASTER_CFG1: usize = 308;
        pub const CU_ACTIVE_NUMBER: usize = 324;
        pub const CU_AO_MASK: usize = 328;
        pub const CU_BITMAP: usize = 332;
        pub const VRAM_TYPE: usize = 396;
        pub const VRAM_BIT_WIDTH: usize = 400;
        pub const CE_RAM_SIZE: usize = 404;
        pub const VCE_HARVEST_CONFIG: usize = 408;
        pub const PCI_REV_ID: usize = 412;
        pub const SIZE: usize = 416;
    }
}
