//! AB-13 ktests: the credential calls through the real dispatcher on held test processes (the harness of
//! `wave_a_ktests`), as root and as a user, against linux-6.6/kernel/sys.c and kernel/groups.c. The ABI diff
//! (`abi-ids`) runs the user paths on CachyOS too; the root paths are only here. All `Safe` and rerunnable.

use crate::ab7_ktests::text;
use crate::wave_a_ktests::{T, expect};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::ab::nr_more::*;
use uapi::errno::*;
use uapi::nr;

const NONE: u64 = u32::MAX as u64;

fn root(name: &str) -> Result<T, String> {
    T::with(name, |o| o.uid = 0)
}

fn res(t: &mut T, n: usize) -> Result<[u32; 3], String> {
    expect("getres*id", t.call(n, &[t.a(0), t.a(4), t.a(8)]), 0)?;
    Ok([t.u32_at(0), t.u32_at(4), t.u32_at(8)])
}

/// The Uid:/Gid:/Groups: lines of /proc/<pid>/status and of its main thread's task/ entry.
fn status_ids(pid: u32) -> Result<(String, String, String), String> {
    let s = text(&format!("/proc/{pid}/status"))?;
    let th = text(&format!("/proc/{pid}/task/{pid}/status"))?;
    let line = |s: &str, k: &str| s.lines().find(|l| l.starts_with(k)).map(String::from).ok_or_else(|| format!("no {k} in {s}"));
    let ids = (line(&s, "Uid:")?, line(&s, "Gid:")?, line(&s, "Groups:")?);
    kassert!((line(&th, "Uid:")?, line(&th, "Gid:")?, line(&th, "Groups:")?) == ids, "task status differs: {th}");
    Ok(ids)
}

ktest!("ab.cred.root_drop", Safe, root_drop);
/// What `chpst -u 1000:1000` does as root: setgroups, setgid, setuid; then nothing comes back.
fn root_drop() -> TestResult {
    let mut t = root("ab13-drop")?;
    let pid = t.p.pid;
    kassert!(status_ids(pid)? == (String::from("Uid:\t0\t0\t0\t0"), String::from("Gid:\t0\t0\t0\t0"), String::from("Groups:\t ")), "root's status");
    t.put(0, &[1000u32, 5, 1000].iter().flat_map(|g| g.to_le_bytes()).collect::<Vec<u8>>());
    expect("setgroups", t.call(SETGROUPS, &[3, t.a(0)]), 0)?;
    expect("getgroups(0)", t.call(GETGROUPS, &[0, 0]), 3)?;
    expect("getgroups(2)", t.call(GETGROUPS, &[2, t.a(64)]), -EINVAL)?;
    expect("getgroups(8)", t.call(GETGROUPS, &[8, t.a(64)]), 3)?;
    kassert!([t.u32_at(64), t.u32_at(68), t.u32_at(72)] == [5, 1000, 1000], "sorted groups");
    expect("getgroups to the kernel", t.call(GETGROUPS, &[3, 0xffff_8880_0000_0000]), -EFAULT)?;
    expect("setgid", t.call(nr::SETGID, &[1000]), 0)?;
    expect("setuid", t.call(nr::SETUID, &[1000]), 0)?;
    kassert!(res(&mut t, GETRESUID)? == [1000; 3] && res(&mut t, GETRESGID)? == [1000; 3], "ids after the drop");
    for n in [nr::GETUID, nr::GETEUID, nr::GETGID, nr::GETEGID] {
        expect("get*id", t.call(n, &[]), 1000)?;
    }
    let (u, g, gr) = status_ids(pid)?;
    kassert!(u == "Uid:\t1000\t1000\t1000\t1000" && g == "Gid:\t1000\t1000\t1000\t1000" && gr == "Groups:\t5 1000 1000 ", "status {u:?} {g:?} {gr:?}");
    // No way back: every id is 1000, CAP_SETUID/CAP_SETGID are gone.
    expect("setuid(0)", t.call(nr::SETUID, &[0]), -EPERM)?;
    expect("seteuid(0)", t.call(SETRESUID, &[NONE, 0, NONE]), -EPERM)?;
    expect("setgid(0)", t.call(nr::SETGID, &[0]), -EPERM)?;
    expect("setgroups", t.call(SETGROUPS, &[0, 0]), -EPERM)?;
    expect("setfsuid(0)", t.call(SETFSUID, &[0]), 1000)?;
    expect("setfsuid(0) changed nothing", t.call(SETFSUID, &[NONE]), 1000)?;
    // commit_creds: the memory is not dumpable after the euid changed.
    kassert!(t.p.mm_flags.dumpable() == 0, "dumpable {}", t.p.mm_flags.dumpable());
    let s = text(&format!("/proc/{pid}/status"))?;
    kassert!(s.contains("\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n"), "caps after the drop: {s}");
    Ok(())
}

ktest!("ab.cred.saved_ids", Safe, saved_ids);
/// A root program that steps down and back with its saved uid (seteuid), setreuid, setfsuid.
fn saved_ids() -> TestResult {
    let mut t = root("ab13-saved")?;
    expect("seteuid(1000)", t.call(SETRESUID, &[NONE, 1000, NONE]), 0)?;
    kassert!(res(&mut t, GETRESUID)? == [0, 1000, 0], "seteuid keeps real and saved 0");
    // Not privileged now (euid 1000), but 0 is still the real and saved id.
    expect("setgid(5) without CAP_SETGID", t.call(nr::SETGID, &[5]), -EPERM)?;
    expect("setfsuid(0)", t.call(SETFSUID, &[0]), 1000)?;
    expect("setfsuid back", t.call(SETFSUID, &[1000]), 0)?;
    expect("seteuid(0)", t.call(SETRESUID, &[NONE, 0, NONE]), 0)?;
    kassert!(res(&mut t, GETRESUID)? == [0; 3], "back to root");
    // setfsuid as root: anything; fsuid shows in status, the effective id stays.
    expect("setfsuid(1234)", t.call(SETFSUID, &[1234]), 0)?;
    let (u, _, _) = status_ids(t.p.pid)?;
    kassert!(u == "Uid:\t0\t0\t0\t1234", "{u:?}");
    // setreuid(1000, 2000) as root: real 1000, effective and saved 2000, fs follows the effective id.
    expect("setreuid", t.call(SETREUID, &[1000, 2000]), 0)?;
    kassert!(res(&mut t, GETRESUID)? == [1000, 2000, 2000], "setreuid");
    expect("setfsuid(-1) asks", t.call(SETFSUID, &[NONE]), 2000)?;
    // Unprivileged swap back and forth (the old real id is allowed).
    expect("setreuid swap", t.call(SETREUID, &[2000, 1000]), 0)?;
    kassert!(res(&mut t, GETRESUID)? == [2000, 1000, 1000], "swap");
    expect("setreuid to a stranger", t.call(SETREUID, &[NONE, 3000]), -EPERM)?;
    expect("setresuid noop", t.call(SETRESUID, &[NONE, NONE, NONE]), 0)?;
    expect("setuid(-1)", t.call(nr::SETUID, &[NONE]), -EINVAL)?;
    expect("setreuid(-1, -2)", t.call(SETREUID, &[NONE, 0xffff_fffe]), -EPERM)?;
    // Groups the same way: setresgid as root, then setregid rules.
    let mut g = root("ab13-gids")?;
    expect("setresgid", g.call(SETRESGID, &[10, 20, 30]), 0)?;
    kassert!(res(&mut g, GETRESGID)? == [10, 20, 30], "setresgid");
    expect("setfsgid(-1) asks", g.call(SETFSGID, &[NONE]), 20)?;
    expect("setregid(-1, 30)", g.call(SETREGID, &[NONE, 30]), 0)?;
    kassert!(res(&mut g, GETRESGID)? == [10, 30, 30], "setregid saved follows (not the real gid)");
    let (_, gl, _) = status_ids(g.p.pid)?;
    kassert!(gl == "Gid:\t10\t30\t30\t30", "{gl:?}");
    Ok(())
}

ktest!("ab.cred.groups_edges", Safe, groups_edges);
/// setgroups' argument rules in Linux's order (groups.c:198, `groups_from_user`), and NGROUPS_MAX.
fn groups_edges() -> TestResult {
    let mut t = root("ab13-groups")?;
    expect("setgroups(65537)", t.call(SETGROUPS, &[65537, t.a(0)]), -EINVAL)?;
    expect("setgroups(-1)", t.call(SETGROUPS, &[u64::MAX, t.a(0)]), -EINVAL)?;
    expect("setgroups NULL", t.call(SETGROUPS, &[2, 0]), -EFAULT)?;
    // An invalid id before the unmapped page is EINVAL; reading on into it is EFAULT.
    let end = (1 << 20) - 4;
    t.put(end, &u32::MAX.to_le_bytes());
    expect("-1 then a fault", t.call(SETGROUPS, &[2, t.a(end)]), -EINVAL)?;
    t.put(end, &7u32.to_le_bytes());
    expect("7 then a fault", t.call(SETGROUPS, &[2, t.a(end)]), -EFAULT)?;
    expect("nothing changed", t.call(GETGROUPS, &[0, 0]), 0)?;
    // NGROUPS_MAX ids (256 KiB), descending: kept sorted.
    let ids: Vec<u8> = (0..65536u32).rev().flat_map(|g| g.to_le_bytes()).collect();
    t.put(0, &ids);
    expect("setgroups(65536)", t.call(SETGROUPS, &[65536, t.a(0)]), 0)?;
    expect("getgroups(0)", t.call(GETGROUPS, &[0, 0]), 65536)?;
    expect("getgroups(65536)", t.call(GETGROUPS, &[65536, t.a(300_000)]), 65536)?;
    kassert!(t.u32_at(300_000) == 0 && t.u32_at(300_000 + 4 * 65535) == 65535, "sorted list");
    expect("clear", t.call(SETGROUPS, &[0, 0]), 0)?;
    expect("cleared", t.call(GETGROUPS, &[0, 0]), 0)?;
    Ok(())
}

ktest!("ab.cred.user_and_kill", Safe, user_and_kill);
/// An ordinary user's limits, and who may signal whom (`kill_ok_by_cred`: the sender's real or effective uid
/// against the target's real or saved uid).
fn user_and_kill() -> TestResult {
    let mut u = T::new("ab13-user")?;
    expect("setuid(0)", u.call(nr::SETUID, &[0]), -EPERM)?;
    expect("setuid(self)", u.call(nr::SETUID, &[1000]), 0)?;
    expect("setresuid(-1,-1,-1)", u.call(SETRESUID, &[NONE, NONE, NONE]), 0)?;
    expect("setresuid(0,..)", u.call(SETRESUID, &[0, NONE, NONE]), -EPERM)?;
    expect("setreuid(1000,1000)", u.call(SETREUID, &[1000, 1000]), 0)?;
    expect("setgid(0)", u.call(nr::SETGID, &[0]), -EPERM)?;
    expect("setfsuid(0)", u.call(SETFSUID, &[0]), 1000)?;
    expect("setfsgid(0)", u.call(SETFSGID, &[0]), 1000)?;
    kassert!(u.p.mm_flags.dumpable() == 1, "a no-op keeps the memory dumpable");
    let mut target = root("ab13-target")?;
    expect("target to 2000/2000/0", target.call(SETRESUID, &[2000, 2000, 0]), 0)?;
    expect("user kills 2000/2000/0", u.call(nr::KILL, &[target.p.pid as u64, 0]), -EPERM)?;
    // euid 2000 is not privileged, but the saved 0 is allowed back; as root again it sets a saved 1000.
    expect("target back to root", target.call(SETRESUID, &[NONE, 0, NONE]), 0)?;
    expect("target to 2000/2000/1000", target.call(SETRESUID, &[2000, 2000, 1000]), 0)?;
    expect("user kills a saved 1000", u.call(nr::KILL, &[target.p.pid as u64, 0]), 0)?;
    let mut other = T::with("ab13-other", |o| o.uid = 1001)?;
    expect("1001 kills 2000/2000/1000", other.call(nr::KILL, &[target.p.pid as u64, 0]), -EPERM)?;
    expect("prlimit of another uid", other.call(nr::PRLIMIT64, &[u.p.pid as u64, 7, 0, u.a(0)]), -EPERM)?;
    Ok(())
}

/// Runs BusyBox with `argv` as `uid` (all ids; no groups) and returns its exit code and output.
fn busybox(argv: &[&str], uid: u32) -> Result<(i32, String), String> {
    run("/bin/busybox", argv, uid)
}

/// Runs the program at `path` with `argv` as `uid` (all ids; no groups) and returns its exit code and output.
fn run(path: &str, argv: &[&str], uid: u32) -> Result<(i32, String), String> {
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["PATH=/bin", "HOME=/"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as alloc::sync::Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as alloc::sync::Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as alloc::sync::Arc<dyn vfs::File>);
    o.uid = uid;
    let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn: {e}"))?;
    let Some(code) = p.wait(Some(60 * 1_000_000_000)) else {
        p.kill(9);
        let _ = p.wait(Some(10 * 1_000_000_000));
        return Err(format!("{argv:?} did not end within 60 s"));
    };
    Ok((code, String::from_utf8_lossy(&out.contents()).into_owned()))
}

/// `id` adds "(name)" after each number when /etc/passwd and /etc/group know it (the stick has them, QEMU's
/// image does not).
fn without_names(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

ktest!("ab.cred.chpst", Safe, chpst);
/// The proof Georg's Cobalt start needs (desk-minipc-w7.md): BusyBox `chpst -u 1000:1000 id` as root, and a
/// `su`-like drop from a root shell whose own ids stay root (fork copies, execve keeps the dropped ids).
fn chpst() -> TestResult {
    if !crate::ab7_ktests::have("/bin/busybox") {
        return ktest::skip("no /bin/busybox in this image");
    }
    let (code, out) = busybox(&["busybox", "chpst", "-u", "1000:1000", "id"], 0)?;
    let out = without_names(&out);
    kassert!(code == 0 && out == "uid=1000 gid=1000 groups=1000\n", "chpst id: code {code}, {out:?}");
    let script = "chpst -u 1000:1000 sh -c 'id; grep -E \"^(Uid|Gid|Groups):\" /proc/self/status; chpst -u 0:0 id || echo no way back'; id -u";
    let (code, out) = busybox(&["sh", "-c", script], 0)?;
    let out = without_names(&out);
    let want = "uid=1000 gid=1000 groups=1000\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nGroups:\t1000 \n";
    kassert!(code == 0 && out.starts_with(want) && out.ends_with("no way back\n0\n"), "su-like drop: code {code}, {out:?}");
    // As a user, chpst cannot switch (setgroups is EPERM).
    let (code, _) = busybox(&["busybox", "chpst", "-u", "0:0", "id"], 1000)?;
    kassert!(code != 0, "a user's chpst -u 0:0 succeeded");
    Ok(())
}

ktest!("ab.cred.caps", Safe, caps);
/// capget/capset over the ids (kernel/capability.c): root's sets, the version handshake, a seteuid that
/// keeps the permitted set, and capset that only keeps the current sets (V2 stores none).
fn caps() -> TestResult {
    const V3: u32 = 0x2008_0522;
    let full = proc::prctl_state::CAP_FULL_SET;
    let mut t = root("ab13-caps")?;
    let get = |t: &mut T, pid: u32| -> Result<[u64; 3], String> {
        t.put(0, &[V3.to_le_bytes(), pid.to_le_bytes()].concat());
        expect("capget", t.call(CAPGET, &[t.a(0), t.a(16)]), 0)?;
        let w = |i: u64| t.u32_at(16 + 4 * i) as u64;
        Ok([w(0) | w(3) << 32, w(1) | w(4) << 32, w(2) | w(5) << 32])
    };
    kassert!(get(&mut t, 0)? == [full, full, 0], "root's sets");
    let pid = t.p.pid;
    let mut u = T::new("ab13-caps-user")?;
    kassert!(get(&mut u, 0)? == [0, 0, 0], "a user's sets");
    kassert!(get(&mut u, pid)? == [full, full, 0], "a user reads root's sets");
    t.put(0, &[0x1234u32.to_le_bytes(), 0u32.to_le_bytes()].concat());
    expect("bad version", t.call(CAPGET, &[t.a(0), t.a(16)]), -EINVAL)?;
    kassert!(t.u32_at(0) == V3, "preferred version written back");
    t.put(0, &[0x1234u32.to_le_bytes(), 0u32.to_le_bytes()].concat());
    expect("bad version, NULL data", t.call(CAPGET, &[t.a(0), 0]), 0)?;
    t.put(0, &[V3.to_le_bytes(), 0x7fff_fff0u32.to_le_bytes()].concat());
    expect("no such task", t.call(CAPGET, &[t.a(0), t.a(16)]), -ESRCH)?;
    // capset: the same sets succeed; a lowering is refused (no stored sets); another pid is EPERM.
    let set = |t: &mut T, pid: u32, s: [u64; 3]| {
        let mut b = [V3.to_le_bytes(), pid.to_le_bytes()].concat();
        for i in 0..2 {
            for x in s {
                b.extend_from_slice(&((x >> (32 * i)) as u32).to_le_bytes());
            }
        }
        t.put(0, &b);
        t.call(CAPSET, &[t.a(0), t.a(8)])
    };
    expect("capset same", set(&mut t, 0, [full, full, 0]), 0)?;
    expect("capset lower", set(&mut t, 0, [0, full, 0]), -EPERM)?;
    expect("capset other pid", set(&mut t, u.p.pid, [full, full, 0]), -EPERM)?;
    expect("user capset raise", set(&mut u, 0, [0, 1, 0]), -EPERM)?;
    // seteuid(1000): effective empty, permitted kept (the saved uid is 0); back to root restores it.
    expect("seteuid(1000)", t.call(SETRESUID, &[NONE, 1000, NONE]), 0)?;
    kassert!(get(&mut t, 0)? == [0, full, 0], "after seteuid");
    expect("seteuid(0)", t.call(SETRESUID, &[NONE, 0, NONE]), 0)?;
    kassert!(get(&mut t, 0)? == [full, full, 0], "back to root");
    Ok(())
}

ktest!("ab.cred.inherit", Safe, inherit);
/// fork copies every id and the groups, execve keeps them with saved = effective (AT_SECURE as the
/// effective ids differ from the real ones); the program has a second thread, so musl repeats each set*id
/// in it (`__synccall`) against V2's one set per group.
fn inherit() -> TestResult {
    if !crate::ab7_ktests::have("/bin/abi-cred") {
        return ktest::skip("no /bin/abi-cred in this image (no musl sysroot when it was built)");
    }
    let (code, out) = run("/bin/abi-cred", &["/bin/abi-cred", "root"], 0)?;
    let want = "setgroups = 0\nsetresgid(7, 8, 9) = 0\nsetresuid(2000, 1000, 0) = 0\n\
                self Uid:\t2000\t1000\t0\t1000\nself Gid:\t7\t8\t9\t8\nself Groups:\t5 1000 \n\
                child Uid:\t2000\t1000\t0\t1000\nchild Gid:\t7\t8\t9\t8\nchild Groups:\t5 1000 \nchild = 0\n\
                exec Uid:\t2000\t1000\t1000\t1000\nexec Gid:\t7\t8\t8\t8\nexec Groups:\t5 1000 \nAT_SECURE 1\n";
    kassert!(code == 0 && out == want, "abi-cred root: exit {code}\n{out}");
    Ok(())
}

/// Whether FS has applied change requests 2 and 3 of plan/notes/ab-AB-13.md (file checks on fsuid/fsgid and
/// the supplementary groups). The desk sets it to true in the same integration; until then a difference
/// in `ab.cred.files` is a skip that names the gap, afterwards it is a failure.
const FS_CR_MERGED: bool = true;

ktest!("ab.cred.files", Safe, files);
/// File access follows setfsuid, setuid and the supplementary groups without an execve (Linux
/// fs/namei.c `acl_permission_check`, kernel/groups.c `in_group_p`).
fn files() -> TestResult {
    if !crate::ab7_ktests::have("/bin/abi-cred") {
        return ktest::skip("no /bin/abi-cred in this image (no musl sysroot when it was built)");
    }
    let (code, out) = run("/bin/abi-cred", &["/bin/abi-cred", "files"], 0)?;
    let want = "files made: yes\nfsuid 1000: root's 0600 file = -EACCES\nfsuid 0: root's 0600 file = 0\n\
                dropped to 1000 with group 50: yes\nuid 1000: root's 0600 file = -EACCES\n\
                uid 1000: 0040 file of group 50 = 0\nuid 1000: 0040 file of group 51 = -EACCES\n";
    if code == 0 && out == want {
        return Ok(());
    }
    if !FS_CR_MERGED && code == 0 && out.starts_with("files made: yes\n") {
        klog::event!(Warn, "ktest", "cred.files_gap", lines = out.lines().count(); "file checks still use the exec-time ids");
        return ktest::skip("FS change requests 2+3 of ab-AB-13.md not applied: file checks use the exec-time uid/gid, no supplementary groups");
    }
    Err(format!("abi-cred files: exit {code}\n{out}"))
}
