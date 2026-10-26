//! Reading what Linux's tools made: the tree, contents, links, holes, htree directories, both descriptor
//! layouts and block sizes 1 KiB to 4 KiB.

use super::util::*;
use crate::inode::ROOT;
use std::process::Command;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

pub const LAYOUTS: &[(&str, &[&str])] = &[
    ("default-4k", &["-b", "4096"]),
    ("1k", &["-b", "1024"]),
    ("2k-nocsum", &["-b", "2048", "-O", "^metadata_csum"]),
    ("32bit", &["-b", "4096", "-O", "^64bit,^metadata_csum,uninit_bg"]),
    ("nojournal", &["-b", "4096", "-O", "^has_journal"]),
    ("no-flex", &["-b", "4096", "-O", "^flex_bg", "-g", "8192"]),
];

fn check_tree(v: &crate::Volume) {
    let mut names = Vec::new();
    walk(v, ROOT, "", &mut names);
    for want in ["/hello.txt", "/sub/deeper/data.bin", "/fast-link", "/slow-link", "/sub/hard", "/many/entry-with-a-long-name-0599", "/sparse", "/lost+found"] {
        assert!(names.iter().any(|n| n == want), "{want} missing in {names:?}");
    }
    assert_eq!(names.iter().filter(|n| n.starts_with("/many/")).count(), 600);
    let hello = lookup(v, "hello.txt");
    let mut b = vec![0u8; 100];
    let n = v.read(hello, 0, &mut b).unwrap();
    assert_eq!(&b[..n], b"hello ext4\n");
    assert_eq!(lookup(v, "sub/hard"), hello);
    assert_eq!(v.stat(hello).unwrap().links(), 2);
    let data = lookup(v, "sub/deeper/data.bin");
    let mut d = vec![0u8; 300_000];
    assert_eq!(v.read(data, 0, &mut d).unwrap(), 300_000);
    assert!(d.iter().enumerate().all(|(i, &x)| x == (i as u32 * 31 % 251) as u8));
    let mut part = vec![0u8; 5000];
    assert_eq!(v.read(data, 123_457, &mut part).unwrap(), 5000);
    assert_eq!(&part[..], &d[123_457..128_457]);
    assert_eq!(v.readlink(lookup(v, "fast-link")).unwrap(), b"hello.txt");
    assert_eq!(v.readlink(lookup(v, "slow-link")).unwrap(), "x/".repeat(60).as_bytes());
    let sp = lookup(v, "sparse");
    let st = v.stat(sp).unwrap();
    assert_eq!(st.size(), 50 << 20);
    assert!(st.blocks512(&v.geo) < 64, "sparse file holds {} sectors", st.blocks512(&v.geo));
    let mut z = vec![1u8; 8192];
    assert_eq!(v.read(sp, 10 << 20, &mut z).unwrap(), 8192);
    assert!(z.iter().all(|&x| x == 0));
    let mut e = [0u8; 3];
    v.read(sp, (50 << 20) - 3, &mut e).unwrap();
    assert_eq!(&e, b"end");
}

#[test]
fn reads_images_of_every_layout() {
    for (name, opts) in LAYOUTS {
        let s = Scratch::new();
        let t = tree(&s);
        let img = mkfs(&s, "64M", opts, Some(&t));
        let (_d, v) = mount(img);
        assert_eq!(v.access, crate::sb::Access::ReadWrite, "{name}");
        check_tree(&v);
    }
}

#[test]
fn reads_htree_directories() {
    let s = Scratch::new();
    let t = tree(&s);
    let img = mkfs(&s, "64M", &[], Some(&t));
    // e2fsck -D indexes every directory that is large enough (htree, Linux's dx layout).
    let p = s.path("idx.img");
    std::fs::write(&p, &img).unwrap();
    let o = Command::new("e2fsck").args(["-fyD"]).arg(&p).output().unwrap();
    assert!(o.status.code().unwrap() <= 1, "{}", String::from_utf8_lossy(&o.stdout));
    let img = std::fs::read(&p).unwrap();
    assert!(debugfs(&s, &img, "htree many").contains("Root node dump"), "not indexed");
    let (_d, v) = mount(img);
    check_tree(&v);
}

#[test]
fn mount_mounts_clean_and_marks_state() {
    let s = Scratch::new();
    let img = mkfs(&s, "32M", &[], None);
    let (d, v) = mount(img);
    v.sync_fs().unwrap();
    assert_clean(&s, &d.image(), "mount + sync");
    let m = debugfs(&s, &d.image(), "stats");
    assert!(m.contains("Filesystem state:         clean"), "{m}");
}

#[test]
fn refuses_and_limits_features() {
    let s = Scratch::new();
    // casefold changes name lookups: refused; inline_data: read only.
    let img = mkfs(&s, "32M", &["-O", "casefold"], None);
    assert!(crate::Volume::mount(std::sync::Arc::new(crate::MemDev::new(img)), opts()).is_err());
    let img = mkfs(&s, "32M", &["-O", "inline_data"], None);
    let (_d, v) = mount(img);
    assert!(matches!(v.access, crate::sb::Access::ReadOnly(_)));
    assert_eq!(v.create(ROOT, b"x", 0o100644, 0, 0, crate::Kind::Reg).err(), Some(crate::errno::EROFS));
    let img = mkfs(&s, "32M", &["-O", "sparse_super2"], None);
    assert!(crate::Volume::mount(std::sync::Arc::new(crate::MemDev::new(img)), opts()).is_err());
    let _ = format!("");
}

#[test]
fn reads_inline_data() {
    let s = Scratch::new();
    let t = s.path("t");
    std::fs::create_dir_all(t.join("d")).unwrap();
    std::fs::write(t.join("small"), b"tiny file").unwrap();
    std::fs::write(t.join("d/a"), b"x").unwrap();
    std::fs::write(t.join("mid"), vec![7u8; 100]).unwrap();
    let img = mkfs(&s, "32M", &["-O", "inline_data", "-I", "256"], Some(&t));
    let (_d, v) = mount(img);
    let small = lookup(&v, "small");
    assert_ne!(v.stat(small).unwrap().flags() & crate::inode::FL_INLINE_DATA, 0);
    let mut b = vec![0u8; 64];
    let n = v.read(small, 0, &mut b).unwrap();
    assert_eq!(&b[..n], b"tiny file");
    let mid = lookup(&v, "mid");
    let mut b = vec![0u8; 200];
    assert_eq!(v.read(mid, 0, &mut b).unwrap(), 100);
    assert!(b[..100].iter().all(|&x| x == 7));
    assert_eq!(lookup(&v, "d/a") > 0, true);
}

#[test]
fn reads_xattrs() {
    let s = Scratch::new();
    let img = mkfs(&s, "32M", &[], None);
    let p = s.path("x.img");
    std::fs::write(&p, &img).unwrap();
    let f = s.path("val");
    std::fs::write(&f, vec![b'v'; 500]).unwrap();
    let cmds = format!("write /dev/null f\nea_set -f {} f user.big\nea_set f user.small hello\n", f.display());
    let c = s.path("cmds");
    std::fs::write(&c, cmds).unwrap();
    let o = Command::new("debugfs").args(["-w", "-f"]).arg(&c).arg(&p).output().unwrap();
    assert!(o.status.success());
    let img = std::fs::read(&p).unwrap();
    let (_d, v) = mount(img);
    let x = v.xattrs(lookup(&v, "f")).unwrap();
    assert!(x.iter().any(|(n, val)| n == b"user.small" && val == b"hello"), "{x:?}");
    assert!(x.iter().any(|(n, val)| n == b"user.big" && val.len() == 500), "{x:?}");
    assert_ne!(v.stat(lookup(&v, "f")).unwrap().file_acl(), 0, "the big value is in a block");
}
