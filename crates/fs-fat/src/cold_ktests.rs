//! FS-14b in-kernel test of cold reads from the boot stick (class Safe: reads only, rerunnable; the Mini-PC
//! runs it over MCP right after a boot):
//! - `fs.fat.cold`: the largest file of the stick that is (almost) not cached yet is mapped by two "processes"
//!   (two file mappings, PROT_READ|PROT_EXEC, MAP_PRIVATE) whose threads fault every page at the same time in
//!   different orders (one jumps around the file, one goes forward from the middle), as Brave's zygote and
//!   renderers fault its 328 MB binary; the next such file is read with read() in 4 KiB pieces. The stick
//!   disk's read bytes (`usb.msd.read_bytes` on the Mini-PC) grow by at most 1.2 x the bytes that were not
//!   cached; a warm pass over both files reads (almost) nothing. A rerun picks files still cold; when every
//!   big file is cached already it skips (reboot for a cold measurement).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use sched::Class;
use vfs::abi::O_RDONLY;
use vfs::mmap::{self, MapBacking, MAP_PRIVATE, PROT_EXEC};
use vfs::ops::Ctx;

const P: u64 = 4096;
const MS: u64 = 1_000_000;
/// Files from 1 MiB to 512 MiB (QEMU has 2 GiB; nothing evicts the cache).
const MIN: u64 = 1 << 20;
const MAX: u64 = 512 << 20;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// A cold candidate: (path, size, pages cached now).
type Cand = (String, u64, u64);

/// Regular files of `MIN..=MAX` bytes with at most a tenth of their pages cached, largest first (`data.img`,
/// the /data pool, is left alone).
/// The cold files (largest first) and how many files of 1..512 MiB the stick has in all.
fn cold_files(ns: &vfs::MountNs) -> Result<(Vec<Cand>, u64), String> {
    let mut out = Vec::new();
    let mut seen = 0u64;
    let mut work = vec![String::from("/boot")];
    while let Some(dir) = work.pop() {
        for (name, a) in ns.list_dir(&dir).map_err(|x| format!("{dir}: {x}"))? {
            let p = format!("{dir}/{}", String::from_utf8_lossy(&name));
            if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
                work.push(p);
                continue;
            }
            if a.mode & uapi::S_IFMT != uapi::S_IFREG || a.size < MIN || a.size > MAX || name.starts_with(b"data.img") {
                continue;
            }
            seen += 1;
            let d = ns.lookup(&p).map_err(|x| format!("{p}: {x}"))?;
            let Some(cache) = d.inode.page_cache() else { continue };
            let cached = cache.pages();
            if cached * 10 <= a.size.div_ceil(P) {
                out.push((p, a.size, cached));
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    Ok((out, seen))
}

fn read_bytes(d: &Option<Arc<block::Disk>>) -> u64 {
    d.as_ref().map_or(0, |d| d.stats().read_bytes)
}

/// Faults every page of `map` (pages `n`) in the order `order(i)`; Err names the first page that failed.
fn fault_all(map: Arc<vfs::mmap::FileMap>, n: u64, order: impl Fn(u64) -> u64) -> Result<(), String> {
    for i in 0..n {
        let pg = order(i);
        let phys = map.get_page(pg, false).map_err(|x| format!("page {pg}: {x:?}"))?;
        map.put_page(phys);
    }
    Ok(())
}

fn fat_cold() -> TestResult {
    if crate::stick::wait_mounted(40_000).is_none() {
        klog::event!(Info, "fat", "ktest.cold_skipped", reason = "no boot stick mounted");
        return ktest::skip("no boot stick mounted");
    }
    let disk = block::disks().into_iter().find(|d| d.kind() == block::DiskKind::Usb && d.stick() == block::StickMatch::Yes);
    kassert!(disk.is_some(), "no USB disk matches the stick");
    let ns = vfs::root_ns();
    let (cands, seen) = cold_files(&ns)?;
    let Some((exe_path, exe_size, exe_cached)) = cands.first().cloned() else {
        klog::event!(Info, "fat", "ktest.cold_skipped", reason = "no cold file", files_1_512mib = seen;
            "every file of 1..512 MiB on the stick is cached: run the test right after a reboot");
        return ktest::skip("no cold file of 1..512 MiB on the stick (all cached: reboot for a cold measurement)");
    };
    let c = Ctx::kernel(&ns);
    let exe = c.open(None, exe_path.as_bytes(), O_RDONLY, 0).map_err(|x| format!("{exe_path}: {x}"))?;
    let pak = cands.get(1).cloned();
    let pc_before = mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageCache as usize]);
    let t0 = time::now_ns();
    let before = read_bytes(&disk);

    // Two mappings faulted by two threads at once, every page, in different orders.
    let n = exe_size.div_ceil(P);
    let mut handles = Vec::new();
    let results: Arc<ksync::SpinLock<Vec<Result<(), String>>>> = Arc::new(ksync::SpinLock::new(Vec::new()));
    for k in 0..2u64 {
        let map = mmap::check(&exe, 1 | PROT_EXEC, MAP_PRIVATE, 0, exe_size).map_err(|x| format!("map: {x}"))?.backing;
        let r = results.clone();
        let h = sched::spawn("fs14b-fault", Class::Normal, move || {
            let res = if k == 0 { fault_all(map, n, |i| (i * 7919) % n) } else { fault_all(map, n, |i| (n / 2 + i) % n) };
            r.lock().push(res);
        })
        .map_err(e)?;
        handles.push(h);
    }
    let joined = handles.iter().all(|h| h.join_timeout(300_000 * MS));
    kassert!(joined, "the fault threads did not finish within 300 s");
    for r in results.lock().drain(..) {
        r?;
    }
    let exe_dev = read_bytes(&disk) - before;
    let t_exe = time::now_ns() - t0;

    // read() of the next cold file in 4 KiB pieces.
    let (mut pak_dev, mut pak_new) = (0, 0);
    if let Some((p, size, cached)) = &pak {
        let f = c.open(None, p.as_bytes(), O_RDONLY, 0).map_err(|x| format!("{p}: {x}"))?;
        let b0 = read_bytes(&disk);
        let mut buf = vec![0u8; P as usize];
        let mut at = 0;
        while at < *size {
            let got = f.pread(at, &mut buf).map_err(|x| format!("{p} at {at}: {x}"))?;
            kassert!(got > 0, "{p}: short read at {at} of {size}");
            at += got as u64;
        }
        pak_dev = read_bytes(&disk) - b0;
        pak_new = (size.div_ceil(P) - cached) * P;
    }
    let exe_new = (n - exe_cached) * P;
    let pc_after = mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageCache as usize]);

    // Warm: the same again from the cache.
    let w0 = read_bytes(&disk);
    let map = mmap::check(&exe, 1 | PROT_EXEC, MAP_PRIVATE, 0, exe_size).map_err(|x| format!("map: {x}"))?.backing;
    fault_all(map, n, |i| i)?;
    if let Some((p, size, _)) = &pak {
        let f = c.open(None, p.as_bytes(), O_RDONLY, 0).map_err(|x| format!("{p}: {x}"))?;
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0;
        while at < *size {
            at += f.pread(at, &mut buf).map_err(|x| format!("{p} at {at}: {x}"))?.max(1) as u64;
        }
    }
    let warm = read_bytes(&disk) - w0;

    klog::event!(Info, "fat", "ktest.cold", exe = exe_path.as_str(), exe_bytes = exe_new, exe_device = exe_dev, exe_ms = t_exe / MS,
        pak = pak.as_ref().map_or("-", |p| p.0.as_str()), pak_bytes = pak_new, pak_device = pak_dev, warm_device = warm,
        pagecache_grew = pc_after.saturating_sub(pc_before));
    let limit = |b: u64| b * 12 / 10 + (64 << 10);
    kassert!(exe_dev <= limit(exe_new), "{exe_path}: {exe_dev} bytes from the stick for {exe_new} cold bytes faulted by two mappings");
    kassert!(pak_dev <= limit(pak_new), "{:?}: {pak_dev} bytes from the stick for {pak_new} cold bytes read()", pak.as_ref().map(|p| &p.0));
    kassert!(warm <= (exe_new + pak_new) / 100 + (256 << 10), "the warm pass read {warm} bytes from the stick");
    // `mem.used.pagecache` is Kind::PageCache: it grew by the pages this test cached.
    kassert!(pc_after >= pc_before + (exe_new + pak_new) * 9 / 10, "Kind::PageCache grew {pc_before} -> {pc_after} for {} cached bytes", exe_new + pak_new);
    Ok(())
}
ktest!("fs.fat.cold", Safe, fat_cold);
