//! Wave 44, items CP and RG: what a kernel ring of the command processor does on the bus, as Linux's GFX 12 code
//! does it: `gfx_v12_0_ring_set_wptr_gfx` and `_compute` (gfx_v12_0.c:4456-4471, :4494-4506) store the write
//! pointer, in dwords, into its 64-bit shadow first (`atomic64_set`) and then ring the queue's doorbell with it
//! (`WDOORBELL64`); `get_rptr` reads the read pointer's slot as 32 bits ("gfx12 is 32bit rptr", :4434-4438,
//! :4473-4477). The kernel rings' slots are system pages behind the GART (`mesring`), so both go through the
//! card's host side; the doorbell through the card (`ringops` frames what goes in between).

use super::host::Card;

/// Linux's `set_wptr` over a doorbell: the shadow (one 8-byte store at the system address `wptr_sys`), a store
/// barrier, then the doorbell at BAR2 dword `doorbell_dw`. `false`: one of the two did not reach the card.
pub fn set_wptr(card: &mut dyn Card, wptr_sys: u64, doorbell_dw: u32, wptr: u64) -> bool {
    card.sys_write(wptr_sys, &wptr.to_le_bytes()) && {
        card.hw().store_barrier();
        card.hw().doorbell_write64(doorbell_dw, wptr)
    }
}

/// Linux's `get_rptr`: the slot's low 32 bits.
pub fn get_rptr(card: &mut dyn Card, rptr_sys: u64) -> Option<u64> {
    let mut b = [0u8; 4];
    card.sys_read(rptr_sys, &mut b).then(|| u64::from(u32::from_le_bytes(b)))
}
