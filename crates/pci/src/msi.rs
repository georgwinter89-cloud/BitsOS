//! PC-3, pure part: the MSI and MSI-X capabilities over any configuration back end and any MSI-X table (the
//! host tests drive both with fakes; `irq.rs` is the kernel side with vectors, threads and events).
//!
//! Layouts (PCI Local Bus 3.0 §6.8, PCIe 6.0 §7.7.1-7.7.2; V1 `apic/msicap.asm`):
//! - MSI: dword at the capability = id, next, **message control** (bit 0 enable, 3:1 messages capable as log2,
//!   6:4 messages enabled as log2, 7 64-bit address, 8 per-vector masking); then address low (+4), for the 64-bit
//!   form address high (+8); data (+8 or +0xC, 16 bits; the upper half is Extended Message Data, kept as read);
//!   with per-vector masking the mask bits (+0xC / +0x10) and pending bits (+0x10 / +0x14).
//! - MSI-X: message control (bits 10:0 table size - 1, 14 function mask, 15 enable), table offset/BIR (+4),
//!   PBA offset/BIR (+8). Table entries are 16 bytes: address low, address high, data, vector control (bit 0
//!   masked); the PBA has one bit per entry.
//!
//! Order rules (Linux `msi_capability_init`, `msix_capability_init`, `pci_msix_write_vector_ctrl`):
//! - MSI: everything masked and the message written while MSI is off, multiple message enable set, then MSI on.
//! - MSI-X: enable **with the function mask set** (no entry can fire while they change), every entry masked,
//!   the used entries written and unmasked (each write read back), then the function mask cleared.
//! - An entry's address/data change only while that entry is masked; a plain MSI without per-vector masking
//!   cannot be masked, so a change there is done with MSI off for a moment (`irq.rs` then wakes the owner once,
//!   a message may have been dropped).
//!
//! Control registers change by read-modify-write of their dword (`ConfigAccess::modify32`, atomic under
//! `pci.cfg` in the kernel), keeping the read-only bits and the capability header as read.

use crate::access::{read16, ConfigAccess, Loc};
use hal::msi::Message;

pub const MSI_ENABLE: u32 = 1 << 16;
pub const MSI_MME_SHIFT: u32 = 20;
pub const MSI_MME_MASK: u32 = 7 << MSI_MME_SHIFT;
pub const MSIX_ENABLE: u32 = 1 << 31;
pub const MSIX_FUNCTION_MASK: u32 = 1 << 30;
/// Vector control bit 0.
pub const ENTRY_MASKED: u32 = hal::msi::MSIX_CTRL_MASKED;
pub const ENTRY_SIZE: usize = 16;

/// An MSI capability as the function describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiLayout {
    pub cap: u8,
    pub is64: bool,
    pub maskbit: bool,
    /// Messages the function can send, as log2 (0..=5; reserved encodings 6 and 7 read as 5).
    pub capable_log2: u8,
}

impl MsiLayout {
    pub fn read(c: &dyn ConfigAccess, at: Loc, cap: u8) -> Option<MsiLayout> {
        let ctrl = read16(c, at, cap as u16 + 2);
        if ctrl == 0xFFFF {
            return None;
        }
        Some(MsiLayout { cap, is64: ctrl & 0x80 != 0, maskbit: ctrl & 0x100 != 0, capable_log2: ((ctrl >> 1) & 7).min(5) as u8 })
    }
    pub fn capable(&self) -> u16 {
        1 << self.capable_log2
    }
    fn off(&self, rel: u16) -> u16 {
        self.cap as u16 + rel
    }
    pub fn data_off(&self) -> u16 {
        self.off(if self.is64 { 0x0C } else { 0x08 })
    }
    /// Mask bits register (only with `maskbit`).
    pub fn mask_off(&self) -> Option<u16> {
        self.maskbit.then(|| self.off(if self.is64 { 0x10 } else { 0x0C }))
    }
    pub fn pending_off(&self) -> Option<u16> {
        self.maskbit.then(|| self.off(if self.is64 { 0x14 } else { 0x10 }))
    }
}

/// The messages to enable for a request of `want` (at least 1): the largest power of two not above `want`
/// nor above what the function can send, as log2.
pub fn msi_block_log2(want: u16, capable_log2: u8) -> u8 {
    let w = want.max(1);
    let floor = 15 - w.leading_zeros() as u8;
    floor.min(capable_log2)
}

/// Turns MSI on or off; the previous enable bit.
pub fn msi_set_enable(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout, on: bool) -> bool {
    let mut was = false;
    c.modify32(at, l.cap as u16, &mut |d| {
        if d == u32::MAX {
            return None;
        }
        was = d & MSI_ENABLE != 0;
        let new = if on { d | MSI_ENABLE } else { d & !MSI_ENABLE };
        (new != d).then_some(new)
    });
    was
}

/// Writes the message (address, then data; the data dword's upper half is kept).
pub fn msi_write_message(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout, m: Message) {
    c.write32(at, l.off(4), m.address_lo());
    if l.is64 {
        c.write32(at, l.off(8), m.address_hi());
    }
    c.modify32(at, l.data_off(), &mut |d| (d != u32::MAX).then_some((d & 0xFFFF_0000) | (m.data & 0xFFFF)));
}

/// The message as the function holds it now.
pub fn msi_read_message(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout) -> Message {
    let lo = c.read32(at, l.off(4)) as u64;
    let hi = if l.is64 { c.read32(at, l.off(8)) as u64 } else { 0 };
    Message { address: hi << 32 | lo, data: c.read32(at, l.data_off()) & 0xFFFF }
}

/// Sets the mask bits register (no-op without per-vector masking).
pub fn msi_write_mask(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout, mask: u32) {
    if let Some(o) = l.mask_off() {
        c.write32(at, o, mask);
    }
}

/// Sets or clears one mask bit (read-modify-write); false without per-vector masking.
pub fn msi_mask_one(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout, index: u16, masked: bool) -> bool {
    let Some(o) = l.mask_off() else { return false };
    let bit = 1u32 << (index & 31);
    c.modify32(at, o, &mut |d| {
        let new = if masked { d | bit } else { d & !bit };
        (new != d).then_some(new)
    });
    true
}

pub fn msi_read_mask(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout) -> Option<u32> {
    l.mask_off().map(|o| c.read32(at, o))
}

pub fn msi_read_pending(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout) -> Option<u32> {
    l.pending_off().map(|o| c.read32(at, o))
}

/// Control as read: (enabled, messages enabled as log2).
pub fn msi_state(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout) -> (bool, u8) {
    let d = c.read32(at, l.cap as u16);
    (d != u32::MAX && d & MSI_ENABLE != 0, ((d & MSI_MME_MASK) >> MSI_MME_SHIFT) as u8)
}

/// MSI set up from off: off, every message masked (`mask` then names the masked ones that stay masked), the
/// message written, `block_log2` messages enabled, then on. Returns the control dword after.
pub fn msi_program(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout, m: Message, block_log2: u8, mask: u32) -> u32 {
    msi_set_enable(c, at, l, false);
    msi_write_mask(c, at, l, u32::MAX);
    msi_write_message(c, at, l, m);
    c.modify32(at, l.cap as u16, &mut |d| {
        (d != u32::MAX).then_some((d & !MSI_MME_MASK) | ((block_log2 as u32 & 7) << MSI_MME_SHIFT))
    });
    msi_write_mask(c, at, l, mask);
    msi_set_enable(c, at, l, true);
    c.read32(at, l.cap as u16)
}

/// MSI off and the multiple message enable back to one (V1 `msi_clear`: `and 0x71` off).
pub fn msi_disable(c: &dyn ConfigAccess, at: Loc, l: &MsiLayout) {
    c.modify32(at, l.cap as u16, &mut |d| {
        let new = d & !(MSI_ENABLE | MSI_MME_MASK);
        (d != u32::MAX && new != d).then_some(new)
    });
}

/// An MSI-X capability as the function describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixLayout {
    pub cap: u8,
    /// Table entries (1..=2048).
    pub size: u16,
    pub table_bir: u8,
    pub table_offset: u32,
    pub pba_bir: u8,
    pub pba_offset: u32,
}

impl MsixLayout {
    pub fn read(c: &dyn ConfigAccess, at: Loc, cap: u8) -> Option<MsixLayout> {
        let d = c.read32(at, cap as u16);
        if d == u32::MAX {
            return None;
        }
        let t = c.read32(at, cap as u16 + 4);
        let p = c.read32(at, cap as u16 + 8);
        Some(MsixLayout {
            cap,
            size: ((d >> 16) & 0x7FF) as u16 + 1,
            table_bir: (t & 7) as u8,
            table_offset: t & !7,
            pba_bir: (p & 7) as u8,
            pba_offset: p & !7,
        })
    }
    pub fn table_bytes(&self) -> usize {
        self.size as usize * ENTRY_SIZE
    }
    /// PBA: one bit per entry, in qwords.
    pub fn pba_bytes(&self) -> usize {
        (self.size as usize).div_ceil(64) * 8
    }
}

/// The MSI-X table (and PBA) as memory: the kernel's `IoMem`, a fake in host tests. Offsets are bytes inside
/// the mapped window; out-of-range accesses fail instead of touching anything.
pub trait Table: Sync {
    fn read32(&self, off: usize) -> Option<u32>;
    fn write32(&self, off: usize, v: u32) -> bool;
}

impl Table for hal::mmio::IoMem {
    fn read32(&self, off: usize) -> Option<u32> {
        hal::mmio::IoMem::read32(self, off).ok()
    }
    fn write32(&self, off: usize, v: u32) -> bool {
        hal::mmio::IoMem::write32(self, off, v).is_ok()
    }
}

/// Byte offset of entry `i`'s vector control dword.
pub fn ctrl_off(i: u16) -> usize {
    i as usize * ENTRY_SIZE + 12
}

/// Writes entry `i`: masked first if it is not, address and data, then the vector control as `masked` says;
/// the last write is read back so it reached the device before anything that follows (Linux
/// `pci_msix_write_vector_ctrl` + `readl`).
pub fn msix_write_entry(t: &dyn Table, i: u16, m: Message, masked: bool) -> bool {
    let base = i as usize * ENTRY_SIZE;
    let ctrl = t.read32(base + 12).unwrap_or(0);
    let mut ok = true;
    if ctrl & ENTRY_MASKED == 0 {
        ok &= t.write32(base + 12, ctrl | ENTRY_MASKED);
    }
    ok &= t.write32(base, m.address_lo());
    ok &= t.write32(base + 4, m.address_hi());
    ok &= t.write32(base + 8, m.data);
    let want = if masked { ctrl | ENTRY_MASKED } else { ctrl & !ENTRY_MASKED };
    ok &= t.write32(base + 12, want);
    let _ = t.read32(base + 12);
    ok
}

pub fn msix_read_entry(t: &dyn Table, i: u16) -> Option<(Message, bool)> {
    let base = i as usize * ENTRY_SIZE;
    let lo = t.read32(base)? as u64;
    let hi = t.read32(base + 4)? as u64;
    let data = t.read32(base + 8)?;
    let ctrl = t.read32(base + 12)?;
    Some((Message { address: hi << 32 | lo, data }, ctrl & ENTRY_MASKED != 0))
}

/// Masks or unmasks entry `i` (the other control bits kept), read back.
pub fn msix_mask_entry(t: &dyn Table, i: u16, masked: bool) -> bool {
    let off = ctrl_off(i);
    let Some(ctrl) = t.read32(off) else { return false };
    let want = if masked { ctrl | ENTRY_MASKED } else { ctrl & !ENTRY_MASKED };
    let ok = want == ctrl || t.write32(off, want);
    let _ = t.read32(off);
    ok
}

/// Pending bit of entry `i` (`pba`: the mapped PBA, offset 0 = its start).
pub fn msix_pending(pba: &dyn Table, i: u16) -> Option<bool> {
    let d = pba.read32((i as usize / 32) * 4)?;
    Some(d & (1 << (i % 32)) != 0)
}

/// Sets the MSI-X enable and function mask bits (the table size and the header kept); the control dword
/// before.
pub fn msix_control(c: &dyn ConfigAccess, at: Loc, l: &MsixLayout, enable: bool, function_mask: bool) -> u32 {
    c.modify32(at, l.cap as u16, &mut |d| {
        if d == u32::MAX {
            return None;
        }
        let mut new = d & !(MSIX_ENABLE | MSIX_FUNCTION_MASK);
        if enable {
            new |= MSIX_ENABLE;
        }
        if function_mask {
            new |= MSIX_FUNCTION_MASK;
        }
        (new != d).then_some(new)
    })
}

/// Control as read: (enabled, function masked).
pub fn msix_state(c: &dyn ConfigAccess, at: Loc, l: &MsixLayout) -> (bool, bool) {
    let d = c.read32(at, l.cap as u16);
    (d != u32::MAX && d & MSIX_ENABLE != 0, d != u32::MAX && d & MSIX_FUNCTION_MASK != 0)
}

/// MSI-X set up: enabled with the function mask, every entry masked, `entries` (index, message, masked)
/// written, function mask cleared. False if a table access failed (nothing is left half-enabled then: MSI-X
/// is switched off again).
pub fn msix_program(c: &dyn ConfigAccess, at: Loc, l: &MsixLayout, t: &dyn Table, entries: &[(u16, Message, bool)]) -> bool {
    msix_control(c, at, l, true, true);
    let mut ok = true;
    for i in 0..l.size {
        ok &= msix_mask_entry(t, i, true);
    }
    for &(i, m, masked) in entries {
        ok &= i < l.size && msix_write_entry(t, i, m, masked);
    }
    if !ok {
        msix_control(c, at, l, false, false);
        return false;
    }
    msix_control(c, at, l, true, false);
    true
}

/// MSI-X off: the used entries masked, then enable cleared and the function mask left set as after reset
/// (V1 `msi_cap_off`; Linux `pci_msix_shutdown`).
pub fn msix_disable(c: &dyn ConfigAccess, at: Loc, l: &MsixLayout, t: Option<&dyn Table>, used: &[u16]) {
    if let Some(t) = t {
        for &i in used {
            msix_mask_entry(t, i, true);
        }
    }
    msix_control(c, at, l, false, true);
}

/// Command register: INTx disable on (bit 10); the command before.
pub fn intx_off(c: &dyn ConfigAccess, at: Loc) -> u16 {
    crate::access::modify_command(c, at, |cmd| {
        (cmd != 0xFFFF && cmd & crate::enable::CMD_INTX_OFF == 0).then_some(cmd | crate::enable::CMD_INTX_OFF)
    })
}

/// Where a request ends up: MSI-X when allowed and present, else MSI, else polled (the routed I/O APIC pins are
/// CP's and only carry the SCI and PS/2; PCI INTx stays off, CP-3 notes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    MsiX { n: u16 },
    Msi { block_log2: u8 },
    Polled,
}

/// The plan for `min..=max` vectors with what the function offers. `msix_usable`: the table lies in a memory
/// BAR (the scan's `msix.outside_bar` check passed). MSI-X gives `min(max, size)` when that reaches `min`;
/// MSI gives a power-of-two block when it reaches `min`. None: nothing fits and polling is not allowed.
pub fn plan(min: u16, max: u16, msix: Option<(u16, bool)>, msi: Option<u8>, allow: (bool, bool, bool)) -> Option<Plan> {
    let (allow_msix, allow_msi, allow_polled) = allow;
    let min = min.max(1);
    let max = max.max(min);
    if allow_msix
        && let Some((size, true)) = msix
        && size >= min
    {
        return Some(Plan::MsiX { n: max.min(size) });
    }
    if allow_msi && let Some(cap_log2) = msi {
        let b = msi_block_log2(max, cap_log2);
        if 1u16 << b >= min {
            return Some(Plan::Msi { block_log2: b });
        }
    }
    allow_polled.then_some(Plan::Polled)
}

/// An aligned run of `n` (a power of two) vectors among `got` (vector numbers on one CPU): the first `base`
/// with `base % n == 0` and every `base..base + n` in `got`.
pub fn aligned_run(got: &[u8], n: u16) -> Option<u8> {
    let n = n.max(1) as u32;
    got.iter().copied().filter(|&b| (b as u32).is_multiple_of(n)).find(|&b| (0..n).all(|k| b as u32 + k <= 0xFF && got.contains(&((b as u32 + k) as u8))))
}
