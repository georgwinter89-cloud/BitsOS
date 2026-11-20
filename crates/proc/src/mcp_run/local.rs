//! MC-L: the process layer tells the local tools door who a pid is (`mcp::local::door::set_lookup`): its
//! subject, the image's executable (`ImageInfo::exe`, Linux `mm->exe_file`) and the image itself: a `Process`
//! is one program image, so the same pid after an exit or an execve is another object and the door sees it;
//! and whether the image's memory is open to other programs of its user (dumpable), which the door refuses.
//! And the ktests of that door with real programs (`mc.local.user_*`).

use super::Stream;
use crate::{Process, SpawnOptions};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use mcp::local::door::{self, Proc, ProgramDoor};
use mcp::local::{By, Local, Peer};
use uapi::ab::signal::SIGKILL;
use vfs::File;

fn lookup(pid: u32) -> Option<Proc> {
    let p = crate::find(pid)?;
    let exe = p.image_info.lock().exe.clone();
    // `may_ptrace`: process_vm_readv/writev and /proc/<pid>/mem reach exactly the images with SUID_DUMP_USER.
    let dumpable = p.mm_flags.dumpable() == 1;
    Some(Proc { subject: p.subject(), exe, finished: p.is_finished(), native: crate::peer::native(&p), dumpable, image: p })
}

/// Closes a new image of the local tools door's program to the other programs of its user (not dumpable, as
/// execve makes a set-id image): process_vm_readv/writev, pidfd_getfd and /proc/<pid>/mem then need
/// CAP_SYS_PTRACE, and the door admits only such an image. execve and spawn call it before the image runs
/// (INT-CL3T), so nobody writes into the program before its first instruction. A fork of the program copies
/// the flag. Not closed by this: an image in a user namespace somebody made (its maker holds the capability
/// there); the door and settingsd refuse such an image by itself (`peer::native`).
pub(crate) fn close_door_program(image: &Process, exe: Option<&vfs::Path>) {
    if exe.is_some_and(door::protects) {
        image.mm_flags.set_dumpable(0);
    }
}

fn install() -> Result<(), &'static str> {
    door::set_lookup(lookup);
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "proc.mcp_local_lookup", install);

const SEC: u64 = 1_000_000_000;
const FAKE: &str = "/bin/mcp-fakesvc";
const ASSIST_COPY: &str = "/tmp/mcl/assist";
const FAKE_COPY: &str = "/tmp/mcl/mcp-fakesvc";

fn end(p: &Process) {
    p.kill(SIGKILL);
    let _ = p.wait(Some(5 * SEC));
}

/// Made but never run (held before its first instruction): a live process of `path`, as spawn leaves it
/// (the door's program closed by `close_door_program`).
fn held(path: &str, name: &str, uid: u32) -> Result<Arc<Process>, String> {
    let argv = [name];
    let mut o = SpawnOptions::new(&argv, &[], super::MCP_PROGRAM);
    o.uid = uid;
    o.held = true;
    crate::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))
}

/// The socket's record when `p` itself called connect.
fn by(p: &Arc<Process>) -> By {
    By::Image(Some(p.clone() as Arc<dyn core::any::Any + Send + Sync>))
}

/// The lookup's answer for a process whose namespaces are somebody's own.
fn apart(pid: u32) -> Option<Proc> {
    lookup(pid).map(|p| Proc { native: false, ..p })
}

/// A byte-for-byte copy of `from` at `to` (another file: another inode).
fn copy(from: &str, to: &str) -> Result<(), String> {
    let ns = vfs::root_ns();
    let bytes = crate::read_program(&ns, from).map_err(|e| format!("read {from}: {e}"))?;
    ns.mkdir_p("/tmp/mcl").map_err(|e| format!("mkdir: {e}"))?;
    let c = vfs::Ctx::kernel(&ns);
    let _ = c.unlink(None, to.as_bytes());
    let f = c.open(None, to.as_bytes(), vfs::abi::O_WRONLY | vfs::abi::O_CREAT | vfs::abi::O_TRUNC, 0o755).map_err(|e| format!("create {to}: {e}"))?;
    let n = f.write(&bytes).map_err(|e| format!("write {to}: {e}"))?;
    kassert!(n == bytes.len(), "short write {n}");
    Ok(())
}

fn present(path: &str) -> TestResult {
    let ns = vfs::root_ns();
    if vfs::Ctx::kernel(&ns).lookup(None, path.as_bytes(), true).is_err() {
        return ktest::fail(&format!("{path} is not in this image (the user workspace was not built into it)"));
    }
    Ok(())
}

fn peer(p: &Process, uid: u32) -> Peer {
    Peer { pid: p.pid, uid, gid: uid }
}

/// The kernel's own door: the boot's /bin/assist passes; the same with its memory open to other programs,
/// another program, a copy of the assistant under its name, a peer whose uid is not the process's, a process
/// that ended and a pid nobody has do not. Safe: it only reads /bin and writes below /tmp/mcl.
fn identity() -> TestResult {
    present(door::PROGRAM)?;
    present(FAKE)?;
    let d = ProgramDoor::kernel();
    let real = held(door::PROGRAM, "assist", 1000)?;
    let other = held(FAKE, "assist", 1000);
    let r = (|| -> TestResult {
        let p = d.judge(peer(&real, 1000), &by(&real)).map_err(|e| format!("the boot's assistant is refused: {e}"))?;
        kassert!(p.subject.pid == real.pid && p.subject.uid == 1000, "subject {:?}", p.subject);
        // spawn closed the assistant's image (`close_door_program`) and left every other program's as it was.
        kassert!(real.mm_flags.dumpable() == 0, "the assistant's memory is open to other programs");
        // An image other programs of uid 1000 can write into (process_vm_writev) is not the assistant.
        real.mm_flags.set_dumpable(1);
        let open = d.judge(peer(&real, 1000), &by(&real));
        real.mm_flags.set_dumpable(0);
        kassert!(open.is_err_and(|e| e.contains("memory is open")), "an assistant with open memory passed");
        kassert!(d.judge(peer(&real, 0), &by(&real)).is_err_and(|e| e.contains("connected as uid 0")), "a peer with another uid than its process passed");
        let other = other.as_ref().map_err(|e| e.clone())?;
        kassert!(other.mm_flags.dumpable() == 1, "another program was closed as if it were the assistant");
        kassert!(d.judge(peer(other, 1000), &by(other)).is_err_and(|e| e.contains("does not run the boot's")), "another program called assist passed");
        // Connect, then become the assistant: the socket's record names the image that called connect, and
        // the assistant's pid is not that image. The same for a record whose image is gone.
        for (what, record) in [("another image connected", by(other)), ("the image that connected is gone", By::Image(None))] {
            kassert!(d.judge(peer(&real, 1000), &record).is_err_and(|e| e.contains("not the program that connected")), "{what}, and the assistant's pid passed");
        }
        // A link without a record (host tests) still asks the pid alone.
        kassert!(d.judge(peer(&real, 1000), &By::Untold).is_ok(), "the pid alone no longer passes where no record exists");
        // The assistant in a namespace somebody made is somebody's puppet.
        let puppet = ProgramDoor::with(door::PROGRAM, Some(apart));
        kassert!(puppet.judge(peer(&real, 1000), &by(&real)).is_err_and(|e| e.contains("namespace of its own")), "an assistant in a namespace of its own passed");
        copy(door::PROGRAM, ASSIST_COPY)?;
        let fake = held(ASSIST_COPY, "assist", 1000)?;
        let v = d.judge(peer(&fake, 1000), &by(&fake));
        end(&fake);
        kassert!(v.is_err_and(|e| e.contains("does not run the boot's")), "a copy of the assistant passed");
        kassert!(d.judge(Peer { pid: 0, uid: 0, gid: 0 }, &By::Image(None)).is_err(), "pid 0 passed");
        kassert!(d.judge(Peer { pid: 0x3fff_fff0, uid: 1000, gid: 1000 }, &By::Untold).is_err_and(|e| e.contains("does not run")), "a pid nobody has passed");
        Ok(())
    })();
    let pid = real.pid;
    end(&real);
    if let Ok(o) = &other {
        end(o);
    }
    let _ = vfs::Ctx::kernel(&vfs::root_ns()).unlink(None, ASSIST_COPY.as_bytes());
    r?;
    kassert!(d.judge(Peer { pid, uid: 1000, gid: 1000 }, &By::Untold).is_err(), "the assistant passed after it ended");
    Ok(())
}
ktest!("mc.local.user_identity", Safe, identity);

/// What a socket keeps of the program that connects (`peer::record`): the image itself and the label
/// SO_PEERSEC gives; and `peer::native` with real namespaces: an assistant whose user made a user namespace
/// around it, and one in a copy of the mount view, are not where the system's programs live.
fn record() -> TestResult {
    use uapi::ab::clone::{CLONE_NEWNS, CLONE_NEWUSER};
    present(door::PROGRAM)?;
    let real = held(door::PROGRAM, "assist", 1000)?;
    let boxed = held(door::PROGRAM, "assist", 1000);
    let rooted = held(door::PROGRAM, "assist", 0);
    let r = (|| -> TestResult {
        let at = vfs::Ctx::kernel(&vfs::root_ns()).lookup(None, door::PROGRAM.as_bytes(), true).map_err(|e| format!("lookup: errno {e}"))?.attr();
        let rec = crate::peer::record(&real);
        let want = format!("bitsos:program:1:init:{}:{}:{}", at.dev, at.ino, door::PROGRAM);
        kassert!(rec.label == want.as_bytes(), "label {:?}, want {want:?}", String::from_utf8_lossy(&rec.label));
        kassert!(crate::peer::image(&rec).is_some_and(|i| Arc::ptr_eq(&i, &real)), "the record is not the image");
        kassert!(crate::peer::native(&real), "a program started plainly is not native");
        let d = ProgramDoor::kernel();
        // Root's own user namespace around the program: uid 1000 makes it, as any program may.
        let boxed = boxed.as_ref().map_err(|e| e.clone())?;
        let t = boxed.threads().into_iter().next().ok_or("no thread")?;
        boxed.unshare(&t, CLONE_NEWUSER).map_err(|e| format!("unshare(user): errno {e}"))?;
        kassert!(!crate::peer::native(boxed), "a program in a user namespace of its own counts as native");
        let label = crate::peer::record(boxed).label.clone();
        kassert!(label.starts_with(b"bitsos:program:1:own:"), "label {:?}", String::from_utf8_lossy(&label));
        kassert!(d.judge(peer(boxed, 1000), &by(boxed)).is_err_and(|e| e.contains("namespace of its own")), "the door admits an assistant in a user namespace");
        // A copy of the mount view (root may make one without a user namespace).
        let rooted = rooted.as_ref().map_err(|e| e.clone())?;
        let t = rooted.threads().into_iter().next().ok_or("no thread")?;
        rooted.unshare(&t, CLONE_NEWNS).map_err(|e| format!("unshare(mount): errno {e}"))?;
        kassert!(!crate::peer::native(rooted), "a program in a mount namespace of its own counts as native");
        kassert!(d.judge(peer(rooted, 0), &by(rooted)).is_err_and(|e| e.contains("namespace of its own")), "the door admits an assistant in a mount namespace");
        Ok(())
    })();
    end(&real);
    for p in [&boxed, &rooted].into_iter().flatten() {
        end(p);
    }
    r
}
ktest!("mc.local.user_record", Safe, record);

fn until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) -> TestResult {
    let end = time::now_ns() + secs * SEC;
    while !cond() {
        if time::now_ns() >= end {
            return Err(format!("{what}: not within {secs} s"));
        }
        sched::sleep_ns(10_000_000);
    }
    Ok(())
}

const LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
const VERSION: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"version","arguments":{}}}"#;
const READ_FILE: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/etc/passwd"}}}"#;

/// Runs `program ask <socket> <lines>` as uid 1000 and gives what it printed once it ended. `closed`: its
/// memory is closed to other programs before it runs (see `held`).
fn ask(program: &str, socket: &str, lines: &[&str], closed: bool) -> Result<String, String> {
    let mut argv = alloc::vec!["mcp-fakesvc", "ask", socket];
    argv.extend_from_slice(lines);
    let out = Stream::new(256 * 1024);
    let mut o = SpawnOptions::new(&argv, &[], super::MCP_PROGRAM);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.held = true;
    let p = crate::spawn_with(program, &o).map_err(|e| format!("spawn {program}: {e}"))?;
    if closed {
        p.mm_flags.set_dumpable(0);
    }
    p.release();
    let done = p.wait(Some(15 * SEC));
    if done.is_none() {
        end(&p);
    }
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    kassert!(done.is_some(), "{program} ask did not end; it printed: {text}");
    Ok(text)
}

fn stopped(local: &Local) -> TestResult {
    local.stop();
    let ok = local.wait_stopped(5 * SEC);
    sched::reap();
    kassert!(ok, "the door's threads did not end");
    Ok(())
}

/// A real program of uid 1000 at a real socket, with a door of the test's own that admits the boot's
/// /bin/mcp-fakesvc: it gets the local list and a local tool's answer, any other tool is unknown; a copy of
/// the same program is refused.
fn socket() -> TestResult {
    const PATH: &str = "/run/bitsos/ktest-tools-u.sock";
    present(FAKE)?;
    let door = Arc::new(ProgramDoor::measure(FAKE));
    let local = mcp::local::start_at(PATH, mcp::TOOLS.entries(), None, door).map_err(|e| format!("start: errno {e}"))?;
    let r = (|| -> TestResult {
        let t = ask(FAKE, PATH, &[LIST, VERSION, READ_FILE], true)?;
        kassert!(t.contains(r#""name":"version""#) && t.contains(r#""safety":"read""#), "the list: {t}");
        for n in ["read_file", "run", "reboot", "screen", "send_keys", "output", "stop", "hw_info", "net_info"] {
            kassert!(!t.contains(&format!(r#""name":"{n}""#)), "{n} is in the local list");
        }
        kassert!(t.contains("BitsOS V2 kernel"), "version did not answer: {t}");
        kassert!(t.contains("Unknown tool: read_file") && !t.contains("root:"), "read_file: {t}");
        kassert!(t.trim_end().ends_with("done") && !t.contains("closed"), "the program was cut off: {t}");
        copy(FAKE, FAKE_COPY)?;
        let t = ask(FAKE_COPY, PATH, &[LIST, VERSION], true);
        let _ = vfs::Ctx::kernel(&vfs::root_ns()).unlink(None, FAKE_COPY.as_bytes());
        let t = t?;
        kassert!(t.contains("Refused: ") && t.contains("closed") && !t.contains("BitsOS V2 kernel"), "a copy of the admitted program: {t}");
        // The right program as execve leaves it today (its memory open to other programs of uid 1000).
        let t = ask(FAKE, PATH, &[LIST, VERSION], false)?;
        kassert!(t.contains("memory is open") && t.contains("closed") && !t.contains("BitsOS V2 kernel"), "the admitted program with open memory: {t}");
        until("connections gone", 5, || local.connections() == 0)?;
        Ok(())
    })();
    stopped(&local)?;
    r
}
ktest!("mc.local.user_socket", Safe, socket);

/// The door as the kernel makes it (for the boot's assistant) refuses another program of uid 1000.
fn refused() -> TestResult {
    const PATH: &str = "/run/bitsos/ktest-tools-r.sock";
    present(FAKE)?;
    let door = Arc::new(ProgramDoor::kernel());
    let local = mcp::local::start_at(PATH, mcp::TOOLS.entries(), None, door).map_err(|e| format!("start: errno {e}"))?;
    let r = (|| -> TestResult {
        let t = ask(FAKE, PATH, &[LIST, VERSION], true)?;
        kassert!(t.contains("Refused: ") && t.contains("closed"), "a stranger was not refused: {t}");
        kassert!(!t.contains(r#""tools""#) && !t.contains("BitsOS V2 kernel"), "a stranger got an answer: {t}");
        Ok(())
    })();
    stopped(&local)?;
    r
}
ktest!("mc.local.user_refused", Safe, refused);
