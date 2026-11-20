//! A register-level fake: byte-addressed memory, the two OCP windows, self-clearing reset. It panics on any write the
//! crate is not allowed to make, so every test that plays anything also checks the allow list.

use crate::hw::Hw;
use crate::ocp::OCP_FLAG;
use crate::regs::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    W8(u16, u8),
    W16(u16, u16),
    W32(u16, u32),
    Delay(u32),
    CfgW(u16, u8),
    ReadRq,
}

/// Locally administered, not a real card's.
pub const TEST_MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

#[derive(Default)]
pub struct State {
    pub mem: BTreeMap<u16, u8>,
    pub ops: Vec<Op>,
    pub mac_ocp: BTreeMap<u32, u16>,
    pub phy_ocp: BTreeMap<u32, u16>,
    pub cfg: BTreeMap<u16, u8>,
    pub no_cfg: bool,
    pub no_read_rq: bool,
    pub stuck_reset: bool,
    pub stuck_phy: bool,
    pub stuck_bmcr: bool,
    /// Each PHY access shows its result only at this look (0: at the first).
    pub slow_phy: u32,
    /// The window's value once the access is done, and the looks until then.
    phy_pending: Option<(u32, u32)>,
}

pub struct Fake {
    pub s: RefCell<State>,
    pub waits: crate::ocp::PhyWaits,
}

/// Addresses are even, so their bit 0 lands on bit 15 of the command, which belongs to the data.
fn reg_of(cmd: u32) -> u32 {
    ((cmd >> 16) & 0x7fff) << 1
}

impl Fake {
    pub fn new() -> Fake {
        let fake = Fake { s: RefCell::new(State::default()), waits: crate::ocp::PhyWaits::new() };
        fake.poke32(TX_CONFIG, 0x64a0_0000);
        fake.poke8(MCU, RXTX_EMPTY | LINK_LIST_RDY | NOW_IS_OOB);
        fake.poke16(INTR_MITIGATE, 0x0103);
        fake.poke16(CPLUS_CMD, 0x2061);
        for (i, b) in TEST_MAC.iter().enumerate() {
            fake.poke8(MAC0_BKP + i as u16, *b);
        }
        {
            let mut s = fake.s.borrow_mut();
            s.phy_ocp.insert(0xa400 + 2 * 2, 0x001c);
            s.phy_ocp.insert(0xa400 + 3 * 2, 0xc862);
            s.cfg.insert(0x890, 0xff);
        }
        fake
    }

    pub fn poke8(&self, off: u16, v: u8) {
        self.s.borrow_mut().mem.insert(off, v);
    }

    pub fn poke16(&self, off: u16, v: u16) {
        for (i, b) in v.to_le_bytes().iter().enumerate() {
            self.poke8(off + i as u16, *b);
        }
    }

    pub fn poke32(&self, off: u16, v: u32) {
        for (i, b) in v.to_le_bytes().iter().enumerate() {
            self.poke8(off + i as u16, *b);
        }
    }

    fn byte(&self, off: u16) -> u8 {
        self.s.borrow().mem.get(&off).copied().unwrap_or(0)
    }

    pub fn ops(&self) -> Vec<Op> {
        self.s.borrow().ops.clone()
    }

    pub fn position(&self, op: Op) -> usize {
        self.ops().iter().position(|o| *o == op).unwrap_or_else(|| panic!("{op:?} never happened"))
    }

    fn check(&self, off: u16, width: Width) {
        assert!(write_allowed(off, width), "write outside the allow list: {off:#x} {width:?}");
        assert!(!NEVER_WRITTEN.contains(&off), "write to a forbidden register: {off:#x}");
        if [MAC0, MAC4, CONFIG1, CONFIG3, CONFIG5].contains(&off) {
            assert_eq!(self.byte(CFG9346), CFG9346_UNLOCK, "{off:#x} written while locked");
        }
    }

    fn ocpdr(&self, cmd: u32) {
        let reg = reg_of(cmd);
        if cmd & OCP_FLAG != 0 {
            self.s.borrow_mut().mac_ocp.insert(reg, cmd as u16);
        } else {
            let data = self.s.borrow().mac_ocp.get(&reg).copied().unwrap_or(0);
            self.poke32(OCPDR, u32::from(data));
        }
    }

    fn gphy(&self, cmd: u32) {
        let reg = reg_of(cmd);
        let (stuck, stuck_bmcr) = {
            let s = self.s.borrow();
            (s.stuck_phy, s.stuck_bmcr)
        };
        if cmd & OCP_FLAG != 0 {
            let mut data = cmd as u16;
            if reg == 0xa400 && !stuck_bmcr {
                data &= !0x8000;
            }
            self.s.borrow_mut().phy_ocp.insert(reg, data);
            self.answer(if stuck { OCP_FLAG } else { 0 }, OCP_FLAG);
        } else {
            let data = self.s.borrow().phy_ocp.get(&reg).copied().unwrap_or(0);
            self.answer(u32::from(data) | if stuck { 0 } else { OCP_FLAG }, 0);
        }
    }

    /// The window shows `busy` for `slow_phy` looks and `done` from then on.
    fn answer(&self, done: u32, busy: u32) {
        let slow = self.s.borrow().slow_phy;
        self.s.borrow_mut().phy_pending = (slow > 0).then_some((done, slow));
        self.poke32(GPHY_OCP, if slow > 0 { busy } else { done });
    }

    /// One look at the window.
    fn look(&self) {
        let pending = self.s.borrow_mut().phy_pending.take();
        match pending {
            Some((done, 1)) => self.poke32(GPHY_OCP, done),
            Some((done, left)) => self.s.borrow_mut().phy_pending = Some((done, left - 1)),
            None => {}
        }
    }
}

impl Hw for Fake {
    fn rd8(&self, off: u16) -> u8 {
        self.byte(off)
    }
    fn rd16(&self, off: u16) -> u16 {
        u16::from_le_bytes([self.byte(off), self.byte(off + 1)])
    }
    fn rd32(&self, off: u16) -> u32 {
        let v = u32::from_le_bytes([self.byte(off), self.byte(off + 1), self.byte(off + 2), self.byte(off + 3)]);
        if off == GPHY_OCP {
            self.look();
        }
        v
    }
    fn wr8(&self, off: u16, val: u8) {
        self.check(off, Width::B1);
        if off == CFG9346 {
            assert!(val == CFG9346_LOCK || val == CFG9346_UNLOCK, "Cfg9346 = {val:#x} is the EEPROM interface");
        }
        self.s.borrow_mut().ops.push(Op::W8(off, val));
        let stuck = self.s.borrow().stuck_reset;
        let stored = if off == CHIP_CMD && !stuck { val & !CMD_RESET } else { val };
        self.poke8(off, stored);
    }
    fn wr16(&self, off: u16, val: u16) {
        self.check(off, Width::B2);
        self.s.borrow_mut().ops.push(Op::W16(off, val));
        self.poke16(off, val);
    }
    fn wr32(&self, off: u16, val: u32) {
        self.check(off, Width::B4);
        self.s.borrow_mut().ops.push(Op::W32(off, val));
        match off {
            OCPDR => self.ocpdr(val),
            GPHY_OCP => self.gphy(val),
            _ => self.poke32(off, val),
        }
    }
    fn delay_us(&self, us: u32) {
        self.s.borrow_mut().ops.push(Op::Delay(us));
    }
    fn cfg_rd8(&self, off: u16) -> Option<u8> {
        let s = self.s.borrow();
        if s.no_cfg { None } else { Some(s.cfg.get(&off).copied().unwrap_or(0)) }
    }
    fn cfg_wr8(&self, off: u16, val: u8) -> bool {
        assert!(off == 0x890 || off == 0x70f, "config write outside the two known bytes: {off:#x}");
        let mut s = self.s.borrow_mut();
        if s.no_cfg {
            return false;
        }
        s.cfg.insert(off, val);
        s.ops.push(Op::CfgW(off, val));
        true
    }
    fn set_max_read_request_4096(&self) -> bool {
        let mut s = self.s.borrow_mut();
        s.ops.push(Op::ReadRq);
        !s.no_read_rq
    }
    fn phy_waits(&self) -> Option<&crate::ocp::PhyWaits> {
        Some(&self.waits)
    }
}
