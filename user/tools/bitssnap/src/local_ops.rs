//! The desktop socket's operations on the real pool (local.rs has the rules who may ask what). Previews (the
//! dry runs) say what a restore would change: the path's size then and now, a program's version and data folder
//! then and now, the generation and the snapshot "everything as at" would bring back.

use crate::dev::Dev;
use crate::local::Ops;
use crate::restore::{self, DATA, below_data, in_snapshot, program, volume, with_snapshot};
use crate::serve::{DATA_STATE, DataState, data_state};
use crate::{local_time, restore_tool};
use bitspkg::store::Store;
use mcpreg::Json;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The desktop user's group (bitscomp runs as 1000:1000, `local::OWNER`).
const OWNER_GID: u32 = 1000;
/// At most this many entries of one folder are listed (the rest: `more`).
const ENTRIES_MAX: usize = 1000;
/// A preview counts at most this many entries of a tree, then and now each (the rest: `more`, "mehr als" in the
/// dialog). REWIND stutter (Georg 13.11.2026): "Diesen Ordner zurückholen" for a home of 260,000 entries counted
/// up to 200,000 a side, every one a lookup of its own (in the snapshot a fresh inode of its own mount); the
/// kernel's caches grew by that much and its compaction ran for a minute. A bound keeps a dry run short and small.
pub const TALLY_MAX: u64 = 20_000;

pub struct DevOps;

fn n(x: u64) -> Json {
    Json::Num(x as f64)
}

fn dev() -> Result<Dev, String> {
    Dev::open().map_err(|e| e.to_string())
}

/// Whether `uid` (with the desktop's group) could list folder `m`: r and x for it by owner, group or others.
pub fn may_list(m: &fs::Metadata, uid: u32) -> bool {
    if !m.is_dir() {
        return false;
    }
    if uid == 0 {
        return true;
    }
    let mode = m.mode();
    let bits = if m.uid() == uid {
        mode >> 6
    } else if m.gid() == OWNER_GID {
        mode >> 3
    } else {
        mode
    };
    bits & 0o5 == 0o5
}

/// Every folder from `root` down to `rel`'s parent (`rel` itself too with `whole`) is a real folder `uid` could
/// list; the path of the last one checked.
fn open_for(root: &Path, rel: &str, uid: u32, whole: bool) -> Result<PathBuf, String> {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let upto = if whole { parts.len() } else { parts.len().saturating_sub(1) };
    let mut cur = root.to_path_buf();
    let check = |p: &Path| match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_dir() && may_list(&m, uid) => Ok(()),
        Ok(m) if m.file_type().is_dir() => Err("a folder that is not yours to look into".to_string()),
        Ok(_) => Err("not a folder (a link is never followed)".to_string()),
        Err(e) => Err(format!("not there ({e})")),
    };
    check(&cur)?;
    for p in &parts[..upto] {
        cur.push(p);
        check(&cur)?;
    }
    Ok(cur)
}

/// Files, folders, links and bytes of the tree at `p` (never through a link), at most `TALLY_MAX` entries; None
/// when nothing is there. The kinds come from the folder's own entries (d_type) and only files are looked at,
/// relative to their folder (`DirEntry::metadata`: fstatat, not a walk from / per entry); `seen` is what was looked at.
/// The walk runs on a thread of the Background class (SCHED_IDLE, as ctxd's indexing): a game in front does not
/// feel it.
pub fn tally(p: &Path) -> Option<Json> {
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = usys::sched_setscheduler(0, usys::SCHED_IDLE, 0);
            tally_upto(p, TALLY_MAX)
        })
        .join()
        .unwrap_or(None)
    })
}

fn tally_upto(p: &Path, max: u64) -> Option<Json> {
    let top = fs::symlink_metadata(p).ok()?;
    let (mut files, mut dirs, mut links, mut bytes, mut seen, mut more) = (0u64, 0u64, 0u64, 0u64, 1u64, false);
    let ft = top.file_type();
    let kind = if ft.is_dir() { "folder" } else if ft.is_symlink() { "link" } else { "file" };
    if ft.is_symlink() {
        links = 1;
    } else if ft.is_file() {
        (files, bytes) = (1, top.size());
    } else if ft.is_dir() {
        dirs = 1;
        let mut todo = vec![p.to_path_buf()];
        'walk: while let Some(d) = todo.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                if seen >= max {
                    more = true;
                    break 'walk;
                }
                seen += 1;
                let Ok(t) = e.file_type() else { continue };
                if t.is_symlink() {
                    links += 1;
                } else if t.is_dir() {
                    dirs += 1;
                    todo.push(e.path());
                } else if t.is_file() {
                    files += 1;
                    bytes += e.metadata().map_or(0, |m| m.size());
                }
            }
        }
    }
    Some(Json::obj(vec![("kind", Json::str(kind)), ("files", n(files)), ("dirs", n(dirs)), ("links", n(links)), ("bytes", n(bytes)), ("more", Json::Bool(more)), ("seen", n(seen))]))
}

/// What the tallies of a dry run's answer looked at, together.
pub fn seen(j: &Json) -> u64 {
    ["then", "now", "data_then", "data_now"].iter().filter_map(|k| j.get(k)?.get("seen")?.as_u64()).sum()
}

/// Whether the entry `name` in folder `dir` is `uid`'s to restore: owned by it, and a folder it could list (a
/// link counts as the link, never followed). `must`: it has to be there (in the snapshot); live it may be new.
fn owned(dir: &Path, name: &str, uid: u32, must: bool) -> Result<(), String> {
    if uid == 0 {
        return Ok(());
    }
    match fs::symlink_metadata(dir.join(name)) {
        Ok(m) if m.uid() != uid => Err(format!("{name}: not yours (owned by uid {})", m.uid())),
        Ok(m) if m.file_type().is_dir() && !may_list(&m, uid) => Err(format!("{name}: a folder that is not yours to look into")),
        Ok(_) => Ok(()),
        Err(_) if !must => Ok(()),
        Err(e) => Err(format!("{name}: not there ({e})")),
    }
}

/// `root/<rel>` on the live volume for `uid`: None when it or a parent is not there (a restore makes it new),
/// Err when a parent is a link or a folder `uid` could not list, or the entry is not `uid`'s (`owned`). For root
/// a planted link names nothing (None), as before.
fn live(root: &Path, rel: &str, uid: u32) -> Result<Option<PathBuf>, String> {
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let mut cur = root.to_path_buf();
    for p in std::iter::once(None).chain(dir.split('/').filter(|s| !s.is_empty()).map(Some)) {
        if let Some(p) = p {
            cur.push(p);
        }
        match fs::symlink_metadata(&cur) {
            Err(_) => return Ok(None),
            Ok(m) if m.file_type().is_dir() && may_list(&m, uid) => {}
            Ok(_) if uid == 0 => return Ok(None),
            Ok(m) if m.file_type().is_dir() => return Err(format!("{}: a folder that is not yours to look into", cur.display())),
            Ok(_) => return Err(format!("{}: not a folder (a link is never followed)", cur.display())),
        }
    }
    owned(&cur, name, uid, false)?;
    let p = cur.join(name);
    Ok(fs::symlink_metadata(&p).is_ok().then_some(p))
}

/// The tree at `/data/<rel>` now as `uid` may see it (`live`).
fn now_tally(rel: &str, uid: u32) -> Result<Json, String> {
    Ok(live(Path::new(DATA), rel, uid)?.and_then(|p| tally(&p)).unwrap_or(Json::Null))
}

/// For `uid` (not root) the target of a restore-path is its own in the snapshot (every parent one it could list,
/// the entry itself owned by it) and on the live volume, so a root-only folder next to its own is never brought
/// back and never counted for it (local.rs has already kept it to its own folder).
fn restorable(snap_root: &Path, live_root: &Path, rel: &str, uid: u32) -> Result<(), String> {
    let dir = open_for(snap_root, rel, uid, false)?;
    owned(&dir, rel.rsplit('/').next().unwrap_or(rel), uid, true)?;
    live(live_root, rel, uid).map(|_| ())
}

/// The program names with a package or a data folder in the snapshot at `snap` (`apps`/`appdata`: below /data).
fn program_names(snap: &Path, apps: Option<&str>, appdata: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    for rel in [apps, appdata].into_iter().flatten() {
        let Ok(d) = in_snapshot(snap, rel) else { continue };
        let Ok(rd) = fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let ok_dir = fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_dir());
            if let (true, Some(name)) = (ok_dir, e.file_name().to_str()) {
                if bitspkg::names::check_name(name).is_ok() && !names.iter().any(|x| x == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

impl Ops for DevOps {
    fn list(&mut self) -> Result<Json, String> {
        let state = data_state(fs::read_to_string(DATA_STATE).ok().as_deref());
        let word = match &state {
            DataState::Ready => "ready",
            DataState::Pending => "pending",
            DataState::Never(_) => "never",
        };
        if state != DataState::Ready {
            let why = if let DataState::Never(w) = &state { w.clone() } else { String::new() };
            return Ok(Json::obj(vec![("data", Json::str(word)), ("why", Json::str(&why)), ("snapshots", Json::Arr(Vec::new()))]));
        }
        let mut d = dev()?;
        let st = d.stats().map_err(|e| e.to_string())?;
        let mut l = d.list(false).map_err(|e| e.to_string())?;
        l.sort_by_key(|s| std::cmp::Reverse((s.created_ns, s.id)));
        let off = st.utc_offset_min * 60;
        let snaps = l
            .iter()
            .map(|s| {
                let t = s.created_ns / 1_000_000_000;
                Json::obj(vec![("id", n(s.id)), ("created_s", n(t)), ("time", Json::str(&local_time(t, off))), ("kind", Json::str(s.kind())), ("label", Json::str(&s.label))])
            })
            .collect();
        let now = if st.wall_ns > 0 { st.wall_ns / 1_000_000_000 } else { restore::now_s() };
        let pending = l.iter().find(|s| s.rollback).map_or(Json::Null, |s| n(s.id));
        Ok(Json::obj(vec![
            ("data", Json::str(word)),
            ("snapshots", Json::Arr(snaps)),
            ("utc_offset_min", Json::Num(st.utc_offset_min as f64)),
            ("now_s", n(now)),
            ("rollback_pending", pending),
        ]))
    }

    fn browse(&mut self, id: u64, path: &str, uid: u32) -> Result<Json, String> {
        let rel = if path.trim_end_matches('/') == DATA { String::new() } else { below_data(path)? };
        with_snapshot(&mut dev()?, id, |_, root| {
            let dir = open_for(root, &rel, uid, true).map_err(|e| format!("{path} in snapshot {id}: {e}"))?;
            // Names and kinds first (the folder's own entries); only the entries shown are looked at.
            let mut names: Vec<(bool, String, fs::DirEntry)> = Vec::new();
            let rd = fs::read_dir(&dir).map_err(|e| format!("{path}: {e}"))?;
            for e in rd.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
                let folder = e.file_type().is_ok_and(|t| t.is_dir());
                names.push((folder, name, e));
            }
            names.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
            let more = names.len() > ENTRIES_MAX;
            let mut entries = Vec::new();
            for (_, name, e) in names.into_iter().take(ENTRIES_MAX) {
                let Ok(m) = e.metadata() else { continue };
                let ft = m.file_type();
                let kind = if ft.is_dir() { "folder" } else if ft.is_symlink() { "link" } else if ft.is_file() { "file" } else { "other" };
                let open = ft.is_dir() && may_list(&m, uid);
                let mut v = vec![("name", Json::str(&name)), ("kind", Json::str(kind)), ("open", Json::Bool(open))];
                if ft.is_file() {
                    v.push(("bytes", n(m.size())));
                }
                entries.push(Json::obj(v));
            }
            let shown = if rel.is_empty() { DATA.to_string() } else { format!("{DATA}/{rel}") };
            Ok(Json::obj(vec![
                ("path", Json::str(&shown)),
                ("entries", Json::Arr(entries)),
                ("more", Json::Bool(more)),
            ]))
        })
    }

    fn programs(&mut self, id: u64) -> Result<Json, String> {
        let st = Store::new(Path::new("/"));
        let apps = program::on_data(&st.apps);
        let appdata = program::on_data(&st.data_dir("x")).and_then(|r| r.rsplit_once('/').map(|(p, _)| p.to_string()));
        with_snapshot(&mut dev()?, id, |_, snap| {
            let list = program_names(snap, apps.as_deref(), appdata.as_deref())
                .into_iter()
                .map(|name| {
                    let then = apps.as_deref().and_then(|a| program::snapshot_version(snap, a, &name)).map(|(v, _)| v);
                    let data = appdata.as_deref().is_some_and(|a| in_snapshot(snap, &format!("{a}/{name}")).is_ok());
                    let now = st.current(&name);
                    Json::obj(vec![
                        ("name", Json::str(&name)),
                        ("version", then.as_deref().map_or(Json::Null, Json::str)),
                        ("now", now.as_deref().map_or(Json::Null, Json::str)),
                        ("data", Json::Bool(data)),
                    ])
                })
                .collect();
            Ok(Json::obj(vec![("programs", Json::Arr(list))]))
        })
    }

    fn restore_path(&mut self, id: u64, path: &str, uid: u32, dry: bool) -> Result<Json, String> {
        let rel = below_data(path)?;
        let mut d = dev()?;
        let then = with_snapshot(&mut d, id, |_, root| {
            restorable(root, Path::new(DATA), &rel, uid).map_err(|e| format!("{path} in snapshot {id}: {e}"))?;
            let src = in_snapshot(root, &rel).map_err(|e| format!("{path}: not in snapshot {id} ({e})"))?;
            Ok(if dry { tally(&src) } else { None })
        })?;
        if dry {
            return Ok(Json::obj(vec![("path", Json::str(&format!("{DATA}/{rel}"))), ("then", then.unwrap_or(Json::Null)), ("now", now_tally(&rel, uid)?)]));
        }
        let r = restore::restore(&mut d, id, path)?;
        Ok(restore_tool::report_json(&r))
    }

    fn restore_program(&mut self, id: u64, name: &str, dry: bool) -> Result<Json, String> {
        bitspkg::names::check_name(name)?;
        let st = Store::new(Path::new("/"));
        let mut d = dev()?;
        if !dry {
            let r = program::restore_program(&mut d, id, name, Path::new("/"))?;
            return Ok(Json::obj(vec![
                ("program", Json::str(&r.name)),
                ("from", n(r.from)),
                ("before", n(r.before)),
                ("version", r.version.as_deref().map_or(Json::Null, Json::str)),
                ("was", r.was.as_deref().map_or(Json::Null, Json::str)),
                ("data", r.data.as_ref().map_or(Json::Null, restore_tool::report_json)),
                ("text", Json::str(&r.text())),
            ]));
        }
        let (apps, data) = (program::on_data(&st.apps), program::on_data(&st.data_dir(name)));
        with_snapshot(&mut d, id, |_, snap| {
            let then = apps.as_deref().and_then(|a| program::snapshot_version(snap, a, name)).map(|(v, _)| v);
            let data_then = data.as_deref().and_then(|r| in_snapshot(snap, r).ok()).and_then(|p| tally(&p));
            if then.is_none() && data_then.is_none() {
                return Err(format!("{name}: neither a package version nor a data folder in snapshot {id}"));
            }
            Ok(Json::obj(vec![
                ("program", Json::str(name)),
                ("version", then.as_deref().map_or(Json::Null, Json::str)),
                ("now", st.current(name).as_deref().map_or(Json::Null, Json::str)),
                ("data_then", data_then.unwrap_or(Json::Null)),
                ("data_now", data.as_deref().and_then(|r| now_tally(r, 0).ok()).unwrap_or(Json::Null)),
                ("data_path", data.as_deref().map_or(Json::Null, |r| Json::str(&format!("{DATA}/{r}")))),
            ]))
        })
    }

    fn restore_system(&mut self, at: i64, dry: bool) -> Result<Json, String> {
        let mut d = dev()?;
        let off = d.stats().map_err(|e| e.to_string())?.utc_offset_min * 60;
        let r = volume::restore_system(&mut d, at, dry)?;
        let s = r.snapshot_ns / 1_000_000_000;
        Ok(Json::obj(vec![
            ("dry_run", Json::Bool(dry)),
            ("snapshot", n(r.snapshot)),
            ("snapshot_s", n(s)),
            ("snapshot_time", Json::str(&local_time(s, off))),
            ("pin", if r.pin == 0 { Json::Null } else { n(r.pin.into()) }),
            ("running", n(r.running.into())),
            ("next", n(r.next.into())),
            ("replaced", if r.replaced == 0 { Json::Null } else { n(r.replaced) }),
            ("note", Json::str(&r.note)),
        ]))
    }

    fn undo(&mut self) -> Result<Json, String> {
        let (done, text) = volume::undo_system(&mut dev()?)?;
        Ok(Json::obj(vec![("undone", Json::Bool(done)), ("text", Json::str(&text))]))
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(what: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitssnap-local-{what}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn listing_rights_by_owner_group_and_others() {
        let d = scratch("rights");
        let me = fs::metadata(&d).unwrap().uid();
        let mode = |m: u32| {
            fs::set_permissions(&d, fs::Permissions::from_mode(m)).unwrap();
            fs::metadata(&d).unwrap()
        };
        assert!(may_list(&mode(0o700), me) && may_list(&mode(0o500), me));
        assert!(!may_list(&mode(0o300), me), "x without r: not listed");
        assert!(!may_list(&mode(0o600), me), "r without x: not listed");
        // Another uid: the group's bits when the folder is the desktop's group, else the others'.
        let (other, grp) = (me + 1, fs::metadata(&d).unwrap().gid() == OWNER_GID);
        assert_eq!(may_list(&mode(0o750), other), grp);
        assert_eq!(may_list(&mode(0o705), other), !grp);
        assert!(may_list(&mode(0o000), 0), "root lists everything");
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(d.join("f"), b"x").unwrap();
        assert!(!may_list(&fs::metadata(d.join("f")).unwrap(), 0), "a file is never a folder to list");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_walk_never_goes_through_a_link_or_a_closed_folder() {
        let d = scratch("walk");
        let me = fs::metadata(&d).unwrap().uid();
        fs::create_dir_all(d.join("home/u/docs")).unwrap();
        fs::write(d.join("home/u/docs/a.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("/etc", d.join("home/u/etc")).unwrap();
        assert_eq!(open_for(&d, "home/u/docs", me, true).unwrap(), d.join("home/u/docs"));
        assert!(open_for(&d, "home/u/etc", me, true).unwrap_err().contains("link is never followed"));
        assert!(open_for(&d, "home/u/etc/passwd", me, false).is_err(), "a parent that is a link");
        assert!(open_for(&d, "home/u/missing/x", me, false).unwrap_err().contains("not there"));
        fs::set_permissions(d.join("home/u"), fs::Permissions::from_mode(0o300)).unwrap();
        assert!(open_for(&d, "home/u/docs/a.txt", me + 1, false).is_err(), "a folder another uid cannot list");
        fs::set_permissions(d.join("home/u"), fs::Permissions::from_mode(0o755)).unwrap();
        let t = tally(&d.join("home")).unwrap();
        let g = |k: &str| t.get(k).and_then(Json::as_u64);
        assert_eq!((g("files"), g("dirs"), g("links"), g("bytes")), (Some(1), Some(3), Some(1), Some(5)), "{t}");
        assert_eq!(tally(&d.join("home/u/docs/a.txt")).unwrap().get("kind").and_then(Json::as_str), Some("file"));
        assert!(tally(&d.join("nothing")).is_none());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_dry_run_counts_a_big_tree_only_up_to_its_bound() {
        // REWIND stutter: Georg's home has 260,000 entries; a dry run of it walked up to 200,000 a side.
        let d = scratch("big");
        let per = 1000u64;
        let dirs = TALLY_MAX / per + 5;
        for i in 0..dirs {
            let sub = d.join(format!("d{i}"));
            fs::create_dir(&sub).unwrap();
            for k in 0..per {
                fs::File::create(sub.join(format!("f{k}"))).unwrap();
            }
        }
        fs::write(d.join("d0/f0"), b"12345").unwrap();
        let t = tally(&d).unwrap();
        let g = |k: &str| t.get(k).and_then(Json::as_u64).unwrap();
        assert_eq!(t.get("more").and_then(Json::as_bool), Some(true), "{t}");
        assert!(g("seen") <= TALLY_MAX && g("files") + g("dirs") <= TALLY_MAX && g("files") > TALLY_MAX / 2, "{t}");
        assert_eq!(seen(&Json::obj(vec![("then", t.clone()), ("now", t.clone())])), 2 * g("seen"));
        // A small tree is counted whole, as before.
        let s = tally(&d.join("d0")).unwrap();
        assert_eq!((s.get("files").and_then(Json::as_u64), s.get("bytes").and_then(Json::as_u64), s.get("more").and_then(Json::as_bool)), (Some(per), Some(5), Some(false)));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_user_restores_only_what_is_his_then_and_now() {
        let (snap, now) = (scratch("own-snap"), scratch("own-now"));
        let me = fs::metadata(&snap).unwrap().uid();
        for d in [&snap, &now] {
            fs::create_dir_all(d.join("home/u/docs")).unwrap();
            fs::write(d.join("home/u/docs/a.txt"), b"hello").unwrap();
            fs::create_dir_all(d.join("access")).unwrap();
        }
        assert_eq!(restorable(&snap, &now, "home/u/docs/a.txt", me), Ok(()));
        assert_eq!(restorable(&snap, &now, "home/u/docs", me), Ok(()));
        assert_eq!(restorable(&snap, &now, "home/u/docs/new.txt", me).map_err(|e| e.contains("not there")), Err(true), "it must be in the snapshot");
        fs::remove_file(now.join("home/u/docs/a.txt")).unwrap();
        assert_eq!(restorable(&snap, &now, "home/u/docs/a.txt", me), Ok(()), "gone now: it comes back new");
        assert_eq!(live(&now, "home/u/docs/a.txt", me), Ok(None));
        // A folder only its owner may list (0700 root's, as /data/bitsos-access), seen by someone else: the
        // folder itself is refused, in the snapshot and live, for the restore and its dry run alike.
        let other = me + 1;
        fs::set_permissions(snap.join("access"), fs::Permissions::from_mode(0o700)).unwrap();
        let e = restorable(&snap, &now, "access", other).unwrap_err();
        assert!(e.contains("not yours"), "{e}");
        assert!(owned(&snap, "access", me, true).is_ok());
        assert!(owned(&snap, "access", other, true).unwrap_err().contains("not yours"));
        fs::set_permissions(snap.join("access"), fs::Permissions::from_mode(0o300)).unwrap();
        assert!(owned(&snap, "access", me, true).unwrap_err().contains("not yours to look into"), "its own, but closed");
        fs::set_permissions(snap.join("access"), fs::Permissions::from_mode(0o755)).unwrap();
        // Live: a parent that is a link, or a folder the user cannot list, refuses (root: the link names nothing).
        fs::remove_dir_all(now.join("home/u/docs")).unwrap();
        std::os::unix::fs::symlink("/etc", now.join("home/u/docs")).unwrap();
        assert!(live(&now, "home/u/docs/a.txt", me).unwrap_err().contains("link is never followed"));
        assert_eq!(live(&now, "home/u/docs/a.txt", 0), Ok(None));
        fs::set_permissions(now.join("home/u"), fs::Permissions::from_mode(0o300)).unwrap();
        assert!(live(&now, "home/u/x", me).unwrap_err().contains("not yours to look into"));
        fs::set_permissions(now.join("home/u"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&snap).unwrap();
        fs::remove_dir_all(&now).unwrap();
    }

    #[test]
    fn program_names_from_packages_and_data_folders() {
        let d = scratch("names");
        for p in ["apps/brave/1.2", "apps/foot", "appdata/brave", "appdata/steam", "appdata/../x", "apps/Bad Name"] {
            let _ = fs::create_dir_all(d.join(p));
        }
        fs::write(d.join("appdata/file"), b"").unwrap();
        assert_eq!(program_names(&d, Some("apps"), Some("appdata")), ["brave", "foot", "steam"]);
        assert_eq!(program_names(&d, None, Some("appdata")), ["brave", "steam"], "packages not on /data: only the data folders");
        fs::remove_dir_all(&d).unwrap();
    }
}
