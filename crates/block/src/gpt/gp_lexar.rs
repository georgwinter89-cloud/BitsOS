//! GP-DISK: the window rules against the Gaming-PC's Lexar NQ790 2 TB as Georg laid it out himself (15.11.2026, read
//! under CachyOS from `/sys/block/nvme1n1/nvme1n1p*/{start,size}` and `lsblk`): its real size (3 907 029 168 blocks
//! of 512 bytes), CachyOS's ext4 partition "daten" from LBA 2048 shrunk by 401 GiB at its end, then a 16 GiB EFI
//! System partition (FAT32, label BITSOS; the size of the Mini-PC's, whose program trees under `\BITSOS2\` take
//! 4.1 GB of it) and a 385 GiB "Linux data" partition without a file system. The table is built sparse (only the
//! blocks that hold something), read through the same `table::scan` the boot uses. The serial is made up.

use super::window::{Key, Why, boot_esp, check_table, foreign_esp, signature, validate};
use crate::table::{self, GUID_EFI_SYSTEM, GUID_LINUX_DATA, Scan};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const BS: u64 = 512;
const TOTAL: u64 = 3_907_029_168;
const GIB: u64 = (1 << 30) / BS;
/// CachyOS's "daten" today: LBA 2048, 3 907 026 944 blocks.
const DATEN_FIRST: u64 = 2048;
const DATEN_LAST_NOW: u64 = 2048 + 3_907_026_944 - 1;
const ESP_GIB: u64 = 16;
const DATA_GIB: u64 = 385;
const DATEN_LAST: u64 = DATEN_LAST_NOW - (ESP_GIB + DATA_GIB) * GIB;
const ESP: (u64, u64) = (DATEN_LAST + 1, DATEN_LAST + ESP_GIB * GIB);
const DATA: (u64, u64) = (ESP.1 + 1, ESP.1 + DATA_GIB * GIB);
const SERIAL: &str = "LXGPTEST0000ZK42";

fn uniq(i: u8) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[0] = i + 1;
    g
}

/// The disk's non-zero blocks.
struct Sparse(BTreeMap<u64, Vec<u8>>);

impl Sparse {
    fn put(&mut self, lba: u64, bytes: &[u8]) {
        for (i, chunk) in bytes.chunks(BS as usize).enumerate() {
            let mut b = vec![0u8; BS as usize];
            b[..chunk.len()].copy_from_slice(chunk);
            self.0.insert(lba + i as u64, b);
        }
    }
    fn read(&self, lba: u64, n: u64) -> Option<Vec<u8>> {
        if lba.checked_add(n)? > TOTAL {
            return None;
        }
        let mut out = vec![0u8; (n * BS) as usize];
        for (l, b) in self.0.range(lba..lba + n) {
            let at = ((l - lba) * BS) as usize;
            out[at..at + BS as usize].copy_from_slice(b);
        }
        Some(out)
    }
    fn scan(&self) -> Scan {
        table::scan(&mut |lba, n| self.read(lba, n), BS as u32, TOTAL)
    }
    /// What the boot reads of a partition for `signature`: its first 128 KiB and its last block.
    fn sig(&self, (first, last): (u64, u64)) -> Option<&'static str> {
        signature(&self.read(first, 256).unwrap(), &self.read(last, 1).unwrap())
    }
}

fn lexar(parts: &[([u8; 16], u64, u64)]) -> Sparse {
    lexar_fat_at(parts, ESP.0)
}

fn lexar_fat_at(parts: &[([u8; 16], u64, u64)], esp_first: u64) -> Sparse {
    let mut d = Sparse(BTreeMap::new());
    for (lba, bytes) in crate::testimg::gpt_pieces(BS as usize, TOTAL, parts) {
        d.put(lba, &bytes);
    }
    // "daten": an ext4 superblock (magic 0xEF53 at byte 0x438); the ESP: mkfs.fat's FAT32 boot sector.
    let mut sb = vec![0u8; 4096];
    sb[0x438] = 0x53;
    sb[0x439] = 0xEF;
    d.put(DATEN_FIRST, &sb);
    let mut fat = crate::testimg::boot_sector(b"mkfs.fat", 0x1234_5678).to_vec();
    fat[82..90].copy_from_slice(b"FAT32   ");
    d.put(esp_first, &fat);
    d
}

fn georgs() -> Sparse {
    lexar(&[(GUID_LINUX_DATA, DATEN_FIRST, DATEN_LAST), (GUID_EFI_SYSTEM, ESP.0, ESP.1), (GUID_LINUX_DATA, DATA.0, DATA.1)])
}

fn key(number: u8, (first, last): (u64, u64)) -> Key {
    Key { serial: String::from(SERIAL), guid: uniq(number - 1), first, blocks: last - first + 1 }
}

/// The layout's numbers themselves: exactly the blocks Georg's partitions have (sysfs `start`/`size`), 1 MiB aligned,
/// inside the usable range.
#[test]
fn the_layout_is_the_one_georg_made() {
    assert_eq!(DATEN_LAST_NOW, 3_907_028_991);
    assert_eq!((DATEN_LAST - DATEN_FIRST + 1, ESP.0, ESP.1 - ESP.0 + 1, DATA.0, DATA.1 - DATA.0 + 1), (3_066_068_992, 3_066_071_040, 33_554_432, 3_099_625_472, 807_403_520));
    assert_eq!((DATEN_LAST_NOW - DATEN_LAST) * BS, 401 << 30, "what was cut off");
    for first in [DATEN_FIRST, ESP.0, DATA.0] {
        assert_eq!(first % 2048, 0, "{first} is not 1 MiB aligned");
    }
    assert_eq!((ESP.1 - ESP.0 + 1) * BS, ESP_GIB << 30);
    assert_eq!((DATA.1 - DATA.0 + 1) * BS, DATA_GIB << 30);
    assert_eq!(DATA.1, DATEN_LAST_NOW, "the two new partitions fill exactly what was cut off");
    let g = georgs().scan().gpt.unwrap();
    assert!(g.first_usable <= DATEN_FIRST && DATA.1 <= g.last_usable && g.disagreement.is_none());
    assert_eq!(g.entries.len(), 3);
}

/// Every rule of the boot check and of bitsdisk on this drive: the window opens on the 385 GiB partition with the
/// ESP named, and nothing else can become one.
#[test]
fn the_window_rules_on_georgs_lexar() {
    let d = georgs();
    let s = d.scan();
    let (data, esp) = (key(3, DATA), key(2, ESP));
    assert_eq!(d.sig(DATA), None, "the new partition is empty");
    let w = validate(&data, Some(&esp), SERIAL, TOTAL, &s, d.sig(DATA)).expect("the window on the 385 GiB partition");
    assert_eq!((w.number, w.first, w.blocks, w.esp), (3, DATA.0, DATA.1 - DATA.0 + 1, Some((ESP.0, ESP.1 - ESP.0 + 1))));
    assert!(!foreign_esp(&s, w.esp), "the drive holds no ESP but BitsOS's own");
    assert!(foreign_esp(&s, None), "without nvme_boot= the BITSOS partition is a foreign ESP");

    // Without nvme_boot= the ESP refuses the window, and the refusal hands Georg the very line that names it.
    let Err(Why::Efi(text)) = validate(&data, None, SERIAL, TOTAL, &s, None) else { panic!("an unnamed ESP must refuse") };
    assert!(text.ends_with(&alloc::format!("; nvme_boot={} names it if it is BitsOS's own", esp.render().unwrap())), "{text}");

    // bitsdisk's offer (`setup::offer` = this table check + the signature): "daten" passes the table (it is a
    // Linux data partition too) and is refused by its ext4 superblock alone; the ESP is no data partition.
    let daten = key(1, (DATEN_FIRST, DATEN_LAST));
    assert!(check_table(&daten.guid, TOTAL, &s, &daten, Some(&esp)).is_ok());
    assert_eq!(d.sig((DATEN_FIRST, DATEN_LAST)), Some("ext2/3/4"));
    assert_eq!(validate(&daten, Some(&esp), SERIAL, TOTAL, &s, d.sig((DATEN_FIRST, DATEN_LAST))), Err(Why::Signature("ext2/3/4")));
    assert_eq!(validate(&key(2, ESP), Some(&esp), SERIAL, TOTAL, &s, d.sig(ESP)), Err(Why::Kind));
    // Another drive's line (the Mini-PC's CONFIG.TXT on this stick), a moved or resized key: refused.
    assert_eq!(validate(&data, Some(&esp), "MINIPCLEXAR123", TOTAL, &s, None), Err(Why::OtherSerial));
    assert!(matches!(validate(&Key { first: DATA.0 + 2048, ..data.clone() }, Some(&esp), SERIAL, TOTAL, &s, None), Err(Why::Moved { .. })));
    assert!(matches!(validate(&Key { blocks: data.blocks - 2048, ..data.clone() }, Some(&esp), SERIAL, TOTAL, &s, None), Err(Why::Resized { .. })));
    // An nvme_boot= line with the wrong size names no ESP of this drive.
    assert!(matches!(validate(&data, Some(&Key { blocks: esp.blocks - 2048, ..esp.clone() }), SERIAL, TOTAL, &s, None), Err(Why::Efi(_))));

    // /boot on that ESP takes writes only with both keys, both windows armed, and exactly that partition.
    let (armed, named) = (Some((data.first, data.blocks)), Some((esp.first, esp.blocks)));
    assert_eq!(boot_esp(&Ok(data.clone()), &Ok(Some(esp.clone())), armed, named, TOTAL, &s, esp.first, esp.blocks), Ok(()));
    assert!(boot_esp(&Ok(data.clone()), &Ok(None), armed, named, TOTAL, &s, esp.first, esp.blocks).is_err());
    assert!(boot_esp(&Ok(data.clone()), &Ok(Some(esp.clone())), None, named, TOTAL, &s, esp.first, esp.blocks).is_err());
    assert!(boot_esp(&Ok(data.clone()), &Ok(Some(esp.clone())), armed, named, TOTAL, &s, DATEN_FIRST, esp.blocks).is_err(), "the boot volume is not the ESP");
    assert!(boot_esp(&Ok(data.clone()), &Ok(Some(esp)), armed, named, TOTAL, &s, ESP.0, 2048).is_err());
}

/// What Georg might do differently in the partition manager: a second ESP (the boot flag on the wrong partition),
/// the data partition formatted, the order swapped; every one refuses the window, the drive stays read-only.
#[test]
fn layouts_that_refuse() {
    let (data, esp) = (key(3, DATA), key(2, ESP));
    // The boot flag also set on "daten": two ESPs.
    let two = lexar(&[(GUID_EFI_SYSTEM, DATEN_FIRST, DATEN_LAST), (GUID_EFI_SYSTEM, ESP.0, ESP.1), (GUID_LINUX_DATA, DATA.0, DATA.1)]);
    assert!(matches!(validate(&data, Some(&esp), SERIAL, TOTAL, &two.scan(), None), Err(Why::Efi(_))));
    // The 385 GiB partition formatted ext4 by mistake: a file system, never erased.
    let mut fmt = georgs();
    let mut sb = vec![0u8; 4096];
    sb[0x438] = 0x53;
    sb[0x439] = 0xEF;
    fmt.put(DATA.0, &sb);
    assert_eq!(validate(&data, Some(&esp), SERIAL, TOTAL, &fmt.scan(), fmt.sig(DATA)), Err(Why::Signature("ext2/3/4")));
    // Data first, then the ESP: the keys name what the table says, and it works the same; the old order's key does not.
    let swapped = lexar(&[(GUID_LINUX_DATA, DATEN_FIRST, DATEN_LAST), (GUID_LINUX_DATA, ESP.0, ESP.0 + DATA_GIB * GIB - 1), (GUID_EFI_SYSTEM, ESP.0 + DATA_GIB * GIB, DATA.1)]);
    let s = swapped.scan();
    assert!(validate(&data, Some(&esp), SERIAL, TOTAL, &s, None).is_err());
    let (d2, e2) = (key(2, (ESP.0, ESP.0 + DATA_GIB * GIB - 1)), key(3, (ESP.0 + DATA_GIB * GIB, DATA.1)));
    assert!(validate(&d2, Some(&e2), SERIAL, TOTAL, &s, None).is_ok());
    // "daten" grown back over the new partitions: overlap.
    let over = lexar(&[(GUID_LINUX_DATA, DATEN_FIRST, DATA.0 + 10), (GUID_EFI_SYSTEM, ESP.0, ESP.1), (GUID_LINUX_DATA, DATA.0, DATA.1)]);
    assert!(matches!(validate(&data, Some(&esp), SERIAL, TOTAL, &over.scan(), None), Err(Why::Overlap(_))));
}

/// The sizes GP-DISK planned first (an 8 GiB ESP and 400 GiB of data, "daten" shrunk by 408 GiB): a key with those
/// numbers names nothing in Georg's table and is refused, never a window somewhere else.
#[test]
fn the_first_plans_keys_refuse_on_georgs_table() {
    let planned_last = DATEN_LAST_NOW - 408 * GIB;
    let esp8 = (planned_last + 1, planned_last + 8 * GIB);
    let data400 = (esp8.1 + 1, esp8.1 + 400 * GIB);
    assert_eq!(data400.1, DATEN_LAST_NOW);
    let s = georgs().scan();
    assert!(validate(&key(3, data400), Some(&key(2, esp8)), SERIAL, TOTAL, &s, None).is_err());
    assert!(validate(&key(3, DATA), Some(&key(2, esp8)), SERIAL, TOTAL, &s, None).is_err(), "an 8 GiB nvme_boot= names no ESP here");
}
