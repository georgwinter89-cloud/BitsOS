//! PF-6 ktests: the kernel's copies straight from a page-cache frame to a program (`copy_frames_to_user`, with
//! `mm.as` and page fills) and without a lock or fault handling (`copy_frame_to_user_nofault`,
//! `copy_to_user_nofault`) end exactly at the first user page that cannot be written, on a test address space
//! loaded by the test thread, and again with the space not owned (the copies that never walk refuse, the locked
//! one goes through its kernel buffer). Rerunnable (Safe): each run makes its own address space and frame.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use ktest::{TestResult, kassert, ktest};
use mm::{AddressSpace, Prot, VmaKind};

const PAGE_A: u64 = 0x58_0000;
const HOLE: u64 = PAGE_A + 0x1000;
const RO: u64 = HOLE + 0x1000;

ktest!("fs.pf6.frame_copies", Safe, frame_copies);

fn pat(i: usize) -> u8 {
    (i % 251) as u8 + 1
}

fn frame_copies() -> TestResult {
    if !hal::usercopy::stats().fast_path {
        return ktest::fail("the user-copy fast path is not installed");
    }
    let Some(frame) = mm_core::PageFrame::new_zeroed() else { return ktest::fail("no frame") };
    let data: alloc::vec::Vec<u8> = (0..4096).map(pat).collect();
    kassert!(frame.write(0, &data), "fill the frame");
    let asp = Arc::new(AddressSpace::new().map_err(|e| format!("address space: {e:?}"))?);
    asp.map_fixed(PAGE_A, 4096, Prot::rw(), VmaKind::Anonymous).map_err(|e| format!("map: {e:?}"))?;
    asp.map_fixed(RO, 4096, Prot { read: true, write: false, exec: false }, VmaKind::Anonymous).map_err(|e| format!("map ro: {e:?}"))?;
    let home = hal::paging::home_root();
    asp.activate();
    let r = checks(&asp, &frame);
    hal::paging::activate(home);
    let r = r.and_then(|_| not_owned(&asp, &frame));
    drop(asp);
    r
}

/// PF-6 fixer: the same space while the test thread runs on its home root (it does not own the space here, as
/// a kernel thread copying for another process): the no-fault copies and the frame's own copy refuse (0, no
/// byte written: they never walk), and `copy_frames_to_user` still copies through its kernel buffer under
/// `mm.as`, ending at the hole. The short-copy counter counts only fast copies a fault ended, not refusals;
/// it is not compared here because other readers in the same boot move it.
fn not_owned(asp: &AddressSpace, frame: &mm_core::PageFrame) -> TestResult {
    let root = asp.root();
    {
        let _p = ksync::PreemptGuard::new();
        kassert!(!hal::paging::tlb::owns_here(root), "the test thread still owns the test space");
        kassert!(asp.copy_to_user_nofault(PAGE_A + 8, &[0xEE; 32]) == 0, "no-fault slice copy into a space not owned");
        kassert!(asp.copy_frame_to_user_nofault(PAGE_A + 8, frame, 0, 32) == 0, "no-fault frame copy into a space not owned");
        kassert!(frame.copy_to_user(root, 0, 32, PAGE_A + 8) == 0, "the frame's copy walked a root not loaded");
        kassert!(frame.copy_to_user_nofault(root, 0, 32, PAGE_A + 8) == 0, "the frame's no-fault copy into a root not loaded");
    }
    let mut back = vec![0u8; 300];
    asp.copy_from_user(&mut back[..200], PAGE_A + 8).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(back[..200].iter().enumerate().all(|(i, &b)| b == pat(100 + i)), "a refused copy wrote user memory");
    let got = asp.copy_frames_to_user(PAGE_A + 1000, &[(frame, 2000, 300)]);
    kassert!(got == 300, "copy_frames_to_user into a space not owned: {got} bytes");
    asp.copy_from_user(&mut back[..300], PAGE_A + 1000).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(back[..300].iter().enumerate().all(|(i, &b)| b == pat(2000 + i)), "bytes copied into a space not owned");
    let got = asp.copy_frames_to_user(HOLE - 50, &[(frame, 0, 30), (frame, 3000, 70)]);
    kassert!(got == 50, "copy_frames_to_user across the hole, not owned: {got} bytes");
    asp.copy_from_user(&mut back[..50], HOLE - 50).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(back[..30].iter().enumerate().all(|(i, &b)| b == pat(i)) && back[30..50].iter().enumerate().all(|(i, &b)| b == pat(3000 + i)), "bytes before the hole, not owned");
    kassert!(asp.copy_frames_to_user(RO, &[(frame, 0, 8)]) == 0, "read-only user page, not owned");
    Ok(())
}

fn checks(asp: &AddressSpace, frame: &mm_core::PageFrame) -> TestResult {
    let mut back = vec![0u8; 256];
    {
        // Preemption off: a kernel thread resumed elsewhere would run on the kernel root (no-fault copies 0).
        let _p = ksync::PreemptGuard::new();
        kassert!(asp.copy_frame_to_user_nofault(HOLE, frame, 0, 16) == 0, "no-fault copy into the hole");
        kassert!(asp.copy_to_user_nofault(HOLE + 8, b"x") == 0, "no-fault slice copy into the hole");
    }
    // The locked copy fills the page and stops exactly at the hole.
    let got = asp.copy_frames_to_user(HOLE - 100, &[(frame, 10, 60), (frame, 1000, 140)]);
    kassert!(got == 100, "copy_frames_to_user across the hole: {got} bytes");
    asp.copy_from_user(&mut back[..100], HOLE - 100).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(back[..60].iter().enumerate().all(|(i, &b)| b == pat(10 + i)), "first part");
    kassert!(back[60..100].iter().enumerate().all(|(i, &b)| b == pat(1000 + i)), "second part");
    kassert!(asp.copy_frames_to_user(PAGE_A, &[(frame, 4000, 200)]) == 0, "a part leaving its frame copies nothing");
    kassert!(asp.copy_frames_to_user(RO, &[(frame, 0, 8)]) == 0, "read-only user page");
    kassert!(asp.copy_frames_to_user(PAGE_A, &[]) == 0, "no parts");
    let before = hal::usercopy::stats().nofault_short;
    {
        let _p = ksync::PreemptGuard::new();
        kassert!(hal::paging::tlb::owns_here(asp.root()), "the test thread does not own its root");
        let n = asp.copy_frame_to_user_nofault(PAGE_A + 8, frame, 100, 200);
        kassert!(n == 200, "no-fault copy into a present page: {n}");
        let n = asp.copy_frame_to_user_nofault(HOLE - 30, frame, 0, 64);
        kassert!(n == 30, "no-fault copy across the hole: {n}");
        kassert!(asp.copy_to_user_nofault(HOLE - 5, b"abcdefgh") == 5, "no-fault slice across the hole");
        kassert!(asp.copy_frame_to_user_nofault(RO, frame, 0, 8) == 0, "no-fault copy into a read-only page");
        kassert!(asp.copy_frame_to_user_nofault(PAGE_A, frame, 4090, 8) == 0, "no-fault part leaving its frame");
        kassert!(asp.copy_to_user_nofault(0xFFFF_8880_0000_0000, b"k") == 0, "kernel address");
    }
    kassert!(hal::usercopy::stats().nofault_short >= before + 3, "short no-fault copies not counted");
    asp.copy_from_user(&mut back[..200], PAGE_A + 8).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(back[..200].iter().enumerate().all(|(i, &b)| b == pat(100 + i)), "no-fault bytes");
    asp.copy_from_user(&mut back[..30], HOLE - 30).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(back[..25].iter().enumerate().all(|(i, &b)| b == pat(i)) && &back[25..30] == b"abcde", "bytes before the hole");
    Ok(())
}
