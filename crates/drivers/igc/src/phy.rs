//! The PHY behind MDIC, inside the PHY0 semaphore its firmware shares (`V1:igcphy.asm`, Linux `igc_phy.c`,
//! `igc_i225.c`). No klog here: results carry what happened, the device logs them (B6).

use crate::hw::{until, Hw};
use crate::regs::{self, mdic, phy, swsm, Model};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyError {
    /// SWSM.SMBI never came free, not even after clearing it once.
    SmbiStuck,
    /// SWSM.SWESMBI never stuck: the firmware holds the semaphore.
    SwesmbiHeld,
    /// SW_FW_SYNC PHY0 kept by the firmware for a second.
    FirmwareHoldsPhy,
    /// MDIC not ready within 100 ms.
    MdicTimeout,
    /// MDIC reported an error (wrong PHY address, a register the PHY does not have).
    MdicError,
}

impl PhyError {
    pub fn as_str(self) -> &'static str {
        match self {
            PhyError::SmbiStuck => "SWSM.SMBI stuck",
            PhyError::SwesmbiHeld => "SWSM held by the firmware",
            PhyError::FirmwareHoldsPhy => "PHY held by its firmware",
            PhyError::MdicTimeout => "PHY does not answer (MDIC timeout)",
            PhyError::MdicError => "PHY does not answer (MDIC error)",
        }
    }
}

const SEM_WAIT_US: u64 = 1_000_000;
const SEM_STEP_US: u64 = 500;
const SWFW_STEP_US: u64 = 5_000;
const MDIC_WAIT_US: u64 = 100_000;
const MDIC_STEP_US: u64 = 50;

/// What taking the PHY semaphore took (the device logs it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Acquired {
    /// SMBI was stuck and was cleared once (Linux `clear_semaphore_once`).
    pub smbi_cleared: bool,
    /// Our own PHY0 bit was already set: a bring-up of ours that died half-way left it; taken over.
    pub took_over_own_bit: bool,
    /// The firmware held PHY0 for a while before letting go.
    pub waited_for_firmware: bool,
}

/// SWSM's two bits taken (Linux `igc_get_hw_semaphore_i225`): SMBI by a read that finds it clear (the read
/// sets it), within 1 s, cleared once and retried if stuck; then SWESMBI, which sticks once the firmware lets
/// go, within 1 s. Up to 3 s in all.
fn swsm_get<H: Hw + ?Sized>(hw: &H, a: &mut Acquired) -> Result<(), PhyError> {
    let smbi_free = || hw.rd(regs::SWSM) & swsm::SMBI == 0;
    if !until(hw, SEM_WAIT_US, SEM_STEP_US, smbi_free) {
        // Left set by someone gone (`V1:igcphy.asm:151`): cleared once, as Linux does.
        swsm_put(hw);
        a.smbi_cleared = true;
        if !until(hw, SEM_WAIT_US, SEM_STEP_US, smbi_free) {
            return Err(PhyError::SmbiStuck);
        }
    }
    let got = until(hw, SEM_WAIT_US, SEM_STEP_US, || {
        hw.set_bits(regs::SWSM, swsm::SWESMBI);
        hw.rd(regs::SWSM) & swsm::SWESMBI != 0
    });
    if !got {
        swsm_put(hw);
        return Err(PhyError::SwesmbiHeld);
    }
    Ok(())
}

/// Both SWSM bits given back.
fn swsm_put<H: Hw + ?Sized>(hw: &H) {
    hw.clear_bits(regs::SWSM, swsm::SMBI | swsm::SWESMBI);
}

/// PHY0 taken from the firmware (Linux `igc_acquire_swfw_sync_i225`, `V1:igcphy.asm:100-125`): the firmware's
/// bit (17) is waited for up to 1 s, 5 ms apart; software's bit (1) already set can only be our own (BitsOS
/// is the card's only software and the device serialises its PHY work), so it is taken over, not waited for.
pub fn acquire<H: Hw + ?Sized>(hw: &H) -> Result<Acquired, PhyError> {
    let mut a = Acquired::default();
    let end = hw.now_ns().saturating_add(SEM_WAIT_US * 1000);
    loop {
        swsm_get(hw, &mut a)?;
        let sync = hw.rd(regs::SW_FW_SYNC);
        if sync & regs::SWFW_PHY0_FW == 0 {
            a.took_over_own_bit = sync & regs::SWFW_PHY0_SW != 0;
            hw.wr(regs::SW_FW_SYNC, sync | regs::SWFW_PHY0_SW);
            swsm_put(hw);
            return Ok(a);
        }
        swsm_put(hw);
        a.waited_for_firmware = true;
        if hw.now_ns() >= end {
            return Err(PhyError::FirmwareHoldsPhy);
        }
        hw.delay_us(SWFW_STEP_US);
    }
}

/// PHY0 given back; without SWSM nothing can be given back (Linux gives up as well).
pub fn release<H: Hw + ?Sized>(hw: &H) -> Result<(), PhyError> {
    let mut a = Acquired::default();
    swsm_get(hw, &mut a)?;
    hw.clear_bits(regs::SW_FW_SYNC, regs::SWFW_PHY0_SW);
    swsm_put(hw);
    Ok(())
}

/// One MDIC operation; `Ok(data)` once ready without error (`V1:igcphy.asm:45-65`).
fn mdic_op<H: Hw + ?Sized>(hw: &H, model: Model, op: u32, reg: u32, data: u16) -> Result<u16, PhyError> {
    let cmd = op | ((reg & 0x1F) << mdic::REG_SHIFT) | (model.phy_addr() << mdic::PHY_SHIFT) | data as u32;
    hw.wr(regs::MDIC, cmd);
    let mut v = 0;
    let ready = until(hw, MDIC_WAIT_US, MDIC_STEP_US, || {
        v = hw.rd(regs::MDIC);
        v & mdic::READY != 0
    });
    if !ready || v == !0 {
        return Err(PhyError::MdicTimeout);
    }
    if v & mdic::ERROR != 0 {
        return Err(PhyError::MdicError);
    }
    Ok(v as u16)
}

pub fn read<H: Hw + ?Sized>(hw: &H, model: Model, reg: u32) -> Result<u16, PhyError> {
    mdic_op(hw, model, mdic::OP_READ, reg, 0)
}

pub fn write<H: Hw + ?Sized>(hw: &H, model: Model, reg: u32, v: u16) -> Result<(), PhyError> {
    mdic_op(hw, model, mdic::OP_WRITE, reg, v).map(|_| ())
}

/// Clause-45 register through the clause-22 MMD window (Linux `igc_access_xmdio_reg`). The window is set back
/// to 0 afterwards even when a step failed, so the next clause-22 access (ANAR, BMCR) is not aimed at an MMD
/// register; the first error is returned.
fn mmd_rw<H: Hw + ?Sized>(hw: &H, model: Model, dev: u16, reg: u16, set: Option<u16>) -> Result<u16, PhyError> {
    let steps = || -> Result<u16, PhyError> {
        write(hw, model, phy::MMDAC, dev)?;
        write(hw, model, phy::MMDAAD, reg)?;
        write(hw, model, phy::MMDAC, phy::MMDAC_FUNC_DATA | dev)?;
        match set {
            Some(v) => write(hw, model, phy::MMDAAD, v).map(|_| v),
            None => read(hw, model, phy::MMDAAD),
        }
    };
    let r = steps();
    let back = write(hw, model, phy::MMDAC, 0);
    let v = r?;
    back.map(|()| v)
}

/// What the PHY reset did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyReset {
    Done,
    /// MANC blocks PHY resets (management owns the PHY); skipped.
    Blocked,
    /// PHPM never reported completion within 10 ms (Linux goes on as well).
    NoCompletion,
}

/// I226 PHY reset (Linux `igc_phy_hw_reset`, `V1:igcphy.asm:67-92`), PHY0 held by the caller.
pub fn reset<H: Hw + ?Sized>(hw: &H) -> PhyReset {
    if hw.rd(regs::MANC) & regs::MANC_BLK_PHY_RST != 0 {
        return PhyReset::Blocked;
    }
    let c = hw.rd(regs::CTRL);
    hw.wr(regs::CTRL, c | regs::ctrl::PHY_RST);
    hw.flush();
    hw.delay_us(1_000);
    hw.wr(regs::CTRL, c & !regs::ctrl::PHY_RST);
    hw.flush();
    hw.delay_us(1_000);
    let done = until(hw, 10_000, 100, || hw.rd(regs::PHPM) & regs::PHPM_RST_COMPL != 0);
    hw.delay_us(1_000);
    if done { PhyReset::Done } else { PhyReset::NoCompletion }
}

/// The advertisement Linux writes (`igc_phy_setup_autoneg` with `IGC_ALL_SPEED_DUPLEX_2500`): 10/100 half and
/// full, 1000 full, and on the I226 2500 full through MMD 7. Pause is not advertised: the MAC's flow control
/// is not set up (no FCRTL/FCRTH), so promising to honour PAUSE would be a lie; Linux advertises it because it
/// configures flow control. QEMU's 82576 PHY has no MMD window (an MDIC error), so 2500 is the I226's alone.
pub fn advertise<H: Hw + ?Sized>(hw: &H, model: Model) -> Result<(), PhyError> {
    let anar = read(hw, model, phy::ANAR)?;
    let anar = (anar & !(phy::ANAR_SPEEDS | phy::ANAR_PAUSE | phy::ANAR_SELECTOR_MASK)) | phy::ANAR_SPEEDS | phy::ANAR_SELECTOR_8023;
    write(hw, model, phy::ANAR, anar)?;
    let c1000 = read(hw, model, phy::CTRL1000)?;
    write(hw, model, phy::CTRL1000, (c1000 & !(phy::CTRL1000_FD | phy::CTRL1000_HD)) | phy::CTRL1000_FD)?;
    if model == Model::I226 {
        let m = mmd_rw(hw, model, phy::MMD_AN, phy::MMD_MULTIGBT_AN_CTRL, None)?;
        mmd_rw(hw, model, phy::MMD_AN, phy::MMD_MULTIGBT_AN_CTRL, Some(m | phy::MULTIGBT_2500_FD))?;
    }
    Ok(())
}

/// BMCR: power-down cleared, autonegotiation on and restarted (`V1:igcphy.asm:30-43`). QEMU's 82576 brings
/// its link up about 500 ms after this write.
pub fn autoneg<H: Hw + ?Sized>(hw: &H, model: Model) -> Result<(), PhyError> {
    let b = read(hw, model, phy::BMCR)?;
    write(hw, model, phy::BMCR, (b & !phy::BMCR_POWER_DOWN) | phy::BMCR_AN_ENABLE | phy::BMCR_RESTART_AN)
}

/// What bringing the PHY up did (for the device's events).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhyReport {
    pub acquired: Acquired,
    /// `None` on the 82576 (no PHPM; QEMU's PHY needs no reset).
    pub reset: Option<PhyReset>,
    /// The advertisement write failed; autonegotiation runs with the PHY's own.
    pub advertise_error: Option<PhyError>,
    /// PHY0 could not be given back (SWSM not obtainable): our bit stays set in SW_FW_SYNC, the firmware is
    /// kept off the PHY until our next bring-up takes the bit over (`acquire`). Not forced clear: SW_FW_SYNC
    /// without SWSM races with the firmware's own read-modify-write (Linux gives up the same way).
    pub release_error: Option<PhyError>,
}

/// The PHY on and autonegotiating (`V1:igcphy.asm:14-25`): on the I226 the firmware's PHPM locks cleared, then
/// inside PHY0 the PHY reset, the advertisement and BMCR; PHY0 is given back on every path.
pub fn up<H: Hw + ?Sized>(hw: &H, model: Model) -> Result<PhyReport, PhyError> {
    if model == Model::I226 {
        hw.clear_bits(regs::PHPM, regs::PHPM_FW_LOCKS);
    }
    let acquired = acquire(hw)?;
    let reset = (model == Model::I226).then(|| reset(hw));
    let advertise_error = advertise(hw, model).err();
    let r = autoneg(hw, model);
    let release_error = release(hw).err();
    r.map(|()| PhyReport { acquired, reset, advertise_error, release_error })
}
