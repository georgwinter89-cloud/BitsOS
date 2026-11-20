//! Wave 43: the engines stopped as Linux stops them, for a Reload: `sdma_v7_0_hw_fini` (sdma_v7_0.c:1422-1434)
//! is `sdma_v7_0_enable(adev, false)` (:444-462), which is `sdma_v7_0_gfx_stop` (:397-410: each engine's ring and
//! IBs off) and then the halt of each engine's MCU, whatever state the rings are in. A running engine reads its
//! ring and writes its slots in system pages of the kernel that started it: the next kernel must not find it
//! running, nor an MCU that the stage unhalted and then left without a ring.
//!
//! Not Linux's: the stop touches the block only once the stage `sdma` set out to write its registers in this
//! boot (`Written`). Before that the GC block may be without power, and a boot that never started the engines
//! leaves the block unread and unwritten.

use super::ihoff::OffIo;
use super::sdmaregs::SdmaRegs;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use rdna4_core::sdma7::{self, INSTANCES};

/// The registers the stop touches, as BAR5 bytes: per engine (RB_CNTL, IB_CNTL, MCU_CNTL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffRegs(pub [(u32, u32, u32); 2]);

impl OffRegs {
    pub fn of(r: &SdmaRegs) -> Option<OffRegs> {
        let one = |i| Some((r.byte(sdma7::RB_CNTL, i)?, r.byte(sdma7::IB_CNTL, i)?, r.byte(sdma7::MCU_CNTL, i)?));
        Some(OffRegs([one(0)?, one(1)?]))
    }
}

/// The stage's word to the Reload hook: set right before the stage's first access to a register of the block,
/// read by the hook without a lock (a Reload may come while the stage runs).
#[derive(Clone, Debug, Default)]
pub struct Written(Arc<AtomicBool>);

impl Written {
    pub fn set(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// What the stop did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The stage never set out to write the block in this boot: nothing read, nothing written.
    Untouched,
    /// Both engines' rings and IBs off, both halted.
    Halted,
    /// A register did not read or take its write: the stop ended there.
    Failed,
}

pub fn stop(io: &mut dyn OffIo, r: &OffRegs, written: &Written) -> Stopped {
    if !written.get() {
        return Stopped::Untouched;
    }
    let mut change = |byte: u32, f: &dyn Fn(u32) -> u32| match io.read(byte) {
        Some(v) if v != u32::MAX => io.write(byte, f(v)),
        _ => false,
    };
    // sdma_v7_0_gfx_stop, engine by engine, then the halts.
    for i in 0..INSTANCES as usize {
        let (rb, ib, _) = r.0[i];
        if !change(rb, &sdma7::rb_cntl_stopped) || !change(ib, &|v| sdma7::ib_cntl(v, false)) {
            return Stopped::Failed;
        }
    }
    for (_, _, mcu) in r.0 {
        if !change(mcu, &|v| sdma7::mcu_halt(v, true)) {
            return Stopped::Failed;
        }
    }
    Stopped::Halted
}
