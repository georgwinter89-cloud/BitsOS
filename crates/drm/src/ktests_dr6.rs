//! DR-6 in-kernel tests (Safe, rerunnable; they need the fake GPU, `drm=fake` or the QEMU fallback, and skip
//! without it): GEM objects on real `DmaPages` (frames, write-combined GTT, INFO's usage), PRIME export with the
//! object's pages and reservation object, import of the own export, carve-outs of 512 MiB and 8 GiB, and the ABI
//! program, whose output must be CachyOS's (host/tests/dr/fixtures/abi-drm-gem.txt, the transcript of
//! `abi-drm-gem` on Linux 7.3 with a real amdgpu), the mapping view until CR-DR6-1, and (QemuOnly) creates past
//! the pin limit and the strict commit. Every test checks that no frame, object or dma-buf is left.

use crate::dma_buf;
use crate::fake::{FakeGpu, FakeMemory};
use crate::file::MinorKind;
use crate::gem;
use crate::gpu::BoPlace;
use crate::ktests::{with_dev, Dev, KMem, BASE, KERNEL};
use crate::node::{open_device, OpenDrm};
use crate::resv::Usage;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use linux::errno::ENOMEM;
use vfs::fd::FdTable;

const P: u64 = 4096;

pub(crate) fn put<T: Wire>(m: &KMem, off: usize, v: &T) {
    let mut b = alloc::vec![0u8; T::SIZE];
    let _ = v.encode(&mut b);
    m.set(off, &b);
}

pub(crate) fn take<T: Wire>(m: &KMem, off: usize) -> Option<T> {
    T::decode(&m.at(off, T::SIZE))
}

pub(crate) fn create(f: &OpenDrm, m: &KMem, size: u64, domains: u32, flags: u32) -> Result<u32, String> {
    match create_raw(f, m, size, domains, flags) {
        Ok(h) => h.ok_or_else(|| "decode".into()),
        Err(e) => Err(format!("GEM_CREATE {size} {domains:#x}: errno {e}")),
    }
}

fn create_raw(f: &OpenDrm, m: &KMem, size: u64, domains: u32, flags: u32) -> Result<Option<u32>, vfs::Errno> {
    let mut c = drm_amdgpu_gem_create::ZERO;
    c.set_in(&drm_amdgpu_gem_create_in { bo_size: size, alignment: 0, domains: domains as u64, domain_flags: flags as u64 });
    put(m, 0, &c);
    f.ioctl_as(DRM_IOCTL_AMDGPU_GEM_CREATE, BASE, m, KERNEL)?;
    Ok(take::<drm_amdgpu_gem_create>(m, 0).map(|c| c.out().handle))
}

pub(crate) fn close(f: &OpenDrm, m: &KMem, h: u32) -> TestResult {
    put(m, 0, &drm_gem_close { handle: h, pad: 0 });
    f.ioctl_as(DRM_IOCTL_GEM_CLOSE, BASE, m, KERNEL).map_err(|e| format!("GEM_CLOSE {h}: errno {e}"))?;
    Ok(())
}

/// What a test must leave as it found: frames, objects, dma-bufs.
pub(crate) struct Balance(u64, u64, usize);

impl Balance {
    pub(crate) fn now() -> Balance {
        Balance(mm_core::dma_pages::live_frames(), gem::live(), dma_buf::live())
    }
    pub(crate) fn check(&self) -> TestResult {
        let n = Balance::now();
        kassert!(n.0 == self.0, "DMA frames {} -> {}", self.0, n.0);
        kassert!(n.1 == self.1, "GEM objects {} -> {}", self.1, n.1);
        kassert!(n.2 == self.2, "dma-bufs {} -> {}", self.2, n.2);
        Ok(())
    }
}

fn objects() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        let r = (|| -> TestResult {
            let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let m = KMem::new(4096);
            let gpu = d.dev.driver().gpu().ok_or("no GPU")?;
            let used = gpu.memory();
            let g = create(&f, &m, 3 * P + 1, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            let wc = create(&f, &m, 2 * P, AMDGPU_GEM_DOMAIN_GTT, AMDGPU_GEM_CREATE_CPU_GTT_USWC)?;
            let v = create(&f, &m, P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
            let now = gpu.memory();
            kassert!(now.gtt_usage == used.gtt_usage + 6 * P, "GTT usage {:#x} -> {:#x}", used.gtt_usage, now.gtt_usage);
            kassert!(now.vram_usage == used.vram_usage + P, "VRAM usage {:#x} -> {:#x}", used.vram_usage, now.vram_usage);
            for h in [g, wc, v] {
                let bo = f.drm_file().bo(h).ok_or("no object")?;
                for k in 0..bo.pages() {
                    kassert!(bo.memory().page_phys(k).is_some_and(|a| a % P == 0), "object {h} page {k} has no frame");
                }
                kassert!(bo.write(bo.size() - 3, b"end"), "write");
                let mut b = [0u8; 3];
                kassert!(bo.read(bo.size() - 3, &mut b) && &b == b"end", "read back");
            }
            kassert!(f.drm_file().bo(v).is_some_and(|b| b.place() == Some(BoPlace::Vram)));
            // The render node's view maps them (MM's hook, File::device_map).
            kassert!(vfs::File::device_map(&*f).is_some(), "no mapping view");
            drop(f);
            kassert!(gpu.memory().gtt_usage == used.gtt_usage, "GTT usage after close");
            Ok(())
        })();
        r.and_then(|()| before.check())
    })
}
ktest!("dr.gem.objects", Safe, objects);

fn prime() -> TestResult {
    with_dev!(d, {
        let Some(fake) = d.dev.driver().as_fake() else { return ktest::skip("not the fake GPU") };
        let before = Balance::now();
        let r = (|| -> TestResult {
            let fds = Arc::new(FdTable::new());
            let a = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let b = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let m = KMem::new(4096);
            let h = create(&a, &m, 2 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            let io = |f: &OpenDrm, cmd: u32| f.ioctl_with(cmd, BASE, &m, KERNEL, Some(fds.clone()));
            put(&m, 0, &drm_prime_handle { handle: h, flags: DRM_CLOEXEC | DRM_RDWR, fd: -1 });
            io(&a, DRM_IOCTL_PRIME_HANDLE_TO_FD).map_err(|e| format!("HANDLE_TO_FD: errno {e}"))?;
            let fd = take::<drm_prime_handle>(&m, 0).ok_or("decode")?.fd;
            let buf = dma_buf::as_dma_buf(&fds.get(fd as usize).map_err(|e| format!("fd: {e}"))?.file).ok_or("no dma-buf")?;
            let bo = a.drm_file().bo(h).ok_or("no object")?;
            kassert!(Arc::ptr_eq(buf.resv(), bo.resv()), "one reservation object");
            kassert!(buf.device_map().is_some(), "the buffer maps");
            kassert!(buf.exporter().page_phys(1) == bo.memory().page_phys(1), "the object's pages");
            put(&m, 0, &drm_prime_handle { handle: 0, flags: 0, fd });
            io(&b, DRM_IOCTL_PRIME_FD_TO_HANDLE).map_err(|e| format!("FD_TO_HANDLE: errno {e}"))?;
            let o = take::<drm_prime_handle>(&m, 0).ok_or("decode")?.handle;
            kassert!(b.drm_file().bo(o).is_some_and(|x| Arc::ptr_eq(&x, &bo)), "the same object in the second file");
            // A job's write fence: dma-buf readers wait, WAIT_IDLE says busy, then idle.
            let fence = fake.ring().emit();
            bo.fence(&fence, false).map_err(|e| format!("fence: errno {e}"))?;
            kassert!(!buf.resv().test_signaled(Usage::Write));
            let mut w = drm_amdgpu_gem_wait_idle::ZERO;
            w.set_in(&drm_amdgpu_gem_wait_idle_in { handle: o, flags: 0, timeout: 0 });
            put(&m, 0, &w);
            io(&b, DRM_IOCTL_AMDGPU_GEM_WAIT_IDLE).map_err(|e| format!("WAIT_IDLE: errno {e}"))?;
            kassert!(take::<drm_amdgpu_gem_wait_idle>(&m, 0).ok_or("decode")?.out().status == 1, "busy");
            fake.ring().signal_through(fence.seqno());
            kassert!(buf.resv().test_signaled(Usage::Read), "idle after the fence");
            drop((buf, bo));
            fds.close_all();
            drop((a, b));
            Ok(())
        })();
        r.and_then(|()| before.check())
    })
}
ktest!("dr.gem.prime", Safe, prime);

/// Carve-outs of 512 MiB and 8 GiB on a second fake (no node): usage, the fall-back to GTT when it is full.
fn carveouts() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        for carve in [512u64 << 20, 8 << 30] {
            let base = FakeMemory::with_carveout(carve);
            let mem = FakeMemory { kernel_vram_other: carve - base.kernel_vram_pinned - 16 * P, ..base };
            let dev = crate::device::register(Arc::new(FakeGpu::radeon_780m_with(mem))).map_err(|e| format!("register: errno {e}"))?;
            let r = (|| -> TestResult {
                let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
                let m = KMem::new(4096);
                let a = create(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
                let b = create(&f, &m, P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
                let gpu = dev.driver().gpu().ok_or("gpu")?;
                kassert!(gpu.memory().vram_usage == carve, "{carve}: VRAM usage {:#x}", gpu.memory().vram_usage);
                kassert!(f.drm_file().bo(a).is_some_and(|x| x.place() == Some(BoPlace::Vram)));
                kassert!(f.drm_file().bo(b).is_some_and(|x| x.place() == Some(BoPlace::Gtt)), "{carve}: full, to GTT");
                Ok(())
            })();
            crate::device::unregister(&dev);
            r?;
        }
        before.check()
    })
}
ktest!("dr.gem.carveouts", Safe, carveouts);

/// Until CR-DR6-1: the file's mapping view (`File::device_map`) holds the memory of every object it was built
/// with until it goes, and a file without objects gets a view too (it refuses every range: EINVAL as Linux,
/// not ENODEV). With the CR, mmap asks `OpenDrm::object_map` and this view is no longer used by MM.
fn view() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        let r = (|| -> TestResult {
            let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let m = KMem::new(4096);
            let frames = mm_core::dma_pages::live_frames();
            kassert!(vfs::File::device_map(&*f).is_some(), "a file without objects has a refusing view");
            kassert!(mm_core::dma_pages::live_frames() == frames + 1, "the refusing view holds one page");
            let a = create(&f, &m, 4 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            let b = create(&f, &m, P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            kassert!(mm_core::dma_pages::live_frames() == frames + 5, "a new handle drops the old view");
            let v = vfs::File::device_map(&*f).ok_or("no view")?;
            let objs = gem::live();
            close(&f, &m, a)?;
            kassert!(gem::live() == objs - 1, "the object went");
            kassert!(mm_core::dma_pages::live_frames() == frames + 5, "the view keeps its pages");
            drop(v);
            kassert!(mm_core::dma_pages::live_frames() == frames + 1, "freed with the view");
            close(&f, &m, b)?;
            drop(f);
            Ok(())
        })();
        r.and_then(|()| before.check())
    })
}
ktest!("dr.gem.view", Safe, view);

/// QemuOnly (it lowers the system's pin and commit limits for a moment): GTT objects are pinned pages, so a
/// create past the pin limit or the strict commit answers ENOMEM and leaves no frame, object or pin charge.
fn pin_limit() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        let pinned = mm::dma_pages::stats().pinned_pages;
        let r = (|| -> TestResult {
            let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let m = KMem::new(4096);
            mm::dma_pages::set_test_limit(Some(pinned + 64));
            let mut made = 0;
            let refused = loop {
                match create_raw(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_GTT, 0) {
                    Ok(_) if made < 8 => made += 1,
                    Ok(_) => return Err("no refusal past the pin limit".into()),
                    Err(e) => break e,
                }
            };
            kassert!(refused == ENOMEM && (1..=4).contains(&made), "refused with errno {refused} after {made}");
            let (frames, objs) = (mm_core::dma_pages::live_frames(), gem::live());
            kassert!(create_raw(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_GTT, 0) == Err(ENOMEM));
            kassert!(mm_core::dma_pages::live_frames() == frames && gem::live() == objs, "a refusal left something");
            mm::dma_pages::set_test_limit(None);
            mm::commit::set_test_limit(Some(mm::commit::committed() + 8));
            kassert!(create_raw(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_GTT, 0) == Err(ENOMEM), "the strict commit");
            mm::commit::set_test_limit(None);
            kassert!(create_raw(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_GTT, 0).is_ok(), "room again");
            drop(f);
            Ok(())
        })();
        mm::dma_pages::set_test_limit(None);
        mm::commit::set_test_limit(None);
        r?;
        let now = mm::dma_pages::stats().pinned_pages;
        kassert!(now == pinned, "pin charge {pinned} -> {now}");
        before.check()
    })
}
ktest!("dr.gem.pin_limit", QemuOnly, pin_limit);

const PROG: &str = "/bin/abi-drm-gem";
const EXPECTED: &str = include_str!("../../../host/tests/dr/fixtures/abi-drm-gem.txt");

/// The ABI program on V2: every line as on CachyOS.
fn program() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::skip("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        let out = proc::stdio::Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let argv = ["abi-drm-gem"];
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
        let diff: Vec<(&str, &str)> = text.lines().zip(EXPECTED.lines()).filter(|(a, b)| a != b).collect();
        kassert!(diff.is_empty() && text.lines().count() == EXPECTED.lines().count(), "{PROG} differs from Linux: {diff:?}");
        drop(p);
        before.check()
    })
}
ktest!("dr.gem.program", Safe, program);
