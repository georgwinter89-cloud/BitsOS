//! SX-3a ktests (all Safe, rerunnable: every view they make is private and goes when they end):
//! - `sx.mnt.views`: in the kernel, on views of their own: a locked unprivileged copy, binds, propagation into
//!   the copy, pivot_root with a lazy unmount, mountinfo seen from a root, and everything given back;
//! - `sx.mnt.programs`: `/bin/abi-ns-mount` (user/abi-tests/c/ns-mount.c, the bwrap-like container and its
//!   propagation, error, clone, setns and read-only overlay cases) as uid 1000 against expected/sx3-*.txt (CachyOS's transcripts; the
//!   errors case ends with Linux 6.6's answer to MNT_FORCE);
//! - `sx.mnt.errors`: the refusals (ELOOP, EINVAL for unbindable, a shared parent of pivot_root and a locked
//!   copy) on views of its own, each leaving the view as it was;
//! - `sx.mnt.lifetime`: the programs' mount namespaces are gone when they end (`proc::ns::mnt::live`);
//! - `sx.mnt.proc_instance` (SX-3b): a proc mount is an instance of its own from the registries (no EBUSY on a
//!   second one), and `sx.mnt.programs` also runs the pidproc, sockbind and nslimit cases (SX-3c: and fdpath);
//! - `sx.mnt.pdeath` (SX-3b fixer): the parent-death signal of a pid namespace's init (skips without CR1);
//! - `sx.bwrap.smoke` (SX-3b fixer): /bin/bwrap on the stock root, a pid namespace with its own /proc (skips
//!   without CR2).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use vfs::mount_ops::{MNT_DETACH, Propagation};
use vfs::ops::Ctx;
use vfs::{MountNs, tmpfs};

const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/abi-ns-mount";

fn e(what: &str) -> impl Fn(i64) -> String + '_ {
    move |x| format!("{what}: {x}")
}

ktest!("sx.mnt.views", Safe, views);
fn views() -> TestResult {
    // A view of its own: /a (shared tmpfs), /b, /c, /d.
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    for d in ["/a", "/b", "/c", "/d"] {
        c.mkdir(None, d.as_bytes(), 0o755).map_err(e(d))?;
    }
    let (sb, root) = tmpfs::new_fs(0o755);
    ns.mount(&c.lookup(None, b"/a", true).map_err(e("/a"))?, sb, root, false).map_err(e("mount /a"))?;
    let a = c.lookup(None, b"/a", true).map_err(e("/a"))?;
    ns.change_type(&a, Propagation::Shared, false).map_err(e("shared"))?;
    c.mkdir(None, b"/a/sub", 0o755).map_err(e("/a/sub"))?;
    // An unprivileged copy: slaves, locked.
    let copy = Arc::new(ns.copy_for(0x5a3a));
    let cc = Ctx::kernel(&copy);
    let ca = cc.lookup(None, b"/a", true).map_err(e("copy /a"))?;
    kassert!(ca.mnt.flags() & vfs::MNT_LOCKED != 0 && ca.mnt.propagation().1 == a.mnt.propagation().0, "copy of /a: flags {:#x} prop {:?}", ca.mnt.flags(), ca.mnt.propagation());
    kassert!(copy.umount_user(ca.clone(), 0) == Err(uapi::errno::EINVAL), "a locked mount is not unmounted alone");
    // A mount in the source reaches the slave.
    let (sb2, r2) = tmpfs::new_fs(0o755);
    ns.mount(&c.lookup(None, b"/a/sub", true).map_err(e("/a/sub"))?, sb2, r2, false).map_err(e("mount /a/sub"))?;
    c.mkdir(None, b"/a/sub/seen", 0o755).map_err(e("seen"))?;
    kassert!(cc.lookup(None, b"/a/sub/seen", true).is_ok(), "propagated into the copy");
    // Bind, pivot_root into a tmpfs base, lazy unmount of the old root.
    let (sb3, r3) = tmpfs::new_fs(0o755);
    copy.add_mount(&cc.lookup(None, b"/d", true).map_err(e("/d"))?, sb3, r3, vfs::MNT_RELATIME).map_err(e("base"))?;
    cc.mkdir(None, b"/d/old", 0o755).map_err(e("old"))?;
    cc.mkdir(None, b"/d/a", 0o755).map_err(e("d/a"))?;
    copy.bind(&cc.lookup(None, b"/d/a", true).map_err(e("d/a"))?, &ca, true).map_err(e("bind"))?;
    let root = copy.root_path();
    let new = cc.lookup(None, b"/d", true).map_err(e("/d"))?;
    copy.pivot_root(&root, &new, &cc.lookup(None, b"/d/old", true).map_err(e("old"))?).map_err(e("pivot_root"))?;
    let cc = Ctx::kernel(&copy);
    let old = copy.cross_down(cc.lookup(None, b"/old", true).map_err(e("/old"))?);
    copy.umount_user(old, MNT_DETACH).map_err(e("umount old"))?;
    let text = vfs::mountinfo::render(&copy, &copy.root_path(), true);
    kassert!(text.lines().count() == 3, "base, /a and /a/sub stay:\n{text}");
    kassert!(cc.lookup(None, b"/a/sub/seen", true).is_ok(), "the bound tree is whole");
    // Everything goes with the views.
    let w = Arc::downgrade(&copy);
    drop((cc, ca, new, root));
    drop(copy);
    kassert!(w.upgrade().is_none(), "the copy outlived its users");
    Ok(())
}

ktest!("sx.mnt.errors", Safe, errors);
/// The refusals of the mount calls on views of its own (no program and no procfs needed): a move into its own
/// subtree (ELOOP), a bind of an unbindable mount (EINVAL), pivot_root below a shared mount (EINVAL), the lone
/// unmount of a locked copy (EINVAL); after each refusal the view is unchanged.
fn errors() -> TestResult {
    use uapi::errno::{ELOOP, EINVAL};
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    for d in ["/a", "/b", "/c", "/d"] {
        c.mkdir(None, d.as_bytes(), 0o755).map_err(e(d))?;
    }
    for d in ["/a", "/b", "/d"] {
        let (sb, root) = tmpfs::new_fs(0o755);
        ns.mount(&c.lookup(None, d.as_bytes(), true).map_err(e(d))?, sb, root, false).map_err(e("mount"))?;
    }
    c.mkdir(None, b"/a/x", 0o755).map_err(e("/a/x"))?;
    c.mkdir(None, b"/d/old", 0o755).map_err(e("/d/old"))?;
    let before = vfs::mountinfo::render(&ns, &ns.root_path(), true);
    let a = c.lookup(None, b"/a", true).map_err(e("/a"))?;
    let r = ns.move_mount(&a, &c.lookup(None, b"/a/x", true).map_err(e("/a/x"))?);
    kassert!(r == Err(ELOOP), "move into its own subtree: {r:?}");
    let b = c.lookup(None, b"/b", true).map_err(e("/b"))?;
    ns.change_type(&b, Propagation::Unbindable, false).map_err(e("unbindable"))?;
    for rec in [false, true] {
        let r = ns.bind(&c.lookup(None, b"/c", true).map_err(e("/c"))?, &b, rec).map(|_| ());
        kassert!(r == Err(EINVAL), "bind (rec {rec}) of an unbindable mount: {r:?}");
    }
    ns.change_type(&b, Propagation::Private, false).map_err(e("private"))?;
    let root = ns.root_path();
    ns.change_type(&root, Propagation::Shared, false).map_err(e("shared root"))?;
    let new = c.lookup(None, b"/d", true).map_err(e("/d"))?;
    let r = ns.pivot_root(&root, &new, &c.lookup(None, b"/d/old", true).map_err(e("/d/old"))?);
    kassert!(r == Err(EINVAL), "pivot_root below a shared mount: {r:?}");
    ns.change_type(&root, Propagation::Private, false).map_err(e("private root"))?;
    let after = vfs::mountinfo::render(&ns, &ns.root_path(), true);
    kassert!(before == after, "the refusals changed the view:\n{before}\n--\n{after}");
    let copy = ns.copy_for(0x5a3b);
    let cc = Ctx::kernel(&copy);
    let ca = cc.lookup(None, b"/a", true).map_err(e("copy /a"))?;
    for flags in [0, MNT_DETACH] {
        let r = copy.umount_user(ca.clone(), flags);
        kassert!(r == Err(EINVAL), "lone unmount (flags {flags:#x}) of a locked copy: {r:?}");
    }
    kassert!(copy.mounts().len() == 4, "the copy keeps its four mounts: {}", copy.mounts().len());
    Ok(())
}

fn have(path: &str) -> bool {
    vfs::root_ns().lookup_path(path.as_bytes(), true).is_ok()
}

/// Runs `PROG arg` as uid 1000 and returns (exit code, output).
fn run(arg: &str) -> Result<(i32, String), String> {
    let argv: Vec<&str> = if arg.is_empty() { Vec::from([PROG]) } else { Vec::from([PROG, arg]) };
    run_argv(&argv)
}

/// Runs `argv` (argv[0] is the program's path) as uid 1000 and returns (exit code, output); 120 s bound.
fn run_argv(argv: &[&str]) -> Result<(i32, String), String> {
    let out = proc::stdio::Capture::new(1 << 20);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let prog = argv[0];
    let p = proc::spawn_with(prog, &o).map_err(|e| format!("spawn {prog}: {e}"))?;
    let code = match p.wait(Some(120 * SEC)) {
        Some(c) => c,
        None => {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("{} did not end within 120 s:\n{}", argv.join(" "), String::from_utf8_lossy(&out.contents())));
        }
    };
    Ok((code, String::from_utf8_lossy(&out.contents()).into_owned()))
}

fn diff(got: &str, want: &str) -> Vec<String> {
    let g: Vec<&str> = got.lines().collect();
    let w: Vec<&str> = want.lines().collect();
    let mut d = Vec::new();
    for i in 0..g.len().max(w.len()) {
        match (g.get(i), w.get(i)) {
            (Some(a), Some(b)) if a == b => {}
            (a, b) => d.push(format!("line {}: got {a:?}, want {b:?}", i + 1)),
        }
    }
    d
}

ktest!("sx.mnt.programs", Safe, programs);
fn programs() -> TestResult {
    let cases: [(&str, &str); 11] = [
        ("", include_str!("../../../user/abi-tests/expected/sx3-mount.txt")),
        ("overlay", include_str!("../../../user/abi-tests/expected/sx3-mount-overlay.txt")),
        ("prop", include_str!("../../../user/abi-tests/expected/sx3-mount-prop.txt")),
        ("errors", include_str!("../../../user/abi-tests/expected/sx3-mount-errors.txt")),
        ("clone", include_str!("../../../user/abi-tests/expected/sx3-mount-clone.txt")),
        ("setns", include_str!("../../../user/abi-tests/expected/sx3-mount-setns.txt")),
        // SX-3b: proc in a new pid namespace is that namespace's instance; a socket bound onto a socket.
        ("pidproc", include_str!("../../../user/abi-tests/expected/sx3-mount-pidproc.txt")),
        ("sockbind", include_str!("../../../user/abi-tests/expected/sx3-mount-sockbind.txt")),
        ("nslimit", include_str!("../../../user/abi-tests/expected/sx3-mount-nslimit.txt")),
        // SX-3c: fd links and getcwd of places the root does not reach (pressure-vessel's fds from outside).
        ("fdpath", include_str!("../../../user/abi-tests/expected/sx3-mount-fdpath.txt")),
        // SX-3c fixer: link(2)/linkat(2) on tmpfs as --copy-runtime uses them.
        ("hardlink", include_str!("../../../user/abi-tests/expected/sx3-mount-hardlink.txt")),
    ];
    if !have(PROG) {
        if have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-ns-mount (did it fail to build?)"));
        }
        return ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    // /proc/<pid>/mountinfo shows the caller's own view only with AB's procfs glue (SX-3a CR 1).
    let (_, probe) = run("probe")?;
    if probe.contains("own view in mountinfo = no") {
        return ktest::skip("procfs still renders the boot view (change request SX-3a CR 1 not applied)");
    }
    for (arg, want) in cases {
        let (code, text) = run(arg)?;
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "{PROG} {arg} differs from the expected transcript (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "sx.mnt.program", mode = arg, lines = text.lines().count());
    }
    Ok(())
}

ktest!("sx.mnt.lifetime", Safe, lifetime);
fn lifetime() -> TestResult {
    if !have(PROG) {
        return ktest::skip("no /bin/abi-ns-mount in this image");
    }
    let before = proc::ns::mnt::live();
    let (code, _) = run("prop")?;
    kassert!(code == 0, "prop mode exit {code}");
    let mut left = 0;
    for _ in 0..50 {
        left = proc::ns::mnt::live();
        if left <= before {
            return Ok(());
        }
        sched::sleep_ns(100_000_000);
    }
    Err(format!("mount namespaces live: {left}, before the run {before}"))
}

// SX-3b: a proc mount is a new instance from the real registries, bound to a pid namespace (here the initial
// one, from the kernel): it has the registered files (/proc/sys/user, /proc/meminfo), the pid part, and a
// second instance on the same place is no EBUSY (the boot /proc reused was).
ktest!("sx.mnt.proc_instance", Safe, proc_instance);
fn proc_instance() -> TestResult {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/p", 0o755).map_err(e("/p"))?;
    let key: Arc<dyn core::any::Any + Send + Sync> = proc::ns::pid::init_pid_ns().clone();
    let mut devs = Vec::new();
    for round in 0..2 {
        let (sb, root) = vfs::proc_instance::instance(key.clone()).map_err(e("instance"))?;
        kassert!(sb.fstype == "proc", "fstype {}", sb.fstype);
        devs.push(sb.dev);
        let at = c.lookup(None, b"/p", true).map_err(e("/p"))?;
        ns.add_mount(&at, sb, root, 0).map_err(|x| format!("mount {round}: errno {x}"))?;
        let limit = ns.read_file("/p/sys/user/max_user_namespaces", 64).map_err(e("max_user_namespaces"))?;
        kassert!(limit.ends_with(b"\n") && limit.len() > 1, "max_user_namespaces = {:?}", String::from_utf8_lossy(&limit));
        kassert!(ns.read_file("/p/meminfo", 1 << 16).is_ok(), "meminfo in the instance");
        // The pid part is there (processes may not exist yet when ktests run: its fixed link).
        kassert!(ns.read_link("/p/mounts").map_err(e("mounts link"))? == b"self/mounts", "the pid part of the instance");
    }
    kassert!(devs[0] != devs[1] && devs[0] != vfs::root_ns().lookup_path(b"/proc", true).map_err(e("/proc"))?.mnt.sb.dev, "every mount a super block of its own: {devs:?}");
    Ok(())
}

ktest!("sx.mnt.pdeath", Safe, pdeath);
/// SX-3b: the parent-death SIGKILL of a pid namespace's init whose parent is outside the namespace (bwrap
/// `--die-with-parent --unshare-pid`, the suite's check 38). Skips while AB's glue (SX-3b CR1,
/// plan/notes/sx-SX-3b-glue.patch) is not applied: the init then keeps running and the program kills it.
fn pdeath() -> TestResult {
    if !have(PROG) {
        return ktest::skip("no /bin/abi-ns-mount in this image");
    }
    let (code, text) = run("pdeath")?;
    if text.contains("still running after 5 s") {
        return ktest::skip("the parent-death signal does not reach a pid-namespace init (SX-3b CR1, crates/proc/src/exit.rs send_pdeath, not applied)");
    }
    let d = diff(&text, include_str!("../../../user/abi-tests/expected/sx3-mount-pdeath.txt"));
    kassert!(code == 0 && d.is_empty(), "{PROG} pdeath differs from the expected transcript (exit {code}):\n{}", d.join("\n"));
    Ok(())
}

ktest!("sx.bwrap.smoke", Safe, bwrap_smoke);
/// SX-3b: the real bwrap (user/ports/bubblewrap) on the stock root, without the scenario's outer wrapper: a new
/// user and pid namespace with its own /proc, where bwrap's own init is pid 1, the shell pid 2 and its readlink
/// pid 3 (CachyOS's transcript of the same command). Skips while the image has no /bin/bwrap (SX-3b CR2: xtask's
/// image puts it there).
fn bwrap_smoke() -> TestResult {
    const BWRAP: &str = "/bin/bwrap";
    if !have(BWRAP) {
        return ktest::skip("no /bin/bwrap in this image (SX-3b CR2 not applied)");
    }
    let argv = [BWRAP, "--ro-bind", "/", "/", "--dev", "/dev", "--unshare-user", "--unshare-pid", "--proc", "/proc", "--die-with-parent", "/bin/sh", "-c", "echo pid=$$; readlink /proc/self; test -d /proc/1 && echo proc=own"];
    let (code, text) = run_argv(&argv)?;
    kassert!(code == 0, "bwrap ended with {code}:\n{text}");
    kassert!(text == "pid=2\n3\nproc=own\n", "bwrap's sandbox differs from CachyOS's \"pid=2 3 proc=own\":\n{text}");
    Ok(())
}
