//! All access to the card. igc's `Hw` has one 32-bit width; this card has registers that are one and two bytes wide
//! next to each other (ChipCmd 0x37 inside the word at 0x34, Cfg9346 next to Config0), so a wider access would change
//! a neighbour. The kernel's `IoMem` already has all three widths (`crates/hal/src/mmio.rs`).

pub trait Hw {
    fn rd8(&self, off: u16) -> u8;
    fn rd16(&self, off: u16) -> u16;
    fn rd32(&self, off: u16) -> u32;
    fn wr8(&self, off: u16, val: u8);
    fn wr16(&self, off: u16, val: u16);
    fn wr32(&self, off: u16, val: u32);
    fn delay_us(&self, us: u32);
    /// Extended config space; `None` when it cannot be reached.
    fn cfg_rd8(&self, off: u16) -> Option<u8>;
    fn cfg_wr8(&self, off: u16, val: u8) -> bool;
    /// `pcie_set_readrq(pdev, 4096)` (M:2630, :2661).
    fn set_max_read_request_4096(&self) -> bool;
    /// Where `ocp` notes how long each PHY window access was waited for; `None`: nobody keeps it.
    fn phy_waits(&self) -> Option<&crate::ocp::PhyWaits> {
        None
    }
}

/// `rtl_loop_wait` (M:898-913): looks first and sleeps after every miss, the last one too.
pub fn wait<H: Hw + ?Sized>(hw: &H, check: impl Fn(&H) -> bool, high: bool, us: u32, tries: u32) -> bool {
    wait_counted(hw, check, high, us, tries).is_some()
}

/// `wait`, returning how many looks missed before the one that hit.
pub fn wait_counted<H: Hw + ?Sized>(hw: &H, check: impl Fn(&H) -> bool, high: bool, us: u32, tries: u32) -> Option<u32> {
    for missed in 0..tries {
        if check(hw) == high {
            return Some(missed);
        }
        hw.delay_us(us);
    }
    None
}
