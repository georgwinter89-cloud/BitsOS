//! ktests of the address spaces (MM-5a): lazy fill, guards and the fault reasons, splitting and merging,
//! brk, a user program touching memory it mapped, the ABI script against Linux's answers, eager fill for
//! callers without the fault path, and that frames and tables come back.

use crate::abi::{self, MemApi};
use crate::linux::{Errno, MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE};
use crate::{AddressSpace, FaultReason, Fill, Prot, VmaKind};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::paging::{self, PageFlags};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const PF_W: u64 = 2;
const PF_U: u64 = 4;
const PF_I: u64 = 16;

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|e| format!("address space: {e:?}"))
}

fn present(a: &AddressSpace, va: u64) -> bool {
    paging::translate(a.root(), va).is_some()
}

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy_fill() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(0, 64 * P, RW, AP, 0).map_err(e)?;
    kassert!(a.resident_pages() == 0, "{} pages filled at mmap", a.resident_pages());
    a.copy_to_user(m + 3 * P + 10, b"lazy").map_err(e)?;
    kassert!(a.resident_pages() == 1 && present(&a, m + 3 * P), "a copy fills exactly its page");
    let mut z = [0xFFu8; 16];
    a.copy_from_user(&mut z, m + 10 * P - 8).map_err(e)?;
    kassert!(z == [0; 16], "fresh pages read as zero");
    kassert!(a.resident_pages() == 3, "the read crossed into two more pages ({})", a.resident_pages());
    a.handle_user_fault(m + 20 * P + 5, PF_U | PF_W).map_err(e)?;
    let (_, fl) = paging::translate(a.root(), m + 20 * P).ok_or("fault did not map the page")?;
    kassert!(fl.contains(PageFlags::USER | PageFlags::WRITABLE | PageFlags::NX));
    kassert!(a.handle_user_fault(m + 20 * P, PF_U | PF_W | 1).is_ok(), "a spurious fault on a fixed page is fine");
    let mut back = [0u8; 4];
    a.copy_from_user(&mut back, m + 3 * P + 10).map_err(e)?;
    kassert!(&back == b"lazy");
    let s = a.read_cstr(m + 3 * P + 10, 64).map_err(e)?;
    kassert!(s == b"lazy", "read_cstr stops at the NUL: {s:?}");
    kassert!(a.resident_pages() == 4);
    // A reservation costs nothing: 64 GiB PROT_NONE (Chromium-style) with one page made usable.
    let r = a.sys_mmap(0, 64 << 30, 0, AP, 0).map_err(e)?;
    a.sys_mprotect(r + (32 << 30), P, RW).map_err(e)?;
    a.write_u64(r + (32 << 30), 0x5EED).map_err(e)?;
    kassert!(a.read_u64(r + (32 << 30)).map_err(e)? == 0x5EED);
    kassert!(a.resident_pages() == 5, "{} pages after the reservation", a.resident_pages());
    Ok(())
}
ktest!("mm.as.lazy_fill", Safe, lazy_fill);

fn guards() -> TestResult {
    let a = lazy()?;
    let base = 0x10_0000_0000u64;
    a.sys_mmap(base, 8 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.write_u64(base + 4 * P, 0xAB).map_err(e)?;
    a.sys_mprotect(base + 4 * P, P, 0).map_err(e)?;
    let f = |addr: u64, err: u64| a.handle_user_fault(addr, err);
    kassert!(f(base + 4 * P, PF_U | 1) == Err(FaultReason::ProtNone), "the guard page");
    kassert!(f(base - 8, PF_U | PF_W) == Err(FaultReason::NotMapped), "just below the mapping");
    kassert!(f(base + 8 * P, PF_U) == Err(FaultReason::NotMapped), "just above it");
    kassert!(f(hal::USER_TOP, PF_U) == Err(FaultReason::KernelAddress));
    kassert!(f(base, PF_U | PF_I) == Err(FaultReason::NoExec));
    kassert!(f(base, PF_U | 8) == Err(FaultReason::Unexpected), "reserved bit");
    a.sys_mprotect(base + 6 * P, P, 1).map_err(e)?;
    kassert!(f(base + 6 * P, PF_U | PF_W) == Err(FaultReason::ReadOnly), "write to a read-only page");
    kassert!(a.copy_to_user(base + 6 * P, b"x").is_err(), "a copy into it fails too");
    kassert!(a.copy_from_user(&mut [0u8; 1], base + 4 * P).is_err(), "a copy from PROT_NONE fails");
    kassert!(a.copy_from_user(&mut [0u8; 1], base + 6 * P).is_ok(), "reading the read-only page works");
    a.sys_mprotect(base + 4 * P, P, RW).map_err(e)?;
    kassert!(a.read_u64(base + 4 * P).map_err(e)? == 0xAB, "content survives PROT_NONE");
    // A page filled while PROT_NONE is impossible from user space; the loader can still not write there.
    a.sys_mprotect(base + 7 * P, P, 0).map_err(e)?;
    kassert!(a.write_loader(base + 7 * P, b"x").is_err());
    Ok(())
}
ktest!("mm.as.guard", Safe, guards);

fn ranges(a: &AddressSpace, lo: u64, hi: u64) -> Vec<(u64, u64, Prot)> {
    a.vmas().into_iter().filter(|v| v.start < hi && v.end > lo).map(|v| (v.start - lo, v.end - lo, v.prot)).collect()
}

fn splitting() -> TestResult {
    let a = lazy()?;
    let b = 0x20_0000_0000u64;
    let ro = Prot { read: true, write: false, exec: false };
    a.sys_mmap(b, 10 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..10 {
        a.write_u64(b + i * P, i + 1).map_err(e)?;
    }
    a.sys_munmap(b + 3 * P, 2 * P).map_err(e)?;
    kassert!(ranges(&a, b, b + 10 * P) == [(0, 3 * P, Prot::rw()), (5 * P, 10 * P, Prot::rw())]);
    kassert!(!present(&a, b + 3 * P) && !present(&a, b + 4 * P), "pages of the hole are gone");
    a.sys_mprotect(b + 6 * P, P, 1).map_err(e)?;
    kassert!(ranges(&a, b, b + 10 * P).len() == 4, "{:?}", ranges(&a, b, b + 10 * P));
    kassert!(a.sys_mprotect(b + P, 6 * P, 1) == Err(Errno::NoMem), "a hole in the range");
    kassert!(ranges(&a, b, b + 10 * P)[..2] == [(0, P, Prot::rw()), (P, 3 * P, ro)], "changed up to the hole");
    a.sys_mprotect(b, 3 * P, RW).map_err(e)?;
    a.sys_mprotect(b + 5 * P, 5 * P, RW).map_err(e)?;
    kassert!(ranges(&a, b, b + 10 * P) == [(0, 3 * P, Prot::rw()), (5 * P, 10 * P, Prot::rw())], "merged back");
    kassert!(a.read_u64(b + 9 * P).map_err(e)? == 10 && a.read_u64(b + 2 * P).map_err(e)? == 3, "contents stay");
    kassert!(a.sys_mmap(b + 2 * P, 2 * P, RW, AP | MAP_FIXED_NOREPLACE, 0) == Err(Errno::Exist));
    a.sys_mmap(b + 2 * P, 4 * P, 1, AP | MAP_FIXED, 0).map_err(e)?;
    kassert!(a.read_u64(b + 2 * P).map_err(e)? == 0 && a.read_u64(b + 5 * P).map_err(e)? == 0, "MAP_FIXED gives fresh pages");
    kassert!(a.read_u64(b + 6 * P).map_err(e)? == 7);
    kassert!(ranges(&a, b, b + 10 * P) == [(0, 2 * P, Prot::rw()), (2 * P, 6 * P, ro), (6 * P, 10 * P, Prot::rw())]);
    let h = a.sys_mmap(b + 3 * P, P, RW, AP, 0).map_err(e)?;
    kassert!(h != b + 3 * P && !(b..b + 10 * P).contains(&h), "a taken hint is not used: {h:#x}");
    a.sys_munmap(b, 10 * P).map_err(e)?;
    kassert!(ranges(&a, b, b + 10 * P).is_empty());
    kassert!(paging::count_user_leaves(a.root(), b, b + 10 * P) == 0);
    Ok(())
}
ktest!("mm.as.split", Safe, splitting);

fn brk() -> TestResult {
    let a = lazy()?;
    let h = 0x30_0000_0000u64;
    a.init_brk(h);
    kassert!(a.brk(0) == h);
    kassert!(a.brk(h + 0x3001) == h + 0x3001);
    a.copy_to_user(h + 0x3000, &[0x11]).map_err(e)?;
    kassert!(a.copy_to_user(h + 0x4000, &[1]).is_err(), "past the heap");
    kassert!(a.brk(h + 0x1000) == h + 0x1000, "shrink");
    kassert!(!present(&a, h + 0x3000), "shrinking gives the pages back");
    kassert!(a.brk(h - 0x1000) == h + 0x1000, "below the start");
    a.sys_mmap(h + 0x8000, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    kassert!(a.brk(h + 0x7000) == h + 0x7000);
    kassert!(a.brk(h + 0x7001) == h + 0x7000, "one free page must stay before the next mapping");
    let mut b = [1u8; 1];
    a.copy_from_user(&mut b, h + 0x6000).map_err(e)?;
    kassert!(b == [0], "regrown heap is zero");
    kassert!(a.vmas().iter().any(|v| v.start == h && v.end == h + 0x7000 && v.kind == VmaKind::Heap));
    kassert!(a.brk(h) == h);
    kassert!(paging::count_user_leaves(a.root(), h, h + 0x8000) == 0);
    Ok(())
}
ktest!("mm.as.brk", Safe, brk);

/// `crates/mm/abi/touch.asm` assembled with NASM 2.16 (`nasm -f bin`).
const TOUCH: [u8; 180] = [
    0xb8, 0x09, 0x00, 0x00, 0x00, 0x31, 0xff, 0xbe, 0x00, 0x00, 0x01, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x22,
    0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0x0f, 0x05, 0x48, 0x89, 0xc3, 0x48, 0xc7,
    0x03, 0x11, 0x11, 0x00, 0x00, 0x48, 0xc7, 0x83, 0x00, 0x80, 0x00, 0x00, 0x22, 0x22, 0x00, 0x00, 0x48, 0x8b, 0x8b, 0x08,
    0xf0, 0x00, 0x00, 0x48, 0x03, 0x0b, 0x48, 0x03, 0x8b, 0x00, 0x80, 0x00, 0x00, 0x48, 0x89, 0xcf, 0x48, 0x89, 0xde, 0xb8,
    0xe8, 0x03, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x0c, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0x49, 0x89, 0xc4, 0x48, 0x8d,
    0xb8, 0x00, 0x30, 0x00, 0x00, 0xb8, 0x0c, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x41, 0xc6, 0x84, 0x24, 0xff, 0x2f, 0x00, 0x00,
    0x77, 0x41, 0x0f, 0xb6, 0xbc, 0x24, 0xff, 0x2f, 0x00, 0x00, 0x4c, 0x89, 0xe6, 0xb8, 0xe9, 0x03, 0x00, 0x00, 0x0f, 0x05,
    0x48, 0x8d, 0xbb, 0x00, 0x40, 0x00, 0x00, 0xbe, 0x00, 0x10, 0x00, 0x00, 0x31, 0xd2, 0xb8, 0x0a, 0x00, 0x00, 0x00, 0x0f,
    0x05, 0x48, 0x8b, 0x83, 0x00, 0x40, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0x0f, 0x05,
];
const USER_CODE: u64 = 0x40_0000;
const USER_HEAP: u64 = 0x80_0000;

/// What the user program reported and how it ended.
#[derive(Default, Debug)]
struct Touch {
    sum: Option<(u64, u64)>,
    heap: Option<(u64, u64)>,
    end: Option<(u64, FaultReason)>,
    fills: u64,
    other: Option<String>,
}

fn run_touch(a: &AddressSpace) -> Touch {
    use hal::entry::{ReturnReason, UserContext};
    let mut t = Touch::default();
    let mut ctx = UserContext::new(USER_CODE, 0);
    for _ in 0..1000 {
        match ctx.execute(false) {
            ReturnReason::Syscall => {
                let [x0, x1, x2, x3, x4, x5] = ctx.syscall_args();
                let r: i64 = match ctx.regs.rax {
                    9 => a.sys_mmap(x0, x1, x2, x3, x5).map(|v| v as i64).unwrap_or_else(|e| -e.code()),
                    10 => a.sys_mprotect(x0, x1, x2).map(|_| 0).unwrap_or_else(|e| -e.code()),
                    11 => a.sys_munmap(x0, x1).map(|_| 0).unwrap_or_else(|e| -e.code()),
                    12 => a.sys_brk(x0) as i64,
                    1000 => {
                        t.sum = Some((x0, x1));
                        0
                    }
                    1001 => {
                        t.heap = Some((x0, x1));
                        0
                    }
                    n => {
                        let _ = x4;
                        t.other = Some(format!("syscall {n}"));
                        return t;
                    }
                };
                ctx.set_syscall_return(r);
            }
            ReturnReason::Trap { vector: 14, error, cr2 } => match a.handle_user_fault(cr2, error) {
                Ok(()) => t.fills += 1,
                Err(why) => {
                    t.end = Some((cr2, why));
                    return t;
                }
            },
            ReturnReason::Trap { vector, error, cr2 } => {
                t.other = Some(format!("trap {vector} error {error:#x} cr2 {cr2:#x} rip {:#x}", ctx.regs.rip));
                return t;
            }
        }
    }
    t.other = Some(String::from("did not end within 1000 returns"));
    t
}

fn user_touch() -> TestResult {
    let fa = mm_core::frames().ok_or("no frame allocator")?;
    let user = || fa.stats().used_by_kind[mm_core::Kind::User as usize];
    let before = user();
    user_touch_in(lazy()?)?;
    // Dropped, it frees everything also while the APs run (MM-6: they never loaded it; a CPU that did would
    // leave it first).
    kassert!(user() == before, "{} bytes of user frames kept after the drop", user().saturating_sub(before));
    Ok(())
}

fn user_touch_in(a: AddressSpace) -> TestResult {
    a.map_fixed(USER_CODE, P, Prot { read: true, write: false, exec: true }, VmaKind::Image).map_err(e)?;
    a.write_loader(USER_CODE, &TOUCH).map_err(e)?;
    a.init_brk(USER_HEAP);
    let kernel = paging::home_root();
    a.activate();
    let t = run_touch(&a);
    paging::activate(kernel);
    if let Some(o) = t.other {
        return Err(format!("user program: {o} ({:?})", t.end));
    }
    let (sum, base) = t.sum.ok_or("no first report")?;
    kassert!(sum == 0x3333, "the program read {sum:#x} back (want 0x3333)");
    kassert!(t.heap == Some((0x77, USER_HEAP)), "heap report {:?}", t.heap);
    kassert!(t.end == Some((base + 0x4000, FaultReason::ProtNone)), "ended with {:?}", t.end);
    kassert!(t.fills >= 4, "only {} faults filled pages (three mmap pages and the heap page)", t.fills);
    kassert!(paging::count_user_leaves(a.root(), base, base + 0x10000) == 3, "only the touched pages have frames");
    kassert!(a.read_u64(base + 0x8000).map_err(e)? == 0x2222);
    Ok(())
}
ktest!("mm.as.user_touch", Safe, user_touch);

/// The script against a real address space; reads and writes go through the user copies.
impl MemApi for AddressSpace {
    fn mmap(&mut self, addr: u64, len: u64, prot: u64, flags: u64) -> Result<u64, Errno> {
        self.sys_mmap(addr, len, prot, flags, 0)
    }
    fn munmap(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        self.sys_munmap(addr, len)
    }
    fn mprotect(&mut self, addr: u64, len: u64, prot: u64) -> Result<(), Errno> {
        self.sys_mprotect(addr, len, prot)
    }
    fn brk(&mut self, addr: u64) -> u64 {
        self.sys_brk(addr)
    }
    fn store(&mut self, addr: u64, byte: u8) -> bool {
        self.copy_to_user(addr, &[byte]).is_ok()
    }
    fn load(&mut self, addr: u64) -> Option<u8> {
        let mut b = [0u8];
        self.copy_from_user(&mut b, addr).ok().map(|_| b[0])
    }
    fn ranges(&self, lo: u64, hi: u64) -> Vec<(u64, u64, Prot)> {
        self.vmas().into_iter().filter(|v| v.start < hi && v.end > lo).map(|v| (v.start, v.end, v.prot)).collect()
    }
    fn madvise(&mut self, addr: u64, len: u64, advice: u64) -> Option<Result<(), Errno>> {
        Some(self.sys_madvise(addr, len, advice))
    }
    fn mremap(&mut self, addr: u64, old: u64, new: u64, flags: u64, new_addr: u64) -> Option<Result<u64, Errno>> {
        Some(self.sys_mremap(addr, old, new, flags, new_addr))
    }
    /// The vector lands in a scratch page of the same space (`MINCORE_VEC`, outside the script's window).
    fn mincore(&mut self, addr: u64, len: u64) -> Option<Result<Vec<u8>, Errno>> {
        let pages = len.div_ceil(P) as usize;
        Some(self.sys_mincore(addr, len, MINCORE_VEC).map(|_| {
            let mut v = alloc::vec![0u8; pages];
            let _ = self.copy_from_user(&mut v, MINCORE_VEC);
            v
        }))
    }
}

/// Where `mincore` of the second script writes its vector (a page mapped for it, far from the window).
const MINCORE_VEC: u64 = 0x30_0000_0000;

/// MM-5b: madvise, mremap, mincore and shared anonymous memory against CachyOS's answers. Programs run with
/// `Fill::OnFault` only (an eager space has every page resident, which mincore shows).
fn abi_diff2() -> TestResult {
    let mut a = lazy()?;
    let heap = 0x10_0000_0000u64;
    a.init_brk(heap);
    a.sys_mmap(MINCORE_VEC, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    let got = abi::run(abi::SCRIPT2, &mut a, heap);
    if let Some(d) = abi::first_difference(&got, abi::LINUX_EXPECTED2) {
        return Err(d);
    }
    klog::event!(Info, "mm", "ktest.abi_diff2", lines = abi::LINUX_EXPECTED2.lines().count(); "madvise, mremap, mincore answer like CachyOS");
    Ok(())
}
ktest!("mm.as.abi_diff2", Safe, abi_diff2);

fn abi_diff() -> TestResult {
    for fill in [Fill::OnFault, Fill::Eager] {
        let mut a = AddressSpace::with_fill(fill).map_err(e)?;
        let heap = 0x10_0000_0000u64;
        a.init_brk(heap);
        let got = abi::run(abi::SCRIPT, &mut a, heap);
        if let Some(d) = abi::first_difference(&got, abi::LINUX_EXPECTED) {
            return Err(format!("{fill:?}: {d}"));
        }
    }
    klog::event!(Info, "mm", "ktest.abi_diff", lines = abi::LINUX_EXPECTED.lines().count(); "memory calls answer like CachyOS");
    Ok(())
}
ktest!("mm.as.abi_diff", Safe, abi_diff);

fn eager() -> TestResult {
    let a = AddressSpace::new().map_err(e)?;
    kassert!(a.fill() == Fill::Eager);
    let m = a.sys_mmap(0, 16 * P, RW, AP, 0).map_err(e)?;
    kassert!(paging::count_user_leaves(a.root(), m, m + 16 * P) == 16, "eager: every page at mmap");
    let r = a.sys_mmap(0, 1 << 30, 0, AP, 0).map_err(e)?;
    kassert!(paging::count_user_leaves(a.root(), r, r + (1 << 30)) == 0, "PROT_NONE stays empty");
    a.sys_mprotect(r, 4 * P, RW).map_err(e)?;
    kassert!(paging::count_user_leaves(a.root(), r, r + (1 << 30)) == 4, "made accessible: filled");
    a.write_u64(r + 3 * P, 0xE6).map_err(e)?;
    kassert!(a.read_u64(r + 3 * P).map_err(e)? == 0xE6, "filled while PROT_NONE, usable after mprotect");
    a.init_brk(0x50_0000_0000);
    a.brk(0x50_0000_0000 + 3 * P);
    kassert!(paging::count_user_leaves(a.root(), 0x50_0000_0000, 0x50_0001_0000) == 3, "heap filled at brk");
    Ok(())
}
ktest!("mm.as.eager", Safe, eager);

fn frees() -> TestResult {
    let fa = mm_core::frames().ok_or("no frame allocator")?;
    let user = || fa.stats().used_by_kind[mm_core::Kind::User as usize];
    let before = user();
    let a = lazy()?;
    let m = a.sys_mmap(0, 4 << 20, RW, AP, 0).map_err(e)?;
    for i in 0..64 {
        a.write_u64(m + i * 16 * P, i).map_err(e)?;
    }
    kassert!(user() - before == 64 * P, "64 pages filled: {} bytes", user() - before);
    kassert!(paging::count_user_tables(a.root()) >= 3);
    a.sys_munmap(m + (2 << 20), 2 << 20).map_err(e)?;
    kassert!(user() - before == 32 * P, "half unmapped: {} bytes still in use", user() - before);
    a.sys_munmap(m, 2 << 20).map_err(e)?;
    kassert!(user() == before, "all unmapped: {} bytes still in use", user() - before);
    kassert!(paging::count_user_tables(a.root()) == 0, "{} page tables left", paging::count_user_tables(a.root()));
    kassert!(a.write_u64(0x60_0000_0000, 1).is_err(), "a write to nothing must fail");
    a.sys_mmap(0x60_0000_0000, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.write_u64(0x60_0000_0000, 1).map_err(e)?;
    drop(a);
    kassert!(user() == before, "after drop: {} bytes still in use", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.as.frees", Safe, frees);

/// INT-A1b (checker finding on `mm_core::alloc_copy`): the copy takes only page-cache and user frames (user
/// frames: copy-on-write after fork); a kernel frame (here a page table) is refused. MM-5b: a MAP_PRIVATE file
/// page copied once is the space's own frame, so mprotect R then RW makes it writable again without a second
/// copy, while a page-cache frame mapped privately stays read-only until its first write copies it.
fn file_recopy() -> TestResult {
    use vfs::abi::{O_CREAT, O_RDWR};
    const PATH: &[u8] = b"/tmp/.mm-file-recopy";
    let ns = vfs::root_ns();
    if ns.lookup("/tmp").is_err() {
        return ktest::skip("no /tmp in this image");
    }
    let t = mm_core::alloc_table_frame().ok_or("no frame for the refusal check")?;
    let refused = mm_core::alloc_copy(t, mm_core::Kind::User);
    mm_core::free_table_frame(t);
    if let Some(c) = refused {
        if let Some(fa) = mm_core::frames() {
            fa.free(c, 0, mm_core::Kind::User);
        }
        return Err("alloc_copy copied a page-table frame".into());
    }
    let f = ns.open_flags(PATH, O_RDWR | O_CREAT, 0o600).map_err(e)?;
    let r = (|| -> TestResult {
        f.file.write_at(0, &0x1111u64.to_le_bytes()).map_err(e)?;
        let a = lazy()?;
        let fa = mm_core::frames().ok_or("no frame allocator")?;
        let user = || fa.stats().used_by_kind[mm_core::Kind::User as usize];
        let before = user();
        let m = a.sys_mmap_file(0, P, RW, MAP_PRIVATE, 0, &mut |len| vfs::mmap::check(&f, RW, MAP_PRIVATE, 0, len)).map_err(e)?;
        a.write_u64(m, 0x2222).map_err(e)?;
        kassert!(user() - before == P, "the private write made one user copy ({} bytes)", user() - before);
        a.sys_mprotect(m, P, 1).map_err(e)?;
        a.sys_mprotect(m, P, RW).map_err(e)?;
        // MM-5b: the private copy is this space's own frame and nobody shares it, so mprotect RW makes it writable
        // at once (Linux `can_change_pte_writable`); no second copy.
        let (_, fl) = paging::translate(a.root(), m).ok_or("the copied page is gone")?;
        kassert!(fl.contains(PageFlags::WRITABLE), "mprotect RW gives an unshared private copy its write right back");
        a.write_u64(m + 8, 0x3333).map_err(e)?;
        kassert!(a.read_u64(m).map_err(e)? == 0x2222 && a.read_u64(m + 8).map_err(e)? == 0x3333, "the copy keeps its data");
        kassert!(user() - before == P, "still one user copy ({} bytes of user frames)", user() - before);
        // A page-cache frame mapped privately stays read-only through mprotect RW: its first write copies.
        let m2 = a.sys_mmap_file(0, P, 1, MAP_PRIVATE, 0, &mut |len| vfs::mmap::check(&f, 1, MAP_PRIVATE, 0, len)).map_err(e)?;
        kassert!(a.read_u64(m2).map_err(e)? == 0x1111, "the file's content");
        a.sys_mprotect(m2, P, RW).map_err(e)?;
        let (_, fl) = paging::translate(a.root(), m2).ok_or("the cache page is gone")?;
        kassert!(!fl.contains(PageFlags::WRITABLE), "mprotect RW keeps a private file page read-only (it waits for the write fault)");
        a.handle_user_fault(m2, PF_U | PF_W | 1).map_err(e)?;
        kassert!(user() - before == 2 * P, "the write fault copied the cache page ({} bytes)", user() - before);
        a.sys_munmap(m2, P).map_err(e)?;
        kassert!(user() - before == P, "the second copy went back with its mapping ({} bytes)", user() - before);
        let mut b = [0u8; 8];
        f.file.read_at(0, &mut b).map_err(e)?;
        kassert!(u64::from_le_bytes(b) == 0x1111, "MAP_PRIVATE writes never reach the file");
        drop(a);
        kassert!(user() == before, "after drop: {} bytes of user frames left", user().saturating_sub(before));
        Ok(())
    })();
    drop(f);
    let _ = ns.remove("/tmp/.mm-file-recopy");
    r
}
ktest!("mm.as.file_recopy", Safe, file_recopy);

fn kva_reserve() -> TestResult {
    kassert!(paging::kernel_va_ready(), "vmap did not install the kernel-VA hook");
    let base = paging::reserve_kernel_va(5 * P).ok_or("no reservation")?;
    kassert!(base >= bootinfo::layout::VMAP_BASE && base + 5 * P <= bootinfo::layout::CPU_AREA_BASE, "{base:#x}");
    kassert!(!crate::vmap::is_free(base - P), "the guard page is reserved too");
    kassert!(paging::translate(paging::current_root(), base - P).is_none(), "and never mapped");
    let fa = mm_core::frames().ok_or("no frame allocator")?;
    let f = fa.alloc(0, mm_core::Zone::Normal, mm_core::Kind::Other).ok_or("no frame")?;
    let root = paging::kernel_root().ok_or("no kernel root")?;
    paging::map_4k(root, base + 2 * P, f, PageFlags::kernel_data(), &mut mm_core::alloc_table_frame).map_err(e)?;
    kassert!(paging::translate(root, base + 2 * P).map(|t| t.0) == Some(f));
    let other = paging::reserve_kernel_va(P).ok_or("second reservation")?;
    kassert!(other + P <= base - P || other - P >= base + 5 * P, "reservations overlap");
    let (live, _) = crate::vmap::kva_counts();
    kassert!(live >= 2);
    kassert!(!paging::release_kernel_va(base + P, 5 * P), "not a reservation start");
    // The owner unmaps its pages before the release (as KernelStack's drop does); the tables stay for it.
    kassert!(paging::unmap_4k(root, base + 2 * P) == Some(f));
    fa.free(f, 0, mm_core::Kind::Other);
    let freed = paging::release_kernel_va(base, 5 * P);
    let freed2 = paging::release_kernel_va(other, P);
    // MM-6: released after the shootdown, with the APs online too.
    kassert!(freed && freed2, "released ({} CPUs)", hal::percpu::cpu_count());
    kassert!(crate::vmap::is_free(base), "the range is free again");
    kassert!(!paging::release_kernel_va(base, 5 * P), "a second release is refused");
    klog::event!(Info, "mm", "ktest.kva", base = klog::Hex(base), released = freed, cpus = hal::percpu::cpu_count());
    Ok(())
}
ktest!("mm.vmap.reserve", Safe, kva_reserve);

/// msync's argument checks and holes (linux-6.6/mm/msync.c:32-108; the same table answered on CachyOS 7.2, and
/// `abi-mm5b msync` diffs it from a program): EINVAL for flags, an unaligned start and ASYNC|SYNC together; the
/// length page-aligned with wrap-around (2^64-1 becomes 0: answer 0; 2^64-4096 wraps past the top: ENOMEM); a
/// hole answers ENOMEM at the end, a plain MS_ASYNC at once.
fn msync_args() -> TestResult {
    use crate::aspace::{MS_ASYNC, MS_INVALIDATE, MS_SYNC};
    use uapi::errno::{EINVAL, ENOMEM};
    let a = lazy()?;
    let m = a.sys_mmap(0, 4 * P, RW, AP, 0).map_err(e)?;
    a.sys_munmap(m + P, P).map_err(e)?;
    type Case<'a> = (&'a str, u64, u64, u64, Result<(), i64>);
    let cases: [Case; 15] = [
        ("flag 8", m, P, 8, Err(EINVAL)),
        ("unaligned start", m + 1, P, MS_ASYNC, Err(EINVAL)),
        ("ASYNC|SYNC", m, P, MS_ASYNC | MS_SYNC, Err(EINVAL)),
        ("length 0", m, 0, MS_SYNC, Ok(())),
        ("length 2^64-1 aligns to 0", m, u64::MAX, MS_ASYNC, Ok(())),
        ("length 2^64-4096 wraps", m, u64::MAX - 0xFFF, MS_ASYNC, Err(ENOMEM)),
        ("length 2^64-8192 wraps", m, u64::MAX - 0x1FFF, MS_ASYNC, Err(ENOMEM)),
        ("hole, ASYNC", m, 4 * P, MS_ASYNC, Err(ENOMEM)),
        ("hole, SYNC", m, 4 * P, MS_SYNC, Err(ENOMEM)),
        ("hole, ASYNC|INVALIDATE", m, 4 * P, MS_ASYNC | MS_INVALIDATE, Err(ENOMEM)),
        ("only the hole", m + P, P, MS_SYNC, Err(ENOMEM)),
        ("hole then mapping", m + P, 2 * P, MS_SYNC, Err(ENOMEM)),
        ("SYNC|INVALIDATE", m, P, MS_SYNC | MS_INVALIDATE, Ok(())),
        ("unaligned length", m + 2 * P, 2 * P - 1, MS_SYNC, Ok(())),
        ("no flags", m + 2 * P, 2 * P - 1, 0, Ok(())),
    ];
    for (what, s, l, f, want) in cases {
        let got = a.sys_msync(s, l, f);
        kassert!(got == want, "msync {what}: {got:?}, Linux {want:?}");
    }
    Ok(())
}
ktest!("mm.as.msync_args", Safe, msync_args);

/// No user-half leaf is ever GLOBAL, whatever flags a caller passes to `map_user_4k`, `replace_user_4k` or
/// `protect_user`: a global translation would survive CR3/PCID switches into other address spaces.
fn user_leaves_not_global() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(0, 2 * P, RW, AP, 0).map_err(e)?;
    a.write_u64(m, 1).map_err(e)?;
    let global = |fl: PageFlags| PageFlags(fl.0 | PageFlags::GLOBAL);
    let g = |va: u64| paging::translate(a.root(), va).map(|(_, fl)| fl.contains(PageFlags::GLOBAL));
    let (phys, fl) = paging::translate(a.root(), m).ok_or("page 0 not mapped")?;
    paging::replace_user_4k(a.root(), m, phys, global(fl)).map_err(e)?;
    kassert!(g(m) == Some(false), "replace_user_4k left GLOBAL set");
    paging::protect_user(a.root(), m, P, &mut |u| Some(global(u.flags))).map_err(e)?;
    kassert!(g(m) == Some(false), "protect_user left GLOBAL set");
    let f = mm_core::alloc_zeroed(mm_core::Kind::User).ok_or("no frame")?;
    if paging::map_user_4k(a.root(), m + P, f, global(PageFlags::user(true, false)), &mut || a.alloc_table()).is_err() {
        if let Some(fa) = mm_core::frames() {
            fa.free(f, 0, mm_core::Kind::User);
        }
        return Err("map_user_4k failed".into());
    }
    a.test_count_anon();
    kassert!(g(m + P) == Some(false), "map_user_4k left GLOBAL set");
    kassert!(a.read_u64(m).map_err(e)? == 1 && a.read_u64(m + P).map_err(e)? == 0, "contents");
    Ok(())
}
ktest!("mm.as.user_leaves_not_global", Safe, user_leaves_not_global);
