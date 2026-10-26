//! MM-rmap ktests: a hole punch or truncate takes a tmpfs file's pages out of every mapping, so they are freed
//! and uncharged at once (Linux `unmap_mapping_range` in `shmem_fallocate`/`shmem_setattr`); a later touch
//! faults a fresh zero page inside the file and SIGBUS past its end. All Safe: rerunnable, no timing. A TLB
//! shootdown another CPU did not confirm keeps its frames (and their charge) until the space ends, as designed
//! (`Deferred`): a test that sees one skips instead of judging the charge (`settled!`).

use crate::aspace::rmap;
use crate::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED, MREMAP_FIXED, MREMAP_MAYMOVE};
use crate::{AddressSpace, FaultReason, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FALLOC_FL_ZERO_RANGE};
use vfs::{Inode, OpenFile};

const P: u64 = 4096;
const RW: u64 = 3;
const PF_U: u64 = 4;
const PUNCH: u32 = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

/// A memfd of `pages` pages (lazily charged: each page when it is allocated, FS-14).
fn tmpfile(pages: u64, flags: u32) -> Result<(Arc<OpenFile>, Arc<Inode>), String> {
    if vfs::root_ns().lookup("/dev/shm").is_err() {
        return Err(String::from("skip"));
    }
    let f = vfs::memfd::create(b"mm-rmap", flags, vfs::Cred::ROOT).map_err(|x| format!("memfd: {x}"))?;
    let i = f.inode().ok_or("no inode")?.clone();
    kassert!(i.page_cache().is_some_and(|c| c.lazy()), "a memfd is charged per page");
    i.truncate(pages * P).map_err(|x| format!("size: {x}"))?;
    Ok((f, i))
}

fn charged(i: &Inode) -> u64 {
    i.page_cache().map_or(0, |c| c.charged())
}

fn map(a: &AddressSpace, f: &OpenFile, pages: u64, flags: u64) -> Result<u64, String> {
    a.sys_mmap_file(0, pages * P, RW, flags, 0, &mut |len| vfs::mmap::check(f, RW, flags, 0, len)).map_err(e)
}

macro_rules! file_or_skip {
    ($pages:expr) => {
        file_or_skip!($pages, 0)
    };
    ($pages:expr, $flags:expr) => {
        match tmpfile($pages, $flags) {
            Ok(x) => x,
            Err(s) if s == "skip" => return ktest::skip("no /dev/shm in this image (memfds live there)"),
            Err(s) => return Err(s),
        }
    };
}

/// The shootdown count, read before an unmap that `settled!` then checks.
fn unconfirmed() -> u64 {
    crate::aspace::UNCONFIRMED.get()
}

/// Skips the test when a TLB shootdown since `$u0` went unconfirmed (anywhere: the counter is global, so this can
/// only skip too often, never judge a deferred frame as leaked).
macro_rules! settled {
    ($u0:expr) => {
        if unconfirmed() != $u0 {
            return ktest::skip("a TLB shootdown went unconfirmed: its frames keep their charge until the space ends");
        }
    };
}

/// A punch under a live shared mapping frees the whole pages of the hole and their charge; the mapping reads
/// zeros there, a store makes a fresh page (charged again); a partly punched page is zeroed in place. A private
/// mapping's copy keeps its data (even_cows = 0), its read-only view of the cache does not hold the page.
fn punch_shared() -> TestResult {
    let (f, i) = file_or_skip!(16);
    let a = lazy()?;
    let m = map(&a, &f, 16, MAP_SHARED)?;
    for pg in 0..16 {
        a.write_u64(m + pg * P, 0x100 + pg).map_err(e)?;
    }
    kassert!(charged(&i) == 16, "16 pages touched, {} charged", charged(&i));
    let rss = a.resident_pages();
    kassert!(rmap::spaces_of(&i) == 1, "one space maps the file: {}", rmap::spaces_of(&i));
    // Pages 4..12 whole, plus the second half of page 3 (zeroed in place, kept).
    let u0 = unconfirmed();
    i.fallocate(PUNCH, 3 * P + P / 2, P / 2 + 8 * P).map_err(e)?;
    settled!(u0);
    kassert!(charged(&i) == 8, "the punch under the mapping freed 8 pages: {} charged", charged(&i));
    kassert!(a.resident_pages() == rss - 8, "resident {} -> {}", rss, a.resident_pages());
    kassert!(a.read_u64(m + 3 * P).map_err(e)? == 0x103, "page 3 keeps its first half");
    kassert!(a.read_u64(m + 3 * P + P / 2).map_err(e)? == 0, "page 3's punched half reads zero");
    for pg in 4..12 {
        kassert!(a.read_u64(m + pg * P).map_err(e)? == 0, "page {pg} reads zero after the punch");
    }
    kassert!(a.read_u64(m + 12 * P).map_err(e)? == 0x10c, "page 12 is past the hole");
    a.write_u64(m + 5 * P, 0x55).map_err(e)?;
    let mut b = [0u8; 8];
    i.read_at(5 * P, &mut b).map_err(e)?;
    kassert!(u64::from_le_bytes(b) == 0x55, "a store into the hole made a fresh shared page");

    // Private: page 0 copied (its data stays), page 1 only read (the cache's page, which the punch frees).
    let pm = map(&a, &f, 2, MAP_PRIVATE)?;
    a.write_u64(pm, 0xAA).map_err(e)?;
    kassert!(a.read_u64(pm + P).map_err(e)? == 0x101, "the private view reads the file");
    let before = charged(&i);
    let u0 = unconfirmed();
    i.fallocate(PUNCH, 0, 2 * P).map_err(e)?;
    settled!(u0);
    kassert!(charged(&i) == before - 2, "pages 0 and 1 freed although mapped twice: {before} -> {}", charged(&i));
    kassert!(a.read_u64(pm).map_err(e)? == 0xAA, "the private copy keeps its data (even_cows = 0)");
    kassert!(a.read_u64(pm + P).map_err(e)? == 0, "the private view of page 1 reads the hole");
    kassert!(a.read_u64(m).map_err(e)? == 0, "the shared view of page 0 reads the hole");
    drop(a);
    kassert!(rmap::spaces_of(&i) == 0, "the space's drop left {} entries", rmap::spaces_of(&i));
    drop(f);
    Ok(())
}
ktest!("mm.rmap.punch", Safe, punch_shared);

/// Two spaces (and a fork's child) map one file shared: a punch takes the page out of all three, the charge
/// goes, a store from one is seen by the others; munmap and the drops remove the reverse-map entries.
fn two_spaces() -> TestResult {
    let (f, i) = file_or_skip!(4);
    let a = lazy()?;
    let b = lazy()?;
    let ma = map(&a, &f, 4, MAP_SHARED)?;
    let mb = map(&b, &f, 4, MAP_SHARED)?;
    for pg in 0..4 {
        a.write_u64(ma + pg * P, 0x200 + pg).map_err(e)?;
        kassert!(b.read_u64(mb + pg * P).map_err(e)? == 0x200 + pg, "b sees a's page {pg}");
    }
    let c = a.fork().map_err(e)?;
    kassert!(c.read_u64(ma + 2 * P).map_err(e)? == 0x202, "the child sees the page");
    kassert!(rmap::spaces_of(&i) == 3, "three spaces map the file: {}", rmap::spaces_of(&i));
    let (ra, rb, rc) = (a.resident_pages(), b.resident_pages(), c.resident_pages());
    let u0 = unconfirmed();
    i.fallocate(PUNCH, P, 2 * P).map_err(e)?;
    settled!(u0);
    kassert!(charged(&i) == 2, "the pages mapped by three spaces were freed: {} charged", charged(&i));
    kassert!(a.resident_pages() == ra - 2 && b.resident_pages() == rb - 2 && c.resident_pages() < rc,
        "resident a {ra}->{} b {rb}->{} c {rc}->{}", a.resident_pages(), b.resident_pages(), c.resident_pages());
    for (s, at) in [(&a, ma), (&b, mb), (&c, ma)] {
        kassert!(s.read_u64(at + P).map_err(e)? == 0 && s.read_u64(at + 2 * P).map_err(e)? == 0, "the hole reads zero everywhere");
        kassert!(s.read_u64(at + 3 * P).map_err(e)? == 0x203, "page 3 kept");
    }
    b.write_u64(mb + P, 0x77).map_err(e)?;
    kassert!(a.read_u64(ma + P).map_err(e)? == 0x77 && c.read_u64(ma + P).map_err(e)? == 0x77, "one fresh page for all");
    kassert!(charged(&i) == 4, "reads of the hole made fresh pages again (as shmem_fault): {}", charged(&i));
    drop(c);
    kassert!(rmap::spaces_of(&i) == 2, "the child's drop: {} left", rmap::spaces_of(&i));
    b.sys_munmap(mb, 4 * P).map_err(e)?;
    kassert!(rmap::spaces_of(&i) == 1, "b's munmap: {} left", rmap::spaces_of(&i));
    let u0 = unconfirmed();
    i.fallocate(PUNCH, 0, 4 * P).map_err(e)?;
    settled!(u0);
    kassert!(charged(&i) == 0, "everything punched: {} charged", charged(&i));
    drop(a);
    drop(b);
    kassert!(rmap::spaces_of(&i) == 0, "{} entries left", rmap::spaces_of(&i));
    drop(f);
    Ok(())
}
ktest!("mm.rmap.two_spaces", Safe, two_spaces);

/// A truncate below mapped pages frees them and their private copies (even_cows = 1); a touch past the new end
/// is SIGBUS in the shared and the private mapping; the new last page's tail reads zero.
fn truncate_bus() -> TestResult {
    let (f, i) = file_or_skip!(4);
    let a = lazy()?;
    let m = map(&a, &f, 4, MAP_SHARED)?;
    for pg in 0..4 {
        a.write_u64(m + pg * P, u64::MAX).map_err(e)?;
    }
    a.write_u64(m + P + 88, u64::MAX).map_err(e)?;
    a.write_u64(m + P + 104, u64::MAX).map_err(e)?;
    let pm = map(&a, &f, 4, MAP_PRIVATE)?;
    a.write_u64(pm + 3 * P, 0x99).map_err(e)?;
    let r = a.resident_pages();
    let u0 = unconfirmed();
    i.truncate(P + 100).map_err(e)?;
    settled!(u0);
    // Global frame counts are not judged (other programs run beside a Safe test on the Mini-PC): the space's own.
    kassert!(a.resident_pages() == r - 3, "shared pages 2, 3 and the private copy of 3 left: resident {r} -> {}", a.resident_pages());
    kassert!(charged(&i) == 2, "truncate under the mappings freed pages 2 and 3: {} charged", charged(&i));
    kassert!(a.handle_user_fault(m + 2 * P, PF_U) == Err(FaultReason::Bus), "a touch past the end is SIGBUS");
    kassert!(a.handle_user_fault(m + 3 * P, PF_U) == Err(FaultReason::Bus), "page 3 too");
    kassert!(a.handle_user_fault(pm + 3 * P, PF_U) == Err(FaultReason::Bus), "the private copy past the end went too");
    kassert!(a.read_u64(m + P + 88).map_err(e)? == u64::MAX, "bytes before the end stay");
    kassert!(a.read_u64(m + P + 104).map_err(e)? == 0, "the new last page's tail reads zero");
    i.truncate(4 * P).map_err(e)?;
    kassert!(a.read_u64(m + 3 * P).map_err(e)? == 0, "grown again: a fresh zero page");
    drop(a);
    drop(f);
    drop(i);
    Ok(())
}
ktest!("mm.rmap.truncate", Safe, truncate_bus);

/// The reverse-map entry follows the space's VMAs of the file: a partial munmap, an mremap shrink and move keep
/// it (and the punch still reaches the moved pages), a MAP_FIXED mapping over the last VMA removes it; then no
/// page stays pinned by a stale mapping.
fn entries() -> TestResult {
    let (f, i) = file_or_skip!(8);
    let a = lazy()?;
    let m = map(&a, &f, 8, MAP_SHARED)?;
    for pg in 0..8 {
        a.write_u64(m + pg * P, 0x300 + pg).map_err(e)?;
    }
    a.sys_munmap(m, 2 * P).map_err(e)?;
    kassert!(rmap::spaces_of(&i) == 1, "a partial munmap keeps the entry: {}", rmap::spaces_of(&i));
    // File pages 2..8 at m + 2P: shrink to 2..6, then move them elsewhere.
    let at = a.sys_mremap(m + 2 * P, 6 * P, 4 * P, 0, 0).map_err(e)?;
    kassert!(at == m + 2 * P && rmap::spaces_of(&i) == 1, "an mremap shrink keeps the entry: {}", rmap::spaces_of(&i));
    let dst = a.sys_mmap(0, 4 * P, RW, MAP_PRIVATE | MAP_ANONYMOUS, 0).map_err(e)?;
    let to = a.sys_mremap(at, 4 * P, 4 * P, MREMAP_MAYMOVE | MREMAP_FIXED, dst).map_err(e)?;
    kassert!(to == dst && rmap::spaces_of(&i) == 1, "an mremap move keeps the entry: {}", rmap::spaces_of(&i));
    kassert!(a.read_u64(dst).map_err(e)? == 0x302, "the moved VMA maps file page 2");
    kassert!(charged(&i) == 8, "unmapping leaves the file's pages: {} charged", charged(&i));
    let r = a.resident_pages();
    let u0 = unconfirmed();
    i.fallocate(PUNCH, 2 * P, 2 * P).map_err(e)?;
    settled!(u0);
    kassert!(charged(&i) == 6, "the punch reached the moved mapping: {} charged", charged(&i));
    kassert!(a.resident_pages() == r - 2, "resident {r} -> {}", a.resident_pages());
    kassert!(a.read_u64(dst + 2 * P).map_err(e)? == 0x304, "file page 4 still mapped at its new place");
    a.sys_mmap(dst, 4 * P, RW, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, 0).map_err(e)?;
    kassert!(rmap::spaces_of(&i) == 0, "MAP_FIXED over the last VMA of the file: {} left", rmap::spaces_of(&i));
    i.fallocate(PUNCH, 0, 8 * P).map_err(e)?;
    kassert!(charged(&i) == 0, "no stale mapping pins a page: {} charged", charged(&i));
    drop(a);
    drop(f);
    Ok(())
}
ktest!("mm.rmap.entries", Safe, entries);

/// A truncate or punch the file refuses changes no mapping: F_SEAL_SHRINK (EPERM), F_SEAL_FUTURE_WRITE (EPERM)
/// and a punch mode the cache refuses (ZERO_RANGE) keep every page mapped, its data and its charge (Linux checks before
/// `unmap_mapping_range`).
fn refused() -> TestResult {
    use uapi::errno::EPERM;
    use vfs::memfd::{F_ADD_SEALS, F_SEAL_FUTURE_WRITE, F_SEAL_SHRINK, MFD_ALLOW_SEALING};
    let (f, i) = file_or_skip!(4, MFD_ALLOW_SEALING);
    let a = lazy()?;
    let m = map(&a, &f, 4, MAP_SHARED)?;
    let pm = map(&a, &f, 4, MAP_PRIVATE)?;
    for pg in 0..4 {
        a.write_u64(m + pg * P, 0x400 + pg).map_err(e)?;
    }
    a.write_u64(pm + 3 * P, 0x99).map_err(e)?;
    let r = a.resident_pages();
    let same = |what: &str| -> TestResult {
        kassert!(a.resident_pages() == r, "{what}: resident {r} -> {}", a.resident_pages());
        kassert!(charged(&i) == 4, "{what}: {} charged", charged(&i));
        for pg in 0..4 {
            kassert!(a.read_u64(m + pg * P).map_err(e)? == 0x400 + pg, "{what}: page {pg} lost its data");
        }
        kassert!(a.read_u64(pm + 3 * P).map_err(e)? == 0x99, "{what}: the private copy lost its data");
        Ok(())
    };
    kassert!(i.fallocate(PUNCH | FALLOC_FL_ZERO_RANGE, 0, 4 * P).is_err(), "a punch with ZERO_RANGE is refused");
    same("PUNCH_HOLE with ZERO_RANGE")?;
    vfs::memfd::seal_fcntl(&f, F_ADD_SEALS, F_SEAL_SHRINK as u64).map_err(e)?;
    kassert!(i.truncate(P) == Err(EPERM), "F_SEAL_SHRINK refuses the truncate");
    same("truncate under F_SEAL_SHRINK")?;
    vfs::memfd::seal_fcntl(&f, F_ADD_SEALS, F_SEAL_FUTURE_WRITE as u64).map_err(e)?;
    kassert!(i.fallocate(PUNCH, 0, 4 * P) == Err(EPERM), "F_SEAL_FUTURE_WRITE refuses the punch");
    same("punch under F_SEAL_FUTURE_WRITE")?;
    drop(a);
    kassert!(rmap::spaces_of(&i) == 0, "{} entries left", rmap::spaces_of(&i));
    drop(f);
    Ok(())
}
ktest!("mm.rmap.refused", Safe, refused);
