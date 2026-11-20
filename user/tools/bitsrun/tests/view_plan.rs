//! PK-3 host tests: the view plan (which binds in which order) from fake trees and systems, and its bwrap form.

use bitspkg::perm::{Permission, Want};
use bitsrun::bwrap;
use bitsrun::plan::{self, Input, Kind, Op, System, Tree};
use std::collections::BTreeMap;

/// A tree as a map: path -> kind (links with their target).
#[derive(Default)]
struct Fake(BTreeMap<String, (Kind, String)>);

impl Fake {
    fn new(entries: &[(&str, Kind)]) -> Fake {
        let mut f = Fake::default();
        for (p, k) in entries {
            f.add(p, *k, "");
        }
        f
    }
    fn add(&mut self, p: &str, k: Kind, target: &str) {
        let parts: Vec<&str> = p.split('/').collect();
        for n in 1..parts.len() {
            self.0.entry(parts[..n].join("/")).or_insert((Kind::Dir, String::new()));
        }
        self.0.insert(p.to_string(), (k, target.to_string()));
    }
}

impl Tree for Fake {
    fn kind(&self, rel: &str) -> Kind {
        if rel.is_empty() {
            return Kind::Dir;
        }
        self.0.get(rel).map(|e| e.0).unwrap_or(Kind::Missing)
    }
    fn list(&self, rel: &str) -> Vec<String> {
        let pre = if rel.is_empty() { String::new() } else { format!("{rel}/") };
        self.0.keys().filter_map(|k| k.strip_prefix(&pre).filter(|r| !r.is_empty() && !r.contains('/')).map(String::from)).collect()
    }
    fn link(&self, rel: &str) -> Option<String> {
        self.0.get(rel).filter(|e| e.0 == Kind::Link).map(|e| e.1.clone())
    }
}

const T: &str = "/apps/probe/1/root";

fn sys_all() -> System {
    System {
        etc: plan::SYSTEM_ETC.iter().map(|n| (n.to_string(), format!("/etc/{n}"))).collect(),
        runtime_dir: Some("/run/user/1000".into()),
        x11: None,
        sys: true,
        devices: vec![("gpu".into(), "/dev/dri".into())],
        user_folders: vec!["Downloads".into(), "Documents".into()],
        fonts: Some(plan::SYSTEM_FONTS.into()),
    }
}

fn input<'a>(tree: &'a Fake, system: System, perms: &'a [Permission], env: &[(&str, &str)]) -> Input<'a> {
    Input {
        tree,
        tree_dir: T.into(),
        name: Some("probe".into()),
        data: Some("/data/appdata/probe".into()),
        home: "/data/home/1000".into(),
        user: "user".into(),
        system,
        permissions: perms,
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        program: "/usr/bin/probe".into(),
        args: vec!["all".into()],
    }
}

fn lines(ops: &[Op]) -> Vec<String> {
    ops.iter().map(|o| o.to_string()).collect()
}

fn full_etc() -> Fake {
    let mut f = Fake::new(&[("usr/bin/probe", Kind::File), ("usr/lib/ld-linux-x86-64.so.2", Kind::File)]);
    for n in plan::SYSTEM_ETC {
        f.add(&format!("etc/{n}"), Kind::File, "");
    }
    f.add("etc/probe.conf", Kind::File, "");
    f
}

#[test]
fn whole_view_in_order_when_etc_takes_the_system_files() {
    let f = full_etc();
    let p = plan::build(&input(&f, sys_all(), &[], &[])).unwrap();
    let want = [
        "ro-bind /apps/probe/1/root/usr -> /usr",
        "symlink /bin -> usr/bin",
        "symlink /sbin -> usr/bin",
        "symlink /lib -> usr/lib",
        "symlink /lib64 -> usr/lib",
        "ro-bind /apps/probe/1/root/etc -> /etc",
        "ro-bind /etc/resolv.conf -> /etc/resolv.conf",
        "ro-bind /etc/ssl/certs/ca-certificates.crt -> /etc/ssl/certs/ca-certificates.crt",
        "ro-bind /etc/passwd -> /etc/passwd",
        "ro-bind /etc/group -> /etc/group",
        "ro-bind /etc/localtime -> /etc/localtime",
        "dev /dev",
        "proc /proc",
        "ro-bind /sys -> /sys",
        "tmpfs /tmp 1777",
        "dir /run 0755",
        "bind /run/user/1000 -> /run/user/1000",
        "dir /data/home/1000 0700",
        "ro-bind /usr/share/fonts -> /data/home/1000/.local/share/fonts",
        "bind /data/appdata/probe -> /data/appdata/probe",
        "bind /data/appdata/probe -> /data/home/1000/.local/share/probe",
        "remount-ro /",
    ];
    assert_eq!(lines(&p.ops), want);
    assert_eq!(p.argv, ["/usr/bin/probe", "all"]);
    assert_eq!(p.chdir, "/data/home/1000");
    assert!(p.notes.iter().any(|n| n.contains("PM-3")), "{:?}", p.notes);
}

#[test]
fn etc_entries_one_by_one_when_a_system_file_is_missing_or_a_link_is_in_the_way() {
    // Arch: ca-certificates.crt is a link into ca-certificates/extracted; no resolv.conf in the tree.
    let mut f = Fake::new(&[("usr/bin/probe", Kind::File), ("etc/passwd", Kind::File), ("etc/fonts/fonts.conf", Kind::File)]);
    f.add("etc/ssl/certs/ca-certificates.crt", Kind::Link, "../../ca-certificates/extracted/tls-ca-bundle.pem");
    f.add("etc/ssl/certs/other.pem", Kind::File, "");
    f.add("etc/ssl/openssl.cnf", Kind::File, "");
    f.add("etc/os-release", Kind::Link, "../usr/lib/os-release");
    f.add("etc/localtime", Kind::Dir, "");
    let p = plan::build(&input(&f, sys_all(), &[], &[])).unwrap();
    let l = lines(&p.ops);
    let etc: Vec<&str> = l.iter().map(String::as_str).skip_while(|s| !s.starts_with("dir /etc")).take_while(|s| *s != "dev /dev").collect();
    assert_eq!(
        etc,
        [
            "dir /etc 0755",
            "ro-bind /apps/probe/1/root/etc/fonts -> /etc/fonts",
            "ro-bind /etc/localtime -> /etc/localtime",
            "symlink /etc/os-release -> ../usr/lib/os-release",
            "ro-bind /etc/passwd -> /etc/passwd",
            "dir /etc/ssl 0755",
            "dir /etc/ssl/certs 0755",
            "ro-bind /etc/ssl/certs/ca-certificates.crt -> /etc/ssl/certs/ca-certificates.crt",
            "ro-bind /apps/probe/1/root/etc/ssl/certs/other.pem -> /etc/ssl/certs/other.pem",
            "ro-bind /apps/probe/1/root/etc/ssl/openssl.cnf -> /etc/ssl/openssl.cnf",
            "ro-bind /etc/group -> /etc/group",
            "ro-bind /etc/resolv.conf -> /etc/resolv.conf",
        ]
    );
    assert!(p.notes.iter().any(|n| n.contains("einzeln")), "{:?}", p.notes);
}

#[test]
fn no_etc_in_the_tree_and_missing_system_files() {
    let f = Fake::new(&[("usr/bin/probe", Kind::File)]);
    let mut s = sys_all();
    s.etc.retain(|(n, _)| n == "passwd" || n == "resolv.conf");
    let p = plan::build(&input(&f, s, &[], &[])).unwrap();
    let l = lines(&p.ops);
    let i = l.iter().position(|s| s == "dir /etc 0755").unwrap();
    assert_eq!(&l[i..i + 3], ["dir /etc 0755", "ro-bind /etc/passwd -> /etc/passwd", "ro-bind /etc/resolv.conf -> /etc/resolv.conf"]);
    assert!(p.notes.iter().any(|n| n.contains("/etc/ssl/certs/ca-certificates.crt") && n.contains("/etc/localtime")), "{:?}", p.notes);
}

#[test]
fn user_folders_and_devices_only_when_wanted() {
    let f = full_etc();
    let perm = |want: Want| Permission { want, why: String::new() };
    let perms = vec![
        perm(Want::Path { path: "/data/home/1000/Downloads".into(), write: true, create: true }),
        perm(Want::Path { path: "/home/someone/Documents".into(), write: false, create: false }),
        perm(Want::Path { path: "/data/home/1000/Pictures".into(), write: true, create: false }),
        perm(Want::Path { path: "/mnt/elsewhere".into(), write: true, create: false }),
        perm(Want::Device { class: "gpu".into(), name: None }),
        perm(Want::Device { class: "audio".into(), name: None }),
        perm(Want::Device { class: "camera".into(), name: None }),
        perm(Want::NetConnect { proto: "tcp".into(), host: None, port: Some(443) }),
    ];
    let p = plan::build(&input(&f, sys_all(), &perms, &[])).unwrap();
    let l = lines(&p.ops);
    assert!(l.contains(&"dev-bind /dev/dri -> /dev/dri".to_string()));
    assert!(!l.iter().any(|s| s.contains("/dev/snd")));
    let tail: Vec<&str> = l.iter().rev().take(3).rev().map(String::as_str).collect();
    assert_eq!(
        tail,
        ["ro-bind /data/home/1000/Documents -> /data/home/1000/Documents", "bind /data/home/1000/Downloads -> /data/home/1000/Downloads", "remount-ro /"]
    );
    for want in ["Pictures: gewünscht, aber nicht vorhanden", "/mnt/elsewhere: nicht in der Sicht", "Gerät audio: gewünscht", "Gerät camera", "Netz: wie das System"] {
        assert!(p.notes.iter().any(|n| n.contains(want)), "{want} in {:?}", p.notes);
    }
    let without = plan::build(&input(&f, sys_all(), &[], &[])).unwrap();
    assert!(!lines(&without.ops).iter().any(|s| s.contains("Downloads") || s.contains("/dev/dri")));
}

#[test]
fn environment_is_made_not_inherited() {
    let f = full_etc();
    let env = [("LD_LIBRARY_PATH", "/x"), ("LD_PRELOAD", "/y.so"), ("LANG", "de_DE.UTF-8"), ("LC_TIME", "de_DE.UTF-8"), ("SECRET", "1"), ("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0"), ("PATH", "/bin")];
    let p = plan::build(&input(&f, sys_all(), &[], &env)).unwrap();
    let e: BTreeMap<&str, &str> = p.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        e.keys().copied().collect::<Vec<_>>(),
        ["BITSOS_PROGRAM", "HOME", "LANG", "LC_TIME", "LOGNAME", "PATH", "USER", "WAYLAND_DISPLAY", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR"]
    );
    assert_eq!(e["PATH"], "/usr/bin");
    assert_eq!(e["XDG_CONFIG_HOME"], "/data/home/1000/.local/share/probe/config");
    assert!(p.notes.iter().any(|n| n.starts_with("LD_LIBRARY_PATH weggelassen")) && p.notes.iter().any(|n| n.starts_with("LD_PRELOAD weggelassen")));
}

#[test]
fn refusals_and_program_paths() {
    assert_eq!(plan::program_in_view("root/usr/bin/x", true).unwrap(), "/usr/bin/x");
    assert_eq!(plan::program_in_view("usr/bin/x", true).unwrap(), "/usr/bin/x");
    assert_eq!(plan::program_in_view("opt/brave/brave", false).unwrap(), "/opt/brave/brave");
    for bad in ["bitsos/run", "root/var/x", "usr/../etc/x", "usr//x", ""] {
        assert!(plan::program_in_view(bad, true).is_err(), "{bad}");
    }
    let no_usr = Fake::new(&[("bin/probe", Kind::File)]);
    assert!(plan::build(&input(&no_usr, sys_all(), &[], &[])).unwrap_err().contains("usr/ fehlt"));
    let no_prog = Fake::new(&[("usr/bin/other", Kind::File)]);
    assert!(plan::build(&input(&no_prog, sys_all(), &[], &[])).unwrap_err().contains("fehlt im Baum"));
}

#[test]
fn opt_shown_other_tree_folders_listed() {
    let mut f = full_etc();
    f.add("opt/brave/brave", Kind::File, "");
    f.add("var/lib/x", Kind::File, "");
    f.add("lib64", Kind::Link, "usr/lib");
    let p = plan::build(&input(&f, sys_all(), &[], &[])).unwrap();
    assert!(lines(&p.ops).contains(&"ro-bind /apps/probe/1/root/opt -> /opt".to_string()));
    assert!(p.notes.iter().any(|n| n.ends_with(": var")), "{:?}", p.notes);
}

#[test]
fn counts_follow_the_input() {
    let mut f = Fake::new(&[("usr/bin/probe", Kind::File)]);
    for k in 0..3000 {
        f.add(&format!("etc/e{k:05}"), Kind::File, "");
    }
    let p = plan::build(&input(&f, sys_all(), &[], &[])).unwrap();
    assert_eq!(lines(&p.ops).iter().filter(|s| s.contains("/root/etc/e")).count(), 3000);
}

#[test]
fn bwrap_arguments() {
    let f = full_etc();
    let mut s = sys_all();
    s.etc.clear();
    s.runtime_dir = None;
    s.sys = false;
    let mut i = input(&f, s, &[], &[]);
    i.data = None;
    let p = plan::build(&i).unwrap();
    let a = bwrap::args(&p);
    let want: Vec<&str> = vec![
        "--unshare-user", "--die-with-parent",
        "--ro-bind", "/apps/probe/1/root/usr", "/usr",
        "--symlink", "usr/bin", "/bin", "--symlink", "usr/bin", "/sbin", "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64",
        "--ro-bind", "/apps/probe/1/root/etc", "/etc",
        "--dev", "/dev", "--proc", "/proc",
        "--perms", "1777", "--tmpfs", "/tmp",
        "--perms", "0755", "--dir", "/run",
        "--perms", "0700", "--dir", "/data/home/1000",
        "--ro-bind", "/usr/share/fonts", "/data/home/1000/.local/share/fonts",
        "--remount-ro", "/",
        "--chdir", "/data/home/1000", "--", "/usr/bin/probe", "all",
    ];
    assert_eq!(a, want);
    assert!(p.notes.iter().any(|n| n.starts_with("kein Datenordner")));
}

#[test]
fn a_later_write_wish_makes_one_note_with_the_final_rights() {
    let f = full_etc();
    let perm = |write: bool| Permission { want: Want::Path { path: "/data/home/1000/Downloads".into(), write, create: false }, why: String::new() };
    let perms = vec![perm(false), perm(true)];
    let p = plan::build(&input(&f, sys_all(), &perms, &[])).unwrap();
    assert!(lines(&p.ops).contains(&"bind /data/home/1000/Downloads -> /data/home/1000/Downloads".to_string()));
    let notes: Vec<&String> = p.notes.iter().filter(|n| n.contains("Downloads")).collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("lesen und schreiben"), "{notes:?}");
}

#[test]
fn a_multilib_tree_gets_lib32() {
    let mut f = full_etc();
    f.add("usr/lib32/libc.so.6", Kind::File, "");
    let p = plan::build(&input(&f, sys_all(), &[], &[])).unwrap();
    assert!(lines(&p.ops).contains(&"symlink /lib32 -> usr/lib32".to_string()));
    let without = plan::build(&input(&full_etc(), sys_all(), &[], &[])).unwrap();
    assert!(!lines(&without.ops).iter().any(|s| s.contains("lib32")));
}

#[test]
fn the_systems_fonts_are_in_every_view_read_only() {
    const BIND: &str = "ro-bind /usr/share/fonts -> /data/home/1000/.local/share/fonts";
    // A closure with its own fontconfig configuration and fonts, one without either, and a bare tree (no name, no
    // data folder): the same one read-only bind at fontconfig's `<dir prefix="xdg">fonts</dir>`, nothing in /usr
    // or /etc of the view changes for it, and no op writes into the data folder.
    let mut with = full_etc();
    with.add("etc/fonts/fonts.conf", Kind::File, "");
    with.add("usr/share/fonts/own/Own.ttf", Kind::File, "");
    let bare = Fake::new(&[("usr/bin/probe", Kind::File)]);
    for (tree, named) in [(&with, true), (&full_etc(), true), (&bare, false)] {
        let mut i = input(tree, sys_all(), &[], &[]);
        if !named {
            (i.name, i.data) = (None, None);
        }
        let l = lines(&plan::build(&i).unwrap().ops);
        assert_eq!(l.iter().filter(|s| s.contains("fonts")).collect::<Vec<_>>(), [BIND], "{l:?}");
        assert!(l.contains(&"ro-bind /apps/probe/1/root/usr -> /usr".to_string()), "the tree's own fonts stay in its /usr: {l:?}");
        let (home, font, ro) = (l.iter().position(|s| s == "dir /data/home/1000 0700").unwrap(), l.iter().position(|s| s == BIND).unwrap(), l.len() - 1);
        assert!(home < font && font < ro && l[ro] == "remount-ro /", "{l:?}");
    }
    // The environment does not move fontconfig's places: no XDG_DATA_HOME, no FONTCONFIG_* (a caller's are dropped).
    let p = plan::build(&input(&with, sys_all(), &[], &[("XDG_DATA_HOME", "/x"), ("FONTCONFIG_FILE", "/y"), ("FONTCONFIG_PATH", "/z")])).unwrap();
    assert!(!p.env.iter().any(|(k, _)| k == "XDG_DATA_HOME" || k.starts_with("FONTCONFIG")), "{:?}", p.env);
    assert!(p.env.contains(&("XDG_CACHE_HOME".to_string(), "/data/home/1000/.local/share/probe/cache".to_string())), "the font cache stays the program's: {:?}", p.env);
    // A system without the folder: no bind (bitsrun's main says so in a note).
    let mut none = sys_all();
    none.fonts = None;
    assert!(!lines(&plan::build(&input(&with, none, &[], &[])).unwrap().ops).iter().any(|s| s.contains("/usr/share/fonts")));
    // A program named `fonts` keeps its data folder at ~/.local/share/fonts; the fonts go to fontconfig's ~/.fonts.
    let mut i = input(&with, sys_all(), &[], &[]);
    (i.name, i.data) = (Some("fonts".into()), Some("/data/appdata/fonts".into()));
    let p = plan::build(&i).unwrap();
    let l = lines(&p.ops);
    assert!(l.contains(&"ro-bind /usr/share/fonts -> /data/home/1000/.fonts".to_string()) && l.contains(&"bind /data/appdata/fonts -> /data/home/1000/.local/share/fonts".to_string()), "{l:?}");
    assert!(p.notes.iter().any(|n| n.starts_with("Schriften:")), "{:?}", p.notes);
}
