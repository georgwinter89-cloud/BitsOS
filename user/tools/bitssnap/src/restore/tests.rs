use super::*;

/// A fresh folder for one test (absolute: the restore tests move the working directory).
fn scratch(what: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("bitssnap-{what}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base
}

#[test]
fn only_paths_below_data() {
    assert_eq!(below_data("/data/home/1000/a.txt").unwrap(), "home/1000/a.txt");
    assert_eq!(below_data("/data/appdata//brave/").unwrap(), "appdata/brave");
    for bad in ["/data", "/data/", "/etc/passwd", "data/x", "/data/../etc", "/data/a/./b", "/database/x"] {
        assert!(below_data(bad).is_err(), "{bad}");
    }
}

#[test]
fn copy_keeps_modes_times_links() {
    let base = scratch("copy");
    let src = base.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a"), b"alpha").unwrap();
    fs::write(src.join("sub/b"), vec![7u8; 3 << 20]).unwrap();
    fs::set_permissions(src.join("a"), fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink("a", src.join("l")).unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::open(src.join("a")).unwrap().set_times(fs::FileTimes::new().set_modified(old)).unwrap();
    fs::File::open(src.join("sub")).unwrap().set_times(fs::FileTimes::new().set_modified(old)).unwrap();
    let t = (1_700_000_000, 0);
    usys::utimensat(-100, Some(src.join("l").to_str().unwrap()), Some([t, t]), 0x100).unwrap();
    let mut r = Report::default();
    copy_tree(&src, &base.join("dst"), &mut r).unwrap();
    assert_eq!((r.files, r.dirs, r.links, r.bytes), (2, 2, 1, 5 + (3 << 20)));
    let d = base.join("dst");
    assert_eq!(fs::read(d.join("a")).unwrap(), b"alpha");
    assert_eq!(fs::metadata(d.join("a")).unwrap().mode() & 0o777, 0o640);
    assert_eq!(fs::metadata(d.join("a")).unwrap().modified().unwrap(), old);
    assert_eq!(fs::metadata(d.join("sub")).unwrap().modified().unwrap(), old, "a folder's time after its content");
    assert_eq!(fs::read_link(d.join("l")).unwrap(), Path::new("a"));
    assert_eq!(fs::symlink_metadata(d.join("l")).unwrap().modified().unwrap(), old, "a symlink's own time");
    assert_eq!(fs::read(d.join("sub/b")).unwrap().len(), 3 << 20);
    // A second copy onto an existing name refuses (the restore always copies to a fresh hidden name).
    assert!(copy_tree(&src.join("a"), &d.join("a"), &mut r).is_err());
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn setuid_hard_links_holes_and_specials() {
    let base = scratch("faithful");
    let src = base.join("src");
    fs::create_dir_all(src.join("x")).unwrap();
    fs::write(src.join("suid"), b"#!").unwrap();
    fs::set_permissions(src.join("suid"), fs::Permissions::from_mode(0o4755)).unwrap();
    fs::write(src.join("one"), b"shared").unwrap();
    fs::hard_link(src.join("one"), src.join("x/two")).unwrap();
    let sparse = fs::File::create(src.join("sparse")).unwrap();
    sparse.set_len(16 << 20).unwrap();
    std::os::unix::fs::FileExt::write_at(&sparse, b"middle", 8 << 20).unwrap();
    drop(sparse);
    let _sock = std::os::unix::net::UnixListener::bind(src.join("sock")).unwrap();
    let mut r = Report::default();
    copy_tree(&src, &base.join("dst"), &mut r).unwrap();
    let d = base.join("dst");
    assert_eq!(fs::metadata(d.join("suid")).unwrap().mode() & 0o7777, 0o4755, "setuid lost");
    let (a, b) = (fs::metadata(d.join("one")).unwrap(), fs::metadata(d.join("x/two")).unwrap());
    assert_eq!((a.ino(), a.nlink(), r.hardlinks), (b.ino(), 2, 1), "hard link not kept");
    let s = fs::metadata(d.join("sparse")).unwrap();
    assert_eq!(s.len(), 16 << 20);
    assert!(s.blocks() * 512 < 1 << 20, "holes filled: {} blocks", s.blocks());
    let got = fs::read(d.join("sparse")).unwrap();
    assert_eq!(&got[8 << 20..(8 << 20) + 6], b"middle");
    assert_eq!(got.iter().filter(|&&b| b != 0).count(), 6);
    assert_eq!(r.skipped, 1);
    assert!(r.special[0].ends_with("/sock"), "{:?}", r.special);
    fs::remove_dir_all(&base).unwrap();
}

/// A snapshot folder and a live folder: `in_place` as `restore` runs it.
fn two_sides(what: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = scratch(what);
    let (snap, live) = (base.join("snap"), base.join("live"));
    fs::create_dir_all(snap.join("home/u/brave")).unwrap();
    fs::write(snap.join("home/u/brave/prefs"), b"old prefs").unwrap();
    fs::write(snap.join("home/u/brave/state"), b"old state").unwrap();
    fs::create_dir_all(live.join("home/u/brave")).unwrap();
    fs::write(live.join("home/u/brave/prefs"), b"broken").unwrap();
    fs::write(live.join("home/u/other"), b"untouched").unwrap();
    (base, snap, live)
}

fn place(live: &Path, snap: &Path, rel: &str) -> Result<Report, String> {
    let _one = one();
    let src = in_snapshot(snap, rel)?;
    let mut rep = Report::default();
    in_place(live.to_str().unwrap(), &src, rel, snap, &mut rep).map(|_| rep)
}

#[test]
fn folder_and_file_swapped_in() {
    let (base, snap, live) = two_sides("swap");
    let r = place(&live, &snap, "home/u/brave").unwrap();
    assert!(r.replaced && r.files == 2 && r.dirs == 1, "{r:?}");
    assert_eq!(fs::read(live.join("home/u/brave/prefs")).unwrap(), b"old prefs");
    assert_eq!(fs::read(live.join("home/u/brave/state")).unwrap(), b"old state");
    assert_eq!(fs::read(live.join("home/u/other")).unwrap(), b"untouched");
    // Nothing hidden left, nothing pending, the working directory back at /.
    let left: Vec<_> = fs::read_dir(live.join("home/u")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert_eq!(fs::read(live.join(PENDING)).unwrap(), b"");
    {
        let _one = one();
        assert_eq!(std::env::current_dir().unwrap(), Path::new("/"));
    }
    // A file whose parents are missing: made, then the file placed (no old one).
    fs::remove_dir_all(live.join("home")).unwrap();
    let r = place(&live, &snap, "home/u/brave/prefs").unwrap();
    assert!(!r.replaced && r.files == 1, "{r:?}");
    assert_eq!(fs::read(live.join("home/u/brave/prefs")).unwrap(), b"old prefs");
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn symlinked_parents_are_never_followed() {
    let (base, snap, live) = two_sides("links");
    let outside = base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("prefs"), b"outside").unwrap();
    // Live side: /data/home/u/brave is a link out of /data.
    fs::remove_dir_all(live.join("home/u/brave")).unwrap();
    std::os::unix::fs::symlink(&outside, live.join("home/u/brave")).unwrap();
    let e = place(&live, &snap, "home/u/brave/prefs").unwrap_err();
    assert!(e.contains("never followed"), "{e}");
    assert_eq!(fs::read(outside.join("prefs")).unwrap(), b"outside");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1, "something was made outside");
    // The last part itself may be a link: it is swapped out as a link, its target untouched.
    let r = place(&live, &snap, "home/u/brave").unwrap();
    assert!(r.replaced, "{r:?}");
    assert!(fs::symlink_metadata(live.join("home/u/brave")).unwrap().is_dir());
    assert_eq!(fs::read(outside.join("prefs")).unwrap(), b"outside");
    // Snapshot side: an absolute link in the snapshot is refused as a parent.
    std::os::unix::fs::symlink(&outside, snap.join("home/u/esc")).unwrap();
    let e = in_snapshot(&snap, "home/u/esc/prefs").unwrap_err();
    assert!(e.contains("never followed"), "{e}");
    assert!(in_snapshot(&snap, "home/u/esc").is_ok(), "the last part may be a link");
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn long_names_and_left_overs() {
    let (base, snap, live) = two_sides("names");
    let long = "n".repeat(250);
    fs::write(snap.join("home/u").join(&long), b"long").unwrap();
    place(&live, &snap, &format!("home/u/{long}")).unwrap();
    assert_eq!(fs::read(live.join("home/u").join(&long)).unwrap(), b"long");
    // A restore cut off: its copy and its line remain; the next restore removes them.
    let cut = live.join("home/u").join(tmp_name("brave"));
    fs::create_dir_all(cut.join("deep")).unwrap();
    fs::write(cut.join("deep/big"), vec![1u8; 1 << 16]).unwrap();
    // A pid no process has (above the kernel's pid_max).
    fs::write(live.join(PENDING), format!("{} {}\n", 1u32 << 30, hex(&format!("home/u/{}", tmp_name("brave"))))).unwrap();
    {
        let _one = one();
        let gone = sweep_locked(live.to_str().unwrap()).unwrap();
        assert_eq!(gone.len(), 1, "{gone:?}");
    }
    assert!(fs::symlink_metadata(&cut).is_err(), "the cut-off copy stays");
    assert_eq!(fs::read(live.join(PENDING)).unwrap(), b"");
    // A live one (this test's parent process) is kept, and blocks a restore of the same target.
    let other = std::os::unix::process::parent_id();
    fs::write(live.join(PENDING), format!("{other} {}\n", hex(&format!("home/u/{}", tmp_name("brave"))))).unwrap();
    let e = place(&live, &snap, "home/u/brave").unwrap_err();
    assert!(e.contains("another restore"), "{e}");
    assert_eq!(fs::read(live.join("home/u/brave/prefs")).unwrap(), b"broken");
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn pending_lines_survive_odd_names() {
    for s in ["a/b", "with space", "new\nline", ""] {
        assert_eq!(unhex(&hex(s)).as_deref(), Some(s));
    }
    assert_eq!(unhex("zz"), None);
    assert_eq!(unhex("abc"), None);
    assert!(tmp_name(&"x".repeat(255)).len() < 40);
}
