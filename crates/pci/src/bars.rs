//! BAR sizing (PCI 3.0 §6.2.5.1; Linux `__pci_read_base`): write all ones, read the mask back, restore.
//!
//! Safety of the sequence: I/O and memory decoding of the function are switched off around it, so the
//! all-ones address never decodes anything (host bridges keep decoding, Linux's `mmio_always_on`); every BAR
//! is restored before the next one and the command register last. Bus mastering is not touched (sizing does
//! not stop a running DMA). The caller holds the configuration lock for the whole sequence and must not log
//! inside it (the console may live in a BAR whose decoding is off for those microseconds).
//!
//! A function whose BAR registers all read 0 or all ones (most bridges and ports: nothing implemented or
//! nothing assigned) is not touched at all: switching a port's memory decoding off would stop it forwarding
//! to everything below it (the GPU's framebuffer, the NVMe) for nothing. A memory BAR that does not fit the
//! CPU's physical address bits (or wraps) is not handed to anyone: the slot stays empty with a problem.
//! Functions in D3hot are not sized here (`build` notes them; PC-2's `enable_device` wakes them first).

use crate::access::{read16, ConfigAccess, Loc};
use crate::node::{Bar, Bars, Problem};
use alloc::vec::Vec;

const CMD_IO: u32 = 1 << 0;
const CMD_MEM: u32 = 1 << 1;

/// Number of BAR slots for a header type (bit 7, multi-function, ignored): 6 for a device, 2 for a bridge,
/// none for CardBus (its 0x10 is the socket register block, not sized here).
pub fn slots(header_type: u8) -> usize {
    match header_type & 0x7F {
        0 => 6,
        1 => 2,
        _ => 0,
    }
}

/// Lowest set bit of a size mask: the decoded size (Linux `pci_size`; robust against masks with holes).
fn size_of(mask: u64) -> u64 {
    mask.isolate_lowest_one()
}

/// Current D state from the function's PM capability (0 = D0 ... 3 = D3hot); `None` without one.
pub fn power_state(c: &dyn ConfigAccess, at: Loc) -> Option<u8> {
    let status = read16(c, at, 6);
    let pm = crate::caps::walk_std(c, at, status, &mut Vec::new()).into_iter().find(|&(_, i)| i == crate::caps::id::PM)?;
    Some((read16(c, at, pm.0 as u16 + 4) & 3) as u8)
}

/// Sizes every BAR of the function. `keep_decode`: do not switch decoding off (host bridges).
/// `phys_limit`: first physical address past the CPU's address bits (`hal::mmio::phys_limit`).
pub fn size_bars(c: &dyn ConfigAccess, at: Loc, header_type: u8, keep_decode: bool, phys_limit: u64, problems: &mut Vec<Problem>) -> Bars {
    let mut out: Bars = [None; 6];
    let n = slots(header_type);
    if n == 0 {
        return out;
    }
    // Reads only: is there anything to size? 0 = not implemented (or a 32-bit BAR left at 0), all ones =
    // nothing answers.
    if (0..n).all(|i| matches!(c.read32(at, 0x10 + 4 * i as u16), 0 | u32::MAX)) {
        return out;
    }
    let cmd = read16(c, at, 4) as u32;
    // Status bits are write-1-to-clear: writing the low half with a zero upper half clears nothing.
    let decode = cmd & (CMD_IO | CMD_MEM);
    if !keep_decode && decode != 0 {
        c.write32(at, 4, cmd & !(CMD_IO | CMD_MEM));
    }
    let mut i = 0;
    while i < n {
        let off = 0x10 + 4 * i as u16;
        let orig = c.read32(at, off);
        if orig == u32::MAX {
            // A function that stopped answering (or a hostile all-ones register): no BAR there.
            i += 1;
            continue;
        }
        if orig & 1 == 1 {
            c.write32(at, off, u32::MAX);
            let back = c.read32(at, off);
            c.write32(at, off, orig);
            let mut mask = (back & !3) as u64;
            // 16-bit I/O decoders return 0 in the upper half (Linux `pci_size` for I/O).
            if mask & 0xFFFF_0000 == 0 && mask != 0 {
                mask |= 0xFFFF_0000;
            }
            let size = size_of(mask | 0xFFFF_FFFF_0000_0000);
            let addr = (orig & !3) as u64;
            if mask != 0 && size <= 0x1_0000 {
                if addr > 0xFFFF {
                    problems.push(Problem::BarIoTooHigh { index: i as u8, addr });
                } else {
                    out[i] = Some(Bar::Io { port: addr as u16, size: size as u32 });
                }
            }
            i += 1;
            continue;
        }
        let kind = (orig >> 1) & 3;
        if kind == 3 {
            problems.push(Problem::BarReservedType { index: i as u8 });
            i += 1;
            continue;
        }
        let is64 = kind == 2;
        if is64 && i + 1 >= n {
            problems.push(Problem::Bar64InLastSlot { index: i as u8 });
            i += 1;
            continue;
        }
        c.write32(at, off, u32::MAX);
        let lo = c.read32(at, off);
        c.write32(at, off, orig);
        let (hi_orig, hi) = if is64 {
            let h = c.read32(at, off + 4);
            c.write32(at, off + 4, u32::MAX);
            let hs = c.read32(at, off + 4);
            c.write32(at, off + 4, h);
            (h, hs)
        } else {
            (0, u32::MAX)
        };
        let mask = ((hi as u64) << 32) | (lo & !0xF) as u64;
        // An unimplemented BAR reads back 0 (the low flag bits do not count).
        if lo & !0xF != 0 || (is64 && hi != 0) {
            let phys = ((hi_orig as u64) << 32) | (orig & !0xF) as u64;
            let size = size_of(mask);
            // Unassigned (0) stays visible; an address the CPU cannot reach is never handed out.
            if phys != 0 && phys.checked_add(size).is_none_or(|end| end > phys_limit) {
                problems.push(Problem::BarBeyondPhys { index: i as u8, phys, size });
            } else {
                out[i] = Some(Bar::Mem { phys, size, prefetch: orig & 8 != 0, is64 });
            }
        }
        i += if is64 { 2 } else { 1 };
    }
    if !keep_decode && decode != 0 {
        c.write32(at, 4, cmd);
    }
    out
}
