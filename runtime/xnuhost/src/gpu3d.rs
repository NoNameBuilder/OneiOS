// SPDX-License-Identifier: MIT
//! virtio-gpu 3D: what virtio_gpu.rs asks of the renderer behind it (VirtIO 1.2 5.7 with crosvm's DRM native
//! context), and guest pages as dma-bufs. The real renderers are gpu3d_kgsl.rs: Droid-VM's drm2kgsl (virglrenderer's
//! msm protocol served on /dev/kgsl-3d0), linked into xnuhost (static; no helper process), and on the Galaxy A56
//! gpu3d_amdgpu.rs (the amdgpu protocol on an amdgpu render node, in Rust). The unit tests put a fake
//! one here and drive the device with generated command streams.
//!
//! /dev/udmabuf makes a dma-buf from ranges of a memfd; guest RAM is one memfd (mem.rs), so a blob's guest segments
//! become dma-bufs KGSL imports, with no copy and no pinning by address: one where the blob fits udmabuf's limits
//! (size_limit_mb, list_limit), several in a row where it does not.
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use xh_core::bytes::le;

pub use drm2kgsl_sys::Errno;

/// A guest memory segment of a HOST3D_GUEST blob: guest physical address and length (virtio_gpu_mem_entry, its wire
/// layout).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub addr: u64,
    pub length: u32,
    pub pad: u32,
}

impl Entry {
    pub const fn new(addr: u64, length: u32) -> Entry {
        Entry { addr, length, pad: 0 }
    }

    /// The entry at the start of `b` (16 bytes, little-endian).
    pub fn decode(b: &[u8]) -> Entry {
        Entry { addr: le::u64(b, 0), length: le::u32(b, 8), pad: le::u32(b, 12) }
    }
}

pub const CAPSET_DRM: u32 = 6;
pub const BLOB_GUEST: u32 = 1;
pub const BLOB_HOST3D: u32 = 2;
pub const BLOB_HOST3D_GUEST: u32 = 3;
pub const BLOB_FLAG_MAPPABLE: u32 = 1;
pub const BLOB_FLAG_GUEST_HANDLE: u32 = 8;
pub const MAP_CACHE_CACHED: u32 = 1;
pub const MAP_CACHE_UNCACHED: u32 = 2;
pub const MAP_CACHE_WC: u32 = 3;

/// The device's answer when a context's `ring` passes `fence` (and every earlier fence of that ring). Any thread.
pub type Retire = Arc<dyn Fn(u32, u32, u64) + Send + Sync>;

/// A renderer behind the virtio-gpu device. Contexts: 0 or an errno. HOST3D_GUEST blobs (a context's BOs) come with
/// the guest's pages; HOST3D ones (a context's shmem) are the renderer's; GUEST ones (ctx 0: the display's surfaces,
/// made with the GUEST_HANDLE flag) are only made known, so that a context can attach them. All calls but the retire
/// come from the device's command lanes, one at a time.
pub trait Gpu3d: Send + Sync {
    /// Capsets: their count, then each one's id, highest version and size; the bytes of one.
    fn capset_count(&self) -> u32;
    fn capset_info(&self, index: u32) -> Option<(u32, u32, u32)>;
    fn capset(&self, id: u32, version: u32, out: &mut [u8]) -> usize;
    fn ctx_create(&self, ctx: u32, capset: u32, name: &[u8]) -> Result<(), Errno>;
    fn ctx_destroy(&self, ctx: u32);
    /// The context's command stream (whole 32-bit words in an aligned buffer).
    fn submit(&self, ctx: u32, commands: &[u8]) -> Result<(), Errno>;
    /// A fence on `ring` of `ctx`: retired later through the retire, or at once (from inside this call).
    fn submit_fence(&self, ctx: u32, ring: u32, fence: u64) -> Result<(), Errno>;
    #[allow(clippy::too_many_arguments)]
    fn create_blob(
        &self,
        ctx: u32,
        res: u32,
        blob_mem: u32,
        flags: u32,
        blob_id: u64,
        size: u64,
        entries: &[Entry],
    ) -> Result<(), Errno>;
    /// Where a host blob is in the pool the guest knows (/arm-io/virtio-gpu drm-host-pool), and how to cache it.
    fn map_blob(&self, res: u32) -> Result<(u64, u32), Errno>;
    fn attach(&self, ctx: u32, res: u32, attach: bool) -> Result<(), Errno>;
    fn unref(&self, res: u32);
    /// The guest's word on who submits (OneiOSAdreno.cpp oneios_gpu_hint; gpu3d_kgsl/policy.rs HINT_*): byte 0 the
    /// task's role and DarwinBG, byte 1 the thread's QoS, byte 2 the time to the frame's deadline. Called right after
    /// a context is made (CTX_CREATE's context_init bits 8-23; byte 2 is 0) and before each SUBMIT_3D (the command
    /// header's three padding bytes). An older guest sends zeros.
    fn hint(&self, _ctx: u32, _hint: [u8; 3]) {}
    /// Set by the device before the first context.
    fn set_retire(&self, retire: Retire);
}

/// A log line of the renderer: level 0 error, 1 info, 2 debug.
pub type LogFn = fn(level: u8, line: &str);

/// drm2kgsl on /dev/kgsl-3d0 (gpu3d_kgsl.rs). Guest pages are imported through /dev/udmabuf from the RAM memfd
/// (`ram_fd`, guest physical `ram_phys` at offset 0, `ram_size` bytes); host blobs (the contexts' shmem) are carved from
/// the pool, a range of that memfd the kernel does not use: guest physical `pool_pa`, `pool_size` bytes, mapped at
/// `pool_host` here. The memfd must carry F_SEAL_SHRINK (udmabuf refuses one without). Err(why) when there is no KGSL,
/// no udmabuf, no dma heap or no drm2kgsl in this build. One per process.
pub fn kgsl_open(
    ram_fd: RawFd,
    ram_phys: u64,
    ram_size: u64,
    pool_host: *mut u8,
    pool_pa: u64,
    pool_size: u64,
    log: LogFn,
) -> Result<Box<dyn Gpu3d>, String> {
    if !drm2kgsl_sys::AVAILABLE {
        return Err(
            "this build has no drm2kgsl (external/virglrenderer was absent: scripts/setup/get-references.sh virglrenderer)"
                .into(),
        );
    }
    crate::gpu3d_kgsl::open(ram_fd, ram_phys, ram_size, pool_host, pool_pa, pool_size, log)
}

/// The amdgpu native context on the render node `node` (gpu3d_amdgpu.rs: the Galaxy A56's Xclipse), over the same
/// RAM and pool as [`kgsl_open`]. Err(why) when the node is not amdgpu or cannot be probed.
#[allow(clippy::too_many_arguments)]
pub fn amdgpu_open(
    node: &str,
    ram_fd: RawFd,
    ram_phys: u64,
    ram_size: u64,
    pool_host: *mut u8,
    pool_pa: u64,
    pool_size: u64,
    log: LogFn,
) -> Result<Box<dyn Gpu3d>, String> {
    crate::gpu3d_amdgpu::open(node, ram_fd, ram_phys, ram_size, pool_host, pool_pa, pool_size, log)
}

// ---- guest pages as dma-bufs (/dev/udmabuf)---------------------------------------------------------------------------
// The kernel caps one udmabuf at size_limit_mb MiB and list_limit runs of pages (64 and 1024 by default, and on the
// Galaxy S24 Ultra), so a blob becomes as many udmabufs ("chunks") as it needs, in the blob's order: a chunk ends where
// the next page would pass either limit. Adjacent segments are joined. The limits used are the kernel's
// (/sys/module/udmabuf/parameters) but never more than 64 MiB and 1024, so every host splits a blob the way the phone
// does.
const MAX_BYTES: u64 = 64 << 20; // the kernel's defaults; a host that allows more is not given more
const MAX_RUNS: u32 = 1024;
static LIMIT_BYTES: AtomicU64 = AtomicU64::new(0);
static LIMIT_RUNS: AtomicU32 = AtomicU32::new(0);

/// One udmabuf of a blob: its descriptor and size.
#[derive(Debug)]
pub struct Chunk {
    pub fd: OwnedFd,
    pub size: u64,
}

fn parameter(name: &str) -> i64 {
    let path = format!("/sys/module/udmabuf/parameters/{name}");
    xh_core::fs::read_raw(path).map_or(-1, |t| xh_core::parse::leading_i64(&t, 10).map_or(0, |(v, _)| v))
}

/// The limits in use: bytes and runs per udmabuf.
pub fn udmabuf_limits() -> (u64, u32) {
    if LIMIT_BYTES.load(Ordering::Relaxed) == 0 || LIMIT_RUNS.load(Ordering::Relaxed) == 0 {
        // Not readable (another SELinux domain) or absurd: the defaults.
        let (mb, list) = (parameter("size_limit_mb"), parameter("list_limit"));
        let bytes = if mb > 0 && (mb as u64) < (MAX_BYTES >> 20) { (mb as u64) << 20 } else { MAX_BYTES };
        let runs = if list > 0 && (list as u64) < MAX_RUNS as u64 { list as u32 } else { MAX_RUNS };
        LIMIT_BYTES.store(bytes, Ordering::Relaxed);
        LIMIT_RUNS.store(runs, Ordering::Relaxed);
    }
    (LIMIT_BYTES.load(Ordering::Relaxed), LIMIT_RUNS.load(Ordering::Relaxed))
}

/// Sets the limits (the tests; 0, 0 reads the kernel's again).
pub fn udmabuf_set_limits(bytes: u64, runs: u32) {
    LIMIT_BYTES.store(bytes & !0xfff, Ordering::Relaxed);
    LIMIT_RUNS.store(runs, Ordering::Relaxed);
}

// _IOW('u', 0x43, struct udmabuf_create_list): its flags and count, the items after them
const UDMABUF_CREATE_LIST: libc::c_ulong = xh_core::ioctl::iow(b'u', 0x43, 8) as libc::c_ulong;
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// struct udmabuf_create_list and its items, as the kernel reads them: { u32 flags, count } then { u32 memfd, pad;
/// u64 offset, size } each.
pub struct UdmabufList {
    words: Vec<u64>,
    count: usize,
}

impl UdmabufList {
    fn new(max: u32) -> UdmabufList {
        UdmabufList { words: vec![0; 1 + 3 * max as usize], count: 0 }
    }

    /// (memfd, offset, size) of each item.
    pub fn items(&self) -> impl Iterator<Item = (u32, u64, u64)> + '_ {
        (0..self.count).map(|i| (self.words[1 + 3 * i] as u32, self.words[2 + 3 * i], self.words[3 + 3 * i]))
    }

    fn last_end(&self) -> Option<u64> {
        self.count.checked_sub(1).map(|i| self.words[2 + 3 * i] + self.words[3 + 3 * i])
    }

    fn grow_last(&mut self, by: u64) {
        self.words[3 + 3 * (self.count - 1)] += by;
    }

    fn push(&mut self, memfd: u32, offset: u64, size: u64) {
        let i = self.count;
        (self.words[1 + 3 * i], self.words[2 + 3 * i], self.words[3 + 3 * i]) = (memfd as u64, offset, size);
        self.count += 1;
    }

    /// UDMABUF_CREATE_LIST on `dev` (what the hook calls to pass a list on to the kernel).
    pub fn ioctl(&mut self, dev: RawFd) -> libc::c_int {
        self.words[0] = UDMABUF_FLAGS_CLOEXEC as u64 | (self.count as u64) << 32;
        // SAFETY: the words are the kernel's struct (flags, count, then `count` items), all of them ours.
        unsafe { drm2kgsl_sys::sys::ioctl(dev, UDMABUF_CREATE_LIST, self.words.as_mut_ptr().cast()) }
    }
}

/// The UDMABUF_CREATE_LIST call: the result (a dma-buf fd, or -1 with errno).
pub type CreateList = fn(dev: RawFd, list: &mut UdmabufList) -> libc::c_int;
static CREATE: AtomicUsize = AtomicUsize::new(0);

/// Replaces the UDMABUF_CREATE_LIST call (None: the ioctl). Tests set it to make the kernel refuse a list: on ENOMEM
/// [`udmabufs`] halves the size limit (down to 4 MiB, for the rest of the process) and splits again.
pub fn set_udmabuf_create(f: Option<CreateList>) {
    CREATE.store(f.map_or(0, |f| f as usize), Ordering::Release);
}

fn create_list(dev: RawFd, list: &mut UdmabufList) -> libc::c_int {
    match CREATE.load(Ordering::Acquire) {
        0 => list.ioctl(dev),
        // SAFETY: CREATE only ever holds 0 or a `CreateList` stored by set_udmabuf_create.
        p => (unsafe { std::mem::transmute::<usize, CreateList>(p) })(dev, list),
    }
}

/// One split at these limits. ENOMEM from the kernel is returned as it is (the caller may try again smaller).
fn udmabufs_at(
    ram_fd: RawFd,
    ram_phys: u64,
    ram_size: u64,
    entries: &[Entry],
    max_bytes: u64,
    max_runs: u32,
) -> Result<Vec<Chunk>, Errno> {
    if entries.is_empty() {
        return Err(Errno(libc::EINVAL));
    }
    for e in entries {
        let (addr, length) = (e.addr, e.length as u64);
        if length == 0
            || (addr | length) & 0xfff != 0
            || addr < ram_phys
            || addr - ram_phys > ram_size
            || length > ram_size - (addr - ram_phys)
        {
            return Err(Errno(libc::EINVAL));
        }
    }
    let mut list = UdmabufList::new(max_runs);
    let mut chunks = Vec::new();
    let dev = drm2kgsl_sys::sys::open(c"/dev/udmabuf", libc::O_RDWR | libc::O_CLOEXEC)
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(libc::EIO)))?;
    let mut bytes = 0u64;
    let mut done = |list: &mut UdmabufList, bytes: &mut u64| -> Result<(), Errno> {
        let fd = create_list(dev.as_raw_fd(), list);
        if fd < 0 {
            return Err(Errno::last());
        }
        // SAFETY: a new dma-buf descriptor of ours.
        chunks.push(Chunk { fd: unsafe { OwnedFd::from_raw_fd(fd) }, size: *bytes });
        *bytes = 0;
        list.count = 0;
        Ok(())
    };
    for e in entries {
        let (mut offset, mut left) = (e.addr - ram_phys, e.length as u64);
        while left > 0 {
            let joins = list.last_end() == Some(offset);
            // The chunk is done: it is full, or this piece would be one run too many.
            if list.count > 0 && (bytes == max_bytes || (!joins && list.count == max_runs as usize)) {
                done(&mut list, &mut bytes)?;
                continue;
            }
            let take = left.min(max_bytes - bytes);
            if joins {
                list.grow_last(take);
            } else {
                list.push(ram_fd as u32, offset, take);
            }
            bytes += take;
            offset += take;
            left -= take;
        }
    }
    done(&mut list, &mut bytes)?; // the blob is
    Ok(chunks)
}

// Ported from YumikoHayamato/OneiOS (s25-ultra-sm8750, 51124a6; the S25 Ultra's `gpuctl large` failed, and passed with
// it). A kernel whose udmabuf keeps the page array in one kmalloc (Linux before 6.10: the S25 Ultra's 6.6, the S24
// Ultra's 6.1) needs physically contiguous memory for it: 8 bytes a page, 128 KiB (order 5) for a 64 MiB udmabuf. With
// fragmented memory that allocation fails (dmesg "page allocation failure: order:5", the ioctl ENOMEM) although plenty
// of RAM is free. Then udmabufs get smaller, by halves down to MIN_BYTES, for the rest of the process: more chunks,
// which the chunked blobs already take, instead of a blob the guest cannot have. Nothing changes until the kernel
// refuses one.
const MIN_BYTES: u64 = 4 << 20;

/// The chunks of a blob (1 or more), or the errno (EINVAL: a segment outside the RAM or not whole pages, or the memfd
/// without F_SEAL_SHRINK).
pub fn udmabufs(ram_fd: RawFd, ram_phys: u64, ram_size: u64, entries: &[Entry]) -> Result<Vec<Chunk>, Errno> {
    let (mut max_bytes, max_runs) = udmabuf_limits();
    loop {
        let r = udmabufs_at(ram_fd, ram_phys, ram_size, entries, max_bytes, max_runs);
        match r {
            Err(Errno(libc::ENOMEM)) if max_bytes > MIN_BYTES => {}
            r => return r,
        }
        let was = max_bytes;
        max_bytes = ((max_bytes / 2) & !0xfff).max(MIN_BYTES);
        LIMIT_BYTES.store(max_bytes, Ordering::Relaxed);
        eprintln!(
            "xnuhost: gpu: udmabuf: the kernel refused a {} MiB udmabuf (ENOMEM: its page array needs contiguous memory); \
             udmabufs of at most {} MiB from now on",
            was >> 20,
            max_bytes >> 20
        );
    }
}

/// One udmabuf of segments that fit one (E2BIG when they need more).
pub fn udmabuf(ram_fd: RawFd, ram_phys: u64, ram_size: u64, entries: &[Entry]) -> Result<OwnedFd, Errno> {
    let mut chunks = udmabufs(ram_fd, ram_phys, ram_size, entries)?;
    if chunks.len() != 1 {
        return Err(Errno(libc::E2BIG));
    }
    Ok(chunks.pop().unwrap().fd)
}

/// A borrowed view of chunks, for an attach.
pub fn chunk_views(chunks: &[Chunk]) -> Vec<(BorrowedFd<'_>, u64)> {
    use std::os::fd::AsFd;
    chunks.iter().map(|c| (c.fd.as_fd(), c.size)).collect()
}
