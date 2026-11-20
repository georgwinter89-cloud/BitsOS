//! Radeon RX 9070 XT (Navi 48, PCI 1002:7550), the Gaming-PC's two cards. Lane GP, skeleton of
//! `plan/spec/gpu-rdna4.md`: facts and arithmetic for the GX-10 chain, which starts after the first Steam game
//! on the Mini-PC. No crate of the kernel depends on this one and it binds to nothing, so it changes no machine's
//! behaviour; `crates/drivers/amdgpu`, which drives the 780M, is not touched and not used.
//!
//! Sources: `A:` is Linux 7.3.1 `drivers/gpu/drm/amd/amdgpu`, `M:` is `include/asic_reg/mp/mp_14_0_2_offset.h` and `P:`
//! is `pm/swsmu/inc/pmfw_if/smu_v14_0_2_ppsmc.h` there. Layout:
//! - `ipfacts`: the measured discovery table and the roles it asks for;
//! - `fwset`: firmware names, sizes and hashes, and the two rules that change the names;
//! - `attach`: the first reads of a card, over a trait that has no write;
//! - `layout`: where the discovery blob lies, when a read of it is accepted, GART and DMA limits;
//! - `regmap`: base-index arithmetic for the first registers, and what BAR5 reaches;
//! - `card`: which of the two identical cards is the one to drive;
//! - `refuse`: what must never be sent to the secure processor or the SMU;
//! - `bootseq`: the one way to the secure processor's bootloader, Linux's nine loads of a cold start;
//! - `bootnote`: how far that way was gone, for the kernel after a Reload;
//! - `devinfo`: what libdrm saw on this card, as named constants;
//! - `sdma7` (wave 43): SDMA 7.0.1's two engines, their registers, values, doorbells and interrupt entries;
//! - `nbreg`, `hub41`, `ih7`, `pte12` (wave 42): NBIF 6.3.1 with DF, GMC 12 with MMHUB 4.1.0 and the GART's entries,
//!   IH 7.0, as registers, fields and the arithmetic of Linux's sequences;
//! - `smu14`, `smuseq`, `smugate`, `pptable14`, `pcielink`, `scpm`, `thm14` (wave 43): the power controller's start as
//!   Linux makes it: the messages and their answers, the sequence fed by the answers, the gate that opens the
//!   mailbox for exactly that sequence, the PMFW's table by its documented header, the link limits from
//!   configuration space, whether the secure processor keeps the table (SCPM), the thermal alert's registers.
//! - `hub41` with `hubseq`, `gc12`, `fault12` (wave 43): the two memory hubs as one hub with two register sets,
//!   the graphics block's start, and a page fault as Linux reads and words it.
//! - `mes12`, `mes12api`, `mqd12` (wave 44): MES 12's registers and the arithmetic of its start, its API frames
//!   by the documented header, and the queues' MQDs as Linux fills them.
//! - `thm14`'s handler half, `thmgate`, `metrics14` (wave 44): what Linux does when the card reports its
//!   temperature, the gate that opens exactly those accesses, and a temperature read from the PMFW's metrics.
//! - `gc12ring` (wave 44): the kernel rings' start once MES maps them: CP_MAX_CONTEXT, CP_DEVICE_ID, the ring test,
//!   the interrupt enables and the CP's interrupt sources.
//!
//! `plan/spec/gpu-rdna4.md` section 4 says which piece is built where.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod attach;
pub mod bootnote;
pub mod bootseq;
pub mod card;
pub mod clearstate12;
pub mod devinfo;
pub mod doorbell12;
pub mod fault12;
pub mod fwset;
pub mod gc12;
pub mod gc12cp;
pub mod gc12ring;
pub mod hub41;
pub mod hubseq;
pub mod ih7;
pub mod ipfacts;
pub mod layout;
pub mod mes12;
pub mod mes12api;
pub mod metrics14;
pub mod mqd12;
pub mod nbreg;
pub mod pcielink;
pub mod pptable14;
pub mod pte12;
pub mod refuse;
pub mod regmap;
pub mod scpm;
pub mod sdma7;
pub mod smu14;
pub mod smugate;
pub mod smuseq;
pub mod thm14;
pub mod thmgate;

#[cfg(test)]
mod tests;
