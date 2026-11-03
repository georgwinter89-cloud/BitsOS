//! DR-3 in-kernel tests (Safe, rerunnable): syncobj ioctls on an open of the device with a fresh fd table, a
//! wait woken by a kernel thread signaling the fake's ring, an unplug ending a wait, sync_file poll, and the
//! ABI program `/bin/abi-drm-syncobj`. On hardware without a fake (the Mini-PC) they skip, as DR-2's do.

use crate::device;
use crate::fake::FakeGpu;
use crate::file::MinorKind;
use crate::ktests::{with_dev, Dev, KMem, BASE, KERNEL};
use crate::node::{open_device, OpenDrm};
use crate::fence;
use crate::sync_file;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use linux::errno::*;
use vfs::fd::FdTable;
use vfs::File;

struct T {
    f: Arc<OpenDrm>,
    fds: Arc<FdTable>,
    m: KMem,
}

impl T {
    fn new(d: &Arc<device::Device>) -> Result<T, String> {
        let f = open_device(d.clone(), MinorKind::Render, true).map_err(|e| format!("open: errno {e}"))?;
        Ok(T { f, fds: Arc::new(FdTable::new()), m: KMem::new(8192) })
    }
    fn put<W: Wire>(&self, off: usize, v: &W) {
        let mut b = vec![0u8; W::SIZE];
        let _ = v.encode(&mut b);
        self.m.set(off, &b);
    }
    fn take<W: Wire>(&self, off: usize) -> Option<W> {
        W::decode(&self.m.at(off, W::SIZE))
    }
    fn io(&self, cmd: u32) -> Result<i64, vfs::Errno> {
        self.f.ioctl_with(cmd, BASE, &self.m, KERNEL, Some(self.fds.clone()))
    }
    fn create(&self, flags: u32) -> Result<u32, String> {
        self.put(0, &drm_syncobj_create { handle: 0, flags });
        self.io(DRM_IOCTL_SYNCOBJ_CREATE).map_err(|e| format!("create: errno {e}"))?;
        Ok(self.take::<drm_syncobj_create>(0).map(|c| c.handle).unwrap_or(0))
    }
    fn wait(&self, h: u32, flags: u32, timeout: i64) -> Result<i64, vfs::Errno> {
        self.m.set(1024, &h.to_le_bytes());
        self.put(0, &drm_syncobj_wait { handles: BASE + 1024, timeout_nsec: timeout, count_handles: 1, flags, first_signaled: 0, pad: 0, deadline_nsec: 0 });
        self.io(DRM_IOCTL_SYNCOBJ_WAIT)
    }
}

impl Drop for T {
    fn drop(&mut self) {
        self.fds.close_all();
    }
}

fn now() -> u64 {
    fence::now().unwrap_or(0)
}

fn basics() -> TestResult {
    with_dev!(d, {
        let t = T::new(&d.dev)?;
        let a = t.create(0)?;
        let s = t.create(DRM_SYNCOBJ_CREATE_SIGNALED)?;
        kassert!(a >= 1 && s > a, "handles {a} {s}");
        kassert!(t.wait(a, 0, 0) == Err(EINVAL), "no fence");
        kassert!(t.wait(a, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, 0) == Err(ETIME));
        let t0 = now();
        kassert!(t.wait(a, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, (t0 + 20_000_000) as i64) == Err(ETIME));
        kassert!(now() >= t0 + 20_000_000, "back before the deadline");
        kassert!(t.wait(s, 0, 0) == Ok(0));
        // Export and import through the fd table.
        t.put(0, &drm_syncobj_handle { handle: s, flags: DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE, fd: -1, pad: 0, point: 0 });
        kassert!(t.io(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD) == Ok(0));
        let sfd = t.take::<drm_syncobj_handle>(0).map(|h| h.fd).unwrap_or(-1);
        let of = t.fds.get(sfd as usize).map_err(|e| format!("sync_file fd {sfd}: errno {e}"))?;
        kassert!(of.file.link_name().as_deref() == Some("anon_inode:sync_file"));
        kassert!(vfs::poll::file_mask(&*of.file) & vfs::poll::Events::IN != 0, "a signaled sync_file is readable");
        t.put(0, &drm_syncobj_handle { handle: a, flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE, fd: sfd, pad: 0, point: 0 });
        kassert!(t.io(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE) == Ok(0));
        kassert!(t.wait(a, 0, 0) == Ok(0), "imported");
        kassert!(t.f.drm_file().syncobj_count() == 2);
        Ok(())
    })
}
ktest!("dr.syncobj.basics", Safe, basics);

fn ring_wait() -> TestResult {
    with_dev!(d, {
        let Some(fake) = d.dev.driver().as_fake() else { return ktest::skip("not the fake GPU") };
        let t = T::new(&d.dev)?;
        let h = t.create(0)?;
        let f = fake.ring().emit();
        t.f.drm_file().syncobj(h).ok_or("no syncobj")?.replace_fence(Some(f.clone()));
        let sfd = {
            t.put(0, &drm_syncobj_handle { handle: h, flags: DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE, fd: -1, pad: 0, point: 0 });
            t.io(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD).map_err(|e| format!("export: errno {e}"))?;
            t.take::<drm_syncobj_handle>(0).map(|x| x.fd).unwrap_or(-1)
        };
        let sf = t.fds.get(sfd as usize).ok().and_then(|o| sync_file::as_sync_file(&o.file)).ok_or("no sync_file")?;
        let seq = sf.poll_head().map(|q| q.seq()).unwrap_or(0);
        kassert!(t.wait(h, 0, 0) == Err(ETIME));
        let dev = d.dev.clone();
        let j = sched::spawn("dr3-ktest-ring", sched::Class::Normal, move || {
            sched::sleep_ns(20_000_000);
            if let Some(f) = dev.driver().as_fake() {
                f.ring().signal_all();
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        let r = t.wait(h, 0, (now() + 5_000_000_000) as i64);
        j.join_timeout(10_000_000_000);
        kassert!(r == Ok(0), "wait: {r:?}");
        kassert!(fence::is_signaled(&f) && f.callbacks() == 0, "callbacks left: {}", f.callbacks());
        kassert!(sf.poll_head().map(|q| q.seq()).unwrap_or(0) > seq, "the sync_file's pollers were not woken");
        kassert!(sf.events().0 == vfs::poll::Events::IN);
        Ok(())
    })
}
ktest!("dr.syncobj.ring_wait", Safe, ring_wait);

fn unplug_wakes() -> TestResult {
    // Its own device: only under a hypervisor, where a second fake may come and go.
    if !hal::serial::under_hypervisor() {
        return ktest::skip("adds a fake device: only under a hypervisor");
    }
    let dev = crate::add_device(Arc::new(FakeGpu::at(0xd5, 0))).map_err(|e| format!("add: errno {e}"))?;
    let r = (|| -> TestResult {
        let t = T::new(&dev)?;
        let h = t.create(0)?;
        let fake = dev.driver().as_fake().ok_or("no fake")?;
        t.f.drm_file().syncobj(h).ok_or("no syncobj")?.replace_fence(Some(fake.ring().emit()));
        let d2 = dev.clone();
        let j = sched::spawn("dr3-ktest-unplug", sched::Class::Normal, move || {
            sched::sleep_ns(20_000_000);
            crate::remove_device(&d2);
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        let t0 = now();
        let r = t.wait(h, 0, (t0 + 30_000_000_000) as i64);
        let took = now().saturating_sub(t0);
        j.join_timeout(10_000_000_000);
        kassert!(r == Err(ENODEV), "wait: {r:?}");
        // Timing is the Mini-PC list's; here only that it was not the 30 s deadline.
        kassert!(took < 10_000_000_000, "the unplug did not end the wait ({took} ns)");
        fake.ring().signal_all();
        Ok(())
    })();
    crate::remove_device(&dev);
    r
}
ktest!("dr.syncobj.unplug", Safe, unplug_wakes);

const PROG: &str = "/bin/abi-drm-syncobj";

fn program() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::skip("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    with_dev!(_d, { run_program() })
}

fn run_program() -> TestResult {
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let argv = ["abi-drm-syncobj"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {PROG}: {e}"))?;
    let ended = p.wait(Some(60_000_000_000)).is_some();
    if !ended {
        p.kill(9);
        p.wait(Some(5_000_000_000));
    }
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    kassert!(ended && p.status() == Some(proc::ExitStatus::Exited(0)), "{PROG} ended {:?}: {text}", p.status());
    kassert!(text.starts_with("amdgpu render node: found\n"), "{text}");
    let no: Vec<&str> = text.lines().filter(|l| l.ends_with(": no")).collect();
    kassert!(no.is_empty(), "{PROG}: {no:?}");
    kassert!(text.contains("wait for submit, signaled by a thread = 0\n"), "{text}");
    Ok(())
}
ktest!("dr.syncobj.program", Safe, program);

