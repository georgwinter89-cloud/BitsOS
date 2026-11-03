//! DR-3 host tests: syncobj ioctls through the dispatcher with the fake's ring fences (pending states user
//! space cannot make without a GPU), blocking waits woken by another thread, unplug and kill, timelines and
//! their chains, sync_file poll/merge/info, eventfd registrations, and no callbacks left behind.

use crate::device;
use crate::dispatch::Caller;
use crate::dr2_tests::MINORS;
use crate::fake::FakeGpu;
use crate::fence::{self, Fence};
use crate::file::MinorKind;
use crate::node::{open_device, OpenDrm};
use crate::sync_file::{self, SyncFile};
use crate::syncobj::Syncobj;
use crate::uapi::*;
use crate::wire::Wire;
use core::sync::atomic::{AtomicBool, Ordering};
use linux::errno::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::vec::Vec;
use std::{thread, vec};
use vfs::fd::FdTable;
use vfs::File;

const BASE: u64 = 0x10000;
const ME: Caller = Caller { pid: 42, sys_admin: false, sys_nice: false };

struct Mem {
    b: Mutex<Vec<u8>>,
    kill: AtomicBool,
}

impl Mem {
    fn new() -> Mem {
        Mem { b: Mutex::new(vec![0; 8192]), kill: AtomicBool::new(false) }
    }
    fn set(&self, off: usize, d: &[u8]) {
        self.b.lock().unwrap()[off..off + d.len()].copy_from_slice(d);
    }
    fn get(&self, off: usize, n: usize) -> Vec<u8> {
        self.b.lock().unwrap()[off..off + n].to_vec()
    }
    fn put<T: Wire>(&self, off: usize, v: &T) {
        let mut b = vec![0u8; T::SIZE];
        v.encode(&mut b).unwrap();
        self.set(off, &b);
    }
    fn take<T: Wire>(&self, off: usize) -> T {
        T::decode(&self.get(off, T::SIZE)).unwrap()
    }
    fn u32s(&self, off: usize, v: &[u32]) {
        for (k, x) in v.iter().enumerate() {
            self.set(off + 4 * k, &x.to_le_bytes());
        }
    }
    fn u64s(&self, off: usize, v: &[u64]) {
        for (k, x) in v.iter().enumerate() {
            self.set(off + 8 * k, &x.to_le_bytes());
        }
    }
}

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let b = self.b.lock().unwrap();
        let Some(s) = addr.checked_sub(BASE).and_then(|s| usize::try_from(s).ok()) else { return false };
        if s + buf.len() > b.len() {
            return false;
        }
        buf.copy_from_slice(&b[s..s + buf.len()]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut b = self.b.lock().unwrap();
        let Some(s) = addr.checked_sub(BASE).and_then(|s| usize::try_from(s).ok()) else { return false };
        if s + data.len() > b.len() {
            return false;
        }
        b[s..s + data.len()].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
    fn interrupted(&self) -> bool {
        self.kill.load(Ordering::SeqCst)
    }
}

struct Fx {
    f: Arc<OpenDrm>,
    fds: Arc<FdTable>,
}

impl Fx {
    fn new() -> Fx {
        let d = {
            let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
            device::register(Arc::new(FakeGpu::radeon_780m())).unwrap()
        };
        Fx { f: open_device(d, MinorKind::Render, false).unwrap(), fds: Arc::new(FdTable::new()) }
    }
    fn io(&self, cmd: u32, m: &Mem) -> Result<i64, vfs::Errno> {
        self.f.ioctl_with(cmd, BASE, m, ME, Some(self.fds.clone()))
    }
    fn ring(&self) -> &crate::fake::FakeRing {
        self.f.device().driver().as_fake().unwrap().ring()
    }
    fn create(&self, m: &Mem, flags: u32) -> u32 {
        m.put(0, &drm_syncobj_create { handle: 0, flags });
        self.io(DRM_IOCTL_SYNCOBJ_CREATE, m).unwrap();
        m.take::<drm_syncobj_create>(0).handle
    }
    fn obj(&self, h: u32) -> Arc<Syncobj> {
        self.f.drm_file().syncobj(h).unwrap()
    }
    /// WAIT on `hs` (handles at +1024) with an absolute timeout; the struct at 0.
    fn wait(&self, m: &Mem, hs: &[u32], flags: u32, timeout: i64) -> (Result<i64, vfs::Errno>, u32) {
        m.u32s(1024, hs);
        let w = drm_syncobj_wait { handles: BASE + 1024, timeout_nsec: timeout, count_handles: hs.len() as u32, flags, first_signaled: 77, pad: 0, deadline_nsec: 0 };
        m.put(0, &w);
        let r = self.io(DRM_IOCTL_SYNCOBJ_WAIT, m);
        (r, m.take::<drm_syncobj_wait>(0).first_signaled)
    }
    fn twait(&self, m: &Mem, hs: &[u32], pts: &[u64], flags: u32, timeout: i64) -> Result<i64, vfs::Errno> {
        m.u32s(1024, hs);
        m.u64s(2048, pts);
        let w = drm_syncobj_timeline_wait {
            handles: BASE + 1024,
            points: BASE + 2048,
            timeout_nsec: timeout,
            count_handles: hs.len() as u32,
            flags,
            first_signaled: 0,
            pad: 0,
            deadline_nsec: 0,
        };
        m.put(0, &w);
        self.io(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, m)
    }
    fn query(&self, m: &Mem, h: u32, flags: u32) -> u64 {
        m.u32s(1024, &[h]);
        m.put(0, &drm_syncobj_timeline_array { handles: BASE + 1024, points: BASE + 2048, count_handles: 1, flags });
        self.io(DRM_IOCTL_SYNCOBJ_QUERY, m).unwrap();
        u64::from_le_bytes(m.get(2048, 8).try_into().unwrap())
    }
    fn export(&self, m: &Mem, h: u32) -> Result<i32, vfs::Errno> {
        m.put(0, &drm_syncobj_handle { handle: h, flags: DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE, fd: -1, pad: 0, point: 0 });
        self.io(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, m)?;
        Ok(m.take::<drm_syncobj_handle>(0).fd)
    }
    fn sync_file(&self, fd: i32) -> Arc<SyncFile> {
        let f = self.fds.get(fd as usize).unwrap().file.clone();
        sync_file::as_sync_file(&f).unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.fds.close_all();
        let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
        device::unregister(self.f.device());
    }
}

fn now() -> u64 {
    fence::now().unwrap()
}

/// Runs `f` on another thread after `ms` milliseconds.
fn later(ms: u64, f: impl FnOnce() + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        f();
    })
}

#[test]
fn handles_are_their_own_table_and_go_at_close() {
    let fx = Fx::new();
    let m = Mem::new();
    let a = fx.create(&m, 0);
    let b = fx.create(&m, DRM_SYNCOBJ_CREATE_SIGNALED);
    assert_eq!((a, b), (1, 2));
    assert_eq!(fx.f.drm_file().handle_count(), 0, "no GEM handle");
    m.put(0, &drm_syncobj_destroy { handle: a, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_DESTROY, &m), Ok(0));
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_DESTROY, &m), Err(EINVAL));
    assert_eq!(fx.create(&m, 0), 1, "the lowest free handle again");
    assert_eq!(fx.f.drm_file().syncobj_count(), 2);
    assert_eq!(fx.f.drm_file().release_syncobjs(), 2);
}

#[test]
fn ring_fence_pending_then_signaled() {
    let fx = Fx::new();
    let m = Mem::new();
    let h = fx.create(&m, 0);
    let f = fx.ring().emit();
    fx.obj(h).replace_fence(Some(f.clone()));
    // Timeout 0 and a past time: ETIME, first_signaled untouched, no callback left on the fence.
    assert_eq!(fx.wait(&m, &[h], 0, 0), (Err(ETIME), 77));
    assert_eq!(fx.wait(&m, &[h], 0, 5), (Err(ETIME), 77));
    assert_eq!(f.callbacks(), 0);
    // A real deadline: not back before it.
    let t0 = now();
    assert_eq!(fx.wait(&m, &[h], 0, (t0 + 30_000_000) as i64).0, Err(ETIME));
    assert!(now() >= t0 + 30_000_000);
    assert_eq!(f.callbacks(), 0, "the timed-out wait took its callback off");
    // Signaled by the ring while waiting.
    let r = fx.f.device().clone();
    let j = later(20, move || {
        r.driver().as_fake().unwrap().ring().signal_all();
    });
    let t0 = Instant::now();
    assert_eq!(fx.wait(&m, &[h], 0, (now() + 5_000_000_000) as i64), (Ok(0), 0));
    assert!(t0.elapsed() < Duration::from_secs(4));
    j.join().unwrap();
    assert!(fence::is_signaled(&f));
    assert_eq!(fence::status(&f), 1);
    assert_eq!(fx.ring().pending(), 0);
}

#[test]
fn wait_any_and_all_with_first_signaled() {
    let fx = Fx::new();
    let m = Mem::new();
    let (a, b) = (fx.create(&m, 0), fx.create(&m, 0));
    let (fa, fb) = (fx.ring().emit(), fx.ring().emit());
    fx.obj(a).replace_fence(Some(fa));
    fx.obj(b).replace_fence(Some(fb.clone()));
    fb.signal();
    assert_eq!(fx.wait(&m, &[a, b], 0, 0), (Ok(0), 1));
    assert_eq!(fx.wait(&m, &[a, b], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, 0), (Err(ETIME), 77));
    // WAIT_ALL that completes while waiting keeps the index found at the start (b = 1).
    let r = fx.f.device().clone();
    let j = later(10, move || {
        r.driver().as_fake().unwrap().ring().signal_all();
    });
    assert_eq!(fx.wait(&m, &[a, b], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, (now() + 5_000_000_000) as i64), (Ok(0), 1));
    j.join().unwrap();
    // None signaled at the start of a WAIT_ALL: first_signaled is ~0.
    let (c, d) = (fx.create(&m, 0), fx.create(&m, 0));
    fx.obj(c).replace_fence(Some(fx.ring().emit()));
    fx.obj(d).replace_fence(Some(fx.ring().emit()));
    let r = fx.f.device().clone();
    let j = later(10, move || {
        r.driver().as_fake().unwrap().ring().signal_all();
    });
    assert_eq!(fx.wait(&m, &[c, d], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, (now() + 5_000_000_000) as i64), (Ok(0), u32::MAX));
    j.join().unwrap();
}

#[test]
fn wait_errors_as_linux_7_3() {
    let fx = Fx::new();
    let m = Mem::new();
    let none = fx.create(&m, 0);
    let sig = fx.create(&m, DRM_SYNCOBJ_CREATE_SIGNALED);
    assert_eq!(fx.wait(&m, &[none], 0, 0), (Err(EINVAL), 77));
    assert_eq!(fx.wait(&m, &[sig], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE, 0).0, Err(EINVAL));
    assert_eq!(fx.wait(&m, &[sig], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE, 0), (Ok(0), 0));
    assert_eq!(fx.wait(&m, &[99], 0, 0).0, Err(ENOENT));
    assert_eq!(fx.wait(&m, &[], 0, 0), (Ok(0), 77), "no handles: 0, untouched");
    assert_eq!(fx.wait(&m, &[], 0x40, 0).0, Err(EINVAL));
    let w = drm_syncobj_wait { handles: 0xdead_0000, timeout_nsec: 0, count_handles: 1, flags: 0, first_signaled: 0, pad: 0, deadline_nsec: 0 };
    m.put(0, &w);
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_WAIT, &m), Err(EFAULT));
    // A big count over a short buffer: room is reserved first, then the read faults (Linux: kmalloc, EFAULT).
    let w = drm_syncobj_wait { handles: BASE + 1024, timeout_nsec: 0, count_handles: 1 << 20, flags: 0, first_signaled: 0, pad: 0, deadline_nsec: 0 };
    m.put(0, &w);
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_WAIT, &m), Err(EFAULT));
    // The largest count: ENOMEM before any user memory is read (a host with room for 48 GB reads and faults).
    let w = drm_syncobj_wait { count_handles: u32::MAX, ..w };
    m.put(0, &w);
    let r = fx.io(DRM_IOCTL_SYNCOBJ_WAIT, &m);
    assert!(r == Err(ENOMEM) || r == Err(EFAULT), "{r:?}");
    // A killed caller stops between two steps of a long read: EINTR, not ENOENT for the zero handles past it.
    m.u32s(1024, &[sig; 512]);
    let w = drm_syncobj_wait { count_handles: 512, ..w };
    m.put(0, &w);
    m.kill.store(true, Ordering::SeqCst);
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_WAIT, &m), Err(EINTR));
    m.kill.store(false, Ordering::SeqCst);
    // Linux 6.6's 32-byte struct: the same call, first_signaled copied back.
    m.u32s(1024, &[sig]);
    let w = drm_syncobj_wait { handles: BASE + 1024, timeout_nsec: 0, count_handles: 1, flags: 0, first_signaled: 77, pad: 0, deadline_nsec: 0xffff };
    m.put(0, &w);
    let cmd32 = crate::ioc::ioc(3, 0x64, 0xc3, 32);
    assert_eq!(fx.f.ioctl_with(cmd32, BASE, &m, ME, Some(fx.fds.clone())), Ok(0));
    assert_eq!(m.take::<drm_syncobj_wait>(0).first_signaled, 0);
    assert_eq!(m.take::<drm_syncobj_wait>(0).deadline_nsec, 0xffff, "bytes past the caller's size are left alone");
}

#[test]
fn wait_for_submit_and_available_wake_on_a_new_fence() {
    let fx = Fx::new();
    let m = Mem::new();
    let h = fx.create(&m, 0);
    assert_eq!(fx.wait(&m, &[h], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, 0).0, Err(ETIME));
    let o = fx.obj(h);
    let f = fx.ring().emit();
    let (o2, f2) = (o.clone(), f.clone());
    let j = later(10, move || {
        o2.replace_fence(Some(f2.clone()));
        thread::sleep(Duration::from_millis(10));
        f2.signal();
    });
    assert_eq!(fx.wait(&m, &[h], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, (now() + 5_000_000_000) as i64), (Ok(0), 0));
    j.join().unwrap();
    assert_eq!(o.queued(), (0, 0), "the submit waiter left the syncobj");
    // WAIT_AVAILABLE (timeline wait): done once the point exists, signaled or not.
    let t = fx.create(&m, 0);
    let ot = fx.obj(t);
    let pending = fx.ring().emit();
    let j = later(10, move || ot.add_point(pending, 3));
    assert_eq!(fx.twait(&m, &[t], &[3], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE, (now() + 5_000_000_000) as i64), Ok(0));
    j.join().unwrap();
    assert_eq!(fx.twait(&m, &[t], &[3], 0, 0), Err(ETIME));
}

#[test]
fn unplug_and_kill_end_a_blocking_wait() {
    let fx = Fx::new();
    let m = Mem::new();
    let h = fx.create(&m, 0);
    fx.obj(h).replace_fence(Some(fx.ring().emit()));
    let d = fx.f.device().clone();
    let j = later(20, move || {
        device::unplug(&d);
    });
    let t0 = Instant::now();
    assert_eq!(fx.wait(&m, &[h], 0, (now() + 20_000_000_000) as i64).0, Err(ENODEV));
    assert!(t0.elapsed() < Duration::from_secs(1), "woken by the unplug, not by a slice: {:?}", t0.elapsed());
    j.join().unwrap();
    // A killed caller: EINTR within a slice.
    let fx = Fx::new();
    let m = Mem::new();
    let h = fx.create(&m, 0);
    fx.obj(h).replace_fence(Some(fx.ring().emit()));
    m.kill.store(true, Ordering::SeqCst);
    assert_eq!(fx.wait(&m, &[h], 0, (now() + 20_000_000_000) as i64).0, Err(EINTR));
}

#[test]
fn timeline_points_query_and_waits() {
    let fx = Fx::new();
    let m = Mem::new();
    let t = fx.create(&m, 0);
    let o = fx.obj(t);
    let fs: Vec<Arc<Fence>> = (0..3).map(|_| fx.ring().emit()).collect();
    for (k, f) in fs.iter().enumerate() {
        o.add_point(f.clone(), k as u64 + 1);
    }
    assert_eq!(fx.query(&m, t, 0), 0);
    assert_eq!(fx.query(&m, t, DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED), 3);
    assert_eq!(fx.twait(&m, &[t], &[1], 0, 0), Err(ETIME));
    assert_eq!(fx.twait(&m, &[t], &[4], 0, 0), Err(EINVAL), "not submitted");
    fx.ring().signal_through(1);
    assert_eq!(fx.query(&m, t, 0), 1);
    assert_eq!(fx.twait(&m, &[t], &[1], 0, 0), Ok(0));
    assert_eq!(fx.twait(&m, &[t], &[2], 0, 0), Err(ETIME));
    let r = fx.f.device().clone();
    let j = later(10, move || {
        r.driver().as_fake().unwrap().ring().signal_through(2);
    });
    assert_eq!(fx.twait(&m, &[t], &[2], 0, (now() + 5_000_000_000) as i64), Ok(0));
    j.join().unwrap();
    assert_eq!(fx.query(&m, t, 0), 2);
    assert_eq!(fx.twait(&m, &[t], &[3], 0, 0), Err(ETIME));
    fx.ring().signal_all();
    assert_eq!(fx.query(&m, t, 0), 3);
    // Signaled points left the chain: point 1 is found as "gone" (the stub).
    assert!(fence::find_seqno(o.fence(), 1).unwrap().is_none_or(|f| fence::is_signaled(&f)));
    for f in &fs {
        assert_eq!(f.callbacks(), 0);
    }
    // An out-of-order point: its own context, the chain's seqno.
    o.add_point(fx.ring().emit(), 2);
    assert_eq!(fx.query(&m, t, DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED), 3);
}

#[test]
fn long_chain_drops_without_recursion() {
    let ring = crate::fake::FakeRing::new("gfx_0.0.0");
    let mut head = None;
    for p in 1..=200_000u64 {
        head = Some(fence::chain(head, ring.emit(), p));
    }
    let o = Syncobj::new(false);
    o.replace_fence(head);
    assert_eq!(o.fence().unwrap().seqno(), 200_000);
    drop(o);
    // The ring still holds every fence; signaling them all runs no callbacks of the gone chain.
    assert_eq!(ring.signal_all(), 200_000);
}

#[test]
fn transfer_flattens_and_moves_points() {
    let fx = Fx::new();
    let m = Mem::new();
    let (src, t, bin) = (fx.create(&m, 0), fx.create(&m, 0), fx.create(&m, 0));
    let f = fx.ring().emit();
    fx.obj(src).replace_fence(Some(f.clone()));
    m.put(0, &drm_syncobj_transfer { src_handle: src, dst_handle: t, src_point: 0, dst_point: 7, flags: 0, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_TRANSFER, &m), Ok(0));
    assert_eq!(fx.query(&m, t, DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED), 7);
    assert_eq!(fx.query(&m, t, 0), 0);
    m.put(0, &drm_syncobj_transfer { src_handle: t, dst_handle: bin, src_point: 7, dst_point: 0, flags: 0, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_TRANSFER, &m), Ok(0));
    assert_eq!(fx.wait(&m, &[bin], 0, 0).0, Err(ETIME));
    f.signal();
    assert_eq!(fx.wait(&m, &[bin], 0, 0).0, Ok(0));
    assert_eq!(fx.query(&m, t, 0), 7);
    m.put(0, &drm_syncobj_transfer { src_handle: t, dst_handle: bin, src_point: 7, dst_point: 0, flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_TRANSFER, &m), Err(EINVAL), "Linux 7.3 takes only WAIT_FOR_SUBMIT");
}

#[test]
fn sync_file_poll_info_and_merge() {
    let fx = Fx::new();
    let m = Mem::new();
    let (a, b) = (fx.create(&m, 0), fx.create(&m, 0));
    let (fa, fb) = (fx.ring().emit(), fx.ring().emit());
    fx.obj(a).replace_fence(Some(fa.clone()));
    fx.obj(b).replace_fence(Some(fb.clone()));
    let sa = fx.export(&m, a).unwrap();
    let sb = fx.export(&m, b).unwrap();
    assert_eq!(fx.fds.cloexec(sa as usize), Ok(true));
    let s = fx.sync_file(sa);
    assert_eq!(s.events().0, 0);
    let name = s.name();
    assert!(name.starts_with(b"amdgpu-gfx_0.0.0"), "{:?}", core::str::from_utf8(&name));
    // FILE_INFO of a pending fence.
    let mut info = sync_file_info::ZERO;
    m.put(512, &info);
    assert_eq!(s.ioctl_with(SYNC_IOC_FILE_INFO, BASE + 512, &m, Some(&fx.fds)), Ok(0));
    info = m.take(512);
    assert_eq!((info.status, info.num_fences), (0, 1));
    // Merge of two fences of one ring: the later one only.
    let mut md = sync_merge_data::ZERO;
    md.name[..2].copy_from_slice(b"mg");
    md.fd2 = sb;
    m.put(512, &md);
    assert_eq!(s.ioctl_with(SYNC_IOC_MERGE, BASE + 512, &m, Some(&fx.fds)), Ok(0));
    let merged = fx.sync_file(m.take::<sync_merge_data>(512).fence);
    assert!(Arc::ptr_eq(merged.fence(), &fb), "same context: the later fence");
    // A fence of another context: an array of two, sorted by context.
    let other = Fence::new(fence::context_alloc(1), 9, "amdgpu", "sdma0");
    let so = SyncFile::new(other.clone(), b"");
    let two = fence::merge(&[s.fence(), so.fence()]);
    assert_eq!(fence::unwrap(&two).len(), 2);
    assert!(fence::unwrap(&two)[0].context() < fence::unwrap(&two)[1].context());
    // Poll: the queue is woken when the fence signals, and IN shows.
    let seq = s.poll_head().unwrap().seq();
    fa.signal();
    assert!(s.poll_head().unwrap().seq() > seq, "a wake after the signal");
    assert_eq!(s.events().0, vfs::poll::Events::IN);
    assert_eq!(fa.callbacks(), 0);
    // FILE_INFO with the array: names of a signaled fence are Linux 7.3's.
    let mut info = sync_file_info::ZERO;
    info.num_fences = 1;
    info.sync_fence_info = BASE + 4096;
    m.put(512, &info);
    assert_eq!(s.ioctl_with(SYNC_IOC_FILE_INFO, BASE + 512, &m, Some(&fx.fds)), Ok(0));
    let fi: sync_fence_info = m.take(4096);
    assert!(fi.driver_name.starts_with(b"detached-driver\0") && fi.obj_name.starts_with(b"signaled-timeline\0"));
    assert_eq!(fi.status, 1);
    assert_eq!(fi.timestamp_ns, fa.timestamp());
    // Import into a syncobj; a syncobj fd is no sync_file.
    let c = fx.create(&m, 0);
    m.put(0, &drm_syncobj_handle { handle: c, flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE, fd: sa, pad: 0, point: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &m), Ok(0));
    assert_eq!(fx.wait(&m, &[c], 0, 0).0, Ok(0));
    m.put(0, &drm_syncobj_handle { handle: c, flags: 0, fd: -1, pad: 0, point: 0 });
    fx.io(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &m).unwrap();
    let ofd = m.take::<drm_syncobj_handle>(0).fd;
    m.put(0, &drm_syncobj_handle { handle: c, flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE, fd: ofd, pad: 0, point: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &m), Err(EINVAL));
    m.put(0, &drm_syncobj_handle { handle: 0, flags: 0, fd: ofd, pad: 0, point: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &m), Ok(0));
    let c2 = m.take::<drm_syncobj_handle>(0).handle;
    assert!(Arc::ptr_eq(&fx.obj(c), &fx.obj(c2)));
    let f = fx.fds.get(ofd as usize).unwrap().file.clone();
    assert_eq!(f.link_name().as_deref(), Some("anon_inode:syncobj_file"));
    drop((s, merged, so));
}

#[test]
fn eventfd_on_ring_fences() {
    let fx = Fx::new();
    let m = Mem::new();
    let efile = vfs::eventfd::new_eventfd(0, vfs::eventfd::EFD_NONBLOCK);
    let efd = fx.fds.install_flags(efile.clone(), 0, true).unwrap() as i32;
    let count = || {
        let mut b = [0u8; 8];
        efile.file.read_stream(&mut b, true).map(|_| u64::from_le_bytes(b)).unwrap_or(0)
    };
    let h = fx.create(&m, 0);
    let f = fx.ring().emit();
    fx.obj(h).replace_fence(Some(f.clone()));
    m.put(0, &drm_syncobj_eventfd { handle: h, flags: 0, point: 0, fd: efd, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Ok(0));
    assert_eq!(count(), 0);
    f.signal();
    assert_eq!(count(), 1);
    // WAIT_AVAILABLE: fires when the point appears; the registration outlives the handle.
    let t = fx.create(&m, 0);
    let o = fx.obj(t);
    m.put(0, &drm_syncobj_eventfd { handle: t, flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE, point: 2, fd: efd, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Ok(0));
    m.put(0, &drm_syncobj_eventfd { handle: t, flags: 0, point: 2, fd: efd, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Ok(0));
    assert_eq!(o.queued(), (0, 2));
    m.put(0, &drm_syncobj_destroy { handle: t, pad: 0 });
    fx.io(DRM_IOCTL_SYNCOBJ_DESTROY, &m).unwrap();
    let g = fx.ring().emit();
    o.add_point(g.clone(), 2);
    assert_eq!(count(), 1, "available");
    g.signal();
    assert_eq!(count(), 1, "signaled");
    // Not an eventfd: EINVAL; no fd: EBADF.
    m.put(0, &drm_syncobj_eventfd { handle: h, flags: 0, point: 0, fd: fx.export(&m, h).unwrap(), pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Err(EINVAL));
    m.put(0, &drm_syncobj_eventfd { handle: h, flags: 0, point: 0, fd: 999, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Err(EBADF));
}

#[test]
fn no_fd_table_and_unsupported_features() {
    let fx = Fx::new();
    let m = Mem::new();
    let h = fx.create(&m, DRM_SYNCOBJ_CREATE_SIGNALED);
    m.put(0, &drm_syncobj_handle { handle: h, flags: 0, fd: -1, pad: 0, point: 0 });
    assert_eq!(fx.f.ioctl_with(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, BASE, &m, ME, None), Err(EMFILE));
    assert_eq!(m.take::<drm_syncobj_handle>(0).fd, -1, "nothing written on failure");
}

/// FS-17: a thread asleep in epoll_wait on a pending sync_file, or on an eventfd armed by SYNCOBJ_EVENTFD, is
/// woken by the ring's signal on another thread (a lost wake shows as no event within 3 s).
#[test]
fn blocking_epoll_on_sync_file_and_eventfd_is_woken() {
    use vfs::epoll::{self, Epoll, EPOLL_CTL_ADD};
    use vfs::poll::Events;
    let fx = Fx::new();
    let m = Mem::new();
    let wait = |ep: &Arc<vfs::fd::OpenFile>| {
        let mut out: Vec<(u32, u64)> = Vec::new();
        let t0 = Instant::now();
        let n = ep.file.as_epoll().unwrap().wait(&mut out, 4, Some(now() + 3_000_000_000), &|| false).unwrap();
        (n, out, t0.elapsed())
    };
    // The sync_file of a pending ring fence.
    let h = fx.create(&m, 0);
    let f = fx.ring().emit();
    fx.obj(h).replace_fence(Some(f.clone()));
    let sfd = fx.export(&m, h).unwrap();
    let ep = epoll::new_epoll();
    Epoll::ctl(&ep, EPOLL_CTL_ADD, &fx.fds.get(sfd as usize).unwrap(), sfd, (Events::IN, 7), false).unwrap();
    let f2 = f.clone();
    let t = later(50, move || {
        f2.signal();
    });
    let (n, out, took) = wait(&ep);
    t.join().unwrap();
    assert_eq!((n, out), (1, vec![(Events::IN, 7)]), "sync_file woke after {took:?}");
    assert!(took < Duration::from_millis(2500), "{took:?}");
    assert_eq!(f.callbacks(), 0);
    // An eventfd on a syncobj without fence, armed: the point arrives and signals from another thread.
    let efile = vfs::eventfd::new_eventfd(0, vfs::eventfd::EFD_NONBLOCK);
    let efd = fx.fds.install_flags(efile.clone(), 0, true).unwrap() as i32;
    let ep2 = epoll::new_epoll();
    Epoll::ctl(&ep2, EPOLL_CTL_ADD, &efile, efd, (Events::IN, 9), false).unwrap();
    let e = fx.create(&m, 0);
    m.put(0, &drm_syncobj_eventfd { handle: e, flags: 0, point: 3, fd: efd, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Ok(0));
    let o = fx.obj(e);
    let g = fx.ring().emit();
    let g2 = g.clone();
    let t = later(50, move || {
        o.add_point(g2.clone(), 3);
        thread::sleep(Duration::from_millis(30));
        g2.signal();
    });
    let (n, out, took) = wait(&ep2);
    t.join().unwrap();
    assert_eq!((n, out), (1, vec![(Events::IN, 9)]), "eventfd woke after {took:?}");
    assert!(took < Duration::from_millis(2500), "{took:?}");
}

/// A user struct that can be read but not written back.
struct NoWrite<'a>(&'a Mem);

impl vfs::sys::UserMem for NoWrite<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.read(addr, buf)
    }
    fn write(&self, _addr: u64, _data: &[u8]) -> bool {
        false
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, vfs::Errno> {
        self.0.read_cstr(addr, max)
    }
}

/// SYNC_IOC_MERGE that cannot write its answer: EFAULT and no fd left behind (Linux installs the fd only after
/// the copy back), so no other thread's file can be closed in its place.
#[test]
fn merge_fault_leaves_no_fd() {
    let fx = Fx::new();
    let m = Mem::new();
    let h = fx.create(&m, DRM_SYNCOBJ_CREATE_SIGNALED);
    let a = fx.export(&m, h).unwrap();
    let s = fx.sync_file(a);
    let before = fx.fds.open_fds().len();
    let mut md = sync_merge_data::ZERO;
    md.fd2 = a;
    m.put(512, &md);
    assert_eq!(s.ioctl_with(SYNC_IOC_MERGE, BASE + 512, &NoWrite(&m), Some(&fx.fds)), Err(EFAULT));
    assert_eq!(fx.fds.open_fds().len(), before);
    // The next fd is the lowest free one again (the reserved number went back).
    assert_eq!(s.ioctl_with(SYNC_IOC_MERGE, BASE + 512, &m, Some(&fx.fds)), Ok(0));
    assert_eq!(m.take::<sync_merge_data>(512).fence, a + 1);
}

/// A ring that goes signals its pending fences, so an armed eventfd entry (which holds its fence) and the
/// user's eventfd are let go (the dma-fence rule: every fence signals).
#[test]
fn dropped_ring_signals_and_releases_eventfd_entries() {
    let fx = Fx::new();
    let m = Mem::new();
    let ring = crate::fake::FakeRing::new("test_ring");
    let f = ring.emit();
    let h = fx.create(&m, 0);
    fx.obj(h).replace_fence(Some(f.clone()));
    let efile = vfs::eventfd::new_eventfd(0, vfs::eventfd::EFD_NONBLOCK);
    let efd = fx.fds.install_flags(efile.clone(), 0, true).unwrap();
    m.put(0, &drm_syncobj_eventfd { handle: h, flags: 0, point: 0, fd: efd as i32, pad: 0 });
    assert_eq!(fx.io(DRM_IOCTL_SYNCOBJ_EVENTFD, &m), Ok(0));
    fx.fds.close(efd).unwrap();
    m.put(0, &drm_syncobj_destroy { handle: h, pad: 0 });
    fx.io(DRM_IOCTL_SYNCOBJ_DESTROY, &m).unwrap();
    assert_eq!(f.callbacks(), 1, "the armed entry hangs on the fence");
    assert!(Arc::strong_count(&efile.file) > 1, "and holds the eventfd");
    drop(ring);
    assert!(fence::is_signaled(&f));
    assert_eq!(f.callbacks(), 0);
    assert_eq!(Arc::strong_count(&efile.file), 1, "the entry let the eventfd go");
    let mut b = [0u8; 8];
    assert_eq!(efile.file.read_stream(&mut b, true), Ok(8));
    assert_eq!(u64::from_le_bytes(b), 1);
}
