//! DR-7 in-kernel tests (Safe, rerunnable; they need the fake GPU and skip without it): GEM_VA builds real GFX 11
//! page tables in the fake (every page of GTT, write-combined GTT and "VRAM" objects walks to its frame with the
//! right SNOOPED bit and permissions, CLEAR leaves holes, PRT walks as PRT), the memory stays until the unmap
//! ran, VMs on carve-outs of 512 MiB and 8 GiB, and the ABI program, whose output must be CachyOS's
//! (host/tests/dr/fixtures/abi-drm-va.txt, the transcript of `abi-drm-va` on Linux 7.3 with a real amdgpu).
//! Every test checks that no frame, object or dma-buf is left.

use crate::fake::{FakeGpu, FakeMemory};
use crate::fake_vm::TableWalk;
use crate::file::MinorKind;
use crate::ktests::{with_dev, Dev, KMem, BASE, KERNEL};
use crate::ktests_dr6::{close, create, put, Balance};
use crate::node::{open_device, OpenDrm};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const V: u64 = 0x1_0000_0000;
const RW: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE;

#[allow(clippy::too_many_arguments)]
fn va(f: &OpenDrm, m: &KMem, h: u32, op: u32, flags: u32, addr: u64, off: u64, size: u64) -> Result<(), String> {
    put(m, 0, &drm_amdgpu_gem_va { handle: h, operation: op, flags, va_address: addr, offset_in_bo: off, map_size: size, ..drm_amdgpu_gem_va::ZERO });
    f.ioctl_as(DRM_IOCTL_AMDGPU_GEM_VA, BASE, m, KERNEL).map(|_| ()).map_err(|e| format!("GEM_VA op {op} at {addr:#x}: errno {e}"))
}

fn walk(f: &OpenDrm, addr: u64) -> TableWalk {
    match f.drm_file().vm.backend_vm() {
        Some(v) => v.as_fake().map_or(TableWalk::NoTables, |x| x.walk(addr)),
        None => TableWalk::NoTables,
    }
}

fn tables() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::fail("not the fake GPU");
        }
        let before = Balance::now();
        let r = (|| -> TestResult {
            let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let m = KMem::new(4096);
            let g = create(&f, &m, 4 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            let wc = create(&f, &m, 2 * P, AMDGPU_GEM_DOMAIN_GTT, AMDGPU_GEM_CREATE_CPU_GTT_USWC)?;
            let v = create(&f, &m, 2 * P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
            let x = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE | AMDGPU_VM_MTYPE_UC;
            // (handle, address, flags, snooped)
            let cases = [(g, V, RW, true), (wc, V + 64 * P, RW, false), (v, 0xffff_8000_0000_0000, x, false)];
            for &(h, at, fl, snooped) in &cases {
                let bo = f.drm_file().bo(h).ok_or("no object")?;
                va(&f, &m, h, AMDGPU_VA_OP_MAP, fl, at, 0, bo.size())?;
                let exec = fl & AMDGPU_VM_PAGE_EXECUTABLE != 0;
                let write = fl & AMDGPU_VM_PAGE_WRITEABLE != 0;
                for k in 0..bo.pages() {
                    let want = bo.memory().page_phys(k).ok_or("no frame")?;
                    let w = walk(&f, at + k * P + 0x18);
                    kassert!(
                        w == TableWalk::Page { phys: want + 0x18, read: true, write, exec, snooped },
                        "object {h} page {k} at {at:#x}: {w:?}, want frame {want:#x}"
                    );
                }
            }
            // CLEAR the middle of the GTT mapping: a hole in the tables, the rest stays.
            va(&f, &m, 0, AMDGPU_VA_OP_CLEAR, 0, V + P, 0, 2 * P)?;
            kassert!(walk(&f, V + P) == TableWalk::Hole && walk(&f, V + 2 * P) == TableWalk::Hole, "cleared pages walk as holes");
            kassert!(matches!(walk(&f, V + 3 * P), TableWalk::Page { .. }), "the piece after stays");
            // PRT over whole 2 MiB blocks and more.
            va(&f, &m, 0, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT, 1 << 34, 0, 6 << 20)?;
            kassert!(walk(&f, (1 << 34) + (3 << 20)) == TableWalk::Prt, "PRT");
            // GEM_CLOSE: the bo_va and its translations go.
            close(&f, &m, g)?;
            kassert!(walk(&f, V) == TableWalk::Hole && walk(&f, V + 3 * P) == TableWalk::Hole, "closed object unmapped");
            let fake = f.drm_file().vm.backend_vm().ok_or("no VM")?;
            kassert!(fake.as_fake().is_some_and(|x| x.untabled() == 0), "every map had frames");
            drop(fake);
            drop(f);
            Ok(())
        })();
        r.and_then(|()| before.check())
    })
}
ktest!("dr.vm.tables", QemuAll, tables);

/// A delayed UNMAP keeps the memory reachable until it runs; another file's VM is apart.
fn memory_held() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::fail("not the fake GPU");
        }
        let before = Balance::now();
        let r = (|| -> TestResult {
            let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let f2 = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
            let m = KMem::new(4096);
            let h = create(&f, &m, 8 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            va(&f, &m, h, AMDGPU_VA_OP_MAP, RW, V, 0, 8 * P)?;
            va(&f, &m, h, AMDGPU_VA_OP_UNMAP, AMDGPU_VM_DELAY_UPDATE, V, 0, 8 * P)?;
            kassert!(matches!(walk(&f, V), TableWalk::Page { .. }), "a delayed UNMAP leaves the tables");
            let h2 = create(&f2, &m, P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
            va(&f2, &m, h2, AMDGPU_VA_OP_MAP, RW, V, 0, P)?;
            kassert!(walk(&f2, V + P) == TableWalk::Hole, "the second file sees only its own mapping");
            let frames = mm_core::dma_pages::live_frames();
            close(&f, &m, h)?;
            kassert!(walk(&f, V) == TableWalk::Hole, "the close ran the queued unmap");
            kassert!(mm_core::dma_pages::live_frames() < frames, "the object's frames went after the unmap");
            drop(f2);
            drop(f);
            Ok(())
        })();
        r.and_then(|()| before.check())
    })
}
ktest!("dr.vm.memory_held", QemuAll, memory_held);

/// Georg 27.10.: any carve-out size; the VM and a VRAM mapping on 512 MiB and 8 GiB.
fn carveouts() -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::fail("not the fake GPU");
        }
        let before = Balance::now();
        for carve in [512u64 << 20, 8 << 30] {
            let dev = crate::device::register(Arc::new(FakeGpu::radeon_780m_with(FakeMemory::with_carveout(carve))))
                .map_err(|e| format!("register: errno {e}"))?;
            let r = (|| -> TestResult {
                let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
                let m = KMem::new(4096);
                let a = create(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
                va(&f, &m, a, AMDGPU_VA_OP_MAP, RW, V, 0, 16 * P)?;
                kassert!(matches!(walk(&f, V + 15 * P), TableWalk::Page { snooped: false, .. }), "{carve}: VRAM page");
                Ok(())
            })();
            crate::device::unregister(&dev);
            r?;
        }
        before.check()
    })
}
ktest!("dr.vm.carveouts", QemuAll, carveouts);

const PROG: &str = "/bin/abi-drm-va";
const EXPECTED: &str = include_str!("../../../host/tests/dr/fixtures/abi-drm-va.txt");

/// The ABI program on V2: every line as on CachyOS.
fn program() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::fail("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::fail("not the fake GPU");
        }
        let before = Balance::now();
        let out = proc::stdio::Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let argv = ["abi-drm-va"];
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
ktest!("dr.vm.program", QemuAll, program);
