//! The model's bus: every register read and write, VRAM through the card's memory, the doorbells and the clock,
//! as the driver reaches the card through `Hw`. Each block's model answers for its own registers first
//! (`fakering`, `fakecp`, `fakenb`, `fakepmfw`, `fakegc`, `fakesdma`); what is left is the secure processor's and the SMU's mailbox
//! (`fakemodel`, `fakeboot`, `fakecmd`, `fakesmu`).

use super::fakemodel::{MC_BASE, Model, same, stack_at};
use crate::gpucore::psp::Hw;
use alloc::format;
use rdna4_core::refuse;
use rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS;

impl Hw for Model {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.deepest = self.deepest.min(stack_at());
        if let Some(v) = self.ring_read(byte) {
            return Some(v);
        }
        if let Some(v) = self.cp_read(byte) {
            return Some(v);
        }
        if let Some(v) = self.nb_read(byte).or_else(|| self.pmfw_read(byte)) {
            return Some(v);
        }
        if let Some(v) = self.gc_read(byte) {
            return Some(v);
        }
        if let Some(v) = self.sdma_read(byte) {
            return Some(v);
        }
        // The bytes by value: the bootloader's reads change the model.
        let p = self.regs.psp;
        let c36 = self.regs.c2pmsg_36;
        if byte == p.c2pmsg_81 {
            return Some(self.boot_read81());
        }
        if byte == p.c2pmsg_35 {
            return Some(self.boot_read35());
        }
        let r = &self.regs;
        Some(match byte {
            b if b == p.c2pmsg_64 => {
                // Linux's ready: 0x80000000 under 0x8000FFFF (psp_v14_0.c:302-305): a status is not ready.
                self.ready_seen |= self.c64 & 0x8000_FFFF == 0x8000_0000;
                self.c64
            }
            b if b == c36 => self.boot.c36,
            b if b == p.c2pmsg_67 => self.c67,
            b if b == p.c2pmsg_69 => self.c69,
            b if b == p.c2pmsg_70 => self.c70,
            b if b == p.c2pmsg_71 => self.c71,
            b if b == r.rsmu_index => self.index,
            b if b == r.rsmu_data => {
                if self.index == SMN_MP1_FIRMWARE_FLAGS {
                    self.smu_flags
                } else {
                    0
                }
            }
            b if b == r.smu_argument => self.c82,
            b if b == r.fb_base => (MC_BASE >> 24) as u32,
            _ => return self.card.reg_read(byte),
        })
    }

    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.deepest = self.deepest.min(stack_at());
        self.writes.push((byte, value));
        // The rings' registers, then the command processor's: its VMID selections and unhalts are writes of
        // `fakegc`'s registers.
        if let Some(reached) = self.ring_write(byte, value) {
            return reached;
        }
        if let Some(reached) = self.cp_write(byte, value) {
            return reached;
        }
        // Before `nb_write`: MMHUB's fault control with its clear bit is `fakegchub`'s.
        if let Some(reached) = self.gc_write(byte, value) {
            return reached;
        }
        if let Some(reached) = self.nb_write(byte, value) {
            return reached;
        }
        if let Some(reached) = self.sdma_write(byte, value) {
            return reached;
        }
        let Some(reg) = self.reg_of(byte) else { return self.refuse(format!("byte {byte:#x}: a register G2 does not know")) };
        // The bootloader's mailbox is the model's own state machine (`fakeboot`), not the driver's gate.
        if let Some(to) = refuse::bootloader_mailbox(reg) {
            return self.boot_write(to, value);
        }
        // Which message the SMU takes is the model's own power controller's to say (`fakesmu`, `fakepmfw`).
        if !same(reg, rdna4_core::regmap::SMU_MESSAGE)
            && let Some(why) = refuse::write_refused(reg, value)
        {
            return self.refuse(format!("{} {value:#x}: {why}", reg.name));
        }
        if reg.hw_id == rdna4_core::ipfacts::hw::MP0 {
            self.psp_write(reg, byte, value)
        } else {
            self.smu_write(reg, byte, value)
        }
    }

    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.deepest = self.deepest.min(stack_at());
        if at.saturating_add(buf.len() as u64) > self.vram_bytes() || self.pmfw_hides(at) {
            return false;
        }
        let start = (u64::from(self.card.memsize) << 20).saturating_sub(rdna4_core::layout::DISCOVERY_FROM_END);
        if at >= start {
            return self.card.mem_read(at, buf);
        }
        self.peek(at, buf);
        true
    }

    fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.deepest = self.deepest.min(stack_at());
        let end = at.saturating_add(data.len() as u64);
        if end > self.vram_bytes() {
            return self.refuse(format!("memory write at {at:#x} past VRAM"));
        }
        if at < self.picture.end && end > self.picture.start {
            return self.refuse(format!("memory write at {at:#x} into the firmware's picture"));
        }
        if let Some(why) = self.nb_mem_refused(at, data) {
            return self.refuse(why);
        }
        self.nb_mem_note(at, data);
        self.sdma_mem_note(at, data);
        self.poke(at, data);
        self.unflushed = true;
        true
    }

    fn store_barrier(&mut self) {
        (self.nb.barriers, self.nb.table_unfenced) = (self.nb.barriers + 1, false);
    }

    fn now_us(&mut self) -> u64 {
        self.card.now_us
    }

    fn delay_us(&mut self, us: u64) {
        self.card.now_us += us;
    }

    fn doorbell_write32(&mut self, index_dw: u32, value: u32) -> bool {
        self.nb_doorbell(index_dw, value)
    }

    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        // Below SDMA's first doorbell (BAR2 dword 0x200) they are the command processor's and MES's.
        if let Some(reached) = self.cp_doorbell(index_dw, value) {
            return reached;
        }
        self.sdma_doorbell(index_dw, value)
    }
}
