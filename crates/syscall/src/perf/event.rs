//! One perf event and the file behind its fd (linux-7.3.1/kernel/events/core.c `perf_fops`): counts, times,
//! read(2), poll, the ioctls, mmap of the ring buffer, close.
//!
//! Counts (`perf_event_count` of the two clocks): a CPU-wide event counts the wall-clock time it was enabled; a
//! thread event the time its thread ran while it was enabled (the scheduler's run time). TIME_ENABLED and
//! TIME_RUNNING are those same times (a software event always runs while enabled; a thread event's context only
//! advances while its thread runs). DUMMY counts nothing.

use super::ring::{Charge, Put, Ring};
use super::{sampler, side};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{IrqSpinLock, SpinLock};
use perf_abi::record::{Ident, ReadValue};
use perf_abi::{Attr, attr::flag};
use uapi::errno::*;
use vfs::abi::Attr as FileAttr;
use vfs::poll::{Events, PollQueue};
use vfs::{Errno, File, OpenFile};

/// Which clock the event is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    CpuClock,
    TaskClock,
    Dummy,
}

/// What the event watches.
pub(crate) enum Target {
    /// Everything on one CPU (pid -1).
    Cpu(usize),
    /// One thread (pid 0 or a tid), on any CPU or only on `cpu`.
    Thread { thread: Arc<proc::Thread>, tid: u32, cpu: Option<usize> },
}

/// The changing part (`perf_event`'s state, count and times), IRQ-safe: the sampler reads it.
pub(crate) struct State {
    pub enabled: bool,
    /// When it was last enabled: ns of the monotonic clock (CPU), or the thread's run time (thread).
    since: u64,
    /// Count and enabled time gathered before `since`.
    count: u64,
    time: u64,
    /// The sampler's time towards the next sample (ns).
    pub acc: u64,
    /// `event_limit` of PERF_EVENT_IOC_REFRESH (0: no limit).
    pub limit: i64,
}

pub(crate) struct Event {
    pub id: u64,
    pub attr: Attr,
    pub kind: Kind,
    pub target: Target,
    /// The sample period in ns (0: a counting event); `attr.sample_freq` is turned into it at open.
    pub period: AtomicU64,
    pub st: IrqSpinLock<State>,
    /// The buffer samples and records go to (its own after mmap, or another event's after SET_OUTPUT).
    pub rb: IrqSpinLock<Option<Arc<Ring>>>,
    /// Whether `rb` is this event's own mapping (not one borrowed by SET_OUTPUT).
    own_rb: AtomicBool,
    /// `event->waitq`: the ring's wake-ups and the end of its thread reach it.
    pub wq: Arc<PollQueue>,
    pub lost_samples: AtomicU64,
    /// Its thread ended (`PERF_EVENT_STATE_EXIT`): poll says EPOLLHUP.
    pub hup: AtomicBool,
    /// Samples it wrote (ktests, counters).
    pub samples: AtomicU64,
    /// `kernel.perf_event_max_stack` or the attribute's own limit.
    pub max_stack: u64,
    /// Added to the monotonic clock for the TIME of its records (`use_clockid` with REALTIME or TAI).
    pub clock_off: u64,
    /// Locked pages charged to the opener (`perf_mmap_account`).
    charge: SpinLock<Option<Charge>>,
    pub(crate) closed: AtomicBool,
    /// The group leader of a group member (None: it leads its own group). Fixed after open, so the sampler may
    /// read it; the cycle with `siblings` is broken at the close of either side.
    pub leader: Option<Arc<Event>>,
    /// The other members of the group it leads (changed in thread context only, rebuilt outside the lock).
    pub siblings: IrqSpinLock<Vec<Arc<Event>>>,
    /// A thread event of a caller without CAP_PERFMON on every CPU (cpu -1): its timer share is spread over the
    /// CPUs (`sampler::refresh_cpu`), so it cannot load every CPU at the full rate.
    pub spread: AtomicBool,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Locked pages per uid (`user->perf_mlock`).
static MLOCKED: SpinLock<Vec<(u32, u64)>> = SpinLock::named("perf.mlock", Vec::new());

impl Event {
    pub(crate) fn new(attr: Attr, kind: Kind, target: Target, period: u64, max_stack: u64, clock_off: u64, leader: Option<Arc<Event>>) -> Arc<Event> {
        let enabled = !attr.has(flag::DISABLED);
        let ev = Arc::new(Event {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            attr,
            kind,
            target,
            period: AtomicU64::new(period),
            st: IrqSpinLock::named("perf.event", State { enabled: false, since: 0, count: 0, time: 0, acc: 0, limit: 0 }),
            rb: IrqSpinLock::named("perf.rb", None),
            own_rb: AtomicBool::new(false),
            wq: Arc::new(PollQueue::new()),
            lost_samples: AtomicU64::new(0),
            hup: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            max_stack,
            clock_off,
            charge: SpinLock::named("perf.charge", None),
            closed: AtomicBool::new(false),
            leader,
            siblings: IrqSpinLock::named("perf.siblings", Vec::new()),
            spread: AtomicBool::new(false),
        });
        if enabled {
            ev.set_enabled(true);
        }
        ev
    }

    /// The sampler and the side-band lists see it, and its leader counts it (after `new`, before the fd exists).
    pub(crate) fn publish(self: &Arc<Self>) -> Result<(), Errno> {
        if let Some(l) = &self.leader {
            change_list(&l.siblings, |v| v.push(self.clone()))?;
        }
        sampler::attach(self);
        side::attach(self);
        Ok(())
    }

    /// The group this event is read with: its leader (itself when it leads, or when its leader was closed).
    pub(crate) fn group_leader(self: &Arc<Self>) -> Arc<Event> {
        match &self.leader {
            Some(l) if !l.closed.load(Ordering::Acquire) => l.clone(),
            _ => self.clone(),
        }
    }

    /// The members' values after the leader's (`perf_read_group`, a sample's group read). IRQ-safe.
    pub(crate) fn sibling_values(&self, out: &mut [ReadValue]) -> usize {
        let s = self.siblings.lock();
        let mut n = 0;
        for (o, m) in out.iter_mut().zip(s.iter()) {
            *o = m.read_value();
            n += 1;
        }
        n
    }

    pub(crate) fn cpu(&self) -> Option<usize> {
        match &self.target {
            Target::Cpu(c) => Some(*c),
            Target::Thread { cpu, .. } => *cpu,
        }
    }

    pub(crate) fn tid(&self) -> Option<u32> {
        match &self.target {
            Target::Thread { tid, .. } => Some(*tid),
            Target::Cpu(_) => None,
        }
    }

    /// The scheduler id of its thread (0 before the thread runs, or for a CPU event).
    pub(crate) fn sched_id(&self) -> u64 {
        match &self.target {
            Target::Thread { thread, .. } => thread.kthread(),
            Target::Cpu(_) => 0,
        }
    }

    /// Its thread's run time now (ns).
    fn runtime(&self) -> u64 {
        match &self.target {
            Target::Thread { thread, .. } => thread.sched_thread().map_or(0, |k| k.cpu_times().total_ns()),
            Target::Cpu(_) => 0,
        }
    }

    /// The clock the count runs on now.
    fn clock(&self) -> u64 {
        match self.target {
            Target::Cpu(_) => time::now_ns(),
            Target::Thread { .. } => self.runtime(),
        }
    }

    pub(crate) fn sampling(&self) -> bool {
        self.period.load(Ordering::Relaxed) != 0 && self.kind != Kind::Dummy
    }

    /// (count, enabled, running) now.
    pub(crate) fn values(&self) -> (u64, u64, u64) {
        let now = self.clock();
        let s = self.st.lock();
        let open = if s.enabled { now.saturating_sub(s.since) } else { 0 };
        let time = s.time + open;
        let count = if self.kind == Kind::Dummy { 0 } else { s.count + open };
        (count, time, time)
    }

    /// `perf_event_enable`/`perf_event_disable`.
    pub(crate) fn set_enabled(self: &Arc<Self>, on: bool) {
        let now = self.clock();
        {
            let mut s = self.st.lock();
            if s.enabled == on {
                return;
            }
            if on {
                s.since = now;
            } else {
                let open = now.saturating_sub(s.since);
                s.count += open;
                s.time += open;
            }
            s.enabled = on;
        }
        sampler::refresh(self);
        self.update_userpage();
    }

    /// The REFRESH limit ran out in the sampler (hard-IRQ context): disabled as `set_enabled(false)` does, the
    /// sampler stops at its next look (`perf_event_disable_inatomic`).
    pub(crate) fn stop_in_irq(&self) {
        let now = self.clock();
        let mut s = self.st.lock();
        if s.enabled {
            let open = now.saturating_sub(s.since);
            s.count += open;
            s.time += open;
            s.enabled = false;
        }
    }

    /// PERF_EVENT_IOC_RESET: the count starts again at 0 (the times go on).
    pub(crate) fn reset(&self) {
        let now = self.clock();
        let mut s = self.st.lock();
        let open = if s.enabled { now.saturating_sub(s.since) } else { 0 };
        s.time += open;
        s.since = now;
        s.count = 0;
        drop(s);
        self.update_userpage();
    }

    pub(crate) fn update_userpage(&self) {
        let (c, e, r) = self.values();
        if let Some(rb) = self.rb.lock().as_ref() {
            rb.update_userpage(c, e, r);
        }
    }

    /// The values of a read (`perf_read_one`, or the one-member group of `perf_read_group`).
    pub(crate) fn read_value(&self) -> ReadValue {
        let (value, enabled, running) = self.values();
        ReadValue { value, enabled, running, id: self.id, lost: self.lost_samples.load(Ordering::Relaxed) }
    }


    /// Writes a record into the event's buffer and wakes its readers when due. False when it was lost (or there is
    /// no buffer). IRQ-safe.
    pub(crate) fn output(&self, rec: &[u8], ident: &Ident, sample: bool) -> bool {
        let rb = self.rb.lock();
        let Some(r) = rb.as_ref() else { return false };
        let wakeup_events = if self.attr.has(flag::WATERMARK) { 0 } else { self.attr.wakeup };
        let p = Put { sample_type: self.attr.sample_type, id_all: self.attr.has(flag::SAMPLE_ID_ALL), ident, sample, wakeup_events };
        match r.put(rec, &p) {
            Ok(wake) => {
                if wake {
                    super::WAKEUPS.inc();
                    r.wake_all();
                }
                true
            }
            Err(()) => {
                // Every lost record counts, side-band too (`__perf_output_begin`'s fail path).
                self.lost_samples.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// `perf_mmap`: the event's ring buffer for a mapping of `pages` pages (made by the first mapping).
    pub(crate) fn map(self: &Arc<Self>, pgoff: u64, pages: u64) -> Result<Arc<Ring>, Errno> {
        if self.cpu().is_none() && self.attr.has(flag::INHERIT) {
            return Err(EINVAL);
        }
        if pgoff != 0 {
            // The AUX area needs a PMU that has one.
            return Err(EINVAL);
        }
        if pages == 0 || pages > i32::MAX as u64 {
            return Err(if pages == 0 { EINVAL } else { ENOMEM });
        }
        let data = pages - 1;
        if data != 0 && !data.is_power_of_two() {
            return Err(EINVAL);
        }
        let mut rb = self.rb.lock();
        if let Some(r) = rb.as_ref() {
            if r.nr_pages != pages {
                return Err(EINVAL);
            }
            if !self.own_rb.load(Ordering::Relaxed) {
                return Err(EBUSY);
            }
            return Ok(r.clone());
        }
        drop(rb);
        let charge = self.account(pages)?;
        let watermark = if self.attr.has(flag::WATERMARK) { self.attr.wakeup as u64 } else { 0 };
        let r = match Ring::new(pages, watermark) {
            Ok(r) => r,
            Err(e) => {
                unaccount(charge);
                return Err(e);
            }
        };
        if let Err(e) = r.add_waiter(self.wq.clone()) {
            unaccount(charge);
            return Err(e);
        }
        rb = self.rb.lock();
        if rb.is_some() {
            // Another thread mapped it meanwhile: use that one.
            let other = rb.clone().ok_or(EINVAL)?;
            drop(rb);
            unaccount(charge);
            return if other.nr_pages == pages { Ok(other) } else { Err(EINVAL) };
        }
        *rb = Some(r.clone());
        drop(rb);
        self.own_rb.store(true, Ordering::Relaxed);
        *self.charge.lock() = Some(charge);
        self.update_userpage();
        Ok(r)
    }

    /// `perf_mmap_calc_limits` + `perf_mmap_account` for the opener's uid (V2 keeps no per-mm pinned count: what
    /// exceeds the uid's budget is checked against RLIMIT_MEMLOCK alone).
    fn account(&self, pages: u64) -> Result<Charge, Errno> {
        let me = proc::current();
        let uid = me.as_ref().map_or(0, |p| p.cred().uid);
        let cpus = hal::percpu::cpu_count().max(1) as u64;
        let limit = (super::MLOCK_KB.load(Ordering::Relaxed) >> 2) * cpus;
        let mut m = MLOCKED.lock();
        let have = m.iter().find(|e| e.0 == uid).map_or(0, |e| e.1).min(limit);
        let extra = (have + pages).saturating_sub(limit);
        if extra != 0
            && let Some(p) = me.as_ref()
        {
            let lock_limit = p.limits.get(uapi::ab::rlimit::RLIMIT_MEMLOCK).map_or(u64::MAX, |l| l.cur) >> 12;
            if extra > lock_limit && super::is_paranoid() && !p.capable(proc::cred::Cap::IpcLock) {
                return Err(EPERM);
            }
        }
        let user_pages = pages - extra;
        match m.iter_mut().find(|e| e.0 == uid) {
            Some(e) => e.1 += user_pages,
            None => {
                m.try_reserve(1).map_err(|_| ENOMEM)?;
                m.push((uid, user_pages));
            }
        }
        Ok(Charge { uid, user_pages })
    }

    /// PERF_EVENT_IOC_SET_OUTPUT (`perf_event_set_output`, linux-7.3.1/kernel/events/core.c:13715): records go
    /// to `to`'s buffer (None: back to none). Every refusal is EINVAL, as Linux's.
    pub(crate) fn set_output(self: &Arc<Self>, to: Option<&Arc<Event>>) -> Result<(), Errno> {
        let Some(to) = to else {
            // An event with its own mapping keeps it.
            if self.own_rb.load(Ordering::Relaxed) {
                return Err(EINVAL);
            }
            let old = self.rb.lock().take();
            if let Some(r) = old {
                r.remove_waiter(&self.wq);
            }
            return Ok(());
        };
        // No circular reference; the same CPU, and for per-thread buffers (cpu -1) the same thread; one clock.
        if Arc::ptr_eq(self, to) || self.cpu() != to.cpu() || (to.cpu().is_none() && self.tid() != to.tid()) || !same_clock(&self.attr, &to.attr) {
            return Err(EINVAL);
        }
        if self.own_rb.load(Ordering::Relaxed) {
            return Err(EINVAL);
        }
        let r = to.rb.lock().clone().ok_or(EINVAL)?;
        r.add_waiter(self.wq.clone())?;
        let old = self.rb.lock().replace(r);
        if let Some(o) = old {
            o.remove_waiter(&self.wq);
        }
        Ok(())
    }

    /// The end of the event (close of its last fd).
    pub(crate) fn close(self: &Arc<Self>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.set_enabled(false);
        sampler::detach(self);
        side::detach(self);
        // Linux breaks the group up: the members go on alone; a member leaves its leader's list.
        let _ = change_list(&self.siblings, |v| v.clear());
        if let Some(l) = &self.leader {
            let _ = change_list(&l.siblings, |v| v.retain(|e| !Arc::ptr_eq(e, self)));
        }
        let old = self.rb.lock().take();
        if let Some(r) = old {
            r.remove_waiter(&self.wq);
        }
        if let Some(c) = self.charge.lock().take() {
            unaccount(c);
        }
    }
}

/// `event->clock` equality: the default perf clock, or the same clock id.
pub(crate) fn same_clock(a: &Attr, b: &Attr) -> bool {
    let (ua, ub) = (a.has(flag::USE_CLOCKID), b.has(flag::USE_CLOCKID));
    ua == ub && (!ua || a.clockid == b.clockid)
}

/// Changes a list the sampler reads under an IRQ-safe lock: a copy made outside it, swapped in, the old one dropped
/// outside it (nothing allocates or frees with interrupts off).
pub(crate) fn change_list(l: &IrqSpinLock<Vec<Arc<Event>>>, f: impl Fn(&mut Vec<Arc<Event>>)) -> Result<(), Errno> {
    loop {
        let n = l.lock().len();
        let mut v: Vec<Arc<Event>> = Vec::new();
        v.try_reserve_exact(n + 1).map_err(|_| ENOMEM)?;
        let old = {
            let mut g = l.lock();
            if g.len() != n {
                None
            } else {
                v.extend(g.iter().cloned());
                f(&mut v);
                Some(core::mem::replace(&mut *g, v))
            }
        };
        if old.is_some() {
            drop(old);
            return Ok(());
        }
    }
}

fn unaccount(c: Charge) {
    let mut m = MLOCKED.lock();
    if let Some(e) = m.iter_mut().find(|e| e.0 == c.uid) {
        e.1 = e.1.saturating_sub(c.user_pages);
    }
    m.retain(|e| e.1 != 0);
}

/// `__perf_event_read_size` for a group of `nr` members (1 without FORMAT_GROUP).
pub(crate) fn read_size(read_format: u64, nr: usize) -> usize {
    use perf_abi::*;
    let entry = 1 + (read_format & (FORMAT_ID | FORMAT_LOST)).count_ones() as usize;
    let mut n = (read_format & (FORMAT_TOTAL_TIME_ENABLED | FORMAT_TOTAL_TIME_RUNNING)).count_ones() as usize;
    if read_format & FORMAT_GROUP != 0 {
        n += 1 + entry * nr;
    } else {
        n += entry;
    }
    n * 8
}

/// The words of a read: `perf_read_one`, or `perf_read_group` (`v` the leader's values, `sib` the members').
pub(crate) fn read_words(read_format: u64, v: &ReadValue, sib: &[ReadValue], out: &mut Vec<u64>) {
    use perf_abi::*;
    let mut put = |x: u64| out.push(x);
    if read_format & FORMAT_GROUP != 0 {
        put(1 + sib.len() as u64);
        if read_format & FORMAT_TOTAL_TIME_ENABLED != 0 {
            put(v.enabled);
        }
        if read_format & FORMAT_TOTAL_TIME_RUNNING != 0 {
            put(v.running);
        }
        for m in core::iter::once(v).chain(sib) {
            put(m.value);
            if read_format & FORMAT_ID != 0 {
                put(m.id);
            }
            if read_format & FORMAT_LOST != 0 {
                put(m.lost);
            }
        }
    } else {
        put(v.value);
        if read_format & FORMAT_TOTAL_TIME_ENABLED != 0 {
            put(v.enabled);
        }
        if read_format & FORMAT_TOTAL_TIME_RUNNING != 0 {
            put(v.running);
        }
        if read_format & FORMAT_ID != 0 {
            put(v.id);
        }
        if read_format & FORMAT_LOST != 0 {
            put(v.lost);
        }
    }
}

/// The file behind a perf fd (`perf_fops`); its drop is `perf_release`.
pub(crate) struct PerfFile {
    pub ev: Arc<Event>,
}

/// Every live perf file by its address (group_fd and SET_OUTPUT find the event behind an fd).
static OBJECTS: SpinLock<Vec<(usize, Weak<Event>)>> = SpinLock::named("perf.objects", Vec::new());

pub(crate) fn file_for(ev: Arc<Event>) -> Result<Arc<PerfFile>, Errno> {
    let f = Arc::new(PerfFile { ev: ev.clone() });
    let mut o = OBJECTS.lock();
    o.try_reserve(1).map_err(|_| ENOMEM)?;
    o.push((Arc::as_ptr(&f) as *const u8 as usize, Arc::downgrade(&ev)));
    Ok(f)
}

/// The event behind an open file (`is_perf_file`).
pub(crate) fn of(f: &OpenFile) -> Option<Arc<Event>> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    OBJECTS.lock().iter().find(|e| e.0 == addr).and_then(|e| e.1.upgrade())
}

impl Drop for PerfFile {
    fn drop(&mut self) {
        let me = self as *const PerfFile as usize;
        let gone: Vec<(usize, Weak<Event>)> = {
            let mut o = OBJECTS.lock();
            let (gone, keep): (Vec<_>, Vec<_>) = core::mem::take(&mut *o).into_iter().partition(|e| e.0 == me);
            *o = keep;
            gone
        };
        drop(gone);
        self.ev.close();
    }
}

impl PerfFile {
    /// `__perf_read`: the words of one read (ENOSPC for a buffer shorter than `read_size`).
    fn read_into(&self, left: usize) -> Result<Vec<u64>, Errno> {
        let ev = &self.ev;
        let rf = ev.attr.read_format;
        let mut w = Vec::new();
        if rf & perf_abi::FORMAT_GROUP != 0 {
            let leader = ev.group_leader();
            let n = leader.siblings.lock().len();
            let mut sib = alloc::vec![ReadValue::default(); n];
            let n = leader.sibling_values(&mut sib);
            if left < read_size(rf, 1 + n) {
                return Err(ENOSPC);
            }
            read_words(rf, &leader.read_value(), &sib[..n], &mut w);
        } else {
            if left < read_size(rf, 1) {
                return Err(ENOSPC);
            }
            read_words(rf, &ev.read_value(), &[], &mut w);
        }
        Ok(w)
    }

    /// The events an ioctl acts on: with PERF_IOC_FLAG_GROUP the whole group (`perf_event_for_each`).
    fn targets(&self, arg: u64) -> Vec<Arc<Event>> {
        if arg & perf_abi::IOC_FLAG_GROUP == 0 {
            return alloc::vec![self.ev.clone()];
        }
        let l = self.ev.group_leader();
        let mut v = alloc::vec![l.clone()];
        v.extend(l.siblings.lock().iter().cloned());
        v
    }

    /// The ioctls (`_perf_ioctl`); `mem` reads and writes the caller's argument.
    fn ioctl_cmd(&self, cmd: u64, arg: u64, mem: &dyn vfs::sys::UserMem) -> Result<i64, Errno> {
        use perf_abi::*;
        let ev = &self.ev;
        match cmd {
            IOC_ENABLE | IOC_DISABLE | IOC_RESET => {
                for e in self.targets(arg) {
                    match cmd {
                        IOC_ENABLE => e.set_enabled(true),
                        IOC_DISABLE => e.set_enabled(false),
                        _ => e.reset(),
                    }
                }
                Ok(0)
            }
            IOC_REFRESH => {
                if ev.attr.has(flag::INHERIT) || !ev.sampling() {
                    return Err(EINVAL);
                }
                ev.st.lock().limit += arg as i32 as i64;
                ev.set_enabled(true);
                Ok(0)
            }
            IOC_PERIOD => {
                let mut b = [0u8; 8];
                if !mem.read(arg, &mut b) {
                    return Err(EFAULT);
                }
                let v = u64::from_le_bytes(b);
                if !ev.sampling() || v == 0 || v & (1 << 63) != 0 {
                    return Err(EINVAL);
                }
                ev.period.store(v, Ordering::Relaxed);
                sampler::refresh(ev);
                Ok(0)
            }
            IOC_ID => {
                if !mem.write(arg, &ev.id.to_le_bytes()) {
                    return Err(EFAULT);
                }
                Ok(0)
            }
            IOC_SET_OUTPUT => {
                // Only a 64-bit -1 means "none" (`arg != -1` on an unsigned long); anything else is an fd number
                // (`CLASS(fd)` takes its low 32 bits).
                if arg == u64::MAX {
                    ev.set_output(None)?;
                    return Ok(0);
                }
                let p = proc::current().ok_or(EBADF)?;
                let f = p.fds.get(arg as u32 as usize).map_err(|_| EBADF)?;
                let to = of(&f).ok_or(EBADF)?;
                ev.set_output(Some(&to))?;
                Ok(0)
            }
            IOC_PAUSE_OUTPUT => {
                let rb = ev.rb.lock();
                let r = rb.as_ref().ok_or(EINVAL)?;
                r.pause(arg != 0);
                Ok(0)
            }
            IOC_MODIFY_ATTRIBUTES => {
                // `perf_event_modify_attr`: only breakpoints can change; another type is EINVAL. Linux gives the
                // clocks their own PMU's type (`perf_swevent_init` rewrites `attr.type` to it), which no program
                // passes: for them every type differs.
                let a = super::open::copy_attr_with(|at, b| mem.read(at, b), |at, b| mem.write(at, b), arg).map_err(|e| -e)?;
                let hidden_type = matches!(ev.kind, Kind::CpuClock | Kind::TaskClock);
                Err(if hidden_type || a.kind != ev.attr.kind { EINVAL } else { EOPNOTSUPP })
            }
            IOC_SET_FILTER | IOC_SET_BPF | IOC_QUERY_BPF => Err(EINVAL),
            _ => Err(ENOTTY),
        }
    }
}

impl File for PerfFile {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn read_stream(&self, buf: &mut [u8], _nonblock: bool) -> Result<usize, Errno> {
        let w = self.read_into(buf.len())?;
        for (i, x) in w.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
        }
        Ok(w.len() * 8)
    }
    /// `perf_read`: ENOSPC for a buffer shorter than `read_size`, never blocks.
    fn read_iter(&self, to: &mut dyn vfs::node::IoSink, _status: u64) -> Option<Result<usize, Errno>> {
        let r = self.read_into(to.left()).and_then(|w| {
            let b: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
            if to.copy(&b) == b.len() { Ok(b.len()) } else { Err(EFAULT) }
        });
        Some(r)
    }
    /// No write operation.
    fn write_stream(&self, _buf: &[u8], _nonblock: bool) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        0o600
    }
    /// `perf_poll`: EPOLLHUP once its thread ended or while it has no buffer; else what the buffer's wake-ups
    /// left (taken by this look).
    fn events(&self) -> Events {
        if self.ev.hup.load(Ordering::Acquire) {
            return Events(Events::HUP);
        }
        let rb = self.ev.rb.lock();
        Events(match rb.as_ref() {
            Some(r) => r.take_poll(),
            None => Events::HUP,
        })
    }
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Err(ESPIPE))
    }
    fn attr(&self) -> Option<FileAttr> {
        Some(crate::timerfd::anon_attr())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.ev.wq)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:[perf_event]"))
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn vfs::sys::UserMem) -> Option<Result<i64, Errno>> {
        Some(self.ioctl_cmd(cmd, arg, mem))
    }
    fn device_map_at(&self, pgoff: u64, pages: u64) -> Option<Result<Arc<dyn core::any::Any + Send + Sync>, Errno>> {
        Some(self.ev.map(pgoff, pages).map(|r| r.map.clone() as Arc<dyn core::any::Any + Send + Sync>))
    }
}


/// `event_limit` reached zero after an overflow: the event stops (`perf_event_disable_inatomic`). IRQ-safe part.
pub(crate) fn limit_hit(st: &mut State) -> bool {
    if st.limit == 0 {
        return false;
    }
    st.limit -= 1;
    st.limit == 0
}
