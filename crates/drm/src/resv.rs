//! DR-5: the reservation object of a buffer (Linux `dma_resv`, linux-6.6/drivers/dma-buf/dma-resv.c), as far as
//! dma-buf needs it: a set of fences, each with its usage, that poll, EXPORT_SYNC_FILE and IMPORT_SYNC_FILE read
//! and fill. DR-6 gives GEM objects the same object (a GEM buffer and its dma-buf share one, as `dmabuf->resv =
//! obj->resv`) and adds the driver's submissions to it.
//!
//! Watchers (a dma-buf's poll queue) hear about every fence of the set that signals: the set arms one callback
//! per fence when it is added and tells its watchers after the fence's state has changed (FS-17: state first,
//! then the wake; the watcher computes what is ready now).

use crate::fence::{self, Fence, FenceCb, Registration};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::SpinLock;
use linux::errno::ENOMEM;
use vfs::Errno;

/// `enum dma_resv_usage` (include/linux/dma-resv.h:55-102): ordered, a query for one usage sees every fence of
/// that usage and the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Usage {
    /// Memory management (moves, clears): everyone waits.
    Kernel,
    /// Implicit-sync writers.
    Write,
    /// Implicit-sync readers.
    Read,
    /// Not for implicit sync at all (page-table updates, preemption fences).
    Bookkeep,
}

impl Usage {
    /// `dma_resv_usage_rw`: what an access must wait for. A writer waits for everything implicit sync knows
    /// (`Read` and below), a reader only for the writers.
    pub fn rw(write: bool) -> Usage {
        if write { Usage::Read } else { Usage::Write }
    }
}

/// Told when a fence of the set may have signaled (called without any lock of the set held).
pub trait ResvWatch: Send + Sync {
    fn changed(&self);
}

struct Slot {
    fence: Arc<Fence>,
    usage: Usage,
    /// DR-8: who added it (`amdgpu_sync_get_owner`: the client VM of a CS job, a DRM file's id); 0 = nobody,
    /// which every implicit-sync query sees (`AMDGPU_FENCE_OWNER_UNDEFINED`).
    owner: u64,
    /// The callback that tells the watchers; None once the fence had signaled when it was armed.
    _reg: Option<Registration>,
}

struct Inner {
    slots: Vec<Slot>,
    watchers: Vec<Weak<dyn ResvWatch>>,
}

pub struct Resv {
    me: Weak<Resv>,
    inner: SpinLock<Inner>,
}

/// The fence callback of one slot: tells the set's watchers.
struct Signaled(Weak<Resv>);

impl FenceCb for Signaled {
    fn call(&self) {
        if let Some(r) = self.0.upgrade() {
            r.notify();
        }
    }
}

klog::counter!(RESV_FENCES, "drm.resv.fences_added");

impl Resv {
    pub fn new() -> Arc<Resv> {
        Arc::new_cyclic(|me| Resv { me: me.clone(), inner: SpinLock::named("drm.resv", Inner { slots: Vec::new(), watchers: Vec::new() }) })
    }

    /// Adds a watcher (kept weakly: a dropped watcher is forgotten on the next change).
    pub fn watch(&self, w: Weak<dyn ResvWatch>) -> Result<(), Errno> {
        let mut g = self.inner.lock();
        g.watchers.retain(|x| x.strong_count() > 0);
        g.watchers.try_reserve(1).map_err(|_| ENOMEM)?;
        g.watchers.push(w);
        Ok(())
    }

    /// `dma_resv_reserve_fences` + `dma_resv_add_fence` for the leaves of `f` (a container is unwrapped, as
    /// Linux's callers do): a fence replaces an older one of the same context whose usage is the same or weaker
    /// (later in the order: a writer replaces a reader, not the other way), and signaled fences leave the set. ENOMEM when the set cannot grow (nothing was added then).
    pub fn add_fence(&self, f: &Arc<Fence>, usage: Usage) -> Result<(), Errno> {
        self.add_fence_owned(f, usage, 0)
    }

    /// `add_fence` with the fence's owner (DR-8: a CS job's file), for [`Resv::fences_not_owned`].
    pub fn add_fence_owned(&self, f: &Arc<Fence>, usage: Usage, owner: u64) -> Result<(), Errno> {
        let leaves = fence::unwrap(f);
        let mut old: Vec<Slot> = Vec::new();
        {
            let mut g = self.inner.lock();
            g.slots.try_reserve(leaves.len()).map_err(|_| ENOMEM)?;
            old.try_reserve(g.slots.len() + leaves.len()).map_err(|_| ENOMEM)?;
            let mut k = 0;
            while k < g.slots.len() {
                let s = &g.slots[k];
                let same = leaves.iter().any(|l| l.context() == s.fence.context() && s.usage >= usage && later_or_same(l, &s.fence));
                if same || fence::is_signaled(&s.fence) {
                    old.push(g.slots.swap_remove(k));
                } else {
                    k += 1;
                }
            }
            for l in &leaves {
                g.slots.push(Slot { fence: l.clone(), usage, owner, _reg: None });
            }
        }
        // Registrations are dropped (they take the fence's lock) and armed outside the set's lock: a fence
        // callback takes the set's lock, so it is never held across a fence lock.
        drop(old);
        let mut signaled = false;
        for l in &leaves {
            RESV_FENCES.inc();
            let reg = fence::add_callback(l, Arc::new(Signaled(self.me.clone())));
            if reg.is_none() {
                // Signaled after the slot went in (a poller may already sleep on the old answer): no callback
                // will come, so the watchers are told here (FS-17: state first, then the wake).
                signaled = true;
            }
            if let Some(reg) = reg {
                let mut g = self.inner.lock();
                match g.slots.iter_mut().find(|s| Arc::ptr_eq(&s.fence, l) && s._reg.is_none()) {
                    Some(s) => s._reg = Some(reg),
                    // Replaced meanwhile: the registration goes after the lock.
                    None => {
                        drop(g);
                        drop(reg);
                    }
                }
            }
        }
        if signaled {
            self.notify();
        }
        Ok(())
    }

    /// The unsignaled fences a query for `usage` sees (`dma_resv_for_each_fence`).
    pub fn fences(&self, usage: Usage) -> Vec<Arc<Fence>> {
        let g = self.inner.lock();
        let mut v = Vec::new();
        if v.try_reserve(g.slots.len()).is_err() {
            // No memory for the copy: one fence that is not ready makes the answer "wait", which is safe.
            if let Some(s) = g.slots.iter().find(|s| s.usage <= usage && !fence::is_signaled(&s.fence)) {
                v.push(s.fence.clone());
            }
            return v;
        }
        v.extend(g.slots.iter().filter(|s| s.usage <= usage && !fence::is_signaled(&s.fence)).map(|s| s.fence.clone()));
        v
    }

    /// What a CS of `owner` waits for (`amdgpu_sync_resv` with AMDGPU_SYNC_NE_OWNER, amdgpu_sync.c): the fences
    /// `usage` sees but those of the same owner; kernel fences (moves, clears) always.
    pub fn fences_not_owned(&self, usage: Usage, owner: u64) -> Vec<Arc<Fence>> {
        let g = self.inner.lock();
        let wanted = |s: &&Slot| s.usage <= usage && (s.owner == 0 || s.owner != owner || s.usage == Usage::Kernel) && !fence::is_signaled(&s.fence);
        let mut v = Vec::new();
        if v.try_reserve(g.slots.len()).is_err() {
            // As `fences`: one fence that is not ready makes the answer "wait".
            v.extend(g.slots.iter().find(wanted).map(|s| s.fence.clone()));
            return v;
        }
        v.extend(g.slots.iter().filter(wanted).map(|s| s.fence.clone()));
        v
    }

    /// `dma_resv_test_signaled`: every fence `usage` sees has signaled.
    pub fn test_signaled(&self, usage: Usage) -> bool {
        self.inner.lock().slots.iter().all(|s| s.usage > usage || fence::is_signaled(&s.fence))
    }

    /// `dma_resv_get_singleton`: None without a fence, the fence itself when there is one, else their merge.
    pub fn singleton(&self, usage: Usage) -> Option<Arc<Fence>> {
        let fs = self.fences(usage);
        match fs.len() {
            0 => None,
            1 => fs.into_iter().next(),
            _ => {
                let refs: Vec<&Arc<Fence>> = fs.iter().collect();
                Some(fence::merge(&refs))
            }
        }
    }

    /// Fences in the set now, signaled ones included (tests, fdinfo).
    pub fn len(&self) -> usize {
        self.inner.lock().slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn notify(&self) {
        let ws: Vec<Arc<dyn ResvWatch>> = {
            let g = self.inner.lock();
            let mut v = Vec::new();
            if v.try_reserve(g.watchers.len()).is_err() {
                klog::event!(Warn, "drm", "resv.notify_dropped", watchers = g.watchers.len(); "no memory to wake the buffer's pollers");
                return;
            }
            v.extend(g.watchers.iter().filter_map(|w| w.upgrade()));
            v
        };
        for w in ws {
            w.changed();
        }
    }
}

/// `dma_fence_is_later_or_same` for two fences of one context.
fn later_or_same(new: &Arc<Fence>, old: &Arc<Fence>) -> bool {
    new.seqno() >= old.seqno() || Arc::ptr_eq(new, old)
}
