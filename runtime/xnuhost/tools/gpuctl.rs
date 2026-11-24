// SPDX-License-Identifier: MIT
//! gpuctl: tries the GPU path on the phone, without xnuhost or a guest (the port of tools/gpuctl.c). Run as root (su):
//!   gpuctl nodes      the three device nodes, their SELinux labels' effect (can this process open them) and udmabuf's limits
//!   gpuctl routes     KGSL alone, by raw ioctls (no drm2kgsl), each way memory could reach it, PASS or FAIL with the errno:
//!                     a udmabuf of memfd pages imported, a /dev/dma_heap/system buffer imported, a /dev/dma_heap/qcom,system
//!                     buffer imported, KGSL's own GPUOBJ_ALLOC mapped; each also bound into a small VBO and written by the
//!                     CPU. Then the address room: a udmabuf imported beside a VBO of each size drm2kgsl would try
//!   gpuctl probe      opens /dev/kgsl-3d0 through drm2kgsl: the chip, the GPU address range, the DRM capset a guest gets
//!   gpuctl submit     probe, then what one guest context does: the context, its response buffer from the pool, GET_PARAM
//!                     answered there, a buffer of "guest" pages imported through /dev/udmabuf and bound at a GPU address,
//!                     a submit queue, one command buffer of CP_NOPs run by the GPU, and its fence retired
//!   gpuctl large      probe, then buffers past udmabuf's limits, split the way xnuhost splits them: 96 MiB in one run
//!                     (two udmabufs, by size) and 1100 separate pages (two, by count); commands at the end of each are
//!                     run by the GPU, so the last udmabuf is bound where it belongs
//!   gpuctl            all of them, in this order; `routes` only reports there (its failures are not the run's)
//!   gpuctl amdgpu [NODE]  the Galaxy A56's path (an amdgpu render node, default /dev/dri/renderD128; not in the run of
//!                     all): xnuhost's start-time self-test (gpu3d_amdgpu/selftest.rs), the fork's raw steps (a udmabuf
//!                     imported, mapped, a GFX NOP IB run, its fence waited for), then the same through the amdgpu
//!                     renderer's own calls (capset, context, shmem, GEM_NEW and a blob of 3 udmabufs, BO_VA_OP,
//!                     CS_SUBMIT, the virtio fence retired); "gpu: self-test: PASS ..." lines, then "ok" or the FAIL line
//! It stands where virtio_gpu.rs stands in xnuhost: the same renderer (gpu3d_kgsl.rs), a memfd of its own as the guest's
//! RAM. Each step prints what it found; the last line is "ok" or the step that failed and why. Nothing is left behind:
//! the KGSL contexts and the memory go with the process.
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use xh_core::bytes::le;
use xh_core::sys::error_text;

use xnuhost::gpu3d::{
    self, Entry, Gpu3d, BLOB_FLAG_GUEST_HANDLE, BLOB_FLAG_MAPPABLE, BLOB_HOST3D, BLOB_HOST3D_GUEST, CAPSET_DRM,
};

const RAM_PHYS: u64 = 0x8_0000_0000; // any address: the "guest physical" base of the memfd
const RAM_BYTES: u64 = 256 << 20; // sparse: only the pages a step imports are ever allocated (about 110 MiB in `large`)
const POOL_BYTES: u64 = 8 << 20; // as xnuhost carves it (the session's GPU_POOL_BYTES)
const SHMEM_BYTES: u32 = 16384;

// The msm protocol a guest's Turnip speaks (virglrenderer src/drm/msm/msm_proto.h, drm-uapi/msm_drm.h), the few
// messages used here.
const CCMD_IOCTL_SIMPLE: u32 = 2;
const CCMD_GEM_NEW: u32 = 3;
const CCMD_GEM_SUBMIT: u32 = 7;
const IOCTL_GET_PARAM: u32 = 0xc018_6440; // DRM_IOWR(0x40 + nr, struct)
const IOCTL_SUBMITQUEUE_NEW: u32 = 0xc00c_644a;
const PIPE_3D0: u32 = 0x10;
const PARAM_GPU_ID: u32 = 1;
const PARAM_CHIP_ID: u32 = 3;
const PARAM_VA_START: u32 = 0x0e;
const PARAM_VA_SIZE: u32 = 0x0f;
const BO_WC: u32 = 0x0002_0000;
const BO_GUEST_ALLOC: u32 = 0x8000_0000;
const SUBMIT_CMD_BUF: u32 = 1;
const SUBMIT_BO_READ: u32 = 1;
const SUBMIT_FENCE_SN_IN: u32 = 0x0200_0000;

static VERBOSE: AtomicBool = AtomicBool::new(false);

fn log_line(level: u8, line: &str) {
    if level <= 1 || VERBOSE.load(Ordering::Relaxed) {
        println!("  {}{}", if level == 0 { "ERROR " } else { "" }, line);
    }
}

/// A device node, close-on-exec: read-write, or read only.
fn open(path: &str, write: bool) -> io::Result<OwnedFd> {
    std::fs::OpenOptions::new().read(true).write(write).open(path).map(OwnedFd::from)
}

/// An error as the steps print it: strerror's text and the errno, "Permission denied (13)".
fn why(e: &io::Error) -> String {
    format!("{} ({})", error_text(e), e.raw_os_error().unwrap_or(0))
}

/// A KGSL ioctl on its structure.
fn ioctl<T>(fd: BorrowedFd<'_>, request: u32, arg: &mut T) -> io::Result<()> {
    // SAFETY: each caller passes the structure its request encodes.
    unsafe { xh_core::ioctl::ioctl(fd.as_raw_fd(), request, arg) }.map(drop)
}

fn nodes() -> bool {
    let paths = ["/dev/kgsl-3d0", "/dev/udmabuf", "/dev/dma_heap/system"];
    let limits = ["/sys/module/udmabuf/parameters/list_limit", "/sys/module/udmabuf/parameters/size_limit_mb"];
    let mut bad = false;
    for (i, p) in paths.iter().enumerate() {
        let opened = open(p, i != 2);
        println!("{:<22} {}", p, opened.as_ref().map_or_else(error_text, |_| "opens".into()));
        bad |= opened.is_err();
    }
    for l in limits {
        // One udmabuf: at most list_limit runs of adjacent pages and size_limit_mb MiB. A blob past either is several.
        let text =
            std::fs::read_to_string(l).ok().and_then(|t| t.lines().next().map(str::to_owned)).unwrap_or_default();
        println!("{:<44} {}", l, if text.is_empty() { "(not readable)" } else { &text });
    }
    let (bytes, runs) = gpu3d::udmabuf_limits();
    println!("xnuhost splits a guest buffer into udmabufs of at most {} MiB and {} runs of pages", bytes >> 20, runs);
    bad
}

// ---- routes: KGSL by raw ioctls ---------------------------------------------------------------------------------------
// The structures and numbers of msm_kgsl.h used here (external/virglrenderer src/drm/drm2kgsl/msm_kgsl.h).
#[repr(C)]
#[derive(Default)]
struct KAlloc {
    size: u64,
    flags: u64,
    va_len: u64,
    mmapsize: u64,
    id: u32,
    metadata_len: u32,
    metadata: u64,
}
#[repr(C)]
#[derive(Default)]
struct KFree {
    flags: u64,
    private: u64,
    id: u32,
    kind: u32,
    len: u32,
}
#[repr(C)]
#[derive(Default)]
struct KInfo {
    gpuaddr: u64,
    flags: u64,
    size: u64,
    va_len: u64,
    va_addr: u64,
    id: u32,
}
#[repr(C)]
#[derive(Default)]
struct KImport {
    private: u64,
    priv_len: u64,
    flags: u64,
    kind: u32,
    id: u32,
}
#[repr(C)]
#[derive(Default)]
struct KRange {
    child_offset: u64,
    target_offset: u64,
    length: u64,
    child_id: u32,
    op: u32,
}
#[repr(C)]
#[derive(Default)]
struct KRanges {
    ranges: u64,
    nents: u32,
    size: u32,
    id: u32,
    flags: u32,
    fence_id: i32,
    padding: u32,
}
/// KGSL's ioctls: type 0x09.
const fn iow(nr: u8, size: usize) -> u32 {
    xh_core::ioctl::iow(0x09, nr, size)
}
const fn iowr(nr: u8, size: usize) -> u32 {
    xh_core::ioctl::iowr(0x09, nr, size)
}
const K_ALLOC: u32 = iowr(0x45, std::mem::size_of::<KAlloc>());
const K_FREE: u32 = iow(0x46, std::mem::size_of::<KFree>());
const K_INFO: u32 = iowr(0x47, std::mem::size_of::<KInfo>());
const K_IMPORT: u32 = iowr(0x48, std::mem::size_of::<KImport>());
const K_BIND: u32 = iowr(0x56, std::mem::size_of::<KRanges>());
const K_VBO: u64 = 1 << 34;
const K_VBO_NO_MAP_ZERO: u64 = 1 << 35; // the reservation alone, without the zero page mapped over it: quick
const K_IOCOHERENT: u64 = 1 << 31;
const ROUTE_BYTES: u64 = 1 << 20;

/// UDMABUF_CREATE (linux/udmabuf.h): `_IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: u32 = xh_core::ioctl::iow(b'u', 0x42, 24);
/// DMA_HEAP_IOCTL_ALLOC (linux/dma-heap.h): `_IOWR('H', 0, struct dma_heap_allocation_data)`.
const DMA_HEAP_ALLOC: u32 = xh_core::ioctl::iowr(b'H', 0, 24);

/// A VBO of `size` (KGSL_MEMFLAGS_VBO and `flags`): its id and GPU address.
fn k_vbo(kgsl: BorrowedFd<'_>, size: u64, flags: u64) -> io::Result<(u32, u64)> {
    let mut a = KAlloc { size, flags: K_VBO | flags, ..Default::default() };
    ioctl(kgsl, K_ALLOC, &mut a)?;
    let mut info = KInfo { id: a.id, ..Default::default() };
    ioctl(kgsl, K_INFO, &mut info)?;
    Ok((a.id, info.gpuaddr))
}

fn k_release(kgsl: BorrowedFd<'_>, id: u32) {
    let mut f = KFree { id, ..Default::default() };
    let _ = ioctl(kgsl, K_FREE, &mut f);
}

/// A dma-buf imported into KGSL: the object's id.
fn k_import(kgsl: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> io::Result<u32> {
    let fd: RawFd = dmabuf.as_raw_fd();
    let mut req = KImport {
        private: &fd as *const RawFd as u64,
        priv_len: 4,
        flags: K_IOCOHERENT,
        kind: 3, // KGSL_USER_MEM_TYPE_DMABUF
        ..Default::default()
    };
    ioctl(kgsl, K_IMPORT, &mut req)?;
    Ok(req.id)
}

/// A udmabuf of a sealed memfd's first `bytes` (the memfd is closed after: the dma-buf holds its pages).
fn a_udmabuf(bytes: u64) -> io::Result<OwnedFd> {
    let memfd = xh_core::fd::sealed_memfd(c"gpuctl-route", 0, bytes)?;
    let dev = open("/dev/udmabuf", true)?;
    // struct udmabuf_create { u32 memfd, flags; u64 offset, size }
    #[repr(C)]
    struct Create {
        memfd: u32,
        flags: u32,
        offset: u64,
        size: u64,
    }
    let mut c = Create { memfd: memfd.as_raw_fd() as u32, flags: 1, offset: 0, size: bytes };
    // SAFETY: UDMABUF_CREATE with its structure.
    let fd = unsafe { xh_core::ioctl::ioctl(dev.as_raw_fd(), UDMABUF_CREATE, &mut c) }?;
    // SAFETY: the new dma-buf's descriptor, ours.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A buffer of `bytes` from a DMA heap (`path`).
fn a_heap(path: &str, bytes: u64) -> io::Result<OwnedFd> {
    let heap = open(path, false)?;
    // struct dma_heap_allocation_data { u64 len; u32 fd, fd_flags; u64 heap_flags }
    #[repr(C)]
    struct Alloc {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    let mut a = Alloc { len: bytes, fd: 0, fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32, heap_flags: 0 };
    // SAFETY: DMA_HEAP_IOCTL_ALLOC with its structure.
    unsafe { xh_core::ioctl::ioctl(heap.as_raw_fd(), DMA_HEAP_ALLOC, &mut a) }?;
    // SAFETY: the buffer's new descriptor, ours.
    Ok(unsafe { OwnedFd::from_raw_fd(a.fd as RawFd) })
}

/// Where a route's memory comes from.
#[derive(Clone, Copy)]
enum Memory<'a> {
    /// a udmabuf of memfd pages
    Udmabuf,
    /// a DMA heap's buffer
    Heap(&'a str),
    /// KGSL's own GPUOBJ_ALLOC
    Kgsl,
}

/// One route: the memory, KGSL's object of it, its place in a VBO, the CPU's view of it. Whether the GPU side passed.
fn route(kgsl: BorrowedFd<'_>, vbo: u32, slot: u64, name: &str, memory: Memory<'_>) -> bool {
    let mut line = format!("  {name:<28}");
    let (mut dmabuf, mut id, mut ok) = (None::<OwnedFd>, 0u32, false);
    let fail = |line: &mut String, what: &str, e: &io::Error| *line += &format!("FAIL {}: {}", what, why(e));
    'out: {
        if let Memory::Kgsl = memory {
            let mut a = KAlloc { size: ROUTE_BYTES, ..Default::default() };
            if let Err(e) = ioctl(kgsl, K_ALLOC, &mut a) {
                fail(&mut line, "GPUOBJ_ALLOC", &e);
                break 'out;
            }
            id = a.id;
            line += "PASS alloc";
        } else {
            let (made, what) = match memory {
                Memory::Heap(path) => (a_heap(path, ROUTE_BYTES), "the heap's allocation"),
                _ => (a_udmabuf(ROUTE_BYTES), "UDMABUF_CREATE"),
            };
            let buf = match made {
                Ok(buf) => dmabuf.insert(buf),
                Err(e) => {
                    fail(&mut line, what, &e);
                    break 'out;
                }
            };
            match k_import(kgsl, buf.as_fd()) {
                Ok(i) => id = i,
                Err(e) => {
                    fail(&mut line, "GPUOBJ_IMPORT", &e);
                    break 'out;
                }
            }
            line += "PASS import";
        }
        let mut info = KInfo { id, ..Default::default() };
        if ioctl(kgsl, K_INFO, &mut info).is_ok() {
            line += &format!(" (id {}, GPU address 0x{:x}, {} KiB)", id, info.gpuaddr, info.size >> 10);
        }
        let mut range = KRange {
            target_offset: slot * 2 * ROUTE_BYTES,
            length: ROUTE_BYTES,
            child_id: id,
            op: 1,
            ..Default::default()
        };
        let mut bind = KRanges {
            ranges: &mut range as *mut KRange as u64,
            nents: 1,
            size: std::mem::size_of::<KRange>() as u32,
            id: vbo,
            ..Default::default()
        };
        if vbo == 0 {
            line += ", no VBO to bind in";
        } else if let Err(e) = ioctl(kgsl, K_BIND, &mut bind) {
            line += &format!(", FAIL bind in a VBO: {}", why(&e));
        } else {
            line += ", PASS bind in a VBO";
            ok = true;
            range.op = 2; // the same range, unbound
            bind.ranges = &mut range as *mut KRange as u64;
            let _ = ioctl(kgsl, K_BIND, &mut bind);
        }
        // KGSL's own object is mapped through KGSL at its id; a dma-buf through itself.
        let (fd, offset) = match &dmabuf {
            Some(buf) => (buf.as_raw_fd(), 0),
            None => (kgsl.as_raw_fd(), (id as i64) << 12),
        };
        // SAFETY: maps the object (or dma-buf) of ROUTE_BYTES; unmapped below.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ROUTE_BYTES as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if at == libc::MAP_FAILED {
            line += &format!(", FAIL mmap: {}", why(&io::Error::last_os_error()));
        } else {
            let words = at as *mut u32;
            let last = (ROUTE_BYTES / 4 - 1) as usize;
            // SAFETY: inside the map.
            let good = unsafe {
                words.write_volatile(0x6f6e_6569);
                words.add(last).write_volatile(0x6965_6e6f);
                words.read_volatile() == 0x6f6e_6569 && words.add(last).read_volatile() == 0x6965_6e6f
            };
            line += if good { ", PASS CPU mapping" } else { ", FAIL CPU mapping reads back wrong" };
            // SAFETY: the map made above.
            unsafe { libc::munmap(at, ROUTE_BYTES as usize) };
        }
    }
    println!("{line}");
    if id != 0 {
        k_release(kgsl, id);
    }
    drop(dmabuf); // after KGSL let go of its object
    ok
}

/// In a child: KGSL keeps a process's objects and GPU addresses by process, and the steps after this want them
/// untouched. (gpuctl has no other thread yet: the renderer is opened after the routes.)
fn in_child(run: fn(u64) -> i32, arg: u64) -> i32 {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // SAFETY: fork of a process with one thread; the child only runs `run` and exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return -1;
    }
    if pid == 0 {
        let r = run(arg);
        let _ = std::io::stdout().flush();
        // SAFETY: the child's end.
        unsafe { libc::_exit(r) };
    }
    let mut status = 0;
    // SAFETY: waits for our child.
    if unsafe { libc::waitpid(pid, &mut status, 0) } != pid || !libc::WIFEXITED(status) {
        println!("  (the step's process ended abnormally, status 0x{status:x})");
        return -1;
    }
    libc::WEXITSTATUS(status)
}

fn routes_child(_: u64) -> i32 {
    let kgsl = match open("/dev/kgsl-3d0", true) {
        Ok(fd) => fd,
        Err(e) => {
            println!("  /dev/kgsl-3d0: {}", error_text(&e));
            return 3;
        }
    };
    let kgsl = kgsl.as_fd();
    let vbo = match k_vbo(kgsl, 4 << 30, 0) {
        Ok((id, base)) => {
            println!("  a 4 GiB VBO at GPU address 0x{base:x} to bind in");
            id
        }
        Err(e) => {
            println!("  a 4 GiB VBO (KGSL_MEMFLAGS_VBO): {}: nothing can be bound at a guest's address", why(&e));
            0
        }
    };
    let udmabuf = route(kgsl, vbo, 0, "udmabuf of memfd pages", Memory::Udmabuf);
    route(kgsl, vbo, 1, "/dev/dma_heap/system", Memory::Heap("/dev/dma_heap/system"));
    route(kgsl, vbo, 2, "/dev/dma_heap/qcom,system", Memory::Heap("/dev/dma_heap/qcom,system"));
    route(kgsl, vbo, 3, "KGSL GPUOBJ_ALLOC + mmap", Memory::Kgsl);
    if udmabuf {
        0
    } else {
        1
    }
}

/// A VBO of `size` where drm2kgsl reserves its own, then one import beside it. 0: imported; 1: the import failed; 2: no
/// such VBO.
fn room_child(size: u64) -> i32 {
    // Both are tried, in this order; the error said is the udmabuf's when both fail.
    let (kgsl, dmabuf) = match (open("/dev/kgsl-3d0", true), a_udmabuf(65536)) {
        (Ok(kgsl), Ok(dmabuf)) => (kgsl, dmabuf),
        (_, Err(e)) | (Err(e), Ok(_)) => {
            println!("  VBO of {:4} GiB: not tried: {}", size >> 30, error_text(&e));
            return 2;
        }
    };
    let base = match k_vbo(kgsl.as_fd(), size, K_VBO_NO_MAP_ZERO) {
        Ok((_, base)) => base,
        Err(e) => {
            println!("  VBO of {:4} GiB: not granted: {}", size >> 30, why(&e));
            return 2;
        }
    };
    let id = match k_import(kgsl.as_fd(), dmabuf.as_fd()) {
        Ok(id) => id,
        Err(e) => {
            println!(
                "  VBO of {:4} GiB at 0x{:x}: granted; a udmabuf import beside it FAIL: {}{}",
                size >> 30,
                base,
                why(&e),
                if e.raw_os_error() == Some(libc::ENOMEM) {
                    ": the VBO leaves KGSL no GPU address for the object"
                } else {
                    ""
                }
            );
            return 1;
        }
    };
    let mut info = KInfo { id, ..Default::default() };
    let _ = ioctl(kgsl.as_fd(), K_INFO, &mut info);
    println!(
        "  VBO of {:4} GiB at 0x{:x}: granted; a udmabuf import beside it PASS (its own GPU address 0x{:x})",
        size >> 30,
        base,
        info.gpuaddr
    );
    0
}

fn routes(alone: bool) -> bool {
    let ladder: [u64; 7] = [1024 << 30, 256 << 30, 192 << 30, 128 << 30, 64 << 30, 16 << 30, 4 << 30]; // drm2kgsl's
    println!("routes: how memory reaches KGSL on this kernel (raw ioctls, {} KiB each)", ROUTE_BYTES >> 10);
    let memory = in_child(routes_child, 0);
    if memory == 3 {
        println!("{}: no KGSL device", if alone { "FAILED at: routes" } else { "routes: FAIL" });
        return true;
    }
    println!("address room: KGSL gives every object a GPU address of its own, from the range a VBO is reserved in");
    let (mut fits, mut filled) = (0u64, 0u64);
    for &size in &ladder {
        if fits != 0 {
            break;
        }
        match in_child(room_child, size) {
            0 => fits = size,
            1 if filled == 0 => filled = size,
            _ => {}
        }
    }
    if fits != 0 && filled != 0 {
        println!(
            "routes: a {} GiB VBO fills KGSL's address range; drm2kgsl must reserve {} GiB or less (this build of drm2kgsl checks for that room)",
            filled >> 30,
            fits >> 30
        );
    } else if fits != 0 {
        println!("routes: the largest VBO still leaves room for imports ({} GiB)", fits >> 30);
    } else {
        println!("routes: no VBO size leaves room for an import");
    }
    // "FAILED at:" is the run's verdict (the fork reads it): only when this step is the run.
    let verdict = if alone { "FAILED at: routes" } else { "routes: FAIL" };
    if memory != 0 {
        println!(
            "{verdict}: a udmabuf of memfd pages does not reach KGSL (see the lines above for the routes that do)"
        );
        return true;
    }
    if fits == 0 {
        println!("{verdict}: no VBO with room for imports");
        return true;
    }
    false
}

// ---- the renderer ---------------------------------------------------------------------------------------------------
/// The newest fence retired, and a wait for it.
struct Retired {
    fence: Mutex<u64>,
    cond: Condvar,
}

static RETIRED: Retired = Retired { fence: Mutex::new(0), cond: Condvar::new() };

/// What a step failed at: "FAILED at: <what>: <why>".
struct Failed;

fn step(what: &str, ok: bool, why: impl FnOnce() -> String) -> Result<(), Failed> {
    if ok {
        return Ok(());
    }
    println!("FAILED at: {}: {}", what, why());
    Err(Failed)
}

/// A step that gives a value: the value, or "FAILED at: <what>: <the error's text>".
fn check<T>(what: &str, r: io::Result<T>) -> Result<T, Failed> {
    r.map_err(|e| {
        println!("FAILED at: {}: {}", what, error_text(&e));
        Failed
    })
}

struct Gpu {
    gpu: Box<dyn Gpu3d>,
    /// the RAM memfd the renderer imports from (after `gpu`: dropped after it)
    _ram_fd: OwnedFd,
    ram: *mut u8,
    shmem: *mut u8,
    seqno: u32,
}

/// A request: its bytes, little-endian, whole words.
struct Req(Vec<u8>);

impl Req {
    /// vdrm_ccmd_req: cmd, len, seqno, rsp_off (filled at the submit)
    fn new(cmd: u32) -> Req {
        let mut r = Req(Vec::new());
        r.u32(cmd).u32(0).u32(0).u32(0);
        r
    }
    fn u32(&mut self, v: u32) -> &mut Req {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Req {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
}

impl Gpu {
    fn shmem_u32(&self, i: usize) -> u32 {
        // SAFETY: the shmem lies in the pool, mapped for the process's life.
        unsafe { (self.shmem.add(i * 4) as *const u32).read_volatile() }
    }

    fn rsp(&self) -> *mut u8 {
        // SAFETY: rsp_mem_offset lies inside the shmem (checked when the context was opened).
        unsafe { self.shmem.add(self.shmem_u32(1) as usize) }
    }

    /// One request of the context; when `rsp` is given, its response (the guest says how long it may be, at rsp_off).
    fn ccmd(&mut self, ctx: u32, mut req: Req, rsp: Option<&mut [u8]>) -> Result<(), i32> {
        self.seqno += 1;
        let len = req.0.len() as u32;
        req.0[4..8].copy_from_slice(&len.to_le_bytes());
        req.0[8..12].copy_from_slice(&self.seqno.to_le_bytes());
        let rsp_mem = self.rsp();
        if let Some(r) = rsp.as_ref() {
            // SAFETY: inside the shmem's response area.
            unsafe { (rsp_mem as *mut u32).write_unaligned(r.len() as u32) };
        }
        let words: Vec<u32> = req.0.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
        // SAFETY: the words' bytes.
        let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4) };
        self.gpu.submit(ctx, bytes).map_err(|e| -e.0)?;
        if self.shmem_u32(0) != self.seqno {
            return Err(-libc::EIO);
        }
        if let Some(r) = rsp {
            // SAFETY: inside the response area, r.len() bytes.
            unsafe { std::ptr::copy_nonoverlapping(rsp_mem, r.as_mut_ptr(), r.len()) };
        }
        Ok(())
    }

    fn ioctl_simple(&mut self, ctx: u32, nr: u32, payload: &mut [u8]) -> Result<i32, i32> {
        let mut req = Req::new(CCMD_IOCTL_SIMPLE);
        req.u32(nr);
        req.0.extend_from_slice(payload);
        while req.0.len() % 4 != 0 {
            req.0.push(0);
        }
        let mut rsp = vec![0u8; 8 + payload.len()];
        self.ccmd(ctx, req, Some(&mut rsp))?;
        payload.copy_from_slice(&rsp[8..]);
        Ok(le::i32(&rsp, 4))
    }

    fn get_param(&mut self, ctx: u32, param: u32) -> Result<u64, i32> {
        let mut p = [0u8; 24]; // pipe, param, value, len, pad
        p[0..4].copy_from_slice(&PIPE_3D0.to_le_bytes());
        p[4..8].copy_from_slice(&param.to_le_bytes());
        let r = self.ioctl_simple(ctx, IOCTL_GET_PARAM, &mut p)?;
        let value = le::u64(&p, 8);
        if r != 0 {
            Err(r)
        } else {
            Ok(value)
        }
    }
}

fn parity(v: u32) -> u32 {
    let x = v ^ (v >> 4) ^ (v >> 8) ^ (v >> 12) ^ (v >> 16) ^ (v >> 20) ^ (v >> 24) ^ (v >> 28);
    (0x9669 >> (0xf & x)) & 1
}

fn pkt7(opcode: u32, count: u32) -> u32 {
    7 << 28 | count | parity(count) << 15 | (opcode & 0x7f) << 16 | parity(opcode) << 23
}

fn open_renderer() -> Result<Gpu, Failed> {
    // The guest's RAM: sealed against shrinking, as xnuhost's (udmabuf takes no other memfd).
    let fd = check("the RAM memfd", xh_core::fd::sealed_memfd(c"gpuctl-ram", 0, RAM_BYTES))?;
    // SAFETY: a shared map of our memfd, kept for the process's life.
    let ram = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            RAM_BYTES as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    step("mapping the RAM", ram != libc::MAP_FAILED, || error_text(&io::Error::last_os_error()))?;
    let ram = ram as *mut u8;
    let pool = RAM_BYTES - POOL_BYTES;
    // SAFETY: inside the map.
    let host = unsafe { ram.add(pool as usize) };
    let gpu = match gpu3d::kgsl_open(fd.as_raw_fd(), RAM_PHYS, RAM_BYTES, host, RAM_PHYS + pool, POOL_BYTES, log_line) {
        Ok(g) => g,
        Err(why) => {
            println!("FAILED at: opening the renderer: {why}");
            return Err(Failed);
        }
    };
    gpu.set_retire(Arc::new(|_ctx, _ring, fence| {
        let mut f = RETIRED.fence.lock().unwrap();
        if fence > *f {
            *f = fence;
        }
        RETIRED.cond.notify_all();
    }));
    let mut bytes = [0u8; 4096];
    let (id, version, size) = gpu.capset_info(0).unwrap_or((0, 0, 0));
    // virgl_renderer_capset_drm, its msm part: wire_format, major, minor, patch, context_type, pad, has_cached_coherent,
    // priorities; va_start, va_size; gpu_id, gmem_size; gmem_base, chip_id
    let ok = gpu.capset_count() == 1
        && id == CAPSET_DRM
        && size >= 72
        && gpu.capset(id, version, &mut bytes) == size as usize;
    step("the capset", ok, || format!("id {id} size {size}"))?;
    let w = |i: usize| le::u32(&bytes, i * 4);
    let q = |at: usize| le::u64(&bytes, at);
    let (va_start, va_size, chip_id) = (q(32), q(40), q(64));
    println!(
        "capset: DRM (6), {} bytes: wire format {}, msm {}.{}, context type {}, chip 0x{:x}, gpu id {}, GPU addresses 0x{:x} + {} GiB, gmem {} KiB, priorities {}",
        size,
        w(0),
        w(1),
        w(2),
        w(4),
        chip_id,
        w(12),
        va_start,
        va_size >> 30,
        w(13) >> 10,
        w(7)
    );
    step("the capset's content", w(4) == 1 && va_size != 0 && chip_id != 0, || {
        "not an msm context or no GPU address range".into()
    })?;
    Ok(Gpu { gpu, _ram_fd: fd, ram, shmem: std::ptr::null_mut(), seqno: 0 })
}

/// A context as a guest opens one: its response buffer from the pool, GET_PARAM answered there, a submit queue. Its
/// GPU addresses' start and the queue.
fn context_open(g: &mut Gpu, ctx: u32, shmem_res: u32) -> Result<(u64, u32), Failed> {
    g.seqno = 0;
    *RETIRED.fence.lock().unwrap() = 0;
    let r = g.gpu.ctx_create(ctx, CAPSET_DRM, b"gpuctl");
    step("the context", r.is_ok(), || {
        format!("{} (a KGSL context needs /dev/kgsl-3d0 and /dev/dma_heap/system)", r.err().unwrap())
    })?;
    let r = g.gpu.create_blob(ctx, shmem_res, BLOB_HOST3D, BLOB_FLAG_MAPPABLE, 0, SHMEM_BYTES as u64, &[]);
    step("the response buffer (HOST3D blob 0)", r.is_ok(), || r.err().unwrap().to_string())?;
    let m = g.gpu.map_blob(shmem_res);
    let (offset, info) = m.unwrap_or((0, 0));
    step("mapping it from the pool", m.is_ok() && offset + SHMEM_BYTES as u64 <= POOL_BYTES, || {
        format!("{}, offset 0x{:x}", m.err().map_or("Success".to_owned(), |e| e.to_string()), offset)
    })?;
    // SAFETY: the pool's blob, inside the map.
    g.shmem = unsafe { g.ram.add((RAM_BYTES - POOL_BYTES + offset) as usize) };
    let rsp_at = g.shmem_u32(1);
    println!(
        "context {ctx}: response buffer at pool offset 0x{offset:x} (cache mode {info}), responses from byte {rsp_at}"
    );
    step("the response buffer's header", (8..SHMEM_BYTES).contains(&rsp_at), || format!("offset {rsp_at}"))?;

    let r = g.get_param(ctx, PARAM_GPU_ID);
    step("GET_PARAM(GPU_ID)", r.is_ok_and(|v| v != 0), || {
        format!("{}, value {}", r.err().unwrap_or(0), r.unwrap_or(0))
    })?;
    print!("GET_PARAM: gpu id {}", r.unwrap());
    let r = g.get_param(ctx, PARAM_CHIP_ID);
    step("GET_PARAM(CHIP_ID)", r.is_ok(), || format!("{}", r.err().unwrap()))?;
    print!(", chip 0x{:x}", r.unwrap());
    let start = g.get_param(ctx, PARAM_VA_START);
    let size = start.and_then(|_| g.get_param(ctx, PARAM_VA_SIZE));
    step("GET_PARAM(VA_START, VA_SIZE)", size.is_ok_and(|s| s >= 1 << 30), || format!("{}", size.err().unwrap_or(0)))?;
    let va_start = start.unwrap();
    println!(", this context's GPU addresses 0x{:x} + {} GiB", va_start, size.unwrap() >> 30);
    let mut queue = [0u8; 12]; // flags, prio, id
    let r = g.ioctl_simple(ctx, IOCTL_SUBMITQUEUE_NEW, &mut queue);
    step("SUBMITQUEUE_NEW (a KGSL draw context)", r == Ok(0), || format!("{}", r.unwrap_or_else(|e| e)))?;
    Ok((va_start, le::u32(&queue, 8)))
}

/// A buffer object as a guest makes one: its own pages, announced with GEM_NEW at a GPU address of the context's
/// range, then given as a HOST3D_GUEST blob: udmabuf (one or several), KGSL import, bind at the address.
fn guest_buffer(
    g: &mut Gpu,
    ctx: u32,
    res: u32,
    blob: u32,
    iova: u64,
    bytes: u64,
    pages: &[Entry],
) -> Result<(), Failed> {
    let mut gem = Req::new(CCMD_GEM_NEW);
    gem.u64(iova).u64(bytes).u32(BO_WC | BO_GUEST_ALLOC).u32(blob);
    let r = g.ccmd(ctx, gem, None);
    let errors = g.shmem_u32(2);
    step("GEM_NEW (guest-allocated)", r.is_ok() && errors == 0, || {
        format!("{}, async errors {}", r.err().unwrap_or(0), errors)
    })?;
    let r = g.gpu.create_blob(ctx, res, BLOB_HOST3D_GUEST, BLOB_FLAG_GUEST_HANDLE, blob as u64, bytes, pages);
    step("the buffer's pages (udmabuf -> KGSL import -> bind)", r.is_ok(), || {
        format!("{} KiB in {} segments: {}", bytes >> 10, pages.len(), r.err().unwrap())
    })
}

/// 32 CP_NOPs at `offset` of the buffer `res` (bound at `iova`, its bytes at `words`), run by the GPU; its fence
/// retired.
#[allow(clippy::too_many_arguments)]
fn run_nops(
    g: &mut Gpu,
    ctx: u32,
    queue: u32,
    res: u32,
    iova: u64,
    offset: u32,
    words: *mut u32,
    fence: u32,
) -> Result<(), Failed> {
    for i in (0..64).step_by(2) {
        // SAFETY: 64 words inside the guest RAM's map.
        unsafe {
            words.add(i).write_volatile(pkt7(0x10 /* CP_NOP */, 1));
            words.add(i + 1).write_volatile(0x6f6e_6569);
        }
    }
    let mut job = Req::new(CCMD_GEM_SUBMIT);
    job.u32(PIPE_3D0 | SUBMIT_FENCE_SN_IN).u32(queue).u32(1).u32(1).u32(fence); // flags, queue_id, nr_bos, nr_cmds, fence
    job.u32(SUBMIT_BO_READ).u32(res).u64(iova); // the bo: flags, handle, presumed
    job.u32(SUBMIT_CMD_BUF).u32(0).u32(offset).u32(64 * 4).u32(0).u32(0).u64(0); // the cmd
    let before = g.get_param(ctx, 0x09 /* FAULTS */);
    let start = Instant::now();
    let r = g.ccmd(ctx, job, None);
    let errors = g.shmem_u32(2);
    step("GEM_SUBMIT (32 CP_NOPs)", r.is_ok() && errors == 0, || {
        format!("{}, async errors {}: KGSL refused the command", r.err().unwrap_or(0), errors)
    })?;
    let r = g.gpu.submit_fence(ctx, 1, fence as u64); // the queue's ring: priority 0 is ring 1
    step("the fence", r.is_ok(), || r.err().unwrap().to_string())?;
    let until = Instant::now() + Duration::from_secs(5);
    let mut done = RETIRED.fence.lock().unwrap();
    while *done < fence as u64 && Instant::now() < until {
        done = RETIRED.cond.wait_timeout(done, until - Instant::now()).unwrap().0;
    }
    let retired = *done >= fence as u64;
    drop(done);
    let took = start.elapsed();
    step("the fence retiring", retired, || {
        "not within 5 s: the GPU did not run the command, or KGSL did not report it".into()
    })?;
    let faults = g.get_param(ctx, 0x09 /* FAULTS */);
    let errors = g.shmem_u32(2);
    println!(
        "submit: queue {}, 256 bytes of commands at 0x{:x} run by the GPU, fence retired after {:.2} ms; GPU faults {}, async errors {}",
        queue,
        iova + offset as u64,
        took.as_secs_f64() * 1e3,
        faults.unwrap_or(0),
        errors
    );
    let ok = errors == 0 && (faults.is_err() || before.is_err() || faults == before);
    step("no fault", ok, || {
        format!(
            "async errors {}, GPU faults {} ({} before): the GPU did not find the commands at that address",
            errors,
            faults.unwrap_or(0),
            before.unwrap_or(0)
        )
    })
}

fn ram_words(g: &Gpu, offset: u64) -> *mut u32 {
    // SAFETY: inside the guest RAM's map.
    unsafe { g.ram.add(offset as usize) as *mut u32 }
}

fn submit(g: &mut Gpu) -> Result<(), Failed> {
    let (ctx, shmem_res, bo_res, bo_bytes) = (1, 1, 2, 0x4000u32);
    let (va_start, queue) = context_open(g, ctx, shmem_res)?;
    // 16 KiB at 1 MiB of the "RAM".
    let iova = va_start + (16 << 20);
    guest_buffer(g, ctx, bo_res, 1, iova, bo_bytes as u64, &[Entry::new(RAM_PHYS + (1 << 20), bo_bytes)])?;
    println!("buffer: 16 KiB of guest pages imported through /dev/udmabuf, bound at GPU address 0x{iova:x}");
    let words = ram_words(g, 1 << 20);
    run_nops(g, ctx, queue, bo_res, iova, 0, words, 1)?;
    g.gpu.unref(bo_res);
    g.gpu.unref(shmem_res);
    g.gpu.ctx_destroy(ctx);
    Ok(())
}

/// Buffers one udmabuf cannot hold (64 MiB, 1024 runs of pages on this phone): several, bound in a row.
fn large(g: &mut Gpu) -> Result<(), Failed> {
    const PAGES: u64 = 1100;
    let (ctx, shmem_res, big_res, many_res) = (2, 11, 12, 13);
    let (big_bytes, big_at, many_at, many_bytes) = (96u64 << 20, 16u64 << 20, 120u64 << 20, PAGES * 4096);
    let (limit, runs) = gpu3d::udmabuf_limits();
    let (va_start, queue) = context_open(g, ctx, shmem_res)?;

    // 96 MiB in one run of pages: more than one udmabuf's bytes.
    let big_iova = va_start + (256 << 20);
    guest_buffer(g, ctx, big_res, 1, big_iova, big_bytes, &[Entry::new(RAM_PHYS + big_at, big_bytes as u32)])?;
    println!(
        "large: {} MiB in 1 run of pages as {} udmabufs of up to {} MiB, bound at GPU address 0x{:x}",
        big_bytes >> 20,
        big_bytes.div_ceil(limit),
        limit >> 20,
        big_iova
    );
    // Commands in its first udmabuf, then in its last 64 KiB: the last udmabuf is bound after the others.
    let w = ram_words(g, big_at);
    run_nops(g, ctx, queue, big_res, big_iova, 0, w, 1)?;
    let w = ram_words(g, big_at + big_bytes - 65536);
    run_nops(g, ctx, queue, big_res, big_iova, (big_bytes - 65536) as u32, w, 2)?;

    // 1100 pages, every second page of a range: more than one udmabuf's runs.
    let pages: Vec<Entry> = (0..PAGES).map(|i| Entry::new(RAM_PHYS + many_at + i * 8192, 4096)).collect();
    let many_iova = va_start + (512 << 20);
    guest_buffer(g, ctx, many_res, 2, many_iova, many_bytes, &pages)?;
    println!(
        "large: {} KiB in {} separate pages as {} udmabufs of up to {} runs, bound at GPU address 0x{:x}",
        many_bytes >> 10,
        PAGES,
        PAGES.div_ceil(runs as u64),
        runs,
        many_iova
    );
    // Commands in its last page: page 1099 of the buffer, the 76th of its second udmabuf.
    let w = ram_words(g, many_at + (PAGES - 1) * 8192);
    run_nops(g, ctx, queue, many_res, many_iova, ((PAGES - 1) * 4096) as u32, w, 3)?;
    g.gpu.unref(many_res);
    g.gpu.unref(big_res);
    g.gpu.unref(shmem_res);
    g.gpu.ctx_destroy(ctx);
    Ok(())
}

/// `gpuctl amdgpu [NODE] [-v]`: the amdgpu self-test xnuhost runs at the start; the exit status.
fn amdgpu(args: &[String]) -> i32 {
    use xnuhost::gpu3d_amdgpu as amd;
    VERBOSE.store(args.iter().any(|a| a == "-v"), Ordering::Relaxed);
    let node = args.iter().find(|a| *a != "-v").map_or(amd::DEFAULT_NODE, String::as_str);
    let sys: Arc<dyn amd::Dev> = Arc::new(amd::System);
    match amd::node_check(&*sys, node) {
        Ok(driver) => println!("{node}: {driver}"),
        Err(why) => {
            println!("FAILED at: the render node: {why}");
            return 1;
        }
    }
    match amd::selftest::run(sys, node, log_line) {
        Ok(()) => {
            println!("ok");
            0
        }
        Err(line) => {
            println!("FAILED at: {line}");
            1
        }
    }
}

/// `gpuctl addon FILE [BUILD]`: whether a gpuaddon (the kernel's `kernel.kc.gpuaddon`, or `oneios-tool ios gpuaddon`'s
/// output) carries the Galaxy A56's Metal plug-in as xnuhost's GPU choice reads it (gpu3d_amdgpu::plugin_in_addon), and
/// for which image build; with BUILD (e.g. 20F66) its oneios-target must name it. Reads the file only; the exit status.
fn addon(args: &[String]) -> i32 {
    use xnuhost::gpu3d_amdgpu as amd;
    let Some(path) = args.first() else {
        eprintln!("usage: gpuctl addon FILE [BUILD]");
        return 2;
    };
    let blob = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!("FAILED at: {path}: {e}");
            return 1;
        }
    };
    match amd::plugin_in_addon(Some(&blob), args.get(1).map(String::as_str)) {
        Ok(p) => {
            println!("{path}: the {} plug-in, {} files, oneios-target \"{}\"", amd::PLUGIN, p.files, p.target);
            println!("ok");
            0
        }
        Err(why) => {
            println!("FAILED at: {path}: {why}");
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut what = args.get(1).map_or("all", String::as_str);
    VERBOSE.store(args.get(2).is_some_and(|a| a == "-v"), Ordering::Relaxed);
    if what == "-v" {
        VERBOSE.store(true, Ordering::Relaxed);
        what = "all";
    }
    if what == "amdgpu" {
        std::process::exit(amdgpu(&args[2..]));
    }
    if what == "addon" {
        std::process::exit(addon(&args[2..]));
    }
    let all = what == "all";
    if !all && !["nodes", "routes", "probe", "submit", "large"].contains(&what) {
        eprintln!("usage: gpuctl [nodes|routes|probe|submit|large] [-v]\n       gpuctl amdgpu [NODE] [-v]\n       gpuctl addon FILE [BUILD]");
        std::process::exit(2);
    }
    if all || what == "nodes" {
        let bad = nodes();
        if !all {
            println!("{}", if bad { "FAILED at: the device nodes" } else { "ok" });
            std::process::exit(bad as i32);
        }
    }
    // Before the renderer: its threads and its KGSL state are not for a fork. Alone, its result is the run's; in a
    // whole run it only reports.
    if all || what == "routes" {
        let bad = routes(!all);
        if !all {
            if !bad {
                println!("ok");
            }
            std::process::exit(bad as i32);
        }
    }
    let Ok(mut g) = open_renderer() else { std::process::exit(1) };
    if (all || what == "submit") && submit(&mut g).is_err() {
        std::process::exit(1);
    }
    if (all || what == "large") && large(&mut g).is_err() {
        std::process::exit(1);
    }
    println!("ok");
}
