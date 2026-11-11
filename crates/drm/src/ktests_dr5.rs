//! DR-5 in-kernel tests (Safe, rerunnable, no GPU needed): /dev/udmabuf, a udmabuf on real page-cache frames,
//! a DmaPages buffer with its device mapping, fences and poll wakes, and the ABI program, whose output must be
//! CachyOS's (host/tests/dr/fixtures/abi-dmabuf.txt, the transcript of `cargo xtask abi-diff abi-dmabuf`).

use crate::dma_buf::{self, DmaBuf, PagesExporter};
use crate::fence::{self, Fence};
use crate::ktests::{KMem, BASE};
use crate::resv::Usage;
use crate::sync_file::{self, SyncFile};
use crate::udmabuf::{self, Item, UDMABUF_CREATE};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use linux::errno::*;
use vfs::fd::{FdTable, OpenFile};
use vfs::poll::Events;
use vfs::File;

const P: u64 = 4096;

fn node() -> TestResult {
    let dn = vfs::root_ns().lookup("/dev/udmabuf").map_err(|e| format!("lookup /dev/udmabuf: errno {e}"))?;
    let f = vfs::node::file_of(&dn);
    kassert!(f.mode() == linux::S_IFCHR | 0o666, "mode {:o}", f.mode());
    kassert!(f.rdev() == vfs::abi::encode_dev(10, 258), "rdev {:#x}", f.rdev());
    let m = KMem::new(64);
    kassert!(f.ioctl_user(0x4008_7544, BASE, &m) == Some(Err(ENOTTY)));
    Ok(())
}
ktest!("dr.udmabuf.node", Safe, node);

fn memfd(fds: &FdTable, pages: u64) -> Result<(Arc<OpenFile>, u32), String> {
    let f = vfs::memfd::create(b"ktest-dr5", vfs::memfd::MFD_ALLOW_SEALING, vfs::inode::Cred::ROOT).map_err(|e| format!("memfd: errno {e}"))?;
    f.inode().ok_or("memfd without inode")?.truncate(pages * P).map_err(|e| format!("truncate: errno {e}"))?;
    vfs::memfd::seal_fcntl(&f, vfs::memfd::F_ADD_SEALS, vfs::memfd::F_SEAL_SHRINK as u64).map_err(|e| format!("seal: errno {e}"))?;
    let n = fds.install_flags(f.clone(), 0, false).map_err(|e| format!("install: errno {e}"))?;
    Ok((f, n as u32))
}

fn shared_pages() -> TestResult {
    let before = dma_buf::live();
    let r = (|| -> TestResult {
        let fds = FdTable::new();
        let (f, n) = memfd(&fds, 3)?;
        let m = KMem::new(64);
        let mut c = [0u8; 24];
        c[0..4].copy_from_slice(&n.to_le_bytes());
        c[16..24].copy_from_slice(&(3 * P).to_le_bytes());
        m.set(0, &c);
        let fd = udmabuf::ioctl_with(UDMABUF_CREATE, BASE, &m, Some(&fds)).map_err(|e| format!("create: errno {e}"))?;
        let b = dma_buf::as_dma_buf(&fds.get(fd as usize).map_err(|e| format!("fd: {e}"))?.file).ok_or("no dma-buf")?;
        kassert!(b.size() == 3 * P);
        for k in 0..3 {
            kassert!(b.exporter().page_phys(k).is_some_and(|a| a % P == 0), "page {k} has no frame");
        }
        kassert!(b.exporter().page_phys(3).is_none());
        f.file.write_at(2 * P + 7, b"ktest").map_err(|e| format!("write: errno {e}"))?;
        let mut got = [0u8; 5];
        kassert!(b.exporter().read(2 * P + 7, &mut got) && &got == b"ktest");
        kassert!(b.exporter().write(P, b"back"));
        let mut back = [0u8; 4];
        f.file.read_at(P, &mut back).map_err(|e| format!("read: errno {e}"))?;
        kassert!(&back == b"back");
        // A list of two items of one memfd: pages in item order.
        let items = [Item { memfd: n, offset: 2 * P, size: P }, Item { memfd: n, offset: P, size: P }];
        let l = udmabuf::build(Some(&fds), &items).map_err(|e| format!("list: errno {e}"))?;
        kassert!(l.exporter().page_phys(0) == b.exporter().page_phys(2) && l.exporter().page_phys(1) == b.exporter().page_phys(1));
        fds.close_all();
        Ok(())
    })();
    kassert!(dma_buf::live() == before, "buffers left: {} -> {}", before, dma_buf::live());
    r
}
ktest!("dr.udmabuf.shared", Safe, shared_pages);

fn pages_buffer() -> TestResult {
    let pages = match mm::dma_pages::DmaPages::alloc(4, Default::default()) {
        Ok(p) => p,
        Err(e) => return Err(format!("DmaPages: {e:?}")),
    };
    {
        let b = DmaBuf::export(Arc::new(PagesExporter { pages: pages.clone(), name: "ktest" }), None).map_err(|e| format!("export: errno {e}"))?;
        kassert!(b.size() == 4 * P && b.device_map().is_some(), "a DmaPages buffer is mappable");
        kassert!(b.exporter().write(3 * P + 1, b"dr5"));
        let mut got = [0u8; 3];
        kassert!(pages.read(3 * P + 1, &mut got) && &got == b"dr5");
        kassert!(b.exporter().page_phys(2) == pages.page_phys(2));
        let mut s = String::new();
        b.show_fdinfo(&mut s);
        kassert!(s.contains("exp_name:\tktest\n") && s.starts_with("size:\t16384\n"), "{s}");
    }
    kassert!(Arc::strong_count(&pages) == 1, "the buffer and its mapping let the pages go");
    Ok(())
}
ktest!("dr.dmabuf.pages", Safe, pages_buffer);

fn fences_and_poll() -> TestResult {
    let fds = FdTable::new();
    let (_f, n) = memfd(&fds, 1)?;
    let b = udmabuf::build(Some(&fds), &[Item { memfd: n, offset: 0, size: P }]).map_err(|e| format!("build: errno {e}"))?;
    let w = Fence::new(fence::context_alloc(1), 1, "ktest", "dr5");
    let wfd = sync_file::install(Some(&fds), SyncFile::new(w.clone(), b"")).map_err(|e| format!("install: errno {e}"))?;
    let m = KMem::new(64);
    let mut a = [0u8; 8];
    let _ = dma_buf_import_sync_file { flags: DMA_BUF_SYNC_WRITE, fd: wfd }.encode(&mut a);
    m.set(0, &a);
    kassert!(b.ioctl_with(DMA_BUF_IOCTL_IMPORT_SYNC_FILE, BASE, &m, Some(&fds)) == Ok(0));
    kassert!(b.events().0 == 0, "a pending writer: {:#x}", b.events().0);
    let _ = dma_buf_export_sync_file { flags: DMA_BUF_SYNC_READ, fd: -1 }.encode(&mut a);
    m.set(0, &a);
    kassert!(b.ioctl_with(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, BASE, &m, Some(&fds)) == Ok(0));
    let efd = i32::from_le_bytes(m.at(4, 4).try_into().unwrap_or([0xff; 4]));
    let ef = fds.get(efd as usize).map_err(|e| format!("exported fd: {e}"))?;
    let s = sync_file::as_sync_file(&ef.file).ok_or("no sync_file")?;
    kassert!(Arc::ptr_eq(s.fence(), &w));
    let seq = b.poll_head().map(|q| q.seq()).unwrap_or(0);
    w.signal();
    kassert!(b.events().0 == Events::IN | Events::OUT);
    kassert!(b.poll_head().map(|q| q.seq()).unwrap_or(0) > seq, "the signal woke the buffer's queue");
    kassert!(b.resv().test_signaled(Usage::Read));
    drop(s);
    drop(ef);
    fds.close_all();
    drop(b);
    kassert!(w.callbacks() == 0, "callbacks left: {}", w.callbacks());
    Ok(())
}
ktest!("dr.dmabuf.fences", Safe, fences_and_poll);

const PROG: &str = "/bin/abi-dmabuf";
const EXPECTED: &str = include_str!("../../../host/tests/dr/fixtures/abi-dmabuf.txt");

const EXPECTED_MMAP: &str = include_str!("../../../host/tests/dr/fixtures/abi-dmabuf-mmap.txt");

fn program() -> TestResult {
    run(&["abi-dmabuf"], EXPECTED)
}
ktest!("dr.dmabuf.program", Safe, program);

/// The mapping cases, once MM maps dma-buf fds (CR-DR5-1/2); skipped before.
fn program_mmap() -> TestResult {
    let fds = FdTable::new();
    let (_f, n) = memfd(&fds, 1)?;
    let b = udmabuf::build(Some(&fds), &[Item { memfd: n, offset: 0, size: P }]).map_err(|e| format!("build: errno {e}"))?;
    let of = OpenFile::anon(b, vfs::abi::O_RDWR);
    if mm::device::lookup(&of).is_none() {
        return ktest::fail("MM cannot map dma-buf fds yet (CR-DR5-1/2, plan/notes/dr-DR-5.md)");
    }
    run(&["abi-dmabuf", "mmap"], EXPECTED_MMAP)
}
ktest!("dr.dmabuf.program_mmap", Safe, program_mmap);

fn run(argv: &[&str], expected: &str) -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::fail("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
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
    let diff: Vec<(&str, &str)> = text.lines().zip(expected.lines()).filter(|(a, b)| a != b).collect();
    kassert!(diff.is_empty() && text.lines().count() == expected.lines().count(), "{PROG} {argv:?} differs from Linux: {diff:?}");
    Ok(())
}
