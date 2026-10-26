//! `snap.rollback` (Safe, RAM, rerunnable): VS-3's whole-volume rollback on a private pool image, mounted,
//! unmounted and mounted again as the boot does it (`data::bring_up`).

use super::{RUN, e, read};
use crate::{Target, list, take};
use alloc::format;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use fs_ext4::data;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;
use vfs::MountNs;

/// Writes `data` to `path` (made or overwritten) and makes it durable, as a program saving with fsync would.
fn put(ns: &MountNs, path: &str, data: &[u8]) -> TestResult {
    let c = vfs::ops::Ctx::kernel(ns);
    let f = c.open(None, path.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT | vfs::abi::O_TRUNC, 0o644).map_err(e)?;
    f.pwrite(0, data).map_err(e)?;
    f.file.fsync().map_err(e)?;
    let d = c.open(None, b"/d", vfs::abi::O_RDONLY | vfs::abi::O_DIRECTORY, 0).map_err(e)?;
    d.file.fsync().map_err(e)
}

/// One "boot": mount the image at /d, run `f`, unmount (written back) and unregister.
fn boot<T>(ns: &Arc<MountNs>, p: &data::Place, f: impl FnOnce(&Target) -> Result<T, alloc::string::String>) -> Result<T, alloc::string::String> {
    let (ph, m) = data::bring_up(ns, p, true, 32, &|| 1 << 40, &|_| {});
    let m = m.ok_or_else(|| format!("the private pool did not mount: {ph:?}"))?;
    let r = Target::of(ns, "/d", &m).map_err(e).and_then(|t| f(&t));
    let _ = vfs::writeback::Flush::stop(&*m.vol);
    if let Ok(path) = ns.lookup_path(b"/d", true) {
        let _ = ns.umount(&path);
    }
    if let Some(d) = &m.disk {
        block::unregister(d);
    }
    r
}

fn rollback() -> TestResult {
    let name = format!("snaprb{}", RUN.fetch_add(1, Ordering::Relaxed));
    let ns = Arc::new(MountNs::new_empty());
    ns.mkdir_p("/stick").map_err(e)?;
    let p = data::Place { image: "/stick/data.img", temp: "/stick/data.img.new", at: "/d", pool_name: &name, on_stick: false };
    // Boot 1: a, snapshot, then a changed and b new; the rollback scheduled changes nothing yet.
    let s = boot(&ns, &p, |t| {
        put(&ns, "/d/a", b"one")?;
        let s = take(t, "kt-15:00", false, false).map_err(e)?.id;
        put(&ns, "/d/a", b"two")?;
        put(&ns, "/d/b", b"bee")?;
        let tt = t.clone();
        let target = move || Ok(tt.clone());
        let run = |l: &str| crate::dev::run(l.as_bytes(), &target);
        kassert!(run("rollback").map_err(e)? == "pending=0 not_done=0\n", "pending before");
        kassert!(run(&format!("rollback {}", t.vol)) == Err(EINVAL), "the volume itself as target");
        kassert!(run("rollback 999999") == Err(ENOENT), "a missing snapshot");
        for bad in ["rollback x", "rollback 1 2", "rollback cancel now", "restore-at", "restore-at x", "restore-at 1 wet", "restore-at cancel now", "system-at", "system-pin off"] {
            kassert!(run(bad) == Err(EINVAL), "{bad:?} was not refused");
        }
        // Before every snapshot: nothing to go back to, nothing scheduled.
        let r = run("restore-at 1").map_err(e)?;
        kassert!(r.starts_with("ok=0 note=no snapshot of /data"), "restore-at before all snapshots: {r}");
        // A stray undo: no restore-at of this pool to undo, nothing changes (a PIN of Georg's would stay).
        let u = run("restore-at cancel").map_err(e)?;
        kassert!(u.starts_with("undone=0 text=no restore-at"), "a stray restore-at cancel: {u}");
        kassert!(run(&format!("rollback {s}")).map_err(e)? == format!("scheduled to={s} replaced=0\n"), "schedule");
        kassert!(run("rollback").map_err(e)? == format!("pending={s} not_done=0\n"), "pending");
        kassert!(crate::delete(t, s) == Err(EBUSY), "the scheduled snapshot was deleted");
        let l = list(t, false).map_err(e)?;
        kassert!(l.iter().any(|x| x.id == s && x.rollback), "list marks it: {l:?}");
        kassert!(run("list").map_err(e)?.contains("rollback=1"), "the list line marks it");
        kassert!(read(&ns, "/d/a").as_deref() == Some(&b"two"[..]), "a scheduled rollback changed the live volume");
        Ok(s)
    })?;
    // Boot 2: the volume is as in the snapshot; the state before is "before-rollback".
    let kept = boot(&ns, &p, |t| {
        kassert!(read(&ns, "/d/a").as_deref() == Some(&b"one"[..]), "a after the rollback");
        kassert!(read(&ns, "/d/b").is_none(), "b after the rollback");
        kassert!(crate::pending_rollback(t).map_err(e)?.is_none(), "still scheduled after it ran");
        let l = list(t, false).map_err(e)?;
        let k = l.iter().find(|x| x.label == bitsvol::BEFORE_ROLLBACK).ok_or_else(|| format!("no before-rollback snapshot: {l:?}"))?;
        kassert!(!k.auto && l.iter().any(|x| x.id == s && !x.rollback), "kinds after: {l:?}");
        // A change of mind: scheduled and cancelled leaves the next boot alone.
        crate::schedule_rollback(t, k.id).map_err(e)?;
        kassert!(crate::cancel_rollback(t).map_err(e)? == Some(k.id), "cancel");
        kassert!(crate::cancel_rollback(t).map_err(e)?.is_none(), "cancel twice");
        put(&ns, "/d/c", b"sea")?;
        Ok(k.id)
    })?;
    // Boot 3: nothing scheduled, nothing changes; then the undo.
    boot(&ns, &p, |t| {
        let (a, c) = (read(&ns, "/d/a"), read(&ns, "/d/c"));
        kassert!(a.as_deref() == Some(&b"one"[..]) && c.as_deref() == Some(&b"sea"[..]), "a cancelled rollback ran: a={a:?} c={c:?}");
        crate::schedule_rollback(t, kept).map_err(e)?;
        Ok(())
    })?;
    // Boot 4: back to the state before the first rollback (c was written after it: in the next before-rollback).
    boot(&ns, &p, |t| {
        kassert!(read(&ns, "/d/a").as_deref() == Some(&b"two"[..]) && read(&ns, "/d/b").as_deref() == Some(&b"bee"[..]), "the undo");
        kassert!(read(&ns, "/d/c").is_none(), "c after the undo");
        let l = list(t, false).map_err(e)?;
        kassert!(l.iter().filter(|x| x.label == bitsvol::BEFORE_ROLLBACK).count() == 2, "two before-rollback snapshots: {l:?}");
        Ok(())
    })
}
ktest!("snap.rollback", Safe, rollback);
