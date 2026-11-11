//! The boot volume's pure rules (host-tested): STICK.ID's text, the write decision over its facts (`stick.rs` gathers
//! them, the module text there lists the rules) and the source name /proc/mounts shows.

use alloc::string::String;
use alloc::vec::Vec;

/// `\BITSOS2\STICK.ID`'s text as 16 bytes (bitsboot's rule: 32 hex digits, white space around ignored).
pub fn parse_stick_id(text: &[u8]) -> Option<[u8; 16]> {
    let t = core::str::from_utf8(text).ok()?.trim();
    if t.len() != 32 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(id)
}

/// What the write decision looks at.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub loader_id: Option<[u8; 16]>,
    pub volume_id: Option<[u8; 16]>,
    pub label: Vec<u8>,
    /// USB disks recognised as the stick, and USB disks that might be it.
    pub sticks: usize,
    pub doubts: usize,
    pub device_read_only: bool,
    pub table_clean: bool,
    pub layout_ok: bool,
    /// The volume is an NVMe partition: `gpt::window::boot_esp`'s answer (it stands for `table_clean` there).
    pub nvme: Option<Result<(), &'static str>>,
}

/// Ok: the volume may be written (below `\BITSOS2\`); Err: why not (pure; host-tested).
pub fn verdict(f: &Facts) -> Result<(), &'static str> {
    let Some(want) = f.loader_id else { return Err("bitsboot handed over no \\BITSOS2\\STICK.ID") };
    match f.volume_id {
        None => return Err("the volume has no readable \\BITSOS2\\STICK.ID"),
        Some(id) if id != want => return Err("the volume's STICK.ID differs from the boot stick's"),
        _ => {}
    }
    if !f.label.eq_ignore_ascii_case(b"BITSOS") {
        return Err("the volume label is not BITSOS");
    }
    if f.sticks != 1 {
        return Err("more than one disk looks like the boot volume");
    }
    if f.doubts != 0 {
        return Err("a disk could not be told apart from the boot volume");
    }
    if f.device_read_only {
        return Err("the boot device is write-protected or refuses writes");
    }
    match f.nvme {
        Some(rule) => rule?,
        None if !f.table_clean => return Err("its partition table is protected (R8a)"),
        None => {}
    }
    if !f.layout_ok {
        return Err("the volume does not keep two mirrored FATs");
    }
    Ok(())
}

/// `/dev/<disk>p<n>` for the partition starting at `start` (n is Linux's partition number from the scan: the
/// table slot, logical partitions from 5; Linux's `disk_name` puts a `p` after a disk name ending in a digit),
/// `/dev/<disk>` for a stick without a table.
pub fn source_name(disk: &str, start: u64, parts: &[(u64, u64, String, u32)]) -> String {
    match parts.iter().find(|p| p.0 == start) {
        Some(p) if start != 0 => {
            let sep = if disk.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
            alloc::format!("/dev/{disk}{sep}{}", p.3)
        }
        _ => alloc::format!("/dev/{disk}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Facts {
        Facts { loader_id: Some([7; 16]), volume_id: Some([7; 16]), label: b"BITSOS".to_vec(), sticks: 1, doubts: 0, device_read_only: false, table_clean: true, layout_ok: true, nvme: None }
    }

    #[test]
    fn source_names_like_linux() {
        // Slot 2 is empty: the second partition is number 3, its slot, not its position (sdb3, not sdb2).
        let parts = alloc::vec![(2048u64, 100u64, String::from("FAT32"), 1u32), (4096, 100, String::from("Linux"), 3)];
        assert_eq!(source_name("msd0", 2048, &parts), "/dev/msd0p1");
        assert_eq!(source_name("msd0", 4096, &parts), "/dev/msd0p3");
        assert_eq!(source_name("sdb", 4096, &parts), "/dev/sdb3");
        assert_eq!(source_name("msd1", 0, &[]), "/dev/msd1");
    }

    #[test]
    fn identity_rules() {
        assert_eq!(verdict(&good()), Ok(()));
        let cases: Vec<(Facts, &str)> = alloc::vec![
            (Facts { loader_id: None, ..good() }, "bitsboot handed over no \\BITSOS2\\STICK.ID"),
            (Facts { volume_id: None, ..good() }, "the volume has no readable \\BITSOS2\\STICK.ID"),
            (Facts { volume_id: Some([8; 16]), ..good() }, "the volume's STICK.ID differs from the boot stick's"),
            (Facts { label: b"ZORINOS".to_vec(), ..good() }, "the volume label is not BITSOS"),
            (Facts { sticks: 2, ..good() }, "more than one disk looks like the boot volume"),
            (Facts { doubts: 1, ..good() }, "a disk could not be told apart from the boot volume"),
            (Facts { device_read_only: true, ..good() }, "the boot device is write-protected or refuses writes"),
            (Facts { table_clean: false, ..good() }, "its partition table is protected (R8a)"),
            (Facts { layout_ok: false, ..good() }, "the volume does not keep two mirrored FATs"),
        ];
        for (f, why) in cases {
            assert_eq!(verdict(&f), Err(why));
        }
        assert_eq!(verdict(&Facts { label: b"bitsos".to_vec(), ..good() }), Ok(()));
    }

    /// The Lexar's ESP as /boot (6.11.2026): the window rule stands for the table rule, every identity rule stays.
    #[test]
    fn identity_rules_on_the_nvme() {
        let nvme = |rule| Facts { nvme: Some(rule), table_clean: false, ..good() };
        assert_eq!(verdict(&nvme(Ok(()))), Ok(()), "a Windows disk is fine when the window rule passes");
        assert_eq!(verdict(&nvme(Err("CONFIG.TXT has no nvme_boot= line naming the boot partition"))), Err("CONFIG.TXT has no nvme_boot= line naming the boot partition"));
        let cases: Vec<(Facts, &str)> = alloc::vec![
            (Facts { volume_id: Some([8; 16]), ..nvme(Ok(())) }, "the volume's STICK.ID differs from the boot stick's"),
            (Facts { volume_id: None, ..nvme(Ok(())) }, "the volume has no readable \\BITSOS2\\STICK.ID"),
            (Facts { label: b"NO NAME".to_vec(), ..nvme(Ok(())) }, "the volume label is not BITSOS"),
            (Facts { layout_ok: false, ..nvme(Ok(())) }, "the volume does not keep two mirrored FATs"),
            (Facts { sticks: 2, ..nvme(Ok(())) }, "more than one disk looks like the boot volume"),
            (Facts { doubts: 1, ..nvme(Ok(())) }, "a disk could not be told apart from the boot volume"),
            (Facts { device_read_only: true, ..nvme(Ok(())) }, "the boot device is write-protected or refuses writes"),
            (Facts { loader_id: None, ..nvme(Ok(())) }, "bitsboot handed over no \\BITSOS2\\STICK.ID"),
        ];
        for (f, why) in cases {
            assert_eq!(verdict(&f), Err(why));
        }
    }

    #[test]
    fn stick_id_text() {
        assert_eq!(parse_stick_id(b"00112233445566778899aabbccddeeff\r\n").unwrap()[15], 0xff);
        assert!(parse_stick_id(b"0011").is_none());
        assert!(parse_stick_id(b"zz112233445566778899aabbccddeeff").is_none());
    }
}
