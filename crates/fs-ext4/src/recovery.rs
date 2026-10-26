//! Journal replay at mount (`jbd2_journal_recover`, linux-6.6/fs/jbd2/recovery.c:286): three passes over
//! the log from `s_start` like `do_one_pass` (:463). SCAN finds the last transaction with a valid commit
//! block, REVOKE collects the revoke records of those transactions, REPLAY hands each logged block that no
//! later revoke covers and whose checksum holds to `put` (its home block, or a read-only mount's memory).
//!
//! The journal's contents are hostile input: every walk is bounded by the log's length, block numbers are
//! checked against the volume, and the revoke table is allocated fallibly. A descriptor or revoke block with a
//! bad checksum is judged at its transaction's commit block like Linux (`need_check_commit_time`, :584/:754):
//! a commit older than the one before means a stale log, the scan ends quietly; otherwise the transaction is
//! damaged. Linux then fails the mount (-EFSBADCRC); V2 replays the transactions before it and counts the
//! damage in `bad`, which mounts the volume read-only for e2fsck (`Volume::recover`).

use crate::dev::Dev;
use crate::errno::*;
use crate::jbd2::{self, gb32, Jsb, Layout, Tag};
use alloc::vec::Vec;

/// Where the journal's blocks lie on the volume: (first journal block, first volume block, count) runs.
#[derive(Default)]
pub struct Map {
    pub runs: Vec<(u32, u64, u32)>,
}

impl Map {
    /// The volume block of journal block `j`.
    pub fn phys(&self, j: u32) -> Option<u64> {
        let i = self.runs.partition_point(|r| r.0 <= j).checked_sub(1)?;
        let (l, p, n) = self.runs[i];
        (j - l < n).then(|| p + (j - l) as u64)
    }
    /// How many journal blocks from `j` on are contiguous on the volume (at least 1 when mapped).
    pub fn contiguous(&self, j: u32) -> u32 {
        let Some(i) = self.runs.partition_point(|r| r.0 <= j).checked_sub(1) else { return 0 };
        let (l, _, n) = self.runs[i];
        n.saturating_sub(j - l)
    }
}

/// What a replay did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Replayed {
    /// The first transaction id without a valid commit (`info.end_transaction`).
    pub end: u32,
    pub transactions: u32,
    pub blocks: u64,
    pub revoke_hits: u64,
    /// Logged blocks skipped (bad checksum or a block number outside the volume), plus a committed
    /// transaction whose descriptor or revoke block is damaged (the scan ends before it).
    pub bad: u64,
}

pub struct Log<'a> {
    pub dev: &'a dyn Dev,
    pub map: &'a Map,
    pub jsb: &'a Jsb,
    pub lay: Layout,
}

impl Log<'_> {
    fn read(&self, j: u32) -> Result<Vec<u8>, i64> {
        let p = self.map.phys(j).ok_or(EUCLEAN)?;
        let mut b = Vec::new();
        b.try_reserve_exact(self.lay.bs).map_err(|_| ENOMEM)?;
        b.resize(self.lay.bs, 0);
        self.dev.read(p * self.lay.bs as u64, &mut b)?;
        Ok(b)
    }
    fn next(&self, j: u32, n: u32) -> u32 {
        let (first, end) = (self.jsb.first, self.jsb.log_end());
        let len = end - first;
        first + ((j - first) as u64 + n as u64).rem_euclid(len as u64) as u32
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Pass {
    Scan,
    Revoke,
    Replay,
}

/// Replays the log (nothing when `s_start` is 0). `fs_blocks` bounds the home block numbers.
pub fn replay(log: &Log, fs_blocks: u64, put: &mut dyn FnMut(u64, Vec<u8>) -> Result<(), i64>) -> Result<Replayed, i64> {
    let mut r = Replayed { end: log.jsb.sequence, ..Replayed::default() };
    if log.jsb.start == 0 {
        r.end = log.jsb.sequence.wrapping_add(1);
        return Ok(r);
    }
    let mut revoked: Vec<(u64, u32)> = Vec::new();
    one_pass(log, Pass::Scan, &mut r, &mut revoked, fs_blocks, put)?;
    one_pass(log, Pass::Revoke, &mut r, &mut revoked, fs_blocks, put)?;
    revoked.sort_unstable();
    // Keep the newest revoke of each block (`set_revoke`, revoke.c:683).
    let mut dedup: Vec<(u64, u32)> = Vec::new();
    dedup.try_reserve_exact(revoked.len()).map_err(|_| ENOMEM)?;
    for (b, s) in revoked {
        match dedup.last_mut() {
            Some(l) if l.0 == b => {
                if jbd2::tid_gt(s, l.1) {
                    l.1 = s;
                }
            }
            _ => dedup.push((b, s)),
        }
    }
    one_pass(log, Pass::Replay, &mut r, &mut dedup, fs_blocks, put)?;
    Ok(r)
}

fn one_pass(log: &Log, pass: Pass, r: &mut Replayed, revoked: &mut Vec<(u64, u32)>, fs_blocks: u64, put: &mut dyn FnMut(u64, Vec<u8>) -> Result<(), i64>) -> Result<(), i64> {
    let mut seq = log.jsb.sequence;
    let mut pos = log.jsb.start;
    // A log is never longer than the journal: the walk ends after that many blocks whatever the contents.
    let limit = (log.jsb.log_end() - log.jsb.first) as u64;
    let mut walked = 0u64;
    // Scan: a descriptor or revoke block of this transaction failed its checksum; the commit decides.
    let mut check_time = false;
    let mut last_time = 0u64;
    loop {
        if pass != Pass::Scan && !jbd2::tid_gt(r.end, seq) {
            break;
        }
        if walked >= limit {
            break;
        }
        let b = log.read(pos)?;
        walked += 1;
        pos = log.next(pos, 1);
        if gb32(&b, 0) != jbd2::MAGIC || gb32(&b, 8) != seq {
            break;
        }
        match gb32(&b, 4) {
            jbd2::DESCRIPTOR => {
                let tags = jbd2::tags(&log.lay, &b);
                if !log.lay.tail_ok(&b) {
                    if pass != Pass::Scan {
                        // Not reached: the scan ended before this transaction.
                        return Err(EUCLEAN);
                    }
                    klog::event!(Info, "ext4", "journal.bad_descriptor", seq = seq, at = pos; "descriptor checksum wrong: the commit time decides between stale and damaged");
                    check_time = true;
                }
                if pass != Pass::Replay {
                    pos = log.next(pos, tags.len() as u32);
                    walked += tags.len() as u64;
                    continue;
                }
                for t in tags {
                    let at = pos;
                    pos = log.next(pos, 1);
                    walked += 1;
                    replay_one(log, r, revoked, fs_blocks, seq, at, t, put)?;
                }
            }
            jbd2::COMMIT => {
                let time = jbd2::gb64(&b, 0x30);
                if check_time {
                    if time >= last_time {
                        klog::event!(Warn, "ext4", "journal.damaged", seq = seq; "a committed transaction has a damaged descriptor or revoke block: not replayed, the mount goes read-only for e2fsck");
                        r.bad += 1;
                    } else {
                        klog::event!(Info, "ext4", "journal.stale", seq = seq; "a checksum mismatch in an older commit: stale log, the scan ends");
                    }
                    break;
                }
                if pass == Pass::Scan && !log.lay.commit_ok(&b) {
                    klog::event!(Warn, "ext4", "journal.bad_commit", seq = seq; "commit checksum wrong: this transaction is not replayed");
                    break;
                }
                if pass == Pass::Scan {
                    r.transactions += 1;
                    last_time = time;
                }
                seq = seq.wrapping_add(1);
            }
            jbd2::REVOKE => {
                if !log.lay.tail_ok(&b) {
                    if pass != Pass::Scan {
                        return Err(EUCLEAN);
                    }
                    klog::event!(Info, "ext4", "journal.bad_revoke", seq = seq; "revoke checksum wrong: the commit time decides between stale and damaged");
                    check_time = true;
                    continue;
                }
                if pass == Pass::Revoke {
                    let Some(list) = jbd2::revokes(&log.lay, &b) else {
                        klog::event!(Warn, "ext4", "journal.bad_revoke_count", seq = seq);
                        return Err(EUCLEAN);
                    };
                    revoked.try_reserve(list.len()).map_err(|_| ENOMEM)?;
                    revoked.extend(list.into_iter().map(|blk| (blk, seq)));
                }
            }
            _ => break,
        }
    }
    if pass == Pass::Scan {
        r.end = seq;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn replay_one(log: &Log, r: &mut Replayed, revoked: &[(u64, u32)], fs_blocks: u64, seq: u32, at: u32, t: Tag, put: &mut dyn FnMut(u64, Vec<u8>) -> Result<(), i64>) -> Result<(), i64> {
    if let Ok(i) = revoked.binary_search_by(|e| e.0.cmp(&t.blk))
        && !jbd2::tid_gt(seq, revoked[i].1)
    {
        r.revoke_hits += 1;
        return Ok(());
    }
    let mut d = log.read(at)?;
    let csum_ok = match log.lay.csum {
        jbd2::Csum::None => true,
        jbd2::Csum::V3 => log.lay.block_csum(seq, &d) == t.csum,
        jbd2::Csum::V2 => log.lay.block_csum(seq, &d) as u16 == t.csum as u16,
    };
    if !csum_ok || t.blk >= fs_blocks {
        klog::event!(Warn, "ext4", "journal.bad_block", seq = seq, block = t.blk, csum_ok = csum_ok; "logged block not replayed");
        r.bad += 1;
        return Ok(());
    }
    if t.flags & jbd2::FLAG_ESCAPE != 0 {
        d[..4].copy_from_slice(&jbd2::MAGIC.to_be_bytes());
    }
    r.blocks += 1;
    put(t.blk, d)
}

