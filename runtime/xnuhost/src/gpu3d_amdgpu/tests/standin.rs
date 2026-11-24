// SPDX-License-Identifier: MIT
//! A stand-in for amdgpu behind the renderer's Dev table: what the kernel's amdgpu does with each ioctl the renderer
//! makes, enough to check what it was asked. Each open is a DRM file of its own (an eventfd stands for it) with its
//! GEM handles (PRIME imports of the renderer's dma-bufs; the same dma-buf twice in one file is one handle, as
//! drm_prime's table makes it), its VA mappings (checked against the BOs' sizes; an IB must lie in a mapping of a BO
//! of the submission's list), its amdgpu contexts, BO lists and syncobjs. A submission's fence is an eventfd: the
//! test signals it (`complete`) and the sync_file the renderer exported from the ring's syncobj becomes readable.
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard};

use super::super::uapi::*;
use super::super::Dev;
use crate::gpu3d::{Chunk, Entry, Errno};

pub const NODE: &str = "/stand-in/dri/renderD128";

/// How guest pages become dma-bufs: one memfd per segment (a blob of several chunks from a few pages), or
/// /dev/udmabuf as on the phone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chunks {
    PerSegment,
    Udmabuf,
}

#[derive(Clone, Debug)]
pub struct Submission {
    pub file: RawFd,
    pub ctx_id: u32,
    pub bo_list: Vec<BoListEntry>,
    pub list_handle: u32,
    pub ibs: Vec<CsChunkIb>,
    pub fence: Option<CsChunkFence>,
    pub deps: usize,
    pub syncobj_out: u32,
    pub seq: u64,
    work: Arc<OwnedFd>,
    done: bool,
}

#[derive(Default)]
struct File {
    /// handle -> (the dma-buf's inode, its size)
    handles: HashMap<u32, (u64, u64)>,
    maps: Vec<GemVa>,
    ctxs: HashSet<u32>,
    lists: HashMap<u32, Vec<u32>>,
    /// syncobj -> the index of the submission whose fence it holds
    syncobjs: HashMap<u32, Option<usize>>,
    metadata: HashMap<u32, (u64, u64, u32)>,
}

#[derive(Default)]
pub struct State {
    files: HashMap<RawFd, File>,
    /// the descriptor of each open, in order
    pub order: Vec<RawFd>,
    pub opens: usize,
    pub closed: usize,
    next: u32,
    pub submissions: Vec<Submission>,
    /// every GEM_VA, in order: (file, the request)
    pub vas: Vec<(RawFd, GemVa)>,
    /// every ioctl's number, in order
    pub calls: Vec<u32>,
    /// requests that fail with this errno
    pub fail: HashMap<u32, i32>,
    /// submissions are done the moment they are made (the self-test's GPU)
    pub instant: bool,
    pub exports: usize,
}

pub struct Stand {
    pub driver: &'static str,
    pub chunks: Chunks,
    st: Mutex<State>,
    /// the stand-in's descriptors (one per open), closed when the renderer drops theirs: tracked by a dup each
    held: Mutex<HashMap<RawFd, OwnedFd>>,
}

pub fn device() -> InfoDevice {
    InfoDevice {
        device_id: 0x15bf,
        chip_rev: 1,
        external_rev: 0x81,
        pci_rev: 0x10,
        family: 150, // AMDGPU_FAMILY_GC_11_0_1
        num_shader_engines: 1,
        num_shader_arrays_per_engine: 2,
        gpu_counter_freq: 100_000,
        max_engine_clock: 1_300_000,
        cu_active_number: 8,
        num_rb_pipes: 4,
        enabled_rb_pipes_mask: 0xf,
        ids_flags: 1,
        virtual_address_offset: 0x20_0000,
        virtual_address_max: 0x7fff_ffff_f000,
        virtual_address_alignment: 4096,
        pte_fragment_size: 2 << 20,
        gart_page_size: 4096,
        vram_type: 8,
        vram_bit_width: 64,
        high_va_offset: 0xffff_8000_0000_0000,
        high_va_max: 0xffff_ffff_ffff_f000,
        ..Default::default()
    }
}

pub const GB_ADDR_CONFIG: u32 = 0x0000_0544;
/// GFX 1 ring, COMPUTE 4, DMA 2 (an RDNA3's shape): virtio rings 1, 2-5, 6-7
pub const RINGS: [(u32, u32); 3] = [(0, 0x1), (1, 0xf), (2, 0x3)];

impl Stand {
    pub fn new(driver: &'static str, chunks: Chunks) -> Arc<Stand> {
        Arc::new(Stand { driver, chunks, st: Mutex::new(State::default()), held: Mutex::new(HashMap::new()) })
    }

    pub fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The GPU finishes submission `k`.
    pub fn complete(&self, k: usize) {
        let mut s = self.st();
        let sub = &mut s.submissions[k];
        sub.done = true;
        let one = 1u64;
        // SAFETY: 8 bytes from our u64 into our eventfd.
        unsafe { libc::write(sub.work.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    /// The live DRM files (an open whose descriptor the renderer still holds).
    pub fn live(&self) -> usize {
        let mut h = self.held.lock().unwrap();
        let mut s = self.st();
        // A descriptor of the renderer's is closed when F_GETFD fails on it... unless the number was reused: the
        // stand-in's own dup of it is kept, and fstat of both says whether they are still the same file.
        let gone: Vec<RawFd> =
            h.iter().filter(|(fd, mine)| !same_file(**fd, mine.as_raw_fd())).map(|(fd, _)| *fd).collect();
        for fd in gone {
            h.remove(&fd);
            s.files.remove(&fd);
            s.closed += 1;
        }
        h.len()
    }

    pub fn handles(&self, file: RawFd) -> usize {
        self.st().files.get(&file).map_or(0, |f| f.handles.len())
    }

    pub fn maps(&self, file: RawFd) -> Vec<GemVa> {
        self.st().files.get(&file).map_or(Vec::new(), |f| f.maps.clone())
    }

    pub fn metadata(&self, file: RawFd, handle: u32) -> Option<(u64, u64, u32)> {
        self.st().files.get(&file).and_then(|f| f.metadata.get(&handle).copied())
    }

    /// The descriptor of the `k`th open (0: the renderer's probe).
    pub fn opened(&self, k: usize) -> RawFd {
        self.st().order[k]
    }
}

fn same_file(a: RawFd, b: RawFd) -> bool {
    // SAFETY: fstat into our structs; a closed descriptor fails.
    unsafe {
        let (mut x, mut y): (libc::stat, libc::stat) = (std::mem::zeroed(), std::mem::zeroed());
        libc::fstat(a, &mut x) == 0 && libc::fstat(b, &mut y) == 0 && x.st_ino == y.st_ino && x.st_dev == y.st_dev
    }
}

fn eventfd() -> OwnedFd {
    // SAFETY: a new eventfd of ours.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: as above.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn dmabuf_id(fd: RawFd) -> Option<(u64, u64)> {
    // SAFETY: fstat and lseek on a descriptor the renderer gave.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return None;
        }
        let size = libc::lseek(fd, 0, libc::SEEK_END);
        (size > 0).then_some((st.st_ino, size as u64))
    }
}

/// `arg` as a T.
///
/// # Safety
/// `arg` points at a T.
unsafe fn at<'a, T>(arg: *mut libc::c_void) -> &'a mut T {
    // SAFETY: the caller's contract.
    unsafe { &mut *(arg as *mut T) }
}

impl Dev for Stand {
    fn open(&self, path: &str) -> Result<OwnedFd, Errno> {
        if path != NODE {
            return Err(Errno(libc::ENOENT));
        }
        // a memfd: an inode of its own (eventfds share one), so `live` can tell a closed file from a reused number
        let fd = xh_core::fd::memfd(c"stand-in-drm-file", libc::MFD_CLOEXEC).unwrap();
        let mine = fd.try_clone().unwrap();
        let mut s = self.st();
        s.opens += 1;
        s.order.push(fd.as_raw_fd());
        s.files.insert(fd.as_raw_fd(), File::default());
        self.held.lock().unwrap().insert(fd.as_raw_fd(), mine);
        Ok(fd)
    }

    unsafe fn ioctl(&self, fd: RawFd, request: u32, arg: *mut libc::c_void) -> Result<(), Errno> {
        let mut g = self.st();
        let s = &mut *g;
        s.calls.push(request);
        if let Some(&e) = s.fail.get(&request) {
            return Err(Errno(e));
        }
        let instant = s.instant;
        let next = &mut s.next;
        let mut id = || {
            *next += 1;
            *next
        };
        let subs = &mut s.submissions;
        let Some(f) = s.files.get_mut(&fd) else { return Err(Errno(libc::EBADF)) };
        let einval = Err(Errno(libc::EINVAL));
        // SAFETY: in each arm, the renderer passes the structure the request encodes.
        unsafe {
            match request {
                DRM_IOCTL_VERSION => {
                    let v = at::<DrmVersion>(arg);
                    (v.version_major, v.version_minor, v.version_patchlevel) = (3, 49, 0);
                    let n = self.driver.len().min(v.name_len);
                    if !v.name.is_null() {
                        std::ptr::copy_nonoverlapping(self.driver.as_ptr(), v.name, n);
                    }
                    v.name_len = self.driver.len();
                    Ok(())
                }
                DRM_IOCTL_AMDGPU_INFO => {
                    let q = at::<AmdgpuInfo>(arg);
                    let out = q.return_pointer as *mut u8;
                    let put = |bytes: &[u8]| {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len().min(q.return_size as usize))
                    };
                    let as_bytes = |p: *const u8, n: usize| std::slice::from_raw_parts(p, n).to_vec();
                    match q.query {
                        AMDGPU_INFO_DEV_INFO => {
                            let d = device();
                            put(&as_bytes((&d as *const InfoDevice).cast(), std::mem::size_of::<InfoDevice>()));
                            Ok(())
                        }
                        AMDGPU_INFO_HW_IP_INFO => match RINGS.iter().find(|r| r.0 == q.arg[0]) {
                            Some(&(_, mask)) => {
                                let h =
                                    InfoHwIp { hw_ip_version_major: 11, available_rings: mask, ..Default::default() };
                                put(&as_bytes((&h as *const InfoHwIp).cast(), std::mem::size_of::<InfoHwIp>()));
                                Ok(())
                            }
                            None => einval,
                        },
                        AMDGPU_INFO_READ_MMR_REG if q.arg[0] == 0x263e => {
                            put(&GB_ADDR_CONFIG.to_le_bytes());
                            Ok(())
                        }
                        AMDGPU_INFO_VRAM_GTT => {
                            let v = InfoVramGtt {
                                vram_size: 512 << 20,
                                vram_cpu_accessible_size: 512 << 20,
                                gtt_size: 4 << 30,
                            };
                            put(&as_bytes((&v as *const InfoVramGtt).cast(), 24));
                            Ok(())
                        }
                        AMDGPU_INFO_VRAM_USAGE | AMDGPU_INFO_VIS_VRAM_USAGE | AMDGPU_INFO_GTT_USAGE => {
                            put(&(1u64 << 20).to_le_bytes());
                            Ok(())
                        }
                        _ => einval,
                    }
                }
                DRM_IOCTL_PRIME_FD_TO_HANDLE => {
                    let p = at::<DrmPrimeHandle>(arg);
                    let Some((ino, size)) = dmabuf_id(p.fd) else { return einval };
                    if let Some((&h, _)) = f.handles.iter().find(|(_, v)| v.0 == ino) {
                        p.handle = h;
                    } else {
                        p.handle = id();
                        f.handles.insert(p.handle, (ino, size));
                    }
                    Ok(())
                }
                DRM_IOCTL_GEM_CLOSE => {
                    let c = at::<DrmGemClose>(arg);
                    if f.handles.remove(&c.handle).is_none() {
                        return einval;
                    }
                    f.maps.retain(|m| m.handle != c.handle);
                    Ok(())
                }
                DRM_IOCTL_AMDGPU_GEM_VA => {
                    let v = *at::<GemVa>(arg);
                    s.vas.push((fd, v));
                    let aligned = (v.va_address | v.offset_in_bo | v.map_size) & 0xfff == 0;
                    let no_bo = v.operation == AMDGPU_VA_OP_CLEAR || v.flags & AMDGPU_VM_PAGE_PRT != 0;
                    if !aligned {
                        return einval;
                    }
                    if !no_bo {
                        let Some(&(_, size)) = f.handles.get(&v.handle) else { return Err(Errno(libc::ENOENT)) };
                        if v.offset_in_bo + v.map_size > size {
                            return einval;
                        }
                    }
                    match v.operation {
                        AMDGPU_VA_OP_MAP if !no_bo => f.maps.push(v),
                        AMDGPU_VA_OP_UNMAP => {
                            let Some(i) =
                                f.maps.iter().position(|m| m.handle == v.handle && m.va_address == v.va_address)
                            else {
                                return Err(Errno(libc::ENOENT));
                            };
                            f.maps.remove(i);
                        }
                        _ => {}
                    }
                    Ok(())
                }
                DRM_IOCTL_AMDGPU_CTX => {
                    let c = at::<Ctx>(arg);
                    match c.op {
                        AMDGPU_CTX_OP_ALLOC_CTX => {
                            let n = id();
                            f.ctxs.insert(n);
                            *c = Ctx { op: n, flags: 0, ctx_id: 0, priority: 0 };
                            Ok(())
                        }
                        AMDGPU_CTX_OP_FREE_CTX if f.ctxs.remove(&c.ctx_id) => Ok(()),
                        AMDGPU_CTX_OP_SET_STABLE_PSTATE | AMDGPU_CTX_OP_GET_STABLE_PSTATE
                            if f.ctxs.contains(&c.ctx_id) =>
                        {
                            c.op = c.flags; // out.pstate.flags
                            Ok(())
                        }
                        _ => einval,
                    }
                }
                DRM_IOCTL_AMDGPU_VM => Ok(()),
                DRM_IOCTL_AMDGPU_GEM_METADATA => {
                    let m = at::<GemMetadata>(arg);
                    if !f.handles.contains_key(&m.handle) {
                        return Err(Errno(libc::ENOENT));
                    }
                    f.metadata.insert(m.handle, (m.flags, m.tiling_info, m.data_size_bytes));
                    Ok(())
                }
                DRM_IOCTL_AMDGPU_BO_LIST => {
                    let l = at::<BoListIn>(arg);
                    match l.operation {
                        AMDGPU_BO_LIST_OP_CREATE => {
                            let e =
                                std::slice::from_raw_parts(l.bo_info_ptr as *const BoListEntry, l.bo_number as usize);
                            let n = id();
                            f.lists.insert(n, e.iter().map(|e| e.bo_handle).collect());
                            l.operation = n;
                            Ok(())
                        }
                        AMDGPU_BO_LIST_OP_DESTROY if f.lists.remove(&l.list_handle).is_some() => Ok(()),
                        _ => einval,
                    }
                }
                DRM_IOCTL_SYNCOBJ_CREATE => {
                    let o = at::<DrmSyncobj>(arg);
                    o.handle = id();
                    f.syncobjs.insert(o.handle, None);
                    Ok(())
                }
                DRM_IOCTL_SYNCOBJ_DESTROY => {
                    let o = at::<DrmSyncobj>(arg);
                    if f.syncobjs.remove(&o.handle).is_none() {
                        return einval;
                    }
                    Ok(())
                }
                DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD => {
                    let h = at::<DrmSyncobjHandle>(arg);
                    if h.flags != DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE {
                        return einval;
                    }
                    let Some(Some(k)) = f.syncobjs.get(&h.handle) else { return einval };
                    h.fd = std::os::fd::IntoRawFd::into_raw_fd(subs[*k].work.try_clone().unwrap());
                    s.exports += 1;
                    Ok(())
                }
                DRM_IOCTL_AMDGPU_CS => {
                    let cs = at::<Cs>(arg);
                    if !f.ctxs.contains(&cs.ctx_id) {
                        return Err(Errno(libc::ENOENT));
                    }
                    let ptrs = std::slice::from_raw_parts(cs.chunks as *const u64, cs.num_chunks as usize);
                    let mut sub = Submission {
                        file: fd,
                        ctx_id: cs.ctx_id,
                        bo_list: Vec::new(),
                        list_handle: cs.bo_list_handle,
                        ibs: Vec::new(),
                        fence: None,
                        deps: 0,
                        syncobj_out: 0,
                        seq: 0,
                        work: Arc::new(eventfd()),
                        done: false,
                    };
                    for &p in ptrs {
                        let ch = *(p as *const CsChunk);
                        match ch.chunk_id {
                            AMDGPU_CHUNK_ID_BO_HANDLES => {
                                let l = *(ch.chunk_data as *const BoListIn);
                                if l.bo_info_size != 8 || ch.length_dw != 6 {
                                    return einval;
                                }
                                sub.bo_list = std::slice::from_raw_parts(
                                    l.bo_info_ptr as *const BoListEntry,
                                    l.bo_number as usize,
                                )
                                .to_vec();
                            }
                            AMDGPU_CHUNK_ID_IB => sub.ibs.push(*(ch.chunk_data as *const CsChunkIb)),
                            AMDGPU_CHUNK_ID_FENCE => sub.fence = Some(*(ch.chunk_data as *const CsChunkFence)),
                            AMDGPU_CHUNK_ID_DEPENDENCIES => sub.deps += ch.length_dw as usize / 6,
                            AMDGPU_CHUNK_ID_SYNCOBJ_OUT => {
                                sub.syncobj_out = (*(ch.chunk_data as *const CsChunkSem)).handle
                            }
                            _ => return einval,
                        }
                    }
                    let in_list: Vec<u32> = if sub.list_handle != 0 {
                        f.lists.get(&sub.list_handle).cloned().unwrap_or_default()
                    } else {
                        sub.bo_list.iter().map(|e| e.bo_handle).collect()
                    };
                    if in_list.iter().any(|h| !f.handles.contains_key(h)) {
                        return Err(Errno(libc::ENOENT));
                    }
                    // Each IB lies in a mapping of a BO of the list (else the GPU would fault).
                    for ib in &sub.ibs {
                        let (a, b) = (ib.va_start, ib.va_start + ib.ib_bytes as u64);
                        let ok = f.maps.iter().any(|m| {
                            in_list.contains(&m.handle) && a >= m.va_address && b <= m.va_address + m.map_size
                        });
                        if !ok {
                            return Err(Errno(libc::EFAULT));
                        }
                    }
                    if let Some(fe) = sub.fence {
                        if !f.handles.contains_key(&fe.handle) {
                            return Err(Errno(libc::ENOENT));
                        }
                    }
                    let k = subs.len();
                    sub.seq = k as u64 + 1;
                    if sub.syncobj_out != 0 {
                        let Some(slot) = f.syncobjs.get_mut(&sub.syncobj_out) else { return einval };
                        *slot = Some(k);
                    }
                    cs.ctx_id = sub.seq as u32; // out.handle
                    cs.bo_list_handle = (sub.seq >> 32) as u32;
                    if instant {
                        sub.done = true;
                        let one = 1u64;
                        libc::write(sub.work.as_raw_fd(), (&one as *const u64).cast(), 8);
                    }
                    subs.push(sub);
                    Ok(())
                }
                DRM_IOCTL_AMDGPU_WAIT_CS => {
                    let w = at::<WaitCs>(arg);
                    let Some(sub) = subs.get(w.handle.wrapping_sub(1) as usize).filter(|x| x.file == fd) else {
                        return einval;
                    };
                    w.handle = u64::from(!sub.done); // out.status
                    Ok(())
                }
                _ => Err(Errno(libc::ENOTTY)),
            }
        }
    }

    fn chunks(&self, ram_fd: RawFd, ram_phys: u64, ram_size: u64, entries: &[Entry]) -> Result<Vec<Chunk>, Errno> {
        match self.chunks {
            Chunks::Udmabuf => crate::gpu3d::udmabufs(ram_fd, ram_phys, ram_size, entries),
            Chunks::PerSegment => entries
                .iter()
                .map(|e| {
                    let fd =
                        xh_core::fd::memfd(c"stand-in-chunk", libc::MFD_CLOEXEC).map_err(|_| Errno(libc::ENOMEM))?;
                    let file = std::fs::File::from(fd);
                    file.set_len(e.length as u64).map_err(|_| Errno(libc::ENOMEM))?;
                    Ok(Chunk { fd: file.into(), size: e.length as u64 })
                })
                .collect(),
        }
    }
}
