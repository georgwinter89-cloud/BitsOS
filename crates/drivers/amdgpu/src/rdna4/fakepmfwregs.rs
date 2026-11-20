//! Wave 43: the model's power controller beside its mailbox (`fakepmfw`): what its answer register and the
//! thermal alert's four registers read, the thermal alert's writes in Linux's order and with Linux's change only
//! (`smu_v14_0_set_irq_state`, ../pm/swsmu/smu14/smu_v14_0.c:754-797), and a test's driver table that does not
//! read out of VRAM.

use super::fakemodel::{MC_BASE, Model};
use super::fakepmfw::FakePmfw;
use alloc::format;
use rdna4_core::smu14;
use rdna4_core::thm14;

impl Model {
    /// Tests (`table_unreadable`): a memory read at the VRAM offset `at` is the driver table's and fails.
    pub(super) fn pmfw_hides(&self, at: u64) -> bool {
        self.pmfw.table_unreadable && FakePmfw::table_mc(self.pmfw.driver) == Some(MC_BASE + at)
    }

    /// A read of the answer register or of one of the thermal alert's registers.
    pub(super) fn pmfw_read(&mut self, byte: u32) -> Option<u32> {
        let r = &self.regs;
        if byte == r.smu_answer {
            if self.pmfw.wait > 0 {
                self.pmfw.wait -= 1;
                if self.pmfw.wait == 0 {
                    self.c90 = self.pmfw.answer;
                }
                return Some(0);
            }
            self.pmfw.seen_answer |= smu14::decode(self.c90).lets_send();
            return Some(self.c90);
        }
        let p = &self.pmfw;
        [(r.thm_int_ctrl, p.thm_ctrl), (r.thm_int_ena, p.thm_ena), (r.mp1_ih_sw_int, p.sw_int), (r.mp1_ih_sw_int_ctrl, p.sw_int_ctrl)].into_iter().find(|(b, _)| *b == byte).map(|(_, v)| v)
    }

    /// A write of one of the thermal alert's registers: `None` for any other byte.
    pub(super) fn pmfw_thermal(&mut self, byte: u32, v: u32) -> Option<bool> {
        // Wave 44: once the start's four went, only Linux's thermal handler writes them (`fakethermal`).
        if self.pmfw.thermal == 4 {
            return self.thm_write(byte, v);
        }
        let r = self.regs.clone();
        let (step, want, what) = match byte {
            b if b == r.thm_int_ctrl => (0, thm14::int_ctrl(self.pmfw.thm_ctrl, self.pmfw.table.software_shutdown_temp, smu14::THERMAL_MIN_MILLI), "THM_THERMAL_INT_CTRL"),
            b if b == r.thm_int_ena => (1, thm14::INT_ENA, "THM_THERMAL_INT_ENA"),
            b if b == r.mp1_ih_sw_int => (2, thm14::sw_int(self.pmfw.sw_int), "MP1_SMN_IH_SW_INT"),
            b if b == r.mp1_ih_sw_int_ctrl => (3, thm14::sw_int_ctrl(self.pmfw.sw_int_ctrl), "MP1_SMN_IH_SW_INT_CTRL"),
            _ => return None,
        };
        if !self.pmfw.enabled {
            return Some(self.refuse(format!("{what} written before the features are on (Linux enables the thermal alert after them, amdgpu_smu.c:1885)")));
        }
        if self.pmfw.thermal != step {
            return Some(self.refuse(format!("{what} written out of Linux's order (smu_v14_0.c:754-797): {} of the four went before it", self.pmfw.thermal)));
        }
        if v != want {
            return Some(self.refuse(format!("{what} written {v:#x}: Linux's change of what it holds is {want:#x}")));
        }
        match step {
            0 => self.pmfw.thm_ctrl = v,
            1 => self.pmfw.thm_ena = v,
            2 => self.pmfw.sw_int = v,
            _ => self.pmfw.sw_int_ctrl = v,
        }
        self.pmfw.thermal = step + 1;
        Some(true)
    }
}
