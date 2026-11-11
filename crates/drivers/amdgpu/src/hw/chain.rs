//! GX-8b: the kept chain as the job half sees it: one owner behind one lock, one short step at a time. The live chain
//! is `psplive::LIVE`, the fake chain the tests' owner; the ring code names neither. The owner stays
//! `Bringup<Watch<..>>`, so every memory and register write of a job still passes the GX-D9 watch.

use crate::gpucore::bringup::Bringup;
use crate::gpucore::mes::{self, MesAddrs};
use crate::gpucore::psp::Hw;
use crate::gpucore::vmrecover;
use crate::gpucore::vmring::{self, Queue, VmAddrs, VmInputs, VmReport};
use crate::gpucore::watch::Watch;
use crate::ipmod::mes11::{M, f};
use alloc::string::String;
use alloc::vec::Vec;

/// What a compute queue's rebuild left.
pub(crate) struct Rebuilt {
    pub queue: Result<Queue, &'static str>,
    pub detail: String,
    /// No firmware owns the queue's HQD any more (the tool must not start the CP in this boot).
    pub stranded: bool,
}

/// What a GC re-init brings back: the chain from the PSP on, its queues found idle.
pub(crate) struct Reinit {
    pub vmi: VmInputs,
    pub queues: [Queue; 3],
    /// The DCN snapshot from before the recovery still matches (GX-D9).
    pub display_equal: bool,
}

pub(crate) trait ChainOwner {
    fn io(&mut self) -> &mut dyn Hw;
    /// Clears GFXHUB's fault registers; the status after.
    fn clear_fault(&mut self, a: &VmAddrs, us: u64) -> Option<u32>;
    /// The kernel compute ring off its pipe through MES and back on it, its memory rewritten (`vmrecover::compute`).
    fn rebuild_compute(&mut self, inp: &VmInputs) -> Rebuilt;
    /// GX-9a: the same of GFX ring 0 (`vmrecover::gfx`).
    fn rebuild_gfx(&mut self, inp: &VmInputs) -> Rebuilt;
    /// GX-9a: SDMA 0 stopped and started (`vmrecover::sdma_queue`).
    fn restart_sdma(&mut self, inp: &VmInputs) -> Rebuilt;
    /// GX-9a: one SQ_CMD kill of the waves in `vmid` (gfx_v11_0.c:5693); false: the write did not land.
    fn kill_waves(&mut self, a: &MesAddrs, vmid: u8) -> bool;
    /// Writes the GX-D9 watch stopped or saw.
    fn display_writes(&mut self) -> u64;
}

impl<H: Hw> ChainOwner for Bringup<Watch<H>> {
    fn io(&mut self) -> &mut dyn Hw {
        self
    }
    fn clear_fault(&mut self, a: &VmAddrs, us: u64) -> Option<u32> {
        vmring::clear_fault(self, a, us).ok().flatten()
    }
    fn rebuild_compute(&mut self, inp: &VmInputs) -> Rebuilt {
        let mut r = VmReport::default();
        let mut detail = String::new();
        let queue = vmrecover::compute(self, inp, &mut r, &mut detail);
        Rebuilt { queue, detail, stranded: r.stranded }
    }
    fn rebuild_gfx(&mut self, inp: &VmInputs) -> Rebuilt {
        let mut r = VmReport::default();
        let mut detail = String::new();
        let queue = vmrecover::gfx(self, inp, &mut r, &mut detail);
        Rebuilt { queue, detail, stranded: r.stranded }
    }
    fn restart_sdma(&mut self, inp: &VmInputs) -> Rebuilt {
        let mut detail = String::new();
        let queue = vmrecover::sdma_queue(self, inp, &mut detail);
        Rebuilt { queue, detail, stranded: false }
    }
    fn kill_waves(&mut self, a: &MesAddrs, vmid: u8) -> bool {
        mes::wr(self, a, M::SqCmd, f::sq_kill(vmid)).is_ok()
    }
    fn display_writes(&mut self) -> u64 {
        self.hw().total()
    }
}

/// The owner behind its lock. Every wait for the lock is bounded; `with` answers false when it ran nothing.
///
/// GX-9a: the full reset's steps run on a thread of their own and are no single step of the owner: the bring-up replaces it.
/// Each writes its lines and answers for itself; the defaults are a chain with no full reset.
pub(crate) trait Chain: Send + Sync {
    fn with(&self, f: &mut dyn FnMut(&mut dyn ChainOwner)) -> bool;
    /// See `Rebuilt::stranded`.
    fn note_stranded(&self) {}
    /// Linux's IP suspend before mode 2 (amdgpu_device.c:4772): every engine halted, GC unloaded, the PSP's rings stopped.
    /// True: every engine is proven halted.
    fn suspend_gc(&self, lines: &mut Vec<String>) -> bool {
        lines.push(String::from("reset: this chain has no suspend"));
        false
    }
    /// GfxDeviceDriverReset(2), behind `gx_mode2=on` (GX-D9). The picture still equal, or None: no answer, or refused.
    fn mode2_reset(&self, lines: &mut Vec<String>) -> Option<bool> {
        lines.push(String::from("reset: this chain has no mode 2"));
        None
    }
    /// GC's bring-up again, from the PSP: a stop first if the chain still runs. None: a stage failed.
    fn reinit_gc(&self, lines: &mut Vec<String>) -> Option<Reinit> {
        lines.push(String::from("reset: this chain has no re-init"));
        None
    }
    /// `true`: the ladder went on without the full reset step in progress, which must end at its next look and start nothing
    /// new. `false`: a new step starts (no thread of an earlier one still runs).
    fn reset_abandoned(&self, _gone: bool) {}
    /// A full reset passed, with a ring test on every queue: whatever an earlier stop left running is gone.
    fn proven_halted(&self) {}
    /// A hang ended the device for this boot.
    fn device_lost(&self) {}
}
