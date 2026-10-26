//! Names of the BootInfo records, for the loader log line that lists what was handed over (Mini-PC check:
//! the M0 log names the BootInfo records).

use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::{BootInfo, tag};
use core::fmt::Write;

pub fn tag_name(t: u32) -> &'static str {
    match t {
        tag::MEMORY_MAP => "MEMORY_MAP",
        tag::RSDP => "RSDP",
        tag::SMBIOS => "SMBIOS",
        tag::FRAMEBUFFER => "FRAMEBUFFER",
        tag::MODE_LIST => "MODE_LIST",
        tag::CONFIG => "CONFIG",
        tag::STICK => "STICK",
        tag::LOADER_LOG => "LOADER_LOG",
        tag::TSC => "TSC",
        tag::RELOAD => "RELOAD",
        tag::MODULE => "MODULE",
        tag::CRASH_PAGE => "CRASH_PAGE",
        tag::TRAMPOLINE => "TRAMPOLINE",
        tag::RELOAD_STUB => "RELOAD_STUB",
        tag::KERNEL_IMAGE => "KERNEL_IMAGE",
        tag::GENERATION => "GENERATION",
        tag::PAGE_TABLES => "PAGE_TABLES",
        tag::BOOT_ID => "BOOT_ID",
        tag::FIRMWARE => "FIRMWARE",
        tag::HANDOVER => "HANDOVER",
        tag::EDID => "EDID",
        _ => "UNKNOWN",
    }
}

/// `HANDOVER GENERATION KERNEL_IMAGE MODULE*3 CONFIG(415 B) ...` in record order; repeated tags are counted,
/// payload sizes are shown for the variable-size records. `None` if the bytes are no valid BootInfo.
pub fn summary(bytes: &[u8]) -> Option<String> {
    let bi = BootInfo::parse(bytes).ok()?;
    let mut order: Vec<(u32, usize, usize)> = Vec::new(); // (tag, count, payload bytes)
    for r in bi.records() {
        match order.iter_mut().find(|(t, _, _)| *t == r.tag) {
            Some(e) => {
                e.1 += 1;
                e.2 += r.payload.len();
            }
            None => order.push((r.tag, 1, r.payload.len())),
        }
    }
    let mut s = String::new();
    for (t, n, bytes) in order {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(tag_name(t));
        if tag_name(t) == "UNKNOWN" {
            let _ = write!(s, "#{t}");
        }
        if n > 1 {
            let _ = write!(s, "*{n}");
        }
        if matches!(t, tag::CONFIG | tag::LOADER_LOG | tag::STICK | tag::MODE_LIST | tag::MEMORY_MAP | tag::EDID) {
            let _ = write!(s, "({bytes} B)");
        }
    }
    Some(s)
}

/// `INITRD 583780 B, rtw.bin 99 B` as the kernel's reader (`BootInfo::modules`) sees the MODULE records;
/// `none` without any. `None` if the bytes are no valid BootInfo.
pub fn modules(bytes: &[u8]) -> Option<String> {
    let bi = BootInfo::parse(bytes).ok()?;
    let mut s = String::new();
    for m in bi.modules() {
        if !s.is_empty() {
            s.push_str(", ");
        }
        let _ = write!(s, "{} {} B", String::from_utf8_lossy(m.name), m.size);
    }
    Some(if s.is_empty() { String::from("none") } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bootinfo::*;
    use std::vec;

    #[test]
    fn names_every_known_tag() {
        for t in 1..=21 {
            assert_ne!(tag_name(t), "UNKNOWN", "tag {t}");
        }
        assert_eq!(tag_name(99), "UNKNOWN");
    }

    #[test]
    fn summary_in_order_with_counts() {
        let mut buf = vec![0u8; 4096];
        let mut w = Writer::new(&mut buf).unwrap();
        w.handover(Handover::default()).unwrap();
        w.module(&Module { kind: module_kind::INITRAMFS, phys: 1, size: 2, name: b"INITRD" }).unwrap();
        w.module(&Module { kind: module_kind::FIRMWARE, phys: 1, size: 2, name: b"fw.bin" }).unwrap();
        w.config(b"a=1\n").unwrap();
        w.edid(&[0; 128]).unwrap();
        w.record(77, b"x").unwrap();
        let s = summary(&buf).unwrap();
        assert_eq!(s, "HANDOVER MODULE*2 CONFIG(4 B) EDID(128 B) UNKNOWN#77");
        assert!(summary(b"garbage").is_none());
        assert_eq!(modules(&buf).unwrap(), "INITRD 2 B, fw.bin 2 B");
        let mut empty = vec![0u8; 4096];
        Writer::new(&mut empty).unwrap().handover(Handover::default()).unwrap();
        assert_eq!(modules(&empty).unwrap(), "none");
        assert!(modules(b"garbage").is_none());
    }
}
