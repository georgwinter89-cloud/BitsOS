//! `snap.user` (QemuOnly: it writes /data): /bin/bitssnap as root on the real /data: take, a file and then a
//! folder restored (everything else in the folder untouched, the state before saved first and the restore
//! undone from it), a path outside /data and a missing one refused without a trace, `list`, `serve --once`;
//! as uid 1000 refused. The snapshots it made are deleted again and its folder removed (rerunnable).

use super::{RUN, SEC, data_mount, e, read};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use ktest::{TestResult, kassert, ktest};
use proc::SpawnOptions;
use proc::stdio::{Capture, Input};

const BIN: &str = "/bin/bitssnap";

/// Runs bitssnap with `args` as `uid`: (exit code, stdout, stderr).
fn run(args: &[&str], uid: u32) -> Result<(i32, String, String), String> {
    let out = Capture::new(1 << 20);
    let err = Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin", "SETTINGS_SOCKET=/nonexistent"];
    let mut argv: Vec<&str> = alloc::vec![BIN];
    argv.extend_from_slice(args);
    let mut o = SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdin = Some(Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    o.uid = uid;
    let p = proc::spawn_with(BIN, &o).map_err(|x| format!("spawn {BIN}: {x}"))?;
    let code = match p.wait(Some(120 * SEC)) {
        Some(c) => c,
        None => {
            p.kill(uapi::ab::signal::SIGKILL);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("bitssnap {args:?} did not end in 120 s"));
        }
    };
    let t = |c: &Capture| String::from_utf8_lossy(&c.contents()).into_owned();
    Ok((code, t(&out), t(&err)))
}

fn ok(args: &[&str]) -> Result<String, String> {
    let (c, out, err) = run(args, 0)?;
    if c != 0 {
        return Err(format!("bitssnap {args:?}: exit {c}\n{out}--- stderr ---\n{err}"));
    }
    Ok(out)
}

/// The number after `word` in `text` ("snapshot 12 taken").
fn number_after(text: &str, word: &str) -> Option<u64> {
    let i = text.find(word)? + word.len();
    text[i..].trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn user() -> TestResult {
    let t = match data_mount() {
        Ok(t) => t,
        Err(r) => return r,
    };
    if vfs::root_ns().lookup(BIN).is_err() {
        return Err(format!("{BIN} is not in this image (xtask builds the user workspace into /bin)"));
    }
    let baseline: Vec<u64> = crate::list(&t, false).map_err(e)?.iter().map(|s| s.id).collect();
    let ns = vfs::root_ns();
    let dir = format!("/data/vs-user-{}", RUN.fetch_add(1, Ordering::Relaxed));
    let r = (|| -> TestResult {
        ns.mkdir_p(&format!("{dir}/sub")).map_err(e)?;
        for (p, d) in [("f.txt", "one"), ("sub/a", "A"), ("sub/b", "B"), ("keep.txt", "k1")] {
            ns.create_file_bytes(format!("{dir}/{p}").as_bytes(), 0o644, d.as_bytes()).map_err(e)?;
        }
        let out = ok(&["take", "--label", "vsuser"])?;
        let id = number_after(&out, "snapshot").ok_or_else(|| format!("take said {out:?}"))?;
        ns.write_file(&format!("{dir}/f.txt"), b"two", Some(0)).map_err(e)?;
        ns.remove(&format!("{dir}/sub/b")).map_err(e)?;
        ns.write_file(&format!("{dir}/sub/a"), b"AA", Some(0)).map_err(e)?;
        ns.create_file_bytes(format!("{dir}/sub/c").as_bytes(), 0o644, b"C").map_err(e)?;
        ns.write_file(&format!("{dir}/keep.txt"), b"k2", Some(0)).map_err(e)?;
        let id_s = format!("{id}");
        // A file.
        let out = ok(&["restore", &id_s, &format!("{dir}/f.txt")])?;
        let before = number_after(&out, "the state before is snapshot").ok_or_else(|| format!("restore said {out:?}"))?;
        kassert!(read(&ns, &format!("{dir}/f.txt")).as_deref() == Some(&b"one"[..]), "f.txt not restored");
        kassert!(read(&ns, &format!("{dir}/sub/a")).as_deref() == Some(&b"AA"[..]), "a file restore touched the folder");
        // A folder: as in the snapshot, the rest untouched.
        ok(&["restore", &id_s, &format!("{dir}/sub")])?;
        kassert!(read(&ns, &format!("{dir}/sub/a")).as_deref() == Some(&b"A"[..]), "sub/a");
        kassert!(read(&ns, &format!("{dir}/sub/b")).as_deref() == Some(&b"B"[..]), "sub/b");
        kassert!(read(&ns, &format!("{dir}/sub/c")).is_none(), "sub/c was not in the snapshot");
        kassert!(read(&ns, &format!("{dir}/keep.txt")).as_deref() == Some(&b"k2"[..]), "keep.txt changed");
        // Undo the file restore from its "before-restore" snapshot.
        ok(&["restore", &format!("{before}"), &format!("{dir}/f.txt")])?;
        kassert!(read(&ns, &format!("{dir}/f.txt")).as_deref() == Some(&b"two"[..]), "undo of the restore");
        let out = ok(&["list"])?;
        kassert!(!out.contains(".bitssnap"), "list: {out}");
        // Refused without a trace: nothing mounted, no new snapshot.
        let n = crate::list(&t, false).map_err(e)?.len();
        let (c, _, _) = run(&["restore", &id_s, "/etc/passwd"], 0)?;
        kassert!(c == 1, "restore outside /data: exit {c}");
        let (c, _, err) = run(&["restore", &id_s, &format!("{dir}/nothere")], 0)?;
        kassert!(c == 1 && err.contains("not in snapshot"), "restore of a missing path: exit {c} {err}");
        let l = crate::list(&t, false).map_err(e)?;
        kassert!(l.len() == n && l.iter().all(|s| !s.mounted), "a refused restore left something: {l:?}");
        // The listing.
        let out = ok(&["list", "--sizes"])?;
        kassert!(out.contains("vsuser") && out.matches("before-restore").count() >= 3, "list: {out}");
        // Not for others.
        let (c, _, err) = run(&["list"], 1000)?;
        kassert!(c == 1 && err.contains("errno 13"), "uid 1000: exit {c} {err}");
        // One scheduler round (settings: the defaults, the socket is named away), then one with nothing new.
        ok(&["serve", "--once"])?;
        let out = ok(&["serve", "--once"])?;
        kassert!(out.contains("nothing changed"), "a second round right after: {out}");
        // VS-6: the status (this /data is far from 80 % full: no warning, nothing proposed).
        let out = ok(&["status"])?;
        kassert!(out.contains("% full") && out.contains("space: fine") && !out.contains("WARNING"), "status: {out}");
        Ok(())
    })();
    for s in crate::list(&t, false).map_err(e)? {
        if !baseline.contains(&s.id) {
            let _ = crate::mounts::umount(&t, s.id);
            crate::delete(&t, s.id).map_err(|x| format!("delete {}: errno {x}", s.id))?;
        }
    }
    let _ = ns.remove_tree(&dir);
    let _ = vfs::writeback::sync_durable();
    r
}
ktest!("snap.user", QemuOnly, user);
