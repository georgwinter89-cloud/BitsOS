//! The run's picture of the machine (perf's "machine/thread/map" model, small): the programs by pid with their
//! name and code mappings, built from `/proc` at the start and from the COMM, MMAP2, FORK and EXIT records, and
//! the samples turned into (program, stack of functions) counts for the current window.

use crate::store::{Builder, KERNEL, Window};
use crate::syms::{Files, Kernel};
use perf_abi::record::{Parsed, Record};
use perf_abi::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Deepest stack kept per sample (innermost frames).
pub const MAX_DEPTH: usize = 32;
/// The name of BitsOS's kernel threads (pid 0, a tid with the top bit) and of a pid nobody named.
pub const KTHREADS: &str = "[Kern-Threads]";

#[derive(Clone)]
struct Map {
    start: u64,
    end: u64,
    pgoff: u64,
    path: Arc<str>,
}

#[derive(Clone, Default)]
struct Proc {
    comm: String,
    maps: Vec<Map>,
    dead: bool,
}

impl Proc {
    fn add_map(&mut self, m: Map) {
        self.maps.retain(|x| x.end <= m.start || x.start >= m.end);
        let at = self.maps.partition_point(|x| x.start < m.start);
        self.maps.insert(at, m);
    }
    fn find(&self, a: u64) -> Option<&Map> {
        let i = self.maps.partition_point(|x| x.start <= a).checked_sub(1)?;
        let m = &self.maps[i];
        (a < m.end).then_some(m)
    }
}

pub struct Model {
    procs: HashMap<u32, Proc>,
    pub kernel: Kernel,
    files: Files,
    pub b: Builder,
    kcache: HashMap<u64, u32>,
    ucache: HashMap<(Arc<str>, u64), u32>,
    cur: HashMap<(u32, u32), u64>,
    pub idle: u64,
    idle_window: u64,
    pub samples: u64,
    pub lost: u64,
}

impl Model {
    pub fn new(kernel: Kernel) -> Model {
        Model { procs: HashMap::new(), kernel, files: Files::default(), b: Builder::default(), kcache: HashMap::new(), ucache: HashMap::new(), cur: HashMap::new(), idle: 0, idle_window: 0, samples: 0, lost: 0 }
    }

    /// The programs running now, from `/proc/<pid>/comm` and `maps` (what perf calls synthesizing).
    pub fn scan_proc(&mut self) {
        let Ok(rd) = std::fs::read_dir("/proc") else { return };
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim_end().to_string();
            let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
            let p = self.procs.entry(pid).or_default();
            p.comm = comm;
            for l in maps.lines() {
                if let Some(m) = maps_line(l) {
                    p.add_map(m);
                }
            }
        }
    }

    /// A side-band record (samples go to `sample`).
    pub fn side_band(&mut self, r: &Record<'_>) {
        match *r {
            Record::Comm { pid, name, exec, .. } => {
                let p = self.procs.entry(pid).or_default();
                p.comm = String::from_utf8_lossy(name).into_owned();
                if exec {
                    p.maps.clear();
                }
                p.dead = false;
            }
            Record::Mmap2 { pid, addr, len, pgoff, prot, name, .. } if prot & 4 != 0 => {
                let m = Map { start: addr, end: addr.saturating_add(len), pgoff, path: Arc::from(String::from_utf8_lossy(name).as_ref()) };
                self.procs.entry(pid).or_default().add_map(m);
            }
            Record::Mmap { pid, addr, len, pgoff, name, .. } => {
                let m = Map { start: addr, end: addr.saturating_add(len), pgoff, path: Arc::from(String::from_utf8_lossy(name).as_ref()) };
                self.procs.entry(pid).or_default().add_map(m);
            }
            Record::Fork { pid, ppid, .. } if pid != ppid => {
                let parent = self.procs.get(&ppid).cloned().unwrap_or_default();
                self.procs.insert(pid, Proc { dead: false, ..parent });
            }
            Record::Exit { pid, tid, .. } if pid == tid => {
                if let Some(p) = self.procs.get_mut(&pid) {
                    p.dead = true;
                }
            }
            Record::Lost { lost, .. } => self.lost += lost,
            _ => {}
        }
    }

    /// Forgets the programs that ended (after the samples of the same round were counted).
    pub fn purge(&mut self) {
        self.procs.retain(|_, p| !p.dead);
    }

    fn kernel_func(&mut self, a: u64) -> u32 {
        if let Some(&f) = self.kcache.get(&a) {
            return f;
        }
        let name = self.kernel.lookup(a).unwrap_or("[Kern]").to_string();
        let f = self.b.func(&name, KERNEL);
        self.kcache.insert(a, f);
        f
    }

    fn user_func(&mut self, pid: u32, a: u64) -> u32 {
        let hit = self.procs.get(&pid).and_then(|p| p.find(a)).map(|m| (m.path.clone(), a - m.start + m.pgoff));
        let Some((path, off)) = hit else { return self.b.func("[unbekannt]", "[unbekannt]") };
        if let Some(&f) = self.ucache.get(&(path.clone(), off)) {
            return f;
        }
        let f = match self.files.lookup(&path, off) {
            Some(n) => self.b.func(&n, &path),
            None => self.b.func("[unbekannt]", &path),
        };
        self.ucache.insert((path, off), f);
        f
    }

    /// One sample: idle (pid 0, tid 0) or a stack of functions of a program.
    pub fn sample(&mut self, s: &Parsed<'_>) {
        if s.pid == 0 && s.tid == 0 {
            self.idle += 1;
            self.idle_window += 1;
            return;
        }
        self.samples += 1;
        let mut frames: Vec<u32> = Vec::with_capacity(MAX_DEPTH);
        if s.chain_len() == 0 {
            let f = if s.misc & MISC_CPUMODE_MASK == MISC_KERNEL { self.kernel_func(s.ip) } else { self.user_func(s.pid, s.ip) };
            frames.push(f);
        }
        let (mut kernel, mut first) = (s.misc & MISC_CPUMODE_MASK == MISC_KERNEL, true);
        for i in 0..s.chain_len() {
            if frames.len() >= MAX_DEPTH {
                break;
            }
            let a = s.chain_at(i);
            if a >= CONTEXT_MAX {
                kernel = a == CONTEXT_KERNEL;
                first = true;
                continue;
            }
            // A return address points behind its call: look up the call itself.
            let at = if first { a } else { a.saturating_sub(1) };
            first = false;
            let f = if kernel { self.kernel_func(at) } else { self.user_func(s.pid, at) };
            frames.push(f);
        }
        let name = match self.procs.get(&s.pid) {
            Some(p) if !p.comm.is_empty() => p.comm.clone(),
            _ if s.pid == 0 => KTHREADS.to_string(),
            _ => format!("[{}]", s.pid),
        };
        let prog = self.b.prog(&name, s.pid);
        let stack = self.b.stack(&frames);
        *self.cur.entry((prog, stack)).or_insert(0) += 1;
    }

    /// Closes the current window.
    pub fn close_window(&mut self, start_ms: u64, len_ms: u64) {
        let mut counts: Vec<(u32, u32, u64)> = self.cur.drain().map(|((p, s), c)| (p, s, c)).collect();
        counts.sort_unstable();
        let idle = std::mem::take(&mut self.idle_window);
        self.b.p.windows.push(Window { start_ms, len_ms, idle, counts });
    }
}

/// One `/proc/<pid>/maps` line of executable code with a name.
fn maps_line(l: &str) -> Option<Map> {
    let mut it = l.split_whitespace();
    let (range, perms, off, _dev, _ino) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next()?);
    let name: String = it.collect::<Vec<_>>().join(" ");
    if perms.as_bytes().get(2) != Some(&b'x') || name.is_empty() {
        return None;
    }
    let (a, b) = range.split_once('-')?;
    Some(Map { start: u64::from_str_radix(a, 16).ok()?, end: u64::from_str_radix(b, 16).ok()?, pgoff: u64::from_str_radix(off, 16).ok()?, path: Arc::from(name.as_str()) })
}
