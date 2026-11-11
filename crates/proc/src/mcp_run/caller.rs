//! MC-9a: the process layer tells the MCP gate who calls (`mcp::gate::identity::set_caller`): pid, euid and the
//! image's executable (`ImageInfo::exe`, Linux `mm->exe_file`), so the gate can let only the boot's
//! /bin/bitsinstall claim the installer's role. And the ktest of that judgement with real processes.

use crate::SpawnOptions;
use alloc::format;
use alloc::string::String;
use ktest::{TestResult, kassert, ktest};
use mcp::gate::identity::{self, Caller, Measured};

fn caller() -> Option<Caller> {
    let p = crate::current()?;
    let exe = p.image_info.lock().exe.clone();
    Some(Caller { pid: p.pid, euid: p.cred().euid, exe })
}

fn install() -> Result<(), &'static str> {
    identity::set_caller(caller);
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "proc.mcp_installer_caller", install);

const COPY: &str = "/tmp/mc9a-installer-copy";

/// Starts `path` as `uid` and reports it as a caller (it ends by itself: a probe of a gate that is not there).
fn started(path: &str, argv: &[&str], uid: u32) -> Result<Caller, String> {
    let mut o = SpawnOptions::new(argv, &[], crate::mcp_run::MCP_PROGRAM);
    o.uid = uid;
    let p = crate::spawn_with(path, &o).map_err(|e| format!("{path}: {e}"))?;
    let c = Caller { pid: p.pid, euid: p.cred().euid, exe: p.image_info.lock().exe.clone() };
    let _ = p.wait(Some(5_000_000_000));
    Ok(c)
}

fn write_copy(bytes: &[u8]) -> Result<(), String> {
    let ns = vfs::root_ns();
    let c = vfs::Ctx::kernel(&ns);
    let _ = c.unlink(None, COPY.as_bytes());
    let f = c.open(None, COPY.as_bytes(), vfs::abi::O_WRONLY | vfs::abi::O_CREAT | vfs::abi::O_TRUNC, 0o755).map_err(|e| format!("create {COPY}: {e}"))?;
    let n = f.write(bytes).map_err(|e| format!("write {COPY}: {e}"))?;
    kassert!(n == bytes.len(), "short write {n}");
    Ok(())
}

/// The boot's /bin/bitsinstall as root passes; as another user, another program, a copy, a changed file or a
/// caller without an executable does not. Safe: it only reads /bin and writes its own file below /tmp.
fn identity_test() -> TestResult {
    let Some(m) = Measured::of(identity::EXE) else { return ktest::fail("no /bin/bitsinstall in this image") };
    let probe = ["bitsinstall", "claim-probe", "--node", "/nonexistent-mc9a"];
    let real = started(identity::EXE, &probe, 0)?;
    let pid = real.pid;
    kassert!(identity::judge(Some(&m), Some(real)) == Ok(pid), "the boot's installer as root is refused");
    let user = started(identity::EXE, &probe, 1000)?;
    kassert!(identity::judge(Some(&m), Some(user)).is_err_and(|e| e.contains("not root")), "uid 1000 passed");
    if let Ok(other) = started("/bin/bitsmcp", &["bitsmcp", "mode"], 0) {
        kassert!(identity::judge(Some(&m), Some(other)).is_err_and(|e| e.contains("does not run")), "bitsmcp passed");
    }
    kassert!(identity::judge(Some(&m), Some(Caller { pid: 9, euid: 0, exe: None })).is_err(), "no executable passed");
    kassert!(identity::judge(Some(&m), None).is_err(), "a kernel thread passed");
    kassert!(identity::judge(None, Some(Caller { pid: 9, euid: 0, exe: None })).is_err(), "no measurement passed");
    // A byte-for-byte copy is another file.
    let bytes = crate::read_program(&vfs::root_ns(), identity::EXE).map_err(|e| format!("read: {e}"))?;
    let r = (|| -> TestResult {
        write_copy(&bytes)?;
        let copy = started(COPY, &probe, 0)?;
        kassert!(identity::judge(Some(&m), Some(copy)).is_err_and(|e| e.contains("does not run")), "a copy passed");
        // Measured, then changed: refused.
        let mc = Measured::of(COPY).ok_or("copy not measured")?;
        let ns = vfs::root_ns();
        let f = vfs::Ctx::kernel(&ns).open(None, COPY.as_bytes(), vfs::abi::O_WRONLY | vfs::abi::O_APPEND, 0).map_err(|e| format!("open: {e}"))?;
        f.write(b"\0").map_err(|e| format!("append: {e}"))?;
        drop(f);
        let changed = started(COPY, &probe, 0)?;
        kassert!(identity::judge(Some(&mc), Some(changed)).is_err_and(|e| e.contains("changed")), "a changed file passed");
        Ok(())
    })();
    let _ = vfs::Ctx::kernel(&vfs::root_ns()).unlink(None, COPY.as_bytes());
    r
}
ktest!("mc.installer.identity", Safe, identity_test);
