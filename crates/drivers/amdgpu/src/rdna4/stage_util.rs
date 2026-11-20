//! What every stage of the card uses: why a stage stops (the plain cause in German and the technical detail), a
//! register read that stops on a card that does not answer, the stick's files, and the session's ring and firmware
//! buffer when they are there.

use super::regs14::Regs14;
use super::ring14::{Pri, Ring14};
use super::smu14::SmuStop;
use super::stage::Session;
use super::vram::Bump;
use crate::gpucore::fwload::FwSource;
use crate::gpucore::psp::{self, Hw, PspError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Why a stage ended early: the plain cause and the technical detail.
pub struct Stop(pub &'static str, pub String);

pub(super) fn stop(plain: &'static str, detail: impl Into<String>) -> Stop {
    Stop(plain, detail.into())
}

pub(super) fn psp_stop(e: PspError) -> Stop {
    let plain = match &e {
        PspError::Gone { .. } => "Karte antwortet beim Lesen nicht",
        PspError::WriteRefused { .. } => "ein Schreibzugriff war nicht erlaubt",
        PspError::Timeout { what: "fence", .. } => "Sicherheitsprozessor hat einen Befehl nicht beantwortet",
        PspError::Timeout { .. } => "Sicherheitsprozessor antwortet nicht rechtzeitig",
        PspError::RingStatus { .. } => "Sicherheitsprozessor lehnt den Befehlskanal ab",
        PspError::CmdStatus { .. } => "Sicherheitsprozessor lehnt einen Befehl ab",
        PspError::Mem { .. } => "Speicher der Karte lässt sich nicht beschreiben",
        PspError::Wedged => "der Befehlskanal hängt: erst abbauen",
        PspError::CmdRefused { .. } => "ein Befehl steht nicht auf der Liste des Erlaubten",
        PspError::RingNotReset { .. } => "der neue Befehlskanal fängt nicht am Anfang an",
        PspError::TocSize(_) => "eine Datei auf dem Stick passt nicht",
        _ => "unerwarteter Fehler im Befehlskanal",
    };
    stop(plain, format!("{e:?}"))
}

pub(super) fn smu_stop(e: SmuStop) -> Stop {
    let plain = match e {
        SmuStop::Gone { .. } => "Karte antwortet beim Lesen nicht",
        SmuStop::Refused { .. } => "ein Schreibzugriff war nicht erlaubt",
        SmuStop::NotRunning { .. } => "Energieverwaltung der Karte läuft nicht",
        SmuStop::Busy { .. } => "Energieverwaltung ist noch mit einer alten Frage beschäftigt",
        SmuStop::Hung { .. } => "Energieverwaltung steckt in einem Fehlerzustand und bekommt keine Frage mehr",
        SmuStop::Timeout { .. } => "Energieverwaltung antwortet nicht",
        SmuStop::Answer { .. } => "Energieverwaltung beantwortet eine Frage nicht",
    };
    stop(plain, format!("{e:?}"))
}

pub(super) fn read(hw: &mut dyn Hw, byte: u32) -> Result<u32, Stop> {
    psp::read(hw, byte).map_err(psp_stop)
}

/// The ring's registers read now (the note's four), `u32::MAX` unreadable.
pub(super) fn ring_regs(hw: &mut dyn Hw, r: &Regs14) -> [u32; 4] {
    [r.psp.c2pmsg_67, r.psp.c2pmsg_71, r.psp.c2pmsg_69, r.psp.c2pmsg_70].map(|b| hw.reg_read(b).unwrap_or(u32::MAX))
}

/// The ring, when it is there to take a command.
pub(super) fn need_ring(s: &mut Session) -> Result<&mut Ring14, Stop> {
    match s.ring.as_mut() {
        Some(r) if r.created() && !r.wedged() => Ok(r),
        Some(r) if !r.created() => Err(stop("der Befehlskanal ist nicht fertig angelegt: erst abbauen", "ring create failed")),
        Some(_) => Err(stop("der Befehlskanal hängt: erst abbauen", "ring wedged")),
        None => Err(stop("zuerst den Befehlskanal anlegen", "no ring in this boot")),
    }
}

/// The 1 MiB firmware buffer, made on first use in the card's window (after the picture, `vram`).
pub(super) fn need_pri(s: &mut Session) -> Result<&mut Pri, Stop> {
    if s.pri.is_none() {
        let l = s.layout.clone().ok_or_else(|| stop("kein sicherer Platz im Speicher der Karte", "no window planned: the stage gmc plans it"))?;
        let bump = s.bump.get_or_insert_with(|| Bump::new(&l));
        s.pri = Some(Pri::new(bump, &l).map_err(psp_stop)?);
    }
    s.pri.as_mut().ok_or_else(|| stop("unerwarteter Fehler im Befehlskanal", "no firmware buffer"))
}

pub(super) fn read_file(files: &mut dyn FwSource, name: &str) -> Result<Vec<u8>, Stop> {
    match files.read(&crate::gpucore::fwload::stick_path(name)) {
        Ok(Some(b)) => Ok(b),
        Ok(None) => Err(stop("eine Datei fehlt auf dem Stick", format!("{name} missing"))),
        Err(e) => Err(stop("eine Datei auf dem Stick lässt sich nicht lesen", format!("{name}: {e:?}"))),
    }
}
