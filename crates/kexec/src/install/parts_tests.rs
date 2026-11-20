//! UPD-4 host tests of the parts' rules as the kernel applies them: hostile names and paths (escapes, links'
//! names, FAT's case folding, a file where a folder is), sizes that overflow, and the one written form of a
//! slot's FILES and a part's record.

extern crate std;

use super::parts_rules::{self as rules, List};
use super::update_node::{Command, parse};
use alloc::string::{String, ToString};
use alloc::vec;
use std::format;

const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn part_names_never_reach_the_mediums_own_folders() {
    for good in ["bin", "mesa", "lo-gly", "vkquake", "firmware", "x_1", "9070"] {
        assert!(rules::name_ok(good).is_ok(), "{good}");
    }
    let long = "a".repeat(rules::NAME_MAX + 1);
    for bad in ["", "sys", "settings", "update", "mcp", "home", "crash", "efi", "SYS", "Mesa", "config.txt", "stick.id", "data.img", "a.b", "-x", "_x", "a/b", "..", ".", "settin~1", "brävé", "a b", long.as_str()] {
        assert!(rules::name_ok(bad).is_err(), "{bad:?}");
    }
    assert_eq!(rules::folder("mesa", H), "mesa.0123456789abcdef");
    assert_eq!(rules::record_name("mesa", H), "mesa.0123456789abcdef.PART");
    assert_eq!(rules::new_folder("mesa", H), "mesa.0123456789abcdef.NEW");
    assert!(rules::hex_ok(H) && !rules::hex_ok(&H.to_uppercase()) && !rules::hex_ok(&H[1..]) && !rules::hex_ok(&format!("{}g", &H[1..])));
}

#[test]
fn a_path_stays_inside_its_part() {
    for good in ["a", "lib/libGL.so.1", "share/fonts/Deja Vu Sans.ttf", "x/y/z/w.v", "libstdc++.so.6", "ä/ö.txt"] {
        assert!(rules::path_ok(good, false).is_ok(), "{good}");
    }
    let comp = "c".repeat(256);
    let deep = "d/".repeat(2100) + "f";
    for bad in [
        "", "../x", "a/../b", "a/..", "./a", "/abs", "a//b", "a/", "a\\b", "..\\..\\SYS\\KERNEL", "C:x", "a/b.", "a/b ", "a/\u{1}b", "a\u{7f}", "a/*", "a?",
        "a|b", "a\"b", "<a>", comp.as_str(), deep.as_str(),
    ] {
        assert!(rules::path_ok(bad, false).is_err(), "{bad:?}");
    }
    assert!(rules::path_ok("bitscomp", true).is_ok());
    assert!(rules::path_ok("sub/bitscomp", true).is_err(), "the bin part has no folders");
}

#[test]
fn a_list_takes_each_name_once_in_order() {
    let mut l = List::default();
    l.add("a.txt", false, 1).unwrap();
    l.add("b/c", false, 2).unwrap();
    l.add("b/d", false, 3).unwrap();
    assert_eq!((l.count, l.bytes, l.dirs()), (3, 6, 1));
    assert!(l.add("b/c", false, 1).is_err(), "twice");
    assert!(l.add("a.txt", false, 1).is_err(), "out of order");
    // FAT ignores case: LIB/x and lib/x would be one file.
    let mut l = List::default();
    l.add("LIB/x", false, 1).unwrap();
    assert!(l.add("lib/x", false, 1).is_err());
    // Only ASCII letters fold, as in fs-fat (name.rs `fold`) and Linux vfat with utf8 (nls_utf8's identity tables):
    // "É" and "é" are two names there, so they are two here.
    let mut l = List::default();
    l.add("\u{c9}", false, 1).unwrap();
    l.add("\u{e9}", false, 1).unwrap();
    assert_eq!(l.count, 2);
    // A file where a folder is, both ways round.
    let mut l = List::default();
    l.add("A/b", false, 1).unwrap();
    assert!(l.add("a", false, 1).is_err(), "a file named like a folder");
    let mut l = List::default();
    l.add("X", false, 1).unwrap();
    assert!(l.add("x/y", false, 1).is_err(), "a folder named like a file");
    // Sizes that would overflow the sum.
    let mut l = List::default();
    l.add("a", false, u64::MAX).unwrap();
    assert!(l.add("b", false, 1).is_err());
    assert_eq!(rules::line(H, 5, "a b"), format!("{H} 5 a b\n"));
}

#[test]
fn the_space_bound_covers_every_cluster() {
    // 3 files of 1 byte in 1 folder on 4 KiB clusters: 3 clusters of data at least.
    let need = rules::on_medium(3, 1, 3, 4096);
    assert!(need >= 3 * 4096 + 4096, "{need}");
    assert!(rules::on_medium(10, 2, 100 << 20, 32768) > 100 << 20);
    assert_eq!(rules::on_medium(u64::MAX, u64::MAX, u64::MAX, 4096), u64::MAX, "huge sizes saturate, never wrap");
}

#[test]
fn files_and_records_have_one_written_form() {
    let parts = vec![("bin".to_string(), H.to_string()), ("mesa".to_string(), H.replace('0', "f"))];
    let t = rules::files_render(&parts);
    assert_eq!(rules::files_parse(t.as_bytes()), Ok(parts.clone()));
    let mesa = H.replace('0', "f");
    for bad in [
        t.replace("bitsos-files 1", "bitsos-files 2"),
        t.replace("part mesa", "part Mesa"),
        t.replace("part mesa", "part sys"),
        format!("bitsos-files 1\npart mesa {mesa}\npart bin {H}\n"),
        format!("bitsos-files 1\npart bin {H}\npart bin {H}\n"),
        t.replace(H, &H[..63]),
        t.replace('\n', "\r\n"),
        format!("{t}extra\n"),
        t.trim_end().to_string(),
    ] {
        assert!(rules::files_parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
    assert_eq!(rules::files_parse(b"bitsos-files 1\n"), Ok(vec![]));
    let r = rules::record_render("mesa", H, 12, 3456);
    assert_eq!(rules::record_parse(r.as_bytes()), Ok(("mesa".to_string(), H.to_string(), 12, 3456)));
    for bad in [r.replace("files 12", "files 012"), r.replace("name mesa", "name ../x"), r.replace(H, "x"), String::from("bitsos-part 1\n")] {
        assert!(rules::record_parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
}

#[test]
fn the_parts_commands_of_the_node() {
    assert_eq!(parse("plan 169"), Ok(Command::Plan(169)));
    assert_eq!(parse(&format!("part mesa {H} 3 1 300")), Ok(Command::Part { name: "mesa".into(), hash: H.into(), files: 3, dirs: 1, bytes: 300 }));
    assert_eq!(parse("file 12 share/a b.txt"), Ok(Command::File { size: 12, path: "share/a b.txt".into() }));
    assert_eq!(parse("seal"), Ok(Command::Seal));
    assert_eq!(parse(&format!("use bin {H}")), Ok(Command::Use { name: "bin".into(), hash: H.into() }));
    for bad in ["plan 0", "plan x", "part mesa", "part mesa h 1 1", "file x a", "file 12", "use bin"] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}
