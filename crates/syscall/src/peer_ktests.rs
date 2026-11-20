//! INT-CL3T ktests, through the real dispatcher on held test processes (sx2's `T`: the test thread plays the
//! program):
//! - `nw.sock.peer_label`: an AF_UNIX socket keeps the program that connected it and gives its label with
//!   SO_PEERSEC (CL-S CR 2, MC-L CR 2; `proc::peer`);
//! - `mc.local.user_closed`: the local tools door's program is closed to the other programs of its user from
//!   its start (MC-L CR 1).
//!
//! Not here: `/proc/<pid>/mem` asks `proc::current()`, which a played call does not have; it follows the same
//! rule (`Process::may_ptrace`) and is checked with real programs in host/tests/cl/30-assist.
//!
//! All `Safe` and rerunnable: every test makes its own processes and ends them, nothing waits without an end.

use crate::sx2_ktests::{T, expect};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::ab::clone::CLONE_NEWUSER;
use uapi::ab::nr_more::{PIDFD_GETFD, PIDFD_OPEN, PROCESS_VM_READV, PROCESS_VM_WRITEV};
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const AF_UNIX: u64 = 1;
const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOL_SOCKET: u64 = 1;
const SO_PEERSEC: u64 = 31;
/// Places in a test process's scratch memory.
const ADDR: u64 = 0x2000;
const BUF: u64 = 0x2400;
const LEN: u64 = 0x2800;
const VEC: u64 = 0x2c00;

fn seq() -> u64 {
    static N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, core::sync::atomic::Ordering::Relaxed) ^ (time::now_ns() & 0xffff_0000)
}

/// `struct sockaddr_un` for `path` in `t`'s memory: (address, length).
fn sun(t: &T, path: &str) -> (u64, u64) {
    let mut b = alloc::vec![1u8, 0];
    b.extend_from_slice(path.as_bytes());
    b.push(0);
    t.put(ADDR, &b);
    (t.a(ADDR), b.len() as u64)
}

/// getsockopt(SO_PEERSEC) with `room` bytes: the label without its closing NUL, or (errno, the length the
/// kernel wrote back).
fn label(t: &mut T, fd: i64, room: u32) -> Result<String, (i64, u32)> {
    t.put(LEN, &room.to_le_bytes());
    let (buf, len) = (t.a(BUF), t.a(LEN));
    let r = t.call(nr::GETSOCKOPT, &[fd as u64, SOL_SOCKET, SO_PEERSEC, buf, len]);
    let mut n = [0u8; 4];
    let _ = t.p.mm.copy_from_user(&mut n, len);
    let n = u32::from_le_bytes(n);
    if r != 0 {
        return Err((r, n));
    }
    let mut b = alloc::vec![0u8; n as usize];
    let _ = t.p.mm.copy_from_user(&mut b, buf);
    // A label that does not end with NUL is no answer.
    if b.pop() != Some(0) {
        return Err((i64::MIN, n));
    }
    Ok(String::from_utf8_lossy(&b).into_owned())
}

fn socket(t: &mut T, family: u64) -> Result<i64, String> {
    let fd = t.call(nr::SOCKET, &[family, SOCK_STREAM, 0]);
    kassert!(fd >= 0, "socket: {fd}");
    Ok(fd)
}

fn connect(t: &mut T, path: &str) -> Result<i64, String> {
    let fd = socket(t, AF_UNIX)?;
    let (a, n) = sun(t, path);
    expect("connect", t.call(nr::CONNECT, &[fd as u64, a, n]), 0)?;
    Ok(fd)
}

fn accept(t: &mut T, listener: i64) -> Result<i64, String> {
    let fd = t.call(nr::ACCEPT, &[listener as u64, 0, 0]);
    kassert!(fd >= 0, "accept: {fd}");
    Ok(fd)
}

/// The test images are started from memory, so their label names no file.
const PLAIN: &str = "bitsos:program:1:init:0:0:";
const APART: &str = "bitsos:program:1:own:0:0:";

fn peer_label() -> TestResult {
    let path = format!("/tmp/peer-ktest-{}.sock", seq());
    let mut srv = T::new("peer-srv")?;
    let mut cli = T::new("peer-cli")?;
    let mut boxed = T::new("peer-boxed")?;
    let r = (|| -> TestResult {
        let l = socket(&mut srv, AF_UNIX)?;
        let (a, n) = sun(&srv, &path);
        expect("bind", srv.call(nr::BIND, &[l as u64, a, n]), 0)?;
        expect("listen", srv.call(nr::LISTEN, &[l as u64, 8]), 0)?;
        // No peer yet, and a socket that never has such a peer: no label, as Linux without one.
        let lone = socket(&mut cli, AF_UNIX)?;
        kassert!(label(&mut cli, lone, 64).is_err_and(|e| e.0 == -ENOPROTOOPT), "an unconnected socket has a label");
        let tcp = socket(&mut cli, AF_INET)?;
        kassert!(label(&mut cli, tcp, 64).is_err_and(|e| e.0 == -ENOPROTOOPT), "a TCP socket has a label");

        let c = connect(&mut cli, &path)?;
        let s = accept(&mut srv, l)?;
        let got = label(&mut srv, s, 64);
        kassert!(got.as_deref() == Ok(PLAIN), "the accepted socket's label: {got:?}");
        // The connecting side reads the program that listens.
        let got = label(&mut cli, c, 64);
        kassert!(got.as_deref() == Ok(PLAIN), "the connected socket's label: {got:?}");
        // Too little room: ERANGE and the length it takes (the text and its NUL); exactly enough fits.
        let need = PLAIN.len() as u32 + 1;
        let short = label(&mut srv, s, need - 1);
        kassert!(short == Err((-ERANGE, need)), "a short buffer: {short:?}");
        let exact = label(&mut srv, s, need);
        kassert!(exact.as_deref() == Ok(PLAIN), "a buffer of exactly the length: {exact:?}");

        // A program in a user namespace it made: the record says so, taken when it connects.
        let early = connect(&mut boxed, &path)?;
        expect("unshare(CLONE_NEWUSER)", boxed.unshare(CLONE_NEWUSER), 0)?;
        let late = connect(&mut boxed, &path)?;
        let (s1, s2) = (accept(&mut srv, l)?, accept(&mut srv, l)?);
        let (g1, g2) = (label(&mut srv, s1, 64), label(&mut srv, s2, 64));
        kassert!(g1.as_deref() == Ok(PLAIN), "connected before it left: {g1:?} (the record is of the moment of connect)");
        kassert!(g2.as_deref() == Ok(APART), "connected from its own user namespace: {g2:?}");
        let _ = (early, late);

        // socketpair: both ends name their maker.
        let sv = boxed.a(VEC);
        expect("socketpair", boxed.call(nr::SOCKETPAIR, &[AF_UNIX, SOCK_STREAM, 0, sv]), 0)?;
        let mut fds = [0u8; 8];
        let _ = boxed.p.mm.copy_from_user(&mut fds, sv);
        for at in [0, 4] {
            let fd = u32::from_le_bytes([fds[at], fds[at + 1], fds[at + 2], fds[at + 3]]) as i64;
            let got = label(&mut boxed, fd, 64);
            kassert!(got.as_deref() == Ok(APART), "socketpair end {fd}: {got:?}");
        }
        Ok(())
    })();
    let _ = vfs::Ctx::kernel(&vfs::root_ns()).unlink(None, path.as_bytes());
    r
}
ktest!("nw.sock.peer_label", Safe, peer_label);

const ASSIST: &str = "/bin/assist";
const OTHER: &str = "/bin/mcp-fakesvc";

fn held(path: &str, uid: u32) -> Result<Arc<Process>, String> {
    let argv = [path];
    let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
    o.uid = uid;
    o.held = true;
    proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))
}

fn end(p: &Process) {
    p.kill(9);
    let _ = p.wait(Some(10 * SEC));
}

/// process_vm_readv or process_vm_writev of 4 bytes at the target's argv strings.
fn vm(t: &mut T, call: usize, target: &Process) -> i64 {
    let at = target.image_info.lock().arg_start;
    let iov = |base: u64| {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&base.to_le_bytes());
        b[8..].copy_from_slice(&4u64.to_le_bytes());
        b
    };
    t.put(VEC, &iov(t.a(BUF)));
    t.put(VEC + 16, &iov(at));
    let (l, r) = (t.a(VEC), t.a(VEC + 16));
    t.call(call, &[u64::from(target.pid), l, 1, r, 1, 0])
}

/// stdin of `target` through a pidfd (`pidfd_getfd`): the new fd, or the errno.
fn steal(t: &mut T, target: &Process) -> Result<i64, String> {
    let pfd = t.call(PIDFD_OPEN, &[u64::from(target.pid), 0]);
    kassert!(pfd >= 0, "pidfd_open: {pfd}");
    Ok(t.call(PIDFD_GETFD, &[pfd as u64, 0, 0]))
}

/// A program of uid 1000 reads and writes another program of uid 1000 and takes its files, as Linux lets it;
/// not the assistant: spawn closed its image before its first instruction. Root still may.
fn closed() -> TestResult {
    for path in [ASSIST, OTHER] {
        if vfs::Ctx::kernel(&vfs::root_ns()).lookup(None, path.as_bytes(), true).is_err() {
            return ktest::fail(&format!("{path} is not in this image (the user workspace was not built into it)"));
        }
    }
    let mut me = T::new("peer-snoop")?;
    let assist = held(ASSIST, 1000)?;
    let other = held(OTHER, 1000);
    let root = held(OTHER, 0);
    let r = (|| -> TestResult {
        let other = other.as_ref().map_err(|e| e.clone())?;
        kassert!(assist.mm_flags.dumpable() == 0 && other.mm_flags.dumpable() == 1, "dumpable: assist {}, another program {}", assist.mm_flags.dumpable(), other.mm_flags.dumpable());
        expect("process_vm_readv of another program", vm(&mut me, PROCESS_VM_READV, other), 4)?;
        expect("process_vm_writev of another program", vm(&mut me, PROCESS_VM_WRITEV, other), 4)?;
        let fd = steal(&mut me, other)?;
        kassert!(fd >= 0, "pidfd_getfd of another program: {fd}");
        expect("process_vm_readv of the assistant", vm(&mut me, PROCESS_VM_READV, &assist), -EPERM)?;
        expect("process_vm_writev of the assistant", vm(&mut me, PROCESS_VM_WRITEV, &assist), -EPERM)?;
        expect("pidfd_getfd of the assistant", steal(&mut me, &assist)?, -EPERM)?;
        // The shared rule itself, both ways it is asked (/proc/<pid>/mem asks with the fs ids).
        for fs_creds in [false, true] {
            kassert!(!me.p.may_ptrace(&assist, fs_creds) && me.p.may_ptrace(other, fs_creds), "may_ptrace(fs_creds {fs_creds})");
        }
        let root = root.as_ref().map_err(|e| e.clone())?;
        kassert!(root.may_ptrace(&assist, false) && root.may_ptrace(&assist, true), "root may no longer look into the assistant");
        Ok(())
    })();
    end(&assist);
    for p in [&other, &root].into_iter().flatten() {
        end(p);
    }
    r
}
ktest!("mc.local.user_closed", Safe, closed);
