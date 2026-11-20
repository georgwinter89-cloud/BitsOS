//! In-kernel test of UPDATE's install node (Safe: only `status`, nothing written; the Mini-PC runs it over MCP):
//! - `rb.install.update_node_access`: root in the kernel reads `status`; a root process MCP's run started
//!   (`proc::MCP_PROGRAM`, here `/bin/bitsupdate status`) is refused at open, as bitsdisk's node refuses it.

use super::update_node::OPEN_REFUSED;
use alloc::format;
use alloc::string::String;
use core::sync::atomic::Ordering;
use ktest::{TestResult, kassert, ktest};

fn update_node_access() -> TestResult {
    let ns = vfs::root_ns();
    let c = vfs::Ctx::kernel(&ns);
    let f = c.open(None, b"/dev/bitsos/update", vfs::abi::O_RDWR, 0).map_err(|e| format!("open as the kernel: {e}"))?;
    f.write(b"status").map_err(|e| format!("write status: {e}"))?;
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).map_err(|e| format!("read the answer: {e}"))?;
    let t = String::from_utf8_lossy(&buf[..n]);
    kassert!(t.contains("\"running\"") && t.ends_with('\n'), "status answer: {t}");

    let before = OPEN_REFUSED.load(Ordering::Relaxed);
    let argv = ["bitsupdate", "status"];
    let mut o = proc::SpawnOptions::new(&argv, &[], proc::MCP_PROGRAM);
    o.uid = 0;
    proc::spawn_with("/bin/bitsupdate", &o).map_err(|e| format!("/bin/bitsupdate: {e}"))?;
    for _ in 0..2000 {
        if OPEN_REFUSED.load(Ordering::Relaxed) > before {
            return Ok(());
        }
        sched::sleep_ns(10_000_000);
    }
    Err(String::from("bitsupdate under MCP's program id was never refused"))
}
ktest!("rb.install.update_node_access", Safe, update_node_access);
