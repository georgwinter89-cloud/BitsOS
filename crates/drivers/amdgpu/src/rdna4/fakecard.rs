//! A card as the read-only attach meets it, built from the table measured on the Gaming-PC
//! (`rdna4_core::ipfacts::TABLE`): the two BAR5 dwords Linux reads before there is a table, and the discovery binary
//! at `memsize - 64 KiB` in VRAM, or at the place its mailbox names (DRIVER_SCRATCH_0..2) for a card whose firmware
//! put it elsewhere, and the PSP's runtime database 1 MiB below the end of VRAM with its SCPM entry. It refuses
//! what Linux would not do at this point: every write (register, memory, doorbell) fails and is kept, so a test
//! can show that none was even tried.

use crate::gpucore::disc;
use crate::gpucore::psp::Hw;
use alloc::vec;
use alloc::vec::Vec;
use rdna4_core::attach::{C2PMSG_33_DWORD, DRIVER_SCRATCH_0_DWORD, DRIVER_SCRATCH_2_DWORD, IFWI_READY, MEMSIZE_DWORD};
use rdna4_core::ipfacts::Entry;
use rdna4_core::layout;

/// STRAP0 as the measured card answered Linux, and where it lies.
pub const STRAP0_SEEN: u32 = 0x310C_7550;
const STRAP0_DWORD: u32 = 0xD3C;

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

/// A discovery binary (include/discovery.h: binary header, IP table version 4 with one die, 32-bit bases) holding
/// `table`, every checksum right, `len` bytes long (the reserved region is 10 KiB).
pub fn blob(table: &[Entry], len: usize) -> Vec<u8> {
    let mut b = vec![0u8; 60];
    b[0..4].copy_from_slice(&disc::BINARY_SIGNATURE.to_le_bytes());
    put16(&mut b, 4, 1);
    let ip = b.len();
    b.extend_from_slice(&disc::IP_TABLE_SIGNATURE.to_le_bytes());
    b.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0, 1, 0]); // version 4, size (below), id, one die
    let die_info = b.len();
    b.resize(die_info + 64, 0);
    b.extend_from_slice(&[0, 0]); // 32-bit bases
    let die = b.len();
    put16(&mut b, die_info + 2, die as u16);
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&(table.len() as u16).to_le_bytes());
    for e in table {
        b.extend_from_slice(&e.hw_id.to_le_bytes());
        b.extend_from_slice(&[e.instance, e.bases.len() as u8, e.version.major, e.version.minor, e.version.rev, 0]);
        for base in e.bases {
            b.extend_from_slice(&base.to_le_bytes());
        }
    }
    let ip_size = b.len() - ip;
    put16(&mut b, ip + 6, ip_size as u16);
    let size = b.len();
    put16(&mut b, 10, size as u16);
    put16(&mut b, 12, ip as u16);
    let sum = disc::checksum(&b[ip..ip + ip_size]);
    put16(&mut b, 14, sum);
    put16(&mut b, 16, ip_size as u16);
    let whole = disc::checksum(&b[10..]);
    put16(&mut b, 8, whole);
    b.resize(len.max(size), 0);
    b
}

/// The GC info table the fake card's binary carries (version 1.0, `struct gc_info_v1_0`, include/discovery.h): 4
/// shader engines of 2 arrays, 4 WGPs (8 CUs) per array, 4 render backends per engine, every other field 0.
/// Invented to give the trace's layout (8 GRBM_GFX_INDEX selections, gc-cp.txt); the real table's version and
/// values are read at the hardware round (plan/spec/gpu-rdna4.md section 7, `gc-info`).
pub const GC_INFO: [(usize, u32); 5] = [(0, 4), (1, 4), (2, 0), (3, 4), (16, 2)];
const GC_INFO_FIELDS: usize = 19;

/// `b` (a binary of `blob`, its length cut to what it holds) with the GC info table behind it, the header's
/// table list and both checksums right.
pub fn with_gc_info(mut b: Vec<u8>, len: usize) -> Vec<u8> {
    let size = usize::from(u16::from_le_bytes([b[10], b[11]]));
    b.truncate(size);
    let at = b.len();
    let bytes = 12 + GC_INFO_FIELDS * 4;
    b.extend_from_slice(&disc::GC_TABLE_ID.to_le_bytes());
    b.extend_from_slice(&[1, 0, 0, 0]);
    b.extend_from_slice(&(bytes as u32).to_le_bytes());
    let mut fields = [0u32; GC_INFO_FIELDS];
    for (i, v) in GC_INFO {
        fields[i] = v;
    }
    b.extend(fields.iter().flat_map(|v| v.to_le_bytes()));
    put16(&mut b, 20, at as u16);
    let sum = disc::checksum(&b[at..at + bytes]);
    put16(&mut b, 22, sum);
    put16(&mut b, 24, bytes as u16);
    let size = b.len();
    put16(&mut b, 10, size as u16);
    let whole = disc::checksum(&b[10..]);
    put16(&mut b, 8, whole);
    b.resize(len.max(size), 0);
    b
}

/// What the fake kept of a write that reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tried {
    Reg { byte: u32, value: u32 },
    Mem { at: u64, bytes: usize },
    Doorbell { index_dw: u32 },
}

pub struct FakeCard {
    /// `RCC_CONFIG_MEMSIZE` in MiB.
    pub memsize: u32,
    /// The discovery binary, at `memsize - 64 KiB` in VRAM unless `blob_at` says otherwise.
    pub blob: Vec<u8>,
    /// The VRAM offset the binary lies at instead of the default place (which then reads zeros).
    pub blob_at: Option<u64>,
    /// DRIVER_SCRATCH_0..2: the offset (low, high) and size the card's mailbox names; all 0 on the measured card.
    pub scratch: [u32; 3],
    /// The PSP's runtime database, from its header on, at `memsize - 1 MiB` (`rdna4_core::scpm`); empty: none.
    pub runtime_db: Vec<u8>,
    /// Milliseconds until C2PMSG_33 shows the card's firmware ready (0: at once; `u64::MAX`: never).
    pub ready_after_ms: u64,
    /// BAR5 reads all ones (a card that does not answer).
    pub gone: bool,
    pub tried: Vec<Tried>,
    pub reg_reads: Vec<u32>,
    /// (VRAM offset, bytes) of every memory read.
    pub mem_reads: Vec<(u64, usize)>,
    pub now_us: u64,
    /// `RCC_STRAP0_RCC_DEV0_EPF0_STRAP0` (BAR5 dword 0xd3c): the trace's value (discovery.txt line 6), revision 1.
    pub strap0: u32,
}

impl FakeCard {
    /// The card as measured: 16304 MiB, the 46-entry table, ready at once; its runtime database says SCPM is on
    /// (Linux's start of the power controller on this card sends what only an SCPM card gets, the trace's
    /// section smu).
    pub fn measured() -> FakeCard {
        FakeCard::with(layout::MEMSIZE_MIB_SEEN, with_gc_info(blob(&rdna4_core::ipfacts::TABLE, 0), layout::DISCOVERY_READ))
    }

    pub fn with(memsize: u32, blob: Vec<u8>) -> FakeCard {
        FakeCard { memsize, blob, blob_at: None, scratch: [0; 3], runtime_db: rdna4_core::scpm::synthetic(1).to_vec(), ready_after_ms: 0, gone: false, tried: Vec::new(), reg_reads: Vec::new(), mem_reads: Vec::new(), now_us: 0, strap0: STRAP0_SEEN }
    }

    /// The measured card with its table at `offset` instead, and a mailbox that says so with `size` bytes.
    pub fn with_mailbox(offset: u64, size: u32) -> FakeCard {
        FakeCard { blob_at: Some(offset), scratch: [offset as u32, (offset >> 32) as u32, size], ..FakeCard::measured() }
    }

    fn blob_start(&self) -> u64 {
        self.blob_at.unwrap_or((u64::from(self.memsize) << 20).saturating_sub(layout::DISCOVERY_FROM_END))
    }
}

impl Hw for FakeCard {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.reg_reads.push(byte);
        if self.gone {
            return Some(u32::MAX);
        }
        Some(match byte / 4 {
            C2PMSG_33_DWORD if self.now_us / 1000 >= self.ready_after_ms => IFWI_READY,
            MEMSIZE_DWORD => self.memsize,
            STRAP0_DWORD => self.strap0,
            d @ DRIVER_SCRATCH_0_DWORD..=DRIVER_SCRATCH_2_DWORD => self.scratch[(d - DRIVER_SCRATCH_0_DWORD) as usize],
            _ => 0,
        })
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.tried.push(Tried::Reg { byte, value });
        false
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.mem_reads.push((at, buf.len()));
        let start = self.blob_start();
        let db = (u64::from(self.memsize) << 20).saturating_sub(rdna4_core::scpm::FROM_END);
        for (i, out) in buf.iter_mut().enumerate() {
            let pos = at + i as u64;
            let of = |from: u64, bytes: &Vec<u8>| pos.checked_sub(from).and_then(|o| bytes.get(o as usize)).copied();
            *out = of(start, &self.blob).or_else(|| of(db, &self.runtime_db)).unwrap_or(0);
        }
        true
    }
    fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.tried.push(Tried::Mem { at, bytes: data.len() });
        false
    }
    fn now_us(&mut self) -> u64 {
        self.now_us
    }
    fn delay_us(&mut self, us: u64) {
        self.now_us += us;
    }
    fn doorbell_write64(&mut self, index_dw: u32, _value: u64) -> bool {
        self.tried.push(Tried::Doorbell { index_dw });
        false
    }
}
