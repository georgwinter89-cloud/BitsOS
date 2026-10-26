//! `bitspkg selftest DIR`: the install/upgrade/rollback/remove round trip in a scratch folder, so the same check
//! runs on V2 (ktest ul.tools.cases, user/tools/cases.txt) as on CachyOS: hard links, RENAME_EXCHANGE, flock,
//! O_NOFOLLOW and read-only folders on the file system DIR is on; then PK-2's removal without leftovers
//! (clean.rs: settings keys through a private settingsd, rules and actions, a program that writes elsewhere).

use crate::install::{self, Outcome};
use crate::store::Store;
use crate::history::{self, SnapRoot, Step};
use crate::{build, fsx, remove, verify};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const SPEC: &str = "format = 1\nname = \"selftest\"\nversion = \"V\"\nprogram = \"bin/run\"\n";

fn tree(dir: &Path, v: &str) -> Result<std::path::PathBuf, String> {
    let t = dir.join(format!("tree-{v}"));
    let put = |p: &str, data: &str, mode: u32| -> Result<(), String> {
        let f = t.join(p);
        fs::create_dir_all(f.parent().unwrap_or(&t)).map_err(|e| fsx::err(&f, e))?;
        fs::write(&f, data).map_err(|e| fsx::err(&f, e))?;
        fs::set_permissions(&f, fs::Permissions::from_mode(mode)).map_err(|e| fsx::err(&f, e))
    };
    put("package.toml", &SPEC.replace('V', v), 0o644)?;
    put("bin/run", &format!("#!/bin/sh\necho {v}\n"), 0o755)?;
    put("lib/libsame.so.1", "same in every version\n", 0o644)?;
    std::os::unix::fs::symlink("libsame.so.1", t.join("lib/libsame.so")).map_err(|e| fsx::err(&t, e))?;
    Ok(t)
}

fn need(ok: bool, what: &str) -> Result<(), String> {
    if ok {
        println!("ok   {what}");
        Ok(())
    } else {
        Err(format!("selftest: {what}: failed"))
    }
}

pub fn run(dir: &Path, allow_copy: bool) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| fsx::err(dir, e))?;
    let work = dir.join("bitspkg-selftest");
    fsx::remove_tree(&work)?;
    fs::create_dir(&work).map_err(|e| fsx::err(&work, e))?;
    let st = Store::new(&work.join("root"));
    let mut pkgs = Vec::new();
    for v in ["1", "2"] {
        let out = work.join(format!("pkg-{v}"));
        build::build(&tree(&work, v)?, None, Some(&out))?;
        pkgs.push(out);
    }
    let prog = |st: &Store| fs::read_to_string(st.pkg("selftest").join("current/bin/run")).unwrap_or_default();
    let r1 = install::install(&st, &pkgs[0])?;
    need(r1.outcome == Outcome::Installed && prog(&st).contains("echo 1"), "install 1")?;
    let r2 = install::install(&st, &pkgs[1])?;
    need(prog(&st).contains("echo 2") && r2.previous.as_deref() == Some("1"), "upgrade to 2, 1 kept")?;
    let ino = |v: &str| fs::metadata(st.version_dir("selftest", v).join("lib/libsame.so.1")).map(|m| m.ino()).ok();
    if r1.copied + r2.copied > 0 {
        need(allow_copy, "hard links refused here (files copied); pass --allow-copy where that is expected")?;
    } else {
        need(ino("1").is_some() && ino("1") == ino("2"), "identical file shared by hard link")?;
    }
    let mode = fs::metadata(st.version_dir("selftest", "2").join("bin")).map(|m| m.mode() & 0o777).unwrap_or(0);
    need(mode == 0o555, "version folders read-only")?;
    let a = work.join("xa");
    let b = work.join("xb");
    std::os::unix::fs::symlink("1", &a).map_err(|e| fsx::err(&a, e))?;
    std::os::unix::fs::symlink("2", &b).map_err(|e| fsx::err(&b, e))?;
    let swapped = fsx::exchange(&a, &b).is_ok() && fsx::read_link(&a).as_deref() == Some("2");
    need(swapped, "RENAME_EXCHANGE swaps two symlinks")?;
    let (sp, _) = crate::space::report(&st, Some("selftest"))?;
    let v = &sp[0].versions;
    let same = "same in every version\n".len() as u64;
    need(v.len() == 2 && v[0].size.bytes - v[0].alone.bytes == same && sp[0].package.bytes + same == v[0].size.bytes + v[1].size.bytes, "space counts a shared file once")?;
    install::rollback(&st, "selftest")?;
    need(prog(&st).contains("echo 1") && st.previous("selftest").as_deref() == Some("2"), "rollback to 1")?;
    let (bad, c) = verify::verify_counted(&st, None, true)?;
    need(bad.is_empty() && c.versions == 2 && c.files == 4 && c.store_files == 3, "verify all versions and the store")?;
    // B1 (PK-5): a copy of the volume as a snapshot holds it: current 1, previous 2.
    let snap = work.join("snap");
    fs::create_dir(&snap).map_err(|e| fsx::err(&snap, e))?;
    copy_tree(&st.apps, &snap.join("apps"))?;
    copy_tree(&st.data, &snap.join("appdata"))?;
    let lib = st.version_dir("selftest", "1").join("lib/libsame.so.1");
    need(fs::OpenOptions::new().append(true).open(&lib).is_err() || is_root(), "package files not writable")?;
    install::switch(&st, "selftest", "2")?;
    remove::remove_version(&st, "selftest", "1")?;
    need(prog(&st).contains("echo 2") && verify::verify(&st, None, true)?.is_empty(), "remove version 1, 2 intact")?;
    let at = history::program_at(&SnapRoot::volume(&snap), &st, "selftest")?;
    need(at.step == Step::Install(snap.join("apps/selftest/1")) && at.data_then.is_some(), "B1: snapshot names version 1, gone live")?;
    history::bring_back(&st, &at)?;
    let ino1 = ino("1");
    need(prog(&st).contains("echo 1") && (r1.copied + r2.copied > 0 || (ino1.is_some() && ino1 == ino("2"))), "B1: version 1 back from the snapshot, linked to the store")?;
    need(verify::verify(&st, None, true)?.is_empty(), "B1: verify clean after bringing it back")?;
    install::rollback(&st, "selftest")?;
    need(prog(&st).contains("echo 2"), "B1: rollback undoes it")?;
    remove::remove(&st, "selftest", false)?;
    remove::gc(&st)?;
    let left: Vec<String> = fs::read_dir(st.content()).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into()).collect()).unwrap_or_default();
    need(!st.pkg("selftest").exists() && !st.data_dir("selftest").exists() && left.is_empty(), "remove leaves no package, data or store file")?;
    fsx::remove_tree(&work)?;
    for line in crate::clean::round_trip(&crate::clean::work_in(dir))? {
        println!("ok   {line}");
    }
    println!("selftest ok");
    Ok(())
}

/// A plain copy that keeps symlinks, modes and (within the copy) nothing else: the stand-in for a mounted snapshot.
/// Folders get their mode after their content (0555 folders).
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let md = fs::symlink_metadata(from).map_err(|e| fsx::err(from, e))?;
    if md.file_type().is_symlink() {
        let t = fs::read_link(from).map_err(|e| fsx::err(from, e))?;
        return std::os::unix::fs::symlink(t, to).map_err(|e| fsx::err(to, e));
    }
    if md.is_file() {
        fs::copy(from, to).map_err(|e| fsx::err(to, e))?;
        return fs::set_permissions(to, fs::Permissions::from_mode(md.mode() & 0o7777)).map_err(|e| fsx::err(to, e));
    }
    if !md.is_dir() {
        return Ok(());
    }
    fs::create_dir(to).map_err(|e| fsx::err(to, e))?;
    for e in fs::read_dir(from).map_err(|e| fsx::err(from, e))?.flatten() {
        copy_tree(&e.path(), &to.join(e.file_name()))?;
    }
    fs::set_permissions(to, fs::Permissions::from_mode(md.mode() & 0o7777)).map_err(|e| fsx::err(to, e))
}

/// Root may write a 0444 file (CAP_DAC_OVERRIDE); the check only means something for other users.
fn is_root() -> bool {
    usys::geteuid() == Ok(0)
}
