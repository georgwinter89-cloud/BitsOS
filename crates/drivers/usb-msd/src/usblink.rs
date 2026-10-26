//! The real link: a Bulk-Only interface of a USB device through US-3's transfer API.
//!
//! `run` is V1's K-2 queued command (`usb/msdq.asm:1-27`) made general: the CBW is queued on bulk OUT; its
//! completion callback queues the data phase (a device may stall IN tokens that come before its command: QEMU's
//! does), the data's callback queues the CSW (QEMU's usb-storage loses a CSW packet queued while data is in
//! flight), and a clean CSW's callback queues the next command's CBW. A whole run of commands costs the waiting
//! thread one wake-up. A stage that does not finish in its time (CBW 5 s, data 10 s per 64 KiB, CSW 20 s) ends
//! the run: both endpoints aborted, the stage reported as `Timeout`, and the BOT layer does reset recovery.

use crate::bot;
use crate::link::{Cmd, Dir, Link, Ran, CBW_TIMEOUT_NS, CSW_TIMEOUT_NS};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{SpinLock, WaitQueue};
use usb_core::{Buffer, Device, Request, Setup, UsbError, XferResult};

/// How long a run waits for the callbacks of an aborted stage before it gives the run up.
const ABORT_GRACE_NS: u64 = 2_000_000_000;

klog::counter!(RUNS, "usb.msd.runs");
klog::counter!(STAGE_TIMEOUTS, "usb.msd.stage_timeouts");

pub struct UsbLink {
    dev: Arc<Device>,
    iface: u8,
    ep_in: u8,
    ep_out: u8,
    /// CBW at 0..31, CSW at 32..45 of the current run. Replaced after a run whose callbacks never came back.
    small: SpinLock<Buffer>,
    /// A run was given up with transfers outstanding (`Link::take_poisoned`).
    poisoned: AtomicBool,
}

impl UsbLink {
    pub fn new(dev: Arc<Device>, iface: u8, ep_in: u8, ep_out: u8) -> Result<UsbLink, UsbError> {
        let small = dev.alloc_buffer(64)?;
        Ok(UsbLink { dev, iface, ep_in, ep_out, small: SpinLock::named("usb.msd.small", small), poisoned: AtomicBool::new(false) })
    }
    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }
    pub fn endpoints(&self) -> (u8, u8) {
        (self.ep_in, self.ep_out)
    }
    /// Sends raw bytes on bulk OUT (ktests: a broken CBW to drive reset recovery).
    pub fn raw_out(&self, bytes: &[u8]) -> Result<usize, UsbError> {
        self.dev.out_bytes(self.ep_out, bytes, CBW_TIMEOUT_NS)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Cbw,
    Data,
    Csw,
}

struct ChainState {
    results: Vec<Ran>,
    done: bool,
    /// The waiter gave up on a stage: callbacks stop chaining, a cancelled stage counts as a timeout.
    timed_out: bool,
    progress: u64,
    stage: (usize, Stage),
    stage_timeout: u64,
}

struct Chain {
    dev: Arc<Device>,
    ep_in: u8,
    ep_out: u8,
    small: Buffer,
    cmds: Vec<Cmd>,
    st: SpinLock<ChainState>,
    wq: WaitQueue,
}

impl Chain {
    fn finish(&self) {
        self.st.lock().done = true;
        self.wq.wake_all();
    }

    /// Records the stage about to be queued; false if the run is over (timed out) and nothing may be queued.
    fn enter(&self, i: usize, stage: Stage, timeout: u64) -> bool {
        let mut s = self.st.lock();
        if s.timed_out || s.done {
            return false;
        }
        s.progress += 1;
        s.stage = (i, stage);
        s.stage_timeout = timeout;
        true
    }

    fn submit(self: &Arc<Self>, ep: u8, req: Request, then: impl FnOnce(&Arc<Chain>, XferResult) + Send + 'static) {
        let me = self.clone();
        let cb: usb_core::OnDone = Box::new(move |r: &XferResult| then(&me, *r));
        // `submit_then` either queues (the callback runs later, in the controller's event context) or fails at
        // once without running the callback.
        if let Err(e) = self.dev.submit_then(ep, req, cb) {
            // The closure was consumed by the failed submit; finish the stage here.
            self.fail_stage(XferResult::failed(e, 0));
        }
    }

    fn fail_stage(&self, r: XferResult) {
        let (i, stage) = self.st.lock().stage;
        self.record(i, stage, r, [0; 13]);
        self.finish();
    }

    fn record(&self, i: usize, stage: Stage, r: XferResult, csw: [u8; 13]) {
        let mut s = self.st.lock();
        while s.results.len() <= i {
            s.results.push(Ran::cbw_only(XferResult::failed(UsbError::Cancelled, 0)));
        }
        let ran = &mut s.results[i];
        match stage {
            Stage::Cbw => ran.cbw = r,
            Stage::Data => ran.data = Some(r),
            Stage::Csw => ran.csw = Some((r, csw)),
        }
    }

    fn start(self: &Arc<Self>, i: usize) {
        if !self.enter(i, Stage::Cbw, CBW_TIMEOUT_NS) {
            return self.finish();
        }
        if !self.small.write(0, &self.cmds[i].cbw) {
            return self.fail_stage(XferResult::failed(UsbError::NoMemory, 0));
        }
        self.submit(self.ep_out, Request::new(self.small.clone(), 0, 31), move |c, r| c.on_cbw(i, r));
    }

    fn on_cbw(self: &Arc<Self>, i: usize, r: XferResult) {
        self.record(i, Stage::Cbw, r, [0; 13]);
        if r.error.is_some() || r.actual != 31 {
            return self.finish();
        }
        match self.cmds[i].data.clone() {
            Some(req) => {
                if !self.enter(i, Stage::Data, self.cmds[i].data_timeout_ns) {
                    return self.finish();
                }
                let ep = if self.cmds[i].dir == Dir::In { self.ep_in } else { self.ep_out };
                self.submit(ep, req, move |c, r| c.on_data(i, r));
            }
            None => self.csw(i),
        }
    }

    fn on_data(self: &Arc<Self>, i: usize, r: XferResult) {
        self.record(i, Stage::Data, r, [0; 13]);
        if r.error.is_some() || r.actual != self.cmds[i].data_len() {
            return self.finish();
        }
        self.csw(i);
    }

    fn csw(self: &Arc<Self>, i: usize) {
        if !self.enter(i, Stage::Csw, CSW_TIMEOUT_NS) {
            return self.finish();
        }
        self.submit(self.ep_in, Request::new(self.small.clone(), 32, 13), move |c, r| c.on_csw(i, r));
    }

    fn on_csw(self: &Arc<Self>, i: usize, r: XferResult) {
        let mut b = [0u8; 13];
        let _ = self.small.read(32, &mut b[..r.actual.min(13)]);
        self.record(i, Stage::Csw, r, b);
        let clean = {
            let s = self.st.lock();
            bot::clean(&self.cmds[i], &s.results[i])
        };
        if clean && i + 1 < self.cmds.len() {
            self.start(i + 1);
        } else {
            self.finish();
        }
    }
}

impl Link for UsbLink {
    fn name(&self) -> String {
        format!("{} if{}", self.dev.path(), self.iface)
    }
    fn alloc(&self, len: usize) -> Result<Buffer, UsbError> {
        self.dev.alloc_buffer(len)
    }
    fn bulk(&self, dir_in: bool, req: Request, timeout_ns: u64) -> XferResult {
        self.dev.transfer(if dir_in { self.ep_in } else { self.ep_out }, req, timeout_ns)
    }
    fn clear_halt(&self, dir_in: bool) -> Result<(), UsbError> {
        self.dev.clear_halt(if dir_in { self.ep_in } else { self.ep_out })
    }
    fn abort_all(&self) {
        let _ = self.dev.abort(self.ep_in);
        let _ = self.dev.abort(self.ep_out);
    }
    fn bot_reset(&self) -> Result<(), UsbError> {
        self.dev.control_out(Setup::new(0x21, 0xFF, 0, self.iface as u16, 0), &[]).map(|_| ())
    }
    fn max_lun(&self) -> Result<u8, UsbError> {
        let mut b = [0u8; 1];
        match self.dev.control_in(Setup::new(0xA1, 0xFE, 0, self.iface as u16, 1), &mut b)? {
            0 => Err(UsbError::Invalid("empty GET MAX LUN answer")),
            _ => Ok(b[0]),
        }
    }
    fn gone(&self) -> bool {
        self.dev.is_gone()
    }
    fn now_ns(&self) -> u64 {
        self.dev.host().now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        self.dev.host().sleep_ns(ns)
    }
    fn take_poisoned(&self) -> bool {
        self.poisoned.swap(false, Ordering::AcqRel)
    }

    fn run(&self, cmds: &[Cmd]) -> Vec<Ran> {
        if cmds.is_empty() {
            return Vec::new();
        }
        RUNS.inc();
        let small = self.small.lock().clone();
        let chain = Arc::new(Chain {
            dev: self.dev.clone(),
            ep_in: self.ep_in,
            ep_out: self.ep_out,
            small,
            cmds: cmds.to_vec(),
            st: SpinLock::named("usb.msd.chain", ChainState {
                results: Vec::new(),
                done: false,
                timed_out: false,
                progress: 0,
                stage: (0, Stage::Cbw),
                stage_timeout: CBW_TIMEOUT_NS,
            }),
            wq: WaitQueue::new(),
        });
        chain.start(0);
        // Only `done` wakes the waiter (one wake-up per run). When a stage's time is up the waiter looks whether
        // anything moved meanwhile and waits again for the stage now under way; nothing moved = timeout.
        let mut timed_out = false;
        loop {
            let (p0, t) = {
                let s = chain.st.lock();
                (s.progress, s.stage_timeout)
            };
            if chain.wq.wait_until_timeout(|| chain.st.lock().done, t) {
                break;
            }
            if chain.st.lock().progress != p0 {
                continue;
            }
            timed_out = true;
            break;
        }
        if timed_out {
            let (i, stage) = {
                let mut s = chain.st.lock();
                s.timed_out = true;
                s.stage
            };
            STAGE_TIMEOUTS.inc();
            klog::event!(Warn, "usb", "msd.stage_timeout", device = self.name(), command = i, stage = format!("{stage:?}"),
                commands = cmds.len());
            self.abort_all();
            if !chain.wq.wait_until_timeout(|| chain.st.lock().done, ABORT_GRACE_NS) {
                // The callbacks never came back: give this run up and never share its buffer again.
                klog::event!(Error, "usb", "msd.run_abandoned", device = self.name(), command = i);
                if let Ok(b) = self.dev.alloc_buffer(64) {
                    *self.small.lock() = b;
                }
                self.poisoned.store(true, Ordering::Release);
                chain.st.lock().done = true;
            }
            // The stage that ran out of time reports a timeout (it may say Cancelled from the abort).
            chain.record_timeout(i, stage);
        }
        let s = chain.st.lock();
        s.results.clone()
    }
}

impl Chain {
    fn record_timeout(&self, i: usize, stage: Stage) {
        let mut s = self.st.lock();
        while s.results.len() <= i {
            s.results.push(Ran::cbw_only(XferResult::failed(UsbError::Timeout, 0)));
        }
        let ran = &mut s.results[i];
        // A stage that completed by itself (the abort came too late to matter) keeps its result.
        let to = |r: XferResult| if r.error.is_none() { r } else { XferResult::failed(UsbError::Timeout, r.actual) };
        match stage {
            Stage::Cbw => ran.cbw = to(ran.cbw),
            Stage::Data => ran.data = Some(ran.data.map_or(XferResult::failed(UsbError::Timeout, 0), to)),
            Stage::Csw => ran.csw = Some(ran.csw.map_or((XferResult::failed(UsbError::Timeout, 0), [0; 13]), |(r, b)| (to(r), b))),
        }
    }
}
