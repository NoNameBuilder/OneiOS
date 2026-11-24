// SPDX-License-Identifier: MIT
//! The amdgpu path's self-test: what xnuhost runs before it gives the guest this renderer (session/board.rs, gpu=auto
//! and gpu=3d on an amdgpu host), and what `gpuctl amdgpu` runs alone. Two parts, each line "gpu: self-test: PASS
//! ..." or one "gpu: self-test: FAIL at ...: why":
//!   1. the A56 fork's steps (Musicalista/OneiOS-A56 c83b9ca src/gpu3d_xclipse.c; PASS on the A56 2026-11-13/14 in
//!      its record), by raw ioctls: a udmabuf of a sealed memfd imported into the render node (PRIME), mapped at a GPU
//!      address (GEM_VA), an amdgpu context, a BO list, an IB of eight GFX NOPs (0x80000000, the fork's PACKET2 NOP)
//!      submitted (CS) and its fence waited for (WAIT_CS, 1 s);
//!   2. the same through the renderer's own Gpu3d calls, as a guest's Mesa drives them (K1, K2): the capset, a
//!      context, its shmem from the pool, QUERY_INFO answered there, GEM_NEW and a HOST3D_GUEST blob of three
//!      udmabufs (one per guest page, three pages apart), CREATE_CTX, BO_VA_OP over the three, a CS_SUBMIT of the NOP
//!      IB (in the BO's last chunk) with the BO in its list, and its virtio fence retired through the retire callback.
//!
//! Nothing outlives it: its memfd, its renderer (and the renderer's fence thread), its DRM files go when it returns.
//! It never waits more than 2 s for the GPU.
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use xh_core::bytes::le;

use super::uapi::*;
use super::{call, info, prime_import, Amdgpu, Dev};
use crate::gpu3d::{self, Chunk, Entry, Errno, Gpu3d, LogFn};

/// The fork's IB: eight GFX10 PACKET2 NOPs (the IB alignment's eight dwords).
const NOP: u32 = 0x8000_0000;
const PHYS: u64 = 0x20_0000_0000; // any address: the "guest physical" base of the memfd
const PAGE: u64 = 4096;

fn fail(log: LogFn, what: &str, why: impl std::fmt::Display) -> String {
    let line = format!("gpu: self-test: FAIL at {what}: {why}");
    log(0, &line);
    line
}

fn pass(log: LogFn, what: &str) {
    log(1, &format!("gpu: self-test: PASS {what}"));
}

/// A sealed memfd of `size` bytes mapped here (udmabuf takes no other).
struct Ram {
    fd: OwnedFd,
    ptr: *mut u8,
    size: u64,
}

impl Ram {
    fn new(size: u64) -> Result<Ram, String> {
        let fd = xh_core::fd::sealed_memfd(c"oneios-amdgpu-selftest", libc::MFD_CLOEXEC, size)
            .map_err(|e| format!("a sealed memfd: {}", xh_core::sys::error_text(&e)))?;
        // SAFETY: a shared map of our memfd, unmapped in Drop.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", xh_core::sys::error_text(&std::io::Error::last_os_error())));
        }
        Ok(Ram { fd, ptr: p.cast(), size })
    }

    fn words(&self, off: u64, words: &[u32]) {
        assert!(off + words.len() as u64 * 4 <= self.size);
        // SAFETY: inside our mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), self.ptr.add(off as usize), words.len() * 4)
        }
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        // SAFETY: our mapping, of this size.
        unsafe { libc::munmap(self.ptr.cast(), self.size as usize) };
    }
}

/// A GPU address for `size` bytes in the device's low range, 64 KiB aligned.
fn gpu_va(d: &InfoDevice, size: u64) -> Option<u64> {
    let va = (d.virtual_address_offset.max(1 << 20) + 0xffff) & !0xffff;
    (va != 0 && d.virtual_address_max >= size && va <= d.virtual_address_max - size).then_some(va)
}

/// Part 1: the fork's steps, by raw ioctls.
fn raw(dev: &dyn Dev, node: &str, log: LogFn) -> Result<(), String> {
    let ram = Ram::new(2 * PAGE).map_err(|e| fail(log, "the memfd", e))?;
    ram.words(0, &[NOP; 8]);
    let chunks = dev
        .chunks(ram.fd.as_raw_fd(), PHYS, ram.size, &[Entry::new(PHYS, PAGE as u32)])
        .map_err(|e| fail(log, "a udmabuf of the memfd", e))?;
    let fd = dev.open(node).map_err(|e| fail(log, node, e))?;
    let fd = fd.as_raw_fd();
    let handle = prime_import(dev, fd, chunks[0].fd.as_raw_fd()).map_err(|e| fail(log, "PRIME_FD_TO_HANDLE", e))?;
    let mut d = InfoDevice::default();
    info(dev, fd, AMDGPU_INFO_DEV_INFO, [0; 4], &mut d).map_err(|e| fail(log, "AMDGPU_INFO_DEV_INFO", e))?;
    let va = gpu_va(&d, PAGE).ok_or_else(|| fail(log, "the GPU address range", "too small"))?;
    let mut m = GemVa {
        handle,
        operation: AMDGPU_VA_OP_MAP,
        flags: AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE,
        va_address: va,
        map_size: PAGE,
        ..Default::default()
    };
    call(dev, fd, DRM_IOCTL_AMDGPU_GEM_VA, &mut m).map_err(|e| fail(log, "GEM_VA", e))?;
    pass(
        log,
        &format!(
            "raw: guest memory (a udmabuf of a sealed memfd) imported into {node} and mapped at GPU address 0x{va:x}"
        ),
    );
    let mut c = Ctx { op: AMDGPU_CTX_OP_ALLOC_CTX, ..Default::default() };
    call(dev, fd, DRM_IOCTL_AMDGPU_CTX, &mut c).map_err(|e| fail(log, "AMDGPU_CTX alloc", e))?;
    let ctx = c.op; // out.alloc.ctx_id
    let mut entry = BoListEntry { bo_handle: handle, bo_priority: 0 };
    let mut list = BoListIn {
        operation: AMDGPU_BO_LIST_OP_CREATE,
        list_handle: 0,
        bo_number: 1,
        bo_info_size: 8,
        bo_info_ptr: (&mut entry as *mut BoListEntry) as u64,
    };
    call(dev, fd, DRM_IOCTL_AMDGPU_BO_LIST, &mut list).map_err(|e| fail(log, "AMDGPU_BO_LIST", e))?;
    let list_handle = list.operation; // out.list_handle
    let ib = CsChunkIb { va_start: va, ib_bytes: 32, ip_type: AMDGPU_HW_IP_GFX, ..Default::default() };
    let chunk = CsChunk { chunk_id: AMDGPU_CHUNK_ID_IB, length_dw: 8, chunk_data: (&ib as *const CsChunkIb) as u64 };
    let ptr = (&chunk as *const CsChunk) as u64;
    let mut cs =
        Cs { ctx_id: ctx, bo_list_handle: list_handle, num_chunks: 1, flags: 0, chunks: (&ptr as *const u64) as u64 };
    call(dev, fd, DRM_IOCTL_AMDGPU_CS, &mut cs).map_err(|e| fail(log, "AMDGPU_CS", e))?;
    let seq = (cs.ctx_id as u64) | (cs.bo_list_handle as u64) << 32; // out.handle
    pass(log, &format!("raw: an IB of 8 GFX NOPs submitted (context {ctx}, sequence {seq})"));
    let deadline = xh_core::time::monotonic_ns() + 1_000_000_000;
    let mut w =
        WaitCs { handle: seq, timeout: deadline, ip_type: AMDGPU_HW_IP_GFX, ip_instance: 0, ring: 0, ctx_id: ctx };
    call(dev, fd, DRM_IOCTL_AMDGPU_WAIT_CS, &mut w).map_err(|e| fail(log, "WAIT_CS", e))?;
    if w.handle != 0 {
        return Err(fail(log, "WAIT_CS", "the GPU did not finish it in 1 s"));
    }
    pass(log, &format!("raw: the GPU ran it (fence {seq} done)"));
    // The list, the context and the mapping go with the DRM file.
    let mut l = BoListIn { operation: AMDGPU_BO_LIST_OP_DESTROY, list_handle, ..Default::default() };
    let _ = call(dev, fd, DRM_IOCTL_AMDGPU_BO_LIST, &mut l);
    Ok(())
}

/// The renderer's Dev with one udmabuf per guest segment: a blob of several chunks from a few pages.
struct OneEach(Arc<dyn Dev>);

impl Dev for OneEach {
    fn open(&self, path: &str) -> Result<OwnedFd, Errno> {
        self.0.open(path)
    }
    unsafe fn ioctl(&self, fd: RawFd, request: u32, arg: *mut libc::c_void) -> Result<(), Errno> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.0.ioctl(fd, request, arg) }
    }
    fn chunks(&self, ram_fd: RawFd, ram_phys: u64, ram_size: u64, entries: &[Entry]) -> Result<Vec<Chunk>, Errno> {
        let mut all = Vec::new();
        for e in entries {
            all.extend(self.0.chunks(ram_fd, ram_phys, ram_size, std::slice::from_ref(e))?);
        }
        Ok(all)
    }
}

/// A guest's Mesa, as far as the self-test and the tests need one: amdgpu ccmds into the renderer, responses read
/// back from the shmem in the pool.
pub struct Guest<'a> {
    pub gpu: &'a dyn Gpu3d,
    pub ctx: u32,
    pub shmem: *mut u8,
    pub shmem_size: usize,
    next_rsp: u32,
    seqno: u32,
}

/// A request: the header (cmd, len, seqno, rsp_off: the last two filled when it is sent), then the body.
pub fn request(cmd: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; HDR];
    le::put_u32(&mut b, 0, cmd);
    b.extend_from_slice(body);
    b.resize(b.len().div_ceil(8) * 8, 0);
    let len = b.len() as u32;
    le::put_u32(&mut b, 4, len);
    b
}

impl<'a> Guest<'a> {
    /// The context's shmem at `shmem` (its MAP_BLOB'd address in the pool).
    pub fn new(gpu: &'a dyn Gpu3d, ctx: u32, shmem: *mut u8, shmem_size: usize) -> Guest<'a> {
        Guest { gpu, ctx, shmem, shmem_size, next_rsp: 0, seqno: 0 }
    }

    pub fn shmem_u32(&self, at: usize) -> u32 {
        assert!(at + 4 <= self.shmem_size);
        // SAFETY: inside the shmem's mapping; the renderer writes it, so a volatile read.
        unsafe { std::ptr::read_volatile(self.shmem.add(at) as *const u32) }
    }

    /// Numbers the requests, gives each with a response its room (vdrm_alloc_rsp: the length written there first),
    /// and sends them in one SUBMIT_3D. The responses' offsets, in order (None for those without one).
    pub fn send(&mut self, reqs: &mut [(Vec<u8>, usize)]) -> Result<Vec<Option<usize>>, Errno> {
        let mut stream = Vec::new();
        let mut at = Vec::new();
        let room = (self.shmem_size - SHMEM_SIZE) as u32;
        for (r, rsp_len) in reqs.iter_mut() {
            self.seqno += 1;
            le::put_u32(r, 8, self.seqno);
            if *rsp_len != 0 {
                let len = (*rsp_len as u32).div_ceil(8) * 8;
                if self.next_rsp + len >= room {
                    self.next_rsp = 0;
                }
                let off = self.next_rsp;
                self.next_rsp += len;
                le::put_u32(r, 12, off);
                let abs = SHMEM_SIZE + off as usize;
                // SAFETY: inside the shmem (room checked above): the guest's own length field.
                unsafe { std::ptr::write_volatile(self.shmem.add(abs) as *mut u32, len) };
                at.push(Some(abs));
            } else {
                at.push(None);
            }
            stream.extend_from_slice(r);
        }
        self.gpu.submit(self.ctx, &stream)?;
        Ok(at)
    }

    /// One request; its response's bytes (`rsp_len` of them; empty when it has none).
    pub fn one(&mut self, cmd: u32, body: &[u8], rsp_len: usize) -> Result<Vec<u8>, Errno> {
        let at = self.send(&mut [(request(cmd, body), rsp_len)])?;
        Ok(match at[0] {
            Some(abs) => {
                let mut out = vec![0u8; rsp_len];
                // SAFETY: inside the shmem: the response the renderer wrote.
                unsafe { std::ptr::copy_nonoverlapping(self.shmem.add(abs), out.as_mut_ptr(), rsp_len) };
                out
            }
            None => Vec::new(),
        })
    }

    pub fn seqno(&self) -> u32 {
        self.seqno
    }
}

/// Request bodies (after the 16-byte header), as Mesa's amdgpu_virtio*.c fills them.
pub mod body {
    use xh_core::bytes::le;

    pub fn query_info(query: u32, return_size: u32, arg: [u32; 4]) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        le::put_u32(&mut b, 8, return_size);
        le::put_u32(&mut b, 12, query);
        for (k, a) in arg.iter().enumerate() {
            le::put_u32(&mut b, 16 + k * 4, *a);
        }
        b
    }
    pub fn gem_new(blob_id: u64, size: u64, align: u64, heap: u32, flags: u64) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        le::put_u64(&mut b, 0, blob_id);
        le::put_u64(&mut b, 8, size);
        le::put_u64(&mut b, 16, align);
        le::put_u32(&mut b, 24, heap);
        le::put_u64(&mut b, 32, flags);
        b
    }
    #[allow(clippy::too_many_arguments)]
    pub fn va_op(va: u64, size: u64, flags: u64, flags2: u64, offset: u64, res: u32, op: u32) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        for (k, v) in [va, size, flags, flags2, offset].iter().enumerate() {
            le::put_u64(&mut b, k * 8, *v);
        }
        le::put_u32(&mut b, 40, res);
        le::put_u32(&mut b, 44, op);
        b
    }
    /// CS_SUBMIT: the chunks (id, data), their descriptors first (amdgpu_cs_submit_raw2's encoding).
    pub fn cs(ctx: u32, ring_idx: u32, chunks: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0u8; 16];
        le::put_u32(&mut b, 0, ctx);
        le::put_u32(&mut b, 4, chunks.len() as u32);
        le::put_u32(&mut b, 12, ring_idx);
        let mut offset = 0u32;
        for (id, data) in chunks {
            let mut d = [0u8; 8];
            d[..2].copy_from_slice(&(*id as u16).to_le_bytes());
            d[2..4].copy_from_slice(&((data.len() / 4) as u16).to_le_bytes());
            d[4..].copy_from_slice(&offset.to_le_bytes());
            b.extend_from_slice(&d);
            offset += data.len() as u32;
        }
        for (_, data) in chunks {
            b.extend_from_slice(data);
        }
        b
    }
    pub fn ib(va: u64, bytes: u32, ip: u32, ring: u32) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        le::put_u64(&mut b, 8, va);
        le::put_u32(&mut b, 16, bytes);
        le::put_u32(&mut b, 20, ip);
        le::put_u32(&mut b, 28, ring);
        b
    }
    pub fn bo_list(entries: &[(u32, u32)]) -> Vec<u8> {
        entries.iter().flat_map(|(h, p)| h.to_le_bytes().into_iter().chain(p.to_le_bytes())).collect()
    }
    pub fn create_ctx(priority_or_id: u32, flags: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        le::put_u32(&mut b, 0, priority_or_id);
        le::put_u32(&mut b, 4, flags);
        b
    }
}

/// The guest ring of the GFX IP's first ring (amdgpu_virtio_device.c: ring 0 is the CPU's, then each IP type's rings
/// in type order).
pub fn gfx_ring(probe: &super::Probe) -> Option<u32> {
    let mut next = 1;
    for &(ip, n) in &probe.rings {
        if ip == AMDGPU_HW_IP_GFX {
            return Some(next);
        }
        next += n;
    }
    None
}

#[derive(Default)]
struct Retired {
    got: Mutex<Vec<(u32, u32, u64)>>,
    cond: Condvar,
}

/// Part 2: the same through the renderer (Amdgpu over a memfd of its own as the guest's RAM).
fn renderer(dev: Arc<dyn Dev>, node: &str, log: LogFn) -> Result<(), String> {
    const RAM_BYTES: u64 = 8 << 20;
    const POOL_BYTES: u64 = 4 << 20;
    let ram = Ram::new(RAM_BYTES).map_err(|e| fail(log, "the memfd", e))?;
    let pool_off = RAM_BYTES - POOL_BYTES;
    // SAFETY: inside the mapping.
    let pool_host = unsafe { ram.ptr.add(pool_off as usize) };
    let g = Amdgpu::open(
        Arc::new(OneEach(dev)),
        node,
        ram.fd.as_raw_fd(),
        PHYS,
        RAM_BYTES,
        pool_host,
        PHYS + pool_off,
        POOL_BYTES,
        log,
    )
    .map_err(|e| fail(log, "opening the renderer", e))?;
    let caps = g.capset_info(0);
    let mut cap = vec![0u8; cap::SIZE];
    if caps != Some((gpu3d::CAPSET_DRM, 0, cap::SIZE as u32))
        || g.capset(gpu3d::CAPSET_DRM, 0, &mut cap) != cap::SIZE
        || le::u32(&cap, cap::CONTEXT_TYPE) != cap::CONTEXT_AMDGPU
    {
        return Err(fail(log, "the capset", format!("{caps:?}, context type {}", le::u32(&cap, cap::CONTEXT_TYPE))));
    }
    let retired = Arc::new(Retired::default());
    let r = retired.clone();
    g.set_retire(Arc::new(move |ctx, ring, fence| {
        r.got.lock().unwrap_or_else(|e| e.into_inner()).push((ctx, ring, fence));
        r.cond.notify_all();
    }));
    let ctx = 1;
    g.ctx_create(ctx, gpu3d::CAPSET_DRM, b"xnu:self-test").map_err(|e| fail(log, "a context", e))?;
    let (shmem_res, bo_res) = (1, 2);
    const SHMEM: u64 = 16384;
    g.create_blob(ctx, shmem_res, gpu3d::BLOB_HOST3D, gpu3d::BLOB_FLAG_MAPPABLE, 0, SHMEM, &[])
        .map_err(|e| fail(log, "the shmem blob", e))?;
    let (off, _) = g.map_blob(shmem_res).map_err(|e| fail(log, "MAP_BLOB of the shmem", e))?;
    // SAFETY: inside the pool (map_blob answered an offset in it).
    let mut guest = Guest::new(&g, ctx, unsafe { pool_host.add(off as usize) }, SHMEM as usize);
    if guest.shmem_u32(SHMEM_RSP_MEM_OFFSET) != SHMEM_SIZE as u32 {
        return Err(fail(log, "the shmem's header", "rsp_mem_offset"));
    }
    let want = std::mem::size_of::<InfoDevice>();
    let rsp = guest
        .one(ccmd::QUERY_INFO, &body::query_info(AMDGPU_INFO_DEV_INFO, want as u32, [0; 4]), RSP + want)
        .map_err(|e| fail(log, "QUERY_INFO", e))?;
    let device_id = le::u32(&rsp, RSP);
    if le::u32(&rsp, RSP_RET) != 0
        || device_id != g.probe().dev.device_id
        || guest.shmem_u32(SHMEM_SEQNO) != guest.seqno()
    {
        return Err(fail(
            log,
            "QUERY_INFO's answer in the shmem",
            format!("ret {}, device 0x{device_id:x}", le::u32(&rsp, RSP_RET) as i32),
        ));
    }
    pass(log, &format!("renderer: a context, its shmem at pool offset 0x{off:x}, QUERY_INFO answered there (device 0x{device_id:04x})"));
    // A BO of three pages, each a udmabuf of its own, three pages apart in the RAM; the IB in the last.
    const BO: u64 = 3 * PAGE;
    let pages = [PHYS + 16 * PAGE, PHYS + 19 * PAGE, PHYS + 22 * PAGE];
    ram.words(22 * PAGE, &[NOP; 8]);
    guest
        .one(ccmd::GEM_NEW, &body::gem_new(1, BO, PAGE, AMDGPU_GEM_DOMAIN_GTT, 0), 0)
        .map_err(|e| fail(log, "GEM_NEW", e))?;
    let entries: Vec<Entry> = pages.iter().map(|&p| Entry::new(p, PAGE as u32)).collect();
    g.create_blob(
        ctx,
        bo_res,
        gpu3d::BLOB_HOST3D_GUEST,
        gpu3d::BLOB_FLAG_MAPPABLE | gpu3d::BLOB_FLAG_GUEST_HANDLE,
        1,
        BO,
        &entries,
    )
    .map_err(|e| fail(log, "the guest blob (HOST3D_GUEST)", e))?;
    let rsp =
        guest.one(ccmd::CREATE_CTX, &body::create_ctx(0, 0), CTX_RSP_SIZE).map_err(|e| fail(log, "CREATE_CTX", e))?;
    let amd_ctx = le::u32(&rsp, CTX_RSP_ID);
    if le::u32(&rsp, RSP_RET) != 0 || amd_ctx == 0 {
        return Err(fail(log, "CREATE_CTX", format!("ret {}", le::u32(&rsp, RSP_RET) as i32)));
    }
    let va = gpu_va(&g.probe().dev, BO).ok_or_else(|| fail(log, "the GPU address range", "too small"))?;
    let flags = (AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE) as u64;
    let rsp = guest
        .one(ccmd::BO_VA_OP, &body::va_op(va, BO, flags, 0, 0, bo_res, AMDGPU_VA_OP_MAP), RSP)
        .map_err(|e| fail(log, "BO_VA_OP", e))?;
    if le::u32(&rsp, RSP_RET) != 0 {
        return Err(fail(log, "BO_VA_OP", Errno(-(le::u32(&rsp, RSP_RET) as i32))));
    }
    pass(log, &format!("renderer: GEM_NEW and a guest blob of 3 udmabufs, mapped as one BO at GPU address 0x{va:x}"));
    let ring = gfx_ring(g.probe()).ok_or_else(|| fail(log, "the rings", "no GFX ring"))?;
    let cs = body::cs(
        amd_ctx,
        ring,
        &[
            (AMDGPU_CHUNK_ID_BO_HANDLES, body::bo_list(&[(bo_res, 0)])),
            (AMDGPU_CHUNK_ID_IB, body::ib(va + 2 * PAGE, 32, AMDGPU_HW_IP_GFX, 0)),
        ],
    );
    let rsp = guest.one(ccmd::CS_SUBMIT, &cs, RSP).map_err(|e| fail(log, "CS_SUBMIT", e))?;
    if le::u32(&rsp, RSP_RET) != 0 {
        return Err(fail(log, "CS_SUBMIT", Errno(-(le::u32(&rsp, RSP_RET) as i32))));
    }
    let start = xh_core::time::monotonic_ns();
    g.submit_fence(ctx, ring, 1).map_err(|e| fail(log, "the virtio fence", e))?;
    let got = retired.got.lock().unwrap_or_else(|e| e.into_inner());
    let (got, timeout) = retired
        .cond
        .wait_timeout_while(got, Duration::from_secs(2), |v| !v.contains(&(ctx, ring, 1)))
        .unwrap_or_else(|e| e.into_inner());
    if timeout.timed_out() {
        return Err(fail(log, "the virtio fence", "not retired in 2 s"));
    }
    drop(got);
    pass(
        log,
        &format!(
            "renderer: CS_SUBMIT of the NOP IB on ring {ring} (GFX), its virtio fence retired through the retire callback \
             in {} us",
            (xh_core::time::monotonic_ns() - start) / 1000
        ),
    );
    let _ = guest.one(ccmd::BO_VA_OP, &body::va_op(va, BO, flags, 0, 0, bo_res, AMDGPU_VA_OP_UNMAP), RSP);
    let _ = guest.one(ccmd::CREATE_CTX, &body::create_ctx(amd_ctx, CTX_DESTROY), CTX_RSP_SIZE);
    g.unref(bo_res);
    g.unref(shmem_res);
    g.ctx_destroy(ctx);
    Ok(())
}

/// Both parts on `node`; Err(the FAIL line).
pub fn run(dev: Arc<dyn Dev>, node: &str, log: LogFn) -> Result<(), String> {
    raw(&*dev, node, log)?;
    renderer(dev, node, log)
}
