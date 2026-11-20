//! The chip's internal bus. MAC registers are reached through OCPDR, PHY registers through GPHY_OCP (M:1106-1166);
//! there are no MDIO pins. Unlike Linux, a refused address or a timeout is returned, not logged and skipped: the
//! caller of a firmware script must learn that a write did not happen. In exchange the PHY window is waited for much
//! longer than Linux's numbers say (`PHY_WAIT_TRIES`), and the longest wait is kept (`PhyWaits`).

use crate::hw::{wait_counted, Hw};
use crate::regs::{GPHY_OCP, OCPDR};
use core::sync::atomic::{AtomicU32, Ordering};

pub const OCP_FLAG: u32 = 0x8000_0000; // M:416
pub const STD_PHY_BASE: i32 = 0xa400; // M:81
const PAGE_SELECT: u16 = 0x1f; // M:1251

/// Linux looks ten times with `fsleep(25)` in between (M:1124, :1136). That sleep is `usleep_range(25, 31)`
/// (include/linux/delay.h:129-136), a timer sleep with a context switch, so its ten looks stretch over several
/// hundred microseconds, and a miss is logged and passed over (M:898-913). Here the step is an exact 25 us and a miss
/// is an error that ends the firmware script, so the looks go on for 20 ms before the window is called dead. An
/// access that answers ends at its first or second look as before.
pub const PHY_WAIT_US: u32 = 25;
pub const PHY_WAIT_TRIES: u32 = 800;
/// A link look runs on the poller every driver shares: each of its reads gives up after 2 ms. A read that slow fails
/// under Linux too (`-ETIMEDOUT`, M:1139-1140, which stops phylib), and Linux keeps its link on this card.
pub const PHY_LOOK_TRIES: u32 = 80;
/// Linux's ten looks, taken as exact: a wait beyond this is one its numbers alone would not have covered.
pub const LINUX_WAIT_US: u32 = PHY_WAIT_US * 10;

/// How long PHY window accesses were waited for (looks missed x 25 us): nobody has measured it on the card, so the
/// first round does, instead of depending on it.
#[derive(Debug, Default)]
pub struct PhyWaits {
    max_us: AtomicU32,
    over_linux: AtomicU32,
}

impl PhyWaits {
    pub const fn new() -> PhyWaits {
        PhyWaits { max_us: AtomicU32::new(0), over_linux: AtomicU32::new(0) }
    }
    fn note(&self, us: u32) {
        self.max_us.fetch_max(us, Ordering::Relaxed);
        if us >= LINUX_WAIT_US {
            self.over_linux.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// The longest wait so far, a timed-out one included.
    pub fn max_us(&self) -> u32 {
        self.max_us.load(Ordering::Relaxed)
    }
    /// Accesses that took longer than Linux's ten looks of 25 us.
    pub fn over_linux(&self) -> u32 {
        self.over_linux.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OcpError {
    /// Odd or above 16 bits; Linux calls this "Invalid ocp reg" (M:1106-1109).
    BadAddress(i32),
    /// The PHY window never answered.
    Timeout(u32),
}

pub const fn addr_ok(reg: i32) -> bool {
    reg >= 0 && (reg as u32) & 0xffff_0001 == 0
}

fn checked(reg: i32) -> Result<u32, OcpError> {
    if addr_ok(reg) { Ok(reg as u32) } else { Err(OcpError::BadAddress(reg)) }
}

pub const fn write_cmd(reg: u32, data: u16) -> u32 {
    OCP_FLAG | (reg << 15) | data as u32
}

pub const fn read_cmd(reg: u32) -> u32 {
    reg << 15
}

pub fn mac_write<H: Hw + ?Sized>(hw: &H, reg: i32, data: u16) -> Result<(), OcpError> {
    hw.wr32(OCPDR, write_cmd(checked(reg)?, data));
    Ok(())
}

pub fn mac_read<H: Hw + ?Sized>(hw: &H, reg: i32) -> Result<u16, OcpError> {
    hw.wr32(OCPDR, read_cmd(checked(reg)?));
    Ok(hw.rd32(OCPDR) as u16)
}

/// `r8168_mac_ocp_modify` (M:1180): clear `mask`, then set `set`.
pub fn mac_modify<H: Hw + ?Sized>(hw: &H, reg: i32, mask: u16, set: u16) -> Result<(), OcpError> {
    let data = mac_read(hw, reg)?;
    mac_write(hw, reg, (data & !mask) | set)
}

fn phy_flag<H: Hw + ?Sized>(hw: &H) -> bool {
    hw.rd32(GPHY_OCP) & OCP_FLAG != 0
}

/// Waits for the window's flag and notes how long that took.
fn phy_done<H: Hw + ?Sized>(hw: &H, reg: u32, high: bool, tries: u32) -> Result<(), OcpError> {
    let missed = wait_counted(hw, phy_flag, high, PHY_WAIT_US, tries);
    if let Some(w) = hw.phy_waits() {
        w.note(missed.unwrap_or(tries) * PHY_WAIT_US);
    }
    missed.map(|_| ()).ok_or(OcpError::Timeout(reg))
}

pub fn phy_ocp_write<H: Hw + ?Sized>(hw: &H, reg: i32, data: u16) -> Result<(), OcpError> {
    phy_ocp_write_within(hw, reg, data, PHY_WAIT_TRIES)
}

pub fn phy_ocp_read<H: Hw + ?Sized>(hw: &H, reg: i32) -> Result<u16, OcpError> {
    phy_ocp_read_within(hw, reg, PHY_WAIT_TRIES)
}

/// `tries` looks of 25 us before the window counts as dead.
pub fn phy_ocp_write_within<H: Hw + ?Sized>(hw: &H, reg: i32, data: u16, tries: u32) -> Result<(), OcpError> {
    let reg = checked(reg)?;
    hw.wr32(GPHY_OCP, write_cmd(reg, data));
    phy_done(hw, reg, false, tries)
}

pub fn phy_ocp_read_within<H: Hw + ?Sized>(hw: &H, reg: i32, tries: u32) -> Result<u16, OcpError> {
    let reg = checked(reg)?;
    hw.wr32(GPHY_OCP, read_cmd(reg));
    phy_done(hw, reg, true, tries)?;
    Ok(hw.rd32(GPHY_OCP) as u16)
}

/// `r8168g_mdio_write`'s mapping (M:1249-1262): the standard page is reg x 2, any other page starts at reg 0x10.
/// The arithmetic is Linux's: a reg below 0x10 on another page lands before the page, which is not refused here.
pub const fn phy_addr(base: i32, reg: u16) -> i32 {
    let reg = if base != STD_PHY_BASE { reg as i32 - 0x10 } else { reg as i32 };
    base + reg * 2
}

pub const fn page_base(page: u16) -> i32 {
    if page == 0 { STD_PHY_BASE } else { (page as i32) << 4 }
}

/// PHY and MAC-MCU access by (page, reg). Linux keeps one `ocp_base` for both (M:1251, :1279), and a firmware script
/// relies on where it stands, so this does too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mdio {
    base: i32,
    /// Looks per PHY access before the window counts as dead.
    tries: u32,
}

impl Default for Mdio {
    fn default() -> Self {
        Mdio { base: STD_PHY_BASE, tries: PHY_WAIT_TRIES }
    }
}

impl Mdio {
    /// For the poller's link look: the short wait (`PHY_LOOK_TRIES`).
    pub fn for_look() -> Mdio {
        Mdio { base: STD_PHY_BASE, tries: PHY_LOOK_TRIES }
    }

    pub fn base(&self) -> i32 {
        self.base
    }

    /// M:2498: "At least one firmware doesn't reset tp->ocp_base."
    pub fn reset_base(&mut self) {
        self.base = STD_PHY_BASE;
    }

    pub fn phy_write<H: Hw + ?Sized>(&mut self, hw: &H, reg: u16, value: u16) -> Result<(), OcpError> {
        if reg == PAGE_SELECT {
            self.base = page_base(value);
            return Ok(());
        }
        phy_ocp_write_within(hw, phy_addr(self.base, reg), value, self.tries)
    }

    pub fn phy_read<H: Hw + ?Sized>(&self, hw: &H, reg: u16) -> Result<u16, OcpError> {
        if reg == PAGE_SELECT {
            return Ok(if self.base == STD_PHY_BASE { 0 } else { (self.base >> 4) as u16 });
        }
        phy_ocp_read_within(hw, phy_addr(self.base, reg), self.tries)
    }

    /// `mac_mcu_write` (M:1276-1284): same page register, but no 0x10 offset, no x 2, and page 0 means address 0.
    pub fn mcu_write<H: Hw + ?Sized>(&mut self, hw: &H, reg: u16, value: u16) -> Result<(), OcpError> {
        if reg == PAGE_SELECT {
            self.base = (value as i32) << 4;
            return Ok(());
        }
        mac_write(hw, self.base + reg as i32, value)
    }

    pub fn mcu_read<H: Hw + ?Sized>(&self, hw: &H, reg: u16) -> Result<u16, OcpError> {
        mac_read(hw, self.base + reg as i32)
    }

    /// `phy_modify_paged`: the page is put back whatever happens, and like phylib (`__phy_modify_changed`) nothing is
    /// written when the value already stands. Returns the old value.
    pub fn modify_paged<H: Hw + ?Sized>(
        &mut self,
        hw: &H,
        page: u16,
        reg: u16,
        clear: u16,
        set: u16,
    ) -> Result<u16, OcpError> {
        let saved = self.base;
        self.base = page_base(page);
        let result = self.phy_read(hw, reg).and_then(|old| {
            let new = (old & !clear) | set;
            if new != old {
                self.phy_write(hw, reg, new)?;
            }
            Ok(old)
        });
        self.base = saved;
        result
    }

    pub fn read_paged<H: Hw + ?Sized>(&mut self, hw: &H, page: u16, reg: u16) -> Result<u16, OcpError> {
        let saved = self.base;
        self.base = page_base(page);
        let result = self.phy_read(hw, reg);
        self.base = saved;
        result
    }
}
