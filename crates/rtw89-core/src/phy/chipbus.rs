//! Host tests only: the JS test's chip model (js/phy-test-expect.mjs `ChipDev`) in Rust: a
//! [`MemBus`] that answers reads like the chip would (the XTAL SI register file from the
//! writes, the RF thermal through R_SWSI_V1, PHY status bitmaps, the RPL bias bytes, NCTL,
//! the C2H registers, efuse bytes), lets the first `rdy_fails` efuse RDY polls fail, and keeps
//! every untraced memory write (the H2C bytes in the CH12 buffers).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::bus::{Bus, BusError, BusResult, MemWidth, Width, field, prep};
use crate::membus::MemBus;
use crate::tables::regs::*;

use super::io::BB;

/// fake IE bitmap: bits 4..7 set, bits 1, 13, 20 clear
pub const STS_READ: u32 = 0x0f0f_00f0;
/// fake R_P0_RPL1 / R_P1_RPL1 bias bytes (-12, 16)
pub const RPL1: [u32; 2] = [0xf4, 0x10];
/// fake R_DCFO B_DCFO
pub const DCFO_READ: u32 = 5;
const XT: u32 = R_AX_WLAN_XTAL_SI_CTRL;
const C2H: [u32; 4] = [
    R_AX_C2HREG_DATA0,
    R_AX_C2HREG_DATA1,
    R_AX_C2HREG_DATA2,
    R_AX_C2HREG_DATA3,
];

type NctlFn = Box<dyn FnMut(u32) -> u32>;
type PollFn = Box<dyn FnMut(u32) -> bool>;

/// The model's knobs (ChipDev's `model`)
pub struct Model {
    /// XTAL SI bytes before any write (offset -> byte)
    pub xtal: BTreeMap<u32, u32>,
    /// what the RF thermal meter reads
    pub thermal: u32,
    /// 0x8080's answer to its n-th read
    pub nctl: NctlFn,
    pub nctl_reads: u32,
    /// how many efuse RDY polls fail first
    pub rdy_fails: u32,
    /// the C2H registers' words
    pub c2h: [u32; 4],
    /// the efuse contents the dumps read (None: every byte unprogrammed, the DAV map unused)
    pub efuse: Option<EfuseChip>,
}

/// The chip's efuse for the probe's dumps (Bringup::probe_efuse on this bus)
pub struct EfuseChip {
    /// the physical map R_AX_EFUSE_CTRL gives byte by byte (0xff past its end)
    pub phys: Vec<u8>,
    /// the DAV physical map behind XTAL_SI_READ_VAL. Only the low address byte is modelled:
    /// the DAV map has 96 bytes, so its high address bits are always 0.
    pub dav: Vec<u8>,
}

impl Default for Model {
    fn default() -> Model {
        let mut xtal = BTreeMap::new();
        xtal.insert(XTAL_SI_WL_RFC_S0, 0xc7);
        xtal.insert(XTAL_SI_WL_RFC_S1, 0xc7);
        Model {
            xtal,
            thermal: 0x1a,
            nctl: Box::new(|_| 4),
            nctl_reads: 0,
            rdy_fails: 0,
            c2h: [0; 4],
            efuse: None,
        }
    }
}

/// A PHY_CAP answer in the C2H registers (phy-test-expect.mjs `phycapC2h`)
pub fn phycap_c2h(
    tx_nss: u32,
    rx_nss: u32,
    tx_ant: u32,
    rx_ant: u32,
    prot: u32,
    id: u32,
) -> [u32; 4] {
    [
        prep(RTW89_C2HREG_HDR_FUNC_MASK, id) | prep(RTW89_C2HREG_PHYCAP_W0_RX_NSS, rx_nss),
        prep(RTW89_C2HREG_PHYCAP_W1_TX_NSS, tx_nss) | prep(RTW89_C2HREG_PHYCAP_W1_PROT, prot),
        0,
        prep(RTW89_C2HREG_PHYCAP_W3_ANT_TX_NUM, tx_ant)
            | prep(RTW89_C2HREG_PHYCAP_W3_ANT_RX_NUM, rx_ant),
    ]
}

pub struct ChipBus {
    pub mem: MemBus,
    pub model: Model,
    /// the last byte a normal XTAL SI write put at each offset
    xtal_written: BTreeMap<u32, u32>,
    /// replaces the RDY rule when set (ChipDev's `onPoll` override): whether a poll passes
    pub poll_ok: Option<PollFn>,
    /// every `mem_write_bytes`: address and bytes
    pub writes: Vec<(u32, Vec<u8>)>,
}

impl ChipBus {
    pub fn new(model: Model) -> ChipBus {
        ChipBus {
            mem: MemBus::new(),
            model,
            xtal_written: BTreeMap::new(),
            poll_ok: None,
            writes: Vec::new(),
        }
    }

    // ChipDev.answer: a register value instead of the stored one
    fn answer(&mut self, a: u32) -> Option<u32> {
        let m = &mut self.model;
        let sts0 = BB + R_PHY_STS_BITMAP_ADDR_START;
        Some(match a {
            XT => {
                let off = field(self.mem.peek(XT, Width::W32), B_AX_WL_XTAL_SI_ADDR_MASK);
                if let Some(ef) = &m.efuse {
                    // the DAV engine: always ready, the byte at the low address written last
                    let low = self
                        .xtal_written
                        .get(&XTAL_SI_LOW_ADDR)
                        .copied()
                        .unwrap_or(0);
                    let dav = ef.dav.get(low as usize).copied().map_or(0xff, u32::from);
                    match off {
                        XTAL_SI_CTRL => return Some(prep(B_AX_WL_XTAL_SI_DATA_MASK, XTAL_SI_RDY)),
                        XTAL_SI_READ_VAL => return Some(prep(B_AX_WL_XTAL_SI_DATA_MASK, dav)),
                        _ => {}
                    }
                }
                let byte = self
                    .xtal_written
                    .get(&off)
                    .or_else(|| m.xtal.get(&off))
                    .copied()
                    .unwrap_or(0);
                prep(B_AX_WL_XTAL_SI_DATA_MASK, byte)
            }
            R_AX_EFUSE_CTRL => {
                let a = field(self.mem.peek(a, Width::W32), B_AX_EF_ADDR_MASK) as usize;
                let byte = m.efuse.as_ref().and_then(|e| e.phys.get(a)).copied();
                B_AX_EF_RDY | byte.map_or(0xff, u32::from) // 0xff: an unprogrammed byte
            }
            _ if a == BB + R_SWSI_V1 => prep(RR_TM_VAL, m.thermal),
            _ if a == BB + 0x8080 => {
                let n = m.nctl_reads;
                m.nctl_reads += 1;
                (m.nctl)(n)
            }
            _ if (sts0..sts0 + 16 * 4).contains(&a) => STS_READ,
            _ if a == BB + R_P0_RPL1 => RPL1[0],
            _ if a == BB + R_P1_RPL1 => RPL1[1],
            _ if a == BB + R_BANDEDGE => B_BANDEDGE_EN,
            _ if a == BB + R_DCFO => DCFO_READ,
            R_AX_C2HREG_CTRL => 1,
            _ => return C2H.iter().position(|&r| r == a).map(|i| m.c2h[i]),
        })
    }

    /// The trace as golden-file lines
    pub fn lines(&self) -> Vec<alloc::string::String> {
        self.mem.lines()
    }
}

impl Bus for ChipBus {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        let v = self.mem.read(w, addr)?;
        Ok(self.answer(addr).map_or(v, |x| x & w.mask()))
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        if addr == XT && field(val, B_AX_WL_XTAL_SI_MODE_MASK) == XTAL_SI_NORMAL_WRITE {
            self.xtal_written.insert(
                field(val, B_AX_WL_XTAL_SI_ADDR_MASK),
                field(val, B_AX_WL_XTAL_SI_DATA_MASK),
            );
        }
        self.mem.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        let old = self.mem.peek(addr, w);
        self.mem.rmw(w, addr, clear, set)?;
        let old = self.answer(addr).map_or(old, |x| x & w.mask());
        self.mem.poke(addr, w, (old & !clear) | set);
        Ok(())
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.mem.poll(w, addr, mask, want, ms)?;
        let ok = match self.poll_ok.as_mut() {
            Some(f) => f(addr),
            None => {
                let fail = addr == R_AX_EFUSE_CTRL && self.model.rdy_fails > 0;
                if fail {
                    self.model.rdy_fails -= 1;
                }
                !fail
            }
        };
        if ok {
            return Ok(());
        }
        self.mem.poll_failures += 1;
        Err(BusError::PollTimeout {
            addr,
            width: w,
            mask,
            want,
            last: self.mem.peek(addr, w),
        })
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.mem.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.mem.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.mem.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.mem.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.mem.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.mem.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.mem.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.writes.push((addr, data.to_vec()));
        self.mem.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.mem.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.mem.now_ms()
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        self.mem.note(text)
    }
}
