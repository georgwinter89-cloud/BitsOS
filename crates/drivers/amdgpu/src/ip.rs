//! GX-D2: the GPU as IP blocks with versions, as Linux's `amdgpu_discovery.c` builds them (`ip_versions[HWIP][inst]`,
//! `amdgpu.h:635` `enum amd_hw_ip_block_type`). Until GX-6 parses the discovery blob (VRAM top − 64 KiB), the
//! versions come from a table keyed by PCI device ID and say so (`Source::Table`); DCN 3.1.4 is upgraded to
//! `Source::Measured` once the attach's DP DTO signature passed (gate 7: segment 1 is a DCN 3.1.4's).
//! The Gaming-PC's RX 9070 (gfx1201, DCN 4.x) is then "new versions", not a new driver.

use alloc::vec::Vec;

/// IP block kinds (Linux `amd_hw_ip_block_type` names, the ones V2 meets first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IpKind {
    /// Graphics and compute (GC).
    Gc,
    /// Display core next (DCE_HWIP in Linux).
    Dcn,
    Sdma,
    Vcn,
    /// PSP.
    Mp0,
    /// SMU.
    Mp1,
    Nbio,
    Mmhub,
    /// Interrupt handler ring (OSSSYS).
    Ih,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IpVersion {
    pub major: u8,
    pub minor: u8,
    pub rev: u8,
}

impl core::fmt::Display for IpVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.rev)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// From the device-ID table (documentation, not read from this GPU).
    Table(&'static str),
    /// Proven by a register signature on this GPU.
    Measured(&'static str),
    /// From the IP discovery table (GX-6).
    Discovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpBlock {
    pub kind: IpKind,
    pub instance: u8,
    pub version: IpVersion,
    pub source: Source,
}

const fn v(major: u8, minor: u8, rev: u8) -> IpVersion {
    IpVersion { major, minor, rev }
}

const DOC: &str = "Linux Documentation/gpu/amdgpu/apu-asic-info-table.csv (Phoenix/Hawk Point big die)";

/// Phoenix1 / Hawk Point (1002:15BF, 1002:1900; Windows puts both in the big-die PowerPlay class,
/// G/doku/20-gpu-kern.md §1). Only versions a source names; GX-6 replaces them with the discovery table's.
const PHOENIX: &[(IpKind, IpVersion, &str)] = &[
    (IpKind::Gc, v(11, 0, 1), DOC),
    (IpKind::Dcn, v(3, 1, 4), DOC),
    (IpKind::Sdma, v(6, 0, 1), DOC),
    (IpKind::Vcn, v(4, 0, 2), DOC),
    (IpKind::Mp0, v(13, 0, 4), DOC),
    (IpKind::Mp1, v(13, 0, 4), "V2-PLAN GX-6 (SMU 13.0.4), G/doku/20-gpu-kern.md §8"),
    (IpKind::Nbio, v(7, 7, 0), "dcn314-regs bar5.rs (NBIO 7.7), linux-6.6 amdgpu/nbio_v7_7.c"),
];

/// The IP blocks of a GPU by PCI device ID (empty for an unknown one: its display may still attach, the gates
/// decide; the other blocks wait for the discovery table).
pub fn known_blocks(device: u16) -> Vec<IpBlock> {
    match device {
        0x15BF | 0x1900 => PHOENIX
            .iter()
            .map(|&(kind, version, src)| IpBlock { kind, instance: 0, version, source: Source::Table(src) })
            .collect(),
        _ => Vec::new(),
    }
}

/// After the attach proved the DCN (DP DTO signature, gate 7): the DCN block is DCN 3.1.4, measured.
pub fn mark_dcn_measured(blocks: &mut Vec<IpBlock>) {
    const WHY: &str = "attach gate 7: DP DTO modulo 600 MHz in segment 1 (DCN 3.1.4's layout)";
    match blocks.iter_mut().find(|b| b.kind == IpKind::Dcn) {
        Some(b) => {
            b.version = v(3, 1, 4);
            b.source = Source::Measured(WHY);
        }
        None => {
            blocks.push(IpBlock { kind: IpKind::Dcn, instance: 0, version: v(3, 1, 4), source: Source::Measured(WHY) })
        }
    }
}

pub fn block(blocks: &[IpBlock], kind: IpKind) -> Option<&IpBlock> {
    blocks.iter().find(|b| b.kind == kind)
}
