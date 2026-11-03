//! DRM sync objects (linux-6.6/drivers/gpu/drm/drm_syncobj.c, with the behaviour of the Linux 7.3 that the ABI
//! diff compares against where it differs: WAIT with no handles answers 0, WAIT_AVAILABLE waits for a missing
//! fence, WAIT_DEADLINE, the TIMELINE flags of HANDLE_TO_FD/FD_TO_HANDLE, TRANSFER takes only WAIT_FOR_SUBMIT).
//! A syncobj holds one fence or none; a timeline syncobj holds a chain whose nodes are its points. Handles are
//! per file (their own table, not the GEM one). Drivers attach their job fences with `replace_fence` and
//! `add_point`, and take in-fences with `find_fence`.
//!
//! Waits: absolute CLOCK_MONOTONIC deadlines; timeout 0 or a past time is one cheap look (ETIME without any
//! registration or event). A blocking wait sleeps on its own `Waiter`, woken by fence callbacks, by a syncobj
//! getting a fence (WAIT_FOR_SUBMIT) and by the device's unplug (ENODEV); it looks at kill and signals every
//! `SLICE_NS` (B4).

use crate::dispatch::Ctx;
use crate::fence::{self, Fence, FenceCb, Registration, Waiter};
use crate::file::DrmFile;
use crate::sync_file::{self, SyncFile, SyncobjFile};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;
use linux::errno::*;
use vfs::Errno;
use vfs::fd::FdTable;

/// How often a blocking wait looks at kill, signals and the device without a wake (vfs `KILL_SLICE_NS`).
const SLICE_NS: u64 = vfs::poll::KILL_SLICE_NS;
/// `DRM_SYNCOBJ_WAIT_FOR_SUBMIT_TIMEOUT` of `drm_syncobj_find_fence` (relative).
const FIND_SUBMIT_NS: u64 = 5_000_000_000;
/// Handles or points read from the program per step (between two looks at kill and unplug).
const CHUNK: usize = 256;

klog::counter!(WAIT_TIMEOUTS, "drm.syncobj.wait_timeouts");
klog::counter!(WAIT_BLOCKED, "drm.syncobj.waits_blocked");
klog::counter!(CREATED, "drm.syncobj.created");

/// A WAIT_FOR_SUBMIT/WAIT_AVAILABLE waiter on a syncobj without a fence for its point (`syncobj_wait_entry`).
struct SubmitWait {
    point: u64,
    fence: SpinLock<Option<Arc<Fence>>>,
    waker: Arc<dyn FenceCb>,
}

/// `syncobj_eventfd_entry`: signals `target` (an eventfd) once the point is available or signaled.
struct EventfdEntry {
    point: u64,
    available: bool,
    target: Arc<dyn vfs::File>,
    fence: SpinLock<Option<Arc<Fence>>>,
    fired: AtomicBool,
}

impl EventfdEntry {
    /// `eventfd_signal(ctx, 1)` on the eventfd behind `target` (checked at EVENTFD time).
    fn fire(&self) {
        if self.fired.swap(true, Ordering::AcqRel) {
            return;
        }
        // `eventfd_signal(ctx, 1)`: saturates at u64::MAX; a full counter adds nothing.
        if let Some(efd) = self.target.as_eventfd() {
            if efd.signal(1) == 0 {
                klog::event!(Debug, "drm", "syncobj.eventfd_full"; "the eventfd counter is full: event dropped");
            }
        }
        *self.fence.lock() = None;
    }
}

impl FenceCb for EventfdEntry {
    fn call(&self) {
        let f = self.fence.lock().clone();
        if f.is_some_and(|f| fence::is_signaled(&f)) {
            self.fire();
        }
    }
}

struct Inner {
    fence: Option<Arc<Fence>>,
    waits: Vec<Arc<SubmitWait>>,
    eventfds: Vec<Arc<EventfdEntry>>,
}

pub struct Syncobj {
    inner: SpinLock<Inner>,
}

/// What a fence change made ready; handled after the lock.
struct Ready {
    waits: Vec<Arc<SubmitWait>>,
    eventfds: Vec<(Arc<EventfdEntry>, Arc<Fence>)>,
}

impl Ready {
    fn run(self) {
        for w in self.waits {
            w.waker.call();
        }
        for (e, f) in self.eventfds {
            arm_eventfd(e, f);
        }
    }
}

/// The fence that signals `point` now: Ok(None) = no fence yet / point not submitted.
fn point_fence(cur: Option<Arc<Fence>>, point: u64) -> Option<Arc<Fence>> {
    let cur = cur?;
    match fence::find_seqno(Some(cur), point) {
        Ok(f) => Some(f.unwrap_or_else(fence::stub)),
        Err(()) => None,
    }
}

/// The second half of `syncobj_eventfd_entry_func`: an available point fires now, else on its fence.
fn arm_eventfd(e: Arc<EventfdEntry>, f: Arc<Fence>) {
    if e.available {
        e.fire();
        return;
    }
    *e.fence.lock() = Some(f.clone());
    match fence::add_callback(&f, e.clone()) {
        None => e.fire(),
        Some(r) => r.detach(),
    }
}

impl Syncobj {
    /// `drm_syncobj_create`: CREATE_SIGNALED gets a private stub.
    pub fn new(signaled: bool) -> Arc<Syncobj> {
        CREATED.inc();
        let fence = signaled.then(|| Fence::private_stub(fence::now().unwrap_or(0)));
        Arc::new(Syncobj { inner: SpinLock::named("drm.syncobj", Inner { fence, waits: Vec::new(), eventfds: Vec::new() }) })
    }

    /// `drm_syncobj_fence_get`.
    pub fn fence(&self) -> Option<Arc<Fence>> {
        self.inner.lock().fence.clone()
    }

    /// Under the lock after a fence change: the waiters and eventfd entries whose point exists now.
    fn collect(g: &mut Inner) -> Ready {
        let mut r = Ready { waits: Vec::new(), eventfds: Vec::new() };
        if g.fence.is_none() {
            return r;
        }
        let cur = g.fence.clone();
        g.waits.retain(|w| match point_fence(cur.clone(), w.point) {
            Some(f) => {
                *w.fence.lock() = Some(f);
                r.waits.push(w.clone());
                false
            }
            None => true,
        });
        g.eventfds.retain(|e| match point_fence(cur.clone(), e.point) {
            Some(f) => {
                r.eventfds.push((e.clone(), f));
                false
            }
            None => true,
        });
        r
    }

    /// `drm_syncobj_replace_fence`: None empties it (RESET).
    pub fn replace_fence(&self, f: Option<Arc<Fence>>) {
        let (old, ready) = {
            let mut g = self.inner.lock();
            let changed = match (&g.fence, &f) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (None, None) => false,
                _ => true,
            };
            let old = core::mem::replace(&mut g.fence, f);
            let ready = if changed { Syncobj::collect(&mut g) } else { Ready { waits: Vec::new(), eventfds: Vec::new() } };
            (old, ready)
        };
        drop(old);
        ready.run();
    }

    /// `drm_syncobj_add_point`: `fence` becomes timeline point `point` (a chain node on top of the old fence).
    pub fn add_point(&self, f: Arc<Fence>, point: u64) {
        let (prev, ready) = {
            let mut g = self.inner.lock();
            let prev = g.fence.clone();
            if let Some(p) = &prev {
                if p.seqno() >= point {
                    klog::event!(Debug, "drm", "syncobj.unordered_point", point = point, last = p.seqno(); "a point not after the last: QUERY may answer the last one");
                }
            }
            g.fence = Some(fence::chain(prev.clone(), f, point));
            (prev, Syncobj::collect(&mut g))
        };
        ready.run();
        // Walk once so that signaled points leave the chain (Linux's garbage collection).
        let mut it = prev;
        while let Some(c) = it {
            it = fence::walk(&c);
        }
    }

    /// `drm_syncobj_fence_add_wait`: the point's fence now if there is one, else queued.
    fn add_wait(&self, w: &Arc<SubmitWait>) {
        let mut g = self.inner.lock();
        match point_fence(g.fence.clone(), w.point) {
            Some(f) => *w.fence.lock() = Some(f),
            None => g.waits.push(w.clone()),
        }
    }

    fn remove_wait(&self, w: &Arc<SubmitWait>) {
        self.inner.lock().waits.retain(|x| !Arc::ptr_eq(x, w));
    }

    /// `drm_syncobj_add_eventfd`.
    fn add_eventfd(&self, e: Arc<EventfdEntry>) {
        let ready = {
            let mut g = self.inner.lock();
            match point_fence(g.fence.clone(), e.point) {
                Some(f) => Some((e, f)),
                None => {
                    g.eventfds.push(e);
                    None
                }
            }
        };
        if let Some((e, f)) = ready {
            arm_eventfd(e, f);
        }
    }

    /// Waiters queued on this object (tests).
    pub fn queued(&self) -> (usize, usize) {
        let g = self.inner.lock();
        (g.waits.len(), g.eventfds.len())
    }
}

// ---- the per-file handle table (Linux's `syncobj_idr`)

impl DrmFile {
    /// `drm_syncobj_get_handle`.
    pub fn syncobj_insert(&self, o: Arc<Syncobj>) -> Result<u32, Errno> {
        self.syncobjs.lock().insert(o)
    }
    /// `drm_syncobj_find`.
    pub fn syncobj(&self, h: u32) -> Option<Arc<Syncobj>> {
        self.syncobjs.lock().get(h).cloned()
    }
    /// `drm_syncobj_destroy`: EINVAL for a handle the file does not have.
    pub fn syncobj_destroy(&self, h: u32) -> Result<(), Errno> {
        let o = self.syncobjs.lock().remove(h).ok_or(EINVAL)?;
        drop(o);
        Ok(())
    }
    pub fn syncobj_count(&self) -> usize {
        self.syncobjs.lock().len()
    }
    /// `drm_syncobj_release` at the last close.
    pub fn release_syncobjs(&self) -> usize {
        let all = self.syncobjs.lock().drain();
        all.len()
    }
}

// ---- waiting

/// When a wait ends: None = look once (timeout 0, a past time, or no clock yet).
fn deadline(timeout_nsec: i64) -> Option<u64> {
    if timeout_nsec <= 0 {
        return None;
    }
    let now = fence::now()?;
    (timeout_nsec as u64 > now).then_some(timeout_nsec as u64)
}

/// Why a blocking wait must stop besides its condition: kill or a signal (EINTR, Linux's ERESTARTSYS), the
/// device going (ENODEV).
fn stop(ctx: &Ctx) -> Option<Errno> {
    if ctx.mem.interrupted() || ctx.mem.signal_pending() {
        return Some(EINTR);
    }
    ctx.dev.unplugged().then_some(ENODEV)
}

/// Between two steps of reading a program's array: only a kill (EINTR) or unplug (ENODEV) ends it. A plain
/// signal does not, as Linux's `copy_from_user`, which only a fatal signal cuts short.
fn stop_copy(ctx: &Ctx) -> Option<Errno> {
    if ctx.mem.interrupted() {
        return Some(EINTR);
    }
    ctx.dev.unplugged().then_some(ENODEV)
}

/// The device's unplug wakes a waiter while it is registered.
struct UnplugHook<'a> {
    ctx: &'a Ctx<'a>,
    id: u64,
}

impl<'a> UnplugHook<'a> {
    fn new(ctx: &'a Ctx<'a>, w: &Arc<Waiter>) -> UnplugHook<'a> {
        UnplugHook { ctx, id: ctx.dev.on_unplug(w.clone()) }
    }
}

impl Drop for UnplugHook<'_> {
    fn drop(&mut self) {
        self.ctx.dev.off_unplug(self.id);
    }
}

/// Sleeps until a wake, the deadline or the next slice. Err(ETIME) when the deadline has passed.
fn sleep(w: &Waiter, end: Option<u64>) -> Result<(), Errno> {
    let (Some(end), Some(now)) = (end, fence::now()) else { return Err(ETIME) };
    if now >= end {
        return Err(ETIME);
    }
    w.sleep((end - now).min(SLICE_NS));
    Ok(())
}

/// `drm_syncobj_find_fence`: the fence of `point`; WAIT_FOR_SUBMIT waits up to 5 s for it to appear.
pub fn find_fence(ctx: &Ctx, handle: u32, point: u64, flags: u32) -> Result<Arc<Fence>, Errno> {
    let o = ctx.file.syncobj(handle).ok_or(ENOENT)?;
    if let Some(f) = point_fence(o.fence(), point) {
        return Ok(f);
    }
    if flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT == 0 {
        return Err(EINVAL);
    }
    let waiter = Waiter::new();
    let sw = Arc::new(SubmitWait { point, fence: SpinLock::named("drm.syncobj.wait", None), waker: waiter.clone() });
    o.add_wait(&sw);
    let _hook = UnplugHook::new(ctx, &waiter);
    let end = fence::now().map(|n| n.saturating_add(FIND_SUBMIT_NS));
    let r = loop {
        if let Some(f) = sw.fence.lock().clone() {
            break Ok(f);
        }
        if let Some(e) = stop(ctx) {
            break Err(e);
        }
        if let Err(e) = sleep(&waiter, end) {
            klog::event!(Info, "drm", "syncobj.submit_timeout", handle = handle, point = point; "no fence within 5 s: ETIME");
            break Err(e);
        }
    };
    o.remove_wait(&sw);
    r
}

struct Entry {
    fence: Option<Arc<Fence>>,
    sub: Option<Arc<SubmitWait>>,
    reg: Option<Registration>,
}

/// `drm_syncobj_array_wait_timeout`: the index of the first signaled object (u32::MAX if none was signaled at
/// the start of a WAIT_ALL), as Linux's `first_signaled`.
fn array_wait(ctx: &Ctx, objs: &[Arc<Syncobj>], points: &[u64], flags: u32, timeout_nsec: i64) -> Result<u32, Errno> {
    let all = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
    let available = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0;
    let submit = flags & (DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE) != 0;
    let n = objs.len();
    let mut first = u32::MAX;
    let mut entries: Vec<Entry> = Vec::new();
    entries.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    let mut signaled = 0usize;
    for (i, o) in objs.iter().enumerate() {
        let f = point_fence(o.fence(), points[i]);
        if f.is_none() && !submit {
            return Err(EINVAL);
        }
        if let Some(f) = &f {
            if available || fence::is_signaled(f) {
                if signaled == 0 {
                    first = i as u32;
                }
                signaled += 1;
            }
        }
        entries.push(Entry { fence: f, sub: None, reg: None });
    }
    if signaled == n || (signaled > 0 && !all) {
        return Ok(first);
    }
    let end = deadline(timeout_nsec);
    if end.is_none() {
        // The cheap answer of a zero timeout (171 such polls per frame in a vkcube trace): one more look.
        let now_done = entries.iter().filter(|e| e.fence.as_ref().is_some_and(fence::is_signaled)).count();
        if !all {
            if let Some(i) = entries.iter().position(|e| e.fence.as_ref().is_some_and(fence::is_signaled)) {
                return Ok(i as u32);
            }
        } else if now_done == n {
            return Ok(first);
        }
        WAIT_TIMEOUTS.inc();
        return Err(ETIME);
    }
    WAIT_BLOCKED.inc();
    let waiter = Waiter::new();
    if submit {
        for (i, e) in entries.iter_mut().enumerate() {
            if e.fence.is_none() {
                let sw = Arc::new(SubmitWait { point: points[i], fence: SpinLock::named("drm.syncobj.wait", None), waker: waiter.clone() });
                objs[i].add_wait(&sw);
                e.sub = Some(sw);
            }
        }
    }
    let hook = UnplugHook::new(ctx, &waiter);
    let r = 'wait: loop {
        let mut count = 0usize;
        for (i, e) in entries.iter_mut().enumerate() {
            if e.fence.is_none() {
                e.fence = e.sub.as_ref().and_then(|s| s.fence.lock().clone());
            }
            let Some(f) = &e.fence else { continue };
            let done = available
                || fence::is_signaled(f)
                || (e.reg.is_none() && {
                    e.reg = fence::add_callback(f, waiter.clone());
                    e.reg.is_none()
                });
            if done {
                if all {
                    count += 1;
                } else {
                    break 'wait Ok(i as u32);
                }
            }
        }
        if count == n {
            break Ok(first);
        }
        if let Some(e) = stop(ctx) {
            klog::event!(Debug, "drm", "syncobj.wait_stopped", errno = e, minor = ctx.dev.render_minor());
            break Err(e);
        }
        if let Err(e) = sleep(&waiter, end) {
            WAIT_TIMEOUTS.inc();
            break Err(e);
        }
    };
    drop(hook);
    for (i, e) in entries.iter().enumerate() {
        if let Some(s) = &e.sub {
            objs[i].remove_wait(s);
        }
    }
    r
}

// ---- reading the program's arrays

fn get<T: Wire>(data: &[u8]) -> Result<T, Errno> {
    T::decode(data).ok_or(EINVAL)
}

fn set<T: Wire>(data: &mut [u8], v: &T) -> Result<(), Errno> {
    v.encode(data).ok_or(EINVAL)
}

/// `drm_syncobj_array_find`: room for every handle first (ENOMEM before any user memory is touched, as
/// Linux's `kmalloc_array`), then all handles read (EFAULT), then looked up (ENOENT). The read goes in steps
/// that look at kill and unplug, so a huge array cannot hold the device guard.
fn array_find(ctx: &Ctx, user: u64, count: u32) -> Result<Vec<Arc<Syncobj>>, Errno> {
    let count = count as usize;
    let mut handles: Vec<u32> = Vec::new();
    handles.try_reserve_exact(count).map_err(|_| ENOMEM)?;
    let mut out: Vec<Arc<Syncobj>> = Vec::new();
    out.try_reserve_exact(count).map_err(|_| ENOMEM)?;
    let mut buf = [0u8; CHUNK * 4];
    while handles.len() < count {
        if let Some(e) = stop_copy(ctx) {
            return Err(e);
        }
        let k = handles.len();
        let n = (count - k).min(CHUNK);
        let addr = user.checked_add((k * 4) as u64).ok_or(EFAULT)?;
        if !ctx.mem.read(addr, &mut buf[..n * 4]) {
            return Err(EFAULT);
        }
        handles.extend(buf[..n * 4].chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])));
    }
    for h in handles {
        out.push(ctx.file.syncobj(h).ok_or(ENOENT)?);
    }
    Ok(out)
}

/// A user array of `n` u64 points; a null pointer means all zero (as Linux). Read in steps like `array_find`.
fn points(ctx: &Ctx, user: u64, n: usize) -> Result<Vec<u64>, Errno> {
    let mut v: Vec<u64> = Vec::new();
    v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    if user == 0 {
        v.resize(n, 0);
        return Ok(v);
    }
    let mut b = [0u8; CHUNK * 8];
    while v.len() < n {
        if let Some(e) = stop_copy(ctx) {
            return Err(e);
        }
        let k = v.len();
        let m = (n - k).min(CHUNK);
        let addr = user.checked_add((k * 8) as u64).ok_or(EFAULT)?;
        if !ctx.mem.read(addr, &mut b[..m * 8]) {
            return Err(EFAULT);
        }
        v.extend(b[..m * 8].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8]))));
    }
    Ok(v)
}

fn need(ctx: &Ctx, timeline: bool) -> Result<(), Errno> {
    let f = &ctx.dev.driver().info().features;
    if !f.syncobj || (timeline && !f.syncobj_timeline) {
        return Err(EOPNOTSUPP);
    }
    Ok(())
}

fn fds<'a>(ctx: &'a Ctx) -> Option<&'a FdTable> {
    ctx.fds.as_deref()
}

// ---- the ioctls (drm_syncobj.c:831-1660)

/// `drm_syncobj_create_ioctl`.
pub fn create_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let mut a: drm_syncobj_create = get(data)?;
    if a.flags & !DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
        return Err(EINVAL);
    }
    a.handle = ctx.file.syncobj_insert(Syncobj::new(a.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0))?;
    set(data, &a)?;
    Ok(0)
}

/// `drm_syncobj_destroy_ioctl`.
pub fn destroy_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let a: drm_syncobj_destroy = get(data)?;
    if a.pad != 0 {
        return Err(EINVAL);
    }
    ctx.file.syncobj_destroy(a.handle)?;
    Ok(0)
}

/// `drm_syncobj_handle_to_fd_ioctl`: a syncobj fd, or with EXPORT_SYNC_FILE a sync_file of the fence (at
/// `point` with the TIMELINE flag).
pub fn handle_to_fd_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let mut a: drm_syncobj_handle = get(data)?;
    if a.pad != 0 || a.flags & !(DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE | DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE) != 0 {
        return Err(EINVAL);
    }
    let point = if a.flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE != 0 { a.point } else { 0 };
    let file: Arc<dyn vfs::File> = if a.flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE != 0 {
        if fds(ctx).is_none() {
            return Err(EMFILE);
        }
        SyncFile::new(find_fence(ctx, a.handle, point, 0)?, b"")
    } else {
        if a.point != 0 {
            return Err(EINVAL);
        }
        SyncobjFile::new(ctx.file.syncobj(a.handle).ok_or(EINVAL)?)
    };
    a.fd = sync_file::install(fds(ctx), file)?;
    set(data, &a)?;
    Ok(0)
}

/// `drm_syncobj_fd_to_handle_ioctl`: a syncobj fd's object under a new handle, or with IMPORT_SYNC_FILE a
/// sync_file's fence into an existing syncobj (as timeline point `point` with the TIMELINE flag).
pub fn fd_to_handle_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let mut a: drm_syncobj_handle = get(data)?;
    if a.pad != 0 || a.flags & !(DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE | DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE) != 0 {
        return Err(EINVAL);
    }
    let point = if a.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE != 0 { a.point } else { 0 };
    let file = sync_file::lookup(fds(ctx), a.fd);
    if a.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE != 0 {
        let f = file.ok().and_then(|f| sync_file::as_sync_file(&f)).ok_or(EINVAL)?;
        let o = ctx.file.syncobj(a.handle).ok_or(ENOENT)?;
        if point != 0 {
            // Flattened (as TRANSFER does) so a chain never holds a chain.
            o.add_point(fence::merge(&[f.fence()]), point);
        } else {
            o.replace_fence(Some(f.fence().clone()));
        }
        return Ok(0);
    }
    if a.point != 0 {
        return Err(EINVAL);
    }
    let o = file.ok().and_then(|f| sync_file::as_syncobj(&f)).ok_or(EINVAL)?;
    a.handle = ctx.file.syncobj_insert(o)?;
    set(data, &a)?;
    Ok(0)
}

/// `drm_syncobj_transfer_ioctl`: a fence (point) of one syncobj into another (as a point when `dst_point`).
pub fn transfer_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, true)?;
    let a: drm_syncobj_transfer = get(data)?;
    if a.pad != 0 || a.flags & !DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0 {
        return Err(EINVAL);
    }
    let dst = ctx.file.syncobj(a.dst_handle).ok_or(ENOENT)?;
    let f = find_fence(ctx, a.src_handle, a.src_point, a.flags)?;
    if a.dst_point != 0 {
        dst.add_point(fence::merge(&[&f]), a.dst_point);
    } else {
        dst.replace_fence(Some(f));
    }
    Ok(0)
}

/// `drm_syncobj_wait_ioctl`.
pub fn wait_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let mut a: drm_syncobj_wait = get(data)?;
    if a.flags & !(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE) != 0 {
        return Err(EINVAL);
    }
    if a.count_handles == 0 {
        return Ok(0);
    }
    let objs = array_find(ctx, a.handles, a.count_handles)?;
    let zeros = points(ctx, 0, objs.len())?;
    a.first_signaled = array_wait(ctx, &objs, &zeros, a.flags, a.timeout_nsec)?;
    set(data, &a)?;
    Ok(0)
}

/// `drm_syncobj_timeline_wait_ioctl`.
pub fn timeline_wait_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, true)?;
    let mut a: drm_syncobj_timeline_wait = get(data)?;
    if a.flags
        & !(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE)
        != 0
    {
        return Err(EINVAL);
    }
    if a.count_handles == 0 {
        return Ok(0);
    }
    let objs = array_find(ctx, a.handles, a.count_handles)?;
    let pts = points(ctx, a.points, objs.len())?;
    a.first_signaled = array_wait(ctx, &objs, &pts, a.flags, a.timeout_nsec)?;
    set(data, &a)?;
    Ok(0)
}

/// `drm_syncobj_eventfd_ioctl`: the eventfd gets 1 when the point is signaled (or available).
pub fn eventfd_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, true)?;
    let a: drm_syncobj_eventfd = get(data)?;
    if a.flags & !DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0 || a.pad != 0 {
        return Err(EINVAL);
    }
    let o = ctx.file.syncobj(a.handle).ok_or(ENOENT)?;
    // `eventfd_ctx_fdget`: EBADF for no file, EINVAL for a file that is no eventfd.
    let target = sync_file::lookup(fds(ctx), a.fd)?;
    if target.as_eventfd().is_none() {
        return Err(EINVAL);
    }
    let e = Arc::new(EventfdEntry {
        point: a.point,
        available: a.flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0,
        target,
        fence: SpinLock::named("drm.syncobj.eventfd", None),
        fired: AtomicBool::new(false),
    });
    o.add_eventfd(e);
    Ok(0)
}

/// `drm_syncobj_reset_ioctl`.
pub fn reset_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let a: drm_syncobj_array = get(data)?;
    if a.pad != 0 || a.count_handles == 0 {
        return Err(EINVAL);
    }
    for o in array_find(ctx, a.handles, a.count_handles)? {
        o.replace_fence(None);
    }
    Ok(0)
}

/// `drm_syncobj_signal_ioctl`: each gets a new signaled stub (`drm_syncobj_assign_null_handle`).
pub fn signal_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, false)?;
    let a: drm_syncobj_array = get(data)?;
    if a.pad != 0 || a.count_handles == 0 {
        return Err(EINVAL);
    }
    for o in array_find(ctx, a.handles, a.count_handles)? {
        o.replace_fence(Some(Fence::private_stub(fence::now().unwrap_or(0))));
    }
    Ok(0)
}

/// `drm_syncobj_timeline_signal_ioctl`: point `points[i]` with the shared stub.
pub fn timeline_signal_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, true)?;
    let a: drm_syncobj_timeline_array = get(data)?;
    if a.flags != 0 || a.count_handles == 0 {
        return Err(EINVAL);
    }
    let objs = array_find(ctx, a.handles, a.count_handles)?;
    let pts = points(ctx, a.points, objs.len())?;
    for (o, p) in objs.iter().zip(pts) {
        o.add_point(fence::stub(), p);
    }
    Ok(0)
}

/// `drm_syncobj_query_ioctl`: the last signaled point (LAST_SUBMITTED: the last added); 0 for a binary one.
pub fn query_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    need(ctx, true)?;
    let a: drm_syncobj_timeline_array = get(data)?;
    if a.flags & !DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0 || a.count_handles == 0 {
        return Err(EINVAL);
    }
    for (i, o) in array_find(ctx, a.handles, a.count_handles)?.iter().enumerate() {
        let point = match o.fence() {
            Some(f) if f.is_chain() => {
                if a.flags & DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0 {
                    f.seqno()
                } else {
                    let mut last = f.clone();
                    let mut it = fence::walk(&f);
                    while let Some(c) = it {
                        if c.context() != f.context() {
                            break;
                        }
                        it = fence::walk(&c);
                        last = c;
                    }
                    if fence::is_signaled(&last) { last.seqno() } else { last.chain_prev_seqno() }
                }
            }
            _ => 0,
        };
        let addr = a.points.checked_add(i as u64 * 8).ok_or(EFAULT)?;
        if !ctx.mem.write(addr, &point.to_le_bytes()) {
            return Err(EFAULT);
        }
    }
    Ok(0)
}
