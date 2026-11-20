//! DB2 ktests of the access log below the syscalls (the probe program's run is vfs-sys `access_ktests`):
//! - `access.ring.overflow`: on one CPU with interrupts off, a repeat folds into the last record, a full ring counts
//!   every further record as lost (exactly), exec and fork records still find room in the last `RESERVE` slots,
//!   and the storage never moves or grows;
//! - `access.switch`: paused, a program's create is not recorded but counted as lost; on again, it is, with its
//!   folder and name;
//! - `access.weak`: a record keeps neither the mount nor the file alive (umount is not busy, a removed file goes),
//!   and a file removed before the reader comes is still counted under its folder;
//! - `access.cost_log` (Safe: only logged) and `access.cost` (Hardware: judged on the Mini-PC): what an open costs
//!   with the log on against off, for opens of different files (each a new record) and of one file (folded);
//!   the "off" runs record nothing at all (not even a lost count) for their few milliseconds.
//!
//! They may run on a live system where bitsaccess reads the log: each holds it (`drain::hold`), takes only its own
//! records and gives the others back to the next read, and pauses instead of switching off.

use super::{Kind, ObjRef, drain, ring};
use crate::abi;
use crate::inode::Cred;
use crate::ops::Ctx;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const PID: u32 = 0x7fff_0db2;

fn ring_overflow() -> TestResult {
    let hold = drain::hold();
    drop(hold.take(|_| false));
    let (before, after_fold, after_fill, after_ident) = {
        let _irq = hal::cpu::IrqGuard::new();
        let before = ring::look_this_cpu().ok_or("no ring on this CPU")?;
        for _ in 0..100 {
            super::record(Kind::CapRaise, 0, PID, 0, ObjRef::Num(7));
        }
        let after_fold = ring::look_this_cpu().ok_or("ring gone")?;
        let free = after_fold.cap - after_fold.len;
        for i in 0..free + 10 {
            super::record(Kind::Ptrace, 0, PID, 0, ObjRef::Num(1000 + i as u64));
        }
        let after_fill = ring::look_this_cpu().ok_or("ring gone")?;
        for i in 0..ring::RESERVE + 3 {
            super::record(Kind::Fork, 0, PID, 0, ObjRef::Num(5000 + i as u64));
        }
        (before, after_fold, after_fill, ring::look_this_cpu().ok_or("ring gone")?)
    };
    drop(hold.take(|r| r.pid == PID));
    let ident_lost = ring::take_identity_lost();
    kassert!(after_fold.len == before.len + 1 && after_fold.last_n == Some(100), "100 repeats: {before:?} -> {after_fold:?}");
    kassert!(after_fill.len == after_fill.cap - ring::RESERVE, "file and net records took the reserve: {after_fill:?}");
    kassert!(after_fill.lost == after_fold.lost + ring::RESERVE as u64 + 10, "lost {} -> {}, want +{}", after_fold.lost, after_fill.lost, ring::RESERVE + 10);
    kassert!(after_ident.len == after_ident.cap && after_ident.lost == after_fill.lost + 3, "forks in the reserve: {after_fill:?} -> {after_ident:?}");
    kassert!(ident_lost, "a lost fork record was not noted");
    kassert!(after_ident.at == before.at && after_ident.cap == before.cap && before.cap == ring::CAP, "storage moved or grew: {before:?} -> {after_ident:?}");
    Ok(())
}
ktest!("access.ring.overflow", Safe, ring_overflow);

fn prog(ns: &crate::MountNs) -> Ctx<'_> {
    Ctx { ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred::ROOT, umask: 0o022, subject: policy::Subject { program: policy::ProgramId(1), pid: PID, uid: 0, tree: 0 } }
}

/// The merged lines of `PID` now waiting (the others' records go back to the next read).
fn lines_of_pid(hold: &drain::Hold) -> String {
    let mut known = alloc::collections::BTreeMap::new();
    let m = drain::merge(hold.take(|r| r.pid == PID), &mut known, &|_| None, false);
    let text = drain::render(&m, true, 0, 0);
    let mark = format!("\"pid\":{PID},");
    text.lines().filter(|l| l.contains(&mark)).map(|l| format!("{l}\n")).collect()
}

/// Back to what the system had: off drops what waits, as the switch does.
fn restore(was: bool) {
    super::set_on(was);
}

fn switch() -> TestResult {
    let ns = crate::root_ns();
    ns.mkdir_p("/tmp/db2-switch").map_err(|e| format!("mkdir: {e}"))?;
    let c = prog(&ns);
    let hold = drain::hold();
    let was = super::on();
    super::pause();
    let lost0 = super::dropped();
    let off = c.open(None, b"/tmp/db2-switch/aus.txt", abi::O_CREAT | abi::O_WRONLY, 0o644).map(drop);
    let lost = super::dropped() - lost0;
    let off_lines = lines_of_pid(&hold);
    super::set_on(true);
    let on = c.open(None, b"/tmp/db2-switch/an.txt", abi::O_CREAT | abi::O_WRONLY, 0o644).map(drop);
    let on_lines = lines_of_pid(&hold);
    restore(was);
    kassert!(off.is_ok() && on.is_ok(), "opens: {off:?} {on:?}");
    kassert!(off_lines.is_empty(), "recorded while paused: {off_lines}");
    kassert!(lost >= 1, "the paused create was not counted as lost");
    kassert!(on_lines.contains("\"k\":\"create\",\"dir\":\"/tmp/db2-switch\",\"name\":\"an.txt\",\"n\":1"), "not recorded while on: {on_lines:?}");
    Ok(())
}
ktest!("access.switch", Safe, switch);

fn weak() -> TestResult {
    let ns = crate::root_ns();
    ns.mkdir_p("/tmp/db2-weak").map_err(|e| format!("mkdir: {e}"))?;
    let c = prog(&ns);
    let hold = drain::hold();
    let was = super::on();
    super::set_on(true);
    let r = (|| -> TestResult {
        let made = c.open(None, b"/tmp/db2-weak/weg.txt", abi::O_CREAT | abi::O_WRONLY, 0o644).map_err(|e| format!("create: {e}"))?;
        let p = c.lookup(None, b"/tmp/db2-weak/weg.txt", true).map_err(|e| format!("lookup: {e}"))?;
        let (mounts, dentries) = (alloc::sync::Arc::strong_count(&p.mnt), alloc::sync::Arc::strong_count(&p.dentry));
        super::opened(&c, &p, uapi::S_IFREG, false, false);
        kassert!(alloc::sync::Arc::strong_count(&p.mnt) == mounts && alloc::sync::Arc::strong_count(&p.dentry) == dentries, "a record holds the mount or the file");
        let gone = alloc::sync::Arc::downgrade(&p.dentry);
        drop((made, p));
        c.unlink(None, b"/tmp/db2-weak/weg.txt").map_err(|e| format!("unlink: {e}"))?;
        kassert!(gone.upgrade().is_none(), "the removed file still lives");
        let text = lines_of_pid(&hold);
        kassert!(text.contains("\"k\":\"create\",\"dir\":\"/tmp/db2-weak\",\"name\":\"weg.txt\",\"n\":1"), "the removed file's create: {text}");
        kassert!(text.contains("\"k\":\"read\",\"dir\":\"/tmp/db2-weak\",\"n\":1"), "the removed file's read: {text}");
        Ok(())
    })();
    restore(was);
    r
}
ktest!("access.weak", Safe, weak);

const N: usize = 20_000;
/// Different files the loop opens in turn: no open repeats the one before, so each takes a new slot.
const FILES: usize = 64;
/// Opens between two emptyings of the ring (outside the timed part): it never fills.
const CHUNK: usize = 512;

/// ns per open+close of tmpfs files as a program, the files in turn.
fn per_open(c: &Ctx<'_>, hold: &drain::Hold, files: &[Vec<u8>]) -> Result<u64, String> {
    for f in files.iter().cycle().take(200) {
        kassert!(c.open(None, f, abi::O_RDONLY, 0).is_ok(), "warm-up open failed");
    }
    drop(hold.take(|r| r.pid == PID));
    let (mut spent, mut done) = (0, 0);
    while done < N {
        let t0 = ksync::now_ns().unwrap_or(0);
        for i in done..done + CHUNK {
            let f = c.open(None, &files[i % files.len()], abi::O_RDONLY, 0);
            kassert!(f.is_ok(), "open failed");
            drop(f);
        }
        spent += ksync::now_ns().unwrap_or(0) - t0;
        done += CHUNK;
        drop(hold.take(|r| r.pid == PID));
    }
    Ok(spent / done as u64)
}

/// ns per open: (different files off, on; one file off, on).
fn measure() -> Result<(u64, u64, u64, u64), String> {
    let ns = crate::root_ns();
    ns.mkdir_p("/tmp/db2-cost").map_err(|e| format!("mkdir: {e}"))?;
    let files: Vec<Vec<u8>> = (0..FILES).map(|i| format!("/tmp/db2-cost/f{i}").into_bytes()).collect();
    for f in &files {
        drop(ns.open_flags(f, abi::O_CREAT | abi::O_WRONLY, 0o644).map_err(|e| format!("create: {e}"))?);
    }
    let c = prog(&ns);
    let hold = drain::hold();
    let was = super::on();
    let run = |on: bool, files: &[Vec<u8>]| {
        if on { super::set_on(true) } else { super::hush() }
        per_open(&c, &hold, files)
    };
    let r: Result<_, String> = (|| Ok((run(false, &files)?, run(true, &files)?, run(false, &files[..1])?, run(true, &files[..1])?)))();
    restore(was);
    let (off, on, one_off, one_on) = r?;
    klog::event!(Info, "vfs", "access.open_cost", off_ns = off, on_ns = on, hook_ns = on.saturating_sub(off), folded_off_ns = one_off, folded_on_ns = one_on, folded_hook_ns = one_on.saturating_sub(one_off));
    Ok((off, on, one_off, one_on))
}

fn cost_log() -> TestResult {
    measure().map(drop)
}
ktest!("access.cost_log", Safe, cost_log);

/// On the Mini-PC the log may add at most a quarter of an open's cost (at least 250 ns are allowed), judged on
/// opens of different files (each a new record: the costly way).
fn cost() -> TestResult {
    let (off, on, _, _) = measure()?;
    let allowed = (off / 4).max(250);
    kassert!(on.saturating_sub(off) <= allowed, "the log adds {} ns per open (off {off} ns, on {on} ns), allowed {allowed}", on.saturating_sub(off));
    Ok(())
}
ktest!("access.cost", Hardware, cost);
