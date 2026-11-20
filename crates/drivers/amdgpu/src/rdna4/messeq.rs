//! Wave 44, item ME: how MES's two stages are held against Linux's trace (`linuxseq`), section `mes` (init.txt
//! 9502-9567): `mes` from its start to the aggregated doorbells, `mesq` the rest.
//!
//! The declared differences: the one bind of MES's piece and the four addresses of the KIQ pipe's queue, each
//! exactly the object BitsOS's own allocation put there (`Kiq`, recorded by the stage when it binds the piece). Linux bound the piece in `sw_init`; BitsOS binds it right before the
//! stage's first access (the section's first GRBM_GFX_CNTL write), when GFXHUB is on and MES does not run yet: the
//! flush walks both hubs by registers (`Rule::BindBothBefore`), the last bind of a boot that does so. The program
//! counter's start comes from the file's header, the doorbells (0x18 for the KIQ ring, the aggregated ones from
//! 0x800) are Linux's fixed numbers of this card's doorbell layout: they have no rule. The host tests, the ktest
//! and every stage call's own `linuxseq` line use this one declaration (`chainseq`).

use super::linuxseq::rules::{Address, Rule};
use super::mesring::{Place, Port, lay};
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::{mes12, mqd12, pte12};

/// The KIQ pipe's queue as the stage bound it (GART addresses): its MQD page, its ring, its read and write
/// pointers' slots. Each of the queue's four address registers must name exactly its own object, not merely some
/// place in the GART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kiq {
    pub mqd: u64,
    pub ring: u64,
    pub rptr: u64,
    pub wptr: u64,
}

impl Kiq {
    pub fn of(p: &Place, kiq: &Port) -> Kiq {
        Kiq { mqd: p.mc + lay::mqd(1), ring: p.mc + lay::ring(1), rptr: p.slot_mc(kiq.slots.rptr), wptr: p.slot_mc(kiq.slots.wptr) }
    }
}

fn pair(lo: &'static str, hi: &'static str, shift: u8, align: u64, at: u64, bytes: u64, why: &'static str) -> Rule {
    let within: Range<u64> = at..at + bytes;
    Rule::Address(Address { lo, hi: Some(hi), shift, keep: 0, align, within, bytes, why })
}

/// The section's rules for the KIQ queue the stage bound; none while no stage bound it in this boot.
pub fn rules(kiq: Option<Kiq>) -> Result<Vec<Rule>, &'static str> {
    let k = kiq.ok_or("the stage mes has not bound MES's piece in this boot, so the KIQ queue's addresses have no rule")?;
    Ok(alloc::vec![
        Rule::BindBothBefore {
            reg: "GRBM_GFX_CNTL",
            why: "Linux bound MES's rings, MQDs, EOP buffers and slots (and the kernel queues' memory) while the blocks set up; BitsOS binds them in one bind right before MES's first access, when GFXHUB is on and MES does not run yet, so the flush walks both hubs by registers: the last bind of the boot before the flush goes through MES",
        },
        pair("CP_MQD_BASE_ADDR", "CP_MQD_BASE_ADDR_HI", 0, pte12::PAGE, k.mqd, u64::from(mqd12::MQD_BYTES), "the KIQ pipe's MQD lies on the page BitsOS bound for it in the GART"),
        pair("CP_HQD_PQ_BASE", "CP_HQD_PQ_BASE_HI", 8, pte12::PAGE, k.ring, u64::from(mes12::RING_BYTES), "the KIQ pipe's ring lies where BitsOS bound it in the GART"),
        pair("CP_HQD_PQ_RPTR_REPORT_ADDR", "CP_HQD_PQ_RPTR_REPORT_ADDR_HI", 0, 8, k.rptr, 8, "the KIQ ring's read-pointer slot, which BitsOS took in MES's write-back page"),
        pair("CP_HQD_PQ_WPTR_POLL_ADDR", "CP_HQD_PQ_WPTR_POLL_ADDR_HI", 0, 8, k.wptr, 8, "the KIQ ring's write-pointer slot, which BitsOS took in MES's write-back page"),
    ])
}
