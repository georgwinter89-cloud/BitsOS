//! The chip's built-in PHY over `ocp::Mdio`. The chip's own settings are from `P:`; what phylib does around them is
//! from Linux 7.3.1 `drivers/net/phy`: `G:` is `phy_device.c`, `C:` is `phy-c45.c`, `R:` is `realtek/realtek_main.c`.
//! On the Gaming-PC Linux binds "Realtek Internal NBASE-T PHY" (R:3322-3335): `rtl822x_get_features`,
//! `rtl822x_config_aneg`, `rtl822x_read_status`, `rtlgen_resume`; no `soft_reset` and no `config_init` of its own.
//!
//! Like phylib, a modify writes only when the value changes (`__phy_modify_changed`, phy-core.c).
//!
//! One deviation, on purpose: EEE is not advertised. Linux advertises every EEE mode the PHY has; here the two
//! advertisement registers get 0, which is what Linux writes after `ethtool --set-eee ... eee off` (C:1054-1070).
//! The card is the desk's only way in, so it never naps.

use crate::fwscript::{self, FwError, Played, Script};
use crate::hw::Hw;
use crate::ocp::{self, Mdio, OcpError};

const MII_BMCR: u16 = 0x00;
const MII_BMSR: u16 = 0x01;
const MII_PHYSID1: u16 = 0x02;
const MII_PHYSID2: u16 = 0x03;
const MII_ADVERTISE: u16 = 0x04;
const MII_CTRL1000: u16 = 0x09;
const MII_ESTATUS: u16 = 0x0f;
/// `RTL_PHYSR` = `MII_RESV2` (R:236): the standard page's 0x1a is PHY OCP 0xa434.
const RTL_PHYSR: u16 = 0x1a;

const BMCR_RESET: u16 = 0x8000;
const BMCR_ANENABLE: u16 = 0x1000;
const BMCR_PDOWN: u16 = 0x0800;
const BMCR_ISOLATE: u16 = 0x0400;
const BMCR_ANRESTART: u16 = 0x0200;
const BMSR_ESTATEN: u16 = 0x0100;
const BMSR_ANEGCOMPLETE: u16 = 0x0020;
const BMSR_LSTATUS: u16 = 0x0004;

// PHY OCP addresses phylib reaches as MMD "vendor 2" registers (M:5399-5425 hands them through unchanged).
const AN_10GBT_CTRL: i32 = 0xa5d4; // R:246
const PMA_SPEED: i32 = 0xa616; // R:248
const PCS_EEE_ABLE: i32 = 0xa5c4; // R:243
const AN_EEE_ADV: i32 = 0xa5d0; // R:244
const PCS_EEE_ABLE2: i32 = 0xa6ec; // R:251
const AN_EEE_ADV2: i32 = 0xa6d4; // R:250
const EEE_CAP1_MASK: u16 = 0x007e; // C:806-809
const EEE_CAP2_MASK: u16 = 0x0003; // C:823-824

/// Read from the Gaming-PC's sysfs, 5.11.; `RTL_8251B` in R:258.
pub const PHY_ID_8126A: u32 = 0x001c_c862;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PagedMod {
    pub page: u16,
    pub reg: u16,
    pub clear: u16,
    pub set: u16,
}

const fn pm(page: u16, reg: u16, clear: u16, set: u16) -> PagedMod {
    PagedMod { page, reg, clear, set }
}

/// `rtl8126a_hw_phy_config` after the firmware (P:1148-1156): 10M on (P:728), legacy force mode off (P:996), ALDPS off
/// (P:723), then `rtl8125_common_config_eee_phy` (P:104-109).
pub const SETTINGS: [PagedMod; 6] = [
    pm(0x0a44, 0x11, 0, 1 << 11),
    pm(0x0a5b, 0x12, 1 << 15, 0),
    pm(0x0a43, 0x10, 1 << 2, 0),
    pm(0x0a6d, 0x14, 0x0010, 0),
    pm(0x0a42, 0x14, 0x0080, 0),
    pm(0x0a4a, 0x11, 0x0200, 0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyError {
    Ocp(OcpError),
    WrongId(u32),
    ResetStuck,
    Firmware(FwError),
}

impl From<OcpError> for PhyError {
    fn from(e: OcpError) -> Self {
        PhyError::Ocp(e)
    }
}

pub fn read_id<H: Hw + ?Sized>(hw: &H, mdio: &Mdio) -> Result<u32, OcpError> {
    let high = mdio.phy_read(hw, MII_PHYSID1)?;
    let low = mdio.phy_read(hw, MII_PHYSID2)?;
    Ok((u32::from(high) << 16) | u32::from(low))
}

/// True when something was written.
fn modify_std<H: Hw + ?Sized>(hw: &H, mdio: &mut Mdio, reg: u16, clear: u16, set: u16) -> Result<bool, OcpError> {
    let old = mdio.phy_read(hw, reg)?;
    let new = (old & !clear) | set;
    if new != old {
        mdio.phy_write(hw, reg, new)?;
    }
    Ok(new != old)
}

fn modify_vend2<H: Hw + ?Sized>(hw: &H, reg: i32, clear: u16, set: u16) -> Result<bool, OcpError> {
    let old = ocp::phy_ocp_read(hw, reg)?;
    let new = (old & !clear) | set;
    if new != old {
        ocp::phy_ocp_write(hw, reg, new)?;
    }
    Ok(new != old)
}

/// `rtlgen_resume` (R:2865-2873): `genphy_resume` (G:2821), then 20 ms because "internal PHY's from RTL8168h up may
/// not be instantly ready".
fn resume<H: Hw + ?Sized>(hw: &H, mdio: &mut Mdio) -> Result<(), OcpError> {
    modify_std(hw, mdio, MII_BMCR, BMCR_PDOWN, 0)?;
    hw.delay_us(20_000);
    Ok(())
}

/// `phy_read_poll_timeout(BMCR, !(val & BMCR_RESET), 50000, 600000, true)`: sleeps before each look.
fn reset_clear<H: Hw + ?Sized>(hw: &H, mdio: &Mdio) -> Result<bool, OcpError> {
    for _ in 0..12 {
        hw.delay_us(50_000);
        if mdio.phy_read(hw, MII_BMCR)? & BMCR_RESET == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `genphy_soft_reset` with autonegotiation on (G:2675-2702) and `phy_poll_reset` (G:1325-1339).
pub fn soft_reset<H: Hw + ?Sized>(hw: &H, mdio: &mut Mdio) -> Result<(), PhyError> {
    modify_std(hw, mdio, MII_BMCR, BMCR_ISOLATE, BMCR_RESET | BMCR_ANRESTART)?;
    if !reset_clear(hw, mdio)? {
        return Err(PhyError::ResetStuck);
    }
    hw.delay_us(1000);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhyUp {
    pub id: u32,
    /// `None` when no script was given: Linux goes on without one (F:214-233), whether it links then is unverified.
    pub played: Option<Played>,
}

/// `rtl8169_up`'s PHY part (M:5035-5037): `phy_init_hw` writes nothing for this PHY driver (G:1341-1385 with no
/// `soft_reset`, `config_init` or `config_intr`), `phy_resume`, then `rtl8169_init_phy` (M:2539-2562). Identity
/// first, so that no script and no setting reaches a PHY this crate does not know.
pub fn bring_up<H: Hw + ?Sized>(hw: &H, mdio: &mut Mdio, script: Option<&Script<'_>>) -> Result<PhyUp, PhyError> {
    let id = read_id(hw, mdio)?;
    if id != PHY_ID_8126A {
        return Err(PhyError::WrongId(id));
    }
    resume(hw, mdio)?;
    let played = match script {
        Some(s) => {
            let played = fwscript::apply(s, hw, mdio).map_err(PhyError::Firmware)?;
            // M:2501-2504: "PHY soft reset may still be in progress". Linux does not look at the outcome either;
            // a reset that is really stuck fails `soft_reset` below.
            reset_clear(hw, mdio)?;
            Some(played)
        }
        None => None,
    };
    for m in SETTINGS {
        mdio.modify_paged(hw, m.page, m.reg, m.clear, m.set)?;
    }
    soft_reset(hw, mdio)?;
    Ok(PhyUp { id, played })
}

/// Link modes as a bit set.
pub mod mode {
    pub const HALF_10: u16 = 1 << 0;
    pub const FULL_10: u16 = 1 << 1;
    pub const HALF_100: u16 = 1 << 2;
    pub const FULL_100: u16 = 1 << 3;
    pub const HALF_1000: u16 = 1 << 4;
    pub const FULL_1000: u16 = 1 << 5;
    pub const FULL_2500: u16 = 1 << 6;
    pub const FULL_5000: u16 = 1 << 7;
    /// What Linux reports as supported on the Gaming-PC's card (`ethtool`, 14.11.).
    pub const GAMING_PC: u16 = HALF_10 | FULL_10 | HALF_100 | FULL_100 | FULL_1000 | FULL_2500 | FULL_5000;
}

/// `rtl822x_get_features` (R:2175-2191) and `genphy_read_abilities` (G:2727-2762). The 10 Gb/s bit is not looked at:
/// this chip has no such mode.
pub fn abilities<H: Hw + ?Sized>(hw: &H, mdio: &Mdio) -> Result<u16, OcpError> {
    let mut modes = 0;
    let pma = ocp::phy_ocp_read(hw, PMA_SPEED)?;
    let bmsr = mdio.phy_read(hw, MII_BMSR)?;
    let mut take = |bit: bool, m: u16| {
        if bit {
            modes |= m;
        }
    };
    take(pma & 0x2000 != 0, mode::FULL_2500);
    take(pma & 0x4000 != 0, mode::FULL_5000);
    take(bmsr & 0x0800 != 0, mode::HALF_10);
    take(bmsr & 0x1000 != 0, mode::FULL_10);
    take(bmsr & 0x2000 != 0, mode::HALF_100);
    take(bmsr & 0x4000 != 0, mode::FULL_100);
    if bmsr & BMSR_ESTATEN != 0 {
        let est = mdio.phy_read(hw, MII_ESTATUS)?;
        take(est & 0x2000 != 0, mode::FULL_1000);
        take(est & 0x1000 != 0, mode::HALF_1000);
    }
    Ok(modes)
}

/// Modes at or below `max_mbps`.
pub const fn capped(modes: u16, max_mbps: u32) -> u16 {
    let mut keep = mode::HALF_10 | mode::FULL_10;
    if max_mbps >= 100 {
        keep |= mode::HALF_100 | mode::FULL_100;
    }
    if max_mbps >= 1000 {
        keep |= mode::HALF_1000 | mode::FULL_1000;
    }
    if max_mbps >= 2500 {
        keep |= mode::FULL_2500;
    }
    if max_mbps >= 5000 {
        keep |= mode::FULL_5000;
    }
    modes & keep
}

/// The three advertisement values for a set of modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advert {
    /// `MII_ADVERTISE` bits within 0x0fe0. Pause and asymmetric pause are always in: `phy_support_asym_pause`
    /// (M:5480), and what `ethtool` shows on the Gaming-PC.
    pub adv: u16,
    /// `MII_CTRL1000` bits within 0x0300.
    pub ctrl1000: u16,
    /// `MDIO_AN_10GBT_CTRL` bits within 0x1180: 2.5G is 0x0080, 5G is 0x0100 (include/uapi/linux/mdio.h:332-333).
    pub mgbt: u16,
}

pub const fn advert(modes: u16) -> Advert {
    const fn bit(modes: u16, m: u16, v: u16) -> u16 {
        if modes & m != 0 { v } else { 0 }
    }
    Advert {
        adv: bit(modes, mode::HALF_10, 0x0020)
            | bit(modes, mode::FULL_10, 0x0040)
            | bit(modes, mode::HALF_100, 0x0080)
            | bit(modes, mode::FULL_100, 0x0100)
            | 0x0400
            | 0x0800,
        ctrl1000: bit(modes, mode::HALF_1000, 0x0100) | bit(modes, mode::FULL_1000, 0x0200),
        mgbt: bit(modes, mode::FULL_2500, 0x0080) | bit(modes, mode::FULL_5000, 0x0100),
    }
}

/// `phy_start`'s first step for this PHY: `rtl822x_config_aneg` (R:2193-2210) into `__genphy_config_aneg`
/// (G:2309-2345): multi-gigabit advertisement, EEE advertisement (none, see the module text), the two classic
/// advertisement registers, then autonegotiation restarted when anything changed or it was not running.
/// `genphy_setup_master_slave` writes nothing while nobody asked for a role (G:2198-2200).
pub fn config_aneg<H: Hw + ?Sized>(hw: &H, mdio: &mut Mdio, a: Advert) -> Result<(), OcpError> {
    let mut changed = modify_vend2(hw, AN_10GBT_CTRL, 0x0080 | 0x0100 | 0x1000, a.mgbt)?;
    // C:798-830: each advertisement only where the PHY has the modes at all.
    if ocp::phy_ocp_read(hw, PCS_EEE_ABLE)? & EEE_CAP1_MASK != 0 {
        changed |= modify_vend2(hw, AN_EEE_ADV, EEE_CAP1_MASK, 0)?;
    }
    if ocp::phy_ocp_read(hw, PCS_EEE_ABLE2)? & EEE_CAP2_MASK != 0 {
        changed |= modify_vend2(hw, AN_EEE_ADV2, EEE_CAP2_MASK, 0)?;
    }
    // genphy_config_advert, G:2081-2118.
    changed |= modify_std(hw, mdio, MII_ADVERTISE, 0x0fe0, a.adv)?;
    if mdio.phy_read(hw, MII_BMSR)? & BMSR_ESTATEN != 0 {
        changed |= modify_std(hw, mdio, MII_CTRL1000, 0x0300, a.ctrl1000)?;
    }
    // genphy_check_and_restart_aneg, G:2277-2296.
    if !changed {
        let bmcr = mdio.phy_read(hw, MII_BMCR)?;
        changed = bmcr & BMCR_ANENABLE == 0 || bmcr & BMCR_ISOLATE != 0;
    }
    if changed {
        modify_std(hw, mdio, MII_BMCR, BMCR_ISOLATE, BMCR_ANENABLE | BMCR_ANRESTART)?;
    }
    Ok(())
}

/// `rtlgen_decode_physr` (R:1666-1712). 0x0200 means 10 Gb/s there, which this chip cannot do.
pub const fn speed_mbps(physr: u16) -> Option<u32> {
    match physr & 0x0630 {
        0x0000 => Some(10),
        0x0010 => Some(100),
        0x0020 => Some(1000),
        0x0210 => Some(2500),
        0x0220 => Some(5000),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    /// `None` while down, or for a value the table does not know.
    pub speed_mbps: Option<u32>,
    pub full_duplex: bool,
}

const DOWN: Link = Link { up: false, speed_mbps: None, full_duplex: false };

/// `genphy_update_link` (G:2429-2470) as phylib runs it for a MAC-interrupt PHY, then `rtlgen_read_status`
/// (R:1714-1732): no link while autonegotiation restarts or is unfinished; the link bit is latched low, so a first
/// read that says "down" is followed by a second.
pub fn read_link<H: Hw + ?Sized>(hw: &H, mdio: &Mdio) -> Result<Link, OcpError> {
    if mdio.phy_read(hw, MII_BMCR)? & BMCR_ANRESTART != 0 {
        return Ok(DOWN);
    }
    let mut bmsr = mdio.phy_read(hw, MII_BMSR)?;
    if bmsr & BMSR_LSTATUS == 0 {
        bmsr = mdio.phy_read(hw, MII_BMSR)?;
    }
    if bmsr & BMSR_LSTATUS == 0 || bmsr & BMSR_ANEGCOMPLETE == 0 {
        return Ok(DOWN);
    }
    let physr = mdio.phy_read(hw, RTL_PHYSR)?;
    Ok(Link { up: true, speed_mbps: speed_mbps(physr), full_duplex: physr & 0x0008 != 0 })
}
