//! How deep the stack is (it grows down: a lower address is deeper). The card's own `Hw` (`livestage`) and the
//! fake card (`fakemodel`) keep the lowest address at which a stage reached the card; the stage budget
//! (`fakemodel::STAGE_STACK`) and the measurement run (`messung`) take it against the caller's own.

/// Where the stack is now.
#[inline(never)]
pub fn stack_at() -> usize {
    let probe = 0u8;
    core::hint::black_box(&raw const probe) as usize
}

/// Bytes from `top` (taken by the caller) down to `lowest`; 0 when nothing was reached.
pub fn below(top: usize, lowest: usize) -> usize {
    if lowest == usize::MAX { 0 } else { top.saturating_sub(lowest) }
}
