//! The write-refusal hook (R8a, R8b): every write, write with FUA or with a flush before it ("flush with
//! data") and every discard passes `decide` and then every plugged-in `WriteGate` before it is queued. A plain
//! cache flush carries no data and passes.
//!
//! The block layer's own rules, in this order:
//! 1. the device says read-only -> `DeviceReadOnly`;
//! 2. the partition table (read once at registration, again when the size changes) holds a Windows partition
//!    or an EFI System Partition, or could not be read -> refused for good (also for a RAM disk);
//! 3. by kind: RAM disks may be written; a USB disk that is (or, while the boot stick's identity is not known,
//!    might be) the boot stick only with CONFIG `stick_write=on` (FS-7 will allow `\BITSOS2\` instead); any
//!    other USB disk only with CONFIG `usb_msd_write=on` (a test switch); NVMe only with `nvme_write=on`
//!    (NV's gate checks it too); a stacked device (partition, crypt, bitsvol on top of a `Disk`) passes here
//!    because its writes go through the lower `Disk` and its hook; anything else is refused.

use crate::error::Refusal;
use crate::queue::Kind;
use crate::table::Verdict;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

/// What a registered device is (decides the rule in step 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskKind {
    Ram,
    Usb,
    Nvme,
    /// A device on top of another `Disk` (partition, crypt, bitsvol): the lower disk's hook applies.
    Stacked,
    Other,
}

impl DiskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DiskKind::Ram => "ram",
            DiskKind::Usb => "usb",
            DiskKind::Nvme => "nvme",
            DiskKind::Stacked => "stacked",
            DiskKind::Other => "other",
        }
    }
}

/// Is this disk the stick the system booted from?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StickMatch {
    Yes,
    No,
    /// The stick's identity is not known (no BootInfo record, block 0 unreadable for the loader): a USB disk
    /// is then treated as the stick.
    Unknown,
}

/// The CONFIG switches the rules read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Switches {
    pub stick_write: bool,
    pub usb_msd_write: bool,
    pub nvme_write: bool,
}

/// One write the hook is asked about.
#[derive(Clone, Copy, Debug)]
pub struct WriteCheck<'a> {
    pub disk: &'a str,
    pub kind: DiskKind,
    pub op: Kind,
    /// The blocks the request changes, `(lba, blocks)`. None: the disk-wide question "could any write to this
    /// disk pass?" (`Disk::write_policy`, behind `DiskDevice::info().read_only` and `disk.registered`): a
    /// gate that allows some ranges answers true there, a gate that refuses the whole disk false.
    pub range: Option<(u64, u64)>,
}

/// A plugged-in refusal rule (NV's `nvme_write` gate, FS-7's stick rule). `allows` is called for every
/// write, FUA/preflush write and discard of the disks it applies to, before the request is queued (with
/// `range: Some`), and for the disk-wide question (`range: None`); it must be cheap and must not sleep.
pub trait WriteGate: Send + Sync {
    fn name(&self) -> &'static str;
    fn allows(&self, w: &WriteCheck<'_>) -> bool;
    /// FS-7: a gate plugged into the positively identified boot stick (`StickMatch::Yes`, never `Unknown`) that
    /// opens it for the writes it allows, without CONFIG `stick_write` (FAT's `\BITSOS2\` rule: only the
    /// sectors the file system announces). Only per-disk gates are asked; the other gates still decide.
    fn opens_boot_stick(&self) -> bool {
        false
    }
}

/// Gates that apply to every disk (each decides by `WriteCheck::kind`/`disk` itself).
static GLOBAL_GATES: SpinLock<Vec<Arc<dyn WriteGate>>> = SpinLock::named("block.gates", Vec::new());

/// Plugs in a gate for every disk, also the ones registered later.
pub fn add_global_gate(g: Arc<dyn WriteGate>) {
    klog::event!(Info, "block", "gate.added", gate = g.name(), scope = "all");
    GLOBAL_GATES.lock().push(g);
}

pub(crate) fn global_gates() -> Vec<Arc<dyn WriteGate>> {
    GLOBAL_GATES.lock().clone()
}

/// Step 3's boot-stick refusal opened by a gate (FS-7; pure: host-tested): only `BootStick`, only for a disk
/// positively identified as the stick, only when one of its own gates opens it. Everything else stands.
pub fn opened(rule: Result<(), Refusal>, stick: StickMatch, gate_opens: bool) -> Result<(), Refusal> {
    match rule {
        Err(Refusal::BootStick) if stick == StickMatch::Yes && gate_opens => Ok(()),
        r => r,
    }
}

/// Steps 1-3 (pure: host-tested).
pub fn decide(kind: DiskKind, read_only: bool, table: &Verdict, stick: StickMatch, sw: Switches) -> Result<(), Refusal> {
    if read_only {
        return Err(Refusal::DeviceReadOnly);
    }
    match table {
        Verdict::Windows(_) => return Err(Refusal::WindowsPartition),
        Verdict::Efi(_) => return Err(Refusal::EfiPartition),
        Verdict::Unknown(_) => return Err(Refusal::UnknownTable),
        Verdict::Clean => {}
    }
    match kind {
        DiskKind::Ram | DiskKind::Stacked => Ok(()),
        DiskKind::Usb => {
            if stick != StickMatch::No {
                if sw.stick_write { Ok(()) } else { Err(Refusal::BootStick) }
            } else if sw.usb_msd_write {
                Ok(())
            } else {
                Err(Refusal::UsbWritesOff)
            }
        }
        DiskKind::Nvme => {
            if stick == StickMatch::Yes && !sw.stick_write {
                Err(Refusal::BootStick)
            } else if sw.nvme_write {
                Ok(())
            } else {
                Err(Refusal::NvmeWritesOff)
            }
        }
        DiskKind::Other => Err(Refusal::UnknownKind),
    }
}

/// What the loader told about the boot stick (BootInfo `Stick`), handed in by `configure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StickIdentity {
    /// The first 96 bytes of the boot volume's block 0 (its FAT boot sector), when the loader could read it.
    pub block0: Option<[u8; crate::table::FIRST_BYTES]>,
    /// USB vendor/product, when the firmware told (logged only: two sticks of one model share it).
    pub usb: Option<(u16, u16)>,
    /// `\BITSOS2\STICK.ID` as the loader read it (16 bytes from 32 hex digits; None when missing or only V1's
    /// `\BITSOS\STICK.ID` was there). FS-7 compares it with the volume's own file before writing.
    pub id: Option<[u8; 16]>,
}

pub(crate) struct Facts {
    pub config: Option<fn(&str) -> Option<String>>,
    pub stick: Option<StickIdentity>,
    pub configured: bool,
}

pub(crate) static FACTS: SpinLock<Facts> = SpinLock::named("block.facts", Facts { config: None, stick: None, configured: false });

fn on(f: Option<fn(&str) -> Option<String>>, key: &str) -> bool {
    f.and_then(|f| f(key)).is_some_and(|v| v.trim() == "on")
}

/// The switches now (all off before `configure`: the safe side).
pub fn switches() -> Switches {
    let f = FACTS.lock().config;
    Switches { stick_write: on(f, "stick_write"), usb_msd_write: on(f, "usb_msd_write"), nvme_write: on(f, "nvme_write") }
}

/// Whether any USB disk could take a write under the current switches. usb-msd asks this when a device
/// attaches (its own last line of defence: with both switches off every USB unit refuses writes itself too).
pub fn usb_writes_possible() -> bool {
    let s = switches();
    // FS-7: the identified boot stick takes writes through the FAT layer's gate (its block 0 and STICK.ID
    // known): its unit must not refuse them itself. Other USB disks stay refused by the hook (`UsbWritesOff`).
    let fat_stick = stick_identity().is_some_and(|i| i.block0.is_some() && i.id.is_some());
    s.stick_write || s.usb_msd_write || fat_stick
}

pub fn stick_identity() -> Option<StickIdentity> {
    FACTS.lock().stick
}

pub(crate) fn stick_match(scan: &crate::table::Scan) -> StickMatch {
    stick_match_with(stick_identity().and_then(|s| s.block0), scan)
}

/// The boot-stick match for the loader's block 0 (pure: host-tested). A scan that could not read every
/// partition start answers Unknown, never No: the missing block may be the stick's (R8a).
pub(crate) fn stick_match_with(block0: Option<[u8; crate::table::FIRST_BYTES]>, scan: &crate::table::Scan) -> StickMatch {
    match block0 {
        Some(b) if scan.holds_first(&b) => StickMatch::Yes,
        Some(_) if scan.incomplete => StickMatch::Unknown,
        Some(_) => StickMatch::No,
        None => StickMatch::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiskKind::*;

    const CLEAN: Verdict = Verdict::Clean;
    fn sw(stick: bool, usb: bool, nvme: bool) -> Switches {
        Switches { stick_write: stick, usb_msd_write: usb, nvme_write: nvme }
    }

    #[test]
    fn windows_and_efi_are_refused_whatever_the_switches() {
        for kind in [Ram, Usb, Nvme, Stacked, Other] {
            for stick in [StickMatch::Yes, StickMatch::No, StickMatch::Unknown] {
                let all = sw(true, true, true);
                assert_eq!(decide(kind, false, &Verdict::Windows("x".into()), stick, all), Err(Refusal::WindowsPartition));
                assert_eq!(decide(kind, false, &Verdict::Efi("x".into()), stick, all), Err(Refusal::EfiPartition));
                assert_eq!(decide(kind, false, &Verdict::Unknown("x".into()), stick, all), Err(Refusal::UnknownTable));
                assert_eq!(decide(kind, true, &CLEAN, stick, all), Err(Refusal::DeviceReadOnly));
            }
        }
    }

    #[test]
    fn usb_rules() {
        // The stick: only stick_write.
        assert_eq!(decide(Usb, false, &CLEAN, StickMatch::Yes, sw(false, true, true)), Err(Refusal::BootStick));
        assert_eq!(decide(Usb, false, &CLEAN, StickMatch::Yes, sw(true, false, false)), Ok(()));
        // Unknown identity counts as the stick.
        assert_eq!(decide(Usb, false, &CLEAN, StickMatch::Unknown, sw(false, true, false)), Err(Refusal::BootStick));
        // Other USB disks: only usb_msd_write.
        assert_eq!(decide(Usb, false, &CLEAN, StickMatch::No, sw(true, false, true)), Err(Refusal::UsbWritesOff));
        assert_eq!(decide(Usb, false, &CLEAN, StickMatch::No, sw(false, true, false)), Ok(()));
    }

    #[test]
    fn only_a_gate_on_the_identified_stick_opens_it() {
        let stick = decide(Usb, false, &CLEAN, StickMatch::Yes, Switches::default());
        assert_eq!(opened(stick, StickMatch::Yes, true), Ok(()));
        assert_eq!(opened(stick, StickMatch::Yes, false), Err(Refusal::BootStick));
        let unknown = decide(Usb, false, &CLEAN, StickMatch::Unknown, Switches::default());
        assert_eq!(opened(unknown, StickMatch::Unknown, true), Err(Refusal::BootStick), "a doubt refuses");
        for v in [Verdict::Windows("x".into()), Verdict::Efi("x".into()), Verdict::Unknown("x".into())] {
            let r = decide(Usb, false, &v, StickMatch::Yes, Switches::default());
            assert!(opened(r, StickMatch::Yes, true).is_err(), "{v:?} stays refused");
        }
        let ro = decide(Usb, true, &CLEAN, StickMatch::Yes, Switches::default());
        assert_eq!(opened(ro, StickMatch::Yes, true), Err(Refusal::DeviceReadOnly));
        let other = decide(Usb, false, &CLEAN, StickMatch::No, Switches::default());
        assert_eq!(opened(other, StickMatch::No, true), Err(Refusal::UsbWritesOff));
    }

    #[test]
    fn other_kinds() {
        assert_eq!(decide(Ram, false, &CLEAN, StickMatch::Unknown, Switches::default()), Ok(()));
        assert_eq!(decide(Stacked, false, &CLEAN, StickMatch::Unknown, Switches::default()), Ok(()));
        assert_eq!(decide(Other, false, &CLEAN, StickMatch::No, sw(true, true, true)), Err(Refusal::UnknownKind));
        assert_eq!(decide(Nvme, false, &CLEAN, StickMatch::No, sw(true, true, false)), Err(Refusal::NvmeWritesOff));
        assert_eq!(decide(Nvme, false, &CLEAN, StickMatch::Unknown, sw(false, false, true)), Ok(()));
        assert_eq!(decide(Nvme, false, &CLEAN, StickMatch::Yes, sw(false, false, true)), Err(Refusal::BootStick));
    }
}
