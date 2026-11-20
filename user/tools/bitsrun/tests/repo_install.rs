//! PK-6d on the host (CachyOS, as uid 1000, a scratch root on /tmp): jq and foot from the fixture mirror
//! (host/tests/pk/fixture-mirror.sh, built here from the build host's pacman cache, read-only) through a stand-in
//! for /bin/fetch (no network, R9), installed with the real keyrings of /usr/share/pacman/keyrings, triggers run
//! through this crate's bitsrun. Checked: the jq install cut at every named point (each next run finishes from
//! what is complete), jq and foot run through bitsrun, the emulated hooks' outputs are in the tree and hashed in
//! the manifest, the manifest round trip, `info --scripts`, a flipped byte, a wrong package signature and the
//! space limit refused with their reasons and nothing left, removal back to the tree before.
//! Also: a dead first mirror (the next one serves, the report says so), a cut after the download loads nothing
//! again, a package without any signature is refused, and what install registers (settings, rules, actions: a
//! recording stand-in for the services) is taken back by the removal.
//!
//! bitspkg's commands run as a child of this test binary (`child` below): a cut ends its process at once.

use bitspkg::manifest::Manifest;
use bitspkg::store::Store;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD: &str = "PK6D_CHILD_ARGS";
const REG: &str = "PK6D_REGISTERED";

/// The services' side as files: `REG/<program>` holds what the current version registered (its settings, rules
/// and actions), and removal takes it away; an empty folder at the end means nothing is left registered.
struct Recorder(PathBuf);

impl bitspkg::hooks::Hook for Recorder {
    fn what(&self) -> &'static str {
        "record"
    }
    fn needed(&self, _: &[Manifest]) -> bool {
        true
    }
    fn install(&self, m: &Manifest) -> Result<(), String> {
        let mut lines = vec![format!("settings {} actions {} services {}", m.settings.len(), m.actions.len(), m.services.len())];
        lines.extend(m.permissions.iter().map(|p| format!("rule {}", p.describe())));
        fs::create_dir_all(&self.0).map_err(|e| e.to_string())?;
        fs::write(self.0.join(&m.name), lines.join("\n")).map_err(|e| e.to_string())
    }
    fn remove(&self, name: &str, _: &[Manifest], _: bool) -> Result<(), String> {
        match fs::remove_file(self.0.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// The child: `bitspkg --root R --settings none ARGS` through the library, exit with its code.
#[test]
fn child() {
    let Ok(args) = std::env::var(CHILD) else {
        // Not the child: check that the helper process really runs bitspkg, so this test never passes empty.
        let o = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child", "--nocapture", "--test-threads=1"])
            .env(CHILD, format!("{}\u{1f}vercmp\u{1f}1:1-1\u{1f}2-1", std::env::temp_dir().join("bitsrun-child-probe").display()))
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.code() == Some(0) && out.trim_end().rsplit(char::is_whitespace).next() == Some("1"), "the helper process did not run bitspkg: {out}");
        return;
    };
    let mut a: Vec<String> = args.split('\u{1f}').map(str::to_string).collect();
    let root = PathBuf::from(a.remove(0));
    let mut st = Store::new(&root);
    st.hooks = bitspkg::hooks::Hooks::standard(None);
    if let Ok(reg) = std::env::var(REG) {
        st.hooks.list = vec![Box::new(Recorder(PathBuf::from(reg)))];
    }
    let cmd = a.remove(0);
    let code = match cmd.as_str() {
        "verify" => match bitspkg::verify::verify_counted(&st, None, true) {
            Ok((bad, c)) if bad.is_empty() => {
                println!("verify: all good ({} packages, {} versions)", c.packages, c.versions);
                0
            }
            Ok((bad, _)) => {
                println!("verify: {}", bad.join("\n"));
                1
            }
            Err(e) => {
                println!("verify: {e}");
                1
            }
        },
        "gc" => match bitspkg::remove::gc(&st) {
            Ok(_) => 0,
            Err(e) => {
                println!("{e}");
                1
            }
        },
        "remove" => match bitspkg::remove::remove(&st, &a[0], false) {
            Ok(_) => 0,
            Err(e) => {
                println!("{e}");
                1
            }
        },
        _ => match bitspkg::repo::cli::run(&st, &cmd, a) {
            Ok(c) => c,
            Err(e) => {
                println!("bitspkg: {e}");
                1
            }
        },
    };
    std::process::exit(code);
}

struct Env {
    dir: PathBuf,
    mirror: PathBuf,
    fetch: PathBuf,
}

fn bitsrun() -> &'static str {
    env!("CARGO_BIN_EXE_bitsrun")
}

fn cli(e: &Env, root: &Path, args: &[&str], cut: Option<&str>) -> (i32, String) {
    let joined = std::iter::once(root.to_str().unwrap()).chain(args.iter().copied()).collect::<Vec<_>>().join("\u{1f}");
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, joined)
        .env("BITSPKG_FETCH", &e.fetch)
        .env("BITSPKG_BITSRUN", bitsrun())
        .env(REG, root.with_extension("reg"))
        .env_remove(bitspkg::cut::ENV);
    match cut {
        Some(p) => c.env(bitspkg::cut::ENV_POINT, p),
        None => c.env_remove(bitspkg::cut::ENV_POINT),
    };
    let o = c.output().unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn clean(d: &Path) {
    if d.exists() {
        let _ = Command::new("chmod").arg("-R").arg("u+w").arg(d).status();
        fs::remove_dir_all(d).unwrap();
    }
}

fn setup() -> Env {
    // /tmp is a tmpfs on CachyOS: the per-file fsyncs of an install cost nothing there.
    let dir = std::env::temp_dir().join(format!("bitsrun-pk6d-{}", std::process::id()));
    clean(&dir);
    fs::create_dir_all(&dir).unwrap();
    let mirror = dir.join("mirror");
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let o = Command::new("bash").arg(ws.join("host/tests/pk/fixture-mirror.sh")).arg(&mirror).output().unwrap();
    assert!(o.status.success(), "RED: fixture mirror: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let fetch = dir.join("fetch");
    let script = format!(
        "#!/bin/bash\nout=\"\"; url=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift 2;; --) url=\"$2\"; shift 2;; -q) shift;; *) shift 2;; esac; done\necho \"$url\" >> {}\nsrc=\"{}/${{url#http://fix/}}\"\nif [ -f \"$src\" ]; then cp \"$src\" \"$out\"; else echo \"fetch: HTTP 404 Not Found\" >&2; exit 7; fi\n",
        dir.join("calls.log").display(),
        mirror.display()
    );
    fs::write(&fetch, script).unwrap();
    fs::set_permissions(&fetch, fs::Permissions::from_mode(0o755)).unwrap();
    Env { dir, mirror, fetch }
}

/// The sources file: `fixture` from `fixture_servers` (a TOML list), the refusal repositories from the mirror.
fn sources(e: &Env, extra: &str, fixture_servers: &str) -> PathBuf {
    let src = |n: &str, servers: &str| format!("[[source]]\nname = \"{n}\"\nservers = {servers}\nsig_db = \"optional\"\nkeyring = \"archlinux\"\n\n");
    let main = "[\"http://fix/main/$repo\"]";
    let p = e.dir.join(format!("src{}-{}.toml", extra.len(), fixture_servers.len()));
    fs::write(&p, format!("{extra}{}{}{}{}", src("fixture", fixture_servers), src("flip", main), src("badpkg", main), src("unsigned", main))).unwrap();
    p
}

/// Package downloads the fetch stand-in was asked for so far.
fn pkg_calls(e: &Env) -> usize {
    fs::read_to_string(e.dir.join("calls.log")).unwrap_or_default().lines().filter(|l| l.ends_with(".pkg.tar.zst")).count()
}

/// A scratch root with the build host's keyrings (as the image will have them: CR to the desk) and a refresh.
fn root(e: &Env, name: &str, src: &Path) -> PathBuf {
    let r = e.dir.join(name);
    clean(&r);
    let k = r.join("etc/bitspkg/keyrings");
    fs::create_dir_all(&k).unwrap();
    for f in fs::read_dir("/usr/share/pacman/keyrings").unwrap().flatten() {
        fs::copy(f.path(), k.join(f.file_name())).unwrap();
    }
    let (c, out) = cli(e, &r, &["refresh", "--sources", src.to_str().unwrap()], None);
    assert_eq!(c, 0, "refresh: {out}");
    r
}

fn snapshot(r: &Path) -> Vec<String> {
    let mut v: Vec<String> = bitspkg::snapshot::take(r).unwrap().into_iter().map(|(k, v)| format!("{k} {v}")).collect();
    v.retain(|l| !l.starts_with("apps/.next-id ") && !l.starts_with("apps/.lock "));
    v
}

fn run_ok(r: &Path, args: &[&str]) -> String {
    let o = Command::new(bitsrun()).arg("--store").arg(r).args(args).env("LANG", "de_DE.UTF-8").output().unwrap();
    let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(o.status.success(), "bitsrun {args:?}: {t}");
    t
}

/// A command of foot's tree in its view (`--root`, foot's name and data folder), in the C locale.
fn in_view(tree: &Path, data: &Path, home: &Path, cmd: &[&str]) -> (bool, String) {
    let o = Command::new(bitsrun()).arg("--root").arg(tree).args(["--name", "foot", "--data"]).arg(data).arg("--").args(cmd).env("HOME", home).env("LC_ALL", "C").output().unwrap();
    (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// PK-FONTS: foot's closure brings fontconfig but no font, so whatever its own fc-match finds is the system's.
/// It finds one with the closure's fonts.conf and, in a tree without /etc (fontconfig's built-in configuration),
/// at the same place; nothing is put into the data folder but fontconfig's cache; the view cannot write the fonts.
fn fonts_in_the_view(e: &Env, st: &Store, version: &str) {
    assert!(Path::new(bitsrun::plan::SYSTEM_FONTS).is_dir(), "RED: this host has no {}: the font check cannot do its work", bitsrun::plan::SYSTEM_FONTS);
    let home = e.dir.join("home");
    fs::create_dir_all(&home).unwrap();
    let (tree, data) = (st.version_dir("foot", version).join("root"), st.data_dir("foot"));
    assert!(!tree.join("usr/share/fonts").exists(), "foot's closure has a font now: this check needs a closure without one");
    let bare = e.dir.join("noconf");
    fs::create_dir_all(&bare).unwrap();
    assert!(Command::new("cp").arg("-al").arg(tree.join("usr")).arg(bare.join("usr")).status().unwrap().success());
    let place = format!("{}/.local/share/fonts/", home.display());
    for (t, what) in [(&tree, "the closure's fonts.conf"), (&bare, "no /etc/fonts in the tree")] {
        let (ok, out) = in_view(t, &data, &home, &["fc-match", "-f", "%{file}\n", "monospace"]);
        assert!(ok && out.lines().any(|l| l.starts_with(&place)), "{what}: fc-match finds no system font below {place}: {out}");
    }
    // The folders of the hard-linked copy only: the files are the store's inodes and keep their mode.
    assert!(Command::new("find").arg(&bare).args(["-type", "d", "-exec", "chmod", "u+w", "{}", "+"]).status().unwrap().success());
    fs::remove_dir_all(&bare).unwrap();
    let (ok, out) = in_view(&tree, &data, &home, &["sh", "-c", ": > \"$HOME/.local/share/fonts/pk-probe\""]);
    assert!(!ok && out.contains("Read-only file system"), "the view can write the system's fonts: {out}");
    assert!(!data.join("fonts").exists() && !data.join("config/fontconfig").exists(), "a font or a fonts.conf was placed in the data folder");
    let cache = fs::read_dir(data.join("cache/fontconfig")).map(|d| d.flatten().filter(|f| f.file_name().to_string_lossy().contains(".cache-")).count()).unwrap_or(0);
    assert!(cache > 0, "fontconfig's cache is not in the program's cache folder");
}

#[test]
fn repo_install_jq_and_foot() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let e = setup();
    let src = sources(&e, "", "[\"http://fix/main/$repo\"]");
    let s = src.to_str().unwrap();
    let r = root(&e, "root", &src);
    let st = Store::new(&r);
    let (c, out) = cli(&e, &r, &["gc"], None);
    assert_eq!(c, 0, "{out}");
    let before = snapshot(&r);

    // jq, cut at every named point; each next run starts from what the cut left.
    let points = ["after download", "after verify", "after unpack", "after triggers", "before switch", "after switch"];
    for p in points {
        let calls = pkg_calls(&e);
        let (c, out) = cli(&e, &r, &["install", "jq", "--sources", s], Some(p));
        assert_eq!(c, bitspkg::cut::EXIT, "cut {p}: {out}");
        if p == "after verify" {
            // The run before was cut after its download: its files are complete in the cache and taken from there.
            assert_eq!(pkg_calls(&e), calls, "cut {p}: the cached packages were loaded again: {out}");
        }
        let cur = st.current("jq");
        assert_eq!(cur.is_some(), p == "after switch", "cut {p}: current is {cur:?}");
        // As PK-1's cut test: the next command recovers (gc drops what the cut left unswitched), then verify.
        let (c, out) = cli(&e, &r, &["gc"], None);
        assert_eq!(c, 0, "cut {p}: gc: {out}");
        let (c, out) = cli(&e, &r, &["verify"], None);
        assert_eq!(c, 0, "cut {p}: verify: {out}");
        eprintln!("jq cut {p}: green");
    }
    let (c, out) = cli(&e, &r, &["install", "jq", "--sources", s], None);
    assert_eq!(c, 0, "jq: {out}");
    assert!(out.contains("schon installiert") || out.contains("Installiert: jq"), "{out}");
    assert_eq!(st.current("jq").as_deref(), Some("1.8.2-1.1+1"));
    // The tree equals an uncut install's.
    let r2 = root(&e, "ref", &src);
    let (c, out) = cli(&e, &r2, &["install", "jq", "--sources", s], None);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("Hook 11-glibc-ldconfig: emuliert (ldconfig): root/etc/ld.so.cache"), "{out}");
    // A ProgramId given before a cut is never reused, so only the id file may differ.
    let strip = |v: Vec<String>| v.into_iter().filter(|l| l.starts_with("apps/") && !l.starts_with("apps/jq/id ")).collect::<Vec<_>>();
    assert_eq!(strip(snapshot(&r)), strip(snapshot(&r2)), "the cut-and-finished tree differs from the uncut one");
    clean(&r2);
    assert!(run_ok(&r, &["jq", "-n", "[1+1]"]).contains("2"));

    // foot: GUI, its hooks and launchers.
    let (c, out) = cli(&e, &r, &["install", "foot", "--sources", s], None);
    assert_eq!(c, 0, "foot: {out}");
    for want in [
        "Pakete: 36 (31 neu entpackt, 5 schon in /apps/.pkgs)",
        "Hook 10-glibc-locale-gen: emuliert (localedef)",
        "Hook 40-fontconfig-config: emuliert: ",
        "Hook fontconfig: später: beim ersten Start",
        "Installationsskript von fontconfig (nie ausgeführt",
        "Starter: Foot (foot;",
    ] {
        assert!(out.contains(want), "foot report lacks {want:?}:\n{out}");
    }
    let v = st.current("foot").unwrap();
    let m = st.manifest("foot", &v).unwrap();
    assert_eq!(m.format, 2);
    assert_eq!(Manifest::from_text(&m.to_text()).unwrap(), m, "manifest round trip");
    assert_eq!(m.uses.len(), 36);
    assert!(m.origins.iter().all(|o| o.key.len() == 40), "every package names the key that signed it");
    for t in &m.triggers {
        let f = m.files.iter().find(|f| f.path == t.output).unwrap_or_else(|| panic!("{} is no file of the version", t.output));
        assert_eq!(f.sha256, t.sha256);
    }
    assert!(m.triggers.iter().any(|t| t.output == "root/usr/lib/locale/de_DE.utf8/LC_CTYPE"));
    assert!(m.symlinks.iter().any(|l| l.path.starts_with("root/etc/fonts/conf.d/") && l.target.starts_with("/usr/share/fontconfig/conf.default/")));
    assert!(fs::symlink_metadata(st.pkgs_entry("glibc", "2.44+r50+g1848099f063e-3")).unwrap().permissions().mode() & 0o222 == 0, ".pkgs entries are read-only");
    assert!(run_ok(&r, &["foot", "--version"]).contains("foot version: 1.28.0"));
    fonts_in_the_view(&e, &st, &v);
    let (c, out) = cli(&e, &r, &["info", "--scripts", "foot"], None);
    assert!(c == 0 && out.contains("== bash 5.3.20-2") && out.contains("| post_upgrade() {"), "{out}");
    let (c, out) = cli(&e, &r, &["verify"], None);
    assert!(c == 0 && out.contains("verify: all good (2 packages, 2 versions)"), "{out}");
    let reg = r.with_extension("reg");
    assert!(reg.join("jq").is_file() && reg.join("foot").is_file(), "install registers the current versions with the services");
    assert!(fs::read_to_string(reg.join("foot")).unwrap().starts_with("settings 0 actions 0 services 0"), "a repository program brings no settings, actions or services");

    // Refusals, each with its reason and nothing left behind (a fresh root: in `r` jq's package is unpacked
    // already, so the same %SHA256SUM% would never be downloaded again). This root's `fixture` source names a dead
    // mirror first: the next one serves.
    let src_fb = sources(&e, "", "[\"http://fix/dead/$repo\", \"http://fix/main/$repo\"]");
    let s_fb = src_fb.to_str().unwrap();
    let rr = root(&e, "refusals", &src_fb);
    let (c, _) = cli(&e, &rr, &["gc"], None);
    assert_eq!(c, 0);
    let mid = snapshot(&rr);
    let (c, out) = cli(&e, &rr, &["install", "flip/jq", "--sources", s_fb], None);
    assert!(c == 1 && out.contains("falscher Inhalt"), "flipped byte: {out}");
    let (c, out) = cli(&e, &rr, &["install", "badpkg/jq", "--sources", s_fb], None);
    assert!(c == 1 && out.contains("Signatur vom Spiegel abgelehnt") && out.contains("Signatur aus der Datenbank abgelehnt"), "wrong signature: {out}");
    let (c, out) = cli(&e, &rr, &["install", "unsigned/jq", "--sources", s_fb], None);
    assert!(c == 1 && out.contains("keine Signatur"), "no signature: {out}");
    let leftover: Vec<String> = snapshot(&rr).into_iter().filter(|l| !mid.contains(l) && !l.starts_with("data/bitspkg/cache/") && !l.starts_with("data/bitspkg/cache ")).collect();
    assert!(leftover.is_empty(), "a refused install leaves nothing in /apps: {leftover:?}");
    // The good packages of the closure stay in the cache for the next try (resume); the refused one is gone.
    assert!(!fs::read_dir(rr.join("data/bitspkg/cache")).unwrap().flatten().any(|f| f.file_name().to_string_lossy().starts_with("jq-")));
    // The dead mirror: jq comes from the next one, the report names the fallback; removal leaves the root as it was.
    let (c, out) = cli(&e, &rr, &["install", "jq", "--sources", s_fb], None);
    assert!(c == 0 && out.contains("Ausweichen: http://fix/dead/fixture: nicht vorhanden"), "fallback: {out}");
    assert!(out.contains("schon im Zwischenspeicher") && !out.contains(", 0 schon im Zwischenspeicher"), "the refused runs' good packages were reused: {out}");
    let (c, out) = cli(&e, &rr, &["remove", "jq"], None);
    assert_eq!(c, 0, "{out}");
    assert_eq!(snapshot(&rr), mid, "after the fallback install and its removal the root differs");
    clean(&rr);
    let tiny = sources(&e, "space_max_gb = 0\n", "[\"http://fix/main/$repo\"]");
    let r3 = root(&e, "tiny", &tiny);
    let (c, out) = cli(&e, &r3, &["install", "jq", "--sources", tiny.to_str().unwrap()], None);
    assert!(c == 1 && out.contains("Programme dürfen höchstens 0 B belegen (pkg.space.max_gb)") && out.contains("nichts wurde heruntergeladen"), "space: {out}");
    assert!(!r3.join("data/bitspkg/cache").exists() || fs::read_dir(r3.join("data/bitspkg/cache")).unwrap().next().is_none());
    assert!(!r3.join("apps/jq").exists());
    clean(&r3);

    // Removal: back to the tree before (B2), no .pkgs entry and no store file left.
    for p in ["foot", "jq"] {
        let (c, out) = cli(&e, &r, &["remove", p], None);
        assert_eq!(c, 0, "remove {p}: {out}");
    }
    assert_eq!(snapshot(&r), before, "after removal the tree differs from before the install");
    assert!(fs::read_dir(&reg).map(|mut d| d.next().is_none()).unwrap_or(true), "something stays registered after the removal");
    let _ = e.mirror;
    clean(&e.dir);
}
