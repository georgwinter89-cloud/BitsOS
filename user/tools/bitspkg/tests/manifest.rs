//! The manifest reader against well-formed and hostile manifests.

use bitspkg::manifest::Manifest;
use bitspkg::perm::Want;

const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn base() -> String {
    format!("format = 1\nname = \"x\"\nversion = \"1.0\"\n\n[[file]]\npath = \"bin/x\"\nsha256 = \"{SHA}\"\nsize = 3\nexec = true\n")
}

fn refuse(extra: &str, want: &str) {
    let text = format!("{}{extra}", base());
    match Manifest::from_text(&text) {
        Ok(_) => panic!("accepted a hostile manifest:\n{extra}"),
        Err(e) => assert!(e.contains(want), "for\n{extra}\nwant an error with {want:?}, got {e:?}"),
    }
}

fn file(path: &str) -> String {
    format!("\n[[file]]\npath = {path}\nsha256 = \"{SHA}\"\nsize = 1\n")
}

#[test]
fn full_manifest_round_trips() {
    let text = format!(
        "{}{}",
        base(),
        r#"
[[symlink]]
path = "lib/a.so.1"
target = "a.so.1.2"

[[file]]
path = "lib/a.so.1.2"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 10

[[dir]]
path = "var/cache"

[[permission]]
kind = "path"
path = "/data/home/1000/Downloads"
write = true
create = true
why = "saves downloads"

[[permission]]
kind = "device"
class = "microphone"

[[permission]]
kind = "net_listen"
port = 9222

[[action]]
name = "open_tab"
safety = "change"
de = "Öffnet einen Tab"
en = "Opens a tab"
args = '{"type":"object"}'

[[service]]
name = "helper"
exec = "bin/x"
args = ["--quiet", "a \"b\""]
class = "background"

[[setting]]
key = "app.x.zoom"
type = "int"
min = 50
max = 300
unit = "%"
default = 100
de = "Zoom"
en = "Zoom"
"#
    );
    let m = Manifest::from_text(&text).unwrap();
    assert_eq!(m.files.len(), 2);
    assert_eq!(m.symlinks[0].target, "a.so.1.2");
    assert_eq!(m.dirs, vec!["var/cache".to_string()]);
    assert!(matches!(&m.permissions[0].want, Want::Path { write: true, create: true, .. }));
    assert_eq!(m.permissions[1].kind(), "device");
    assert_eq!(m.services[0].args[1], "a \"b\"");
    assert_eq!(m.settings[0].def.name, "app.x.zoom");
    let again = Manifest::from_text(&m.to_text()).unwrap();
    assert_eq!(again.to_text(), m.to_text());
    assert_eq!(again.files, m.files);
    assert_eq!(again.permissions, m.permissions);
}

#[test]
fn hostile_paths_are_refused() {
    refuse(&file("\"../evil\""), "bad path");
    refuse(&file("\"/etc/passwd\""), "absolute path");
    refuse(&file("\"a/./b\""), "bad path");
    refuse(&file("\"a//b\""), "bad path");
    refuse(&file("\"a/b/\""), "bad path");
    refuse(&file("\"a\\u0000b\""), "byte 0x00");
    refuse(&file("\"caf\u{e9}\""), "byte 0xc3");
    refuse(&file("\"a\\\\b\""), "byte 0x5c");
    refuse(&file("\"bin/x\""), "listed twice");
    refuse(&file("\"BIN/X\""), "listed twice");
    refuse(&file("\"bin/x/inner\""), "is a file or symlink");
    refuse(&file("\"manifest.toml\""), "manifest's own name");
    refuse(&file(&format!("\"{}\"", "a".repeat(256))), "bad path");
}

#[test]
fn hostile_symlinks_are_refused() {
    let link = |p: &str, t: &str| format!("\n[[symlink]]\npath = \"{p}\"\ntarget = \"{t}\"\n");
    refuse(&link("etc", "/etc"), "must be relative");
    refuse(&link("a/up", "../../x"), "leaves the package");
    refuse(&link("d/x", "y/../.."), ".. after a name");
    refuse(&link("up", ".."), "leaves the package");
    refuse(&link("d/self", ".."), "names no file");
    refuse(&format!("{}{}", link("lib", "bin"), file("\"lib/evil\"")), "is a file or symlink");
    refuse(&link("bin", "x"), "is a file or symlink");
    // A legal link that climbs as far as it may.
    let ok = format!("{}{}", base(), link("a/b/c", "../../bin/x"));
    Manifest::from_text(&ok).unwrap();
}

#[test]
fn hostile_names_and_values_are_refused() {
    let with = |k: &str, v: &str| base().replace(&format!("{k} = "), &format!("{k} = {v}\n#"));
    for (k, v, want) in [
        ("name", "\"../evil\"", "bad package name"),
        ("name", "\"Brave\"", "bad package name"),
        ("name", "\"a/b\"", "bad package name"),
        ("name", "\"\"", "bad package name"),
        ("version", "\"1/2\"", "bad version"),
        ("version", "\"..\"", "bad version"),
        ("version", "\"current\"", "bad version"),
        // Names that share apps/<name>/ with the id file and the links' temporaries.
        ("version", "\"id\"", "bad version"),
        ("version", "\"id.new\"", "bad version"),
        ("version", "\"current.new\"", "bad version"),
        ("version", "\"previous.new\"", "bad version"),
        ("version", "\"1.0.new\"", "bad version"),
        ("format", "3", "newer than this bitspkg"),
        ("format", "\"1\"", "must be an integer"),
        ("size", "-1", "negative"),
        ("size", "99999999999999999999", "out of range"),
        ("sha256", "\"ABC\"", "bad sha256"),
        ("exec", "\"yes\"", "true or false"),
    ] {
        let t = with(k, v);
        let e = Manifest::from_text(&t).err().unwrap_or_else(|| panic!("{k} = {v} accepted"));
        assert!(e.contains(want), "{k} = {v}: {e}");
    }
    let no_format = base().replace("format = 1\n", "");
    assert!(Manifest::from_text(&no_format).unwrap_err().contains("format missing"));
}

#[test]
fn hostile_syntax_is_refused() {
    refuse("\nsurprise = 1\n", "unknown key");
    refuse("\n[[file]]\npath = \"a\"\npath = \"b\"\n", "given twice");
    refuse("\n[[thing]]\n", "unknown entry kind");
    refuse("\n[table]\n", "only [[name]] headers");
    refuse("\n[[service]]\nname = \"s\"\nexec = \"bin/x\"\nargs = [[\"a\"]]\n", "nested arrays");
    refuse("\nx.y = 1\n", "bare keys only");
    refuse("\n[[dir]]\npath = \"unterminated\n", "without closing");
    refuse("\n[[dir]]\npath = \"a\" junk\n", "text after the value");
    refuse("\n[[dir]]\npath = 'tab\u{1}'\n", "control character");
    let huge = format!("{}#{}\n", base(), "x".repeat(bitspkg::toml::MAX_BYTES));
    assert!(Manifest::from_text(&huge).unwrap_err().contains("more than"));
}

#[test]
fn hostile_entries_are_refused() {
    let action = |safety: &str, args: &str| {
        format!("\n[[action]]\nname = \"a\"\nsafety = \"{safety}\"\nde = \"d\"\nen = \"e\"\nargs = '{args}'\n")
    };
    refuse(&action("root", "{}"), "safety");
    refuse(&action("read", "not json"), "args");
    refuse(&action("read", "[1]"), "JSON object");
    refuse(&action("read", &format!("{}{}", "[".repeat(5000), "]".repeat(5000))), "args");
    refuse(&format!("{}{}", action("read", "{}"), action("change", "{}")), "listed twice");
    let setting = |key: &str, extra: &str| {
        format!("\n[[setting]]\nkey = \"{key}\"\ntype = \"int\"\nmin = 1\nmax = 5\ndefault = 3\nde = \"d\"\nen = \"e\"\n{extra}")
    };
    refuse(&setting("app.other.zoom", ""), "start with app.x.");
    refuse(&setting("kernel.loglevel", ""), "start with app.x.");
    refuse(&setting("app.x.nvme_write", ""), "R8");
    refuse(&setting("app.x.token", ""), "kernel secret");
    refuse(&setting("app.x.Zoom", ""), "bad key name");
    refuse(&setting("app.x.zoom", "").replace("default = 3", "default = 9"), "default");
    refuse(&setting("app.x.zoom", "surprise = 1\n"), "unknown key");
    let perm = |body: &str| format!("\n[[permission]]\n{body}\n");
    refuse(&perm("kind = \"root\""), "unknown");
    refuse(&perm("kind = \"device\"\nclass = \"keyboard_spy\""), "device class");
    refuse(&perm("kind = \"path\"\npath = \"home\""), "absolute");
    refuse(&perm("kind = \"path\"\npath = \"/home/../etc\""), "bad path");
    refuse(&perm("kind = \"path\"\npath = \"/apps/brave\"\nwrite = true"), "read-only");
    refuse(&perm("kind = \"net_connect\"\nproto = \"tcp\"\nport = 70000"), "out of range");
    refuse(&perm("kind = \"net_bind\"\nproto = \"icmp\"\nport = 1"), "proto");
    refuse(&perm("kind = \"signal\"\nprogram = \"../init\""), "bad package name");
    refuse("\n[[service]]\nname = \"s\"\nexec = \"bin/none\"\n", "not an executable file");
    refuse("\n[[service]]\nname = \"s\"\nexec = \"bin/x\"\nclass = \"kernel_rt\"\n", "class");
    let not_exec = base().replace("exec = true\n", "") + "";
    let with_program = not_exec.replace("version = \"1.0\"\n", "version = \"1.0\"\nprogram = \"bin/x\"\n");
    assert!(Manifest::from_text(&with_program).unwrap_err().contains("not an executable"));
}

/// PK-6d, PACKAGES.md 6.7: format 2 (a program from the repositories) round-trips; its entry kinds are refused in
/// format 1; its paths may carry non-ASCII names only in the locale folders and case twins; links read in the view.
#[test]
fn format_2_round_trip_and_rules() {
    let sha = "ab".repeat(32);
    let text = format!(
        "format = 2\nname = \"foot\"\nversion = \"e1_1.28.0-2.1+1\"\nprogram = \"root/usr/bin/foot\"\n\n\
         [[origin]]\nrepo = \"fixture\"\nname = \"foot\"\nversion = \"1:1.28.0-2.1\"\nfilename = \"foot-1:1.28.0-2.1-x86_64_v3.pkg.tar.zst\"\nsha256 = \"{sha}\"\npackager = \"CachyOS <admin@cachyos.org>\"\nkey = \"882DCFE48E2051D48E2562ABF3B607488DB35A47\"\n\n\
         [[uses]]\npkg = \"foot\"\nversion = \"e1_1.28.0-2.1\"\n\n\
         [[launch]]\ndesktop = \"root/usr/share/applications/foot.desktop\"\nname = \"Foot\"\nexec = \"foot\"\nicon = \"foot\"\ncategories = \"System;TerminalEmulator;\"\n\n\
         [[trigger]]\nhook = \"11-glibc-ldconfig\"\ntool = \"ldconfig\"\noutput = \"root/etc/ld.so.cache\"\nsha256 = \"{sha}\"\n\n\
         [[file]]\npath = \"root/usr/bin/foot\"\nsha256 = \"{sha}\"\nsize = 1\nexec = true\n\n\
         [[file]]\npath = \"root/etc/ld.so.cache\"\nsha256 = \"{sha}\"\nsize = 1\n\n\
         [[file]]\npath = \"root/usr/share/locale/fr/LC_MESSAGES/caf\u{e9}.mo\"\nsha256 = \"{sha}\"\nsize = 1\n\n\
         [[file]]\npath = \"root/usr/share/terminfo/A/x\"\nsha256 = \"{sha}\"\nsize = 1\n\n\
         [[file]]\npath = \"root/usr/share/terminfo/a/x\"\nsha256 = \"{sha}\"\nsize = 1\n\n\
         [[symlink]]\npath = \"root/etc/fonts/conf.d/50-user.conf\"\ntarget = \"/usr/share/fontconfig/conf.default/50-user.conf\"\n"
    );
    let m = Manifest::from_text(&text).unwrap();
    assert_eq!(m.format, 2);
    assert_eq!((m.origins.len(), m.uses.len(), m.launch.len(), m.triggers.len(), m.files.len()), (1, 1, 1, 1, 5));
    assert_eq!(Manifest::from_text(&m.to_text()).unwrap(), m, "round trip");
    let one = text.replacen("format = 2", "format = 1", 1);
    assert!(Manifest::from_text(&one).unwrap_err().contains("only in format 2"), "format 1 knows no [[origin]]");
    let outside = text.replace("root/usr/share/terminfo/a/x", "root/usr/bin/caf\u{e9}");
    assert!(Manifest::from_text(&outside).unwrap_err().contains("non-ASCII"));
    let dup = text.replace("root/usr/share/terminfo/a/x", "root/usr/share/terminfo/A/x");
    assert!(Manifest::from_text(&dup).unwrap_err().contains("listed twice"));
}
