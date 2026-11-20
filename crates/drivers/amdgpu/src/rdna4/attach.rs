//! The read-only attach of one card, in Linux's order for the same reads (linux-7.3.1 amdgpu_device.c:3907 maps BAR5;
//! amdgpu_discovery.c:289-372 `amdgpu_discovery_get_tmr_info` and `amdgpu_discovery_read_binary_from_mem`): PCI
//! facts, the firmware-ready bit, the memory size, the discovery binary at `memsize - 64 KiB` through BAR0, its
//! acceptance and parse. When the default place shows no signature, Linux's second place is asked
//! (amdgpu_discovery.c:338-349: DRIVER_SCRATCH_2 the size, _0/_1 the offset) and read if all of it lies in BAR0;
//! the 780M's reader has the same rule (`gpucore::disc::tmr_place`). Every step is bounded and ends in a named
//! `Stop`; nothing is written. The reads go through `psp::Hw`, here with `mem_read`'s address meaning the byte
//! offset in VRAM (which is the offset in BAR0: the whole VRAM is CPU-visible on this card). Linux itself reads
//! the table through the MM_INDEX pair at this point, which writes the index; BAR0 gives the same bytes unwritten.

use super::policy;
use crate::devs::Key;
use crate::gpucore::disc::{self, Discovery};
use crate::gpucore::psp::Hw;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::attach::{self as rcore, AttachError, MailboxRefusal, Source};
use rdna4_core::ipfacts;
use rdna4_core::layout::{self, BlobRefusal, MemsizeError};
pub use rdna4_core::pcielink::{Host as LinkHost, LinkNow};
use rdna4_core::scpm::{self, Scpm};

/// A memory BAR as the PCI scan left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarFact {
    pub phys: u64,
    pub bytes: u64,
    pub is64: bool,
    pub prefetch: bool,
}

/// A PCIe link: speed code (1 = 2.5 GT/s ... 5 = 32 GT/s, 6 = 64 GT/s) and width, now and at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub speed: u8,
    pub width: u8,
    pub max_speed: u8,
    pub max_width: u8,
}

/// What configuration space says about the function (all read by the PCI scan or by configuration reads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciFacts {
    pub key: Key,
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    pub subsystem: (u16, u16),
    pub class: u32,
    pub command: u16,
    /// PMCSR's power state (0 = D0 ... 3 = D3hot); None without the capability.
    pub power_state: Option<u8>,
    pub bar0: Option<BarFact>,
    pub bar2: Option<BarFact>,
    pub bar5: Option<BarFact>,
    /// The card's own link, and the narrowest link above it up to the root port.
    pub link: Option<Link>,
    pub upstream: Option<(Key, Link)>,
    /// Link Status of the card and of every bridge above it up to the root port, the card's first, as the PCI
    /// scan read them (a function with no link reads 0, as Linux's `pcie_capability_read_word` gives it): what
    /// Linux's `pcie_bandwidth_available` walks at device init for the power controller's link limits
    /// (`rdna4_core::pcielink`).
    pub path: Vec<LinkNow>,
    /// Whether Linux lets the PMFW switch the link's speed on this host (the CPU's vendor and model).
    pub link_host: LinkHost,
    pub msi: bool,
    pub msix: bool,
}

pub const CMD_MEMORY: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;

impl PciFacts {
    pub fn bus_master(&self) -> bool {
        self.command & CMD_BUS_MASTER != 0
    }
}

/// Where the attach stopped, and why. Every variant leaves the card as it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Not an AMD display function with this driver's id.
    NotOurs,
    /// Memory decode is off: reading would need a configuration write, which this wave does not do.
    DecodeOff,
    /// The function is in a sleep state; waking it is a power command.
    Asleep(u8),
    /// BAR5 is not the 512 KiB 32-bit register window.
    Bar5(&'static str),
    /// BAR0 is missing or not a 64-bit memory window.
    Bar0(&'static str),
    /// A BAR5 read failed.
    Unreadable(u32),
    /// The card's own firmware did not report ready within 2 s (C2PMSG_33 bit 31).
    NotReady,
    Memsize(MemsizeError),
    /// The discovery binary would lie outside BAR0.
    OutsideBar0 { memsize_mib: u32 },
    /// The window of BAR0 could not be read.
    VramRead { offset: u64 },
    /// No table at the default place, and the place the card's mailbox names is not one to read.
    Mailbox(MailboxRefusal),
    /// The bytes at the default place were refused. `Signature`: and the mailbox names no other place.
    Blob(BlobRefusal),
    /// The bytes at the mailbox's place were refused.
    MailboxBlob { offset: u64, why: BlobRefusal },
    Parse(disc::Why),
}

impl Stop {
    /// The step's number (1-5) and its name, for the one line on the report page.
    pub fn step(&self) -> (u8, &'static str) {
        match self {
            Stop::NotOurs | Stop::DecodeOff | Stop::Asleep(_) | Stop::Bar5(_) | Stop::Bar0(_) => (1, "pci"),
            Stop::Unreadable(byte) => match byte / 4 {
                rcore::C2PMSG_33_DWORD => (2, "firmware ready"),
                rcore::MEMSIZE_DWORD => (3, "memory size"),
                _ => (4, "table read"),
            },
            Stop::NotReady => (2, "firmware ready"),
            Stop::Memsize(_) | Stop::OutsideBar0 { .. } => (3, "memory size"),
            Stop::VramRead { .. } | Stop::Mailbox(_) => (4, "table read"),
            Stop::Blob(_) | Stop::MailboxBlob { .. } | Stop::Parse(_) => (5, "table check"),
        }
    }
}

impl core::fmt::Display for Stop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Stop::NotOurs => write!(f, "not an AMD display function of this driver"),
            Stop::DecodeOff => write!(f, "memory decode is off (switching it on is a write)"),
            Stop::Asleep(d) => write!(f, "the function is in D{d} (waking it is a power command)"),
            Stop::Bar5(why) => write!(f, "BAR5 (registers): {why}"),
            Stop::Bar0(why) => write!(f, "BAR0 (graphics memory): {why}"),
            Stop::Unreadable(byte) => write!(f, "register {byte:#x} of BAR5 could not be read"),
            Stop::NotReady => write!(f, "the card's firmware did not report ready within {} ms (C2PMSG_33 bit 31)", rcore::IFWI_WAIT_MS),
            Stop::Memsize(MemsizeError::Gone) => write!(f, "the memory-size register reads all ones: the card does not answer"),
            Stop::Memsize(MemsizeError::Zero) => write!(f, "the memory-size register reads 0 (an APU's layout, not this card's)"),
            Stop::OutsideBar0 { memsize_mib } => write!(f, "{memsize_mib} MiB of memory do not fit BAR0: the table would lie outside it"),
            Stop::VramRead { offset } => write!(f, "graphics memory at offset {offset:#x} could not be read"),
            Stop::Mailbox(MailboxRefusal::Gone) => write!(f, "no table at the default place, and the mailbox's size register reads all ones: the card stopped answering"),
            Stop::Mailbox(MailboxRefusal::Size { size }) => write!(f, "no table at the default place, and the card's mailbox names a place of {size} bytes: too small for a table"),
            Stop::Mailbox(MailboxRefusal::Unaligned { offset, size }) => write!(f, "no table at the default place, and the card's mailbox names {size} bytes at offset {offset:#x}: not dword-aligned"),
            Stop::Mailbox(MailboxRefusal::OutsideBar0 { offset, size }) => write!(f, "no table at the default place, and the card's mailbox names {size} bytes at offset {offset:#x}: outside BAR0"),
            Stop::Blob(BlobRefusal::Signature { found }) => write!(f, "no table at the default place (found {found:#010x}), and the card's mailbox names no other (DRIVER_SCRATCH_2 reads 0)"),
            Stop::Blob(b) => write!(f, "the table was not accepted: {b:?}"),
            Stop::MailboxBlob { offset, why } => write!(f, "the table at the mailbox's place (offset {offset:#x}) was not accepted: {why:?}"),
            Stop::Parse(w) => write!(f, "the table did not parse: {w}"),
        }
    }
}

/// What a passed attach knows about the card.
#[derive(Debug)]
pub struct Attached {
    pub memsize_mib: u32,
    pub vram_bytes: u64,
    /// Byte offset of the discovery binary in VRAM (and in BAR0), and its physical address.
    pub discovery_offset: u64,
    pub discovery_phys: u64,
    /// Which of Linux's two places held it, and how many bytes were read there.
    pub source: Source,
    pub read_bytes: usize,
    pub binary_size: u16,
    pub discovery: Discovery,
    /// How long the firmware-ready bit took.
    pub ready_after_ms: u64,
    /// Differences to the table measured on the Gaming-PC on 5.11. (`rdna4_core::ipfacts::TABLE`); empty = the same.
    pub differs: Vec<String>,
    /// Whether the secure processor keeps the power controller's table to itself, from the PSP's runtime database
    /// 1 MiB below the end of VRAM (`rdna4_core::scpm`). Read here, before anything of the card is started, as
    /// Linux reads it in `psp_sw_init` (amdgpu_psp.c:489-499): the start of the power controller depends on it.
    pub scpm: Scpm,
    /// The card's revision as Linux's `adev->rev_id` holds it: STRAP0's ATI_REV_ID (`nbif_v6_3_1_get_rev_id`,
    /// nbif_v6_3_1.c:112-122), read right after the table as `soc24_common_early_init` reads it (soc24.c:345;
    /// the trace's discovery line 6). `None`: the table names no place for it, or the read failed.
    pub rev_id: Option<u32>,
}

struct Reads<'a, H: Hw> {
    hw: &'a mut H,
    failed: Option<u32>,
    slept_ms: u64,
}

impl<H: Hw> rcore::Regs for Reads<'_, H> {
    fn read_dword(&mut self, dword: u32) -> u32 {
        match self.hw.reg_read(dword * 4) {
            Some(v) => v,
            None => {
                self.failed.get_or_insert(dword * 4);
                0
            }
        }
    }
    fn sleep_ms(&mut self, ms: u32) {
        self.slept_ms += u64::from(ms);
        self.hw.delay_us(u64::from(ms) * 1000);
    }
}

/// Step 1: what configuration space must show before any MMIO read.
pub fn pci_gate(p: &PciFacts) -> Result<(BarFact, BarFact), Stop> {
    if p.vendor != rdna4_core::card::VENDOR || p.device != rdna4_core::card::DEVICE || (p.class >> 16) != 0x03 {
        return Err(Stop::NotOurs);
    }
    if p.command & CMD_MEMORY == 0 {
        return Err(Stop::DecodeOff);
    }
    if let Some(d) = p.power_state.filter(|d| *d != 0) {
        return Err(Stop::Asleep(d));
    }
    let bar5 = p.bar5.ok_or(Stop::Bar5("missing"))?;
    if bar5.is64 || bar5.phys == 0 || bar5.bytes != u64::from(rdna4_core::regmap::BAR5_BYTES) {
        return Err(Stop::Bar5("not the 512 KiB 32-bit window"));
    }
    let bar0 = p.bar0.ok_or(Stop::Bar0("missing"))?;
    if !bar0.is64 || bar0.phys == 0 || bar0.bytes == 0 || bar0.phys.checked_add(bar0.bytes).is_none() {
        return Err(Stop::Bar0("not a placed 64-bit window"));
    }
    Ok((bar0, bar5))
}

/// The entries of `d` against the measured table: an entry missing, extra, or with another version or other bases.
pub fn against_measured(d: &Discovery) -> Vec<String> {
    let mut out = Vec::new();
    for m in ipfacts::TABLE.iter() {
        match d.find(m.hw_id, m.instance) {
            None => out.push(format!("hw id {} instance {} is missing", m.hw_id, m.instance)),
            Some(e) => {
                let v = (e.version.major, e.version.minor, e.version.rev);
                if v != (m.version.major, m.version.minor, m.version.rev) {
                    out.push(format!("hw id {} instance {}: version {} (measured {}.{}.{})", m.hw_id, m.instance, e.version, m.version.major, m.version.minor, m.version.rev));
                }
                if e.bases.iter().copied().ne(m.bases.iter().copied()) {
                    out.push(format!("hw id {} instance {}: other register bases", m.hw_id, m.instance));
                }
            }
        }
    }
    for e in d.ips.iter() {
        if ipfacts::find(&ipfacts::TABLE, e.hw_id, e.instance).is_none() {
            out.push(format!("hw id {} instance {} is new", e.hw_id, e.instance));
        }
    }
    out
}

fn read_vram<H: Hw>(hw: &mut H, offset: u64, bytes: usize) -> Result<Vec<u8>, Stop> {
    let mut buf = alloc::vec![0u8; bytes];
    if hw.mem_read(offset, &mut buf) { Ok(buf) } else { Err(Stop::VramRead { offset }) }
}

/// `psp_get_runtime_db_entry` for the SCPM entry (amdgpu_psp.c:389-470): the header, the directory when the cookie
/// is there, the entry when the directory names one. Linux reads these through the visible VRAM mapping too
/// (`amdgpu_device_vram_access`), so its register trace does not hold them.
fn read_scpm<H: Hw>(hw: &mut H, vram_bytes: u64) -> Scpm {
    let Some(db) = vram_bytes.checked_sub(scpm::FROM_END) else { return Scpm::Unread };
    let mut h = [0u8; scpm::HEADER_BYTES];
    if !hw.mem_read(db, &mut h) {
        return Scpm::Unread;
    }
    if let Err(no) = scpm::header(&h) {
        return no;
    }
    let mut dir = alloc::vec![0u8; scpm::DIRECTORY_BYTES];
    if !hw.mem_read(db + scpm::HEADER_BYTES as u64, &mut dir) {
        return Scpm::Unread;
    }
    let at = match scpm::entry(&dir) {
        Ok(offset) => db + u64::from(offset),
        Err(no) => return no,
    };
    let mut e = [0u8; scpm::ENTRY_BYTES];
    if hw.mem_read(at, &mut e) { scpm::status(&e) } else { Scpm::Unread }
}

/// The whole attach. `hw` is the card's register owner (gate shut) or a fake.
pub fn attach<H: Hw>(p: &PciFacts, hw: &mut H) -> Result<Attached, Stop> {
    debug_assert!(policy::allowed(policy::Ask::Read, 1).is_ok());
    let (bar0, _bar5) = pci_gate(p)?;
    let mut reads = Reads { hw, failed: None, slept_ms: 0 };
    let located = rcore::locate_discovery(&mut reads, bar0.phys, bar0.bytes);
    if let Some(byte) = reads.failed {
        return Err(Stop::Unreadable(byte));
    }
    let ready_after_ms = reads.slept_ms;
    let l = located.map_err(|e| match e {
        AttachError::NotReady => Stop::NotReady,
        AttachError::Memsize(m) => Stop::Memsize(m),
        AttachError::OutsideBar0 => Stop::OutsideBar0 { memsize_mib: hw.reg_read(rcore::MEMSIZE_DWORD * 4).unwrap_or(0) },
    })?;
    let mut offset = l.discovery - bar0.phys;
    let mut source = Source::Default;
    let mut buf = read_vram(hw, offset, layout::DISCOVERY_READ)?;
    let header = match layout::accept(&buf) {
        // Linux's rule (amdgpu_discovery.c:322-349): only a missing signature at the default place asks the mailbox.
        Err(BlobRefusal::Signature { found }) => {
            let mut reads = Reads { hw, failed: None, slept_ms: 0 };
            let named = rcore::mailbox_discovery(&mut reads, bar0.bytes);
            if let Some(byte) = reads.failed {
                return Err(Stop::Unreadable(byte));
            }
            let Some(m) = named.map_err(Stop::Mailbox)? else { return Err(Stop::Blob(BlobRefusal::Signature { found })) };
            (offset, source) = (m.offset, Source::Mailbox);
            buf = read_vram(hw, m.offset, m.read)?;
            layout::accept(&buf).map_err(|why| Stop::MailboxBlob { offset: m.offset, why })?
        }
        other => other.map_err(Stop::Blob)?,
    };
    let discovery = disc::parse(&buf).map_err(Stop::Parse)?;
    let mut differs = against_measured(&discovery);
    // The table must place the memory-size register where it was just read (Linux's fixed dword, before any table).
    match crate::ipmod::rdna4::memsize_dword(&discovery) {
        Some(d) if d == rcore::MEMSIZE_DWORD => {}
        Some(d) => differs.push(format!("the table places the memory-size register at dword {d:#x}, it was read at {:#x}", rcore::MEMSIZE_DWORD)),
        None => differs.push(String::from("the table has no NBIF base for the memory-size register")),
    }
    let rev_id = read_rev_id(hw, &discovery);
    let scpm = read_scpm(hw, l.vram_bytes);
    Ok(Attached {
        memsize_mib: l.memsize_mib,
        vram_bytes: l.vram_bytes,
        discovery_offset: offset,
        discovery_phys: bar0.phys + offset,
        source,
        read_bytes: buf.len(),
        binary_size: header.binary_size,
        discovery,
        ready_after_ms,
        differs,
        scpm,
        rev_id,
    })
}

/// STRAP0 through BAR5 at the place the table names (NBIF base 2), one read; all ones is no answer.
fn read_rev_id<H: Hw>(hw: &mut H, d: &Discovery) -> Option<u32> {
    let s = rdna4_core::doorbell12::STRAP0;
    let dw = d.reg(s.hw_id, 0, usize::from(s.base_index), s.offset).filter(|dw| *dw < rdna4_core::regmap::BAR5_BYTES / 4)?;
    hw.reg_read(dw * 4).filter(|v| *v != u32::MAX).map(rdna4_core::doorbell12::rev_id)
}
