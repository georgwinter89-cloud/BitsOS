//! UPDATE: `cargo xtask update-release --version N --notes FILE`: one BitsOS version for the update page, as a
//! release `version-<N>` of the private workshop repository (plan/WERKSTATT.md). It builds the everyday image with
//! the number stamped in (`BITSOS_VERSION`, the lean image like release-assets), strips the kernel's debug
//! sections (objcopy --strip-debug: the loaded segments with the disk guard's and the version's marks stay, both
//! checked afterwards), writes WASISTNEU.TXT from the desk's file, the manifest (sizes, SHA-256) and its Ed25519
//! signature with the desk's key, checks the whole set the way BitsOS will, and with `--publish` uploads it with
//! `gh release create`. Dry run by default (nothing leaves the host).
//!
//! The key: `~/.config/bitsos-release/release.key` (0600, made once by `bitsupdate keygen`; it never leaves the host,
//! never goes into git or a release), its public half in `user/lib/bitsupdate/keys/release.pub` (in git, compiled
//! into BitsOS). The tool refuses when the two do not belong together: BitsOS would refuse the release.

use crate::util::*;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HELP: &str = "usage: cargo xtask update-release --version N --notes FILE [--min-previous M] [--out DIR] [--key-dir DIR]
                                  [--pubkey-file F] [--release] [--ktest] [--full] [--publish]
                                  [--trees all|none|NAME,NAME] [--bin-file NAME=FILE]... [--extra-tree NAME=DIR]...
  Builds version N (BITSOS_VERSION=N, lean image unless --full), writes DIR (default target/xtask/update-N):
  KERNEL (debug sections stripped), INITRD, BOOTX64.EFI, WASISTNEU.TXT (from FILE), MANIFEST, MANIFEST.SIG,
  the parts (UPD-4: bin.<name> for every file of \\BITSOS2\\bin, tree.<name>.<hash>.<k>.tar.zst for every program
  tree and the firmware, --trees picks them), MANIFEST2, MANIFEST2.SIG, and checks them as BitsOS will. The key lives in --key-dir (default ~/.config/bitsos-release, made once with
  0600); its public half must be --pubkey-file (default user/lib/bitsupdate/keys/release.pub, the one BitsOS
  is built with). --publish: gh release create version-N in georgwinter89-cloud/bitsos-werkstatt.";

pub const REPO: &str = "georgwinter89-cloud/bitsos-werkstatt";
pub const FILES: [&str; 8] = ["KERNEL", "INITRD", "BOOTX64.EFI", "WASISTNEU.TXT", "MANIFEST", "MANIFEST.SIG", "MANIFEST2", "MANIFEST2.SIG"];
/// The disk guard's mark (crates/gp-platform/src/disk_rule/marker.rs): a stripped kernel must keep it.
const GUARD_MARK: &[u8] = gp_platform::disk_rule::marker::KERNEL_MARK;

#[derive(Debug, PartialEq)]
pub struct Opts {
    pub version: u32,
    pub notes: PathBuf,
    pub min_previous: Option<u32>,
    pub out: PathBuf,
    pub key_dir: PathBuf,
    pub pubkey: PathBuf,
    pub release: bool,
    pub ktest: bool,
    pub full: bool,
    pub publish: bool,
    pub parts: crate::update_parts::PartsOpts,
}

pub fn parse(root: &Path, a: &mut Args) -> Result<Opts, String> {
    let v = a.value("--version")?.ok_or("--version N is needed (the desk's next number)")?;
    let version = version_number(&v)?;
    let notes = PathBuf::from(a.value("--notes")?.ok_or("--notes FILE is needed (Was ist neu, plain German)")?);
    let min_previous = match a.value("--min-previous")? {
        Some(m) => Some(version_number(&m)?).filter(|&m| m < version).map(Some).ok_or("--min-previous is below --version")?,
        None => None,
    };
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("no HOME for the key folder")?;
    Ok(Opts {
        version,
        notes,
        min_previous,
        out: a.value("--out")?.map(PathBuf::from).unwrap_or_else(|| xdir(root).join(format!("update-{version}"))),
        key_dir: a.value("--key-dir")?.map(PathBuf::from).unwrap_or_else(|| home.join(".config/bitsos-release")),
        pubkey: a.value("--pubkey-file")?.map(PathBuf::from).unwrap_or_else(|| root.join("user/lib/bitsupdate/keys/release.pub")),
        release: a.flag("--release"),
        ktest: a.flag("--ktest"),
        full: a.flag("--full"),
        publish: a.flag("--publish"),
        parts: crate::update_parts::parse(a)?,
    })
}

/// A version number as the desk gives it: 164 and up (above every generation in use on 15.11.2026), at most the
/// boot medium's largest folder number.
pub fn version_number(v: &str) -> Result<u32, String> {
    let n: u32 = v.parse().ok().filter(|_| v.bytes().all(|b| b.is_ascii_digit())).ok_or_else(|| format!("{v:?} is no version number"))?;
    if !(164..=99_999_999).contains(&n) {
        return Err(format!("version {n}: the desk's numbers start at 164 (above every generation in use) and end at 99999999"));
    }
    Ok(n)
}

/// The notes as the page shows them: UTF-8 text, not empty, at most 64 KiB, no control characters but newlines.
pub fn notes_ok(b: &[u8]) -> Result<(), String> {
    let t = std::str::from_utf8(b).map_err(|_| String::from("the notes are no UTF-8 text"))?;
    if t.trim().is_empty() || b.len() > 64 << 10 {
        return Err(String::from("the notes are empty or longer than 64 KiB"));
    }
    if t.chars().any(|c| c.is_control() && c != '\n') {
        return Err(String::from("the notes hold control characters (tabs, CR, escapes): plain lines only"));
    }
    Ok(())
}

/// The `gh` command that publishes `out` (dry run prints it): the fixed files, then the parts (`parts`: their
/// asset names, as `bitsupdate manifest2` left them).
pub fn gh_args(o: &Opts, parts: &[String]) -> Vec<String> {
    let mut v: Vec<String> = ["release", "create", &format!("version-{}", o.version), "--repo", REPO, "--title", &format!("Version {}", o.version)]
        .iter()
        .map(|s| s.to_string())
        .collect();
    v.push("--notes-file".into());
    v.push(o.out.join("WASISTNEU.TXT").display().to_string());
    for f in FILES.iter().copied().chain(parts.iter().map(String::as_str)) {
        v.push(o.out.join(f).display().to_string());
    }
    v
}

/// The parts' asset names in `out` (sorted).
fn part_assets(out: &Path) -> Result<Vec<String>, String> {
    let mut v: Vec<String> = std::fs::read_dir(out)
        .map_err(|e| format!("{}: {e}", out.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("bin.") || n.starts_with("tree."))
        .collect();
    v.sort();
    Ok(v)
}

/// The public key's 64 hex digits of a key file (`#` lines skipped); None: `none` or no key line.
pub fn pub_hex(text: &str) -> Option<String> {
    let l = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'))?;
    (l.len() == 64 && l.bytes().all(|b| b.is_ascii_hexdigit())).then(|| l.to_ascii_lowercase())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The version mark of `n` as bitsver writes it.
pub fn version_mark(n: u32) -> Vec<u8> {
    let mut m = format!("BITSOS-VERSION={n:08};").into_bytes();
    m.extend_from_slice(b"\xb7\n");
    m
}

/// The marker file in an output folder this tool made: only such a folder (or a missing or empty one, or the
/// default `target/xtask/update-<N>`) is cleared before a build.
pub const OUT_MARK: &str = ".update-release";

/// `p` made absolute without following what does not exist yet: the longest existing ancestor canonicalized,
/// the rest appended (`..` in the rest is refused).
fn absolute(p: &Path) -> Result<PathBuf, String> {
    let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map_err(|e| e.to_string())?.join(p) };
    let mut rest = Vec::new();
    let mut at = p.as_path();
    loop {
        if let Ok(c) = at.canonicalize() {
            let mut c = c;
            for r in rest.iter().rev() {
                c.push(r);
            }
            return Ok(c);
        }
        let name = at.file_name().ok_or_else(|| format!("{}: no such path", p.display()))?;
        if name == ".." {
            return Err(format!("{}: .. in a path that does not exist yet", p.display()));
        }
        rest.push(name.to_os_string());
        at = at.parent().ok_or_else(|| format!("{}: no such path", p.display()))?;
    }
}

/// Whether `out` may be cleared (pure on absolute paths): never the key folder, inside it, or a folder that holds
/// it (the desk's only private release key would go); otherwise a missing or empty folder, one this tool made
/// (`OUT_MARK`), or the default `target/xtask/update-<N>`.
pub fn out_ok(out: &Path, key_dir: &Path, default: &Path, exists: bool, empty: bool, marked: bool) -> Result<(), String> {
    if out == key_dir || key_dir.starts_with(out) || out.starts_with(key_dir) {
        return Err(format!("--out {} overlaps the key folder {}: refused (the release key would be deleted)", out.display(), key_dir.display()));
    }
    if !exists || empty || marked || out == default {
        return Ok(());
    }
    Err(format!("--out {} is a folder with files this tool did not make: refused (it is cleared before a build); name a new or empty folder", out.display()))
}

fn check_out(root: &Path, o: &Opts) -> Result<(), String> {
    let out = absolute(&o.out)?;
    let key_dir = absolute(&o.key_dir)?;
    let default = absolute(&xdir(root).join(format!("update-{}", o.version)))?;
    let exists = out.exists();
    let empty = std::fs::read_dir(&out).map(|mut d| d.next().is_none()).unwrap_or(false);
    out_ok(&out, &key_dir, &default, exists, empty, out.join(OUT_MARK).is_file())
}

/// With `--publish` the release must trust the key every BitsOS is built with (pure on canonical paths): the
/// public key file in git, no other file swapped in (`--pubkey-file` or the environment). A test key's release
/// published would make every machine that installs it trust a key whose private half sits unprotected.
pub fn publish_key_ok(pubkey: &Path, in_git: &Path, env_override: bool, tracked: bool, unchanged: bool) -> Result<(), String> {
    if env_override {
        return Err(String::from("--publish with BITSOS_UPDATE_PUBKEY_FILE set: a release is built only with the key in git"));
    }
    if pubkey != in_git {
        return Err(format!("--publish with --pubkey-file {}: a release is built only with {}", pubkey.display(), in_git.display()));
    }
    if !tracked || !unchanged {
        return Err(format!("--publish: {} is not committed as it is (commit the desk's public key first)", in_git.display()));
    }
    Ok(())
}

fn check_publish_key(root: &Path, o: &Opts) -> Result<(), String> {
    let rel = "user/lib/bitsupdate/keys/release.pub";
    let in_git = absolute(&root.join(rel))?;
    let git = |args: &[&str]| Command::new("git").current_dir(root).args(args).output().map(|o| o.status.success()).unwrap_or(false);
    let tracked = git(&["ls-files", "--error-unmatch", rel]);
    let unchanged = git(&["diff", "--quiet", "HEAD", "--", rel]);
    publish_key_ok(&absolute(&o.pubkey)?, &in_git, std::env::var_os("BITSOS_UPDATE_PUBKEY_FILE").is_some(), tracked, unchanged)
}

/// bitsupdate for this host, built as image.rs builds the user workspace (its own stable toolchain: plain cargo
/// from its folder, without the pinned nightly's RUSTUP_TOOLCHAIN) into a target folder of its own.
fn bitsupdate(root: &Path) -> Result<PathBuf, String> {
    let tdir = root.join("target/user-host");
    let mut c = Command::new("cargo");
    c.current_dir(root.join("user")).env_remove("RUSTUP_TOOLCHAIN").env_remove("CARGO_TARGET_DIR");
    c.args(["build", "-q", "--offline", "-p", "bitsupdate", "--target-dir"]).arg(&tdir);
    run_status(&mut c, "building bitsupdate (the signing tool)")?;
    let exe = tdir.join("debug/bitsupdate");
    exe.is_file().then_some(exe.clone()).ok_or_else(|| format!("{} missing after the build", exe.display()))
}

fn run_tool(exe: &Path, args: &[&str]) -> Result<String, String> {
    let o = Command::new(exe).args(args).output().map_err(|e| format!("{}: {e}", exe.display()))?;
    if !o.status.success() {
        return Err(format!("bitsupdate {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The key pair: made once when missing; the public half must be the one BitsOS is built with.
fn key(root: &Path, o: &Opts, tool: &Path) -> Result<PathBuf, String> {
    let key = o.key_dir.join("release.key");
    let made_pub = o.key_dir.join("release.pub");
    if !key.exists() {
        let fp = run_tool(tool, &["keygen", &o.key_dir.display().to_string()])?;
        println!("update-release: {fp} (in {}; the private half never leaves this host)", o.key_dir.display());
        let built_in = std::fs::read_to_string(&o.pubkey).unwrap_or_default();
        if pub_hex(&built_in).is_none() {
            std::fs::copy(&made_pub, &o.pubkey).map_err(|e| format!("{}: {e}", o.pubkey.display()))?;
            println!("update-release: {} now holds the public half: commit it, BitsOS is built with it", o.pubkey.strip_prefix(root).unwrap_or(&o.pubkey).display());
        }
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&key).map_err(|e| format!("{}: {e}", key.display()))?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!("{} is readable by others (mode {mode:o}): chmod 600 it first", key.display()));
    }
    let have = pub_hex(&std::fs::read_to_string(&made_pub).map_err(|e| format!("{}: {e}", made_pub.display()))?);
    let built = pub_hex(&std::fs::read_to_string(&o.pubkey).map_err(|e| format!("{}: {e}", o.pubkey.display()))?);
    if have.is_none() || have != built {
        return Err(format!(
            "the key in {} is not the one BitsOS is built with ({}): BitsOS would refuse this release",
            o.key_dir.display(),
            o.pubkey.display()
        ));
    }
    Ok(key)
}

pub fn cmd_update_release(root: &Path, mut a: Args) -> Result<(), String> {
    if a.flag("--help") {
        println!("{HELP}");
        return Ok(());
    }
    let o = parse(root, &mut a)?;
    a.positionals()?;
    if std::env::var_os("V2_FW_INITRD").is_some() {
        return Err(String::from("V2_FW_INITRD is set: no firmware file goes into a release"));
    }
    let notes = std::fs::read(&o.notes).map_err(|e| format!("{}: {e}", o.notes.display()))?;
    notes_ok(&notes)?;
    check_out(root, &o)?;
    if o.publish {
        check_publish_key(root, &o)?;
    }
    let tool = bitsupdate(root)?;
    let key = key(root, &o, &tool)?;

    // The image with the number in it (a child xtask, so the number reaches every cargo build it starts).
    let date = String::from_utf8_lossy(&Command::new("date").args(["-u", "+%F"]).output().map_err(|e| format!("date: {e}"))?.stdout).trim().to_string();
    let name = format!("upd-{}", o.version);
    let me = std::env::current_exe().map_err(|e| format!("xtask itself: {e}"))?;
    let stick_list = xdir(root).join(format!("{name}.sticklist"));
    let mut img = Command::new(me);
    img.env("V2_STICK_LIST", &stick_list);
    img.args(["image", "--name", &name]).env("BITSOS_VERSION", o.version.to_string()).env("BITSOS_VERSION_DATE", &date).env("BITSOS_UPDATE_PUBKEY_FILE", &o.pubkey).env_remove("V2_FW_INITRD");
    if !o.full {
        img.env("V2_LEAN", "1");
    }
    if o.release {
        img.arg("--release");
    }
    if o.ktest {
        img.arg("--ktest");
    }
    run_status(&mut img, "building the image of this version")?;
    let esp = xdir(root).join(format!("esp-{name}"));

    // Checked again right before (the key may have been made in this run).
    check_out(root, &o)?;
    let _ = std::fs::remove_dir_all(&o.out);
    std::fs::create_dir_all(&o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    std::fs::write(o.out.join(OUT_MARK), "made by cargo xtask update-release; cleared by its next run\n").map_err(|e| e.to_string())?;
    let k = o.out.join("KERNEL");
    run_status(Command::new("objcopy").arg("--strip-debug").arg(esp.join("BITSOS2/SYS/1/KERNEL")).arg(&k), "objcopy --strip-debug (the release kernel)")?;
    let kb = std::fs::read(&k).map_err(|e| e.to_string())?;
    if !contains(&kb, GUARD_MARK) || !contains(&kb, &version_mark(o.version)) {
        return Err(String::from("the stripped kernel lost the disk guard's or the version's mark"));
    }
    for (from, to) in [(esp.join("BITSOS2/SYS/1/INITRD"), "INITRD"), (esp.join("EFI/BOOT/BOOTX64.EFI"), "BOOTX64.EFI")] {
        std::fs::copy(&from, o.out.join(to)).map_err(|e| format!("{}: {e}", from.display()))?;
    }
    std::fs::write(o.out.join("WASISTNEU.TXT"), &notes).map_err(|e| e.to_string())?;
    for l in crate::update_parts::write(&esp, &stick_list, &o.parts, &o.out)? {
        println!("update-release: {l}");
    }
    let out = o.out.display().to_string();
    let mut margs = vec!["manifest", &out, &date];
    let min = o.min_previous.map(|m| m.to_string());
    if let Some(m) = &min {
        margs.push(m);
    }
    let vs = o.version.to_string();
    margs.insert(1, &vs);
    println!("update-release: {}", run_tool(&tool, &margs)?);
    margs[0] = "manifest2";
    println!("update-release: {}", run_tool(&tool, &margs)?);
    for m in ["MANIFEST", "MANIFEST2"] {
        println!("update-release: {}", run_tool(&tool, &["sign", &key.display().to_string(), &o.out.join(m).display().to_string()])?);
    }
    println!("update-release: {}", run_tool(&tool, &["verify", &o.pubkey.display().to_string(), &out])?);
    let gh = gh_args(&o, &part_assets(&o.out)?);
    if !o.publish {
        println!("update-release: dry run, nothing uploaded. To publish: gh {}", gh.join(" "));
        return Ok(());
    }
    let exists = Command::new("gh").args(["release", "view", &format!("version-{}", o.version), "--repo", REPO]).output().map_err(|e| format!("gh: {e}"))?;
    if exists.status.success() {
        return Err(format!("version-{} exists already in {REPO}: a version is published once", o.version));
    }
    run_status(Command::new("gh").args(&gh), "gh release create")?;
    println!("update-release: version-{} published to {REPO}", o.version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_notes_and_the_gh_line() {
        assert_eq!(version_number("164"), Ok(164));
        for bad in ["163", "0", "-1", "16a", "", "100000000"] {
            assert!(version_number(bad).is_err(), "{bad}");
        }
        assert!(notes_ok("Was ist neu:\n- Aktualisieren per Knopf\n".as_bytes()).is_ok());
        for bad in [&b""[..], b"  \n", b"a\tb", b"a\r\nb", b"\xff", b"\x1b[31mrot"] {
            assert!(notes_ok(bad).is_err(), "{bad:?}");
        }
        let o = Opts {
            version: 165,
            notes: "n".into(),
            min_previous: None,
            out: "/o".into(),
            key_dir: "/k".into(),
            pubkey: "/p".into(),
            release: false,
            ktest: false,
            full: false,
            publish: false,
            parts: crate::update_parts::PartsOpts { trees: crate::update_parts::Trees::All, bin_files: vec![], extra_trees: vec![] },
        };
        let g = gh_args(&o, &["bin.bitscomp".into(), format!("tree.mesa.{}.1.tar.zst", "a".repeat(16))]).join(" ");
        assert!(g.starts_with("release create version-165 --repo georgwinter89-cloud/bitsos-werkstatt --title Version 165 --notes-file /o/WASISTNEU.TXT /o/KERNEL"), "{g}");
        assert!(g.contains("/o/MANIFEST /o/MANIFEST.SIG /o/MANIFEST2 /o/MANIFEST2.SIG /o/bin.bitscomp /o/tree.mesa."), "{g}");
        assert!(!g.contains("/k"), "the key folder never goes into the release");
        assert_eq!(pub_hex("# c\nnone\n"), None);
        assert_eq!(pub_hex(&format!("# c\n{}\n", "AB".repeat(32))), Some("ab".repeat(32)));
        assert_eq!(version_mark(165), b"BITSOS-VERSION=00000165;\xb7\n".to_vec());
        assert_eq!(version_mark(165).len(), bitsver::MARK_LEN);
    }

    #[test]
    fn the_out_folder_never_takes_the_key_with_it() {
        let key = Path::new("/home/d/.config/bitsos-release");
        let def = Path::new("/r/target/xtask/update-165");
        for out in ["/home/d/.config/bitsos-release", "/home/d/.config", "/home/d", "/", "/home/d/.config/bitsos-release/x"] {
            let r = out_ok(Path::new(out), key, def, true, false, true);
            assert!(r.as_ref().is_err_and(|t| t.contains("key folder")), "{out}: {r:?}");
        }
        // A folder with foreign files is refused; new, empty, marked or the default one is fine.
        assert!(out_ok(Path::new("/r/src"), key, def, true, false, false).is_err());
        for (exists, empty, marked) in [(false, false, false), (true, true, false), (true, false, true)] {
            assert!(out_ok(Path::new("/r/target/rel"), key, def, exists, empty, marked).is_ok());
        }
        assert!(out_ok(def, key, def, true, false, false).is_ok());
    }

    #[test]
    fn a_published_release_trusts_only_the_key_in_git() {
        let git = Path::new("/r/user/lib/bitsupdate/keys/release.pub");
        assert!(publish_key_ok(git, git, false, true, true).is_ok());
        let test_key = Path::new("/r/target/xtask/runs/rb-56-update/key/release.pub");
        assert!(publish_key_ok(test_key, git, false, true, true).is_err_and(|t| t.contains("--pubkey-file")));
        assert!(publish_key_ok(git, git, true, true, true).is_err_and(|t| t.contains("BITSOS_UPDATE_PUBKEY_FILE")));
        assert!(publish_key_ok(git, git, false, false, true).is_err_and(|t| t.contains("not committed")));
        assert!(publish_key_ok(git, git, false, true, false).is_err_and(|t| t.contains("not committed")));
    }

    #[test]
    fn absolute_paths_of_folders_not_made_yet() {
        let here = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(absolute(Path::new("no-such-dir-xyz/a")).unwrap(), here.join("no-such-dir-xyz/a"));
        assert!(absolute(Path::new("no-such-dir-xyz/../../x")).is_err());
    }
}
