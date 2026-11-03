//! GX-7c: the scanout watcher (GX-D9 as a check). The render bring-up must not disturb the picture the firmware
//! (and later our KMS) shows, so no bring-up write may reach a display register:
//! - DCN's own register segments (DMU instance 0, segments 1..; segment 0 aliases NBIO and is not DCN's);
//! - the MMHUB fields DCN fetches the frame buffer through (FB location and offset, system aperture and its
//!   default page, AGP, L1 TLB, L2 and VM context 0 control: gx-scout-bringup.md section 6, risk 1);
//! - the display's own PMFW mailbox (MP1 C2PMSG_67/83/91, dcn314_smu.c): ours is 66/82/90 (X15);
//! - graphics memory the display reads (the scanout surface, DMCUB's windows), as the owner registers it
//!   (`DisplayGuard::protect`).
//!
//! `DisplayGuard` knows those bytes from the discovery table (any DCN 3.1.x layout, no 780M constant), the
//! fakes count every such write (`pspfake::FakeGpu::display_writes`), and `Watch` wraps any `Hw`: in the fakes
//! it records (`Mode::Record`, a test fails on any record), on the real GPU it refuses the write before it
//! reaches the device (`Mode::Block`) and logs it. Linux itself writes these registers (its DC, 11 337 writes in
//! the 27.10. capture), so the check applies to our sequence only.
//!
//! A display register is also reachable through the index/data windows (GX-7c fixer): NBIO's PCIE_INDEX/DATA
//! and PCIE_INDEX2/DATA2 (SMN; linux-6.6 amdgpu_device.c:498 sends a register beyond the BAR as `reg * 4`, so
//! an SMN address below the BAR's end is the BAR5 byte, and MP1's mailbox also answers at its SMN alias,
//! smu_v13_0.h:33-38) and MM_INDEX/MM_INDEX_HI/MM_DATA (bit 31 clear: a register byte; set: graphics memory,
//! amdgpu_device.c amdgpu_device_mm_access). `Watch` keeps the last index written and judges each data write by
//! its target. The record keeps the first `keep` violations and counts all (R1: a looping driver does not grow it).
//!
//! GX-7h (GX-7c open point 3): MMHUB's invalidation engines (INVALIDATE_ENGn_REQ and _SEM) are watched too
//! (`Class::MmhubFlush`): DCN fetches through the system aperture, not VMID 0, but a flush of the hub it fetches
//! through is the GMC stage's alone. A watched write passes only by an explicit one-shot `Watch::allow` with its
//! reason (the GMC stage's own planned flush), and every such pass is kept in `granted`. The flush engines are
//! not display registers: the fakes do not count them as display writes (`Class::is_display`).

use super::disc::Discovery;
use super::hwid;
use super::psp::Hw;
use crate::ipmod::gmc11;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// The extent in dwords of DCN 3.1.4's segments 1..3 (dcn_3_1_4_offset.h: the highest offset + 1 per segment;
/// segment 3 also holds the DPIA RBBMIF registers at 0x1381e..0x13820).
const DCN_EXTENT: &[(usize, u32, u32)] = &[(1, 0, 0xbb), (2, 0, 0x3909), (3, 0, 0xf7a), (3, 0x1381e, 0x13821)];

/// MMHUB 3.0.1 (segment 1, mmhub_3_0_1_offset.h) registers the display's fetch depends on.
pub const MMHUB_DISPLAY: &[(&str, (usize, u32))] = &[
    ("MMMC_VM_FB_OFFSET", gmc11::MMHUB_FB_OFFSET),
    ("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_LSB", (1, 0x8d8)),
    ("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_MSB", (1, 0x8d9)),
    ("MMMC_VM_LOCAL_SYSMEM_ADDRESS_START", (1, 0x8df)),
    ("MMMC_VM_LOCAL_SYSMEM_ADDRESS_END", (1, 0x8e0)),
    ("MMMC_VM_LOCAL_FB_ADDRESS_START", (1, 0x8e2)),
    ("MMMC_VM_LOCAL_FB_ADDRESS_END", (1, 0x8e3)),
    ("MMMC_VM_FB_LOCATION_BASE", gmc11::MMHUB_FB_LOCATION_BASE),
    ("MMMC_VM_FB_LOCATION_TOP", gmc11::MMHUB_FB_LOCATION_TOP),
    ("MMMC_VM_AGP_TOP", gmc11::MMHUB_AGP_TOP),
    ("MMMC_VM_AGP_BOT", gmc11::MMHUB_AGP_BOT),
    ("MMMC_VM_AGP_BASE", gmc11::MMHUB_AGP_BASE),
    ("MMMC_VM_SYSTEM_APERTURE_LOW_ADDR", gmc11::MMHUB_SYSTEM_APERTURE_LOW),
    ("MMMC_VM_SYSTEM_APERTURE_HIGH_ADDR", gmc11::MMHUB_SYSTEM_APERTURE_HIGH),
    ("MMMC_VM_MX_L1_TLB_CNTL", gmc11::MMHUB_MX_L1_TLB_CNTL),
    ("MMVM_L2_CNTL", gmc11::MMVM_L2_CNTL),
    ("MMVM_L2_CNTL2", (1, 0x701)),
    ("MMVM_L2_CNTL3", (1, 0x702)),
    ("MMVM_L2_CNTL4", (1, 0x718)),
    ("MMVM_L2_CNTL5", (1, 0x71e)),
    ("MMVM_CONTEXT0_CNTL", gmc11::MMVM_CONTEXT0_CNTL),
    ("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", (1, 0x7ab)),
    ("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32", (1, 0x7ac)),
];

/// The display's PMFW mailbox (MP1 segment 1).
pub const DCN_MAILBOX: &[(&str, (usize, u32))] = &[("MP1_SMN_C2PMSG_67", (1, 0x283)), ("MP1_SMN_C2PMSG_83", (1, 0x293)), ("MP1_SMN_C2PMSG_91", (1, 0x29b))];

/// MP1's segment-1 registers in SMN space, plain and with MP1_Public (smu_v13_0.h:33-38: FIRMWARE_FLAGS, dword
/// 0xa of segment 1, is SMN 0x3010028 and is read at 0x3b10028). Assumption from those two defines: segment 1's
/// dword `dw` is SMN 0x3010000 + dw * 4 under both prefixes.
pub const MP1_SMN_SEG1: [u32; 2] = [0x0301_0000, 0x03b1_0000];

/// NBIO 7.7's SMN index/data pairs (nbio_7_7_0_offset.h, segment 0): PCIE_INDEX/DATA, PCIE_INDEX2/DATA2.
const SMN_PAIRS: &[(u32, u32)] = &[(0xc, 0xd), (0xe, 0xf)];
/// MM_INDEX bit 31: the access goes to graphics memory (amdgpu_device_mm_access).
const MM_APERTURE: u32 = 0x8000_0000;

/// How many violations a `Watch` keeps by default (later ones are only counted).
pub const VIOLATIONS_KEEP: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// An SMN address (NBIO's PCIE index/data pairs).
    Smn,
    /// MM_INDEX: a register byte, or graphics memory with bit 31 (MM_INDEX_HI holds bits 31.. of the position).
    Mm,
}

/// An index/data window in BAR5 (byte offsets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub space: Space,
    pub index: u32,
    pub index_hi: Option<u32>,
    pub data: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Inside a DCN register segment.
    Dcn,
    /// An MMHUB field the display fetch uses.
    Mmhub(&'static str),
    /// The display's PMFW mailbox.
    DcnMailbox(&'static str),
    /// Graphics memory the display reads (the scanout surface, DMCUB's windows): `DisplayGuard::protect`.
    Scanout,
    /// GX-7h: an MMHUB invalidation engine's request or semaphore (only the GMC stage's allowed flush).
    MmhubFlush(&'static str),
}

impl Class {
    /// A register or memory the display itself uses (everything but the MMHUB flush engines).
    pub fn is_display(&self) -> bool {
        !matches!(self, Class::MmhubFlush(_))
    }
}

/// MMHUB 3.0.1's invalidation engines (segment 1): ENG0_REQ and ENG0_SEM dwords, one per engine.
pub const MMHUB_FLUSH: &[(&str, (usize, u32))] = &[("MMVM_INVALIDATE_ENG_REQ", (1, 0x763)), ("MMVM_INVALIDATE_ENG_SEM", (1, 0x751))];

#[derive(Clone, Debug, Default)]
pub struct DisplayGuard {
    /// BAR5 byte ranges [lo, hi) of DCN.
    pub dcn: Vec<(u32, u32)>,
    pub named: Vec<(u32, Class)>,
    /// SMN addresses (beyond the BAR5 identity) of display registers.
    pub smn_named: Vec<(u32, Class)>,
    /// MC ranges [lo, hi) of graphics memory the display reads.
    pub mem: Vec<(u64, u64)>,
    /// The index/data windows a register or memory write can go through.
    pub windows: Vec<Window>,
    /// The MC address of graphics memory position 0 (MM_INDEX positions); unknown: a memory write through
    /// MM_DATA counts as scanout whenever anything is protected.
    pub vram_mc: Option<u64>,
}

impl DisplayGuard {
    pub fn from_discovery(d: &Discovery) -> DisplayGuard {
        let mut g = DisplayGuard::default();
        for &(seg, lo, hi) in DCN_EXTENT {
            let r = d.reg(hwid::DMU, 0, seg, lo).and_then(|a| a.checked_mul(4)).zip(d.reg(hwid::DMU, 0, seg, hi).and_then(|b| b.checked_mul(4)));
            if let Some(r) = r {
                g.dcn.push(r);
            }
        }
        for &(n, (seg, dw)) in MMHUB_DISPLAY {
            if let Some(b) = d.reg(hwid::MMHUB, 0, seg, dw).and_then(|a| a.checked_mul(4)) {
                g.named.push((b, Class::Mmhub(n)));
            }
        }
        for &(n, (seg, dw)) in DCN_MAILBOX {
            if let Some(b) = d.reg(hwid::MP1, 0, seg, dw).and_then(|a| a.checked_mul(4)) {
                g.named.push((b, Class::DcnMailbox(n)));
            }
        }
        for &(n, (seg, dw)) in MMHUB_FLUSH {
            for e in 0..u32::from(gmc11::ENGINES) {
                if let Some(b) = d.reg(hwid::MMHUB, 0, seg, dw + e).and_then(|a| a.checked_mul(4)) {
                    g.named.push((b, Class::MmhubFlush(n)));
                }
            }
        }
        for &(i, dd) in SMN_PAIRS {
            let b = |dw| d.reg(hwid::NBIF, 0, 0, dw).and_then(|a| a.checked_mul(4));
            if let (Some(index), Some(data)) = (b(i), b(dd)) {
                g.windows.push(Window { space: Space::Smn, index, index_hi: None, data });
            }
        }
        g.windows.push(Window { space: Space::Mm, index: crate::dcn::MM_INDEX, index_hi: Some(crate::dcn::MM_INDEX_HI), data: crate::dcn::MM_DATA });
        // The mailbox's SMN aliases, from the discovery's own MP1 segment 1 dword (any 13.x layout).
        for &(n, (seg, dw)) in DCN_MAILBOX {
            if seg == 1 && d.reg(hwid::MP1, 0, seg, dw).is_some() {
                for base in MP1_SMN_SEG1 {
                    g.smn_named.push((base + dw * 4, Class::DcnMailbox(n)));
                }
            }
        }
        g
    }

    /// An SMN address: a BAR5 byte as Linux sends it (`reg * 4`), or an MP1 alias of the display's mailbox.
    pub fn smn_class(&self, addr: u32) -> Option<Class> {
        self.smn_named.iter().find(|(a, _)| *a == addr).map(|&(_, c)| c).or_else(|| self.class(addr))
    }

    /// The target of a data write through `w` with these index values: (address, class) when it is the display's.
    pub fn indirect_class(&self, w: &Window, index: u32, hi: u32) -> Option<(u64, Class)> {
        match w.space {
            Space::Smn => self.smn_class(index).map(|c| (u64::from(index), c)),
            Space::Mm if index & MM_APERTURE == 0 => self.class(index).map(|c| (u64::from(index), c)),
            Space::Mm => {
                let pos = (u64::from(hi) << 31) | u64::from(index & !MM_APERTURE);
                match self.vram_mc {
                    Some(base) => self.mem_class(base.saturating_add(pos), 4).map(|c| (pos, c)),
                    None => (!self.mem.is_empty()).then_some((pos, Class::Scanout)),
                }
            }
        }
    }

    /// Graphics memory the display reads from: a write overlapping it is a violation.
    pub fn protect(&mut self, mc: u64, len: u64) {
        self.mem.push((mc, mc.saturating_add(len)));
    }

    pub fn mem_class(&self, mc: u64, len: usize) -> Option<Class> {
        let end = mc.saturating_add(len as u64);
        self.mem.iter().any(|&(lo, hi)| mc < hi && end > lo).then_some(Class::Scanout)
    }

    pub fn class(&self, byte: u32) -> Option<Class> {
        if let Some(&(_, c)) = self.named.iter().find(|(b, _)| *b == byte) {
            return Some(c);
        }
        self.dcn.iter().any(|&(lo, hi)| byte >= lo && byte < hi).then_some(Class::Dcn)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Let the write through and record it (fakes: the test fails on the record).
    Record,
    /// Refuse it before the device sees it (the real GPU).
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    Reg(u32),
    Mem { mc: u64, len: usize },
    /// A data write through an index/data window: the data register's byte and the target (an SMN address, a
    /// register byte, or a graphics memory position for MM_INDEX with bit 31).
    Indirect { data: u32, target: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation {
    pub at: At,
    /// The register value written (0 for memory).
    pub value: u32,
    /// What the register read before (MMHUB fields only).
    pub before: Option<u32>,
    pub class: Class,
    pub blocked: bool,
}

/// Any `Hw` with the scanout watcher in front.
pub struct Watch<H> {
    hw: H,
    guard: DisplayGuard,
    mode: Mode,
    violations: Vec<Violation>,
    keep: usize,
    total: u64,
    /// The last value written to each window's index registers (BAR5 byte -> value).
    index: BTreeMap<u32, u32>,
    /// GX-7h: one-shot passes for watched registers: (byte, value, why), each used once.
    allowed: Vec<(u32, u32, &'static str)>,
    /// The passes used, in order.
    granted: Vec<(u32, u32, &'static str)>,
}

impl<H: Hw> Watch<H> {
    pub fn new(hw: H, guard: DisplayGuard, mode: Mode) -> Watch<H> {
        Watch::with_keep(hw, guard, mode, VIOLATIONS_KEEP)
    }

    /// `keep`: how many violations are kept; all are counted (`total`).
    pub fn with_keep(hw: H, guard: DisplayGuard, mode: Mode, keep: usize) -> Watch<H> {
        Watch { hw, guard, mode, violations: Vec::new(), keep, total: 0, index: BTreeMap::new(), allowed: Vec::new(), granted: Vec::new() }
    }

    /// GX-7h: lets the next write of `value` to the watched register `byte` through, once, for `why` (logged).
    /// Only the MMHUB flush engines are ever allowed (the GMC stage's flush); a display register is not.
    pub fn allow(&mut self, byte: u32, value: u32, why: &'static str) -> bool {
        let ok = matches!(self.guard.class(byte), Some(Class::MmhubFlush(_)));
        if ok {
            self.allowed.push((byte, value, why));
        } else {
            klog::event!(Error, "gx", "watch.allow_refused", byte = klog::Hex(u64::from(byte)), why = why; "only an MMHUB flush engine can be allowed");
        }
        ok
    }

    /// Passes left unused (the caller drops them after its step: `drop_allowed`).
    pub fn pending_allowed(&self) -> usize {
        self.allowed.len()
    }

    pub fn drop_allowed(&mut self) {
        self.allowed.clear();
    }

    /// The writes an allowance let through.
    pub fn granted(&self) -> &[(u32, u32, &'static str)] {
        &self.granted
    }

    pub fn guard(&self) -> &DisplayGuard {
        &self.guard
    }

    /// Every violation seen, kept or not.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Records one violation; true when the write must not reach the device.
    fn flag(&mut self, v: Violation) -> bool {
        self.total = self.total.saturating_add(1);
        if self.violations.len() < self.keep {
            self.violations.push(v);
            klog::event!(Error, "gx", "watch.display_write", at = alloc::format!("{:?}", v.at), value = klog::Hex(u64::from(v.value)),
                class = alloc::format!("{:?}", v.class), blocked = v.blocked; "a bring-up write to a display register or scanout memory");
        } else if self.total == self.keep as u64 + 1 {
            klog::event!(Error, "gx", "watch.capped", keep = self.keep, blocked = v.blocked; "more display writes: from now on only counted");
        }
        v.blocked
    }

    /// The index value a data write through `w` goes to: the last one written through us, else the device's.
    fn index_of(&mut self, reg: u32) -> Option<u32> {
        match self.index.get(&reg) {
            Some(&v) => Some(v),
            None => self.hw.reg_read(reg),
        }
    }

    pub fn hw(&mut self) -> &mut H {
        &mut self.hw
    }

    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }
}

impl<H: Hw> Hw for Watch<H> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.hw.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        let blocked = self.mode == Mode::Block;
        if let Some(w) = self.guard.windows.iter().find(|w| w.data == byte).copied() {
            // An index the device never answered cannot be judged: Block refuses it, Record lets it through.
            let (index, hi) = (self.index_of(w.index), w.index_hi.map_or(Some(0), |h| self.index_of(h)));
            let (Some(index), Some(hi)) = (index, hi) else {
                klog::event!(Warn, "gx", "watch.index_unknown", data = klog::Hex(u64::from(byte)), blocked = blocked; "a data write whose index is unknown");
                return !blocked && self.hw.reg_write(byte, value);
            };
            if let Some((target, class)) = self.guard.indirect_class(&w, index, hi) {
                if self.flag(Violation { at: At::Indirect { data: byte, target }, value, before: None, class, blocked }) {
                    return false;
                }
            }
            return self.hw.reg_write(byte, value);
        }
        let Some(class) = self.guard.class(byte) else {
            let ok = self.hw.reg_write(byte, value);
            if ok && self.guard.windows.iter().any(|w| w.index == byte || w.index_hi == Some(byte)) {
                self.index.insert(byte, value);
            }
            return ok;
        };
        if let Some(i) = self.allowed.iter().position(|&(b, v, _)| b == byte && v == value) {
            let (b, v, why) = self.allowed.remove(i);
            klog::event!(Info, "gx", "watch.allowed", byte = klog::Hex(u64::from(b)), value = klog::Hex(u64::from(v)), class = alloc::format!("{class:?}"), why = why);
            self.granted.push((b, v, why));
            return self.hw.reg_write(byte, value);
        }
        // The value before tells a reader how far the picture could have been moved; DCN itself is not read
        // (some DCN registers have read side effects).
        let before = if matches!(class, Class::Mmhub(_)) { self.hw.reg_read(byte) } else { None };
        if self.flag(Violation { at: At::Reg(byte), value, before, class, blocked }) { false } else { self.hw.reg_write(byte, value) }
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.hw.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        let Some(class) = self.guard.mem_class(mc, data.len()) else { return self.hw.mem_write(mc, data) };
        let blocked = self.mode == Mode::Block;
        if self.flag(Violation { at: At::Mem { mc, len: data.len() }, value: 0, before: None, class, blocked }) { false } else { self.hw.mem_write(mc, data) }
    }
    fn now_us(&mut self) -> u64 {
        self.hw.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.hw.delay_us(us)
    }
    fn flush_hdp(&mut self) {
        self.hw.flush_hdp()
    }
    fn invalidate_hdp(&mut self) {
        self.hw.invalidate_hdp()
    }
    fn mem_write64(&mut self, mc: u64, value: u64) -> bool {
        match self.guard.mem_class(mc, 8) {
            None => self.hw.mem_write64(mc, value),
            Some(_) => self.mem_write(mc, &value.to_le_bytes()),
        }
    }
    fn store_barrier(&mut self) {
        self.hw.store_barrier()
    }    /// Doorbells are not display state (the BAR2 aperture rings an engine, never DCN).
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        self.hw.doorbell_write64(index_dw, value)
    }
}
