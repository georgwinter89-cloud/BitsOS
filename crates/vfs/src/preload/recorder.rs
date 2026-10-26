//! FS-13: the live side of the record. An exec opens a session for its executable (a second exec of the same
//! program inside the window, Brave's children, joins it; so does a forked child); files the session's
//! processes open are noted with their path, and every page-cache fill of such a file in the session's window
//! adds its pages. When the window is over the session is handed to the record (`finish`).
//!
//! Plain data with the process id passed in, so the host tests drive it directly; `K` keeps an opened file
//! alive while its session runs (the kernel's `Path`: the cache pointer the session knows the file by stays
//! that file's).

use super::ranges::Ranges;
use super::record::{FileRec, Ident};
use alloc::vec::Vec;

/// What a session keeps of an opened file: in the kernel a weak reference to its inode (the inode's memory, and
/// with it the cache pointer the session knows the file by, stays that file's while the session runs, but the
/// file, its mount and its clusters are not held: INT-W13). It tells the file's identity when the session ends,
/// so a later boot can see the file changed; None when the file is gone by then (it is left out of the record).
pub trait Keep {
    fn ident(&self) -> Option<Ident>;
}

impl Keep for () {
    fn ident(&self) -> Option<Ident> {
        Some(Ident::default())
    }
}

struct SFile<K> {
    cache: usize,
    path: Vec<u8>,
    ranges: Ranges,
    /// The order of the first fill (files are preloaded in the order the program first read them).
    first_fill: u64,
    keep: K,
}

struct Session<K> {
    id: u64,
    exe: Vec<u8>,
    end_ns: u64,
    pages: u64,
    /// Cleared while it ran: handed out only to drop its keep-alives.
    discard: bool,
    /// Sorted by `cache`.
    files: Vec<SFile<K>>,
}

/// A finished session: the program and the files it read, in reading order.
#[derive(Debug, PartialEq, Eq)]
pub struct Finished {
    pub exe: Vec<u8>,
    pub files: Vec<FileRec>,
}

/// A session taken out of the recorder under its lock; `into_finished` (sorting, allocating, dropping the
/// keep-alives, which may take the file system's locks) runs after the lock.
pub struct Taken<K>(Session<K>);

impl<K: Keep> Taken<K> {
    /// Ok(None): the session was cleared. Err: no memory to hand it over (it is dropped).
    #[allow(clippy::result_unit_err)]
    pub fn into_finished(self) -> Result<Option<Finished>, ()> {
        let mut s = self.0;
        if s.discard {
            return Ok(None);
        }
        // In place (no allocation); unfilled files (u64::MAX) go last and are left out.
        s.files.sort_unstable_by_key(|f| f.first_fill);
        let mut files = Vec::new();
        if files.try_reserve_exact(s.files.len()).is_err() {
            return Err(());
        }
        for f in s.files {
            if !f.ranges.is_empty()
                && let Some(id) = f.keep.ident()
            {
                files.push(FileRec { path: f.path, id, ranges: f.ranges });
            }
        }
        Ok(Some(Finished { exe: s.exe, files }))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecStats {
    /// Execs seen (whether or not they began a session).
    pub execs: u64,
    /// Sessions begun (program starts).
    pub starts: u64,
    /// Fills added to a session.
    pub fills: u64,
    /// Fills by a session's process on a file it did not open (a block device's metadata pages; FS-15: a fill of
    /// the program the process execs next is handed to that program's session at `exec_done`).
    pub unknown: u64,
    /// Fills or files not recorded: no memory, or past the session's page budget.
    pub dropped: u64,
    /// FS-15: the pages of the `unknown` and `dropped` fills (what the device read but no record holds).
    pub unknown_pages: u64,
    pub dropped_pages: u64,
    /// FS-15: pages read while a session ran by processes that record nothing (other programs).
    pub other_pages: u64,
    /// FS-15: pages read while a session ran by kernel threads (a block device's own reads: the data volume's
    /// metadata through \BITSOS2\data.img), not the preload's own. Filled in by `preload::stats`.
    pub kthread_pages: u64,
}

/// FS-15: a recording process's fills of a file its session does not know (yet), by (pid, cache). Exec reads the
/// new program's pages (its headers, the page the loader zeroes the bss tail in) while the process still records
/// for its old program; when the exec hook has noted the new program's files, `exec_done` hands these fills to
/// the new session.
struct Pending {
    pid: u32,
    cache: usize,
    ranges: Ranges,
}

pub struct Recorder<K> {
    sessions: Vec<Session<K>>,
    /// (pid, session id), sorted by pid.
    pids: Vec<(u32, u64)>,
    /// Sorted by (pid, cache); only for pids in `pids`, emptied for a pid at its exec and when it stops recording.
    pending: Vec<Pending>,
    /// The pages `pending` holds: never more than the recording session's page budget has left (fixer F1).
    pending_pages: u64,
    next_id: u64,
    seq: u64,
    pub stats: RecStats,
}

impl<K> Default for Recorder<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> Recorder<K> {
    pub const fn new() -> Recorder<K> {
        Recorder { sessions: Vec::new(), pids: Vec::new(), pending: Vec::new(), pending_pages: 0, next_id: 1, seq: 0, stats: RecStats { execs: 0, starts: 0, fills: 0, unknown: 0, dropped: 0, unknown_pages: 0, dropped_pages: 0, other_pages: 0, kthread_pages: 0 } }
    }

    /// Host tests: the pages the pending fills hold.
    #[cfg(test)]
    pub(crate) fn pending_pages(&self) -> u64 {
        self.pending_pages
    }

    /// Sessions still recording.
    pub fn active(&self) -> usize {
        self.sessions.len()
    }

    /// The processes that record now (the caller's lock-free filter is built from them).
    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.pids.iter().map(|p| p.0)
    }

    fn session_of(&self, pid: u32) -> Option<usize> {
        let i = self.pids.binary_search_by_key(&pid, |p| p.0).ok()?;
        let id = self.pids[i].1;
        self.sessions.iter().position(|s| s.id == id)
    }

    fn set_pid(&mut self, pid: u32, id: u64) -> bool {
        match self.pids.binary_search_by_key(&pid, |p| p.0) {
            Ok(i) => {
                self.pids[i].1 = id;
                true
            }
            Err(i) => {
                if self.pids.try_reserve(1).is_err() {
                    return false;
                }
                self.pids.insert(i, (pid, id));
                true
            }
        }
    }

    fn forget_pid(&mut self, pid: u32) {
        if let Ok(i) = self.pids.binary_search_by_key(&pid, |p| p.0) {
            self.pids.remove(i);
        }
        let lo = self.pending.partition_point(|p| p.pid < pid);
        let hi = self.pending.partition_point(|p| p.pid <= pid);
        self.drop_pending(lo, hi);
    }

    /// Removes `pending[lo..hi]` (one pid's entries, or none).
    fn drop_pending(&mut self, lo: usize, hi: usize) {
        let n = self.pending[lo..hi].iter().fold(0u64, |n, p| n.saturating_add(p.ranges.pages()));
        self.pending_pages = self.pending_pages.saturating_sub(n);
        self.pending.drain(lo..hi);
    }

    /// Notes an unknown fill of `pid` while `pending` stays within `budget` pages (no memory or no budget: it
    /// stays unknown and is not kept).
    fn note_pending(&mut self, pid: u32, cache: usize, first: u64, end: u64, budget: u64) {
        if self.pending_pages.saturating_add(end.saturating_sub(first)) > budget {
            return;
        }
        match self.pending.binary_search_by_key(&(pid, cache), |p| (p.pid, p.cache)) {
            Ok(i) => {
                let before = self.pending[i].ranges.pages();
                let _ = self.pending[i].ranges.add(first, end);
                self.pending_pages += self.pending[i].ranges.pages() - before;
            }
            Err(i) => {
                let mut ranges = Ranges::new();
                if self.pending.try_reserve(1).is_ok() && ranges.add(first, end) {
                    self.pending_pages += ranges.pages();
                    self.pending.insert(i, Pending { pid, cache, ranges });
                }
            }
        }
    }

    /// FS-15: the exec hook of `pid` has noted the new program's files: the fills `pid` made of any of them before
    /// (as `unknown`) join its session as read now; its other pending fills belonged to the old program and go.
    pub fn exec_done(&mut self, pid: u32, max_pages: u64) {
        let lo = self.pending.partition_point(|p| p.pid < pid);
        let hi = self.pending.partition_point(|p| p.pid <= pid);
        if lo == hi {
            return;
        }
        // Taken before the hand-over cuts the entries to the budget.
        let held = self.pending[lo..hi].iter().fold(0u64, |n, p| n.saturating_add(p.ranges.pages()));
        if let Some(i) = self.session_of(pid) {
            for k in lo..hi {
                let s = &mut self.sessions[i];
                let Ok(f) = s.files.binary_search_by_key(&self.pending[k].cache, |f| f.cache) else { continue };
                // Pages handed over are not unknown any more: they are recorded, or dropped (budget, memory).
                let n = self.pending[k].ranges.pages();
                self.stats.unknown_pages = self.stats.unknown_pages.saturating_sub(n);
                let cut = self.pending[k].ranges.cut_to(max_pages.saturating_sub(s.pages));
                let file = &mut s.files[f];
                let before = file.ranges.pages();
                let mut lost = cut;
                for &(a, b) in self.pending[k].ranges.as_slice() {
                    if !file.ranges.add(a, b) {
                        lost += b - a;
                    }
                }
                let grown = file.ranges.pages() - before;
                if grown > 0 && file.first_fill == u64::MAX {
                    file.first_fill = self.seq;
                    self.seq += 1;
                }
                s.pages += grown;
                if lost > 0 {
                    self.stats.dropped += 1;
                    self.stats.dropped_pages += lost;
                }
            }
        }
        self.pending_pages = self.pending_pages.saturating_sub(held);
        self.pending.drain(lo..hi);
    }

    /// `pid` now runs `exe` (execve, or a spawn by the kernel): it joins the program's running session or
    /// begins one that records until `now + window_ns`. True when a session began (a program start).
    pub fn exec(&mut self, pid: u32, exe: &[u8], now: u64, window_ns: u64) -> bool {
        self.stats.execs += 1;
        if let Some(s) = self.sessions.iter().find(|s| s.exe == exe && !s.discard) {
            let id = s.id;
            if !self.set_pid(pid, id) {
                self.forget_pid(pid);
                self.stats.dropped += 1;
            }
            return false;
        }
        let mut name = Vec::new();
        if name.try_reserve_exact(exe.len()).is_err() || self.sessions.try_reserve(1).is_err() {
            // The process no longer runs what an older session of its pid recorded.
            self.forget_pid(pid);
            self.stats.dropped += 1;
            return false;
        }
        name.extend_from_slice(exe);
        let id = self.next_id;
        self.next_id += 1;
        if !self.set_pid(pid, id) {
            self.forget_pid(pid);
            self.stats.dropped += 1;
            return false;
        }
        self.sessions.push(Session { id, exe: name, end_ns: now.saturating_add(window_ns), pages: 0, discard: false, files: Vec::new() });
        self.stats.starts += 1;
        true
    }

    /// A forked child records for its parent's session (a zygote's children read for the program). A child of
    /// a parent that records nothing records nothing either, also when its pid was a recording process's
    /// before (that process exited and the pid was given out again).
    pub fn fork(&mut self, parent: u32, child: u32) {
        let Some(i) = self.session_of(parent) else {
            self.forget_pid(child);
            return;
        };
        let id = self.sessions[i].id;
        if !self.set_pid(child, id) {
            self.forget_pid(child);
            self.stats.dropped += 1;
        }
    }

    /// `pid` exited: it records nothing any more (its pid may be given to another process).
    pub fn exited(&mut self, pid: u32) {
        self.forget_pid(pid);
    }

    /// Whether an open of the file cached at `cache` by `pid` is to be noted (its session runs and does not know
    /// the file yet). Asked first, so the path is built only when needed and outside the recorder's lock.
    pub fn wants(&self, pid: u32, cache: usize) -> bool {
        self.session_of(pid).is_some_and(|i| self.sessions[i].files.binary_search_by_key(&cache, |f| f.cache).is_err())
    }

    /// Notes that `pid`'s session opened the file `path`, cached at `cache`. A keep-alive that is not stored
    /// (no session, the file is known, no memory) comes back, for the caller to drop after its lock.
    #[must_use]
    pub fn open(&mut self, pid: u32, cache: usize, path: Vec<u8>, keep: K) -> Option<K> {
        let Some(i) = self.session_of(pid) else { return Some(keep) };
        let s = &mut self.sessions[i];
        let Err(at) = s.files.binary_search_by_key(&cache, |f| f.cache) else { return Some(keep) };
        if s.files.try_reserve(1).is_err() {
            self.stats.dropped += 1;
            return Some(keep);
        }
        s.files.insert(at, SFile { cache, path, ranges: Ranges::new(), first_fill: u64::MAX, keep });
        None
    }

    /// `pid` read pages `[first, end)` of the file cached at `cache` from its device. Nothing more once the
    /// session holds `max_pages` pages.
    pub fn fill(&mut self, pid: u32, cache: usize, first: u64, end: u64, max_pages: u64) {
        let n = end.saturating_sub(first);
        let Some(i) = self.session_of(pid) else {
            self.stats.other_pages += n;
            return;
        };
        let s = &mut self.sessions[i];
        let Ok(f) = s.files.binary_search_by_key(&cache, |f| f.cache) else {
            self.stats.unknown += 1;
            self.stats.unknown_pages += n;
            let budget = max_pages.saturating_sub(s.pages);
            self.note_pending(pid, cache, first, end, budget);
            return;
        };
        // A soft bound (one fill window past it at most); the record cuts each program to the exact one.
        if s.pages >= max_pages {
            self.stats.dropped += 1;
            self.stats.dropped_pages += n;
            return;
        }
        let file = &mut s.files[f];
        let before = file.ranges.pages();
        if !file.ranges.add(first, end) {
            self.stats.dropped += 1;
            self.stats.dropped_pages += n;
            return;
        }
        if file.first_fill == u64::MAX {
            file.first_fill = self.seq;
            self.seq += 1;
        }
        s.pages += file.ranges.pages() - before;
        self.stats.fills += 1;
    }

    /// The sessions whose window ended by `now` (all with `all`), taken out with their processes, to be turned
    /// into `Finished` after the caller's lock. Nothing is dropped here: when the list for them cannot grow,
    /// they stay (their window is over, the next call takes them).
    pub fn finish(&mut self, now: u64, all: bool) -> Vec<Taken<K>> {
        let mut out = Vec::new();
        let n = self.sessions.iter().filter(|s| all || s.end_ns <= now).count();
        if n == 0 || out.try_reserve_exact(n).is_err() {
            return out;
        }
        let mut i = 0;
        while i < self.sessions.len() {
            if !all && self.sessions[i].end_ns > now {
                i += 1;
                continue;
            }
            let s = self.sessions.swap_remove(i);
            self.pids.retain(|p| p.1 != s.id);
            let pids = &self.pids;
            self.pending.retain(|p| pids.binary_search_by_key(&p.pid, |x| x.0).is_ok());
            self.pending_pages = self.pending.iter().fold(0u64, |n, p| n.saturating_add(p.ranges.pages()));
            out.push(Taken(s));
        }
        out
    }

    /// Forgets every running session (the record was cleared, or recording switched off). Without memory to
    /// hand them out they stay, marked to be discarded, until the next `finish`.
    pub fn clear(&mut self) -> Vec<Taken<K>> {
        self.pids.clear();
        self.pending.clear();
        self.pending_pages = 0;
        for s in self.sessions.iter_mut() {
            s.discard = true;
            s.end_ns = 0;
        }
        self.finish(0, true)
    }
}
