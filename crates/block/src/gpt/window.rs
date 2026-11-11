//! The one window into a disk with a Windows partition (NV-2p, Georg 5.11.2026, R8a): CONFIG
//! `nvme_data=<serial>:<partition unique GUID>:<first lba>:<blocks>` together with `nvme_write=on` lets BitsOS
//! write inside exactly that GPT partition; everything else on the disk stays as unwritable as without the keys.
//!
//! This file holds the pure rules (host-tested): the key's syntax, the boot check against the GPT (`validate`),
//! the file-system signature look (`signature`) and what a request through a partition's port may do
//! (`port_rule`). The state: `Disk::arm_window`/`PartPort` (disk.rs) in the block layer and the controller's
//! `WriteWindow` (crates/drivers/nvme cmd.rs) right before a command is built.
//!
//! A doubt refuses. Every refusal leaves the disk read-only as a whole and /data on the stick's data.img.
//!
//! An EFI System partition on the disk refuses the window, except the one CONFIG `nvme_boot=` names in the same
//! form (Georg 6.11.2026: BitsOS's own boot partition on the Lexar, made by him in Windows next to the window).
//! When bitsboot came from that partition, fs-fat opens a second window of exactly its range (`boot_esp`,
//! `Disk::open_boot_window`) for /boot's writes; it never opens without the first and closes with it.

use crate::error::Refusal;
use crate::table::{GUID_EFI_SYSTEM, GUID_LINUX_DATA, GUID_MS_BASIC_DATA, GptEntry, Scan, Scheme, guid_parse, guid_text};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;

/// CONFIG.TXT's name of the window key (guarded: fs-fat `GUARDED_KEYS`).
pub const KEY: &str = "nvme_data";

/// CONFIG.TXT's name of the key that names BitsOS's own EFI System partition on the window's disk (guarded too).
pub const ESP_KEY: &str = "nvme_boot";

/// A parsed `nvme_data=` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub serial: String,
    pub guid: [u8; 16],
    pub first: u64,
    pub blocks: u64,
}

impl Key {
    /// `<serial>:<GUID>:<first>:<blocks>`; a serial may hold anything but `#` and line ends (`#` starts a CONFIG
    /// comment), so the fields are taken from the right.
    pub fn parse(v: &str) -> Result<Key, &'static str> {
        let mut it = v.trim().rsplitn(4, ':');
        let blocks = it.next().and_then(|t| t.trim().parse::<u64>().ok()).ok_or("the block count is not a number")?;
        let first = it.next().and_then(|t| t.trim().parse::<u64>().ok()).ok_or("the first LBA is not a number")?;
        let guid = it.next().and_then(guid_parse).ok_or("the GUID is not a GUID")?;
        let serial = it.next().map(str::trim).filter(|s| !s.is_empty()).ok_or("no serial")?;
        if blocks == 0 || first.checked_add(blocks).is_none() {
            return Err("the range is empty or runs past 2^64");
        }
        Ok(Key { serial: String::from(serial), guid, first, blocks })
    }

    /// The value as CONFIG.TXT holds it. None: the serial cannot stand in a CONFIG line.
    pub fn render(&self) -> Option<String> {
        if self.serial.is_empty() || !self.serial.bytes().all(|b| (0x20..0x7F).contains(&b) && b != b'#') {
            return None;
        }
        Some(format!("{}:{}:{}:{}", self.serial, guid_text(&self.guid), self.first, self.blocks))
    }

    /// The characters Georg types to confirm: the serial's last six (all of it when shorter).
    pub fn code(&self) -> String {
        let chars: Vec<char> = self.serial.chars().collect();
        chars[chars.len().saturating_sub(6)..].iter().collect()
    }
}

/// Why a disk does not take writes (each one read-only as a whole, /data on data.img).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// CONFIG `nvme_write=on` is missing.
    NoWriteKey,
    /// CONFIG `nvme_data=` is missing.
    NoDataKey,
    /// `nvme_data=` cannot be read.
    BadKey(&'static str),
    /// This disk has another serial (normal for every other disk; no window of the key's is on it).
    OtherSerial,
    /// Block size or a partition start could not be read, or the table is not understood.
    Unclear(String),
    NoGpt,
    /// A disk with an EFI System partition is refused whatever the keys say.
    Efi(String),
    /// Primary and backup GPT do not agree.
    Backup(String),
    /// No entry has the key's unique GUID (or more than one has).
    Guid,
    /// The entry starts elsewhere than the key says.
    Moved { key: u64, table: u64 },
    /// The entry's size is not the key's.
    Resized { key: u64, table: u64 },
    /// The range leaves the table's usable blocks or the disk.
    Outside,
    /// The range overlaps another entry.
    Overlap(u32),
    /// An entry of a type that is never BitsOS's: Microsoft reserved, recovery, LDM, Storage Spaces, unknown ones.
    Kind,
    /// The partition holds a file system or pool that is not this BitsOS pool.
    Signature(&'static str),
}

impl Why {
    /// One word for machines (`/proc/bitsos/data-window`, ktests).
    pub fn word(&self) -> &'static str {
        match self {
            Why::NoWriteKey => "no_nvme_write",
            Why::NoDataKey => "no_nvme_data",
            Why::BadKey(_) => "bad_key",
            Why::OtherSerial => "serial",
            Why::Unclear(_) => "unclear",
            Why::NoGpt => "no_gpt",
            Why::Efi(_) => "efi",
            Why::Backup(_) => "backup_gpt",
            Why::Guid => "guid",
            Why::Moved { .. } => "moved",
            Why::Resized { .. } => "resized",
            Why::Outside => "outside",
            Why::Overlap(_) => "overlap",
            Why::Kind => "type",
            Why::Signature(_) => "signature",
        }
    }

    pub fn text(&self) -> String {
        match self {
            Why::NoWriteKey => String::from("CONFIG.TXT has no nvme_write=on"),
            Why::NoDataKey => String::from("CONFIG.TXT has no nvme_data= line"),
            Why::BadKey(w) => format!("the nvme_data= line cannot be read: {w}"),
            Why::OtherSerial => String::from("no disk has the serial nvme_data= names"),
            Why::Unclear(w) => format!("the partition table is not clear: {w}"),
            Why::NoGpt => String::from("the disk has no GPT"),
            Why::Efi(w) => format!("the disk holds an EFI System partition, it is never written ({w})"),
            Why::Backup(w) => format!("primary and backup GPT do not agree: {w}"),
            Why::Guid => String::from("no single GPT entry has the GUID nvme_data= names"),
            Why::Moved { key, table } => format!("the partition starts at LBA {table}, nvme_data= says {key}: it was moved"),
            Why::Resized { key, table } => format!("the partition has {table} blocks, nvme_data= says {key}: it was resized"),
            Why::Outside => String::from("the range leaves the blocks the GPT may use"),
            Why::Overlap(n) => format!("the range overlaps GPT entry {n}"),
            Why::Kind => String::from("the partition's type is not one Windows gives a new data partition"),
            Why::Signature(s) => format!("the partition holds {s}"),
        }
    }
}

/// A checked window: the GPT entry `number`, blocks `first..first + blocks` of the namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub number: u32,
    pub first: u64,
    pub blocks: u64,
    /// `(first, blocks)` of the EFI System partition `nvme_boot=` names, the one the disk may hold beside it.
    pub esp: Option<(u64, u64)>,
}

/// The file-system or pool signature at the start of a partition. `head`: its first bytes (read at least
/// `HEAD_BYTES`, less only when the partition is smaller), `tail`: its last block (NTFS's backup boot sector).
/// Only signatures: Windows leaves old data in a partition it makes without formatting, so "not zero" says nothing.
pub fn signature(head: &[u8], tail: &[u8]) -> Option<&'static str> {
    let at = |o: usize, m: &[u8]| head.get(o..o + m.len()).is_some_and(|b| b == m);
    if at(0, BITSVOL) || at(4096, BITSVOL) {
        return Some(SIG_BITSVOL);
    }
    for b in [head, tail] {
        if let Some(w) = crate::table::windows_fs(b) {
            return Some(w);
        }
    }
    if at(3, b"EXFAT   ") {
        return Some("exFAT");
    }
    if at(54, b"FAT12   ") || at(54, b"FAT16   ") || at(54, b"FAT     ") || at(82, b"FAT32   ") {
        return Some("FAT");
    }
    if at(0x438, &[0x53, 0xEF]) {
        return Some("ext2/3/4");
    }
    if at(4086, b"SWAPSPACE2") || at(4086, b"SWAP-SPACE") {
        return Some("Linux swap");
    }
    if at(0, b"LUKS\xBA\xBE") {
        return Some("LUKS");
    }
    if at(0, b"XFSB") {
        return Some("XFS");
    }
    if at(0x10040, b"_BHRfS_M") {
        return Some("btrfs");
    }
    if at(512, b"EFI PART") || at(4096, b"EFI PART") {
        return Some("a partition table");
    }
    if at(510, &[0x55, 0xAA]) {
        return Some("a boot sector");
    }
    None
}

/// Is `tail`, the last block of a partition at LBA `tail_lba`, the backup boot sector of the NTFS volume in the
/// partition before it, left there when Windows shrank that volume and made this partition in the room? A shrink
/// writes a new backup at the volume's new end and leaves the old one, so a partition made without formatting ends in
/// it. Proof, all of it: `tail` has the same volume serial as `boot`, the first block of the partition before (at LBA
/// `before`), and that volume, as `tail` gives its size, ended exactly here. Anything else stays a signature.
pub fn left_over_backup(before: u64, boot: &[u8], tail_lba: u64, tail: &[u8], block_size: u32) -> bool {
    let ntfs = |b: &[u8]| crate::table::windows_fs(b) == Some("NTFS boot sector") && b.len() >= 0x50;
    if !ntfs(boot) || !ntfs(tail) {
        return false;
    }
    let bytes_per_sector = u32::from(u16::from_le_bytes([tail[0x0B], tail[0x0C]]));
    let mut total = [0u8; 8];
    total.copy_from_slice(&tail[0x28..0x30]);
    let serial = &tail[0x48..0x50];
    bytes_per_sector == block_size
        && serial.iter().any(|b| *b != 0)
        && serial == &boot[0x48..0x50]
        && before.checked_add(u64::from_le_bytes(total)) == Some(tail_lba)
}

const BITSVOL: &[u8] = b"BITSVOL1";
/// What `signature` calls a bitsvol pool.
pub const SIG_BITSVOL: &str = "a bitsvol pool";
/// How much of a partition's start `signature` looks at (btrfs's superblock lies at 64 KiB).
pub const HEAD_BYTES: usize = 0x10048;

/// The boot check (pure): does the key name exactly one partition of this disk, and may it be written?
/// `Why::OtherSerial` is the normal answer of every other disk. `sig` is `signature` of the named partition (None
/// for a partition with none); a bitsvol pool at the key's own place is this BitsOS's pool (the key names it).
pub fn validate(key: &Key, esp: Option<&Key>, serial: &str, total: u64, scan: &Scan, sig: Option<&'static str>) -> Result<Window, Why> {
    if serial.trim() != key.serial {
        return Err(Why::OtherSerial);
    }
    let e = check_table(&key.guid, total, scan, key, esp)?;
    if let Some(s) = sig
        && s != SIG_BITSVOL
    {
        return Err(Why::Signature(s));
    }
    Ok(Window { number: e.number, first: e.first, blocks: key.blocks, esp: esp.map(|k| (k.first, k.blocks)) })
}

/// The table's side of `validate`: everything but the serial and the signature. Also what `bitsdisk` asks for a
/// partition it offers (with the key made from the entry itself). `esp`: CONFIG's `nvme_boot=`, the one EFI System
/// partition the disk may hold.
pub fn check_table<'a>(guid: &[u8; 16], total: u64, scan: &'a Scan, key: &Key, esp: Option<&Key>) -> Result<&'a GptEntry, Why> {
    if let Some(u) = &scan.unknown {
        return Err(Why::Unclear(u.clone()));
    }
    if scan.incomplete {
        return Err(Why::Unclear(scan.unreadable.clone().unwrap_or_else(|| "a partition start is unreadable".to_string())));
    }
    let Some(g) = scan.gpt.as_ref().filter(|_| scan.scheme == Some(Scheme::Gpt)) else {
        return Err(scan.efi.first().map_or(Why::NoGpt, |e| Why::Efi(e.clone())));
    };
    if let Some(d) = &g.disagreement {
        return Err(Why::Backup(d.clone()));
    }
    let esps: Vec<&GptEntry> = g.entries.iter().filter(|e| e.kind == GUID_EFI_SYSTEM).collect();
    // Every ESP the scan saw must be a GPT entry (a hybrid MBR's EFh one is not) and the one `nvme_boot=` names.
    if let Some(e) = scan.efi.first()
        && (scan.efi.len() != esps.len() || !esps.iter().all(|e| esp.is_some_and(|k| names(k, &key.serial, e, g))))
    {
        // One GPT ESP: the line that would name it, for Georg to copy if it is BitsOS's own (he decides, the key
        // is guarded).
        let hint = match esps.as_slice() {
            [one] if scan.efi.len() == 1 => {
                let k = Key { serial: key.serial.clone(), guid: one.unique, first: one.first, blocks: one.last.saturating_sub(one.first).saturating_add(1) };
                k.render().map(|v| format!("; {ESP_KEY}={v} names it if it is BitsOS's own"))
            }
            _ => None,
        };
        return Err(Why::Efi(format!("{e}{}", hint.unwrap_or_default())));
    }
    let mut it = g.entries.iter().filter(|e| e.unique == *guid);
    let (Some(e), None) = (it.next(), it.next()) else { return Err(Why::Guid) };
    // Windows gives a new partition "Microsoft basic data"; a Linux tool "Linux data". Never Microsoft reserved:
    // it holds no signature to warn anybody.
    if e.kind != GUID_MS_BASIC_DATA && e.kind != GUID_LINUX_DATA {
        return Err(Why::Kind);
    }
    if e.first != key.first {
        return Err(Why::Moved { key: key.first, table: e.first });
    }
    let have = e.last.checked_sub(e.first).and_then(|d| d.checked_add(1)).ok_or(Why::Outside)?;
    if have != key.blocks {
        return Err(Why::Resized { key: key.blocks, table: have });
    }
    if e.first < g.first_usable || e.last > g.last_usable || e.last >= total {
        return Err(Why::Outside);
    }
    if let Some(n) = overlap(g, scan, e) {
        return Err(Why::Overlap(n));
    }
    // The named ESP's window can write all of it: its range must be its own as much as the data's (the driver
    // stacks a disk on every listed entry, and a port is known only by its range).
    if let Some(n) = esps.iter().find_map(|x| overlap(g, scan, x)) {
        return Err(Why::Overlap(n));
    }
    Ok(e)
}

/// The first entry other than GPT entry `e` that overlaps it: another GPT entry, or one of a hybrid MBR's (in
/// `parts` too; the GPT's own are found again by their name, number and place).
fn overlap(g: &crate::table::GptFacts, scan: &Scan, e: &GptEntry) -> Option<u32> {
    if let Some(o) = g.entries.iter().find(|o| o.number != e.number && o.first <= e.last && e.first <= o.last) {
        return Some(o.number);
    }
    scan.parts.iter().find_map(|(first, blocks, kind, number)| {
        let is_gpt = kind.starts_with("GPT ") && g.entries.iter().any(|o| o.number == *number && o.first == *first);
        let last = first.saturating_add(blocks.saturating_sub(1));
        (!is_gpt && *first <= e.last && e.first <= last).then_some(*number)
    })
}

/// Does the scan see an EFI System partition other than `esp` (`(first, blocks)` of the named one)? An MBR EFh one
/// or a second GPT one always counts. The block layer asks this on every write through the window and on a rescan.
pub fn foreign_esp(scan: &Scan, esp: Option<(u64, u64)>) -> bool {
    if scan.efi.is_empty() {
        return false;
    }
    let Some(g) = scan.gpt.as_ref() else { return true };
    let esps: Vec<&GptEntry> = g.entries.iter().filter(|e| e.kind == GUID_EFI_SYSTEM).collect();
    scan.efi.len() != esps.len() || esps.iter().any(|e| Some((e.first, e.last.saturating_sub(e.first).saturating_add(1))) != esp)
}

/// `k` names exactly entry `e` of this disk, inside the usable blocks.
fn names(k: &Key, serial: &str, e: &GptEntry, g: &crate::table::GptFacts) -> bool {
    let have = e.last.checked_sub(e.first).and_then(|d| d.checked_add(1));
    k.serial == serial && k.guid == e.unique && k.first == e.first && have == Some(k.blocks) && e.first >= g.first_usable && e.last <= g.last_usable
}

/// What a request through a partition's port may do (pure). `window`: the armed `(first, blocks)` of `nvme_data=`;
/// `boot`: the named ESP's window, opened once fs-fat took it for the boot volume (`boot_esp`); `port`: this
/// partition's; `range`: the request in the namespace's blocks (None: "could any write pass?").
pub fn port_rule(window: Option<(u64, u64)>, boot: Option<(u64, u64)>, port: (u64, u64), read_only: bool, range: Option<(u64, u64)>) -> Result<(), Refusal> {
    if read_only {
        return Err(Refusal::DeviceReadOnly);
    }
    let (w, key) = match (window, boot) {
        (Some(w), _) if w == port => (w, KEY),
        // The boot ESP's window lives only beside nvme_data's: it never opens a disk by itself.
        (Some(_), Some(b)) if b == port => (b, ESP_KEY),
        _ => return Err(Refusal::WindowsPartition),
    };
    match range {
        None => Ok(()),
        Some((lba, n)) if inside(w, lba, n) => Ok(()),
        Some(_) => Err(Refusal::Gate(key)),
    }
}

/// May the boot volume bitsboot came from, the partition of `start..start + blocks` on an NVMe disk, be written
/// (pure; fs-fat asks it before /boot there takes writes)? Only when it is exactly the EFI System partition
/// `nvme_boot=` names on the disk that holds `nvme_data=`'s armed window, and the table still passes that window's
/// check with both keys. `keys`: `boot_keys()`, `esp`: `esp_key()`, `armed`: the disk's window, `named`: the ESP it
/// was armed with. Err: the first rule that failed.
#[allow(clippy::too_many_arguments)]
pub fn boot_esp(keys: &Result<Key, Why>, esp: &Result<Option<Key>, Why>, armed: Option<(u64, u64)>, named: Option<(u64, u64)>, total: u64, scan: &Scan, start: u64, blocks: u64) -> Result<(), &'static str> {
    let key = match keys {
        Ok(k) => k,
        Err(Why::NoWriteKey) => return Err("CONFIG.TXT has no nvme_write=on"),
        Err(Why::NoDataKey) => return Err("CONFIG.TXT has no nvme_data= line (the boot partition is written only beside its window)"),
        Err(_) => return Err("the nvme_data= line cannot be read"),
    };
    let esp = match esp {
        Ok(Some(e)) => e,
        Ok(None) => return Err("CONFIG.TXT has no nvme_boot= line naming the boot partition"),
        Err(_) => return Err("the nvme_boot= line cannot be read"),
    };
    if esp.serial != key.serial {
        return Err("nvme_boot= names another disk than nvme_data=");
    }
    if armed != Some((key.first, key.blocks)) {
        return Err("nvme_data='s window is not armed on the boot disk");
    }
    if named != Some((esp.first, esp.blocks)) {
        return Err("the boot disk's window was armed without this nvme_boot= partition");
    }
    if check_table(&key.guid, total, scan, key, Some(esp)).is_err() {
        return Err("the boot disk's partition table no longer passes the window check");
    }
    let named_here = scan.gpt.as_ref().and_then(|g| g.entries.iter().find(|e| e.first == start)).is_some_and(|e| {
        e.kind == GUID_EFI_SYSTEM && e.unique == esp.guid && e.first == esp.first && e.last.checked_sub(e.first).and_then(|d| d.checked_add(1)) == Some(esp.blocks)
    });
    if !named_here || start != esp.first || blocks != esp.blocks {
        return Err("the boot volume is not the EFI System partition nvme_boot= names");
    }
    Ok(())
}

/// `lba..lba + n` lies inside `(first, blocks)`, and it is not empty.
pub fn inside(w: (u64, u64), lba: u64, n: u64) -> bool {
    n > 0 && lba >= w.0 && lba.checked_add(n).is_some_and(|e| e <= w.0.saturating_add(w.1))
}

/// Where the boot's window stands, for /data's thread (fs-ext4 data.rs) and `/proc/bitsos/data-window`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// Keys there, the driver is still looking at its disks.
    Looking,
    /// The partition's stacked disk, with its window armed.
    Armed { disk: String, part: String },
    /// Keys there, refused: the words for the log.
    Refused { word: &'static str, text: String },
}

static BOOT: SpinLock<Boot> = SpinLock::named("block.window.boot", Boot::Looking);
/// /data stopped waiting for a window and went on without one.
static DATA_WENT_ON: AtomicBool = AtomicBool::new(false);
/// The driver's "have you seen everything by now?" (`set_look`).
static LOOK: SpinLock<Option<fn()>> = SpinLock::named("block.window.look", None);

pub fn boot_state() -> Boot {
    BOOT.lock().clone()
}

/// A disk decided (only the driver calls these). An armed window is never replaced by a later refusal of another
/// disk, except by a second disk that claims the same key: then both are refused (`set_conflict`).
pub fn set_armed(disk: &str, part: &str) {
    *BOOT.lock() = Boot::Armed { disk: String::from(disk), part: String::from(part) };
    if DATA_WENT_ON.load(Ordering::Acquire) {
        klog::event!(Warn, "block", "window.armed_late", disk = disk, part = part; "/data stopped waiting already and stays on the stick's data.img until the next boot");
    }
}

/// /data's thread stopped waiting (fs-ext4 data.rs): a window armed after this does not carry /data at this boot.
pub fn data_went_on() {
    DATA_WENT_ON.store(true, Ordering::Release);
}

/// The driver hands in how to ask it: with no controller at all none of its code would run on its own, and a key
/// that no disk answers to would keep /data waiting for its whole bound.
pub fn set_look(f: fn()) {
    *LOOK.lock() = Some(f);
}

/// Asks the driver whether it has seen every controller; if so, `Looking` ends.
pub fn look() {
    let f = *LOOK.lock();
    if let Some(f) = f {
        f();
    }
}

pub fn set_refused(why: &Why) {
    let mut b = BOOT.lock();
    if !matches!(*b, Boot::Armed { .. }) {
        *b = Boot::Refused { word: why.word(), text: why.text() };
    }
}

/// Two disks answer to one key: nothing is armed any more.
pub fn set_conflict(text: String) {
    *BOOT.lock() = Boot::Refused { word: "conflict", text };
}

/// The boot's keys, as CONFIG has them at this boot: `nvme_write` strictly from the file (MC-10's run-time opener
/// never opens a window) and the window key. Before `block::configure` everything counts as missing.
pub fn boot_keys() -> Result<Key, Why> {
    let f = crate::policy::FACTS.lock().config;
    let get = |k: &str| f.and_then(|f| f(k));
    let key = get(KEY).ok_or(Why::NoDataKey)?;
    let key = Key::parse(&key).map_err(Why::BadKey)?;
    if !write_key_on() {
        return Err(Why::NoWriteKey);
    }
    Ok(key)
}

/// CONFIG's `nvme_boot=` at this boot: None without the line, `BadKey` when it cannot be read (a doubt refuses).
pub fn esp_key() -> Result<Option<Key>, Why> {
    let f = crate::policy::FACTS.lock().config;
    match f.and_then(|f| f(ESP_KEY)) {
        None => Ok(None),
        Some(v) => Key::parse(&v).map(Some).map_err(Why::BadKey),
    }
}

/// CONFIG `nvme_write=on` at this boot, the file's word only.
pub fn write_key_on() -> bool {
    let f = crate::policy::FACTS.lock().config;
    f.and_then(|f| f("nvme_write")).is_some_and(|v| v.trim() == "on")
}

/// CONFIG has an `nvme_data=` line at this boot, readable or not.
pub fn config_has_key() -> bool {
    let f = crate::policy::FACTS.lock().config;
    f.and_then(|f| f(KEY)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn head() -> Vec<u8> {
        vec![0u8; HEAD_BYTES]
    }

    #[test]
    fn signatures() {
        let tail = vec![0u8; 512];
        assert_eq!(signature(&head(), &tail), None, "blank");
        assert_eq!(signature(&[0x77u8; HEAD_BYTES], &tail), None, "old data is no signature");
        let with = |o: usize, m: &[u8]| {
            let mut h = head();
            h[o..o + m.len()].copy_from_slice(m);
            signature(&h, &tail)
        };
        assert_eq!(with(3, b"NTFS    "), Some("NTFS boot sector"));
        assert_eq!(with(3, b"-FVE-FS-"), Some("BitLocker boot sector"));
        assert_eq!(with(3, b"ReFS\0\0\0\0"), Some("ReFS boot sector"));
        assert_eq!(with(3, b"EXFAT   "), Some("exFAT"));
        assert_eq!(with(82, b"FAT32   "), Some("FAT"));
        assert_eq!(with(54, b"FAT16   "), Some("FAT"));
        assert_eq!(with(0x438, &[0x53, 0xEF]), Some("ext2/3/4"));
        assert_eq!(with(4086, b"SWAPSPACE2"), Some("Linux swap"));
        assert_eq!(with(0, b"BITSVOL1"), Some(SIG_BITSVOL));
        assert_eq!(with(4096, b"BITSVOL1"), Some(SIG_BITSVOL), "the second superblock");
        assert_eq!(with(510, &[0x55, 0xAA]), Some("a boot sector"));
        let mut t = vec![0u8; 512];
        t[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(signature(&head(), &t), Some("NTFS boot sector"), "NTFS's backup at the end");
    }

    fn ntfs(serial: u8, total: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x28..0x30].copy_from_slice(&total.to_le_bytes());
        b[0x48..0x50].fill(serial);
        b
    }

    #[test]
    fn a_shrunk_volumes_old_backup_is_told_from_a_volume() {
        // The Mini-PC's Lexar: volume at 32768, shrunk, the new partition's last block is the old end's backup.
        let (before, end) = (32768, 3_907_026_943);
        let boot = ntfs(7, 1_809_842_175);
        let old = ntfs(7, end - before);
        assert!(left_over_backup(before, &boot, end, &old, 512));
        assert!(!left_over_backup(before, &boot, end, &ntfs(8, end - before), 512), "another volume's serial");
        assert!(!left_over_backup(before, &boot, end, &ntfs(0, end - before), 512), "no serial proves nothing");
        assert!(!left_over_backup(before, &ntfs(0, 1), end, &ntfs(0, end - before), 512), "both without a serial");
        assert!(!left_over_backup(before, &boot, end, &ntfs(7, end - before - 1), 512), "it ended elsewhere");
        assert!(!left_over_backup(before, &boot, end, &old, 4096), "another sector size");
        assert!(!left_over_backup(before, &boot, end, &[0u8; 512], 512), "no backup at all");
        assert!(!left_over_backup(before, &vec![0u8; 512], end, &old, 512), "the partition before holds no NTFS");
    }

    #[test]
    fn the_port_rule() {
        let (w, other) = ((100, 50), (10, 50));
        let ok = |r| port_rule(Some(w), None, w, false, r);
        assert_eq!(ok(None), Ok(()));
        assert_eq!(ok(Some((100, 50))), Ok(()));
        assert_eq!(ok(Some((99, 1))), Err(Refusal::Gate(KEY)));
        assert_eq!(ok(Some((150, 1))), Err(Refusal::Gate(KEY)));
        assert_eq!(ok(Some((149, 2))), Err(Refusal::Gate(KEY)));
        assert_eq!(ok(Some((u64::MAX, 2))), Err(Refusal::Gate(KEY)));
        assert_eq!(port_rule(None, None, w, false, None), Err(Refusal::WindowsPartition), "nothing armed");
        assert_eq!(port_rule(Some(w), None, other, false, Some((100, 1))), Err(Refusal::WindowsPartition), "another partition");
        assert_eq!(port_rule(Some(w), None, w, true, None), Err(Refusal::DeviceReadOnly));
    }

    /// The Lexar's two windows: the boot ESP at 10..60, nvme_data's at 100..150, Windows in between and around.
    #[test]
    fn two_windows_each_through_its_own_port() {
        let (data, esp, win) = ((100, 50), (10, 50), (60, 40));
        let rule = |boot, port, r| port_rule(Some(data), boot, port, false, r);
        assert_eq!(rule(None, esp, None), Err(Refusal::WindowsPartition), "the ESP stays shut until fs-fat opens it");
        assert_eq!(rule(Some(esp), esp, Some((10, 50))), Ok(()));
        assert_eq!(rule(Some(esp), data, Some((100, 50))), Ok(()), "nvme_data's window as before");
        for (lba, n) in [(9, 1), (60, 1), (59, 2), (10, 51), (100, 1), (0, 1)] {
            assert_eq!(rule(Some(esp), esp, Some((lba, n))), Err(Refusal::Gate(ESP_KEY)), "ESP port {lba}+{n}");
        }
        for (lba, n) in [(99, 1), (150, 1), (59, 1), (60, 41), (10, 1)] {
            assert_eq!(rule(Some(esp), data, Some((lba, n))), Err(Refusal::Gate(KEY)), "data port {lba}+{n}");
        }
        assert_eq!(rule(Some(esp), win, Some((60, 1))), Err(Refusal::WindowsPartition), "the Windows partition between");
        assert_eq!(port_rule(None, Some(esp), esp, false, Some((10, 1))), Err(Refusal::WindowsPartition), "never without nvme_data's");
        assert_eq!(port_rule(Some(data), Some(esp), esp, true, Some((10, 1))), Err(Refusal::DeviceReadOnly));
    }

    fn lexar() -> (Scan, Key, Key) {
        use crate::table::{GUID_MS_RESERVED, GptFacts};
        let e = |number, kind, first: u64, last: u64| GptEntry { number, kind, unique: [number as u8; 16], first, last };
        let entries = vec![e(1, GUID_MS_RESERVED, 34, 2047), e(2, GUID_MS_BASIC_DATA, 2048, 9999), e(3, GUID_EFI_SYSTEM, 10_000, 19_999), e(4, GUID_MS_BASIC_DATA, 20_000, 39_999)];
        let parts = entries.iter().map(|x| (x.first, x.last - x.first + 1, format!("GPT {}", x.number), x.number)).collect();
        let scan = Scan {
            scheme: Some(Scheme::Gpt),
            parts,
            efi: vec![String::from("GPT 3: EFI System at LBA 10000")],
            gpt: Some(GptFacts { disk_guid: [0; 16], first_usable: 34, last_usable: 40_000, entries, disagreement: None }),
            ..Scan::default()
        };
        let k = |n: u8, first, blocks| Key { serial: String::from("LEXAR"), guid: [n; 16], first, blocks };
        (scan, k(4, 20_000, 20_000), k(3, 10_000, 10_000))
    }

    #[test]
    fn the_boot_volume_on_the_disk_is_only_the_named_esp() {
        let (scan, data, esp) = lexar();
        let (armed, named) = (Some((20_000, 20_000)), Some((10_000, 10_000)));
        let rule = |keys: Result<Key, Why>, e: Result<Option<Key>, Why>, armed, named, start, blocks| boot_esp(&keys, &e, armed, named, 40_100, &scan, start, blocks);
        assert_eq!(rule(Ok(data.clone()), Ok(Some(esp.clone())), armed, named, 10_000, 10_000), Ok(()));
        let cases: Vec<(Result<Key, Why>, Result<Option<Key>, Why>, Option<(u64, u64)>, Option<(u64, u64)>, u64, u64, &str)> = vec![
            (Err(Why::NoWriteKey), Ok(Some(esp.clone())), armed, named, 10_000, 10_000, "nvme_write=on"),
            (Err(Why::NoDataKey), Ok(Some(esp.clone())), armed, named, 10_000, 10_000, "no nvme_data="),
            (Ok(data.clone()), Ok(None), armed, named, 10_000, 10_000, "no nvme_boot="),
            (Ok(data.clone()), Err(Why::BadKey("x")), armed, named, 10_000, 10_000, "nvme_boot= line cannot"),
            (Ok(data.clone()), Ok(Some(Key { serial: String::from("OTHER"), ..esp.clone() })), armed, named, 10_000, 10_000, "another disk"),
            (Ok(data.clone()), Ok(Some(esp.clone())), None, named, 10_000, 10_000, "not armed"),
            (Ok(data.clone()), Ok(Some(esp.clone())), armed, None, 10_000, 10_000, "armed without"),
            (Ok(data.clone()), Ok(Some(Key { guid: [2; 16], ..esp.clone() })), armed, named, 10_000, 10_000, "window check"),
            (Ok(data.clone()), Ok(Some(Key { blocks: 9_999, ..esp.clone() })), armed, Some((10_000, 9_999)), 10_000, 9_999, "window check"),
            (Ok(data.clone()), Ok(Some(esp.clone())), armed, named, 2048, 7952, "not the EFI System partition"),
            (Ok(data.clone()), Ok(Some(esp.clone())), armed, named, 20_000, 20_000, "not the EFI System partition"),
            (Ok(data.clone()), Ok(Some(esp.clone())), armed, named, 10_000, 9_000, "not the EFI System partition"),
        ];
        for (k, e, a, n, start, blocks, want) in cases {
            let r = rule(k, e, a, n, start, blocks);
            assert!(r.is_err_and(|t| t.contains(want)), "{want}: {r:?}");
        }
    }

    /// The ESP's range is its own like the data's: a hybrid MBR entry mirroring it (not EFh) or a GPT entry
    /// overlapping it gets a stacked disk whose port would be the boot window's.
    #[test]
    fn the_named_esp_overlaps_nothing() {
        let (scan, data, esp) = lexar();
        let check = |s: &Scan| check_table(&data.guid, 40_100, s, &data, Some(&esp)).map(|e| e.number);
        assert_eq!(check(&scan), Ok(4));
        let mut mirror = scan.clone();
        mirror.parts.push((10_000, 10_000, String::from("MBR 1 type 0Bh"), 1));
        assert_eq!(check(&mirror), Err(Why::Overlap(1)), "an MBR entry on the ESP's range");
        let mut same_number = scan.clone();
        same_number.parts.push((10_000, 10_000, String::from("MBR 3 type 07h"), 3));
        assert_eq!(check(&same_number), Err(Why::Overlap(3)), "an MBR entry with the ESP's number and place");
        let mut part_of = scan.clone();
        part_of.parts.push((15_000, 100, String::from("MBR 2 type 07h"), 2));
        assert_eq!(check(&part_of), Err(Why::Overlap(2)), "an MBR entry inside the ESP");
        let mut gpt = scan.clone();
        let g = gpt.gpt.as_mut().unwrap();
        g.entries[1].last = 10_000;
        assert_eq!(check(&gpt), Err(Why::Overlap(2)), "the Windows entry runs into the ESP");
        let mut data_mirror = scan.clone();
        data_mirror.parts.push((20_000, 20_000, String::from("MBR 4 type 07h"), 4));
        assert_eq!(check(&data_mirror), Err(Why::Overlap(4)), "an MBR entry with the data's number and place");
    }
}
