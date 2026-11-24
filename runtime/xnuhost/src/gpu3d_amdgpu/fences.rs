// SPDX-License-Identifier: MIT
//! The amdgpu renderer's virtio fences. A fence on a context's ring retires when the GPU work submitted on that ring
//! before it is done: the ring's last CS_SUBMIT left its fence in the ring's syncobj, exported as a sync_file when the
//! guest asks for the virtio fence (virglrenderer's drm_timeline does the same with one thread per ring). Here one
//! thread serves every ring of every context: it blocks in poll(2) on the oldest sync_file of each ring that has one,
//! and on an eventfd that says the set changed. No timeout, no sleep: it wakes only when a sync_file signals or a
//! fence is added or a context goes. A fence without a sync_file (no GPU work since the ring's last fence) retires with
//! the fences before it, in order; with none before it, at once, in the caller's thread (the Gpu3d contract allows
//! that). Not a realtime thread: it inherits xnuhost's policy.
use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use xh_core::thread::lock;

use crate::gpu3d::Retire;

struct Fence {
    fence: u64,
    fd: Option<OwnedFd>,
    at_us: u64,
}

#[derive(Default)]
struct Rings {
    queues: HashMap<(u32, u32), VecDeque<Fence>>,
    stop: bool,
}

/// The figures of the 10 s line: fences retired, the sum and the most of their time from the guest's fence to the
/// retirement (microseconds).
#[derive(Default)]
pub struct Figures {
    pub retired: AtomicU64,
    pub sum_us: AtomicU64,
    pub max_us: AtomicU64,
}

struct Shared {
    rings: Mutex<Rings>,
    retire: Mutex<Option<Retire>>,
    wake: OwnedFd,
    figures: Figures,
}

impl Shared {
    fn kick(&self) {
        let one = 1u64;
        // SAFETY: writes 8 bytes from our u64 to our eventfd.
        let _ = unsafe { libc::write(self.wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    fn retire(&self, ctx: u32, ring: u32, fence: u64, took_us: u64) {
        let f = &self.figures;
        f.retired.fetch_add(1, Ordering::Relaxed);
        f.sum_us.fetch_add(took_us, Ordering::Relaxed);
        f.max_us.fetch_max(took_us, Ordering::Relaxed);
        let r = lock(&self.retire).clone();
        if let Some(r) = r {
            r(ctx, ring, fence);
        }
    }
}

fn now_us() -> u64 {
    xh_core::time::monotonic_ns() / 1000
}

/// The waiter: its thread runs until it is dropped.
pub struct Waiter {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Waiter {
    pub fn new() -> Result<Waiter, String> {
        // SAFETY: a new eventfd of ours.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(format!("eventfd: {}", xh_core::sys::error_text(&std::io::Error::last_os_error())));
        }
        let shared = Arc::new(Shared {
            rings: Mutex::new(Rings::default()),
            retire: Mutex::new(None),
            // SAFETY: as above.
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            figures: Figures::default(),
        });
        let s = shared.clone();
        let thread = xh_core::thread::spawn("amdgpu-fences", move || run(&s))
            .map_err(|e| format!("the fence thread: {}", xh_core::sys::error_text(&e)))?;
        Ok(Waiter { shared, thread: Some(thread) })
    }

    pub fn set_retire(&self, retire: Retire) {
        *lock(&self.shared.retire) = Some(retire);
    }

    pub fn figures(&self) -> &Figures {
        &self.shared.figures
    }

    /// The guest's fence `fence` on `ring` of `ctx`: retired when `fd` (the ring's last work, a sync_file) signals,
    /// or with the fences before it when there is no `fd`.
    pub fn add(&self, ctx: u32, ring: u32, fence: u64, fd: Option<OwnedFd>) {
        {
            let mut g = lock(&self.shared.rings);
            let q = g.queues.entry((ctx, ring)).or_default();
            if !q.is_empty() || fd.is_some() {
                q.push_back(Fence { fence, fd, at_us: now_us() });
                drop(g);
                self.shared.kick();
                return;
            }
        }
        self.shared.retire(ctx, ring, fence, 0); // nothing before it on the ring
    }

    /// The fences of `ctx` that have not retired: they never will (the context is gone; the device drops them).
    pub fn forget(&self, ctx: u32) {
        lock(&self.shared.rings).queues.retain(|&(c, _), _| c != ctx);
        self.shared.kick();
    }

    /// How many fences wait (the tests).
    pub fn pending(&self) -> usize {
        lock(&self.shared.rings).queues.values().map(VecDeque::len).sum()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        lock(&self.shared.rings).stop = true;
        self.shared.kick();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(s: &Shared) {
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let mut keys: Vec<(u32, u32, u64)> = Vec::new();
    let mut done: Vec<(u32, u32, u64, u64)> = Vec::new();
    loop {
        {
            let mut g = lock(&s.rings);
            if g.stop {
                return;
            }
            // Each ring's leading fences without a sync_file retire now (their work was the fence before them).
            fds.clear();
            keys.clear();
            fds.push(libc::pollfd { fd: s.wake.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            let now = now_us();
            for (&(ctx, ring), q) in g.queues.iter_mut() {
                while q.front().is_some_and(|f| f.fd.is_none()) {
                    let f = q.pop_front().expect("a fence");
                    done.push((ctx, ring, f.fence, now.saturating_sub(f.at_us)));
                }
                if let Some(f) = q.front() {
                    let fd = f.fd.as_ref().expect("a sync_file").as_raw_fd();
                    fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
                    keys.push((ctx, ring, f.fence));
                }
            }
            g.queues.retain(|_, q| !q.is_empty());
        }
        for (ctx, ring, fence, took) in done.drain(..) {
            s.retire(ctx, ring, fence, took);
        }
        // SAFETY: polls our array of pollfds, its length given.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            continue; // EINTR
        }
        if fds[0].revents != 0 {
            let mut v = 0u64;
            // SAFETY: reads the eventfd's 8-byte count into our u64 (non-blocking).
            let _ = unsafe { libc::read(s.wake.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
        }
        let now = now_us();
        let mut g = lock(&s.rings);
        for (p, &(ctx, ring, fence)) in fds[1..].iter().zip(&keys) {
            if p.revents == 0 {
                continue;
            }
            // Signalled (POLLIN), or the sync_file failed (POLLERR): either way the work is over.
            let Some(q) = g.queues.get_mut(&(ctx, ring)) else { continue }; // the context went meanwhile
            if q.front().is_some_and(|f| f.fence == fence) {
                let f = q.pop_front().expect("a fence");
                done.push((ctx, ring, fence, now.saturating_sub(f.at_us)));
            }
        }
    }
}
