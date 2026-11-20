//! The card's stages by name, Linux's chain of them (`Stage::CHAIN`) and the one place that holds every stage to
//! its place in it (`in_order`). What each stage does, and where Linux does it, is `stage`'s table.

use super::session::Session;
use super::stage_util::{Stop, stop};
use alloc::format;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Nbif,
    Gmc,
    Ih,
    Sos,
    Ring,
    Toc,
    Tmr,
    Fw,
    Autoload,
    FwRest,
    Ta,
    Pmfw,
    GcBoot,
    GcHub,
    GcCp,
    Mes,
    MesQ,
    GcRing,
    Sdma,
    SdmaTest,
    RingTest,
    Alive,
    Bootloader,
    Smu,
    Destroy,
    State,
}

impl Stage {
    /// Linux's chain from a cold card to loaded firmware, the started power controller, the graphics block's
    /// start with its hub, the command processor, MES with the kernel's queues mapped through it, the rings and
    /// their tests (wave 44) and the copy engines, in Linux's order: each stands on the one before it. The display
    /// takes its place between `pmfw` and `gcboot` when it is built (Linux's order of sections: smu, display,
    /// gc-boot, gc-hub, gc-cp, mes, gc-ring, sdma).
    pub const CHAIN: [Stage; 19] = [
        Stage::Nbif,
        Stage::Gmc,
        Stage::Ih,
        Stage::Sos,
        Stage::Ring,
        Stage::Toc,
        Stage::Tmr,
        Stage::Fw,
        Stage::Autoload,
        Stage::FwRest,
        Stage::Ta,
        Stage::Pmfw,
        Stage::GcBoot,
        Stage::GcHub,
        Stage::GcCp,
        Stage::Mes,
        Stage::MesQ,
        Stage::GcRing,
        Stage::Sdma,
    ];

    /// The chain, the copy engines' and the rings' rungs, then the looks and the ring's destroy.
    pub const ALL: [Stage; 26] = [
        Stage::Nbif,
        Stage::Gmc,
        Stage::Ih,
        Stage::Sos,
        Stage::Ring,
        Stage::Toc,
        Stage::Tmr,
        Stage::Fw,
        Stage::Autoload,
        Stage::FwRest,
        Stage::Ta,
        Stage::Pmfw,
        Stage::GcBoot,
        Stage::GcHub,
        Stage::GcCp,
        Stage::Mes,
        Stage::MesQ,
        Stage::GcRing,
        Stage::Sdma,
        Stage::SdmaTest,
        Stage::RingTest,
        Stage::Alive,
        Stage::Bootloader,
        Stage::Smu,
        Stage::Destroy,
        Stage::State,
    ];

    pub fn parse(s: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|t| t.name() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Stage::Nbif => "nbif",
            Stage::Gmc => "gmc",
            Stage::Ih => "ih",
            Stage::Alive => "alive",
            Stage::Bootloader => "bootloader",
            Stage::Smu => "smu",
            Stage::Sos => "sos",
            Stage::Ring => "ring",
            Stage::Toc => "toc",
            Stage::Tmr => "tmr",
            Stage::Fw => "fw",
            Stage::Autoload => "autoload",
            Stage::FwRest => "fwrest",
            Stage::Ta => "ta",
            Stage::Pmfw => "pmfw",
            Stage::GcBoot => "gcboot",
            Stage::GcHub => "gchub",
            Stage::GcCp => "gccp",
            Stage::Mes => "mes",
            Stage::MesQ => "mesq",
            Stage::GcRing => "gcring",
            Stage::Sdma => "sdma",
            Stage::SdmaTest => "sdmatest",
            Stage::RingTest => "ringtest",
            Stage::Destroy => "destroy",
            Stage::State => "state",
        }
    }

    /// What Georg reads on the screen.
    pub fn german(self) -> &'static str {
        match self {
            Stage::Nbif => "Anschluss der Karte einrichten",
            Stage::Gmc => "Speicher der Karte einrichten",
            Stage::Ih => "Meldungen der Karte einschalten",
            Stage::Alive => "Sicherheitsprozessor prüfen",
            Stage::Bootloader => "Startlader prüfen",
            Stage::Smu => "Energieverwaltung befragen",
            Stage::Sos => "sicheres Betriebssystem laden",
            Stage::Ring => "Befehlskanal anlegen",
            Stage::Toc => "Inhaltsverzeichnis laden",
            Stage::Tmr => "geschützten Speicher einrichten",
            Stage::Fw => "Steuerprogramme laden",
            Stage::Autoload => "Grafikteil starten",
            Stage::FwRest => "Video- und Bildprogramm laden",
            Stage::Ta => "Vertrauensprogramme laden",
            Stage::Pmfw => "Energieverwaltung starten",
            Stage::GcBoot => "Grafikteil vorbereiten",
            Stage::GcHub => "Speicher des Grafikteils einrichten",
            Stage::GcCp => "Befehlsprozessor starten",
            Stage::Mes => "Planer der Karte starten",
            Stage::MesQ => "Warteschlangen beim Planer anmelden",
            Stage::GcRing => "Befehlsringe prüfen",
            Stage::Sdma => "Kopiereinheit der Karte starten",
            Stage::SdmaTest => "Kopiereinheit der Karte prüfen",
            Stage::RingTest => "Befehlsringe erproben",
            Stage::Destroy => "Befehlskanal abbauen",
            Stage::State => "Stand zeigen",
        }
    }

    /// What Georg reads when a stage is asked for before this one passed.
    fn first(self) -> &'static str {
        match self {
            Stage::Nbif => "zuerst den Anschluss der Karte einrichten",
            Stage::Gmc => "zuerst den Speicher der Karte einrichten",
            Stage::Ih => "zuerst die Meldungen der Karte einschalten",
            Stage::Sos => "zuerst das sichere Betriebssystem laden",
            Stage::Ring => "zuerst den Befehlskanal anlegen",
            Stage::Toc => "zuerst das Inhaltsverzeichnis laden",
            Stage::Tmr => "zuerst den geschützten Speicher einrichten",
            Stage::Fw => "zuerst die Steuerprogramme laden",
            Stage::Autoload => "zuerst den Grafikteil starten",
            Stage::FwRest => "zuerst das Video- und Bildprogramm laden",
            Stage::Ta => "zuerst die Vertrauensprogramme laden",
            Stage::Pmfw => "zuerst die Energieverwaltung der Karte starten",
            Stage::GcBoot => "zuerst den Grafikteil vorbereiten",
            Stage::GcHub => "zuerst den Speicher des Grafikteils einrichten",
            Stage::GcCp => "zuerst den Befehlsprozessor starten",
            Stage::Mes => "zuerst den Planer der Karte starten",
            Stage::MesQ => "zuerst die Warteschlangen beim Planer anmelden",
            Stage::GcRing => "zuerst die Befehlsringe prüfen",
            // The chain's last and what stands outside it: nothing waits for them.
            Stage::Sdma | Stage::SdmaTest | Stage::RingTest | Stage::Alive | Stage::Bootloader | Stage::Smu | Stage::Destroy | Stage::State => "Reihenfolge stimmt nicht",
        }
    }
}

/// The chain's order, checked in this one place for every stage of `Stage::CHAIN`: the stage before it passed in
/// this boot (`Session::before`). Linux's order of blocks: common and GMC (amdgpu_device.c:2397, :2406), IH in
/// `amdgpu_device_ip_hw_init_phase1` (:2469), the secure processor right after it in `amdgpu_device_fw_loading`
/// (:2473), and inside it `psp_load_fw`'s own order (amdgpu_psp.c:3595-3660). The looks and the destroy stand
/// outside the chain and pass.
pub fn in_order(s: &Session, stage: Stage) -> Result<(), Stop> {
    let Some((prev, false)) = s.before(stage) else { return Ok(()) };
    // A `nbif` that stopped after the remap did not pass, and says so where `gmc` is refused for it.
    let half = if prev == Stage::Nbif && s.nb.remapped { ": the one of this boot stopped half-way, run nbif again" } else { "" };
    Err(stop(prev.first(), format!("{} needs the {} stage of this boot{half} (Linux's order: {})", stage.name(), prev.name(), Stage::CHAIN.map(Stage::name).join(", "))))
}
