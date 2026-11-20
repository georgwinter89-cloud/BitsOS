//! PK-6e on the host (CachyOS, as uid 1000, a scratch root on /tmp): `bitspkg update` against the fixture mirror
//! (fixture-mirror.sh, then fixture-update.sh: a rebuilt oniguruma signed by TEST key B, a keyring package
//! bringing B signed by TEST key A, a mirror list package, and a newer keyring package signed by the unknown C),
//! through a stand-in for /bin/fetch (no network, R9). The real keyrings of /usr/share/pacman/keyrings plus the
//! TEST keyring (A) start as the image's. Checked: `update --check` reports and changes nothing; `update` installs
//! the keyring and mirror list packages first (checked with the old keyring), then jq's new version with only
//! oniguruma fetched (checked with the new keyring); the installed mirror list serves; rollback says "1 Teil
//! älter" and back "1 Teil neuer"; removing the old version takes the old oniguruma's `.pkgs` entry; a keyring
//! package with an unknown signature is refused and the keyring kept; an older database stays refused; removal
//! leaves no `.pkgs` entry.

use bitspkg::store::Store;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD: &str = "PK6E_CHILD_ARGS";

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
    let cmd = a.remove(0);
    let r = match cmd.as_str() {
        "verify" => bitspkg::verify::verify_counted(&st, None, true).map(|(bad, c)| {
            println!("verify: {} ({} packages, {} versions)", if bad.is_empty() { "all good".into() } else { bad.join("; ") }, c.packages, c.versions);
            i32::from(!bad.is_empty())
        }),
        "rollback" => bitspkg::install::rollback(&st, &a[0]).map(|(from, to, _)| {
            println!("{} back from {from} to {to}", a[0]);
            bitspkg::repo::update::rollback_lines(&st, &a[0], &from, &to).iter().for_each(|l| println!("{l}"));
            0
        }),
        "remove-version" => bitspkg::remove::remove_version(&st, &a[0], &a[1]).map(|_| 0),
        "remove" => bitspkg::remove::remove(&st, &a[0], false).map(|r| {
            println!("removed ({} .pkgs entries)", r.pkgs_gone);
            0
        }),
        _ => bitspkg::repo::cli::run(&st, &cmd, a),
    };
    std::process::exit(r.unwrap_or_else(|e| {
        println!("bitspkg: {e}");
        1
    }));
}

struct Env {
    dir: PathBuf,
    fetch: PathBuf,
}

fn cli(e: &Env, root: &Path, args: &[&str]) -> (i32, String) {
    let joined = std::iter::once(root.to_str().unwrap()).chain(args.iter().copied()).collect::<Vec<_>>().join("\u{1f}");
    let o = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, joined)
        .env("BITSPKG_FETCH", &e.fetch)
        .env("BITSPKG_BITSRUN", env!("CARGO_BIN_EXE_bitsrun"))
        .env_remove(bitspkg::cut::ENV)
        .env_remove(bitspkg::cut::ENV_POINT)
        .output()
        .unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn clean(d: &Path) {
    if d.exists() {
        let _ = Command::new("chmod").arg("-R").arg("u+w").arg(d).status();
        fs::remove_dir_all(d).unwrap();
    }
}

fn setup() -> Env {
    let dir = std::env::temp_dir().join(format!("bitsrun-pk6e-{}", std::process::id()));
    clean(&dir);
    fs::create_dir_all(&dir).unwrap();
    let mirror = dir.join("mirror");
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for s in ["fixture-mirror.sh", "fixture-update.sh"] {
        let o = Command::new("bash").arg(ws.join("host/tests/pk").join(s)).arg(&mirror).output().unwrap();
        assert!(o.status.success(), "RED: {s}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    let fetch = dir.join("fetch");
    let script = format!(
        "#!/bin/bash\nout=\"\"; url=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift 2;; --) url=\"$2\"; shift 2;; -q) shift;; *) shift 2;; esac; done\necho \"$url\" >> {}\nsrc=\"{}/${{url#http://fix/}}\"\nif [ -f \"$src\" ]; then cp \"$src\" \"$out\"; else echo \"fetch: HTTP 404 Not Found\" >&2; exit 7; fi\n",
        dir.join("calls.log").display(),
        mirror.display()
    );
    fs::write(&fetch, script).unwrap();
    fs::set_permissions(&fetch, fs::Permissions::from_mode(0o755)).unwrap();
    Env { dir, fetch }
}

fn sources(e: &Env, name: &str, servers: &str) -> String {
    let p = e.dir.join(format!("{name}.toml"));
    fs::write(&p, format!("[[source]]\nname = \"fixture\"\nservers = {servers}\nsig_db = \"optional\"\nkeyring = \"archlinux\"\n")).unwrap();
    p.to_str().unwrap().to_string()
}

fn calls(e: &Env) -> Vec<String> {
    fs::read_to_string(e.dir.join("calls.log")).unwrap_or_default().lines().filter(|l| l.ends_with(".pkg.tar.zst")).map(str::to_string).collect()
}

fn snapshot(r: &Path) -> Vec<String> {
    let mut v: Vec<String> = bitspkg::snapshot::take(r).unwrap().into_iter().map(|(k, v)| format!("{k} {v}")).collect();
    v.retain(|l| !l.starts_with("apps/.next-id ") && !l.starts_with("apps/.lock ") && !l.contains("/.inuse "));
    v
}

fn has(out: &str, want: &str) {
    assert!(out.contains(want), "missing {want:?} in:\n{out}");
}

fn folders(r: &Path, pkg: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(r.join("apps/.pkgs").join(pkg)).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

#[test]
fn repo_update_jq_keyring_mirrorlist_rollback_gc() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let e = setup();
    let a = sources(&e, "a", "[\"http://fix/main/$repo\"]");
    let b = sources(&e, "b", "[\"include:bitstest-mirrorlist\", \"http://fix/upd/b/$repo\"]");
    let c = sources(&e, "c", "[\"http://fix/upd/c/$repo\"]");
    let m = sources(&e, "m", "[\"include:bitstest-mirrorlist\"]");
    let r = e.dir.join("root");
    let k = r.join("etc/bitspkg/keyrings");
    fs::create_dir_all(&k).unwrap();
    for d in [PathBuf::from("/usr/share/pacman/keyrings"), e.dir.join("mirror/upd/keys")] {
        for f in fs::read_dir(d).unwrap().flatten() {
            fs::copy(f.path(), k.join(f.file_name())).unwrap();
        }
    }
    let st = Store::new(&r);
    let (code, out) = cli(&e, &r, &["refresh", "--sources", &a]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = cli(&e, &r, &["install", "jq", "--sources", &a]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(st.current("jq").as_deref(), Some("1.8.2-1.1+1"));
    let onig_old = folders(&r, "oniguruma");
    assert_eq!(onig_old.len(), 1);

    // --check: reports, downloads no package, changes nothing in /apps and no keyring.
    let apps = snapshot(&r.join("apps"));
    let n = calls(&e).len();
    let (code, out) = cli(&e, &r, &["update", "--check", "--sources", &b]);
    assert_eq!(code, 0, "{out}");
    has(&out, "jq 1.8.2-1.1+1: 1 Teil neuer (oniguruma 6.9.10-3.1 → 6.9.10-3.1.1)");
    has(&out, "Aktualisierung: Schlüsselbund bitstest-keyring: Abbild → 1-1 (fixture)");
    has(&out, "Aktualisierung: Spiegelliste bitstest-mirrorlist: Abbild → 1-1 (fixture)");
    has(&out, "3 Aktualisierungen gefunden (installiert wird nur auf Wunsch");
    assert_eq!(calls(&e).len(), n, "--check downloaded a package");
    assert_eq!(snapshot(&r.join("apps")), apps, "--check changed /apps");
    assert!(!r.join("data/bitspkg/keyrings").exists() && !r.join("data/bitspkg/mirrorlist").exists());
    assert!(bitspkg::repo::update::last_check(&r).is_some_and(|(_, f)| f == 3), "the check is recorded for the schedule");

    // update: parts first (checked with the old keyring), then jq with only oniguruma fetched.
    let (code, out) = cli(&e, &r, &["update", "--sources", &b]);
    assert_eq!(code, 0, "{out}");
    has(&out, "Neuer Schlüsselbund geprüft:");
    has(&out, "Installiert: jq 1.8.2-1.1+2");
    has(&out, "Pakete: 7 (1 neu entpackt, 6 schon in /apps/.pkgs)");
    has(&out, "Download: 1 Pakete");
    has(&out, "data/bitspkg/keyrings)");
    let got: Vec<String> = calls(&e)[n..].to_vec();
    assert_eq!(got.len(), 3, "keyring, mirror list and oniguruma only: {got:?}");
    assert!(got.iter().any(|u| u.contains("/oniguruma-6.9.10-3.1.1-")), "{got:?}");
    assert_eq!(st.current("jq").as_deref(), Some("1.8.2-1.1+2"));
    assert_eq!(st.previous("jq").as_deref(), Some("1.8.2-1.1+1"));
    assert!(r.join("data/bitspkg/keyrings/bitstest.gpg").is_file() && r.join("data/bitspkg/keyrings/archlinux.gpg").is_file());
    assert!(fs::read_to_string(r.join("data/bitspkg/mirrorlist/bitstest-mirrorlist")).unwrap().contains("Server = http://fix/upd/b/$repo"));
    let (code, out) = cli(&e, &r, &["refresh", "--sources", &m]);
    assert_eq!(code, 0, "the installed mirror list serves: {out}");
    let n = calls(&e).len();
    let (code, out) = cli(&e, &r, &["update", "--sources", &b]);
    assert_eq!(code, 0, "{out}");
    has(&out, "jq 1.8.2-1.1+2: aktuell");
    assert_eq!(calls(&e).len(), n, "a second update fetched something: {out}");

    // rollback with the closure difference, and back.
    let (code, out) = cli(&e, &r, &["rollback", "jq"]);
    assert_eq!(code, 0, "{out}");
    has(&out, "jq 1.8.2-1.1+1 (installiert am ");
    has(&out, "1 Teil älter (oniguruma 6.9.10-3.1.1 → 6.9.10-3.1)");
    let (code, out) = cli(&e, &r, &["rollback", "jq"]);
    assert_eq!(code, 0, "{out}");
    has(&out, "1 Teil neuer (oniguruma 6.9.10-3.1 → 6.9.10-3.1.1)");
    // gc: the old oniguruma goes once no version uses it.
    assert_eq!(folders(&r, "oniguruma").len(), 2);
    let (code, out) = cli(&e, &r, &["remove-version", "jq", "1.8.2-1.1+1"]);
    assert_eq!(code, 0, "{out}");
    has(&out, "event=pkg.pkgs.gone pkg=oniguruma");
    let left = folders(&r, "oniguruma");
    assert!(left.len() == 1 && left != onig_old, "{left:?}");
    let (code, out) = cli(&e, &r, &["verify"]);
    assert_eq!(code, 0, "{out}");

    // A keyring package signed by a key no keyring knows: refused, the keyring as it was, programs untouched.
    let keys = snapshot(&r.join("data/bitspkg/keyrings"));
    let (code, out) = cli(&e, &r, &["update", "--sources", &c]);
    assert_eq!(code, 1, "{out}");
    has(&out, "nicht im Schlüsselbund");
    has(&out, "bitstest-keyring nicht übernommen (der bisherige Schlüsselbund bleibt aktiv)");
    assert_eq!(snapshot(&r.join("data/bitspkg/keyrings")), keys, "the refused keyring changed the active one");
    assert!(!r.join("data/bitspkg/keyrings.new").exists());

    // An older database (main's, older than c's) stays refused on the update path.
    let (code, out) = cli(&e, &r, &["update", "--sources", &a]);
    assert_eq!(code, 1, "{out}");
    has(&out, "Datenbank abgelehnt (oben); die bisherige gilt weiter");
    assert_eq!(st.current("jq").as_deref(), Some("1.8.2-1.1+2"));
    // A check that did not see every source is not recorded (D5's schedule tries again) and says so.
    fs::write(bitspkg::repo::update::check_record(&r), "checked = 1\nfound = 9\n").unwrap();
    let (code, out) = cli(&e, &r, &["update", "--check", "--sources", &a]);
    assert_eq!(code, 1, "{out}");
    has(&out, "Prüfung unvollständig (fixture: Datenbank abgelehnt)");
    assert!(!out.contains("Nichts zu aktualisieren"), "{out}");
    has(&out, "event=pkg.update.check_incomplete");
    assert_eq!(bitspkg::repo::update::last_check(&r), Some((1, 9)), "an incomplete check was recorded");

    // Removal: no .pkgs entry stays.
    let (code, out) = cli(&e, &r, &["remove", "jq"]);
    assert_eq!(code, 0, "{out}");
    has(&out, "removed (7 .pkgs entries)");
    assert!(fs::read_dir(r.join("apps/.pkgs")).map(|mut d| d.next().is_none()).unwrap_or(true));
    clean(&e.dir);
}
