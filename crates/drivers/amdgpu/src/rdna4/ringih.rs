//! Wave 44, item RG: the command processor's interrupt sources at the session's IH reader. `gfx_v12_0_sw_init`
//! registers four of client GRBM_CP (gfx_v12_0.c:1472-1500): the end-of-pipe interrupt, which
//! `gfx_v12_0_eop_irq` (:4875-4933) hands to the fence of the kernel ring the entry's ring id names, and the three
//! faults (bad opcode, privileged register, privileged instruction; `gfx_v12_0_handle_priv_fault` :5092-5102
//! finds the ring the same way and faults its scheduler). The rings' rungs read the IH rings themselves through
//! `sdmaih::read` (the stage holds the session) and count the entries per ring id here. The stage `gcring`
//! registers the four before it turns the faults on, as sw_init comes before hw_init, so the live IH thread
//! counts a fault per ring in every boot that reached it; `ringtest` registers again (the same entries).

use super::ihring::IhUp;
use rdna4_core::gc12ring as r;
use rdna4_core::ih7::Iv;
use rdna4_core::mes12::{KERNEL_QUEUES, Kq};

pub const FAULT_SOURCES: [(u8, &str); 3] = [(r::SRC_BAD_OPCODE, "gfx12 bad opcode"), (r::SRC_PRIV_REG, "gfx12 privileged register"), (r::SRC_PRIV_INSTR, "gfx12 privileged instruction")];

pub fn register(ih: &mut IhUp) {
    ih.reader.register(r::CLIENT_GRBM_CP, r::SRC_EOP, "gfx12 end of pipe");
    for (src, who) in FAULT_SOURCES {
        ih.reader.register(r::CLIENT_GRBM_CP, src, who);
    }
}

/// A kernel queue's ring id as the CP puts it into its entries.
pub const fn ring_id(k: &Kq) -> u8 {
    r::ring_id(k.me, k.pipe, k.queue)
}

/// An entry of the CP's sources whose ring id names none of the kernel's rings: `gfx_v12_0_eop_irq` finds no
/// ring for it (with no user queues on this card nothing else takes it), so it never counts as a ring's.
pub fn wrong_ring_id(iv: &Iv) -> bool {
    let ours = iv.client_id == r::CLIENT_GRBM_CP && (iv.src_id == r::SRC_EOP || FAULT_SOURCES.iter().any(|(s, _)| *s == iv.src_id));
    ours && !KERNEL_QUEUES.iter().any(|k| ring_id(k) == iv.ring_id)
}

/// End-of-pipe entries of a kernel queue that arrived through the reader.
pub fn eops(ih: &IhUp, k: &Kq) -> u64 {
    ih.reader.taken(r::CLIENT_GRBM_CP, r::SRC_EOP, ring_id(k))
}

/// Fault entries (all three sources) of a kernel queue.
pub fn faults(ih: &IhUp, k: &Kq) -> u64 {
    FAULT_SOURCES.iter().map(|(s, _)| ih.reader.taken(r::CLIENT_GRBM_CP, *s, ring_id(k))).sum()
}
