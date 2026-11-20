//! GX-10: a bring-up owner's trail as Linux's trace would show the same bus accesses. Linux traces the logical
//! access (amdgpu_reg_access.c:363-386 and :494-514), so two things it does on the bus appear as one line there and
//! are folded here, each only in exactly Linux's form:
//!
//! - the HDP flush (amdgpu_hdp.c:51-60): a write of 0, then a read of the memory size register. Whatever register
//!   the 0 went to is kept, so a flush through another register than Linux's still differs;
//! - a register past BAR5 through the index pair: `amdgpu_device_indirect_rreg` (amdgpu_reg_access.c:590-608) is
//!   write index, read index, read data; `amdgpu_device_indirect_wreg` (:758-776) is write index, read index,
//!   write data, read data. An address inside BAR5 is never folded: Linux reaches those directly (:372).
//!
//! Anything else stays what it is: a wait with its condition and bound, a sleep with its length.

use crate::gpucore::psp::{Bound, Cond};
use crate::gpucore::trail::Access;
use crate::rdna4::regs14::Regs14;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The card's registers the folds need, as BAR5 bytes (from the card's own table, `Regs14`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bus {
    pub memsize: u32,
    pub index: u32,
    pub data: u32,
    pub bar5_bytes: u32,
}

impl Bus {
    pub fn of(r: &Regs14) -> Bus {
        Bus { memsize: r.memsize, index: r.rsmu_index, data: r.rsmu_data, bar5_bytes: rdna4_core::regmap::BAR5_BYTES }
    }
}

/// What a stage did, in the trace's terms (registers as dwords).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Did {
    Read { dword: u32, value: Option<u32> },
    Write { dword: u32, value: u32, reached: bool },
    Wait { dword: u32, cond: Cond, bound: Bound, reads: u32, last: Option<u32> },
    Hdp { flush: u32, mem: u32, value: Option<u32> },
    Sleep { us: u64 },
}

fn dword(byte: u32) -> Result<u32, String> {
    if byte.is_multiple_of(4) { Ok(byte / 4) } else { Err(format!("an access at byte {byte:#x}, which is no register")) }
}

pub fn fold(trail: &[Access], bus: &Bus) -> Result<Vec<Did>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < trail.len() {
        let rest = &trail[i..];
        let (did, used) = match *rest {
            [Access::Write { byte: ib, value: addr, reached: true }, Access::Read { byte: ib2, .. }, Access::Write { byte: db, value, reached }, Access::Read { byte: db2, .. }, ..]
                if ib == bus.index && ib2 == bus.index && db == bus.data && db2 == bus.data && addr >= bus.bar5_bytes =>
            {
                (Did::Write { dword: dword(addr)?, value, reached }, 4)
            }
            [Access::Write { byte: ib, value: addr, reached: true }, Access::Read { byte: ib2, .. }, Access::Read { byte: db, value }, ..]
                if ib == bus.index && ib2 == bus.index && db == bus.data && addr >= bus.bar5_bytes =>
            {
                (Did::Read { dword: dword(addr)?, value }, 3)
            }
            [Access::Write { byte, value: 0, reached: true }, Access::Read { byte: m, value }, ..] if m == bus.memsize => {
                (Did::Hdp { flush: dword(byte)?, mem: dword(m)?, value }, 2)
            }
            [Access::Read { byte, value }, ..] => (Did::Read { dword: dword(byte)?, value }, 1),
            [Access::Write { byte, value, reached }, ..] => (Did::Write { dword: dword(byte)?, value, reached }, 1),
            [Access::Wait { byte, cond, bound, reads, last, .. }, ..] => (Did::Wait { dword: dword(byte)?, cond, bound, reads, last }, 1),
            [Access::Sleep { us }, ..] => (Did::Sleep { us }, 1),
            [] => break,
        };
        out.push(did);
        i += used;
    }
    Ok(out)
}
