//! Host tests of the crash pieces that are pure logic: CONFIG, the record layout, the symbol table, cpio.

extern crate std;
use super::record::{self, Fnv32, Header, Marker, MarkerFound, RecordError, flags};
use super::symbols::{self, SymError, Symbols};
use super::text::{Buf, OneLine};
use super::*;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

#[test]
fn config_defaults_and_refusals() {
    assert_eq!(parse_config(None, None), (MODE_DEFAULT, 30, None));
    assert_eq!(parse_config(Some("halt"), None), (MODE_HALT, 30, None));
    assert_eq!(parse_config(Some(" reboot "), Some("5")), (MODE_REBOOT, 5, None));
    assert_eq!(parse_config(Some("reboot"), Some("0")), (MODE_REBOOT, 0, None));
    let (m, s, p) = parse_config(Some("explode"), Some("-1"));
    assert_eq!((m, s), (MODE_DEFAULT, 30));
    assert!(p.unwrap().contains("panic="), "the first problem is named");
    assert!(parse_config(None, Some("3601")).2.is_some());
    assert!(parse_config(None, Some("99999999999999999999999")).2.is_some());
    // Defaults: reboot after 30 s on hardware, halt with QEMU's exit code under a hypervisor.
    assert_eq!(decide(MODE_DEFAULT, 30, false), Action::Reboot { secs: 30 });
    assert_eq!(decide(MODE_DEFAULT, 30, true), Action::Halt { qemu_exit: true });
    assert_eq!(decide(MODE_HALT, 30, false), Action::Halt { qemu_exit: false });
    assert_eq!(decide(MODE_HALT, 30, true), Action::Halt { qemu_exit: true });
    assert_eq!(decide(MODE_REBOOT, 2, true), Action::Reboot { secs: 2 });
}

fn header() -> Header {
    Header {
        body_len: 0,
        flags: flags::EXCEPTION,
        boot_id: [7; 16],
        time_ns: 12_345_678_901,
        reload: 3,
        generation: 9,
        cpu: 5,
        cpus_asked: 15,
        cpus_answered: 14,
    }
}

#[test]
fn a_record_round_trips_and_damage_is_seen() {
    let mut page = vec![0u8; 4096];
    let body = b"panic on cpu 5: boom\n #0 ffffffff80001000 x+0x10\nlog:\n[ 1.0] I a b\n";
    record::encode(&mut page, &header(), body).unwrap();
    let (h, b) = record::decode(&page).unwrap();
    assert_eq!(h, Header { body_len: body.len() as u32, ..header() });
    assert_eq!(b, body);
    // One flipped bit anywhere in the checked bytes is a damaged record.
    for at in [20usize, 30, 63, 64, 64 + body.len() - 1] {
        let mut p = page.clone();
        p[at] ^= 1;
        assert_eq!(record::decode(&p).unwrap_err(), RecordError::BadChecksum, "byte {at}");
    }
    // Cold RAM, other versions, lengths beyond the page.
    assert_eq!(record::decode(&vec![0u8; 4096]).unwrap_err(), RecordError::NoMagic);
    let mut p = page.clone();
    p[8] = 2;
    assert_eq!(record::decode(&p).unwrap_err(), RecordError::UnknownVersion(2));
    let mut p = page.clone();
    p[12..16].copy_from_slice(&5000u32.to_le_bytes());
    assert_eq!(record::decode(&p).unwrap_err(), RecordError::BadLength);
    let mut p = page.clone();
    p[10] = 32;
    assert_eq!(record::decode(&p).unwrap_err(), RecordError::BadLength);
    assert_eq!(record::decode(&page[..10]).unwrap_err(), RecordError::NoMagic);
    // The body never reaches the marker.
    assert_eq!(record::body_capacity(4096), 4096 - 128);
    assert!(record::encode(&mut page, &header(), &vec![b'x'; 4096 - 127]).is_none());
    assert!(record::encode(&mut page, &header(), &vec![b'x'; 4096 - 128]).is_some());
    // Marking it reported keeps it valid (the boot side rewrites only the header).
    let (h, b) = record::decode(&page).unwrap();
    let marked = Header { flags: h.flags | flags::REPORTED, ..h };
    let mut f = marked.sum_start();
    f.update(b);
    page[..64].copy_from_slice(&marked.bytes(f.finish()));
    assert_eq!(record::decode(&page).unwrap().0.flags, flags::EXCEPTION | flags::REPORTED);
}

#[test]
fn fnv32_is_the_reference() {
    // FNV-1a 32 test vectors.
    let mut f = Fnv32::new();
    assert_eq!(f.finish(), 0x811c9dc5);
    f.update(b"a");
    assert_eq!(f.finish(), 0xe40c292c);
    let mut f = Fnv32::new();
    f.update(b"foobar");
    assert_eq!(f.finish(), 0xbf9cf968);
}

#[test]
fn the_boot_marker_tells_kept_absent_and_same_boot() {
    let m = Marker { boot_id: [1; 16], generation: 4, reload: 2, time_ns: 99, next_kernel: 0 };
    let b = m.bytes();
    assert_eq!(Marker::parse(&b), Some(m));
    let mut bad = b;
    bad[30] ^= 0x40;
    assert_eq!(Marker::parse(&bad), None, "decayed RAM");
    assert_eq!(Marker::parse(&[0u8; 64]), None);
    assert_eq!(Marker::parse(&b[..40]), None);
    assert_eq!(record::judge(None, &[1; 16]), MarkerFound::Absent);
    assert_eq!(record::judge(Some(&m), &[1; 16]), MarkerFound::SameBoot);
    assert_eq!(record::judge(Some(&m), &[2; 16]), MarkerFound::Kept);
}

const NM: &str = "\
ffffffff80000000 T __text_start
ffffffff80000000 T _start
ffffffff80001000 t kexec::crash::report
ffffffff80001400 T <klog::render::LineBuf as core::fmt::Write>::write_str
ffffffff80002000 r some::rodata
ffffffff80003000 W weak_fn
garbage line
ffffffff8000300 T short_but_valid
zzzzzzzzzzzzzzzz T bad_hex
ffffffff80004000 T
ffffffff90000000 T outside_text
ffffffff80005000 T rb_ksyms_anchor
";

fn table() -> Symbols {
    Symbols::parse_nm(NM.as_bytes(), &|a| (0xFFFF_FFFF_8000_0000..0xFFFF_FFFF_8040_0000).contains(&a)).unwrap()
}

#[test]
fn nm_text_becomes_a_sorted_index() {
    let t = table();
    // _start shares __text_start's address: the first name wins; rodata, garbage, outside text are skipped.
    // `ffffffff8000300` has 15 digits: 0x0fff_ffff_f800_0300, outside the kernel's code.
    assert_eq!(t.len(), 5);
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_0000), Some(("__text_start", 0)));
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_1010), Some(("kexec::crash::report", 0x10)));
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_1400), Some(("<klog::render::LineBuf as core::fmt::Write>::write_str", 0)));
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_3001), Some(("weak_fn", 1)));
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_0300), Some(("__text_start", 0x300)));
    assert_eq!(t.lookup(0xFFFF_FFFF_7FFF_FFFF), None, "below every symbol");
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_5000 + symbols::MAX_OFFSET + 1), None, "too far past the last one");
    assert_eq!(t.find("anchor"), Some(0xFFFF_FFFF_8000_5000));
    assert!(symbols::matches_build(&t, 0xFFFF_FFFF_8000_5000));
    assert!(!symbols::matches_build(&t, 0xFFFF_FFFF_8000_5001), "offset must be 0");
    assert!(!symbols::matches_build(&t, 0xFFFF_FFFF_8000_1000), "wrong name: another build");
    assert!(t.bytes() > 0);
}

#[test]
fn hostile_tables_never_panic() {
    let keep = |_a: u64| true;
    assert_eq!(Symbols::parse_nm(b"", &keep).err(), Some(SymError::Empty));
    assert_eq!(Symbols::parse_nm(b"\xff\xfe\n\n\n", &keep).err(), Some(SymError::Empty));
    // Long names are cut (at a char boundary), invalid UTF-8 names are skipped, CRLF is fine.
    let mut bytes = alloc::format!("ffffffff80000000 T {}\r\n", "é".repeat(300)).into_bytes();
    bytes.extend_from_slice(b"ffffffff80000010 T bad\xffname\n");
    bytes.extend_from_slice(b"ffffffff80000020 T ok\n");
    let t = Symbols::parse_nm(&bytes, &keep).unwrap();
    let (n, _) = t.lookup(0xFFFF_FFFF_8000_0000).unwrap();
    assert!(n.len() <= symbols::NAME_BYTES && n.chars().all(|c| c == 'é'));
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_0010).unwrap().0, n, "the invalid one was skipped");
    assert_eq!(t.lookup(0xFFFF_FFFF_8000_0020), Some(("ok", 0)));
    // Byte mutations of a real table: parse never panics.
    let base = NM.as_bytes();
    for i in 0..base.len() {
        for x in [0u8, b' ', b'\n', 0xff, b'T'] {
            let mut m = base.to_vec();
            m[i] = x;
            let _ = Symbols::parse_nm(&m, &keep);
        }
    }
}

fn newc(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut v = Vec::new();
    let pad = |v: &mut Vec<u8>| {
        while !v.len().is_multiple_of(4) {
            v.push(0)
        }
    };
    for (name, data) in entries.iter().copied().chain([("TRAILER!!!", &b""[..])]) {
        v.extend_from_slice(b"070701");
        for f in [1u32, 0o100644, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0] {
            v.extend_from_slice(alloc::format!("{f:08X}").as_bytes());
        }
        v.extend_from_slice(name.as_bytes());
        v.push(0);
        pad(&mut v);
        v.extend_from_slice(data);
        pad(&mut v);
    }
    v
}

fn find(a: &[u8], want: &str) -> Option<(u64, u64)> {
    let mut read = |off: u64, buf: &mut [u8]| {
        let (s, e) = (off as usize, off as usize + buf.len());
        if e > a.len() {
            return false;
        }
        buf.copy_from_slice(&a[s..e]);
        true
    };
    symbols::cpio_find(&mut read, a.len() as u64, want)
}

#[test]
fn the_table_is_found_in_the_initramfs() {
    let a = newc(&[("bin/hello", b"\x7fELF..."), ("boot", b""), ("boot/kernel.syms", NM.as_bytes()), ("z", b"1")]);
    let (off, len) = find(&a, "boot/kernel.syms").unwrap();
    assert_eq!(&a[off as usize..(off + len) as usize], NM.as_bytes());
    assert_eq!(find(&a, "boot/none"), None);
    assert_eq!(find(&newc(&[("./boot/kernel.syms", b"x")]), "boot/kernel.syms").map(|r| r.1), Some(1));
    // Truncated and mutated archives end the search, never panic.
    for cut in 0..a.len() {
        let _ = find(&a[..cut], "boot/kernel.syms");
    }
    for i in 0..300.min(a.len()) {
        let mut m = a.clone();
        m[i] ^= 0x5a;
        let _ = find(&m, "boot/kernel.syms");
    }
}

#[test]
fn stack_text_cuts_at_char_boundaries_and_stays_one_line() {
    let mut b = Buf::<8>::new();
    let _ = write!(b, "abcdeéé");
    assert_eq!(b.as_str(), "abcdeé");
    assert!(b.was_cut());
    let mut b = Buf::<64>::new();
    let _ = write!(OneLine(&mut b), "panicked at x.rs:1:2:\nboom\r\x1b[2J");
    assert_eq!(b.as_str(), "panicked at x.rs:1:2: boom  [2J");
}

#[test]
fn guesses_are_settled_by_the_symbol_table() {
    let t = table();
    let off = |a: u64| t.lookup(a).map(|x| x.1);
    let trap = |rip: u64| Step::Trap { vector: 2, error: 0, rip, user: false };
    let ret = 0xFFFF_FFFF_8000_1400;
    // Interrupted at a function's first or second byte (push rbp; mov rbp, rsp): the guess is a call.
    for rip in [0xFFFF_FFFF_8000_1000u64, 0xFFFF_FFFF_8000_1001] {
        let mut s = [trap(rip), Step::Guess(ret), Step::Call(ret + 8)];
        assert_eq!(settle_with(&mut s, 3, Some(&off)), 3);
        assert_eq!(s[1], Step::Call(ret));
    }
    // Interrupted mid-function (asm that pushes rbp mid-routine): the guess goes, the rest moves up.
    let mut s = [trap(0xFFFF_FFFF_8000_1010), Step::Guess(ret), Step::Call(ret + 8)];
    assert_eq!(settle_with(&mut s, 3, Some(&off)), 2);
    assert_eq!(s[..2], [trap(0xFFFF_FFFF_8000_1010), Step::Call(ret + 8)]);
    // No table, or no name for the rip: it stays a guess.
    let mut s = [trap(0xFFFF_FFFF_8000_1010), Step::Guess(ret)];
    assert_eq!(settle_with(&mut s, 2, None), 2);
    assert_eq!(s[1], Step::Guess(ret));
    let mut s = [trap(0xFFFF_FFFF_7000_0000), Step::Guess(ret)];
    assert_eq!(settle_with(&mut s, 2, Some(&off)), 2);
    assert_eq!(s[1], Step::Guess(ret));
    // n larger than the buffer is clamped, never a panic.
    let mut s = [Step::Guess(ret)];
    assert_eq!(settle_with(&mut s, 9, Some(&off)), 1);
}

#[test]
fn a_reload_hands_the_next_kernel_id_in_the_marker() {
    let old = Marker { boot_id: [1; 16], generation: 20, reload: 0, time_ns: 5, next_kernel: 0 };
    let ob = old.bytes();
    assert_eq!(&ob[44..64], &[0u8; 20], "no kernel id: the bytes an older kernel writes");
    let m = Marker { next_kernel: 0x1234_5678_9abc_def0, ..old };
    let b = m.bytes();
    assert_eq!(&b[..44], &ob[..44], "the old part is unchanged: an older kernel still reads it");
    assert_eq!(Marker::parse(&b), Some(m));
    let mut bad = b;
    bad[50] ^= 1;
    assert_eq!(Marker::parse(&bad).map(|x| x.next_kernel), Some(0), "a decayed id is no id; the marker stays");
    let mut other = b;
    other[44..48].copy_from_slice(&[0; 4]);
    assert_eq!(Marker::parse(&other).map(|x| x.next_kernel), Some(0));
    let same = MarkerFound::SameBoot;
    assert_eq!(record::handed_kernel(same, Some(&m), 1), Some(0x1234_5678_9abc_def0));
    assert_eq!(record::handed_kernel(same, Some(&m), 0), None, "a firmware start");
    assert_eq!(record::handed_kernel(same, Some(&m), 2), None, "a marker two Reloads old");
    assert_eq!(record::handed_kernel(MarkerFound::Kept, Some(&m), 1), None, "an earlier boot's");
    assert_eq!(record::handed_kernel(same, Some(&old), 1), None, "an older kernel handed nothing");
    assert_eq!(record::handed_kernel(same, None, 1), None);
}
