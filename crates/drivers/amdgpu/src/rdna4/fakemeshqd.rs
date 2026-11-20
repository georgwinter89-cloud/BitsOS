//! Wave 44, item ME: the model's KIQ pipe queue as `mes_v12_0_queue_init_register` (mes_v12_0.c:1595-1655) writes
//! it: every HQD register under the KIQ pipe's selection, in Linux's order, with Linux's values where they are no
//! address; CP_HQD_ACTIVE only on an MQD that GFXHUB's walker finds and that holds what the registers say
//! (`mes_v12_0_mqd_init`). The queue then runs from the MQD's ring and slots (`queue_of`).

use super::fakemes::Q;
use super::fakemodel::Model;
use alloc::format;
use alloc::vec::Vec;
use rdna4_core::mes12::{self, Pipe, f};
use rdna4_core::mqd12::{self, c};
use rdna4_core::regmap::Reg;

impl Model {
    /// The KIQ pipe's HQD registers (`mes_v12_0_queue_init_register`), and CP_HQD_GFX_CONTROL.
    pub(super) fn mes_hqd(&mut self, byte: u32, v: u32, name: &str) -> bool {
        if self.mes_is(byte, mes12::CP_HQD_GFX_CONTROL) {
            if self.gc.grbm != 0 || v != mes12::HQD_GFX_CONTROL || self.mes.hw[Pipe::Sched.index()].is_none() {
                return self.refuse(format!("CP_HQD_GFX_CONTROL written {v:#x}: Linux writes DB_UPDATED_MSG_EN alone under selection 0, after the aggregated doorbells (mes_v12_0.c:1070-1071)"));
            }
            self.mes.gfx_control = v;
            return true;
        }
        if self.mes_selected() != Some(Pipe::Kiq) {
            return self.refuse(format!("{name} written {v:#x} while GRBM_GFX_CNTL selects {:#x}: Linux writes the HQD registers of the KIQ pipe only, under its selection (mes_v12_0.c:1599)", self.gc.grbm));
        }
        let order: Vec<u32> = mes12::queue_init_register(&ZERO).iter().chain(&mes12::queue_init_tail(&ZERO)).filter_map(|op| self.mes_byte(op.reg())).collect();
        let at = self.mes.hqd_order.len();
        // The doorbell control is written twice: off at step 2, with its doorbell at step 13.
        if order.get(at) != Some(&byte) {
            return self.refuse(format!("{name} written as the KIQ queue's register number {} where Linux writes {} (mes_v12_0.c:1595-1655)", at + 1, order.get(at).and_then(|b| super::linuxseq::name_of(b / 4)).unwrap_or("nothing")));
        }
        let want = match at {
            0 | 4 => Some(0),
            1 => Some(0),
            9 => Some(mes12::mes_pq_control(mes12::RING_BYTES)),
            12 => Some(mes12::doorbell_enabled(Pipe::Kiq.doorbell())),
            13 => Some(mes12::persistent_state()),
            14 => Some(1),
            _ => None,
        };
        if want.is_some_and(|w| w != v) {
            return self.refuse(format!("{name} written {v:#x} where Linux writes {:#x}", want.unwrap_or(0)));
        }
        if at == 0 && !self.mes_settled() {
            return self.refuse(format!("{name} written {v:#x} before 500 us passed since both MES pipes started: Linux waits that long before the KIQ pipe's queue (udelay(500) with uni_mes, mes_v12_0.c:1353-1356)"));
        }
        if at == 14 && !self.mes_kiq_active() {
            return false;
        }
        self.mes.hqd_order.push(byte);
        self.mes.hqd.insert(byte, v);
        true
    }

    /// CP_HQD_ACTIVE: the KIQ pipe's queue starts from its MQD, which must be bound and hold the registers' values.
    fn mes_kiq_active(&mut self) -> bool {
        let reg = |m: &Model, r: Reg| m.mes_byte(r).and_then(|b| m.mes.hqd.get(&b)).copied().unwrap_or(0);
        let mqd = u64::from(reg(self, mes12::CP_MQD_BASE_ADDR_HI)) << 32 | u64::from(reg(self, mes12::CP_MQD_BASE_ADDR));
        let Some(m) = self.mes_mqd(mqd) else { return self.refuse(format!("the KIQ queue made active on an MQD at {mqd:#x} that GFXHUB's walker does not find")) };
        let checks = [
            (c::HEADER, mqd12::HEADER),
            (c::MQD_BASE_ADDR_LO, reg(self, mes12::CP_MQD_BASE_ADDR)),
            (c::HQD_PQ_BASE_LO, reg(self, mes12::CP_HQD_PQ_BASE)),
            (c::HQD_PQ_CONTROL, reg(self, mes12::CP_HQD_PQ_CONTROL)),
            (c::HQD_PQ_RPTR_REPORT_ADDR_LO, reg(self, mes12::CP_HQD_PQ_RPTR_REPORT_ADDR)),
            (c::HQD_PQ_WPTR_POLL_ADDR_LO, reg(self, mes12::CP_HQD_PQ_WPTR_POLL_ADDR)),
            (c::HQD_PQ_DOORBELL_CONTROL, mes12::doorbell_enabled(Pipe::Kiq.doorbell())),
            (c::HQD_PERSISTENT_STATE, mes12::persistent_state()),
        ];
        if let Some((at, want)) = checks.iter().find(|(at, want)| m[*at] != *want) {
            return self.refuse(format!("the KIQ queue made active on an MQD whose dword {at} holds {:#x}, not {want:#x} (mes_v12_0_mqd_init)", m[*at]));
        }
        let q = queue_of(&m, mqd);
        self.mes.queue[Pipe::Kiq.index()] = Some(q);
        true
    }

    /// An MQD as GFXHUB's walker finds it (512 dwords on one page).
    pub(super) fn mes_mqd(&self, mc: u64) -> Option<Vec<u32>> {
        let mut b = alloc::vec![0u8; mqd12::MQD_BYTES as usize];
        let sys = self.gc_walk(mc)?;
        if mc % rdna4_core::pte12::PAGE != 0 {
            return None;
        }
        self.nb.sys_peek(sys, &mut b);
        Some(b.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect())
    }

}

/// The values `queue_init_register` writes when only the order matters (a zero MQD's).
const ZERO: mqd12::HqdFromMqd = mqd12::HqdFromMqd { mqd_lo: 0, mqd_hi: 0, pq_base_lo: 0, pq_base_hi: 0, rptr_lo: 0, rptr_hi: 0, pq_control: 0, wptr_lo: 0, wptr_hi: 0, doorbell: 0, persistent: 0, active: 0 };

/// A pipe's queue from its MQD: the ring (`CP_HQD_PQ_BASE << 8`, 2^(QUEUE_SIZE+1) dwords), the slots.
pub(super) fn queue_of(m: &[u32], mqd_mc: u64) -> Q {
    let w = |lo: usize, hi: usize| u64::from(m[hi]) << 32 | u64::from(m[lo]);
    Q {
        ring_mc: w(c::HQD_PQ_BASE_LO, c::HQD_PQ_BASE_HI) << 8,
        ring_dw: 2u64 << f::QUEUE_SIZE.get(m[c::HQD_PQ_CONTROL]),
        wptr_mc: w(c::HQD_PQ_WPTR_POLL_ADDR_LO, c::HQD_PQ_WPTR_POLL_ADDR_HI),
        rptr_mc: w(c::HQD_PQ_RPTR_REPORT_ADDR_LO, c::HQD_PQ_RPTR_REPORT_ADDR_HI),
        doorbell: f::DOORBELL_OFFSET.get(m[c::HQD_PQ_DOORBELL_CONTROL]),
        mqd_mc,
        rptr: 0,
    }
}

