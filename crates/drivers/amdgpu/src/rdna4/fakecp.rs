//! Wave 44, item CP: the model's command processor as Linux's `gfx_v12_0_hw_init` meets it after GFXHUB's start
//! (the trace's section gc-cp, init.txt lines 9362-9501). It answers as the real card answered Linux and refuses,
//! and keeps, what Linux would not do there:
//!
//! - any write before the block has power and its autoload is complete (`Model::gc_awake`, the one gate);
//! - a register of the section written with anything but Linux's value or Linux's one change of what it holds
//!   (GRBM_CNTL's read timeout, DB_RING_CONTROL's counter, SH_MEM_CONFIG 0xc00c, SH_MEM_BASES per VMID, the trap
//!   enable, the save-restore machine, the GUI idle interrupts off, the doorbell ranges, the KIQ's place), a
//!   register Linux only reads there written at all;
//! - GRBM_GFX_INDEX on any selection but Linux's (one per shader array of the card, then all broadcast), and any
//!   other register written while it is left on a selection; SH_MEM_CONFIG, SH_MEM_BASES and the trap enable
//!   written under a VMID Linux does not write them under, any other register while a VMID other than 0 is
//!   selected;
//! - the clear-state buffer's size other than 75 dwords, its address outside VRAM, off a page, over the GART
//!   table, the default page or the picture, or a buffer that does not hold Linux's dwords when its size is
//!   written; the save-restore machine before the buffer;
//! - an unhalt of MEC, or of PFP and ME, in any form but Linux's, before the engines' starts were written and
//!   pulsed in on every pipe (`gcboot`), or before the engine's doorbell range holds Linux's values (the model's
//!   order rule: Linux writes the ranges first, gfx_v12_0.c:3543 before :3546, and the card may not care); a
//!   write that clears CE_HALT (GFX 12 has no constant engine).
//!
//! NBIF's two doorbell entries for the GC block are kept here too: they take Linux's value only. After the
//! graphics unhalt CP_STAT answers what the card answered Linux (`STAT_SEEN`) before it reads 0. At every
//! autoload the block's registers go back to the trace's cold values with the rest of the block (`cold`).

use super::fakemodel::Model;
use super::nbregs::NbRegs;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::gc12cp::{self as cp, Config};
use rdna4_core::regmap::Reg;
use rdna4_core::{clearstate12, doorbell12 as db, gc12};

/// The registers of the section the model keeps, with what the card answered Linux's first read of each.
const COLD: [(Reg, u32); 20] = [
    (cp::GRBM_CNTL, 0x0030_0018),
    (cp::GRBM_GFX_INDEX, cp::GFX_INDEX_ALL),
    (cp::GRBM_CC_GC_SA_UNIT_DISABLE, 0x00FF_0000),
    (cp::GRBM_GC_USER_SA_UNIT_DISABLE, 0x00FF_0000),
    (cp::CC_RB_BACKEND_DISABLE, 0),
    (cp::GC_USER_RB_BACKEND_DISABLE, 0),
    (cp::CC_GC_SHADER_ARRAY_CONFIG, 0xFFF0_0000),
    (cp::GC_USER_SHADER_ARRAY_CONFIG, 0xFFF0_0000),
    (cp::TA_CNTL2, 0x0004_0000),
    (cp::DB_RING_CONTROL, 1),
    (cp::RLC_CSIB_ADDR_HI, 0),
    (cp::RLC_CSIB_ADDR_LO, 0),
    (cp::RLC_CSIB_LENGTH, 0),
    (cp::RLC_SRM_CNTL, 2),
    (cp::CP_INT_CNTL_RING0, 0),
    (cp::CP_RB_DOORBELL_RANGE_LOWER, 0),
    (cp::CP_RB_DOORBELL_RANGE_UPPER, 0),
    (cp::CP_MEC_DOORBELL_RANGE_LOWER, 0),
    (cp::CP_MEC_DOORBELL_RANGE_UPPER, 0),
    (cp::RLC_CP_SCHEDULERS, 0x0000_3038),
];
/// The three registers that are one per VMID (selected through GRBM_GFX_CNTL).
const PER_VMID: [Reg; 3] = [cp::SH_MEM_CONFIG, cp::SH_MEM_BASES, cp::SPI_GDBG_PER_VMID_CNTL];
/// Registers Linux only reads in this section.
const READ_ONLY: [Reg; 7] = [cp::GRBM_CC_GC_SA_UNIT_DISABLE, cp::GRBM_GC_USER_SA_UNIT_DISABLE, cp::CC_RB_BACKEND_DISABLE, cp::GC_USER_RB_BACKEND_DISABLE, cp::CC_GC_SHADER_ARRAY_CONFIG, cp::GC_USER_SHADER_ARRAY_CONFIG, cp::TA_CNTL2];
/// CP_STAT's answers after the graphics unhalt, as the card gave them to Linux (init.txt 9494-9499): 16 times
/// 0x88028000, once 0x8002c000, 3 times 0x88028000, then 0.
pub const STAT_SEEN: [(u32, u32); 3] = [(0x8802_8000, 16), (0x8002_C000, 1), (0x8802_8000, 3)];
pub struct FakeCp {
    /// The card's shader-engine layout as the stage reads it from the fake's discovery table (`fakecard::GC_INFO`).
    pub layout: Config,
    /// BAR5 byte to value, for the section's registers.
    pub vals: BTreeMap<u32, u32>,
    /// (BAR5 byte, VMID) to value, for the per-VMID registers.
    pub vmid_vals: BTreeMap<(u32, u32), u32>,
    /// NBIF's doorbell entries 0 and 3 as written (`None`: not written).
    pub entries: [Option<u32>; 2],
    /// The clear-state buffer the RLC was told: (MC address, dwords), once its size was taken.
    pub csb: Option<(u64, u32)>,
    pub mec_on: bool,
    pub gfx_on: bool,
    /// CP_STAT reads still busy after the graphics unhalt (tests: `u32::MAX`, never 0).
    pub stat_busy: u32,
    pub(super) stat_read: u32,
    /// SCRATCH_REG0 (the rings' test register).
    pub scratch: u32,
    /// What the scheduler placed (`fakecpeng`).
    pub queues: Vec<super::fakecpeng::CpQueue>,
    pub doorbells: Vec<(u32, u64)>,
    /// The interrupts the queues raised: (source, ring id).
    pub raised: Vec<(u8, u8)>,
    /// NBIF's HDP flush done bits as a ring's flush left them (`fakecpeng`).
    pub(super) hdp_done: u32,
    bytes: BTreeMap<u32, Reg>,
}

fn named(byte: u32) -> &'static str {
    super::linuxseq::name_of(byte / 4).unwrap_or("a register of the command processor")
}

impl FakeCp {
    pub fn new(n: &NbRegs, layout: Config) -> FakeCp {
        let mut bytes = BTreeMap::new();
        for r in COLD.iter().map(|(r, _)| *r).chain(PER_VMID).chain([db::ENTRY_0, db::ENTRY_3, cp::SCRATCH_REG0]) {
            if let Some(b) = n.byte(r) {
                bytes.insert(b, r);
            }
        }
        let mut c = FakeCp { layout, vals: BTreeMap::new(), vmid_vals: BTreeMap::new(), entries: [None; 2], csb: None, mec_on: false, gfx_on: false, stat_busy: 0, stat_read: 0, scratch: 0, queues: Vec::new(), doorbells: Vec::new(), raised: Vec::new(), hdp_done: 0, bytes };
        c.cold();
        c
    }

    /// The block's registers as an autoload leaves them: the trace's first reads (an assumption: section 7).
    pub fn cold(&mut self) {
        let at: Vec<(u32, Reg)> = self.bytes.iter().map(|(b, r)| (*b, *r)).collect();
        self.vals.clear();
        for (b, r) in at {
            if let Some((_, v)) = COLD.iter().find(|(c, _)| super::fakemodel::same(*c, r)) {
                self.vals.insert(b, *v);
            }
        }
        self.vmid_vals.clear();
        (self.csb, self.mec_on, self.gfx_on, self.stat_busy, self.scratch) = (None, false, false, 0, 0);
        self.queues.clear();
    }

    fn reg(&self, byte: u32) -> Option<Reg> {
        self.bytes.get(&byte).copied()
    }

    pub fn val(&self, n: &NbRegs, r: Reg) -> u32 {
        n.byte(r).and_then(|b| self.vals.get(&b)).copied().unwrap_or(0)
    }

    /// Both halves of one doorbell range register pair hold Linux's values.
    pub(super) fn ranges(&self, n: &NbRegs, gfx: bool) -> bool {
        let want = if gfx { [(cp::CP_RB_DOORBELL_RANGE_LOWER, db::RB_LOWER), (cp::CP_RB_DOORBELL_RANGE_UPPER, db::RB_UPPER)] } else { [(cp::CP_MEC_DOORBELL_RANGE_LOWER, db::MEC_LOWER), (cp::CP_MEC_DOORBELL_RANGE_UPPER, db::MEC_UPPER)] };
        want.iter().all(|(r, v)| self.val(n, *r) == *v) && want.iter().any(|(_, v)| *v != 0)
    }
}

/// GRBM_GFX_CNTL selects a VMID of ME 0, pipe 0, queue 0 (`soc24_grbm_select(adev, 0, 0, 0, vmid)`).
const fn vmid_of(sel: u32) -> Option<u32> {
    if sel & !gc12::f::VMID.mask == 0 { Some(gc12::f::VMID.get(sel)) } else { None }
}

impl Model {
    fn cp_refuse(&mut self, why: String) -> Option<bool> {
        Some(self.refuse(why))
    }

    /// A read of the command processor's registers (CP_STAT while it settles), or `None` for any other byte.
    pub(super) fn cp_read(&mut self, byte: u32) -> Option<u32> {
        let n = &self.nb.n;
        if n.byte(gc12::CP_STAT) == Some(byte) && self.cp.stat_busy > 0 && self.gc_awake() {
            self.cp.stat_busy -= u32::from(self.cp.stat_busy != u32::MAX);
            let k = self.cp.stat_read;
            self.cp.stat_read += 1;
            let mut at = 0;
            return Some(STAT_SEEN.iter().find(|(_, c)| { at += c; k < at }).map_or(STAT_SEEN[0].0, |(v, _)| *v));
        }
        let r = self.cp.reg(byte)?;
        if !self.gc_awake() && !super::fakemodel::same(r, db::ENTRY_0) && !super::fakemodel::same(r, db::ENTRY_3) {
            return Some(0);
        }
        if PER_VMID.iter().any(|x| super::fakemodel::same(*x, r)) {
            let vmid = vmid_of(self.gc.grbm).unwrap_or(0);
            return Some(self.cp.vmid_vals.get(&(byte, vmid)).copied().unwrap_or(0));
        }
        if super::fakemodel::same(r, cp::SCRATCH_REG0) {
            return Some(self.cp.scratch);
        }
        Some(self.cp.vals.get(&byte).copied().unwrap_or(0))
    }

    /// A write of the command processor's registers: `Some(reached)`, or `None` for any other byte (and for the
    /// writes of GRBM_GFX_CNTL, CP_ME_CNTL and CP_MEC_RS64_CNTL that are `fakegc`'s: the RS64 selections and the
    /// reset pulses).
    pub(super) fn cp_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let n = self.nb.n.clone();
        let is = |r: Reg| n.byte(r) == Some(byte);
        if is(db::ENTRY_0) || is(db::ENTRY_3) {
            let (i, want) = if is(db::ENTRY_0) { (0, db::ENTRY_0_GFX) } else { (1, db::ENTRY_3_MES) };
            if v != want {
                return self.cp_refuse(format!("{} written {v:#x}: Linux writes {want:#x} (nbif_v6_3_1.c:307-308)", named(byte)));
            }
            self.cp.entries[i] = Some(v);
            return Some(true);
        }
        let gc_owned = is(gc12::GRBM_GFX_CNTL) || is(gc12::CP_ME_CNTL) || is(gc12::CP_MEC_RS64_CNTL);
        if gc_owned {
            if !self.gc_awake() {
                return None;
            }
            if is(gc12::GRBM_GFX_CNTL) {
                return vmid_of(v).map(|_| {
                    self.gc.grbm = v;
                    self.gc.vals.insert(byte, v);
                    true
                });
            }
            let cur = self.gc.vals.get(&byte).copied().unwrap_or(0);
            let gfx = is(gc12::CP_ME_CNTL);
            let halts = if gfx { gc12::GFX_HALTS } else { gc12::f::MEC_HALT.mask };
            if cur & halts & !v != 0 {
                return Some(self.cp_unhalt(&n, byte, v, cur, gfx));
            }
            if v & halts & !cur != 0 {
                return Some(self.cp_halt(byte, v, cur, gfx));
            }
            return None;
        }
        let r = self.cp.reg(byte)?;
        if !self.gc_awake() {
            return self.cp_refuse(format!("{} written {v:#x} before the graphics block has power and its autoload is complete (gfx_v12_0.c:3797-3808)", r.name));
        }
        let index = self.cp.val(&n, cp::GRBM_GFX_INDEX);
        if is(cp::GRBM_GFX_INDEX) {
            let Config { shader_engines, sa_per_se, .. } = self.cp.layout;
            let shader_arrays = (0..shader_engines).flat_map(|se| (0..sa_per_se).map(move |sa| cp::select_se_sa(Some(se), Some(sa))));
            if v != cp::GFX_INDEX_ALL && !shader_arrays.clone().any(|s| s == v) {
                return self.cp_refuse(format!("GRBM_GFX_INDEX written {v:#x}: Linux selects one shader array of the card's at a time, then all again (gfx_v12_0.c:5617, :5640)"));
            }
            self.cp.vals.insert(byte, v);
            return Some(true);
        }
        if index != cp::GFX_INDEX_ALL {
            return self.cp_refuse(format!("{} written while GRBM_GFX_INDEX still selects {index:#x}: Linux selects all shader engines again first (gfx_v12_0.c:5640)", r.name));
        }
        if READ_ONLY.iter().any(|x| super::fakemodel::same(*x, r)) {
            return self.cp_refuse(format!("{} written {v:#x}: Linux only reads it here", r.name));
        }
        if PER_VMID.iter().any(|x| super::fakemodel::same(*x, r)) {
            return Some(self.cp_vmid_write(byte, r, v));
        }
        if self.gc.grbm != 0 {
            return self.cp_refuse(format!("{} written while GRBM_GFX_CNTL still selects {:#x}: Linux selects VMID 0 again first (gfx_v12_0.c:1856, :1810)", r.name, self.gc.grbm));
        }
        let cur = self.cp.vals.get(&byte).copied().unwrap_or(0);
        let want = match r.name {
            "GRBM_CNTL" => Some(cp::grbm_read_timeout(cur)),
            "DB_RING_CONTROL" => Some(cp::db_ring_counter(cur)),
            "CP_INT_CNTL_RING0" => Some(cp::gui_idle_off(cur)),
            "RLC_CP_SCHEDULERS" => Some(cp::kiq_setting(cur)),
            "CP_RB_DOORBELL_RANGE_LOWER" => Some(db::RB_LOWER),
            "CP_RB_DOORBELL_RANGE_UPPER" => Some(db::RB_UPPER),
            "CP_MEC_DOORBELL_RANGE_LOWER" => Some(db::MEC_LOWER),
            "CP_MEC_DOORBELL_RANGE_UPPER" => Some(db::MEC_UPPER),
            "SCRATCH_REG0" => Some(0xCAFE_DEAD),
            _ => None,
        };
        if let Some(w) = want.filter(|w| *w != v) {
            return self.cp_refuse(format!("{} written {v:#x} over {cur:#x}: Linux writes {w:#x} there", r.name));
        }
        match r.name {
            "RLC_CSIB_LENGTH" => return Some(self.cp_csib(byte, v)),
            "RLC_SRM_CNTL" if self.cp.csb.is_none() => return self.cp_refuse(String::from("RLC_SRM_CNTL written before the clear-state buffer was set up: Linux's rlc_resume sets the buffer first (gfx_v12_0.c:2110-2113)")),
            "RLC_SRM_CNTL" if v != cp::srm_enable(cur) => return self.cp_refuse(format!("RLC_SRM_CNTL written {v:#x} over {cur:#x}: Linux sets AUTO_INCR_ADDR and SRM_ENABLE and nothing else")),
            "SCRATCH_REG0" => {
                self.cp.scratch = v;
                return Some(true);
            }
            _ => {}
        }
        self.cp.vals.insert(byte, v);
        Some(true)
    }

    fn cp_vmid_write(&mut self, byte: u32, r: Reg, v: u32) -> bool {
        let Some(vmid) = vmid_of(self.gc.grbm) else { return self.refuse(format!("{} written while GRBM_GFX_CNTL selects {:#x}, no VMID of ME 0 pipe 0", r.name, self.gc.grbm)) };
        let cur = self.cp.vmid_vals.get(&(byte, vmid)).copied().unwrap_or(0);
        let want = match r.name {
            "SH_MEM_CONFIG" => Some(cp::DEFAULT_SH_MEM_CONFIG),
            "SH_MEM_BASES" => cp::sh_mem_bases_of(vmid),
            _ => (vmid >= cp::FIRST_KFD_VMID).then(|| cp::trap_en(cur)),
        };
        match want {
            Some(w) if w == v => {
                self.cp.vmid_vals.insert((byte, vmid), v);
                true
            }
            Some(w) => self.refuse(format!("{} written {v:#x} under VMID {vmid}: Linux writes {w:#x} there (gfx_v12_0.c:1840-1856, :1798-1808)", r.name)),
            None => self.refuse(format!("{} written under VMID {vmid}: Linux writes it under no such VMID", r.name)),
        }
    }

    /// RLC_CSIB_LENGTH: the buffer the RLC is told must be Linux's, where nothing else lies.
    fn cp_csib(&mut self, byte: u32, v: u32) -> bool {
        let n = self.nb.n.clone();
        let (lo, hi) = (self.cp.val(&n, cp::RLC_CSIB_ADDR_LO), self.cp.val(&n, cp::RLC_CSIB_ADDR_HI));
        let mc = u64::from(hi) << 32 | u64::from(lo);
        let size = clearstate12::size_dw();
        if v != size {
            return self.refuse(format!("RLC_CSIB_LENGTH written {v:#x}: Linux's clear-state buffer is {size:#x} dwords (gfx_v12_0.c:677-694)"));
        }
        if mc % rdna4_core::pte12::PAGE != 0 || !self.in_vram(mc, clearstate12::BYTES) {
            return self.refuse(format!("the clear-state buffer at mc {mc:#x}: not a page of VRAM (Linux takes a page there, amdgpu_rlc.c:136-140)"));
        }
        let off = mc - super::fakemodel::MC_BASE;
        let end = off + clearstate12::BYTES;
        let over = self.nb.vram_held().into_iter().find(|(_, r)| off < r.end && r.start < end).map(|(w, _)| w).or((off < self.picture.end && end > self.picture.start).then_some("the firmware's picture"));
        if let Some(what) = over {
            return self.refuse(format!("the clear-state buffer at mc {mc:#x} lies over {what}"));
        }
        let mut b = alloc::vec![0u8; clearstate12::BYTES as usize];
        self.peek(off, &mut b);
        if b != clearstate12::bytes() {
            return self.refuse(format!("RLC_CSIB_LENGTH written while the buffer at mc {mc:#x} does not hold Linux's clear state (gfx_v12_0_get_csb_buffer, gfx_v12_0.c:696-722)"));
        }
        self.cp.csb = Some((mc, v));
        self.cp.vals.insert(byte, v);
        true
    }

    /// CP_ME_CNTL or CP_MEC_RS64_CNTL written with a HALT bit set on a running engine: Linux's fini's form only
    /// (`gfx_v12_0_cp_enable(adev, false)`, gfx_v12_0.c:3914), and the engine is halted again.
    fn cp_halt(&mut self, byte: u32, v: u32, cur: u32, gfx: bool) -> bool {
        if self.gc.grbm != 0 {
            return self.refuse(format!("{} written while GRBM_GFX_CNTL still selects {:#x}: Linux selects pipe 0 again first", named(byte), self.gc.grbm));
        }
        let want = if gfx { cp::halt_gfx(cur) } else { cp::halt_mec(cur) };
        if v != want {
            return self.refuse(format!("{} written {v:#x} over {cur:#x}: Linux's halt writes {want:#x} (gfx_v12_0.c:{})", named(byte), if gfx { "2365-2370" } else { "2812-2838" }));
        }
        self.gc.vals.insert(byte, v);
        if gfx {
            (self.cp.gfx_on, self.cp.stat_busy) = (false, 0);
        } else {
            self.cp.mec_on = false;
        }
        true
    }

    /// CP_MEC_RS64_CNTL or CP_ME_CNTL written with a HALT bit cleared: only Linux's unhalt, only on started engines.
    fn cp_unhalt(&mut self, n: &NbRegs, byte: u32, v: u32, cur: u32, gfx: bool) -> bool {
        if self.gc.grbm != 0 {
            return self.refuse(format!("{} written while GRBM_GFX_CNTL still selects {:#x}: Linux selects pipe 0 again first (gfx_v12_0.c:1856, :1810)", named(byte), self.gc.grbm));
        }
        if gfx && cur & gc12::f::CE_HALT.mask & !v != 0 {
            return self.refuse(format!("CP_ME_CNTL written {v:#x}: CE_HALT cleared; GFX 12 has no constant engine, Linux never touches the bit"));
        }
        let want = if gfx { cp::unhalt_gfx(cur) } else { cp::unhalt_mec(cur) };
        if v != want {
            return self.refuse(format!("{} written {v:#x} over {cur:#x}: Linux's unhalt writes {want:#x} (gfx_v12_0.c:{})", named(byte), if gfx { "2365-2370" } else { "2812-2838" }));
        }
        let engines: &[usize] = if gfx { &[0, 1] } else { &[2] };
        if !engines.iter().all(|e| self.gc.started(n, *e) && self.gc.pulses[*e] > 0) {
            return self.refuse(format!("{} written {v:#x}: an unhalt of an engine whose program counter start is not written and pulsed in on every pipe (gfx_v12_0_config_gfx_rs64 comes first, :3816)", named(byte)));
        }
        if !self.cp.ranges(n, gfx) {
            return self.refuse(format!("{} written {v:#x} before its doorbell range holds Linux's values: Linux writes the ranges first (gfx_v12_0.c:3543 before :3546; the model's order rule)", named(byte)));
        }
        self.gc.vals.insert(byte, v);
        if gfx {
            self.cp.gfx_on = true;
            self.cp.stat_busy = self.cp.stat_busy.max(STAT_SEEN.iter().map(|(_, c)| c).sum());
            self.cp.stat_read = 0;
        } else {
            self.cp.mec_on = true;
        }
        true
    }
}

