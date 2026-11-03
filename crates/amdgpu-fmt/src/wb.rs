//! Writeback slots: a pinned buffer the engines write pointers and fences into, cut into 256-bit slots.
//!
//! Linux: 1024 slots of 8 dwords, zeroed once, handed out as dword offsets (amdgpu.h:481-492,
//! amdgpu_device.c:1031-1100). Here the slot count is the caller's (R1); a slot holds a 64-bit value at any
//! 8-byte-aligned dword of it (a fence, rptr, wptr poll value or COND_EXEC flag).

use alloc::vec;
use alloc::vec::Vec;

/// Dwords per slot.
pub const SLOT_DW: u64 = 8;
/// Bytes per slot.
pub const SLOT_BYTES: u64 = SLOT_DW * 4;

/// One slot: its index in the pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot(u32);

impl Slot {
    pub fn index(self) -> u32 {
        self.0
    }
    /// Dword offset in the buffer (what Linux's wb_get returns).
    pub fn dw_offset(self) -> u64 {
        self.0 as u64 * SLOT_DW
    }
    pub fn byte_offset(self) -> u64 {
        self.0 as u64 * SLOT_BYTES
    }
    /// GPU address of the slot given the buffer's GPU address.
    pub fn gpu_addr(self, base: u64) -> u64 {
        base.wrapping_add(self.byte_offset())
    }
}

/// Why the pool refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WbError {
    /// Every slot is taken (Linux: -EINVAL from amdgpu_device_wb_get; BitsOS says ENOSPC).
    Full,
    /// The slot is not in the pool or not handed out.
    NotAllocated(u32),
}

impl WbError {
    pub fn errno(&self) -> i32 {
        match self {
            WbError::Full => crate::errno::ENOSPC,
            WbError::NotAllocated(_) => crate::errno::EINVAL,
        }
    }
}

/// The slot allocator (bitmap).
#[derive(Clone, Debug)]
pub struct WbPool {
    bits: Vec<u64>,
    slots: u32,
    used: u32,
}

impl WbPool {
    /// A pool of `slots` slots (the buffer is `slots * SLOT_BYTES` bytes; the caller zeroes it once).
    pub fn new(slots: u32) -> WbPool {
        WbPool { bits: vec![0; slots.div_ceil(64) as usize], slots, used: 0 }
    }
    pub fn slots(&self) -> u32 {
        self.slots
    }
    pub fn used(&self) -> u32 {
        self.used
    }
    pub fn buffer_bytes(&self) -> u64 {
        self.slots as u64 * SLOT_BYTES
    }
    /// The lowest free slot.
    pub fn get(&mut self) -> Result<Slot, WbError> {
        for (w, word) in self.bits.iter_mut().enumerate() {
            if *word == u64::MAX {
                continue;
            }
            let b = (!*word).trailing_zeros();
            let i = w as u64 * 64 + b as u64;
            if i >= self.slots as u64 {
                break;
            }
            *word |= 1 << b;
            self.used += 1;
            return Ok(Slot(i as u32));
        }
        Err(WbError::Full)
    }
    pub fn is_used(&self, slot: Slot) -> bool {
        slot.0 < self.slots && self.bits[(slot.0 / 64) as usize] & (1 << (slot.0 % 64)) != 0
    }
    pub fn free(&mut self, slot: Slot) -> Result<(), WbError> {
        if !self.is_used(slot) {
            return Err(WbError::NotAllocated(slot.0));
        }
        self.bits[(slot.0 / 64) as usize] &= !(1 << (slot.0 % 64));
        self.used -= 1;
        Ok(())
    }
    /// The slot a dword offset names (Linux frees by dword offset): `None` unless it is a slot start.
    pub fn slot_at(&self, dw_offset: u64) -> Option<Slot> {
        (dw_offset.is_multiple_of(SLOT_DW) && dw_offset / SLOT_DW < self.slots as u64).then_some(Slot((dw_offset / SLOT_DW) as u32))
    }
}
