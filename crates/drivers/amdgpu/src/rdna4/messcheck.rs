//! The measurement run's preflight (`messung`): what must hold before anything is written to the card. Every
//! check here only reads: the card the run found and how the PCI core bound it, the read-only attach, every
//! firmware file the chain loads, by its documented header only (the licence forbids looking inside). The
//! cold card (`C2PMSG_81` 0) is the look `alive`'s first read, judged by the run itself.

use super::fw::{self, Found};
use super::stage::Stage;
use super::{fwlist, pspfiles};
use crate::devs::Key;
use crate::gpucore::fwload::{FwSource, stick_path};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The card the run is for: the Gaming-PC's display card at this PCI address (plan/spec/gpu-rdna4.md section 1).
pub const AT: Key = Key::new(0, 4, 0, 0);
/// The driver path the read-only attach binds the card to (`live`'s `pci_driver!`).
pub const DRIVER: &str = "amdgpu-rdna4";

/// What the preflight reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub key: Key,
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    pub display: bool,
    /// The PCI core's binding of the function (`pci::bind::bound_driver`).
    pub driver: Option<&'static str>,
    /// The read-only attach: Ok, or why it stopped.
    pub attach: Result<(), String>,
    /// The link path at the attach (the card first) and the power controller's facts the attach read.
    pub link: String,
    pub files: Vec<FileCheck>,
    /// The card's revision as the attach read it from STRAP0 (Linux's `adev->rev_id`; `None`: unread).
    pub rev_id: Option<u32>,
}

/// One firmware file of the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCheck {
    pub file: &'static str,
    /// The first stage that loads it, and the stages that read it later.
    pub stage: Stage,
    pub also: &'static [Stage],
    /// Ok: what its header says; Err: why it cannot be loaded.
    pub state: Result<String, String>,
    /// Its size is the one measured from linux-firmware 20261019 (plan/spec/gpu-rdna4.md section 3).
    pub measured: bool,
}

/// Which stage loads a file first, and which read it again.
fn stages_of(file: &str) -> (Stage, &'static [Stage]) {
    match file {
        fwlist::SOS => (Stage::Sos, &[Stage::Ring, Stage::Toc, Stage::Ta]),
        fwlist::TA => (Stage::Ta, &[]),
        "amdgpu/gc_12_0_1_pfp.bin" | "amdgpu/gc_12_0_1_me.bin" | "amdgpu/gc_12_0_1_mec.bin" => (Stage::Fw, &[Stage::GcBoot]),
        "amdgpu/sdma_7_0_1.bin" => (Stage::Fw, &[Stage::Sdma]),
        "amdgpu/vcn_5_0_0.bin" | "amdgpu/dcn_4_0_1_dmcub.bin" => (Stage::Fw, &[Stage::FwRest]),
        _ => (Stage::Fw, &[]),
    }
}

/// Every file the chain loads on this board, by its documented header: the common header, and the secure
/// processor's descriptors or the parts Linux takes out of a file (`fwlist::parts`).
pub fn files(device: u16, revision: u8, src: &mut dyn FwSource) -> Vec<FileCheck> {
    let wanted = fw::wanted(device, revision);
    let found = fw::on_stick(&wanted, src);
    found
        .into_iter()
        .map(|f| {
            let plain = [fwlist::SOS, fwlist::TA].into_iter().chain(fwlist::FILES.iter().map(|(n, _)| *n)).find(|p| fwlist::name_for(p, device, revision) == f.wanted.file).unwrap_or(f.wanted.file);
            let (stage, also) = stages_of(plain);
            let measured = f.usable();
            let state = match &f.found {
                Found::Missing => Err(String::from("fehlt auf dem Startdatenträger")),
                Found::Compressed => Err(String::from("liegt nur gepackt (.zst) dort")),
                Found::Unreadable(e) => Err(format!("ist nicht lesbar (errno {e})")),
                Found::Bad { why, .. } => Err(format!("hat keinen gültigen Kopf ({why})")),
                Found::Present { bytes, header: h } => inner(plain, f.wanted.file, src).map(|parts| {
                    format!("{bytes} bytes, header {}.{}, ip {}.{}, version {:#010x}{parts}", h.header_major, h.header_minor, h.ip_major, h.ip_minor, h.ucode_version)
                }),
            };
            FileCheck { file: f.wanted.file, stage, also, state, measured }
        })
        .collect()
}

/// The parts the chain takes out of a file, by the headers Linux reads.
fn inner(plain: &str, file: &str, src: &mut dyn FwSource) -> Result<String, String> {
    let bytes = match src.read(&stick_path(file)) {
        Ok(Some(b)) => b,
        _ => return Err(String::from("ließ sich beim zweiten Lesen nicht lesen")),
    };
    if plain == fwlist::SOS {
        return pspfiles::SosFile::parse(&bytes).map(|f| format!(", {} parts", f.parts.len())).map_err(|e| format!("Teile nicht lesbar ({e:?})"));
    }
    if plain == fwlist::TA {
        return pspfiles::TaFile::parse(&bytes).map(|f| format!(", {} applications", f.parts.len())).map_err(|e| format!("Anwendungen nicht lesbar ({e:?})"));
    }
    match fwlist::FILES.iter().find(|(n, _)| *n == plain) {
        Some((_, role)) => fwlist::parts(*role, &bytes).map(|p| format!(", {} images", p.len())).map_err(|e| format!("Abbilder nicht lesbar ({e:?})")),
        None => Ok(String::new()),
    }
}

/// One check's outcome: its name, and Ok (what holds) or Err (why the run does not start).
pub type Check = (&'static str, Result<String, String>);

/// The checks on what was read, in order. The run starts only when all hold.
pub fn judge(f: &Facts) -> Vec<Check> {
    let mut out: Vec<Check> = Vec::new();
    let ids = (f.vendor, f.device) == (rdna4_core::card::VENDOR, rdna4_core::card::DEVICE);
    out.push(("karte", if ids && f.key == AT { Ok(format!("{} {:04x}:{:04x} rev {:#04x}", f.key, f.vendor, f.device, f.revision)) } else { Err(format!("die Karte ist {} {:04x}:{:04x}, erwartet {AT} 1002:7550", f.key, f.vendor, f.device)) }));
    out.push(("anzeige", if f.display { Ok(String::from("die Karte zeigt das Bild")) } else { Err(String::from("die Karte zeigt das Bild nicht: die Stufen laufen nur auf der Karte mit dem Bild")) }));
    out.push((
        "treiber",
        match f.driver {
            Some(DRIVER) => Ok(format!("gebunden an {DRIVER} (den lesenden Anschluss), an keinen anderen")),
            Some(other) => Err(format!("die Karte ist an {other} gebunden, nicht an {DRIVER}")),
            None => Err(String::from("die Karte ist an keinen Treiber gebunden")),
        },
    ));
    out.push(("anschluss", f.attach.clone().map(|()| format!("der lesende Anschluss lief durch; Leitung {}", f.link)).map_err(|e| format!("der lesende Anschluss hielt an: {e}"))));
    for file in &f.files {
        out.push((
            "firmware",
            match &file.state {
                Ok(what) => Ok(format!("{}: {what}{}", file.file, if file.measured { " (wie gemessen)" } else { " (andere Größe als gemessen)" })),
                Err(why) => Err(format!("{} {why}; sie lädt die Stufe {}: das Update 175 muss den Firmware-Satz der Karte mitbringen", file.file, file.stage.name())),
            },
        ));
    }
    out
}
