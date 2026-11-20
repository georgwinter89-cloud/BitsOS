//! Wave 44, item ME: MES halted as Linux halts it, for a Reload: `mes_v12_0_enable(adev, false)`
//! (mes_v12_0.c:1356-1368), the last step of `mes_v12_0_kiq_hw_fini` (:2018-2034): CP_MES_CNTL read, both pipes
//! inactive, the instruction cache invalidated, both pipes' resets and HALT set. A running MES reads its rings and
//! writes its slots in system pages of the kernel that started it: the next kernel must not find it running.
//!
//! Not Linux's: Linux first unmaps the scheduler's queue through the KIQ pipe (`amdgpu_mes_unmap_legacy_queue`
//! with RESET_QUEUES, :2022-2025), a command on a ring, which a Reload's hook (no lock, no allocation, nothing
//! that waits on memory) does not send; and the hook touches the block only once the stage `mes` set out to write
//! it in this boot (`Written`). The kernel queues MES mapped into the command processor's queues stay there until
//! the command processor is halted (`gfx_v12_0_cp_enable(adev, false)`): that is the command processor's hook
//! (`cpoff`), which runs right after this one. Linux's `gfx_v12_0_hw_fini` halts MES first
//! (`amdgpu_mes_kiq_hw_fini`, gfx_v12_0.c:3902) and the command processor after it (:3914); the command
//! processor's hook is registered at `gccp`, before this one at `mes`, and hooks run newest first, so a Reload
//! keeps that order. Neither hook sends the kernel queues' unmap first (`amdgpu_gfx_disable_kgq/kcq`, :3895-3900):
//! a command on a ring (plan/spec/gpu-rdna4.md section 7, row `mes-reload`).

use super::ihoff::OffIo;
use super::nbregs::NbRegs;
use rdna4_core::mes12;

pub use super::sdmaoff::Written;

/// What the halt did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Halted {
    /// The stage never set out to write MES in this boot: nothing read, nothing written.
    Untouched,
    /// CP_MES_CNTL was read and written halted.
    Halted { was: u32 },
    /// CP_MES_CNTL did not read or take its write.
    Failed,
}

pub fn halt(io: &mut dyn OffIo, n: &NbRegs, written: &Written) -> Halted {
    if !written.get() {
        return Halted::Untouched;
    }
    let Some(b) = n.byte(mes12::CP_MES_CNTL) else { return Halted::Failed };
    match io.read(b) {
        Some(v) if v != u32::MAX && io.write(b, mes12::halted(v)) => Halted::Halted { was: v },
        _ => Halted::Failed,
    }
}
