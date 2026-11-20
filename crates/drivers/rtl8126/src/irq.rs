//! Which interrupt bits matter (`rtl_set_irq_mask`, M:5321-5327). Linux runs this card on one vector, and so will
//! this driver once it stops polling; the mask and acknowledge writes are `seq::MASK_ACK` and `seq::IRQ_ON`.

pub const RX_OK: u32 = 0x0001; // M:469
pub const RX_ERR: u32 = 0x0002; // M:468
pub const TX_OK: u32 = 0x0004; // M:467
pub const TX_ERR: u32 = 0x0008; // M:466
pub const LINK_CHG: u32 = 0x0020; // M:464

pub const IRQ_MASK: u32 = RX_OK | RX_ERR | TX_OK | TX_ERR | LINK_CHG;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Irq {
    NotOurs,
    Events { status: u32, link_changed: bool },
}

/// `rtl8169_interrupt`'s first look (M:4860-4863). All ones in the low half means the card is gone from the bus.
/// `status` is what gets written back to acknowledge.
pub const fn classify(status: u32) -> Irq {
    if status & 0xffff == 0xffff || status & IRQ_MASK == 0 {
        return Irq::NotOurs;
    }
    Irq::Events { status, link_changed: status & LINK_CHG != 0 }
}
