//! Side-band records (linux-7.3.1/kernel/events/core.c `perf_iterate_sb`, `perf_event_comm`, `perf_event_mmap`,
//! `perf_event_task`): COMM and MMAP2 when an image starts, MMAP2 when a program maps code, FORK and EXIT. A
//! record goes to the events of the CPU it happens on and to the events of the thread it happens in that ask for
//! it (`attr.comm`, `attr.mmap`/`mmap2`/`mmap_data`, `attr.task`). The mappings are read from the program's
//! `/proc/<pid>/maps` (the same lines a profiler reads for the programs that were running before it started).

use super::SIDE_BAND;
use super::event::{Event, Target};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::SpinLock;
use perf_abi::attr::flag;
use perf_abi::record::{self, Ident, Map};
use perf_abi::*;

static SIDE: SpinLock<Vec<Arc<Event>>> = SpinLock::named("perf.side", Vec::new());
/// Events that take side-band records or wait for an exec (one load on the paths of proc and mmap).
static WANTED: AtomicUsize = AtomicUsize::new(0);

fn wants(ev: &Event) -> bool {
    let a = &ev.attr;
    a.has(flag::COMM) || a.has(flag::MMAP) || a.has(flag::MMAP2) || a.has(flag::MMAP_DATA) || a.has(flag::TASK) || a.has(flag::ENABLE_ON_EXEC) || ev.tid().is_some()
}

fn install_hooks() {
    proc::perf_hook::install(proc::perf_hook::TaskHooks { fork, exit, image });
}

pub(crate) fn attach(ev: &Arc<Event>) {
    if !wants(ev) {
        return;
    }
    install_hooks();
    let mut l = SIDE.lock();
    if l.try_reserve(1).is_err() {
        klog::event!(Warn, "perf", "side.no_memory", id = ev.id);
        return;
    }
    l.push(ev.clone());
    WANTED.store(l.len(), Ordering::Release);
    proc::perf_hook::set_on(true);
}

pub(crate) fn detach(ev: &Arc<Event>) {
    let gone = {
        let mut l = SIDE.lock();
        let i = l.iter().position(|e| Arc::ptr_eq(e, ev));
        let g = i.map(|i| l.swap_remove(i));
        WANTED.store(l.len(), Ordering::Release);
        proc::perf_hook::set_on(!l.is_empty());
        g
    };
    drop(gone);
}

/// Whether anyone wants side-band records (the mmap path asks before it looks at its mapping).
pub(crate) fn active() -> bool {
    WANTED.load(Ordering::Acquire) != 0
}

/// The events a record about thread `tid` on this CPU goes to (`perf_iterate_sb`: the CPU's events and the
/// thread's own).
fn targets(tid: u32, cpu: usize, want: impl Fn(&Event) -> bool) -> Vec<Arc<Event>> {
    let l = SIDE.lock();
    let mut v = Vec::new();
    if v.try_reserve(l.len()).is_err() {
        return v;
    }
    for e in l.iter() {
        let here = match &e.target {
            Target::Cpu(c) => *c == cpu,
            Target::Thread { tid: t, cpu: c, .. } => *t == tid && c.is_none_or(|c| c == cpu),
        };
        if here && !e.closed.load(Ordering::Relaxed) && e.st.lock().enabled && want(e) {
            v.push(e.clone());
        }
    }
    v
}

fn task_wanted(e: &Event) -> bool {
    let a = &e.attr;
    a.has(flag::COMM) || a.has(flag::MMAP) || a.has(flag::MMAP2) || a.has(flag::MMAP_DATA) || a.has(flag::TASK)
}

fn emit(e: &Event, rec: Option<&[u8]>, ident: &Ident) {
    if let Some(r) = rec
        && e.output(r, ident, false)
    {
        SIDE_BAND.inc();
    }
}

fn ident(e: &Event, pid: u32, tid: u32, cpu: usize) -> Ident {
    Ident { pid, tid, time: time::now_ns().wrapping_add(e.clock_off), id: e.id, stream_id: e.id, cpu: cpu as u32 }
}

/// `perf_event_fork` (proc's hook, after the new task exists).
fn fork(pid: u32, tid: u32, ppid: u32, ptid: u32) {
    let cpu = hal::percpu::this_cpu();
    let mut buf = [0u8; 128];
    for e in targets(ptid, cpu, task_wanted) {
        let id = ident(&e, pid, tid, cpu);
        let rec = record::task(&mut buf, RECORD_FORK, e.attr.sample_type, e.attr.has(flag::SAMPLE_ID_ALL), &id, ppid, ptid);
        emit(&e, rec, &id);
    }
}

/// `perf_event_exit_task`: the EXIT record, and the thread's own events end (EPOLLHUP).
fn exit(pid: u32, ppid: u32, tid: u32) {
    let cpu = hal::percpu::this_cpu();
    let mut buf = [0u8; 128];
    for e in targets(tid, cpu, task_wanted) {
        let id = ident(&e, pid, tid, cpu);
        let rec = record::task(&mut buf, RECORD_EXIT, e.attr.sample_type, e.attr.has(flag::SAMPLE_ID_ALL), &id, ppid, ppid);
        emit(&e, rec, &id);
    }
    let mine: Vec<Arc<Event>> = SIDE.lock().iter().filter(|e| e.tid() == Some(tid)).cloned().collect();
    for e in mine {
        e.set_enabled(false);
        e.hup.store(true, Ordering::Release);
        e.wq.wake_poll(vfs::poll::Events::HUP);
    }
}

/// `perf_event_exec` + `perf_event_comm(exec)` + the loader's `perf_event_mmap`s: a new image runs in `tid`.
fn image(p: &Arc<proc::Process>, tid: u32) {
    // `perf_event_enable_on_exec`: the thread's events that waited for it.
    let waiting: Vec<Arc<Event>> = SIDE.lock().iter().filter(|e| e.tid() == Some(tid) && e.attr.has(flag::ENABLE_ON_EXEC)).cloned().collect();
    for e in waiting {
        e.set_enabled(true);
    }
    let cpu = hal::percpu::this_cpu();
    let comm_to = targets(tid, cpu, |e| e.attr.has(flag::COMM));
    let mut buf = [0u8; 128];
    let name = comm_bytes(p.name.as_bytes());
    for e in &comm_to {
        let id = ident(e, p.pid, tid, cpu);
        let rec = record::comm(&mut buf, e.attr.sample_type, e.attr.has(flag::SAMPLE_ID_ALL), &id, name, true);
        emit(e, rec, &id);
    }
    maps(p, tid, cpu, 0, u64::MAX);
}

/// `perf_event_mmap` after a successful mmap of `[addr, addr + len)` by thread `tid` of `p` (the mmap syscall).
pub(crate) fn mmapped(p: &proc::Process, tid: u32, addr: u64, len: u64) {
    if !active() {
        return;
    }
    maps(p, tid, hal::percpu::this_cpu(), addr, addr.saturating_add(len));
}

/// TASK_COMM_LEN: 15 bytes and the NUL.
fn comm_bytes(n: &[u8]) -> &[u8] {
    &n[..n.len().min(15)]
}

/// MMAP2 (or MMAP) records for the mappings of `p` inside `[lo, hi)`: the executable ones for `attr.mmap`,
/// the others too for `attr.mmap_data`.
fn maps(p: &proc::Process, tid: u32, cpu: usize, lo: u64, hi: u64) {
    let to = targets(tid, cpu, |e| e.attr.has(flag::MMAP) || e.attr.has(flag::MMAP2) || e.attr.has(flag::MMAP_DATA));
    if to.is_empty() {
        return;
    }
    let text = crate::procfs::maps_of(p);
    let mut buf = [0u8; 4096 + 128];
    for line in text.lines() {
        let Some(m) = parse_maps_line(line) else { continue };
        if m.end <= lo || m.start >= hi {
            continue;
        }
        let exec = m.prot & 4 != 0;
        let name: &[u8] = match m.name {
            Some(n) => n.as_bytes(),
            None if exec => b"//anon",
            None => continue,
        };
        let map = Map { addr: m.start, len: m.end - m.start, pgoff: m.off, maj: m.maj, min: m.min, ino: m.ino, prot: m.prot, flags: m.flags, name };
        let misc = MISC_USER | if exec { 0 } else { MISC_MMAP_DATA };
        for e in &to {
            if !exec && !e.attr.has(flag::MMAP_DATA) {
                continue;
            }
            if exec && !(e.attr.has(flag::MMAP) || e.attr.has(flag::MMAP2)) {
                continue;
            }
            let id = ident(e, p.pid, tid, cpu);
            let (st, all) = (e.attr.sample_type, e.attr.has(flag::SAMPLE_ID_ALL));
            let rec = if e.attr.has(flag::MMAP2) { record::mmap2(&mut buf, st, all, &id, misc, &map) } else { record::mmap(&mut buf, st, all, &id, misc, &map) };
            emit(e, rec, &id);
        }
    }
}

/// One `/proc/<pid>/maps` line taken apart.
pub(crate) struct MapsLine<'a> {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    pub flags: u32,
    pub off: u64,
    pub maj: u32,
    pub min: u32,
    pub ino: u64,
    pub name: Option<&'a str>,
}

/// `start-end perms offset maj:min inode [name]`; prot as PROT_* bits, flags MAP_SHARED (1) or MAP_PRIVATE (2).
pub(crate) fn parse_maps_line(l: &str) -> Option<MapsLine<'_>> {
    let mut it = l.splitn(6, ' ');
    let (range, perms, off, dev, ino) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next()?);
    let name = it.next().map(str::trim).filter(|n| !n.is_empty());
    let (a, b) = range.split_once('-')?;
    let (maj, min) = dev.split_once(':')?;
    let p = perms.as_bytes();
    if p.len() != 4 {
        return None;
    }
    let prot = u32::from(p[0] == b'r') | u32::from(p[1] == b'w') << 1 | u32::from(p[2] == b'x') << 2;
    let flags = if p[3] == b's' { 1 } else { 2 };
    let hex = |s: &str| u64::from_str_radix(s, 16).ok();
    Some(MapsLine {
        start: hex(a)?,
        end: hex(b)?,
        prot,
        flags,
        off: hex(off)?,
        maj: hex(maj)? as u32,
        min: hex(min)? as u32,
        ino: ino.parse().ok()?,
        name,
    })
}
