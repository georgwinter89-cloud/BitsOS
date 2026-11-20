//! The open entries: one per (tree, process, file system) while that process keeps removing there (`GAP_NS`).
//! `OPEN` is taken under the names lock of the file system being changed (lock order: names, then `OPEN`, then
//! the file system's own, `place`); an entry taken out is dropped after `OPEN` is let go.

use super::record::What;
use crate::dentry::{Dentry, SuperBlock};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

pub(super) struct Batch {
    pub tree: u32,
    pub pid: u32,
    pub sb: Weak<SuperBlock>,
    /// `<fs>/.papierkorb/<id>/files` and the entry's record.
    pub files: Arc<Dentry>,
    pub record: Arc<Dentry>,
    pub id: Vec<u8>,
    /// The file system's top as the removing process named it ("/data").
    pub fs: Vec<u8>,
    /// The common top of what the entry holds, as names below the file system's top.
    pub top: Vec<Vec<u8>>,
    /// The top the record says now (None: not written yet).
    pub written: Option<Vec<Vec<u8>>>,
    pub what: What,
    pub secs: i64,
    pub command: String,
    pub chat: Option<String>,
    pub last_ns: u64,
}

pub(super) static OPEN: ksync::Mutex<Vec<Batch>> = ksync::Mutex::named("vfs.trash.open", Vec::new());

pub(super) fn now_ns() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Drops the entries whose process paused longer than `GAP_NS`, those of a file system that went and those
/// someone removed meanwhile (Georg emptying the trash by hand).
/// They go to `gone`, for the caller to drop after `OPEN`.
pub(super) fn prune(open: &mut Vec<Batch>, now: u64, gone: &mut Vec<Batch>) {
    let mut i = 0;
    while i < open.len() {
        let b = &open[i];
        if b.sb.strong_count() > 0 && now.saturating_sub(b.last_ns) <= super::GAP_NS && !b.files.is_dead_dir() && !b.files.is_unlinked() {
            i += 1;
        } else {
            let b = open.swap_remove(i);
            match gone.try_reserve(1) {
                Ok(()) => gone.push(b),
                // No memory to hand it out: dropped here, as an ended entry must not be used again.
                Err(_) => drop(b),
            }
        }
    }
}

pub(super) fn find(open: &[Batch], tree: u32, pid: u32, sb: &Arc<SuperBlock>) -> Option<usize> {
    open.iter().position(|b| b.tree == tree && b.pid == pid && core::ptr::eq(b.sb.as_ptr(), Arc::as_ptr(sb)))
}

/// Ends the open entries of process `pid`: its next removal starts a new entry (a pause, for the tests).
pub fn end_batches_of(pid: u32) {
    let gone: Vec<Batch> = {
        let mut open = OPEN.lock();
        let (gone, keep) = core::mem::take(&mut *open).into_iter().partition(|b| b.pid == pid);
        *open = keep;
        gone
    };
    drop(gone);
}

/// Ends the open entries on `sb` (a restore or an emptying changed its trash).
pub(super) fn end_on(sb: &Arc<SuperBlock>) {
    let gone: Vec<Batch> = {
        let mut open = OPEN.lock();
        let (gone, keep) = core::mem::take(&mut *open).into_iter().partition(|b| core::ptr::eq(b.sb.as_ptr(), Arc::as_ptr(sb)));
        *open = keep;
        gone
    };
    drop(gone);
}
