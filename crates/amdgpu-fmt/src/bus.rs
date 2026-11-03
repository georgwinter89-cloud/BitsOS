//! The memory and register bus the interpreters ([`crate::sim`]) run on, and [`SimMem`], a plain-memory bus
//! for the tests and the fakes.
//!
//! Addresses are GPU virtual addresses in an address space named by its VMID (0 = the kernel's GART/system
//! aperture). How a bus translates them is its business: GX's chip fake puts GR-1's walker (VMID > 0) and GART
//! (VMID 0) behind it, the tests use [`SimMem`]'s mapped regions. A refused access is a [`MemFault`], what the
//! hardware reports as a VM fault.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

/// An access the bus refused (unmapped, or not writable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault {
    pub vmid: u8,
    pub addr: u64,
    pub write: bool,
}

/// Memory and registers as an engine sees them.
pub trait GpuBus {
    /// Fills `out` from `addr`. On a fault nothing useful is in `out`.
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault>;
    /// Writes `data` at `addr`; a fault may leave part of it written (hardware does too).
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault>;
    /// GX-7m2 (change request to GR): an engine fetching its commands (ring or IB dwords). A bus that models
    /// page permissions needs the EXECUTABLE bit for it (the 780M refused an IB page without it, 29.10.:
    /// PERMISSION_FAULTS 0x8, a read by CPF); the default is a plain read. A refused fetch is a read fault.
    fn fetch(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.read(vmid, addr, out)
    }
    /// A register (dword offset); `None` when the bus does not model it.
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        let _ = reg;
        None
    }
    /// A register write; `false` when the bus does not model the register (the engine still reports it).
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        let _ = (reg, value);
        false
    }
}

pub(crate) fn read_u32<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, addr: u64) -> Result<u32, MemFault> {
    let mut b = [0u8; 4];
    bus.read(vmid, addr, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Why [`SimMem::map`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Length 0 or the range wraps the address space.
    BadRange,
    /// The range overlaps the region starting at this address.
    Overlap(u64),
}

/// Sparse plain memory: zero-filled regions per VMID (no fixed size or count), registers in a map.
/// Accesses may span adjacent regions; any byte outside a region faults (a write then changes nothing).
#[derive(Clone, Debug, Default)]
pub struct SimMem {
    regions: BTreeMap<(u8, u64), Vec<u8>>,
    regs: BTreeMap<u32, u32>,
    mirrors: BTreeMap<u32, u32>,
    /// Regions that refuse writes (read-only mappings), by (vmid, start).
    read_only: BTreeMap<(u8, u64), ()>,
}

impl SimMem {
    pub fn new() -> SimMem {
        SimMem::default()
    }

    /// Maps `len` zero bytes at `addr` in `vmid`.
    pub fn map(&mut self, vmid: u8, addr: u64, len: usize) -> Result<(), MapError> {
        let end = addr.checked_add(len as u64).ok_or(MapError::BadRange)?;
        if len == 0 {
            return Err(MapError::BadRange);
        }
        if let Some((&(v, s), r)) = self.regions.range(..(vmid, end)).next_back()
            && v == vmid
            && s + r.len() as u64 > addr
        {
            return Err(MapError::Overlap(s));
        }
        self.regions.insert((vmid, addr), vec![0; len]);
        Ok(())
    }

    /// Like [`SimMem::map`], but writes fault.
    pub fn map_read_only(&mut self, vmid: u8, addr: u64, len: usize) -> Result<(), MapError> {
        self.map(vmid, addr, len)?;
        self.read_only.insert((vmid, addr), ());
        Ok(())
    }

    /// Removes the region starting at `addr`.
    pub fn unmap(&mut self, vmid: u8, addr: u64) -> bool {
        self.read_only.remove(&(vmid, addr));
        self.regions.remove(&(vmid, addr)).is_some()
    }

    /// The region holding `addr`: (start, bytes).
    fn find(&self, vmid: u8, addr: u64) -> Option<(u64, &Vec<u8>)> {
        let (&(v, s), r) = self.regions.range(..=(vmid, addr)).next_back()?;
        (v == vmid && addr - s < r.len() as u64).then_some((s, r))
    }

    /// Checks the whole range is mapped (and writable for a write); returns the first bad byte otherwise.
    fn covered(&self, vmid: u8, addr: u64, len: usize, write: bool) -> Result<(), u64> {
        let mut cur = addr;
        let mut left = len as u64;
        while left > 0 {
            let (s, r) = self.find(vmid, cur).ok_or(cur)?;
            if write && self.read_only.contains_key(&(vmid, s)) {
                return Err(cur);
            }
            let n = (s + r.len() as u64 - cur).min(left);
            left -= n;
            cur = cur.checked_add(n).ok_or(cur)?;
        }
        Ok(())
    }

    /// CPU read (no fault): `None` when any byte is unmapped.
    pub fn peek(&self, vmid: u8, addr: u64, out: &mut [u8]) -> Option<()> {
        self.covered(vmid, addr, out.len(), false).ok()?;
        let mut cur = addr;
        let mut done = 0;
        while done < out.len() {
            let (s, r) = self.find(vmid, cur)?;
            let off = (cur - s) as usize;
            let n = (r.len() - off).min(out.len() - done);
            out[done..done + n].copy_from_slice(&r[off..off + n]);
            done += n;
            cur += n as u64;
        }
        Some(())
    }

    /// CPU write (ignores read-only): `None` when any byte is unmapped; then nothing changed.
    pub fn poke(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Option<()> {
        self.covered(vmid, addr, data.len(), false).ok()?;
        let mut cur = addr;
        let mut done = 0;
        while done < data.len() {
            let (s, len) = self.find(vmid, cur).map(|(s, r)| (s, r.len()))?;
            let r = self.regions.get_mut(&(vmid, s))?;
            let off = (cur - s) as usize;
            let n = (len - off).min(data.len() - done);
            r[off..off + n].copy_from_slice(&data[done..done + n]);
            done += n;
            cur += n as u64;
        }
        Some(())
    }

    pub fn peek_u32(&self, vmid: u8, addr: u64) -> Option<u32> {
        let mut b = [0; 4];
        self.peek(vmid, addr, &mut b)?;
        Some(u32::from_le_bytes(b))
    }
    pub fn peek_u64(&self, vmid: u8, addr: u64) -> Option<u64> {
        let mut b = [0; 8];
        self.peek(vmid, addr, &mut b)?;
        Some(u64::from_le_bytes(b))
    }
    /// Writes dwords (little endian) from `addr`.
    pub fn poke_dwords(&mut self, vmid: u8, addr: u64, dws: &[u32]) -> Option<()> {
        let bytes: Vec<u8> = dws.iter().flat_map(|d| d.to_le_bytes()).collect();
        self.poke(vmid, addr, &bytes)
    }

    /// Sets a register's value (it is then modelled).
    pub fn set_reg(&mut self, reg: u32, value: u32) {
        self.regs.insert(reg, value);
    }
    pub fn reg(&self, reg: u32) -> Option<u32> {
        self.regs.get(&reg).copied()
    }
    /// Every write to `from` also lands in `to` (e.g. an HDP flush request register and its done register).
    pub fn mirror(&mut self, from: u32, to: u32) {
        self.mirrors.insert(from, to);
    }
}

impl GpuBus for SimMem {
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        if let Err(a) = self.covered(vmid, addr, out.len(), false) {
            return Err(MemFault { vmid, addr: a, write: false });
        }
        self.peek(vmid, addr, out).ok_or(MemFault { vmid, addr, write: false })
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        if let Err(a) = self.covered(vmid, addr, data.len(), true) {
            return Err(MemFault { vmid, addr: a, write: true });
        }
        self.poke(vmid, addr, data).ok_or(MemFault { vmid, addr, write: true })
    }
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        self.reg(reg)
    }
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        self.regs.insert(reg, value);
        if let Some(&to) = self.mirrors.get(&reg) {
            self.regs.insert(to, value);
        }
        true
    }
}

/// What one page of a GPU address translates to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Translation {
    /// Host-physical address of the byte.
    Phys(u64),
    /// A PRT range: reads give zero, writes are dropped.
    Prt,
    Fault,
}

/// Physical memory behind a [`Translated`] bus (and its registers, if it models any).
pub trait PhysMem {
    fn read_phys(&mut self, phys: u64, out: &mut [u8]) -> bool;
    fn write_phys(&mut self, phys: u64, data: &[u8]) -> bool;
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        let _ = reg;
        None
    }
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        let _ = (reg, value);
        false
    }
}

/// A bus that translates every GPU page with `translate(vmid, addr, write)` (e.g. [`vm_translation`] for a
/// user VM, [`gart_translation`] for VMID 0) and reaches physical memory through `mem`. Accesses are split at
/// GPU page boundaries.
pub struct Translated<M, F> {
    pub mem: M,
    pub translate: F,
}

impl<M: PhysMem, F: FnMut(u8, u64, bool) -> Translation> Translated<M, F> {
    fn access(&mut self, vmid: u8, addr: u64, len: usize, write: bool, mut f: impl FnMut(&mut M, u64, usize, usize) -> bool) -> Result<(), MemFault> {
        let mut done = 0usize;
        while done < len {
            let cur = addr.checked_add(done as u64).ok_or(MemFault { vmid, addr, write })?;
            let n = ((crate::GPU_PAGE - (cur & (crate::GPU_PAGE - 1))) as usize).min(len - done);
            let fault = MemFault { vmid, addr: cur, write };
            match (self.translate)(vmid, cur, write) {
                Translation::Phys(p) => {
                    if !f(&mut self.mem, p, done, n) {
                        return Err(fault);
                    }
                }
                Translation::Prt => {
                    if !write && !f(&mut self.mem, u64::MAX, done, n) {
                        return Err(fault);
                    }
                }
                Translation::Fault => return Err(fault),
            }
            done += n;
        }
        Ok(())
    }
}

impl<M: PhysMem, F: FnMut(u8, u64, bool) -> Translation> GpuBus for Translated<M, F> {
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        let len = out.len();
        self.access(vmid, addr, len, false, |m, p, off, n| {
            if p == u64::MAX {
                out[off..off + n].fill(0);
                true
            } else {
                m.read_phys(p, &mut out[off..off + n])
            }
        })
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.access(vmid, addr, data.len(), true, |m, p, off, n| m.write_phys(p, &data[off..off + n]))
    }
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        self.mem.reg_read(reg)
    }
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        self.mem.reg_write(reg, value)
    }
}

/// Translation through GR-1's page tables (the walker as the hardware's UTCL2): a page needs the read or write
/// permission the access asks for.
pub fn vm_translation<R: crate::store::TableRead + ?Sized>(tables: &R, root: u64, va: u64, write: bool) -> Translation {
    match crate::walk::walk(tables, root, va) {
        Ok(crate::walk::Walk::Page(l)) if (write && l.flags.write) || (!write && l.flags.read) => Translation::Phys(l.phys),
        Ok(crate::walk::Walk::Prt { .. }) => Translation::Prt,
        _ => Translation::Fault,
    }
}

/// Translation through GR-1's GART (VMID 0).
pub fn gart_translation(gart: &crate::gart::Gart, table: &dyn crate::gart::GartRead, mc: u64) -> Translation {
    match gart.translate(table, mc) {
        Ok(Some((p, _))) => Translation::Phys(p),
        _ => Translation::Fault,
    }
}
