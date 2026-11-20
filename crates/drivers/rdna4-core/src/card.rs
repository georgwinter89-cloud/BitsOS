//! Which card to drive. The two are the same model with the same table and the same register bases, so nothing about a
//! card tells them apart except its place on the bus and what the firmware put in its BAR0. A write meant for the other
//! card would land on the one showing the console (`plan/spec/gpu-rdna4.md` section 5), so the only key is the PCI address and the only
//! rule is the one that names the card with the picture: its BAR0 holds the GOP framebuffer.
//!
//! That the GOP buffer lies in card A's BAR0 is itself unverified on this machine: BitsOS's BootInfo will say.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Card {
    pub at: Bdf,
    pub bar0_base: u64,
    pub bar0_bytes: u64,
}

/// Measured 5.11., lspci. Which slot each one is in is not known.
pub const CARD_A: Card =
    Card { at: Bdf { bus: 0x04, device: 0, function: 0 }, bar0_base: 0xA8_0000_0000, bar0_bytes: 16 << 30 };
pub const CARD_B: Card =
    Card { at: Bdf { bus: 0x07, device: 0, function: 0 }, bar0_base: 0xA0_0000_0000, bar0_bytes: 16 << 30 };

pub const VENDOR: u16 = 0x1002;
pub const DEVICE: u16 = 0x7550;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardRefusal {
    /// A framebuffer of no bytes names nothing.
    EmptyFramebuffer,
    /// In no card's BAR0, or only partly in one: then this is not a card the driver understands.
    NoCardHoldsIt,
    /// Two BAR0s cover it, which firmware would not do. Guessing here is the mistake this rule exists to prevent.
    Ambiguous(Bdf, Bdf),
}

fn holds(card: &Card, base: u64, end: u64) -> bool {
    match card.bar0_base.checked_add(card.bar0_bytes) {
        Some(bar_end) => base >= card.bar0_base && end <= bar_end,
        None => false,
    }
}

/// The card to drive; every other card stays untouched.
pub fn choose(cards: &[Card], gop_base: u64, gop_bytes: u64) -> Result<Bdf, CardRefusal> {
    if gop_bytes == 0 {
        return Err(CardRefusal::EmptyFramebuffer);
    }
    let end = gop_base.checked_add(gop_bytes).ok_or(CardRefusal::NoCardHoldsIt)?;
    let mut holding = cards.iter().filter(|c| holds(c, gop_base, end));
    let first = holding.next().ok_or(CardRefusal::NoCardHoldsIt)?;
    match holding.next() {
        Some(second) => Err(CardRefusal::Ambiguous(first.at, second.at)),
        None => Ok(first.at),
    }
}
