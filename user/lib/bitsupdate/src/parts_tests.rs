//! UPD-4: the form-2 manifest and the install plan against hostile releases: path escapes and links in a tree,
//! names of the medium's own folders, huge sizes, entries named twice (FAT's case folding counted), a tree with a
//! new hash under an old name, pieces out of order, and a signature over a different list. Every refusal comes
//! before a byte is written; the space check before a byte is downloaded.

use crate::manifest::{ASSET_MAX, Entry, Manifest, NOTES, Tree, hex, piece_asset};
use crate::parts::{self, Room};
use crate::parts_rules as rules;
use crate::verify::{self, Why};
use crate::{partsio, sig, tar};
use std::path::PathBuf;

const KERNEL: [u8; 5000] = [7u8; 5000];
const INITRD: [u8; 3000] = [9u8; 3000];

fn entry(name: &str, b: &[u8]) -> Entry {
    Entry { name: name.into(), size: b.len() as u64, sha256: verify::sha256(b) }
}

fn tree(name: &str, hash: &str, pieces: usize) -> Tree {
    Tree {
        name: name.into(),
        hash: hash.into(),
        files: 3,
        dirs: 1,
        bytes: 300,
        pieces: (1..=pieces).map(|k| Entry { name: piece_asset(name, hash, k), size: 1000, sha256: [k as u8; 32] }).collect(),
    }
}

fn release() -> Manifest {
    Manifest {
        version: 169,
        date: "2026-11-17".into(),
        min_previous: None,
        files: vec![entry("KERNEL", &KERNEL), entry("INITRD", &INITRD), entry(NOTES, b"neu!\n")],
        bins: vec![entry("bitscomp", b"ver 169"), entry("brave.desktop", b"[Desktop Entry]\n")],
        trees: vec![tree("brave", &"b".repeat(64), 1), tree("mesa", &"a".repeat(64), 2)],
    }
}

#[test]
fn form_2_has_one_written_form() {
    let m = release();
    let t = m.render();
    assert!(t.starts_with("bitsos-update 2\nversion 169\n"), "{t}");
    assert!(t.contains(&format!("tree mesa {} 3 1 300 2\npiece mesa 1 1000 ", "a".repeat(64))), "{t}");
    assert_eq!(Manifest::parse(t.as_bytes()), Ok(m.clone()));
    assert_eq!(m.trees[1].pieces[1].name, format!("tree.mesa.{}.2.tar.zst", "a".repeat(16)));
    // Form 1 stays what the versions before UPD-4 read.
    let v1 = Manifest { bins: vec![], trees: vec![], ..m };
    assert!(v1.render().starts_with("bitsos-update 1\n"));
    assert_eq!(Manifest::parse(v1.render().as_bytes()), Ok(v1));
}

#[test]
fn hostile_manifests_are_refused() {
    let t = release().render();
    let a64 = "a".repeat(64);
    let b64 = "b".repeat(64);
    let bin = |name: &str| format!("bin {name} 7 {}\n", hex(&verify::sha256(b"ver 169")));
    let comp = bin("bitscomp");
    let bad: Vec<String> = vec![
        // Path escapes and the medium's own names as bin files or trees.
        t.replace(&comp, &bin("../KERNEL")),
        t.replace(&comp, &bin("sub/x")),
        t.replace(&comp, &bin("..")),
        t.replace(&comp, &bin(".hidden")),
        t.replace(&comp, &bin("a\\b")),
        t.replace("tree brave", "tree settings").replace("piece brave", "piece settings"),
        t.replace("tree brave", "tree sys").replace("piece brave", "piece sys"),
        t.replace("tree brave", "tree bin").replace("piece brave", "piece bin"),
        t.replace("tree brave", "tree ../x").replace("piece brave", "piece ../x"),
        t.replace("tree brave", "tree Brave").replace("piece brave", "piece Brave"),
        // Duplicates, FAT's case folding counted; out of order.
        t.replace(&comp, &format!("{comp}{comp}")),
        t.replace(&comp, &format!("{}{comp}", bin("Bitscomp"))),
        t.replace(&format!("tree mesa {a64}"), &format!("tree brave {a64}")).replace("piece mesa", "piece brave"),
        // Huge sizes: a piece above GitHub's 2 GiB, a size that is no number, empty trees and pieces.
        t.replace("piece mesa 1 1000", &format!("piece mesa 1 {}", ASSET_MAX + 1)),
        t.replace("piece mesa 1 1000", "piece mesa 1 99999999999999"),
        t.replace("piece mesa 1 1000", "piece mesa 1 0"),
        t.replace(&format!("{b64} 3 1 300 1"), &format!("{b64} 0 1 300 1")),
        t.replace(&format!("{a64} 3 1 300 2"), &format!("{a64} 3 1 300 0")),
        // Pieces out of order, too few, too many, away from their tree.
        t.replace("piece mesa 1 ", "piece mesa 3 "),
        t.replace(&format!("{a64} 3 1 300 2"), &format!("{a64} 3 1 300 3")),
        t.replace(&format!("{a64} 3 1 300 2"), &format!("{a64} 3 1 300 1")),
        t.replace("piece brave 1", "piece mesa 1"),
        // A bad hash, a bin line after the trees, a file line among the parts, form 2 without bin.
        t.replace(&a64, &a64[1..]),
        format!("{t}{comp}"),
        format!("{t}file BOOTX64.EFI 1 {a64}\n"),
        t.lines().filter(|l| !l.starts_with("bin ")).map(|l| format!("{l}\n")).collect(),
    ];
    for b in bad {
        assert_ne!(b, t);
        assert!(Manifest::parse(b.as_bytes()).is_err(), "{b}");
    }
    // Form 1 may not carry parts.
    assert!(Manifest::parse(t.replacen("bitsos-update 2", "bitsos-update 1", 1).as_bytes()).is_err());
}

#[test]
fn a_signature_over_a_different_list_is_refused() {
    let (key, public) = sig::keygen().unwrap();
    let text = release().render().into_bytes();
    let s = sig::sign(&key, &text).unwrap().into_bytes();
    assert!(verify::manifest(Some(&public), &text, &s, 169, 168, 168).is_ok());
    let t = String::from_utf8(text.clone()).unwrap();
    // Another bin file, another tree hash, one piece's hash: the same signature fits none.
    for changed in [t.replace("bin bitscomp 7", "bin bitscomp 8"), t.replace(&"a".repeat(64), &"c".repeat(64)), t.replacen(&hex(&[1u8; 32]), &hex(&[9u8; 32]), 1)] {
        assert_ne!(changed, t);
        assert_eq!(verify::manifest(Some(&public), changed.as_bytes(), &s, 169, 168, 168), Err(Why::Signature));
    }
}

fn room(stored: &[(&str, &str)], free: u64) -> Room {
    Room { free, cluster: 4096, stored: stored.iter().map(|(n, h)| (n.to_string(), h.to_string())).collect(), ..Room::default() }
}

#[test]
fn the_plan_fetches_what_the_medium_lacks() {
    let m = release();
    let bin_hash = parts::bin_hash(&m.bins);
    let p = parts::plan(&m, &room(&[("brave", &"b".repeat(64)), ("bin", &bin_hash)], 1 << 40));
    assert_eq!(p.parts.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["bin", "brave", "mesa"]);
    assert_eq!(p.present, [true, true, false]);
    assert_eq!(p.download(), 5000 + 3000 + 2000, "the bundle and mesa's two pieces");
    // A tree with a new hash under an old name: mesa aaaa... is not mesa dddd..., it comes again, into its own folder.
    let old = parts::plan(&m, &room(&[("mesa", &"d".repeat(64))], 1 << 40));
    assert_eq!(old.present, [false, false, false]);
    assert_ne!(rules::folder("mesa", &"d".repeat(64)), rules::folder("mesa", &"a".repeat(64)));
    assert_eq!(crate::text::comes(&p), "Kern 1 MB, Schreibtisch unverändert, Programme 1 MB (Grafiktreiber), 1 Programm unverändert.");
}

#[test]
fn the_space_check_comes_before_any_download() {
    let m = release();
    let r = room(&[], 0);
    let p = parts::plan(&m, &r);
    let need = parts::need(&p, &r);
    assert!(need > 8000 + 300 * 3, "{need}");
    assert_eq!(parts::fits(&p, &Room { free: need, ..r.clone() }), Ok(()));
    assert_eq!(parts::fits(&p, &Room { free: need - 1, ..r.clone() }), Err(1));
    // Parts no slot uses count (they go first); what the install prunes counts for the bundle only.
    assert_eq!(parts::fits(&p, &Room { free: need - 100, orphans: 100, ..r.clone() }), Ok(()));
    let parts_only = p.parts_on_medium(4096);
    assert!(parts::fits(&p, &Room { free: parts_only - 1, freed: 1 << 30, ..r.clone() }).is_err(), "the parts are written before the pruning");
    assert_eq!(parts::fits(&p, &Room { free: parts_only, freed: 1 << 30, ..r.clone() }), Ok(()));
    // Huge sizes saturate instead of wrapping into "fits".
    let mut huge = release();
    huge.trees[0].bytes = u64::MAX;
    let hp = parts::plan(&huge, &r);
    assert!(parts::fits(&hp, &Room { free: u64::MAX / 2, ..r }).is_err());
}

/// A piece as the desk packs it: tar entries in byte order, zstd.
fn piece(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = tar::Writer::new(Vec::new());
    for (p, b) in entries {
        w.file(p, 0o644, b.len() as u64, *b).unwrap();
    }
    ruzstd::encoding::compress_to_vec(&w.finish().unwrap()[..], ruzstd::encoding::CompressionLevel::Fastest)
}

fn tmp(name: &str, b: &[u8]) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitsupdate-parts-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    std::fs::write(&p, b).unwrap();
    p
}

/// A tar header of a symbolic link `name` -> `to` (FAT has none; the reader refuses it).
fn link_header(name: &str, to: &str) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    for (at, v) in [(100, "0000644\0"), (108, "0000000\0"), (116, "0000000\0"), (136, "0000000\0")] {
        h[at..at + 8].copy_from_slice(v.as_bytes());
    }
    h[124..136].copy_from_slice(b"00000000000\0");
    h[156] = b'2';
    h[157..157 + to.len()].copy_from_slice(to.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

#[test]
fn a_tree_is_hashed_from_its_files() {
    let one = tmp("t1", &piece(&[("lib/a.so", b"aaa"), ("lib/b.so", b"bb")]));
    let two = tmp("t2", &piece(&[("share/x y.txt", b"x")]));
    let (hash, files, dirs, bytes) = partsio::scan(&[one.clone(), two]).unwrap();
    assert_eq!((files, dirs, bytes), (3, 2, 6));
    let mut want = String::new();
    for (p, b) in [("lib/a.so", &b"aaa"[..]), ("lib/b.so", b"bb"), ("share/x y.txt", b"x")] {
        want.push_str(&rules::line(&hex(&verify::sha256(b)), b.len() as u64, p));
    }
    assert_eq!(hash, hex(&verify::sha256(want.as_bytes())));
    // The same files in one piece: the same tree.
    let whole = tmp("t3", &piece(&[("lib/a.so", b"aaa"), ("lib/b.so", b"bb"), ("share/x y.txt", b"x")]));
    assert_eq!(partsio::scan(&[whole]).unwrap().0, hash);
    // Out of order, twice by FAT's rules, a file where a folder is: refused.
    let bad: [&[(&str, &[u8])]; 3] = [&[("b", b"1"), ("a", b"2")], &[("Lib/a", b"1"), ("lib/a", b"2")], &[("X", b"1"), ("x/y", b"2")]];
    for (i, b) in bad.iter().enumerate() {
        assert!(partsio::scan(&[tmp(&format!("bad{i}"), &piece(b))]).is_err(), "{b:?}");
    }
    // An escaping name and a link: the tar reader refuses both before anything is handed on.
    let mut esc = tar::Writer::new(Vec::new());
    esc.file("ok", 0o644, 1, &b"1"[..]).unwrap();
    let mut raw = esc.finish().unwrap();
    let end = raw.len() - 1024;
    raw.splice(end..end, link_header("evil", "../../SYS/OK"));
    let lp = tmp("link", &ruzstd::encoding::compress_to_vec(&raw[..], ruzstd::encoding::CompressionLevel::Fastest));
    assert!(partsio::scan(&[lp]).is_err_and(|e| e.contains("link")), "a link is refused");
    let mut raw = tar::Writer::new(Vec::new()).finish().unwrap();
    let mut h = link_header("../KERNEL", "");
    h[156] = b'0';
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    raw.splice(0..0, h);
    let ep = tmp("escape", &ruzstd::encoding::compress_to_vec(&raw[..], ruzstd::encoding::CompressionLevel::Fastest));
    assert!(partsio::scan(&[ep]).is_err_and(|e| e.contains("unsafe")), "a path escape is refused");
    // A piece cut short or not zstd at all.
    assert!(partsio::scan(&[tmp("cut", &piece(&[("a", b"123456")])[..20])]).is_err());
    assert!(partsio::scan(&[tmp("junk", b"no zstd here")]).is_err());
    let _ = std::fs::remove_dir_all(one.parent().unwrap());
}

#[test]
fn a_downloaded_asset_is_checked_on_disk() {
    let p = tmp("asset", b"hello");
    assert!(partsio::check_file(&entry("a", b"hello"), &p).is_ok());
    assert!(partsio::check_file(&entry("a", b"hellO"), &p).is_err());
    assert!(partsio::check_file(&entry("a", b"hello!"), &p).is_err());
}

#[test]
fn a_link_in_the_work_folder_is_never_followed() {
    let p = tmp("plain", b"hello");
    let l = p.with_file_name("link");
    let _ = std::fs::remove_file(&l);
    std::os::unix::fs::symlink(&p, &l).unwrap();
    assert!(partsio::check_file(&entry("a", b"hello"), &p).is_ok());
    assert!(partsio::check_file(&entry("a", b"hello"), &l).is_err(), "root followed a link the fetch user could have put there");
    assert!(partsio::open_plain(p.parent().unwrap()).is_err(), "a folder is no plain file");
}

#[test]
fn a_release_without_its_parts_is_refused() {
    use crate::flow::{Fail, withheld};
    let none = || None;
    let unused = || Some(Room::default());
    let used = || Some(Room { in_use: true, ..Room::default() });
    let kernel_only = ["MANIFEST", "MANIFEST.SIG", "KERNEL", "INITRD", "BOOTX64.EFI"];
    assert_eq!(withheld(&kernel_only, &none), Ok(()), "a medium without parts takes a kernel-only release (169 on 168)");
    assert_eq!(withheld(&kernel_only, &unused), Ok(()));
    assert_eq!(withheld(&kernel_only, &used), Err(Fail::Check(Why::PartsWithheld)), "a medium that runs with parts already");
    for left in ["MANIFEST2.SIG", "bin.bitscomp", "tree.mesa.0123456789abcdef.1.tar.zst"] {
        let mut a = kernel_only.to_vec();
        a.push(left);
        assert_eq!(withheld(&a, &none), Err(Fail::Check(Why::PartsWithheld)), "MANIFEST2 taken out, {left} left");
    }
    // The node's plan answer: a part some slot names.
    let j = |by: &str| mcp_wire::json::Json::parse(&format!("{{\"ok\":true,\"free\":1,\"cluster\":4096,\"parts\":[{{\"name\":\"bin\",\"hash\":\"{}\",\"used_by\":[{by}]}}]}}", "a".repeat(64))).unwrap();
    assert!(parts::room(&j("170")).unwrap().in_use);
    assert!(!parts::room(&j("")).unwrap().in_use);
}

#[test]
fn work_folders_of_gone_processes_are_swept() {
    let d = std::env::temp_dir().join(format!("bitsupdate-sweep-{}", std::process::id()));
    let me = std::process::id();
    for n in ["dl-1-0", "dl-2-3", &format!("dl-{me}-0"), "dl-x-1", "other"] {
        std::fs::create_dir_all(d.join(n)).unwrap();
    }
    crate::flow::sweep(&d, &|pid| pid == 2);
    let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    let mut want = vec!["dl-2-3".to_string(), format!("dl-{me}-0"), "dl-x-1".into(), "other".into()];
    want.sort();
    assert_eq!(left, want, "only the folder of the gone process 1 goes");
    let _ = std::fs::remove_dir_all(&d);
}
