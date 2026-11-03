//! PSP 13.0.4 (MP0 of Phoenix/Hawk Point): Linux `psp_v13_0_4` (linux-6.6 amdgpu/psp_v13_0_4.c). On this APU the
//! secure OS already runs when the OS starts (the BIOS loaded it; Linux's bootloader steps only run when
//! C2PMSG_81 reads 0), so the driver's part is the ring: wait for C2PMSG_64 bit 31, ring address and size into
//! C2PMSG_69/70/71, the command into C2PMSG_64, wait for bit 31 again (psp_v13_0_4.c:223-288).
//!
//! GX-7a: the register half of the KM ring (`Mailbox13`: create, stop, write pointer); the protocol over it
//! (frames, command buffers, fences, TMR, firmware loading) is version-independent and lives in
//! `gpucore::psp`. GX-7d: the same code runs on the real PSP through `psplive` (the desk's `gx_bringup`).

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::disc::Discovery;
use crate::gpucore::fw::Need;
use crate::gpucore::hwid;
use crate::gpucore::psp::{self, Hw, Mailbox, PspError, Timeouts};

pub static MODULE: Module = Module {
    name: "psp13_0_4",
    role: Role::Psp,
    versions: &[v(13, 0, 4)],
    linux: "psp_v13_0_4",
    linux_also: &[],
    firmware: Firmware::Files { prefix: "psp", needs: &[Need::one("_toc"), Need::one("_ta")] },
    state: State::Planned("GX-7"),
};

/// (segment index, dword) in MP0's bases (mp_13_0_4_offset.h): read-only facts for the probe and GX-7.
pub const C2PMSG_64: (usize, u32) = (1, 0x80);
pub const C2PMSG_69: (usize, u32) = (1, 0x85);
pub const C2PMSG_70: (usize, u32) = (1, 0x86);
pub const C2PMSG_71: (usize, u32) = (1, 0x87);
/// Non-zero once the secure OS runs (`psp_v13_0_4_is_sos_alive`).
pub const C2PMSG_81: (usize, u32) = (1, 0x91);
/// Bit 31: the bootloader is ready (`psp_v13_0_4_wait_for_bootloader`; only used when the sOS is not running).
pub const C2PMSG_35: (usize, u32) = (1, 0x63);
/// The KM ring's write pointer in dwords (`psp_v13_0_4_ring_get_wptr/set_wptr`, bare metal).
pub const C2PMSG_67: (usize, u32) = (1, 0x83);
/// GX-7b: bit 31 = the firmware's (IFWI) init is done (psp_v13_0.c:143-146; the Mini-PC reads 0x80000000 cold).
pub const C2PMSG_33: (usize, u32) = (1, 0x61);

/// The mailbox registers as BAR5 byte offsets, resolved from MP0's discovery bases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addrs {
    pub c2pmsg_35: u32,
    pub c2pmsg_64: u32,
    pub c2pmsg_67: u32,
    pub c2pmsg_69: u32,
    pub c2pmsg_70: u32,
    pub c2pmsg_71: u32,
    pub c2pmsg_81: u32,
}

impl Addrs {
    /// From MP0 instance 0's segment bases; `None` when MP0 or its segment 1 is missing or an address overflows.
    pub fn from_discovery(d: &Discovery) -> Option<Addrs> {
        Self::with(|(idx, dw)| d.reg(hwid::MP0, 0, idx, dw)?.checked_mul(4))
    }

    /// From segment 1's base in dwords (the 780M's is 0x16000, Linux's sysfs view of 27.10.). Tests only (C1):
    /// the driver reads the bases from the discovery table.
    #[cfg(test)]
    pub fn from_segment1(base_dwords: u32) -> Option<Addrs> {
        Self::with(|(_, dw)| base_dwords.checked_add(dw)?.checked_mul(4))
    }

    fn with(at: impl Fn((usize, u32)) -> Option<u32>) -> Option<Addrs> {
        Some(Addrs {
            c2pmsg_35: at(C2PMSG_35)?,
            c2pmsg_64: at(C2PMSG_64)?,
            c2pmsg_67: at(C2PMSG_67)?,
            c2pmsg_69: at(C2PMSG_69)?,
            c2pmsg_70: at(C2PMSG_70)?,
            c2pmsg_71: at(C2PMSG_71)?,
            c2pmsg_81: at(C2PMSG_81)?,
        })
    }
}

/// The KM ring's register interface of PSP 13.0.4 on bare metal (the SR-IOV path with C2PMSG_101..103 is not
/// BitsOS's).
#[derive(Clone, Copy, Debug)]
pub struct Mailbox13 {
    pub addrs: Addrs,
}

impl Mailbox for Mailbox13 {
    fn sos(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        psp::read(hw, self.addrs.c2pmsg_81)
    }

    /// psp_v13_0_4.c:256-285: wait for bit 31 (sOS ready for ring creation), address and size, the command
    /// `ring_type << 16`, the 20 ms settle Linux keeps for a handshake issue, then bit 31 with status 0.
    fn ring_create(&self, hw: &mut dyn Hw, t: &Timeouts, mc: u64, size: u32) -> Result<(), PspError> {
        let a = self.addrs;
        psp::wait_for(hw, t, a.c2pmsg_64, psp::FLAG_RESPONSE, psp::FLAG_RESPONSE, "ring.ready")?;
        psp::write(hw, a.c2pmsg_69, mc as u32)?;
        psp::write(hw, a.c2pmsg_70, (mc >> 32) as u32)?;
        psp::write(hw, a.c2pmsg_71, size)?;
        psp::write(hw, a.c2pmsg_64, psp::RING_TYPE_KM << 16)?;
        hw.delay_us(t.settle_us);
        // Linux waits for exactly 0x80000000 under mask 0x8000FFFF: a non-zero status never matches and ends
        // in -ETIME there; BitsOS stops at the first answer and names the status.
        let v = psp::wait_for(hw, t, a.c2pmsg_64, psp::FLAG_RESPONSE, psp::FLAG_RESPONSE, "ring.create")?;
        match (v & psp::STATUS_MASK) as u16 {
            0 => Ok(()),
            status => Err(PspError::RingStatus { status }),
        }
    }

    /// psp_v13_0_4.c:209-216: DESTROY_RINGS, settle, wait for bit 31.
    fn ring_stop(&self, hw: &mut dyn Hw, t: &Timeouts) -> Result<(), PspError> {
        psp::write(hw, self.addrs.c2pmsg_64, psp::CTRL_DESTROY_RINGS)?;
        hw.delay_us(t.settle_us);
        psp::wait_for(hw, t, self.addrs.c2pmsg_64, psp::FLAG_RESPONSE, psp::FLAG_RESPONSE, "ring.stop").map(|_| ())
    }

    fn wptr(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        psp::read(hw, self.addrs.c2pmsg_67)
    }

    fn set_wptr(&self, hw: &mut dyn Hw, dwords: u32) -> Result<(), PspError> {
        psp::write(hw, self.addrs.c2pmsg_67, dwords)
    }

    fn ring_size_reg(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        psp::read(hw, self.addrs.c2pmsg_71)
    }

    fn regs(&self, hw: &mut dyn Hw) -> alloc::vec::Vec<(&'static str, Option<u32>)> {
        let a = self.addrs;
        [("C2PMSG_64", a.c2pmsg_64), ("C2PMSG_67", a.c2pmsg_67), ("C2PMSG_69", a.c2pmsg_69), ("C2PMSG_70", a.c2pmsg_70), ("C2PMSG_71", a.c2pmsg_71), ("C2PMSG_81", a.c2pmsg_81)]
            .into_iter()
            .map(|(n, b)| (n, hw.reg_read(b)))
            .collect()
    }
}

impl Addrs {
    /// The mailbox registers the KM ring writes (create, destroy, write pointer): the live owner's write list.
    pub fn writable(&self) -> [u32; 5] {
        [self.c2pmsg_64, self.c2pmsg_67, self.c2pmsg_69, self.c2pmsg_70, self.c2pmsg_71]
    }

    /// Every mailbox register the PSP code reads.
    pub fn readable(&self) -> [u32; 7] {
        [self.c2pmsg_35, self.c2pmsg_64, self.c2pmsg_67, self.c2pmsg_69, self.c2pmsg_70, self.c2pmsg_71, self.c2pmsg_81]
    }
}
