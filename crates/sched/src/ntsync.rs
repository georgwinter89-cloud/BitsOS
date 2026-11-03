//! SC-8: the objects of Linux's ntsync driver (Linux 6.14 `drivers/misc/ntsync.c`, Wine's NT synchronization
//! primitives) without their files: semaphores, mutexes with an owner and an abandoned state, auto/manual
//! events, and the waits for any or all of up to 64 of them plus an alert event. The fds, the ioctl numbers and
//! user memory are the syscall layer's (`syscall::ntsync`).
//!
//! - One **device** per open of `/dev/ntsync` (Linux `ntsync_char_open`); its objects live in the device's map
//!   under **one spin lock** (`sched.ntsync.device`). Linux locks each object and takes a device-wide mutex
//!   for wait-all; one lock gives the same all-or-nothing wait-all (every object checked and taken in one
//!   critical section) and needs no new lock order. The section is short: no user memory, no sleeping, no
//!   wake-up under it (the woken waiters are collected and woken after).
//! - A **waiter** is queued on every object it waits for (`any` list: wait-any and the alert event; `all`
//!   list: wait-all) in arrival order. Whoever changes an object tries the wait-all waiters on it first, then
//!   the wait-any waiters in order (Linux `try_wake_all_obj`, `try_wake_any_*`); a waiter is taken by the first
//!   object that signals it (`signaled`: the index, -1 while waiting) and the object's state is consumed then.
//! - The waiting thread sleeps in slices on its own wait queue and checks the end of its wait (signal, kill,
//!   deadline) between them; a signal ends a wait only if nothing signaled it (Linux: "even if we caught a
//!   signal, we need to communicate success").

use crate::futex::{Deadline, expired, mono_deadline};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;

/// Linux's EOWNERDEAD (SC-8 CR2: now in `uapi::errno`).
pub use uapi::errno::EOWNERDEAD;
/// `NTSYNC_MAX_WAIT_COUNT`: objects per wait (the ABI's own bound, not a V2 limit).
pub const MAX_WAIT_COUNT: usize = 64;
/// A sleeping wait looks at its end conditions at least this often (a missed kick costs at most this).
const SLICE_NS: u64 = 50_000_000;

klog::counter!(WAITS, "ntsync.waits");
klog::counter!(SLEEPS, "ntsync.sleeps");
klog::counter!(TIMEOUTS, "ntsync.timeouts");
klog::counter!(INTERRUPTS, "ntsync.interrupts");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Sem,
    Mutex,
    Event,
}

/// An object's state (`ntsync_obj.u`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Sem { count: u32, max: u32 },
    /// `dead`: the owner was killed holding it (MUTEX_KILL); the next one to take it learns EOWNERDEAD.
    Mutex { count: u32, owner: u32, dead: bool },
    Event { manual: bool, signaled: bool },
}

impl State {
    pub fn kind(&self) -> Kind {
        match self {
            State::Sem { .. } => Kind::Sem,
            State::Mutex { .. } => Kind::Mutex,
            State::Event { .. } => Kind::Event,
        }
    }
    /// `is_signaled`: whether a waiter of `owner` could take it now.
    fn signaled(&self, owner: u32) -> bool {
        match *self {
            State::Sem { count, .. } => count != 0,
            State::Mutex { count, owner: o, .. } => (o == 0 || o == owner) && count < u32::MAX,
            State::Event { signaled, .. } => signaled,
        }
    }
    /// Takes it for `owner` (only when `signaled`); true when an abandoned mutex was taken.
    fn take(&mut self, owner: u32) -> bool {
        match self {
            State::Sem { count, .. } => {
                *count -= 1;
                false
            }
            State::Mutex { count, owner: o, dead } => {
                let was = *dead;
                *dead = false;
                *count += 1;
                *o = owner;
                was
            }
            State::Event { manual, signaled } => {
                if !*manual {
                    *signaled = false;
                }
                false
            }
        }
    }
    /// The first waiters of an object that is not signaled for anyone are not looked at (Linux breaks there).
    fn exhausted(&self) -> bool {
        match *self {
            State::Sem { count, .. } => count == 0,
            State::Mutex { count, .. } => count == u32::MAX,
            State::Event { signaled, .. } => !signaled,
        }
    }
}

struct Waiter {
    /// The index of the object that ended the wait, -1 while waiting.
    signaled: AtomicI32,
    /// A taken mutex was abandoned (stored before `signaled`).
    dead: AtomicBool,
    owner: u32,
    /// Wait-all: the objects (without the alert event) that must all be taken at once.
    all: Vec<u64>,
    wq: WaitQueue,
}

impl Waiter {
    fn done(&self) -> bool {
        self.signaled.load(Ordering::Acquire) != -1
    }
    fn finish(&self, index: u32, dead: bool) {
        if dead {
            self.dead.store(true, Ordering::Relaxed);
        }
        self.signaled.store(index as i32, Ordering::Release);
    }
}

struct Slot {
    st: State,
    any: Vec<(Arc<Waiter>, u32)>,
    all: Vec<Arc<Waiter>>,
}

#[derive(Default)]
struct DevState {
    objs: BTreeMap<u64, Slot>,
}

type Woken = Vec<Arc<Waiter>>;

impl DevState {
    /// `try_wake_all`: takes every object of `w` at once if all are signaled for it.
    fn try_all(&mut self, w: &Arc<Waiter>, woken: &mut Woken) {
        if w.done() || !w.all.iter().all(|id| self.objs.get(id).is_some_and(|s| s.st.signaled(w.owner))) {
            return;
        }
        let mut dead = false;
        for id in &w.all {
            if let Some(s) = self.objs.get_mut(id) {
                dead |= s.st.take(w.owner);
            }
        }
        w.finish(0, dead);
        woken.push(w.clone());
    }
    /// `try_wake_any_obj`: hands object `id` to its wait-any waiters in order while it lasts.
    fn try_any(&mut self, id: u64, woken: &mut Woken) {
        let Some(s) = self.objs.get_mut(&id) else { return };
        for (w, index) in &s.any {
            if s.st.exhausted() {
                break;
            }
            if w.done() || !s.st.signaled(w.owner) {
                continue;
            }
            let dead = s.st.take(w.owner);
            w.finish(*index, dead);
            woken.push(w.clone());
        }
    }
    /// After object `id` changed: its wait-all waiters first, then its wait-any waiters.
    fn changed(&mut self, id: u64, woken: &mut Woken) {
        let all: Vec<Arc<Waiter>> = self.objs.get(&id).map(|s| s.all.clone()).unwrap_or_default();
        for w in &all {
            self.try_all(w, woken);
        }
        self.try_any(id, woken);
    }
}

fn wake(woken: Woken) {
    for w in woken {
        w.wq.wake_all();
    }
}

/// One open of `/dev/ntsync`: the namespace its objects and waits belong to.
pub struct Device {
    st: SpinLock<DevState>,
    next: AtomicU64,
    /// Waits under way on this device (ktests: a helper's wait is queued before they signal it).
    queued: AtomicUsize,
}

impl Device {
    pub fn new() -> Arc<Device> {
        Arc::new(Device { st: SpinLock::named("sched.ntsync.device", DevState::default()), next: AtomicU64::new(1), queued: AtomicUsize::new(0) })
    }

    /// `ntsync_create_sem/mutex/event` after their argument checks: semaphore count above max and a mutex
    /// with an owner but no count (or a count but no owner) are EINVAL.
    pub fn create(self: &Arc<Self>, st: State) -> Result<Arc<Obj>, i64> {
        let ok = match st {
            State::Sem { count, max } => count <= max,
            State::Mutex { count, owner, .. } => (owner == 0) == (count == 0),
            State::Event { .. } => true,
        };
        if !ok {
            return Err(EINVAL);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.st.lock().objs.insert(id, Slot { st, any: Vec::new(), all: Vec::new() });
        Ok(Arc::new(Obj { dev: self.clone(), id, kind: st.kind() }))
    }

    /// The objects this device holds (ktests: every closed object is gone).
    pub fn objects(&self) -> usize {
        self.st.lock().objs.len()
    }

    /// The waits under way on this device now (counted per device, so another process's waits never count).
    pub fn waiters(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

/// One object (the thing behind its fd). Dropping the last reference removes it from its device; a wait holds
/// references to its objects, so nothing is removed while waited for.
pub struct Obj {
    dev: Arc<Device>,
    id: u64,
    kind: Kind,
}

impl Drop for Obj {
    fn drop(&mut self) {
        self.dev.st.lock().objs.remove(&self.id);
    }
}

impl Obj {
    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Changes the state under the device lock and wakes whoever it now satisfies.
    fn change<R>(&self, f: impl FnOnce(&mut State) -> Result<R, i64>) -> Result<R, i64> {
        let mut woken = Vec::new();
        let r = {
            let mut st = self.dev.st.lock();
            let s = st.objs.get_mut(&self.id).ok_or(EINVAL)?;
            let r = f(&mut s.st)?;
            st.changed(self.id, &mut woken);
            r
        };
        wake(woken);
        Ok(r)
    }

    /// The state now (SEM_READ, MUTEX_READ, EVENT_READ).
    pub fn read(&self) -> State {
        let st = self.dev.st.lock();
        st.objs.get(&self.id).map(|s| s.st).unwrap_or(State::Event { manual: false, signaled: false })
    }

    fn want(&self, k: Kind) -> Result<(), i64> {
        if self.kind == k { Ok(()) } else { Err(EINVAL) }
    }

    /// SEM_RELEASE: adds `n`; the previous count. EOVERFLOW past max (nothing changes).
    pub fn sem_release(&self, n: u32) -> Result<u32, i64> {
        self.want(Kind::Sem)?;
        self.change(|s| match s {
            State::Sem { count, max } => {
                let prev = *count;
                match prev.checked_add(n) {
                    Some(sum) if sum <= *max => {
                        *count = sum;
                        Ok(prev)
                    }
                    _ => Err(EOVERFLOW),
                }
            }
            _ => Err(EINVAL),
        })
    }

    /// MUTEX_UNLOCK by `owner` (not 0: EINVAL): the previous count; EPERM when `owner` does not hold it.
    pub fn mutex_unlock(&self, owner: u32) -> Result<u32, i64> {
        if owner == 0 {
            return Err(EINVAL);
        }
        self.want(Kind::Mutex)?;
        self.change(|s| match s {
            State::Mutex { count, owner: o, .. } => {
                if *o != owner {
                    return Err(EPERM);
                }
                let prev = *count;
                *count -= 1;
                if *count == 0 {
                    *o = 0;
                }
                Ok(prev)
            }
            _ => Err(EINVAL),
        })
    }

    /// MUTEX_KILL: `owner` (not 0) ended holding it: free and abandoned; EPERM when `owner` does not hold it.
    pub fn mutex_kill(&self, owner: u32) -> Result<(), i64> {
        if owner == 0 {
            return Err(EINVAL);
        }
        self.want(Kind::Mutex)?;
        self.change(|s| match s {
            State::Mutex { count, owner: o, dead } => {
                if *o != owner {
                    return Err(EPERM);
                }
                *dead = true;
                *o = 0;
                *count = 0;
                Ok(())
            }
            _ => Err(EINVAL),
        })?;
        klog::event!(Debug, "ntsync", "mutex.abandoned", owner = owner);
        Ok(())
    }

    /// EVENT_SET (`pulse`: EVENT_PULSE, signaled only for the waiters it wakes now): the previous state.
    pub fn event_set(&self, pulse: bool) -> Result<bool, i64> {
        self.want(Kind::Event)?;
        let mut woken = Vec::new();
        let prev = {
            let mut st = self.dev.st.lock();
            let s = st.objs.get_mut(&self.id).ok_or(EINVAL)?;
            let State::Event { signaled, .. } = &mut s.st else { return Err(EINVAL) };
            let prev = core::mem::replace(signaled, true);
            st.changed(self.id, &mut woken);
            if pulse && let Some(Slot { st: State::Event { signaled, .. }, .. }) = st.objs.get_mut(&self.id) {
                *signaled = false;
            }
            prev
        };
        wake(woken);
        Ok(prev)
    }

    /// EVENT_RESET: the previous state.
    pub fn event_reset(&self) -> Result<bool, i64> {
        self.want(Kind::Event)?;
        let mut st = self.dev.st.lock();
        match st.objs.get_mut(&self.id) {
            Some(Slot { st: State::Event { signaled, .. }, .. }) => Ok(core::mem::replace(signaled, false)),
            _ => Err(EINVAL),
        }
    }
}

/// One NTSYNC_IOC_WAIT_ANY / WAIT_ALL after the syscall layer checked its arguments (objects of this
/// device, no duplicates for wait-all, at most 64).
pub struct Wait<'a> {
    pub objs: &'a [Arc<Obj>],
    pub alert: Option<&'a Arc<Obj>>,
    pub owner: u32,
    pub all: bool,
    /// None: no timeout (U64_MAX).
    pub deadline: Option<Deadline>,
}

/// What ended a wait that nothing signaled.
pub type Interrupted<'a> = &'a dyn Fn() -> Option<i64>;

/// Runs the wait: `Ok((index, abandoned))` (index == objs.len() for the alert event; `abandoned`: the caller
/// answers EOWNERDEAD with the index written), ETIMEDOUT at the deadline, or `intr`'s errno (checked between
/// slices and on every wake-up).
pub fn wait(dev: &Device, req: &Wait<'_>, intr: Interrupted<'_>) -> Result<(u32, bool), i64> {
    WAITS.inc();
    let n = req.objs.len();
    let w = Arc::new(Waiter {
        signaled: AtomicI32::new(-1),
        dead: AtomicBool::new(false),
        owner: req.owner,
        all: if req.all { req.objs.iter().map(|o| o.id).collect() } else { Vec::new() },
        wq: WaitQueue::new(),
    });
    let ids: Vec<u64> = req.objs.iter().chain(req.alert).map(|o| o.id).collect();
    let mut woken = Vec::new();
    {
        let mut st = dev.st.lock();
        for (i, id) in ids.iter().enumerate() {
            let Some(s) = st.objs.get_mut(id) else { continue };
            if req.all && i < n {
                s.all.push(w.clone());
            } else {
                s.any.push((w.clone(), i as u32));
            }
        }
        // Already signaled? The objects in order, the alert event last (the API wants it so).
        if req.all {
            st.try_all(&w, &mut woken);
            if let Some(a) = req.alert
                && !w.done()
            {
                st.try_any(a.id, &mut woken);
            }
        } else {
            for id in &ids {
                if w.done() {
                    break;
                }
                st.try_any(*id, &mut woken);
            }
        }
    }
    dev.queued.fetch_add(1, Ordering::AcqRel);
    wake(woken);

    let mut end = Ok(());
    while !w.done() {
        if let Some(e) = intr() {
            INTERRUPTS.inc();
            end = Err(e);
            break;
        }
        let left = match req.deadline {
            None => SLICE_NS,
            Some(d) => {
                if expired(Some(d)) {
                    end = Err(ETIMEDOUT);
                    break;
                }
                mono_deadline(d).saturating_sub(time::now_ns()).clamp(1, SLICE_NS)
            }
        };
        SLEEPS.inc();
        w.wq.wait_until_timeout(|| w.done() || intr().is_some(), left);
    }

    {
        let mut st = dev.st.lock();
        for id in &ids {
            if let Some(s) = st.objs.get_mut(id) {
                s.any.retain(|(x, _)| !Arc::ptr_eq(x, &w));
                s.all.retain(|x| !Arc::ptr_eq(x, &w));
            }
        }
    }
    dev.queued.fetch_sub(1, Ordering::AcqRel);
    let index = w.signaled.load(Ordering::Acquire);
    if index != -1 {
        return Ok((index as u32, w.dead.load(Ordering::Relaxed)));
    }
    // Events after the device lock is gone; Debug, since a zero-timeout poll ending this way is routine for Wine.
    match end {
        Err(ETIMEDOUT) => {
            TIMEOUTS.inc();
            klog::event!(Debug, "ntsync", "wait.timeout", all = req.all, count = n as u64);
        }
        Err(e) => klog::event!(Debug, "ntsync", "wait.interrupted", all = req.all, count = n as u64, errno = e),
        Ok(()) => {}
    }
    end.map(|_| (0, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Option<Deadline> = Some(Deadline::Monotonic(0));
    fn never() -> Option<i64> {
        None
    }
    fn go(dev: &Device, objs: &[Arc<Obj>], alert: Option<&Arc<Obj>>, owner: u32, all: bool) -> Result<(u32, bool), i64> {
        wait(dev, &Wait { objs, alert, owner, all, deadline: NOW }, &never)
    }

    #[test]
    fn create_rules() {
        let d = Device::new();
        assert_eq!(d.create(State::Sem { count: 3, max: 2 }).err(), Some(EINVAL));
        assert_eq!(d.create(State::Mutex { count: 1, owner: 0, dead: false }).err(), Some(EINVAL));
        assert_eq!(d.create(State::Mutex { count: 0, owner: 5, dead: false }).err(), Some(EINVAL));
        let o = d.create(State::Sem { count: 0, max: 0 }).unwrap();
        assert_eq!(d.objects(), 1);
        drop(o);
        assert_eq!(d.objects(), 0);
    }

    #[test]
    fn semaphore() {
        let d = Device::new();
        let s = d.create(State::Sem { count: 1, max: 2 }).unwrap();
        assert_eq!(s.sem_release(1), Ok(1));
        assert_eq!(s.sem_release(1), Err(EOVERFLOW));
        assert_eq!(s.sem_release(u32::MAX), Err(EOVERFLOW));
        assert_eq!(s.mutex_unlock(1), Err(EINVAL));
        let v = [s.clone()];
        assert_eq!(go(&d, &v, None, 0, false), Ok((0, false)));
        assert_eq!(go(&d, &v, None, 0, false), Ok((0, false)));
        assert_eq!(go(&d, &v, None, 0, false), Err(ETIMEDOUT));
        assert_eq!(s.read(), State::Sem { count: 0, max: 2 });
    }

    #[test]
    fn mutex_owner_recursion_and_kill() {
        let d = Device::new();
        let m = d.create(State::Mutex { count: 0, owner: 0, dead: false }).unwrap();
        let v = [m.clone()];
        assert_eq!(go(&d, &v, None, 7, false), Ok((0, false)));
        assert_eq!(go(&d, &v, None, 7, false), Ok((0, false)));
        assert_eq!(go(&d, &v, None, 8, false), Err(ETIMEDOUT));
        assert_eq!(m.mutex_unlock(8), Err(EPERM));
        assert_eq!(m.mutex_unlock(0), Err(EINVAL));
        assert_eq!(m.mutex_unlock(7), Ok(2));
        assert_eq!(m.mutex_kill(7), Ok(()));
        assert_eq!(m.read(), State::Mutex { count: 0, owner: 0, dead: true });
        assert_eq!(go(&d, &v, None, 8, false), Ok((0, true)));
        assert_eq!(m.read(), State::Mutex { count: 1, owner: 8, dead: false });
        assert_eq!(m.mutex_kill(7), Err(EPERM));
    }

    #[test]
    fn events_and_alert() {
        let d = Device::new();
        let auto = d.create(State::Event { manual: false, signaled: true }).unwrap();
        let man = d.create(State::Event { manual: true, signaled: false }).unwrap();
        let v = [auto.clone()];
        assert_eq!(go(&d, &v, None, 0, false), Ok((0, false)));
        assert_eq!(auto.read(), State::Event { manual: false, signaled: false });
        assert_eq!(go(&d, &v, Some(&man), 0, false), Err(ETIMEDOUT));
        assert_eq!(man.event_set(false), Ok(false));
        assert_eq!(go(&d, &v, Some(&man), 0, false), Ok((1, false)));
        assert_eq!(go(&d, &v, Some(&man), 0, true), Ok((1, false)));
        assert_eq!(man.event_reset(), Ok(true));
        assert_eq!(auto.event_set(true), Ok(false));
        assert_eq!(auto.read(), State::Event { manual: false, signaled: false });
    }

    #[test]
    fn wait_all_is_all_or_nothing() {
        let d = Device::new();
        let s = d.create(State::Sem { count: 1, max: 5 }).unwrap();
        let e = d.create(State::Event { manual: false, signaled: false }).unwrap();
        let v = [s.clone(), e.clone()];
        assert_eq!(go(&d, &v, None, 0, true), Err(ETIMEDOUT));
        assert_eq!(s.read(), State::Sem { count: 1, max: 5 });
        e.event_set(false).unwrap();
        assert_eq!(go(&d, &v, None, 0, true), Ok((0, false)));
        assert_eq!(s.read(), State::Sem { count: 0, max: 5 });
        assert_eq!(e.read(), State::Event { manual: false, signaled: false });
    }
}
