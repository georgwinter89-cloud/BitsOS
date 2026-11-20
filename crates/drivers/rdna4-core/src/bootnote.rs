//! What a kernel tells the next one of the same power cycle about the card's bootloader. The bootloader keeps every
//! part it took until the card is off, and nothing on the card says how many it has (C2PMSG_35 reads ready after
//! each part, C2PMSG_81 reads 0 until the secure OS runs). A kernel started by a Reload has a fresh `bootseq::Gate`
//! and would begin at the key database again, a second time for that bootloader. So the gate's progress crosses the
//! Reload in a note, and a kernel that inherits one which says a command went out, on a card whose C2PMSG_81 still
//! reads 0, sends nothing until the card was off (`Gate::spent`).
//!
//! The note is written before the stage `sos` sends anything (`IN_FLIGHT`: a kernel that ends in the middle of the
//! loads has said so already) and again when the stage ended, with what really went out.
//!
//! Layout (16 bytes, little endian): 0 magic "GXBOOT01", 8 commands sent u8 (0..=9, or 0xff: a stage was sending),
//! 9 the failed part's descriptor type u8 (0: none), 10 zero u16, 12 C2PMSG_35 as the failed part left it u32.

use crate::bootseq::{Part, SEQUENCE};

pub const MAGIC: [u8; 8] = *b"GXBOOT01";
pub const LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootNote {
    /// How many of Linux's nine went out, or `BootNote::UNKNOWN`.
    pub sent: u8,
    /// The part that was not taken and what C2PMSG_35 held then.
    pub failed: Option<(Part, u32)>,
}

impl BootNote {
    /// `sent` of a note whose writer did not see its stage end.
    pub const UNKNOWN: u8 = 0xFF;
    /// Nothing went to the bootloader.
    pub const NOTHING: BootNote = BootNote { sent: 0, failed: None };
    /// Written before the stage sends: how many went out is known only when the stage ended.
    pub const IN_FLIGHT: BootNote = BootNote { sent: BootNote::UNKNOWN, failed: None };

    /// A command went out, or may have: this bootloader gets nothing more in this power cycle.
    pub fn spent(&self) -> bool {
        self.sent != 0
    }

    pub fn bytes(&self) -> [u8; LEN] {
        let mut b = [0u8; LEN];
        b[..8].copy_from_slice(&MAGIC);
        b[8] = self.sent;
        if let Some((part, c35)) = self.failed {
            b[9] = part as u8;
            b[12..].copy_from_slice(&c35.to_le_bytes());
        }
        b
    }

    /// `None` for anything that is not a whole note of this version.
    pub fn parse(b: &[u8]) -> Option<BootNote> {
        let b: &[u8; LEN] = b.try_into().ok()?;
        let sent = b[8];
        if b[..8] != MAGIC || b[10] != 0 || b[11] != 0 || (usize::from(sent) > SEQUENCE.len() && sent != BootNote::UNKNOWN) {
            return None;
        }
        let c35 = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
        let failed = match b[9] {
            0 if c35 == 0 => None,
            0 => return None,
            t => Some((SEQUENCE.iter().map(|(p, _)| *p).find(|p| *p as u8 == t)?, c35)),
        };
        // A part can only fail once a command went out.
        (failed.is_none() || sent != 0).then_some(BootNote { sent, failed })
    }

    /// What was handed on under the note's tag. Bytes that are no note of this version (another kernel version
    /// wrote them) say that something was written about this bootloader: taken as a sequence in flight.
    pub fn handed(bytes: Option<&[u8]>) -> Option<BootNote> {
        bytes.map(|b| BootNote::parse(b).unwrap_or(BootNote::IN_FLIGHT))
    }
}

impl core::fmt::Display for BootNote {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.sent {
            0 => write!(f, "sent nothing to the bootloader"),
            BootNote::UNKNOWN => write!(f, "was sending to the bootloader when it ended (how many of the nine is not known)"),
            n => write!(f, "sent {n} of the nine bootloader commands"),
        }?;
        match self.failed {
            Some((part, c35)) => write!(f, ", and the {} was not taken (C2PMSG_35 {c35:#010x})", part.name()),
            None => Ok(()),
        }
    }
}
