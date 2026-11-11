//! GX-9b: clocks and GFXOFF on the running 780M, as Linux's swsmu and amdgpu_gfx_off_ctrl do them.
//!
//! **Clocks.** PMFW runs GFXCLK by load itself; the driver only names its limits. Linux's AUTO level is
//! SetHardMinGfxClk(MinGfxClk), then SetSoftMaxGfxClk(MaxGfxClk), both from PMFW's own DpmClocks table
//! (smu_v13_0_4_ppt.c:841-889, :968-1044). Linux also names FCLK, SOCCLK, VCLK and DCLK there, but sends nothing at all
//! when it loads at AUTO (amdgpu_smu.c:1881), so those stay as PMFW has them, and the display's clocks are not even
//! on the message list. No frequency but the table's is ever sent. A failed message is logged, the clocks stay as
//! they are, the driver goes on.
//!
//! **GFXOFF.** Linux's request count: it starts at 1 (not allowed), every user of GC's registers holds it up, and 100 ms
//! after it reached 0 AllowGfxOff goes out (amdgpu_gfx.c:671-720). PMFW and the RLC then power GC down when it is idle.
//!
//! **How registers are read while the block sleeps: they are not.** `Dpm` is the job half's `Chain`, so every step of
//! the job half that can touch a register holds the count and sends DisallowGfxOff first when it was allowed; INFO's
//! register reads, `gx_probe` and `gx_bringup` take `awake`. Stricter than Linux, which rings doorbells and reads its
//! write-backs without it; those two need no awake block here either (a doorbell wakes it, write-backs are memory), so
//! a job in flight with no chain step between is safe too. Metrics, SMUIO's status and the TSC are not GC's and leave
//! the block asleep. The Quiesce hook sends Disallow on its own before the PSP's hook touches GC (`dpm_live`).
//!
//! **A Disallow that failed** leaves the block asleep, and the step goes on without registers (`dpm_asleep`): its memory
//! and doorbells work, so fences are still seen and a hang still ends; the next step asks again. **A message PMFW left
//! unanswered** stops the messages until its late answer shows, one mailbox read per look with no wait; until then
//! the driver table's page stays ours, as PMFW may still write to it.

use crate::dpm_asleep::Asleep;
use crate::dpm_table::{self, GfxRange, Metrics};
use crate::gpucore::gfxup::GcAddrs;
use crate::gpucore::psp::Hw;
use crate::gpucore::smu::{self, SmuError, SmuTimeouts};
use crate::hw::chain::{Chain, ChainOwner, Reinit};
use crate::hw::heap::{Heaps, TablePage};
use crate::ipmod::smu13::{self, Addrs, msg, resp};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use drm::fence::{FenceCb, Waiter};
use ksync::{Mutex, SpinLock};

/// One word to turn GFXOFF off for a build, should the chip disagree: clocks and `gpu` stay.
pub const GFXOFF_ON: bool = true;
/// GFX_OFF_DELAY_ENABLE (amdgpu_gfx.c:34).
pub const GFX_OFF_DELAY_US: u64 = 100_000;
/// RLC_PG_DELAY_3_DEFAULT_GC_11_0_1 (gfx_v11_0.c:57).
pub const PG_DELAY_3: u32 = 0x1388;
const RLC_ENABLE_F32: u32 = 1;
const RLC_SAFE_MODE_CMD: u32 = 1;
const RLC_SAFE_MODE_MESSAGE_1: u32 = 1 << 1;
const GFX_POWER_GATING_ENABLE: u32 = 1;
/// Linux's adev->usec_timeout for the RLC's answer.
const SAFE_MODE_US: u64 = 100_000;
/// How long GC may take to read again after DisallowGfxOff was answered.
const WAKE_US: u64 = 50_000;
/// `last_use` when no clock was at hand: the thread starts the delay at its next look.
const NO_STAMP: u64 = u64::MAX;
/// The chain's lock is held one short step at a time (`LiveChain` gives up after 100 ms): a few tries find it.
const TRIES: u32 = 5;
#[cfg(not(test))]
const IDLE_NS: u64 = 10_000_000_000;

/// The addresses of the running chain.
#[derive(Clone, Copy, Debug)]
pub struct Wire {
    pub smu: Addrs,
    pub gc: GcAddrs,
}

#[derive(Clone, Copy, Debug)]
pub struct DpmCfg {
    /// The delayed Allow's thread; tests drive `tick` themselves on the fake's clock.
    pub thread: bool,
}

/// What the Quiesce hook reads, with no lock.
#[derive(Default)]
pub struct Flags {
    /// AllowGfxOff went out and no Disallow was answered since (or its answer is unknown).
    pub asleep: AtomicBool,
    /// RLC_PG_CNTL.GFX_POWER_GATING_ENABLE is ours to clear.
    pub pg_on: AtomicBool,
}

/// What became of Linux's AUTO level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Level {
    NotStarted,
    Auto(GfxRange),
    /// Nothing was changed, or only as far as the reason says.
    Left(String),
}

struct Table {
    page: TablePage,
    mc: u64,
}

impl Table {
    fn wipe(&self, bytes: usize) -> bool {
        self.page.win.write(self.page.at, &alloc::vec![0u8; bytes])
    }
    fn read(&self, bytes: usize) -> Option<alloc::vec::Vec<u8>> {
        let mut b = alloc::vec![0u8; bytes];
        self.page.win.read(self.page.at, &mut b).then_some(b)
    }
}

pub struct Dpm {
    inner: Arc<dyn Chain>,
    heaps: Arc<Heaps>,
    wire: SpinLock<Option<Wire>>,
    flags: Arc<Flags>,
    /// From `start` to `stop`: the gate works; else every step passes straight through.
    live: AtomicBool,
    /// The driver table is set up.
    ready: AtomicBool,
    /// PMFW left a message unanswered: no more messages until its answer shows (`mailbox_free`).
    dead: AtomicBool,
    /// PMFW refused Allow, or the RLC's part failed: GFXOFF stays off.
    no_allow: AtomicBool,
    /// amdgpu_gfx.h:418 gfx_off_req_count.
    requests: AtomicU32,
    /// The clock of the chain's owner at the last release.
    last_use: AtomicU64,
    /// An Allow is due once the delay has passed.
    armed: AtomicBool,
    closing: AtomicBool,
    /// A step went without registers and the log said so; cleared when the block is up again.
    told_asleep: AtomicBool,
    table: Mutex<Option<Table>>,
    level: SpinLock<Level>,
    allows: AtomicU64,
    disallows: AtomicU64,
    failures: AtomicU64,
    wake: Arc<Waiter>,
    thread: SpinLock<Option<sched::JoinHandle>>,
}

/// The thread's next look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    Idle,
    AfterUs(u64),
    Exit,
}

/// GC stays powered while this lives. `up`: it is (false: PMFW did not answer, or the chain could not be had).
pub struct Awake {
    d: Option<Arc<Dpm>>,
    pub up: bool,
}

impl Drop for Awake {
    fn drop(&mut self) {
        if let Some(d) = self.d.take() {
            d.release(NO_STAMP);
        }
    }
}

/// What `gpu` shows.
pub struct Report {
    pub level: Level,
    pub metrics: Result<Metrics, String>,
    pub smuio: Option<u32>,
    pub requests: u32,
    pub asleep: bool,
    pub pg_on: bool,
    pub dead: bool,
    pub no_allow: bool,
    pub allows: u64,
    pub disallows: u64,
    pub failures: u64,
}

impl Dpm {
    pub(crate) fn new(inner: Arc<dyn Chain>, heaps: Arc<Heaps>) -> Arc<Dpm> {
        Arc::new(Dpm {
            inner,
            heaps,
            wire: SpinLock::named("amdgpu.dpm.wire", None),
            flags: Arc::new(Flags::default()),
            live: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            no_allow: AtomicBool::new(false),
            requests: AtomicU32::new(1),
            last_use: AtomicU64::new(NO_STAMP),
            armed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            told_asleep: AtomicBool::new(false),
            table: Mutex::named("amdgpu.dpm.table", None),
            level: SpinLock::named("amdgpu.dpm.level", Level::NotStarted),
            allows: AtomicU64::new(0),
            disallows: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            wake: Waiter::new(),
            thread: SpinLock::named("amdgpu.dpm.thread", None),
        })
    }

    pub fn flags(&self) -> Arc<Flags> {
        self.flags.clone()
    }

    /// `adev->pm.dpm_enabled`: the metrics are there to ask for.
    pub fn enabled(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.dead.load(Ordering::Acquire)
    }

    /// One message: a failed one is logged and counted, PMFW silent on one stops the messages until `mailbox_free`.
    fn send(&self, hw: &mut dyn Hw, a: &Addrs, id: u32, arg: u32) -> Result<u32, SmuError> {
        let r = smu::send_running(hw, a, SmuTimeouts::RUNNING, id, arg);
        match &r {
            Ok(_) => klog::event!(Debug, "gx", "dpm.sent", name = smu13::msg_name(id), arg = klog::Hex(u64::from(arg))),
            Err(e) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                klog::event!(Warn, "gx", "dpm.message_failed", name = smu13::msg_name(id), arg = klog::Hex(u64::from(arg)), why = format!("{e:?}").as_str();
                    "logged, the clocks stay as they are");
                if e.needs_reboot() {
                    self.dead.store(true, Ordering::Release);
                    klog::event!(Error, "gx", "dpm.pmfw_silent"; "no DPM messages until its answer shows; the job half goes on");
                }
            }
        }
        r
    }

    fn wire(&self) -> Option<Wire> {
        *self.wire.lock()
    }

    /// True: nothing of ours is in flight. Linux's pre-poll without its wait: this runs under the chain's lock on the
    /// job path, and a wedged PMFW must not cost every step 250 ms. The mailbox is MP1's: a sleeping GC does not matter.
    fn mailbox_free(&self, hw: &mut dyn Hw, a: &Addrs) -> bool {
        if !self.dead.load(Ordering::Acquire) {
            return true;
        }
        match hw.reg_read(a.resp) {
            Some(code) if code != resp::NONE => {
                self.dead.store(false, Ordering::Release);
                klog::event!(Warn, "gx", "dpm.pmfw_back", code = klog::Hex(u64::from(code)); "the late answer came: messages again");
                true
            }
            _ => false,
        }
    }

    /// The driver table's page back, unless PMFW may still write to it: the allocator serves page tables from the same
    /// heap. A `TablePage` has no Drop: the page then stays out of use for this boot.
    fn give_back(&self, t: Table) {
        if self.dead.load(Ordering::Acquire) {
            klog::event!(Warn, "gx", "dpm.table_kept", mc = klog::Hex(t.mc); "PMFW may still write it: never reused in this boot");
            return;
        }
        self.heaps.free_table(t.page);
    }

    /// Under the chain: GC powered before the step. A failure leaves `asleep` set; the next step tries again.
    fn wake_gc(&self, hw: &mut dyn Hw) {
        if !self.flags.asleep.load(Ordering::Acquire) {
            return;
        }
        let Some(w) = self.wire() else { return };
        if self.mailbox_free(hw, &w.smu) && self.send(hw, &w.smu, msg::DISALLOW_GFXOFF, 0).is_ok() {
            if !settled(hw, &w.gc) {
                klog::event!(Warn, "gx", "dpm.slow_wake", waited_us = WAKE_US; "GC reads no register yet after DisallowGfxOff");
            }
            self.flags.asleep.store(false, Ordering::Release);
            self.told_asleep.store(false, Ordering::Release);
            self.disallows.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The count down; at 0 the delay runs (`stamp`: the chain's clock now).
    fn release(&self, stamp: u64) {
        self.last_use.store(stamp, Ordering::Release);
        let last = self.requests.fetch_sub(1, Ordering::AcqRel) == 1;
        if last && !self.no_allow.load(Ordering::Acquire) && !self.dead.load(Ordering::Acquire) && !self.armed.swap(true, Ordering::AcqRel) {
            self.wake.call();
        }
    }

    /// GC powered until the answer is dropped (`amdgpu_gfx_off_ctrl(adev, false)`).
    pub fn awake(self: &Arc<Self>) -> Awake {
        if !self.live.load(Ordering::Acquire) {
            return Awake { d: None, up: !self.flags.asleep.load(Ordering::Acquire) };
        }
        // SeqCst against `tick`'s pair: this read of `asleep` skips the chain's lock, so either it sees the store of an
        // Allow on its way, or that Allow's second look at the count sees this request.
        self.requests.fetch_add(1, Ordering::SeqCst);
        for _ in 0..TRIES {
            if !self.flags.asleep.load(Ordering::SeqCst) || self.inner.with(&mut |o| self.wake_gc(o.io())) {
                break;
            }
        }
        let up = !self.flags.asleep.load(Ordering::Acquire);
        if !up {
            klog::event!(Warn, "gx", "dpm.still_asleep"; "GFXOFF could not be taken back: GC's registers stay unread");
        }
        Awake { d: Some(self.clone()), up }
    }

    /// Clocks by load, the driver table, the RLC's part of GFXOFF and the first release. Never fails the driver.
    pub fn start(self: &Arc<Self>, wire: Wire, cfg: DpmCfg) {
        *self.wire.lock() = Some(wire);
        let level = match self.clocks(&wire) {
            Ok(r) => Level::Auto(r),
            Err(why) => {
                klog::event!(Warn, "gx", "dpm.level_left", why = why.as_str());
                Level::Left(why)
            }
        };
        *self.level.lock() = level;
        self.live.store(true, Ordering::Release);
        if GFXOFF_ON && !self.dead.load(Ordering::Acquire) {
            self.enable_gfxoff(&wire);
        }
        #[cfg(not(test))]
        if cfg.thread {
            let me = self.clone();
            match sched::spawn("amdgpu-dpm", sched::Class::Normal, move || me.thread_main()) {
                Ok(t) => *self.thread.lock() = Some(t),
                Err(e) => {
                    self.no_allow.store(true, Ordering::Release);
                    klog::event!(Error, "gx", "dpm.no_thread", why = format!("{e:?}").as_str(); "GFXOFF stays off");
                }
            }
        }
        #[cfg(test)]
        let _ = cfg;
        klog::event!(Info, "gx", "dpm.up", dead = self.dead.load(Ordering::Acquire));
    }

    /// The driver table's address and PMFW's range (smu_v13_0_set_driver_table_location, smu_v13_0_4_ppt.c:150), then AUTO.
    fn clocks(&self, w: &Wire) -> Result<GfxRange, String> {
        let page = self.heaps.alloc_table().ok_or("no carve-out page for the driver table")?;
        let table = Table { mc: self.heaps.carve.mc_base() + page.offset, page };
        let mc = table.mc;
        if !table.wipe(amdgpu_fmt::store::TABLE_BYTES as usize) {
            self.heaps.free_table(table.page);
            return Err(String::from("the driver table's page did not write"));
        }
        let mut features = Err(String::from("the chain could not be had"));
        self.chain(&mut |o| {
            let hw = o.io();
            features = (|| {
                self.send(hw, &w.smu, msg::SET_DRIVER_DRAM_ADDR_HIGH, (mc >> 32) as u32).map_err(|e| format!("SetDriverDramAddrHigh: {e:?}"))?;
                self.send(hw, &w.smu, msg::SET_DRIVER_DRAM_ADDR_LOW, mc as u32).map_err(|e| format!("SetDriverDramAddrLow: {e:?}"))?;
                let lo = self.send(hw, &w.smu, msg::GET_ENABLED_SMU_FEATURES, 0).map_err(|e| format!("GetEnabledSmuFeatures 0: {e:?}"))?;
                let hi = self.send(hw, &w.smu, msg::GET_ENABLED_SMU_FEATURES, 1).map_err(|e| format!("GetEnabledSmuFeatures 1: {e:?}"))?;
                hw.flush_hdp();
                self.send(hw, &w.smu, msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_DPMCLOCKS).map_err(|e| format!("DpmClocks: {e:?}"))?;
                hw.invalidate_hdp();
                Ok(u64::from(hi) << 32 | u64::from(lo))
            })();
        });
        let bytes = table.read(dpm_table::DPMCLOCKS_BYTES);
        // Kept once PMFW has answered the transfer: the metrics work without the limits.
        let features = match features {
            Ok(f) => f,
            Err(why) => {
                self.give_back(table);
                return Err(why);
            }
        };
        *self.table.lock() = Some(table);
        self.ready.store(true, Ordering::Release);
        if features & (1 << smu13::FEATURE_GFX_DPM_BIT) == 0 {
            return Err(format!("PMFW runs no GFX DPM (features {features:#x}): nothing sent"));
        }
        let range = bytes.as_deref().and_then(dpm_table::gfx_range).ok_or("PMFW's DpmClocks names no GFX range: nothing sent")?;
        let mut sent = Err(String::from("the chain could not be had"));
        self.chain(&mut |o| {
            let hw = o.io();
            sent = (|| {
                self.send(hw, &w.smu, msg::SET_HARD_MIN_GFXCLK, range.min).map_err(|e| format!("SetHardMinGfxClk: {e:?}"))?;
                self.send(hw, &w.smu, msg::SET_SOFT_MAX_GFXCLK, range.max).map_err(|e| format!("SetSoftMaxGfxClk: {e:?}"))?;
                Ok(())
            })();
        });
        sent.map(|()| range)
    }

    /// `inner.with`, a few tries.
    fn chain(&self, f: &mut dyn FnMut(&mut dyn ChainOwner)) -> bool {
        (0..TRIES).any(|_| self.inner.with(f))
    }

    /// gfx_v11_0_set_powergating_state on GC 11.0.4 (gfx_v11_0.c:5039-5049): the RLC's power gating, then the count down.
    fn enable_gfxoff(&self, w: &Wire) {
        let mut done = None;
        self.chain(&mut |o| {
            let hw = o.io();
            let r = cntl_pg(hw, &w.gc, true);
            done = Some(r.map(|()| hw.now_us()));
        });
        match done {
            Some(Ok(now)) => {
                self.flags.pg_on.store(true, Ordering::Release);
                self.release(now);
            }
            why => {
                self.no_allow.store(true, Ordering::Release);
                self.failures.fetch_add(1, Ordering::Relaxed);
                klog::event!(Warn, "gx", "dpm.no_gfxoff", why = format!("{why:?}").as_str(); "the RLC's part failed: GFXOFF stays off");
            }
        }
    }

    /// One look: AllowGfxOff when the count stood at 0 for the whole delay.
    pub fn tick(&self) -> Next {
        if self.closing.load(Ordering::Acquire) {
            return Next::Exit;
        }
        if !self.live.load(Ordering::Acquire) {
            return Next::Idle;
        }
        if self.dead.load(Ordering::Acquire) {
            let Some(w) = self.wire() else { return Next::Idle };
            let mut free = false;
            self.inner.with(&mut |o| free = self.mailbox_free(o.io(), &w.smu));
            if !free {
                self.armed.store(false, Ordering::Release);
                return Next::Idle;
            }
            // `release` armed nothing during the silence.
            self.armed.store(GFXOFF_ON && self.flags.pg_on.load(Ordering::Acquire), Ordering::Release);
        }
        if !self.armed.load(Ordering::Acquire) {
            return Next::Idle;
        }
        if self.no_allow.load(Ordering::Acquire) || self.flags.asleep.load(Ordering::Acquire) {
            self.armed.store(false, Ordering::Release);
            return Next::Idle;
        }
        // A long holder (a gx_bringup) keeps the chain's lock: no look at it until the count is down.
        if self.requests.load(Ordering::Acquire) > 0 {
            return Next::AfterUs(GFX_OFF_DELAY_US);
        }
        let Some(w) = self.wire() else { return Next::Idle };
        let mut next = Next::AfterUs(GFX_OFF_DELAY_US);
        self.inner.with(&mut |o| {
            let hw = o.io();
            if self.requests.load(Ordering::Acquire) > 0 {
                return;
            }
            let now = hw.now_us();
            let mut last = self.last_use.load(Ordering::Acquire);
            if last == NO_STAMP {
                self.last_use.store(now, Ordering::Release);
                last = now;
            }
            let passed = now.saturating_sub(last);
            if passed < GFX_OFF_DELAY_US {
                next = Next::AfterUs(GFX_OFF_DELAY_US - passed);
                return;
            }
            // Set first: the Quiesce hook may stop this CPU between the message and its answer. Then the count once more:
            // `awake` skips this lock when it reads `asleep` clear (SeqCst, see there).
            self.flags.asleep.store(true, Ordering::SeqCst);
            if self.requests.load(Ordering::SeqCst) > 0 {
                self.flags.asleep.store(false, Ordering::SeqCst);
                return;
            }
            self.armed.store(false, Ordering::Release);
            next = Next::Idle;
            match self.send(hw, &w.smu, msg::ALLOW_GFXOFF, 0) {
                Ok(_) => {
                    self.allows.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    self.no_allow.store(true, Ordering::Release);
                    // Unanswered it may still take effect: the block counts as asleep.
                    self.flags.asleep.store(e.needs_reboot(), Ordering::Release);
                }
            }
        });
        next
    }

    #[cfg(not(test))]
    fn thread_main(&self) {
        loop {
            match self.tick() {
                Next::Exit => break,
                Next::Idle => self.wake.sleep(IDLE_NS),
                Next::AfterUs(us) => self.wake.sleep(us.saturating_mul(1000)),
            }
        }
    }

    /// The node goes, or the bring-up stage after it: Disallow, the RLC's part off (Linux's order), the table freed.
    /// Afterwards every step passes straight through.
    pub fn stop(&self) {
        if !self.live.swap(false, Ordering::AcqRel) {
            return;
        }
        self.closing.store(true, Ordering::Release);
        self.wake.call();
        let t = self.thread.lock().take();
        if let Some(t) = t
            && !t.join_timeout(5_000_000_000)
        {
            klog::event!(Error, "gx", "dpm.thread_stuck");
        }
        if let Some(w) = self.wire() {
            let ran = self.chain(&mut |o| {
                let hw = o.io();
                self.mailbox_free(hw, &w.smu);
                self.wake_gc(hw);
                if self.flags.pg_on.load(Ordering::Acquire) && !self.flags.asleep.load(Ordering::Acquire) {
                    match cntl_pg(hw, &w.gc, false) {
                        Ok(()) => self.flags.pg_on.store(false, Ordering::Release),
                        Err(why) => klog::event!(Warn, "gx", "dpm.pg_left", why = why),
                    }
                }
            });
            if !ran {
                klog::event!(Warn, "gx", "dpm.stop_no_chain"; "the chain could not be had: the Quiesce hook still sends Disallow");
            }
        }
        self.ready.store(false, Ordering::Release);
        let t = self.table.lock().take();
        if let Some(t) = t {
            self.give_back(t);
        }
        *self.level.lock() = Level::NotStarted;
        klog::event!(Info, "gx", "dpm.down", asleep = self.flags.asleep.load(Ordering::Acquire));
    }

    /// SmuMetrics through the driver table (smu_cmn_update_table, smu_cmn.c:859-901; HDP invalidated before the read),
    /// with SMUIO's status beside it. Neither is GC's: a sleeping block sleeps on.
    pub fn metrics(&self) -> Result<(Metrics, Option<u32>), String> {
        let w = self.wire().ok_or("not started")?;
        let g = self.table.lock_timeout(1_000_000_000).ok_or("another reader holds the table")?;
        let t = g.as_ref().ok_or("no driver table")?;
        if !t.wipe(dpm_table::METRICS_BYTES) {
            return Err(String::from("the driver table's page did not write"));
        }
        let mut sent = Err(String::from("the chain could not be had"));
        let mut status = None;
        self.chain(&mut |o| {
            let hw = o.io();
            if !self.mailbox_free(hw, &w.smu) {
                sent = Err(String::from("PMFW left a message unanswered: nothing goes out until its answer shows"));
                return;
            }
            hw.flush_hdp();
            sent = self.send(hw, &w.smu, msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_SMU_METRICS).map_err(|e| format!("{e:?}"));
            hw.invalidate_hdp();
            status = w.gc.smuio_misc.and_then(|b| hw.reg_read(b));
        });
        sent?;
        let bytes = t.read(dpm_table::METRICS_BYTES).ok_or("the driver table's page did not read")?;
        let m = dpm_table::metrics(&bytes).ok_or("PMFW wrote nothing into the driver table")?;
        Ok((m, status))
    }

    /// One step of the job half, without registers while the block sleeps.
    fn step(&self, o: &mut dyn ChainOwner, f: &mut dyn FnMut(&mut dyn ChainOwner)) {
        if !self.flags.asleep.load(Ordering::Acquire) {
            return f(o);
        }
        if !self.told_asleep.swap(true, Ordering::AcqRel) {
            klog::event!(Warn, "gx", "dpm.step_asleep"; "GFXOFF could not be taken back: steps go without GC's registers until it is");
        }
        f(&mut Asleep(o));
    }

    pub fn report(&self) -> Report {
        let (metrics, smuio) = match self.metrics() {
            Ok((m, s)) => (Ok(m), s),
            Err(e) => (Err(e), None),
        };
        Report {
            level: self.level.lock().clone(),
            metrics,
            smuio,
            requests: self.requests.load(Ordering::Acquire),
            asleep: self.flags.asleep.load(Ordering::Acquire),
            pg_on: self.flags.pg_on.load(Ordering::Acquire),
            dead: self.dead.load(Ordering::Acquire),
            no_allow: self.no_allow.load(Ordering::Acquire),
            allows: self.allows.load(Ordering::Relaxed),
            disallows: self.disallows.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
        }
    }
}

impl Chain for Dpm {
    fn with(&self, f: &mut dyn FnMut(&mut dyn ChainOwner)) -> bool {
        if !self.live.load(Ordering::Acquire) {
            // A stop PMFW never answered leaves the block asleep.
            return self.inner.with(&mut |o| self.step(o, f));
        }
        self.requests.fetch_add(1, Ordering::AcqRel);
        let mut stamp = NO_STAMP;
        // A few tries: a message holds the lock longer (up to 250 ms) than one wait for it lasts (100 ms).
        let ran = self.chain(&mut |o| {
            self.wake_gc(o.io());
            self.step(o, f);
            stamp = o.io().now_us();
        });
        self.release(stamp);
        ran
    }
    fn note_stranded(&self) {
        self.inner.note_stranded();
    }
    /// GX-9a's full reset replaces the owner, and a block that may sleep has no registers to halt it with: clocks and GFXOFF
    /// end first, as a stop's. They stay ended after the reset: `gpu` says so, PMFW keeps the clocks.
    fn suspend_gc(&self, lines: &mut Vec<String>) -> bool {
        self.stop();
        if self.flags.asleep.load(Ordering::Acquire) {
            lines.push(String::from("reset: GC sleeps and PMFW did not give it back"));
            return false;
        }
        self.inner.suspend_gc(lines)
    }
    fn mode2_reset(&self, lines: &mut Vec<String>) -> Option<bool> {
        self.inner.mode2_reset(lines)
    }
    fn reinit_gc(&self, lines: &mut Vec<String>) -> Option<Reinit> {
        self.inner.reinit_gc(lines)
    }
    fn reset_abandoned(&self, gone: bool) {
        self.inner.reset_abandoned(gone);
    }
    fn proven_halted(&self) {
        self.inner.proven_halted();
    }
    fn device_lost(&self) {
        self.inner.device_lost();
    }
}

/// After DisallowGfxOff's answer: RLC_CNTL reads again. Linux 6.6 has no status reader for 13.0.4 and goes straight on
/// (smu_v13_0_gfx_off_control); SMUIO's status is unverified here and reads 0 after a crash, so the register itself
/// says it. Bounded; false: it still reads all ones.
pub fn settled(hw: &mut dyn Hw, gc: &GcAddrs) -> bool {
    let mut last = 0;
    matches!(smu::poll(hw, gc.rlc_cntl, WAKE_US, &mut last, |v| v != u32::MAX), Ok(Some(_)))
}

/// gfx_v11_cntl_pg (gfx_v11_0.c:4991-5021): under the RLC's safe mode GFX_POWER_GATING_ENABLE, on enable with
/// RLC_PG_DELAY_3's hysteresis. A stopped RLC has no safe mode (amdgpu_gfx_rlc_enter_safe_mode). Bounded; no lock, no
/// allocation: the Quiesce hook runs it too.
pub fn cntl_pg(hw: &mut dyn Hw, gc: &GcAddrs, enable: bool) -> Result<(), &'static str> {
    let rlc = hw.reg_read(gc.rlc_cntl).ok_or("RLC_CNTL did not read")?;
    let safe = rlc != u32::MAX && rlc & RLC_ENABLE_F32 != 0;
    if safe {
        if !hw.reg_write(gc.rlc_safe_mode, RLC_SAFE_MODE_CMD | RLC_SAFE_MODE_MESSAGE_1) {
            return Err("RLC_SAFE_MODE refused the write");
        }
        let mut last = 0;
        match smu::poll(hw, gc.rlc_safe_mode, SAFE_MODE_US, &mut last, |v| v & RLC_SAFE_MODE_CMD == 0) {
            Ok(Some(_)) => {}
            Ok(None) => return Err("the RLC did not answer safe mode"),
            Err(_) => return Err("RLC_SAFE_MODE did not read"),
        }
    }
    let mut r = Ok(());
    match hw.reg_read(gc.rlc_pg_cntl) {
        Some(v) if v != u32::MAX => {
            let v = if enable { v | GFX_POWER_GATING_ENABLE } else { v & !GFX_POWER_GATING_ENABLE };
            if !hw.reg_write(gc.rlc_pg_cntl, v) || (enable && !hw.reg_write(gc.rlc_pg_delay_3, PG_DELAY_3)) {
                r = Err("RLC_PG_CNTL or RLC_PG_DELAY_3 refused the write");
            }
        }
        _ => r = Err("RLC_PG_CNTL did not read"),
    }
    if safe && !hw.reg_write(gc.rlc_safe_mode, RLC_SAFE_MODE_CMD) {
        return Err("RLC_SAFE_MODE refused the write");
    }
    r
}
