//! GX-9b on the chip: the addresses from the discovery copy, the start beside the node, and the Quiesce hook. The PSP's
//! hook halts engines through GC's registers, so GC must be awake first. Hooks run newest first and this one is
//! registered after the PSP's (a backend needs a passed bring-up), so it runs before it. devmodel has no way to take a hook
//! back, so there is one for the boot, and each start points it at its own flags.

use crate::dcn::DcnRegs;
use crate::dpm::{self, DpmCfg, Flags, Wire};
use crate::gpu::Gpu;
use crate::gpucore::disc;
use crate::gpucore::gfxup::GcAddrs;
use crate::gpucore::psp::Hw;
use crate::gpucore::smu::{self, SmuTimeouts};
use crate::hw::HardwareGpu;
use crate::ipmod::smu13::{Addrs, msg};
use crate::{probe, state};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;

fn wire(g: &Gpu) -> Result<Wire, String> {
    let found = probe::kept().map(Ok).unwrap_or_else(|| probe::fetch_indirect(g.regs))?;
    let d = disc::parse(&found.bytes).map_err(|w| format!("no discovery table: {w}"))?;
    let smu = Addrs::from_discovery(&d).ok_or("no MP1 or NBIF bases")?;
    let gc = GcAddrs::from_discovery(&d).ok_or("no GC bases")?;
    Ok(Wire { smu, gc })
}

/// Beside the node's start: clocks by load, GFXOFF, the hook. Nothing here can fail the node.
pub fn start(gpu: &Arc<HardwareGpu>) {
    let Some(g) = Gpu::get() else { return };
    let w = match wire(g) {
        Ok(w) => w,
        Err(why) => {
            klog::event!(Error, "gx", "dpm.no_wire", why = why.as_str(); "no clocks or GFXOFF in this run");
            return;
        }
    };
    if let Some(d) = gpu.start_dpm(w, DpmCfg { thread: true }) {
        if HOOK.point(w, d.flags()) {
            devmodel::register_quiesce(Arc::new(DpmQuiesce { regs: g.regs }));
        }
        // The run that started the backend entered with no block to hold: it holds it from here on.
        crate::hw::live::TOOL.hold(d.awake());
    }
}

/// BAR5 for the hook: interrupts off, no lock, every wait bounded and busy.
struct Raw(&'static DcnRegs);

impl Hw for Raw {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        state::wide_readable(byte).then(|| self.0.read(byte).ok()).flatten()
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.0.write_raw(byte, value).is_ok()
    }
    fn mem_read(&mut self, _mc: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn mem_write(&mut self, _mc: u64, _data: &[u8]) -> bool {
        false
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    fn delay_us(&mut self, us: u64) {
        time::delay_us(us);
    }
}

/// What the boot's one hook works on: the last start's.
pub(crate) struct Hook {
    hooked: AtomicBool,
    now: SpinLock<Option<(Wire, Arc<Flags>)>>,
}

static HOOK: Hook = Hook::new();

impl Hook {
    pub(crate) const fn new() -> Hook {
        Hook { hooked: AtomicBool::new(false), now: SpinLock::named("amdgpu.dpm.hook", None) }
    }

    /// True the first time: the hook is still to be registered.
    pub(crate) fn point(&self, w: Wire, f: Arc<Flags>) -> bool {
        let old = self.now.lock().replace((w, f));
        drop(old);
        !self.hooked.swap(true, Ordering::AcqRel)
    }

    /// No waiting: a CPU parked inside `point` leaves nothing it could safely read.
    pub(crate) fn run(&self, hw: &mut dyn Hw) {
        if let Some(g) = self.now.try_lock()
            && let Some((w, f)) = g.as_ref()
        {
            quiesce(hw, w, f);
        }
    }
}

struct DpmQuiesce {
    regs: &'static DcnRegs,
}

impl devmodel::Quiesce for DpmQuiesce {
    fn name(&self) -> &str {
        "amdgpu.dpm"
    }
    fn quiesce(&self) {
        HOOK.run(&mut Raw(self.regs));
    }
}

/// Linux's way out: Disallow, then the RLC's part off; only what this run left set. Testable on the fakes.
pub(crate) fn quiesce(hw: &mut dyn Hw, w: &Wire, f: &Flags) {
    if f.asleep.load(Ordering::Acquire) {
        match smu::send_quiet(hw, &w.smu, SmuTimeouts::QUIESCE, msg::DISALLOW_GFXOFF, 0) {
            Ok(_) => {
                f.asleep.store(false, Ordering::Release);
                let _ = dpm::settled(hw, &w.gc);
            }
            Err(_) => return,
        }
    }
    if f.pg_on.load(Ordering::Acquire) && dpm::cntl_pg(hw, &w.gc, false).is_ok() {
        f.pg_on.store(false, Ordering::Release);
    }
}
