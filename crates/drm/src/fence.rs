//! Fences (Linux `dma_fence`, linux-6.6/drivers/dma-buf/dma-fence.c, dma-fence-chain.c, dma-fence-array.c,
//! dma-fence-unwrap.c): a fence a driver signals (`Fence::new`, `signal`), the always-signaled stub, timeline
//! chains (one node per point, `chain`) and arrays (merged sync_files). Waiters hang a callback on every
//! unsignaled leaf behind a fence (`add_callback`); a callback only says "look again", the waiter re-checks.
//!
//! Signal in thread context: callbacks wake wait queues and write eventfds right in `signal`.
//!
//! Every fence a driver makes must signal in the end (the dma-fence rule), also when its ring or device goes:
//! a waiter's callback holds the waiter, and an armed eventfd entry holds its fence, until that signal.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use ksync::SpinLock;

/// "Something may have changed": the waiter looks at its fence again (several calls are fine).
pub trait FenceCb: Send + Sync {
    fn call(&self);
}

/// `dma_fence_context_counter`: contexts from 1 (0 is the stub's).
static CONTEXTS: AtomicU64 = AtomicU64::new(1);
static CB_IDS: AtomicU64 = AtomicU64::new(1);

/// `dma_fence_context_alloc(n)`: the first of `n` new contexts.
pub fn context_alloc(n: u64) -> u64 {
    CONTEXTS.fetch_add(n, Ordering::Relaxed)
}

/// CLOCK_MONOTONIC in ns (V2: the scheduler's clock); None before it runs. Host tests: the process clock.
pub fn now() -> Option<u64> {
    #[cfg(test)]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        Some(START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64 + 1_000_000_000)
    }
    #[cfg(not(test))]
    {
        ksync::now_ns()
    }
}

/// Callbacks with ids (a plain fence's, a device's unplug list).
pub struct CbList {
    list: SpinLock<Vec<(u64, Arc<dyn FenceCb>)>>,
}

impl Default for CbList {
    fn default() -> Self {
        Self::new()
    }
}

impl CbList {
    pub const fn new() -> CbList {
        CbList { list: SpinLock::named("drm.fence.cbs", Vec::new()) }
    }
    pub fn add(&self, cb: Arc<dyn FenceCb>) -> u64 {
        let id = CB_IDS.fetch_add(1, Ordering::Relaxed);
        self.list.lock().push((id, cb));
        id
    }
    pub fn remove(&self, id: u64) {
        let gone: Vec<_> = {
            let mut l = self.list.lock();
            match l.iter().position(|(i, _)| *i == id) {
                Some(k) => alloc::vec![l.swap_remove(k)],
                None => Vec::new(),
            }
        };
        drop(gone);
    }
    pub fn len(&self) -> usize {
        self.list.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Calls every callback outside the lock (the list stays).
    pub fn call_all(&self) {
        let all: Vec<Arc<dyn FenceCb>> = self.list.lock().iter().map(|(_, c)| c.clone()).collect();
        for c in all {
            c.call();
        }
    }
}

enum Kind {
    /// Signaled by its owner (`signal`).
    Plain { cbs: SpinLock<Vec<(u64, Arc<dyn FenceCb>)>> },
    /// `dma_fence_chain`: `fence` at this point, `prev` the earlier points (dropped from the chain once signaled).
    Chain { prev: SpinLock<Option<Arc<Fence>>>, fence: Arc<Fence>, prev_seqno: u64 },
    /// `dma_fence_array` (signal_on_any = false).
    Array { fences: Vec<Arc<Fence>> },
}

pub struct Fence {
    context: u64,
    seqno: u64,
    driver: &'static str,
    timeline: &'static str,
    kind: Kind,
    /// Plain: the truth (set under `cbs`). Chain/array: set once seen signaled (`DMA_FENCE_FLAG_SIGNALED_BIT`).
    signaled: AtomicBool,
    timestamp: AtomicU64,
    error: AtomicI32,
}

impl Fence {
    /// A driver's fence: unsignaled until `signal`.
    pub fn new(context: u64, seqno: u64, driver: &'static str, timeline: &'static str) -> Arc<Fence> {
        Arc::new(Fence {
            context,
            seqno,
            driver,
            timeline,
            kind: Kind::Plain { cbs: SpinLock::named("drm.fence.cbs", Vec::new()) },
            signaled: AtomicBool::new(false),
            timestamp: AtomicU64::new(0),
            error: AtomicI32::new(0),
        })
    }

    /// `dma_fence_allocate_private_stub(timestamp)`: a new signaled fence, context 0.
    pub fn private_stub(timestamp: u64) -> Arc<Fence> {
        let f = Fence::new(0, 0, "stub", "stub");
        f.signal_at(0, timestamp);
        f
    }

    pub fn context(&self) -> u64 {
        self.context
    }
    pub fn seqno(&self) -> u64 {
        self.seqno
    }
    pub fn is_chain(&self) -> bool {
        matches!(self.kind, Kind::Chain { .. })
    }
    /// A chain node's `prev_seqno` (0 for other fences).
    pub fn chain_prev_seqno(&self) -> u64 {
        match &self.kind {
            Kind::Chain { prev_seqno, .. } => *prev_seqno,
            _ => 0,
        }
    }
    /// `dma_fence_driver_name`: a signaled fence no longer names its driver (Linux 6.17+).
    pub fn driver_name(self: &Arc<Self>) -> &'static str {
        if is_signaled(self) { "detached-driver" } else { self.driver }
    }
    /// `dma_fence_timeline_name`.
    pub fn timeline_name(self: &Arc<Self>) -> &'static str {
        if is_signaled(self) { "signaled-timeline" } else { self.timeline }
    }
    /// When it was signaled (ns, CLOCK_MONOTONIC); 0 while it is not.
    pub fn timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Acquire)
    }

    /// `dma_fence_signal`: false if it was signaled already. Callbacks run here, outside the lock.
    pub fn signal(&self) -> bool {
        self.signal_at(0, now().unwrap_or(0))
    }

    /// `dma_fence_set_error` + `dma_fence_signal`: `errno` > 0 is reported as -errno.
    pub fn signal_error(&self, errno: i32) -> bool {
        self.signal_at(-errno.abs(), now().unwrap_or(0))
    }

    fn signal_at(&self, error: i32, ts: u64) -> bool {
        let Kind::Plain { cbs } = &self.kind else { return false };
        let taken = {
            let mut l = cbs.lock();
            if self.signaled.load(Ordering::Relaxed) {
                return false;
            }
            self.error.store(error, Ordering::Relaxed);
            self.timestamp.store(ts, Ordering::Relaxed);
            self.signaled.store(true, Ordering::Release);
            core::mem::take(&mut *l)
        };
        for (_, cb) in taken {
            cb.call();
        }
        true
    }

    fn add_cb(&self, cb: Arc<dyn FenceCb>) -> Option<u64> {
        let Kind::Plain { cbs } = &self.kind else { return None };
        let mut l = cbs.lock();
        if self.signaled.load(Ordering::Relaxed) {
            return None;
        }
        let id = CB_IDS.fetch_add(1, Ordering::Relaxed);
        l.push((id, cb));
        Some(id)
    }

    fn remove_cb(&self, id: u64) {
        let Kind::Plain { cbs } = &self.kind else { return };
        let gone = {
            let mut l = cbs.lock();
            l.iter().position(|(i, _)| *i == id).map(|k| l.swap_remove(k))
        };
        drop(gone);
    }

    /// Callbacks waiting on this plain fence (tests: none left behind).
    pub fn callbacks(&self) -> usize {
        match &self.kind {
            Kind::Plain { cbs } => cbs.lock().len(),
            _ => 0,
        }
    }
}

impl Drop for Fence {
    /// `dma_fence_chain_release`: a long chain goes link by link, not by recursion.
    fn drop(&mut self) {
        let Kind::Chain { prev, .. } = &mut self.kind else { return };
        let mut next = prev.get_mut().take();
        while let Some(p) = next {
            next = match Arc::into_inner(p) {
                Some(mut f) => match &mut f.kind {
                    Kind::Chain { prev, .. } => prev.get_mut().take(),
                    _ => None,
                },
                None => None,
            };
        }
    }
}

/// `dma_fence_get_stub`: the one shared signaled fence (context 0, seqno 0).
pub fn stub() -> Arc<Fence> {
    static STUB: ksync::Once<Arc<Fence>> = ksync::Once::new();
    STUB.get_or_init(|| Fence::private_stub(now().unwrap_or(0))).clone()
}

/// `dma_fence_chain_contained`.
fn contained(f: &Arc<Fence>) -> &Arc<Fence> {
    match &f.kind {
        Kind::Chain { fence, .. } => fence,
        _ => f,
    }
}

/// `dma_fence_is_signaled`.
pub fn is_signaled(f: &Arc<Fence>) -> bool {
    if f.signaled.load(Ordering::Acquire) {
        return true;
    }
    let done = match &f.kind {
        Kind::Plain { .. } => false,
        Kind::Array { fences } => fences.iter().all(is_signaled),
        // `dma_fence_chain_signaled`: this point's fence and every earlier one still in the chain.
        Kind::Chain { .. } => {
            let mut it = Some(f.clone());
            let mut all = true;
            while let Some(c) = it {
                if !is_signaled(contained(&c)) {
                    all = false;
                    break;
                }
                it = walk(&c);
            }
            all
        }
    };
    if done {
        let ts = leaf_timestamp(f);
        let _ = f.timestamp.compare_exchange(0, ts, Ordering::AcqRel, Ordering::Relaxed);
        f.signaled.store(true, Ordering::Release);
    }
    done
}

fn leaf_timestamp(f: &Arc<Fence>) -> u64 {
    match &f.kind {
        Kind::Plain { .. } => f.timestamp(),
        Kind::Array { fences } => fences.iter().map(|x| x.timestamp()).max().unwrap_or(0),
        Kind::Chain { fence, .. } => fence.timestamp().max(now().unwrap_or(0)),
    }
}

/// `dma_fence_get_status`: 0 pending, 1 signaled, -errno signaled with an error.
pub fn status(f: &Arc<Fence>) -> i32 {
    if !is_signaled(f) {
        return 0;
    }
    let e = match &f.kind {
        Kind::Plain { .. } => f.error.load(Ordering::Relaxed),
        Kind::Array { fences } => fences.iter().map(|x| x.error.load(Ordering::Relaxed)).find(|e| *e != 0).unwrap_or(0),
        Kind::Chain { .. } => 0,
    };
    if e < 0 { e } else { 1 }
}

/// `dma_fence_chain_walk`: the next earlier point whose fence is not signaled yet; signaled ones leave the chain.
pub fn walk(f: &Arc<Fence>) -> Option<Arc<Fence>> {
    let Kind::Chain { prev: slot, .. } = &f.kind else { return None };
    loop {
        let prev = slot.lock().clone()?;
        let replacement = match &prev.kind {
            Kind::Chain { prev: pp, fence, .. } => {
                if !is_signaled(fence) {
                    return Some(prev);
                }
                pp.lock().clone()
            }
            _ => {
                if !is_signaled(&prev) {
                    return Some(prev);
                }
                None
            }
        };
        let old = {
            let mut g = slot.lock();
            if g.as_ref().is_some_and(|p| Arc::ptr_eq(p, &prev)) { core::mem::replace(&mut *g, replacement) } else { None }
        };
        // The unlinked node may be the last owner of a long chain: dropped here, outside the lock.
        drop(old);
    }
}

/// `dma_fence_chain_init` + alloc: the node for `point` on top of `prev` (the syncobj's fence before). An
/// out-of-order point gets a context of its own and the seqno of `prev`, as Linux.
pub fn chain(prev: Option<Arc<Fence>>, fence: Arc<Fence>, point: u64) -> Arc<Fence> {
    let (context, seqno, prev_seqno) = match &prev {
        Some(p) if p.is_chain() && point > p.seqno => (p.context, point, p.seqno),
        Some(p) if p.is_chain() => (context_alloc(1), p.seqno.max(point), 0),
        _ => (context_alloc(1), point, 0),
    };
    Arc::new(Fence {
        context,
        seqno,
        driver: "dma_fence_chain",
        timeline: "unbound",
        kind: Kind::Chain { prev: SpinLock::named("drm.fence.chain", prev), fence, prev_seqno },
        signaled: AtomicBool::new(false),
        timestamp: AtomicU64::new(0),
        error: AtomicI32::new(0),
    })
}

/// `dma_fence_array_create(.., context_alloc(1), 1, false)`.
fn array(fences: Vec<Arc<Fence>>) -> Arc<Fence> {
    Arc::new(Fence {
        context: context_alloc(1),
        seqno: 1,
        driver: "dma_fence_array",
        timeline: "unbound",
        kind: Kind::Array { fences },
        signaled: AtomicBool::new(false),
        timestamp: AtomicU64::new(0),
        error: AtomicI32::new(0),
    })
}

/// `dma_fence_chain_find_seqno`: the node that signals `seqno`. Ok(None): the point is signaled and gone from
/// the chain (callers use the stub). Err: a point not submitted yet (or a point on a fence that is no chain).
pub fn find_seqno(f: Option<Arc<Fence>>, seqno: u64) -> Result<Option<Arc<Fence>>, ()> {
    if seqno == 0 {
        return Ok(f);
    }
    let head = match f {
        Some(h) if h.is_chain() && h.seqno >= seqno => h,
        _ => return Err(()),
    };
    let mut it = Some(head.clone());
    while let Some(c) = it {
        let stop = match &c.kind {
            Kind::Chain { prev_seqno, .. } => c.context != head.context || *prev_seqno < seqno,
            _ => true,
        };
        if stop {
            return Ok(Some(c));
        }
        it = walk(&c);
    }
    Ok(None)
}

/// `dma_fence_unwrap_for_each`: the fences of every node still in a chain, an array's members, else the fence.
pub fn unwrap(f: &Arc<Fence>) -> Vec<Arc<Fence>> {
    let mut out = Vec::new();
    let mut it = Some(f.clone());
    while let Some(c) = it {
        match &contained(&c).kind {
            Kind::Array { fences } => out.extend(fences.iter().cloned()),
            _ => out.push(contained(&c).clone()),
        }
        it = walk(&c);
    }
    out
}

/// `dma_fence_unwrap_merge`: the pending fences of all inputs, one per context (the later one), sorted by
/// context; none pending: a private stub with the newest timestamp; one: that fence; else an array.
pub fn merge(inputs: &[&Arc<Fence>]) -> Arc<Fence> {
    let mut pending: Vec<Arc<Fence>> = Vec::new();
    let mut ts = 0;
    for f in inputs {
        for x in unwrap(f) {
            if is_signaled(&x) {
                ts = ts.max(x.timestamp());
            } else {
                pending.push(x);
            }
        }
    }
    if pending.is_empty() {
        return Fence::private_stub(ts);
    }
    pending.sort_by(|a, b| a.context.cmp(&b.context).then(b.seqno.cmp(&a.seqno)));
    pending.dedup_by(|later, first| later.context == first.context);
    if pending.len() == 1 {
        return pending.remove(0);
    }
    array(pending)
}

/// Every unsignaled plain fence behind `f` (where callbacks go).
fn leaves(f: &Arc<Fence>, out: &mut Vec<Arc<Fence>>) {
    if is_signaled(f) {
        return;
    }
    match &f.kind {
        Kind::Plain { .. } => out.push(f.clone()),
        Kind::Array { fences } => fences.iter().for_each(|x| leaves(x, out)),
        Kind::Chain { .. } => {
            let mut it = Some(f.clone());
            while let Some(c) = it {
                leaves(contained(&c), out);
                it = walk(&c);
            }
        }
    }
}

/// Callbacks on a fence's leaves; dropping it takes them off again.
pub struct Registration {
    regs: Vec<(Arc<Fence>, u64)>,
}

impl Registration {
    /// Leaves the callbacks in place (they go when their fences signal): an eventfd entry that outlives its
    /// syncobj, as Linux's `syncobj_eventfd_entry`.
    pub fn detach(mut self) {
        self.regs.clear();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        for (f, id) in self.regs.drain(..) {
            f.remove_cb(id);
        }
    }
}

/// `dma_fence_add_callback`: None when `f` is signaled already (the callback is not kept), else the
/// registration. `cb` may be called once per leaf; it re-checks `is_signaled(f)` itself.
pub fn add_callback(f: &Arc<Fence>, cb: Arc<dyn FenceCb>) -> Option<Registration> {
    if is_signaled(f) {
        return None;
    }
    let mut ls = Vec::new();
    leaves(f, &mut ls);
    let mut reg = Registration { regs: Vec::with_capacity(ls.len()) };
    for l in ls {
        if let Some(id) = l.add_cb(cb.clone()) {
            reg.regs.push((l, id));
        }
    }
    // A leaf that signaled between the look and the registration: the fence may be complete now.
    if is_signaled(f) {
        return None;
    }
    Some(reg)
}

/// A thread waiting for callbacks (`wake_up_process` of Linux's waits).
pub struct Waiter {
    wq: ksync::WaitQueue,
    fired: AtomicBool,
}

impl Default for Waiter {
    fn default() -> Self {
        Waiter { wq: ksync::WaitQueue::new(), fired: AtomicBool::new(false) }
    }
}

impl Waiter {
    pub fn new() -> Arc<Waiter> {
        Arc::new(Waiter::default())
    }
    /// Sleeps until a callback came or `ns` passed.
    pub fn sleep(&self, ns: u64) {
        self.wq.wait_until_timeout(|| self.fired.swap(false, Ordering::SeqCst), ns);
    }
}

impl FenceCb for Waiter {
    fn call(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.wq.wake_all();
    }
}
