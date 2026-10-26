//! A fake configuration space: functions with 4 KiB of bytes, BARs that answer the sizing protocol
//! (writable address bits, read-only flag bits), a write log, and an optional reach (bus range, 256-byte
//! legacy space) so the walk's limits can be tested.

use crate::access::{ConfigAccess, Loc, EXT_SPACE, STD_SPACE};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::vec::Vec;

#[derive(Clone)]
pub struct FakeFn {
    pub cfg: Vec<u8>,
    /// Writable bits per BAR dword (0x10 + 4*i); 0 = not implemented.
    pub bar_mask: [u32; 6],
    /// Power management behaviour (PC-2), when the function has a PM capability set up by `pm`.
    pub pm: Option<PmModel>,
}

/// How a fake function answers a PMCSR write.
#[derive(Clone, Copy)]
pub struct PmModel {
    pub off: u16,
    /// D3hot -> D0 resets the function (No_Soft_Reset clear): BAR addresses and command read 0 afterwards.
    pub reset_on_wake: bool,
    /// The state bits ignore writes (a function that never leaves D3hot).
    pub stuck: bool,
    /// Reads of PMCSR that still show the old state after a write (a slow function).
    pub lag: u8,
    pending: Option<u16>,
}

impl FakeFn {
    pub fn new(vendor: u16, device: u16, class: u32, header: u8) -> FakeFn {
        let mut f = FakeFn { cfg: alloc::vec![0; EXT_SPACE as usize], bar_mask: [0; 6], pm: None };
        f.w16(0, vendor);
        f.w16(2, device);
        f.w32(8, class << 8 | 1);
        f.w8(0x0E, header);
        f
    }
    pub fn w8(&mut self, o: u16, v: u8) -> &mut Self {
        self.cfg[o as usize] = v;
        self
    }
    pub fn w16(&mut self, o: u16, v: u16) -> &mut Self {
        self.cfg[o as usize..o as usize + 2].copy_from_slice(&v.to_le_bytes());
        self
    }
    pub fn w32(&mut self, o: u16, v: u32) -> &mut Self {
        self.cfg[o as usize..o as usize + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
    pub fn r32(&self, o: u16) -> u32 {
        u32::from_le_bytes(self.cfg[o as usize..o as usize + 4].try_into().unwrap())
    }
    /// Command register (memory + I/O decode on, bus master on: what firmware leaves for a used device).
    pub fn cmd(&mut self, v: u16) -> &mut Self {
        self.w16(4, v)
    }
    /// 32-bit memory BAR of `size` at `addr`.
    pub fn bar32(&mut self, i: usize, addr: u32, size: u32, prefetch: bool) -> &mut Self {
        let flags = if prefetch { 8 } else { 0 };
        self.bar_mask[i] = !(size - 1);
        self.w32(0x10 + 4 * i as u16, addr | flags)
    }
    /// 64-bit memory BAR of `size` at `addr` (slots i and i+1).
    pub fn bar64(&mut self, i: usize, addr: u64, size: u64, prefetch: bool) -> &mut Self {
        let flags = 4 | if prefetch { 8 } else { 0 };
        let mask = !(size - 1);
        self.bar_mask[i] = (mask as u32) & !0xF;
        self.bar_mask[i + 1] = (mask >> 32) as u32;
        self.w32(0x10 + 4 * i as u16, addr as u32 | flags);
        self.w32(0x14 + 4 * i as u16, (addr >> 32) as u32)
    }
    /// I/O BAR; `decode16`: a 16-bit decoder (upper half reads 0).
    pub fn bario(&mut self, i: usize, port: u32, size: u32, decode16: bool) -> &mut Self {
        let m = !(size - 1) & !3;
        self.bar_mask[i] = if decode16 { m & 0xFFFF } else { m };
        self.w32(0x10 + 4 * i as u16, port | 1)
    }
    /// Standard capability list: (offset, id, next) and the status bit.
    pub fn cap(&mut self, off: u8, id: u8, next: u8) -> &mut Self {
        let st = u16::from_le_bytes([self.cfg[6], self.cfg[7]]) | 0x10;
        self.w16(6, st);
        if self.cfg[0x34] == 0 {
            self.cfg[0x34] = off;
        }
        self.w8(off as u16, id).w8(off as u16 + 1, next)
    }
    pub fn ext(&mut self, off: u16, id: u16, ver: u8, next: u16) -> &mut Self {
        self.w32(off, id as u32 | (ver as u32) << 16 | (next as u32) << 20)
    }
    /// PM capability at `off` in D state `state`, made the head of the list; `reset_on_wake` clears
    /// No_Soft_Reset.
    pub fn pm(&mut self, off: u8, next: u8, state: u8, reset_on_wake: bool) -> &mut Self {
        self.cap(off, 0x01, next);
        self.cfg[0x34] = off;
        self.w16(off as u16 + 2, 3);
        let nsr = if reset_on_wake { 0 } else { 0x8 };
        self.w16(off as u16 + 4, nsr | state as u16);
        self.pm = Some(PmModel { off: off as u16 + 4, reset_on_wake, stuck: false, lag: 0, pending: None });
        self
    }
    /// MSI capability (64-bit) with the enable bit as given.
    pub fn msi(&mut self, off: u8, next: u8, enabled: bool) -> &mut Self {
        self.cap(off, 0x05, next);
        self.w16(off as u16 + 2, 0x80 | enabled as u16)
    }
    /// MSI-X capability (4 entries, table and PBA in BAR `bir`) with the enable bit as given.
    pub fn msix(&mut self, off: u8, next: u8, enabled: bool, bir: u8) -> &mut Self {
        self.cap(off, 0x11, next);
        self.w16(off as u16 + 2, 3 | if enabled { 0x8000 } else { 0 });
        self.w32(off as u16 + 4, bir as u32).w32(off as u16 + 8, bir as u32 | 0x800)
    }
    /// Bridge memory window (32-bit, 1 MiB granules) and prefetchable window (64-bit).
    pub fn windows(&mut self, mem: (u32, u32), prefetch: (u64, u64)) -> &mut Self {
        self.w32(0x20, (mem.0 >> 16) & 0xFFF0 | (mem.1 & 0xFFF0_0000));
        self.w32(0x24, 1 | ((prefetch.0 as u32) >> 16) & 0xFFF0 | ((prefetch.1 as u32) & 0xFFF0_0000));
        self.w32(0x28, (prefetch.0 >> 32) as u32).w32(0x2C, (prefetch.1 >> 32) as u32)
    }
    /// Type-1 bus numbers.
    pub fn buses(&mut self, primary: u8, secondary: u8, subordinate: u8) -> &mut Self {
        self.w32(0x18, primary as u32 | (secondary as u32) << 8 | (subordinate as u32) << 16)
    }
    /// A PCIe capability of `port_type` at `off` (version 2, link x4 gen 3).
    pub fn pcie(&mut self, off: u8, port_type: u8, next: u8) -> &mut Self {
        self.cap(off, 0x10, next);
        self.w16(off as u16 + 2, 2 | (port_type as u16) << 4);
        self.w32(off as u16 + 0x0C, 3 | 4 << 4);
        self.w16(off as u16 + 0x12, 3 | 4 << 4)
    }
}

pub struct Fake {
    pub fns: Mutex<BTreeMap<Loc, FakeFn>>,
    pub writes: Mutex<Vec<(Loc, u16, u32)>>,
    /// Command register value at each BAR write (to prove decoding was off).
    pub cmd_at_bar_write: Mutex<Vec<(Loc, u16, u16)>>,
    /// 4096 (ECAM) or 256 (legacy).
    pub space: u16,
    /// Buses reachable (inclusive); outside = space 0.
    pub reach: (u8, u8),
}

pub fn bridge(vendor: u16, device: u16, primary: u8, secondary: u8, subordinate: u8) -> FakeFn {
    let mut f = FakeFn::new(vendor, device, 0x06_04_00, 1);
    f.buses(primary, secondary, subordinate).cmd(0x7);
    f
}

/// A PCIe root port bridge (port type 4).
pub fn root_port(primary: u8, secondary: u8, subordinate: u8) -> FakeFn {
    let mut f = bridge(0x1022, 0x14EE, primary, secondary, subordinate);
    f.pcie(0x58, 4, 0);
    f
}

impl Fake {
    pub fn new() -> Fake {
        Fake {
            fns: Mutex::new(BTreeMap::new()),
            writes: Mutex::new(Vec::new()),
            cmd_at_bar_write: Mutex::new(Vec::new()),
            space: EXT_SPACE,
            reach: (0, 255),
        }
    }
    pub fn legacy() -> Fake {
        Fake { space: STD_SPACE, ..Fake::new() }
    }
    pub fn add(&self, bus: u8, dev: u8, func: u8, f: FakeFn) -> &Self {
        self.fns.lock().unwrap().insert(Loc::new(0, bus, dev, func), f);
        self
    }
    pub fn add_at(&self, at: Loc, f: FakeFn) -> &Self {
        self.fns.lock().unwrap().insert(at, f);
        self
    }
    pub fn writes_to(&self, bus: u8, dev: u8, func: u8) -> usize {
        self.writes.lock().unwrap().iter().filter(|w| w.0 == Loc::new(0, bus, dev, func)).count()
    }
    pub fn snapshot(&self) -> BTreeMap<Loc, Vec<u8>> {
        self.fns.lock().unwrap().iter().map(|(k, v)| (*k, v.cfg.clone())).collect()
    }
}

impl ConfigAccess for Fake {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        if off >= self.space_size(at.segment, at.bus) {
            return u32::MAX;
        }
        let mut fns = self.fns.lock().unwrap();
        let Some(f) = fns.get_mut(&at) else { return u32::MAX };
        if let Some(pm) = f.pm.as_mut().filter(|p| p.off == off & !3 && p.pending.is_some()) {
            if pm.lag > 0 {
                pm.lag -= 1;
            } else {
                let to = pm.pending.take().unwrap_or(0);
                let o = pm.off as usize;
                let csr = u16::from_le_bytes([f.cfg[o], f.cfg[o + 1]]) & !3 | to;
                f.cfg[o..o + 2].copy_from_slice(&csr.to_le_bytes());
            }
        }
        f.r32(off & !3)
    }
    fn narrow_writes(&self) -> bool {
        self.space == EXT_SPACE
    }
    fn write_narrow(&self, at: Loc, off: u16, width: u8, v: u32) -> bool {
        if !self.narrow_writes() || off >= self.space_size(at.segment, at.bus) || !matches!(width, 1 | 2) || !off.is_multiple_of(width as u16) {
            return false;
        }
        self.writes.lock().unwrap().push((at, off, v));
        let mut fns = self.fns.lock().unwrap();
        let Some(f) = fns.get_mut(&at) else { return true };
        if width == 1 {
            f.w8(off, v as u8);
        } else {
            f.w16(off, v as u16);
        }
        true
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        if off >= self.space_size(at.segment, at.bus) {
            return false;
        }
        let off = off & !3;
        self.writes.lock().unwrap().push((at, off, v));
        let mut fns = self.fns.lock().unwrap();
        let Some(f) = fns.get_mut(&at) else { return true };
        let ty = f.cfg[0x0E] & 0x7F;
        let bars = match ty {
            0 => 6,
            1 => 2,
            _ => 0,
        };
        if off == 4 {
            // Status is write-1-to-clear: a zero upper half leaves it.
            f.w16(4, v as u16);
        } else if let Some(pm) = f.pm.as_mut().filter(|p| p.off == off) {
            let old = u16::from_le_bytes([f.cfg[off as usize], f.cfg[off as usize + 1]]);
            let from = old & 3;
            let to = if pm.stuck {
                from
            } else if pm.lag > 0 {
                pm.pending = Some(v as u16 & 3);
                from
            } else {
                v as u16 & 3
            };
            let (reset, off_) = (pm.reset_on_wake && from == 3 && to == 0, off);
            // No_Soft_Reset (bit 3) is read-only; PME_En, Data_Select and the state follow the write.
            let csr = (v as u16 & !0x8 & !3) | (old & 0x8) | to;
            f.w16(off_, csr);
            if reset {
                for i in 0..bars as usize {
                    let o = 0x10 + 4 * i as u16;
                    let m = f.bar_mask[i];
                    let keep = f.r32(o) & !m;
                    f.w32(o, keep);
                }
                f.w16(4, 0);
                f.w8(0x0C, 0);
                f.w8(0x3C, 0);
            }
        } else if (0x10..0x10 + 4 * bars).contains(&off) {
            let i = ((off - 0x10) / 4) as usize;
            let cmd = u16::from_le_bytes([f.cfg[4], f.cfg[5]]);
            self.cmd_at_bar_write.lock().unwrap().push((at, off, cmd));
            let m = f.bar_mask[i];
            let old = f.r32(off);
            f.w32(off, (v & m) | (old & !m));
        } else {
            f.w32(off, v);
        }
        true
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        if segment == 0 && (self.reach.0..=self.reach.1).contains(&bus) { self.space } else { 0 }
    }
    fn name(&self) -> &'static str {
        "fake"
    }
}
