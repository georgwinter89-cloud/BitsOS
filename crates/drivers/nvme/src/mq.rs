//! BL-1: the I/O queues. One queue pair per CPU (or per vector when the controller grants fewer), each with its tag
//! set, many commands in flight, completions reaped in the queue's interrupt (Linux 7.x: drivers/nvme/host/pci.c
//! `struct nvme_queue` with `sq_lock`, `nvme_poll_cq` :1620, `nvme_irq` :1641, `nvme_cid`/`genctr` in nvme.h; tags as
//! block/blk-mq-tag.c `__blk_mq_get_tag` :110). No lock is held across a device wait: the submission lock covers the
//! slot copy and the doorbell, the completion lock (an `IrqSpinLock`, the interrupt takes it) one pass over the ring.
//!
//! A command's CID is its tag plus a per-tag generation in the bits above it, so a late completion of a command
//! given up (or a hostile CID) never matches the tag's next command; it is counted as stale. A slot moves
//! idle -> in flight (its owner, before the doorbell) -> done (the reaper, under the completion lock) -> idle (its
//! owner). Its pages (PRP list, bounce, DSM range) come from the queue's pool and go back only once the command
//! completed or the device was stopped after it was placed (`Controller::stop_gen`): a page the device may still
//! fetch is never handed out again, and the caller's memory is never returned while the device may still write it.

use crate::cmd::Cqe;
use crate::hw::{Bell, DmaMem};
use crate::queue::{CqRing, SqRing};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ksync::{IrqSpinLock, SpinLock, WaitQueue};

const IDLE: u32 = 0;
const INFLIGHT: u32 = 1;
const DONE: u32 = 2;

/// One tag's command: its state word (generation << 2 | phase), the completion the reaper stored, its waiter.
pub(crate) struct Slot {
    st: AtomicU32,
    /// dw0 | status << 32 | cid << 48.
    cqe: AtomicU64,
    pub wq: WaitQueue,
}

/// A queue's free tags: a bitmap of atomic words (bits past the count stay set).
pub(crate) struct Tags {
    words: Vec<AtomicU64>,
    hint: AtomicUsize,
    /// Submitters waiting for a tag.
    pub wq: WaitQueue,
}

impl Tags {
    fn new(n: usize) -> Tags {
        let words = (0..n.div_ceil(64))
            .map(|w| {
                let lo = w * 64;
                let used = n.saturating_sub(lo).min(64);
                AtomicU64::new(if used == 64 { 0 } else { !0u64 << used })
            })
            .collect();
        Tags { words, hint: AtomicUsize::new(0), wq: WaitQueue::new() }
    }
    pub fn get(&self) -> Option<u16> {
        let n = self.words.len();
        let start = self.hint.load(Ordering::Relaxed) % n.max(1);
        for k in 0..n {
            let w = (start + k) % n;
            let mut cur = self.words[w].load(Ordering::Relaxed);
            while cur != !0 {
                let bit = (!cur).trailing_zeros();
                match self.words[w].compare_exchange_weak(cur, cur | 1 << bit, Ordering::Acquire, Ordering::Relaxed) {
                    Ok(_) => {
                        self.hint.store(w, Ordering::Relaxed);
                        return Some((w * 64 + bit as usize) as u16);
                    }
                    Err(c) => cur = c,
                }
            }
        }
        None
    }
    pub fn put(&self, t: u16) {
        let (w, b) = (t as usize / 64, t as usize % 64);
        if let Some(word) = self.words.get(w) {
            word.fetch_and(!(1u64 << b), Ordering::Release);
            self.wq.wake_one();
        }
    }
    pub fn free(&self) -> usize {
        self.words.iter().map(|w| w.load(Ordering::Relaxed).count_zeros() as usize).sum()
    }
}

/// One I/O queue pair.
pub(crate) struct IoQueue<M: DmaMem> {
    pub qid: u16,
    pub depth: u16,
    /// The vector index whose interrupt reaps it (None: polled).
    pub vector: Option<u16>,
    /// The CPU that vector was given.
    pub cpu: Option<u32>,
    pub sq: SpinLock<SqRing<M>>,
    pub cq: IrqSpinLock<CqRing<M>>,
    /// The controller's SQ head as the last completion reported it (the tail never overtakes it).
    pub sq_head: AtomicU16,
    pub sq_db: usize,
    pub cq_db: usize,
    tag_bits: u32,
    seq_mod: u32,
    pub tags: Tags,
    slots: Vec<Slot>,
    pool: SpinLock<Vec<M>>,
    /// Free pages kept in the pool (one largest transfer, its list page and a range page); more go back to mm.
    keep: usize,
    /// The first wait of a command (the controller's I/O timeout; the ktests shorten one queue's to inject a
    /// timeout there).
    pub timeout_ns: AtomicU64,
    pub placed: AtomicU64,
    pub completed: AtomicU64,
    pub inflight: AtomicU32,
}

impl<M: DmaMem> IoQueue<M> {
    /// `depth` entries in `sq`/`cq` memory; `depth - 1` tags (the SQ can never fill, Linux `q_depth - 1`).
    pub fn new(qid: u16, depth: u16, sq: M, cq: M, vector: Option<u16>, cpu: Option<u32>, dbs: (usize, usize), keep: usize, timeout_ns: u64) -> Option<Self> {
        let sq = SqRing::new(depth, sq)?;
        let cq = CqRing::new(depth, cq)?;
        let n = depth as usize - 1;
        let tag_bits = usize::BITS - (n - 1).max(1).leading_zeros();
        if tag_bits > 14 {
            return None;
        }
        // The generation's largest value is left out so the CID never reaches FFFFh (reserved).
        let seq_mod = (1u32 << (16 - tag_bits)) - 1;
        Some(IoQueue {
            qid,
            depth,
            vector,
            cpu,
            sq: SpinLock::named("nvme.sq", sq),
            cq: IrqSpinLock::named("nvme.cq", cq),
            sq_head: AtomicU16::new(0),
            sq_db: dbs.0,
            cq_db: dbs.1,
            tag_bits,
            seq_mod,
            tags: Tags::new(n),
            slots: (0..n).map(|_| Slot { st: AtomicU32::new(IDLE), cqe: AtomicU64::new(0), wq: WaitQueue::new() }).collect(),
            pool: SpinLock::named("nvme.pool", Vec::new()),
            keep,
            timeout_ns: AtomicU64::new(timeout_ns),
            placed: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            inflight: AtomicU32::new(0),
        })
    }

    pub fn tag_count(&self) -> usize {
        self.slots.len()
    }
    pub fn slot(&self, tag: u16) -> &Slot {
        &self.slots[tag as usize]
    }

    /// The tag's next generation, in flight from now on; returns (generation, CID).
    pub fn arm(&self, tag: u16) -> (u16, u16) {
        let s = &self.slots[tag as usize];
        let seq = ((s.st.load(Ordering::Relaxed) >> 2) + 1) % self.seq_mod;
        s.st.store(seq << 2 | INFLIGHT, Ordering::Release);
        (seq as u16, (seq << self.tag_bits) as u16 | tag)
    }
    /// Back to idle without a completion (the command was never placed, or the device stopped since).
    pub fn disarm(&self, tag: u16, seq: u16) {
        self.slots[tag as usize].st.store((seq as u32) << 2 | IDLE, Ordering::Release);
    }
    pub fn done(&self, tag: u16, seq: u16) -> Option<Cqe> {
        let s = &self.slots[tag as usize];
        if s.st.load(Ordering::Acquire) != (seq as u32) << 2 | DONE {
            return None;
        }
        let v = s.cqe.load(Ordering::Relaxed);
        Some(Cqe { dw0: v as u32, sq_head: 0, sq_id: self.qid, cid: (v >> 48) as u16, status: ((v >> 32) & 0x7FFF) as u16 })
    }

    /// One pass over the completion ring: every new completion is handed to its slot and its waiter woken, then
    /// the head doorbell. `irq`: from the interrupt hook (waits for the lock; a thread only tries it, the holder
    /// reaps for it). Returns the completions taken. IRQ-safe: no heap, no klog, only `IrqSpinLock`s.
    pub fn reap(&self, bell: &dyn Bell, stale: &AtomicU64, irq: bool) -> usize {
        let mut cq = if irq {
            self.cq.lock()
        } else {
            match self.cq.try_lock() {
                Some(g) => g,
                None => return 0,
            }
        };
        let mut n = 0usize;
        // At most one ring's worth per pass: a controller flooding the ring cannot hold this CPU.
        while n < self.depth as usize {
            let c = match cq.pop() {
                Ok(Some(c)) => c,
                _ => break,
            };
            n += 1;
            if c.sq_head < self.depth {
                self.sq_head.store(c.sq_head, Ordering::Release);
            }
            let tag = (c.cid as u32 & ((1 << self.tag_bits) - 1)) as usize;
            let seq = c.cid as u32 >> self.tag_bits;
            match self.slots.get(tag) {
                Some(s) if c.sq_id == self.qid && s.st.load(Ordering::Acquire) == seq << 2 | INFLIGHT => {
                    s.cqe.store(c.dw0 as u64 | (c.status as u64) << 32 | (c.cid as u64) << 48, Ordering::Relaxed);
                    s.st.store(seq << 2 | DONE, Ordering::Release);
                    self.completed.fetch_add(1, Ordering::Relaxed);
                    s.wq.wake_all();
                }
                _ => {
                    stale.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if n > 0 {
            bell.ring(self.cq_db, cq.head() as u32);
        }
        n
    }

    /// A page for a PRP list, a bounce or a DSM range: from the pool, else new (thread context).
    pub fn take_page(&self, alloc: &dyn Fn() -> Option<M>) -> Option<M> {
        let p = self.pool.lock().pop();
        p.or_else(alloc)
    }
    /// Pages back once their command completed or the device stopped after it was placed.
    pub fn put_pages(&self, pages: &mut Vec<M>) {
        let extra = {
            let mut pool = self.pool.lock();
            while pool.len() < self.keep {
                match pages.pop() {
                    Some(p) => pool.push(p),
                    None => break,
                }
            }
            core::mem::take(pages)
        };
        drop(extra);
    }
    pub fn pool_len(&self) -> usize {
        self.pool.lock().len()
    }
}

/// The queues of one controller start. A recovery or restart builds a new set; the old one is `dead` first (its
/// waiters retry on the new set once the device stopped).
pub(crate) struct IoSet<M: DmaMem> {
    pub queues: Vec<IoQueue<M>>,
    /// CPU -> queue index (the queue whose vector is on that CPU, else CPU modulo the count).
    pub cpu_map: Vec<u16>,
    /// Vector index -> the queues it reaps.
    pub by_vector: Vec<Vec<u16>>,
    dead: AtomicBool,
}

impl<M: DmaMem> IoSet<M> {
    pub fn new(queues: Vec<IoQueue<M>>, cpus: usize) -> IoSet<M> {
        let n = queues.len().max(1);
        let mut cpu_map: Vec<u16> = (0..cpus.max(1)).map(|c| (c % n) as u16).collect();
        let mut claimed = alloc::vec![false; cpu_map.len()];
        for (i, q) in queues.iter().enumerate() {
            if let Some(c) = q.cpu.map(|c| c as usize).filter(|&c| c < cpu_map.len() && !claimed[c]) {
                cpu_map[c] = i as u16;
                claimed[c] = true;
            }
        }
        let vecs = queues.iter().filter_map(|q| q.vector).max().map_or(0, |v| v as usize + 1);
        let mut by_vector = alloc::vec![Vec::new(); vecs];
        for (i, q) in queues.iter().enumerate() {
            if let Some(v) = q.vector {
                by_vector[v as usize].push(i as u16);
            }
        }
        IoSet { queues, cpu_map, by_vector, dead: AtomicBool::new(false) }
    }
    pub fn queue_for(&self, cpu: usize) -> usize {
        match self.cpu_map.get(cpu) {
            Some(&q) => q as usize,
            None => cpu % self.queues.len().max(1),
        }
    }
    pub fn dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }
    /// No command enters this set any more: each submission lock is taken once (a submitter inside finishes its
    /// doorbell first), then every waiter is woken to see it.
    pub fn kill(&self) {
        self.dead.store(true, Ordering::Release);
        for q in &self.queues {
            drop(q.sq.lock());
        }
        for q in &self.queues {
            q.tags.wq.wake_all();
            for s in &q.slots {
                s.wq.wake_all();
            }
        }
    }
}

/// What the interrupt hook reaches: the current set, the doorbell writer, the stale counter.
pub(crate) struct HookTable<M: DmaMem, B: Bell> {
    pub set: IrqSpinLock<Option<Arc<IoSet<M>>>>,
    pub bell: B,
    pub stale: Arc<AtomicU64>,
}

impl<M: DmaMem, B: Bell> HookTable<M, B> {
    /// Vector `i` fired (hard IRQ, or the PCI core's wake from a thread with interrupts off).
    pub fn irq(&self, i: u16) {
        let g = self.set.lock();
        let Some(s) = g.as_ref() else { return };
        let Some(list) = s.by_vector.get(i as usize) else { return };
        for &qi in list {
            if let Some(q) = s.queues.get(qi as usize) {
                q.reap(&self.bell, &self.stale, true);
            }
        }
    }
    /// Swaps the set the interrupts reap (the old one is dropped here, outside the lock).
    pub fn publish(&self, s: Option<Arc<IoSet<M>>>) {
        let old = core::mem::replace(&mut *self.set.lock(), s);
        drop(old);
    }
}
