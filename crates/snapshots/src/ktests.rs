//! VS-2 ktests (prefix `snap.`):
//! - `private` (Safe, RAM): a pool image in a tmpfs file of a private view (made like data.img), ext4 on its
//!   volume; take, take-if-changed (nothing written: no new snapshot), a write during the freeze waits for the
//!   thaw, a snapshot mounted read-only shows the state at its freeze and refuses writes, delete refuses a
//!   mounted snapshot and the volume itself, the `/dev/bitssnap` command lines (good and bad). Rerunnable.
//! - `data` (Safe, reads only): `stats` and `list` of the boot mount /data (skip without a pool there).
//! - `load` (QemuOnly, writes /data): 100 snapshots of /data while a writer rewrites files, renames over and
//!   fsyncs; every snapshot is mounted read-only and each file in it is whole (one version, never torn); the
//!   freeze pauses as one event (timing judged on the Mini-PC, not here). The snapshots are deleted again,
//!   except when the boot's `ktest=` is exactly `snap.load`: then host/tests/vs/30-snapshots.sh checks each
//!   with e2fsck -fn on the host.
//! - `rename_over` (Safe, RAM): a writer renaming new files over old ones (kernel calls and fds) under 40
//!   snapshots each; the saved file is whole in every snapshot.
//! - `rollback` (Safe, RAM; VS-3, ktests/rollback.rs): /data's whole-volume rollback on a private pool: scheduled
//!   while mounted (nothing changes then), carried out at the next mount with the state before kept as
//!   "before-rollback", undone the same way, cancelled; the scheduled snapshot cannot be deleted; the command
//!   lines `rollback`, `rollback cancel`, `rollback ID`; `restore-at` refuses a time before every snapshot. Rerunnable.
//! - `user` (QemuOnly, writes /data): /bin/bitssnap as root: take, list, a file and a folder restored from a
//!   snapshot (the state before saved first, everything else in the folder untouched), `serve --once`; as uid
//!   1000 refused.

use crate::{Target, delete, list, mounts, take};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use fs_ext4::data;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;
use vfs::{FsFreeze, MountNs};

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
static RUN: AtomicU32 = AtomicU32::new(0);

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn read(ns: &MountNs, p: &str) -> Option<Vec<u8>> {
    ns.read_file(p, 1 << 20).ok()
}

/// A pool image in a private view mounted at /d, as the boot makes data.img; `f` runs with it, then everything
/// is unmounted and unregistered.
fn with_private(f: impl FnOnce(&Arc<MountNs>, &Target) -> TestResult) -> TestResult {
    let name = format!("snapkt{}", RUN.fetch_add(1, Ordering::Relaxed));
    let ns = Arc::new(MountNs::new_empty());
    ns.mkdir_p("/stick").map_err(e)?;
    let p = data::Place { image: "/stick/data.img", temp: "/stick/data.img.new", at: "/d", pool_name: &name, on_stick: false };
    let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 1 << 40, &|_| {});
    let m = m.ok_or_else(|| format!("the private pool did not mount: {ph:?}"))?;
    let r = Target::of(&ns, "/d", &m).map_err(e).and_then(|t| {
        let r = f(&ns, &t);
        // Whatever the test left mounted.
        if let Ok(l) = list(&t, false) {
            for s in l.iter().filter(|s| s.mounted) {
                let _ = mounts::umount(&t, s.id);
            }
        }
        r
    });
    let _ = vfs::writeback::Flush::stop(&*m.vol);
    if let Ok(path) = ns.lookup_path(b"/d", true) {
        let _ = ns.umount(&path);
    }
    if let Some(d) = &m.disk {
        block::unregister(d);
    }
    r
}

fn private() -> TestResult {
    with_private(|ns, t| {
        ns.create_file_bytes(b"/d/a", 0o644, b"one").map_err(e)?;
        let s1 = take(t, "kt-one", false, false).map_err(e)?;
        kassert!(s1.new, "first take made nothing");
        kassert!(!t.sb.is_frozen(), "frozen after the take");
        kassert!(t.pool.lock().map_err(|x| format!("{x:?}"))?.copy_window().is_none(), "the take left its copy window open");
        let u = take(t, "kt-auto", true, true).map_err(e)?;
        kassert!(!u.new && u.id == s1.id, "nothing written, yet a new snapshot: {u:?}");
        ns.write_file("/d/a", b"two", Some(0)).map_err(e)?;
        ns.create_file_bytes(b"/d/b", 0o644, b"bee").map_err(e)?;
        let s2 = take(t, "kt-auto", true, true).map_err(e)?;
        kassert!(s2.new && s2.id != s1.id, "a write, yet no new snapshot: {s2:?}");
        // Frozen by someone else: a take refuses and leaves that freeze alone.
        t.sb.freeze().map_err(e)?;
        let d0 = t.pool.deferred_flushes();
        let r = take(t, "kt-x", false, false);
        let (still, opened, d1) = (t.sb.is_frozen(), t.pool.window_open(), t.pool.deferred_flushes());
        t.sb.thaw();
        kassert!(matches!(r, Err(EBUSY)), "a take while frozen: {r:?}");
        kassert!(still, "a refused take thawed someone else's freeze");
        kassert!(!opened && d1 == d0, "a refused take deferred flushes of someone else's freeze ({d0} -> {d1})");
        // An error inside the frozen section (the pool's snapshot failing): thawed, writes go on, no snapshot.
        crate::FAIL_FROZEN.store(true, Ordering::Relaxed);
        let r = take(t, "kt-fail", false, false);
        kassert!(matches!(r, Err(EIO)), "the injected failure: {r:?}");
        kassert!(!t.sb.is_frozen(), "still frozen after a failed take");
        let p = t.pool.lock().map_err(|x| format!("{x:?}"))?;
        kassert!(p.copy_window().is_none() && !t.pool.window_open(), "a failed take left its window open");
        drop(p);
        kassert!(t.pool.owed_flushes() == 0, "a failed take skipped its commit: {} flushes owed", t.pool.owed_flushes());
        ns.write_file("/d/b", b"bee", Some(0)).map_err(e)?;
        kassert!(list(t, false).map_err(e)?.len() == 2, "a failed take left a snapshot");
        // A change during the freeze waits for the thaw.
        t.sb.freeze().map_err(e)?;
        let done = Arc::new(AtomicBool::new(false));
        let (n2, d2) = (ns.clone(), done.clone());
        let h = sched::spawn("snap-kt-writer", sched::Class::Normal, move || {
            let _ = n2.create_file_bytes(b"/d/during", 0o644, b"later");
            d2.store(true, Ordering::SeqCst);
        })
        .map_err(e)?;
        sched::sleep_ns(200 * MS);
        let waited = !done.load(Ordering::SeqCst);
        t.sb.thaw();
        kassert!(h.join_timeout(10 * SEC), "the writer did not end after the thaw");
        kassert!(waited, "a new file went through while frozen");
        kassert!(read(ns, "/d/during").as_deref() == Some(&b"later"[..]), "the held-back write is lost");
        // The snapshot read-only: the state at its freeze.
        let at = mounts::mount_at(t, s1.id, ns.clone(), "/s1").map_err(e)?;
        kassert!(mounts::mount_at(t, s1.id, ns.clone(), "/elsewhere").map_err(e)? == at, "a second mount moved it");
        kassert!(read(ns, "/s1/a").as_deref() == Some(&b"one"[..]), "snapshot content of a");
        kassert!(read(ns, "/s1/b").is_none(), "b was written after the snapshot");
        kassert!(ns.create_file_bytes(b"/s1/x", 0o644, b"x").is_err(), "a read-only snapshot took a new file");
        kassert!(delete(t, s1.id) == Err(EBUSY), "a mounted snapshot was deleted");
        kassert!(delete(t, t.vol) == Err(EINVAL), "the volume itself is no snapshot to delete");
        let l = list(t, true).map_err(e)?;
        kassert!(l.len() == 2 && l[0].id == s1.id && l[0].mounted && !l[1].mounted, "list: {l:?}");
        kassert!(l.iter().all(|s| s.bytes.is_some()), "sizes: {l:?}");
        kassert!(!l[0].auto && l[1].auto && l[1].label == "kt-auto", "flags and labels: {l:?}");
        // The command lines of /dev/bitssnap.
        let tt = t.clone();
        let target = move || Ok(tt.clone());
        let run = |l: &str| crate::dev::run(l.as_bytes(), &target);
        let listed = run("list").map_err(e)?;
        kassert!(listed.lines().count() == 2 && listed.contains(&format!("id={} ", s1.id)) && listed.contains("mounted=1"), "list line: {listed}");
        kassert!(run("stats").map_err(e)?.contains("snapshots=2"), "stats");
        kassert!(run(&format!("size {}", s2.id)).map_err(e)?.starts_with(&format!("id={} bytes=", s2.id)), "size");
        for bad in ["", "bogus", "take now", "take label=", "take label=a/b", "list everything", "delete", "delete x", "delete 1 2", "stats x"] {
            kassert!(run(bad) == Err(EINVAL), "{bad:?} was not refused");
        }
        for bad in ["take\nlist", "list\nstats", "stats\0", "delete 99999999999999999999999", "delete -1", "size 1 2", "mount", "umount x"] {
            kassert!(run(bad) == Err(EINVAL), "{bad:?} was not refused");
        }
        kassert!(run(&"x".repeat(crate::dev::LINE_MAX + 1)) == Err(EINVAL), "an overlong line");
        kassert!(run(&"list ".repeat((64 << 10) / 5)) == Err(EINVAL), "64 KiB");
        kassert!(crate::dev::run(&[0xff, 0xfe, b' ', b'1'], &target) == Err(EINVAL), "not UTF-8");
        kassert!(run("size 999999") == Err(ENOENT), "size of a missing id");
        kassert!(run("delete 999999") == Err(ENOENT), "a missing id");
        kassert!(run(&format!("delete {}", t.vol)) == Err(EINVAL), "the volume by the command");
        let took = run("take label=kt-cmd").map_err(e)?;
        kassert!(took.starts_with("id=") && took.contains(" new=1 ") && took.contains(" pause_us="), "take line: {took}");
        mounts::umount(t, s1.id).map_err(e)?;
        kassert!(mounts::umount(t, s1.id) == Err(ENOENT), "second umount");
        kassert!(read(ns, "/s1/a").is_none(), "still mounted");
        for s in list(t, false).map_err(e)? {
            kassert!(run(&format!("delete {}", s.id)).map_err(e)?.starts_with("deleted"), "delete {}", s.id);
        }
        kassert!(list(t, false).map_err(e)?.is_empty(), "snapshots left");
        kassert!(read(ns, "/d/a").as_deref() == Some(&b"two"[..]), "the volume changed");
        Ok(())
    })
}
ktest!("snap.private", Safe, private);

/// The boot mount /data as a target, once the boot finished with it (fs.ext4.data judges the mount itself).
fn data_mount() -> Result<Target, TestResult> {
    match data::wait_done(120_000) {
        data::Phase::Mounted { .. } => {}
        p => return Err(ktest::skip(&format!("/data: {p:?}"))),
    }
    match Target::data() {
        Ok(t) => Ok(t),
        Err(ENODEV) => Err(ktest::skip("no /data mount")),
        Err(EOPNOTSUPP) => Err(ktest::skip("/data is a plain ext4 image (no pool)")),
        Err(EROFS) => Err(ktest::skip("/data is read-only")),
        Err(x) => Err(Err(format!("/data: errno {x}"))),
    }
}

fn data_reads() -> TestResult {
    let t = match data_mount() {
        Ok(t) => t,
        Err(r) => return r,
    };
    let target = move || Ok(t.clone());
    let s = crate::dev::run(b"stats", &target).map_err(e)?;
    kassert!(s.starts_with("pool=data ") && s.contains(" free_permille="), "stats: {s}");
    let l = crate::dev::run(b"list", &target).map_err(e)?;
    kassert!(l.lines().all(|x| x.starts_with("id=")), "list: {l}");
    Ok(())
}
ktest!("snap.data", Safe, data_reads);

const FILES: u64 = 8;
const FILE_BYTES: usize = 16 * 1024;

/// One version of a file: `FILE_BYTES` of the little-endian word `v`.
fn version(v: u32) -> Vec<u8> {
    let mut b = vec![0u8; FILE_BYTES];
    for c in b.chunks_exact_mut(4) {
        c.copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// None: missing; Some(Err): torn or cut; Some(Ok(v)): whole version v.
fn whole(b: Option<Vec<u8>>) -> Option<Result<u32, String>> {
    let b = b?;
    if b.len() != FILE_BYTES {
        return Some(Err(format!("{} bytes", b.len())));
    }
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    Some(if b.chunks_exact(4).all(|c| c == v.to_le_bytes()) { Ok(v) } else { Err(format!("mixed versions (first {v})")) })
}

fn writer(ns: Arc<MountNs>, dir: String, stop: Arc<AtomicBool>, writes: Arc<AtomicU64>, errors: Arc<AtomicU64>) {
    let c = vfs::ops::Ctx::kernel(&ns);
    let mut v: u32 = 1;
    while !stop.load(Ordering::Relaxed) {
        let i = v as u64 % FILES;
        let path = format!("{dir}/f{i}");
        let r = (|| -> Result<(), i64> {
            let f = c.open(None, path.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT, 0o644)?;
            f.pwrite(0, &version(v))?;
            drop(f);
            if v % 4 == 0 {
                ns.lookup(&path)?.inode.sync(vfs::pagecache::SyncKind::Fsync)?;
            }
            // Brave's way of saving: a new file written, then renamed over the old one (through fds, as programs
            // do: every step waits while frozen).
            if v % 3 == 0 {
                let tmp = format!("{dir}/save.tmp");
                let f = c.open(None, tmp.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT | vfs::abi::O_TRUNC, 0o644)?;
                f.pwrite(0, &version(v))?;
                drop(f);
                c.rename(None, tmp.as_bytes(), None, format!("{dir}/saved").as_bytes(), 0)?;
            }
            Ok(())
        })();
        if r.is_err() {
            errors.fetch_add(1, Ordering::Relaxed);
        }
        writes.fetch_add(1, Ordering::Relaxed);
        v = v.wrapping_add(1).max(1);
        sched::sleep_ns(2 * MS);
    }
}

/// Every file of `dir` in snapshot `id` is whole. An f-file may be empty (created by open, the snapshot came
/// before its first write: two steps of the writer); `saved` never (it is written before its rename).
fn check_snapshot(t: &Target, id: u64, rel: &str) -> TestResult {
    let ns = vfs::root_ns();
    let at = format!("/tmp/snapkt-load-{id}");
    mounts::mount_at(t, id, ns.clone(), &at).map_err(|x| format!("mount {id}: {x}"))?;
    let r = (|| -> TestResult {
        for name in (0..FILES).map(|i| format!("f{i}")).chain(["saved".into()]) {
            let b = read(&ns, &format!("{at}/{rel}/{name}"));
            if name != "saved" && b.as_ref().is_some_and(Vec::is_empty) {
                continue;
            }
            if let Some(Err(m)) = whole(b) {
                return Err(format!("snapshot {id}: {rel}/{name}: {m}"));
            }
        }
        Ok(())
    })();
    mounts::umount(t, id).map_err(|x| format!("umount {id}: {x}"))?;
    r
}

fn load() -> TestResult {
    let t = match data_mount() {
        Ok(t) => t,
        Err(r) => return r,
    };
    let keep = kexec::boot::config("ktest").as_deref().map(str::trim) == Some("snap.load");
    let free = {
        let s = t.pool.lock().map_err(e)?.stats();
        s.free.saturating_sub(s.reserve).saturating_mul(s.chunk_size as u64)
    };
    // About 1 MiB of copied chunks and metadata per snapshot with this writer; half the free space at most.
    let want = 100u64;
    let n = want.min(free / (2 << 20));
    if n < 10 {
        return ktest::skip("less than 20 MiB free in the pool");
    }
    let ns = vfs::root_ns();
    let rel = format!("vs-load-{}", RUN.fetch_add(1, Ordering::Relaxed));
    let dir = format!("/data/{rel}");
    ns.mkdir_p(&dir).map_err(e)?;
    let (stop, writes, errors) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let h = {
        let (n2, d2, s2, w2, e2) = (ns.clone(), dir.clone(), stop.clone(), writes.clone(), errors.clone());
        sched::spawn("snap-load-writer", sched::Class::Normal, move || writer(n2, d2, s2, w2, e2)).map_err(e)?
    };
    let mut ids = Vec::new();
    let mut pauses = Vec::new();
    let r = (|| -> TestResult {
        for i in 0..n {
            let before = writes.load(Ordering::Relaxed);
            let k = take(&t, "vsload", false, false).map_err(|x| format!("take {i}: errno {x}"))?;
            ids.push(k.id);
            pauses.push(k.pause_ns);
            // Let the writer change something before the next one.
            let end = time::now_ns() + 5 * SEC;
            while writes.load(Ordering::Relaxed) < before + 6 && time::now_ns() < end {
                sched::sleep_ns(2 * MS);
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    let ended = h.join_timeout(30 * SEC);
    r?;
    kassert!(ended, "the writer did not stop");
    kassert!(errors.load(Ordering::Relaxed) == 0, "{} writer errors", errors.load(Ordering::Relaxed));
    for &id in &ids {
        check_snapshot(&t, id, &rel)?;
    }
    pauses.sort_unstable();
    let pick = |q: usize| pauses[(pauses.len() - 1) * q / 100] / 1000;
    klog::event!(Info, "snap", "load.pauses", snapshots = ids.len(), writes = writes.load(Ordering::Relaxed),
        min_us = pick(0), median_us = pick(50), p95_us = pick(95), max_us = pick(100), kept = keep);
    if n < want {
        klog::event!(Warn, "snap", "load.fewer", snapshots = n, want = want, free = free);
    }
    if !keep {
        for &id in &ids {
            delete(&t, id).map_err(|x| format!("delete {id}: errno {x}"))?;
        }
    }
    let _ = ns.remove_tree(&dir);
    // A ktest boot ends without the stop pass: make the removal durable, or the next boot replays a state with
    // the writer's last renames but not their data (a crash's outcome, which a snapshot then shows faithfully).
    let _ = vfs::writeback::sync_durable();
    kassert!(n == want || !keep, "only {n} snapshots fit (want {want}); give the scenario a larger data.img");
    Ok(())
}
ktest!("snap.load", QemuOnly, load);

mod fsyncs;
mod rollback;
mod user;

/// A writer saving Brave's way (a new file renamed over the old, through the kernel's own file calls and through
/// fds) while snapshots are taken: in every snapshot the saved file is whole (rename_over).
fn race_on(ns: &Arc<MountNs>, t: &Target, d: &str, gated: bool, files: bool) -> Result<(u64, u64), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (n2, s2, d2) = (ns.clone(), stop.clone(), String::from(d));
    let _ = ns.mkdir_p(d);
    let h = sched::spawn("snap-race", sched::Class::Normal, move || {
        let c = vfs::ops::Ctx::kernel(&n2);
        let mut v = 1u32;
        let tmp = format!("{d2}/save.tmp");
        let saved = format!("{d2}/saved");
        while !s2.load(Ordering::Relaxed) {
            if files {
                if let Ok(f) = c.open(None, format!("{d2}/f{}", v % 8).as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT, 0o644) {
                    let _ = f.pwrite(0, &version(v));
                }
            }
            if gated {
                if let Ok(f) = c.open(None, tmp.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT | vfs::abi::O_TRUNC, 0o644) {
                    let _ = f.pwrite(0, &version(v));
                }
            } else {
                let _ = n2.remove(&tmp);
                let _ = n2.create_file_bytes(tmp.as_bytes(), 0o644, &version(v));
            }
            let _ = c.rename(None, tmp.as_bytes(), None, saved.as_bytes(), 0);
            v += 1;
            sched::sleep_ns(MS);
        }
    })
    .map_err(e)?;
    let (mut bad, mut n) = (0, 0);
    let rel = d.strip_prefix("/data").or_else(|| d.strip_prefix("/d")).unwrap_or(d);
    for _ in 0..40 {
        let k = take(t, "race", false, false).map_err(e)?;
        let at = mounts::mount_at(t, k.id, vfs::root_ns(), "/tmp/race").map_err(e)?;
        if let Some(Err(m)) = whole(read(&vfs::root_ns(), &format!("{at}{rel}/saved"))) {
            bad += 1;
            klog::event!(Warn, "snap", "race.bad", gated = gated, files = files, id = k.id, what = m.as_str());
        }
        n += 1;
        mounts::umount(t, k.id).map_err(e)?;
        delete(t, k.id).map_err(e)?;
        sched::sleep_ns(3 * MS);
    }
    stop.store(true, Ordering::Relaxed);
    h.join_timeout(10 * SEC);
    let _ = ns.remove_tree(d);
    Ok((n, bad))
}

fn rename_over() -> TestResult {
    for (gated, files) in [(false, false), (true, true)] {
        let mut r = (0, 0);
        with_private(|ns, t| {
            r = race_on(ns, t, "/d/race", gated, files)?;
            Ok(())
        })?;
        kassert!(r.1 == 0, "{} of {} snapshots hold a torn or empty saved file (fd writes: {gated})", r.1, r.0);
    }
    Ok(())
}
ktest!("snap.rename_over", Safe, rename_over);
