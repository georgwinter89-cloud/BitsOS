//! The kept numbers of one address space (MM-ctr): present pages by kind (Linux `MM_ANONPAGES`,
//! `MM_FILEPAGES`, `MM_SHMEMPAGES`, plus V2's device pages), the RSS high-water mark (`hiwater_rss`) and the
//! page tables of the user half (`mm_pgtables_bytes`). Changed where entries come and go: a fill
//! (`fault::fill_at`), a swap (`fault::swap_page`: copy-on-write, a private file copy, a device page change), an
//! unmap (`unmap_one`, per released entry), a page-table allocation (`alloc_table`) or release (`Released.table`),
//! the fork's copy. Moves (`mremap`) change nothing. The drop needs none of them.
//!
//! All writers hold the space's lock (or own a space nobody sees yet), so the atomics only make the readers
//! (`/proc`, `ps`) lock-free. A count that would go below zero is a bug: it stays at zero with an event.

use super::Owner;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What a present page is, for the counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// The program's own frame (anonymous memory, private copies; RssAnon).
    Anon = 0,
    /// A page-cache frame of a file mapping (RssFile).
    File = 1,
    /// A page-cache frame of shared anonymous memory (RssShmem).
    Shmem = 2,
    /// A device's page (not in VmRSS, as Linux's VM_PFNMAP pages).
    Device = 3,
}

impl Class {
    /// The class of frame `phys` mapped in a range of `owner`: a program frame is anonymous wherever it lies
    /// (a private copy inside a file VMA too), a device VMA's page is the device's.
    pub(crate) fn of(phys: u64, owner: &Owner) -> Class {
        match owner {
            Owner::Device => Class::Device,
            Owner::Anon => Class::Anon,
            Owner::File(..) if mm_core::is_user_frame(phys) => Class::Anon,
            Owner::File(_, true) => Class::Shmem,
            Owner::File(_, false) => Class::File,
        }
    }

    /// A page-cache frame (its map count is kept in `share::file_maps`).
    pub(crate) fn cached(self) -> bool {
        matches!(self, Class::File | Class::Shmem)
    }
}

klog::counter!(pub(crate) UNDERFLOWS, "mm.as.counter_underflows");
klog::counter!(pub(crate) MISMATCHES, "mm.as.counter_mismatches");

#[derive(Default)]
pub(crate) struct Counters {
    pages: [AtomicU64; 4],
    hiwater_rss: AtomicU64,
    tables: AtomicU64,
    /// Underflows of this space's counts (the global `UNDERFLOWS` sums all spaces; a test asks its own space).
    underflows: AtomicU64,
}

impl Counters {
    pub(crate) fn add(&self, c: Class, n: u64) {
        self.pages[c as usize].fetch_add(n, Relaxed);
        if c != Class::Device {
            self.hiwater_rss.fetch_max(self.rss(), Relaxed);
        }
    }

    pub(crate) fn sub(&self, c: Class, n: u64) {
        self.take(&self.pages[c as usize], n, c);
    }

    /// One entry changed its frame from class `old` to class `new`.
    pub(crate) fn swap(&self, old: Class, new: Class) {
        if old != new {
            self.add(new, 1);
            self.sub(old, 1);
        }
    }

    pub(crate) fn tables_added(&self, n: u64) {
        self.tables.fetch_add(n, Relaxed);
    }

    pub(crate) fn tables_freed(&self, n: u64) {
        self.take(&self.tables, n, "tables");
    }

    fn take(&self, a: &AtomicU64, n: u64, what: impl core::fmt::Debug) {
        let old = a.try_update(Relaxed, Relaxed, |x| Some(x.saturating_sub(n))).unwrap_or(0);
        if old < n {
            UNDERFLOWS.inc();
            self.underflows.fetch_add(1, Relaxed);
            klog::event!(Error, "mm", "as.counter_underflow", counter = alloc::format!("{what:?}"), had = old, sub = n;
                "a page counter would go below zero: a fill or unmap path missed it (the count stays at 0)");
        }
    }

    pub(crate) fn get(&self, c: Class) -> u64 {
        self.pages[c as usize].load(Relaxed)
    }

    /// VmRSS in pages: anon + file + shmem.
    pub(crate) fn rss(&self) -> u64 {
        self.get(Class::Anon) + self.get(Class::File) + self.get(Class::Shmem)
    }

    /// Every present leaf (VmRSS plus device pages).
    pub(crate) fn present(&self) -> u64 {
        self.rss() + self.get(Class::Device)
    }

    /// VmHWM in pages (never below the current RSS).
    pub(crate) fn hiwater_rss(&self) -> u64 {
        self.hiwater_rss.load(Relaxed).max(self.rss())
    }

    pub(crate) fn tables(&self) -> u64 {
        self.tables.load(Relaxed)
    }

    pub(crate) fn underflows(&self) -> u64 {
        self.underflows.load(Relaxed)
    }

    /// At the drop: the present pages (by class, indexed as `Class`) and tables the teardown found must be what
    /// the counters kept; a difference means a path missed them (Linux's "Bad rss-counter state").
    pub(crate) fn check_teardown(&self, root: u64, found: &[u64; 4], tables: u64) {
        let kept = [Class::Anon, Class::File, Class::Shmem, Class::Device].map(|c| self.get(c));
        if kept != *found || self.tables() != tables {
            MISMATCHES.inc();
            klog::event!(Error, "mm", "as.counter_mismatch", root = klog::Hex(root), kept = alloc::format!("{kept:?}"), found = alloc::format!("{found:?}"),
                kept_tables = self.tables(), found_tables = tables; "the kept page or table counts of an ending address space differ from its tables");
        }
    }
}

impl Class {
    /// The class of frame `phys` mapped inside VMA `v` (None: no VMA there), by the same rules as `of`.
    pub(crate) fn in_vma(phys: u64, v: Option<&crate::vma::Vma>) -> Class {
        match v.and_then(|v| v.file.as_ref().map(|f| (f.device.is_some(), v.kind))) {
            Some((true, _)) => Class::Device,
            _ if mm_core::is_user_frame(phys) => Class::Anon,
            Some((false, crate::vma::VmaKind::Anonymous)) => Class::Shmem,
            Some(_) => Class::File,
            None => Class::Anon,
        }
    }
}
