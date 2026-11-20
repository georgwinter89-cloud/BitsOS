//! The firmware file: not code for a processor but a list of register writes for the PHY and the MAC's MCU, played
//! once at PHY setup (`F:`, `M:2490-2500`). Format and validator are Linux's. Two things are stricter:
//! - a start inside the header and the old format without a header are refused (neither is this chip's file);
//! - a script that would loop for ever, or sleep for minutes, is stopped. Linux trusts the file here.
//!
//! The licence forbids reverse engineering, so nothing in this crate looks at what a script does beyond this walk.

use crate::hw::Hw;
use crate::ocp::{Mdio, OcpError};

const HEADER: usize = 45; // F:32-38, packed
const VERSION: core::ops::Range<usize> = 4..36;
const OFF_START: usize = 36;
const OFF_LEN: usize = 40;
const ACTION: usize = 4;

/// The real file has 3279 actions; a wait inside it may repeat. Both limits only stop a broken file.
pub const MAX_STEPS: u32 = 1 << 22;
pub const MAX_DELAY_MS: u32 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwError {
    TooShort,
    OldFormat,
    Checksum(u8),
    StartOutOfFile(u32),
    LenOutOfFile(u32),
    BadOpcode { index: usize, action: u32 },
    OutOfRange { index: usize, action: u32 },
    Runaway,
    TooMuchDelay,
    Bus(OcpError),
}

impl From<OcpError> for FwError {
    fn from(e: OcpError) -> Self {
        FwError::Bus(e)
    }
}

// F:16-30
const READ: u8 = 0x0;
const DATA_OR: u8 = 0x1;
const DATA_AND: u8 = 0x2;
const BJMPN: u8 = 0x3;
const MDIO_CHG: u8 = 0x4;
const CLEAR_READCOUNT: u8 = 0x7;
const WRITE: u8 = 0x8;
const READCOUNT_EQ_SKIP: u8 = 0x9;
const COMP_EQ_SKIPN: u8 = 0xa;
const COMP_NEQ_SKIPN: u8 = 0xb;
const WRITE_PREVIOUS: u8 = 0xc;
const SKIPN: u8 = 0xd;
const DELAY_MS: u8 = 0xe;

fn fields(action: u32) -> (u8, u16, u16) {
    ((action >> 28) as u8, ((action >> 16) & 0xfff) as u16, action as u16)
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// What a script reads and writes. `mcu` selects the MAC-MCU accessors, as `PHY_MDIO_CHG` does (F:172-181).
pub trait FwBus {
    fn read(&mut self, mcu: bool, reg: u16) -> Result<u16, OcpError>;
    fn write(&mut self, mcu: bool, reg: u16, data: u16) -> Result<(), OcpError>;
    fn delay_ms(&mut self, ms: u32);
}

/// A script over the real accessors.
pub struct OcpBus<'a, H: Hw + ?Sized> {
    pub hw: &'a H,
    pub mdio: &'a mut Mdio,
}

impl<H: Hw + ?Sized> FwBus for OcpBus<'_, H> {
    fn read(&mut self, mcu: bool, reg: u16) -> Result<u16, OcpError> {
        if mcu { self.mdio.mcu_read(self.hw, reg) } else { self.mdio.phy_read(self.hw, reg) }
    }
    fn write(&mut self, mcu: bool, reg: u16, data: u16) -> Result<(), OcpError> {
        if mcu { self.mdio.mcu_write(self.hw, reg, data) } else { self.mdio.phy_write(self.hw, reg, data) }
    }
    fn delay_ms(&mut self, ms: u32) {
        self.hw.delay_us(ms.saturating_mul(1000));
    }
}

/// A file that passed `rtl_fw_format_ok` and `rtl_fw_data_ok` (F:41-138).
#[derive(Clone, Copy, Debug)]
pub struct Script<'a> {
    version: &'a [u8],
    start: u32,
    code: &'a [u8],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Played {
    pub steps: u32,
    pub writes: u32,
    pub delay_ms: u32,
}

impl<'a> Script<'a> {
    pub fn parse(file: &'a [u8]) -> Result<Script<'a>, FwError> {
        if file.len() < ACTION {
            return Err(FwError::TooShort);
        }
        if le32(file, 0) != 0 {
            return Err(FwError::OldFormat);
        }
        if file.len() < HEADER {
            return Err(FwError::TooShort);
        }
        let sum = file.iter().fold(0u8, |s, b| s.wrapping_add(*b));
        if sum != 0 {
            return Err(FwError::Checksum(sum));
        }
        let start = le32(file, OFF_START);
        if (start as usize) < HEADER || start as usize > file.len() {
            return Err(FwError::StartOutOfFile(start));
        }
        let len = le32(file, OFF_LEN);
        if len as usize > (file.len() - start as usize) / ACTION {
            return Err(FwError::LenOutOfFile(len));
        }
        let code = &file[start as usize..start as usize + len as usize * ACTION];
        // strscpy of a 32-byte field keeps at most 31.
        let field = &file[VERSION];
        let end = field.iter().position(|b| *b == 0).unwrap_or(field.len() - 1);
        let script = Script { version: &field[..end], start, code };
        script.validate()?;
        Ok(script)
    }

    pub fn version(&self) -> &'a [u8] {
        self.version
    }

    pub fn start(&self) -> u32 {
        self.start
    }

    pub fn len(&self) -> usize {
        self.code.len() / ACTION
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    fn action(&self, index: usize) -> u32 {
        le32(self.code, index * ACTION)
    }

    fn validate(&self) -> Result<(), FwError> {
        let size = self.len();
        for index in 0..size {
            let action = self.action(index);
            let (op, regno, val) = fields(action);
            let regno = regno as usize;
            let in_range = match op {
                READ | DATA_OR | DATA_AND | CLEAR_READCOUNT | WRITE | WRITE_PREVIOUS | DELAY_MS => true,
                MDIO_CHG => val <= 1,
                BJMPN => regno <= index,
                READCOUNT_EQ_SKIP => index + 2 < size,
                COMP_EQ_SKIPN | COMP_NEQ_SKIPN | SKIPN => index + 1 + regno < size,
                _ => return Err(FwError::BadOpcode { index, action }),
            };
            if !in_range {
                return Err(FwError::OutOfRange { index, action });
            }
        }
        Ok(())
    }

    /// `rtl_fw_write_firmware` (F:140-207). A failed access stops the script: unlike Linux, the rest of it does not
    /// run on a value that is really an error code.
    pub fn play<B: FwBus + ?Sized>(&self, bus: &mut B) -> Result<Played, FwError> {
        let mut played = Played::default();
        let (mut predata, mut count, mut mcu) = (0u16, 0u32, false);
        let mut index = 0;
        while index < self.len() {
            played.steps += 1;
            if played.steps > MAX_STEPS {
                return Err(FwError::Runaway);
            }
            let (op, regno, data) = fields(self.action(index));
            let mut next = index + 1;
            match op {
                READ => {
                    predata = bus.read(mcu, regno)?;
                    count += 1;
                }
                DATA_OR => predata |= data,
                DATA_AND => predata &= data,
                // Linux subtracts regno + 1 and lets the loop add one back; the validator keeps this at or above 0.
                BJMPN => next = index - regno as usize,
                MDIO_CHG => mcu = data != 0,
                CLEAR_READCOUNT => count = 0,
                WRITE => {
                    bus.write(mcu, regno, data)?;
                    played.writes += 1;
                }
                READCOUNT_EQ_SKIP if count == data as u32 => next += 1,
                COMP_EQ_SKIPN if predata == data => next += regno as usize,
                COMP_NEQ_SKIPN if predata != data => next += regno as usize,
                WRITE_PREVIOUS => {
                    bus.write(mcu, regno, predata)?;
                    played.writes += 1;
                }
                SKIPN => next += regno as usize,
                DELAY_MS => {
                    played.delay_ms = played.delay_ms.saturating_add(data as u32);
                    if played.delay_ms > MAX_DELAY_MS {
                        return Err(FwError::TooMuchDelay);
                    }
                    bus.delay_ms(data as u32);
                }
                _ => {}
            }
            index = next;
        }
        Ok(played)
    }
}

/// `r8169_apply_firmware` (M:2490-2500).
pub fn apply<H: Hw + ?Sized>(script: &Script<'_>, hw: &H, mdio: &mut Mdio) -> Result<Played, FwError> {
    let played = script.play(&mut OcpBus { hw, mdio: &mut *mdio });
    mdio.reset_base();
    played
}
