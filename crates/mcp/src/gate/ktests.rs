//! MC-8a ktests (`mcp.gate.*`, Safe, rerunnable): the offer table over the kernel's real registry, a gate's
//! commands over the real tools (a gate of the test's own, never the armed one), and the node and
//! `/proc/bitsos/mcp` as a program sees them.

use super::*;
use crate::kernel::{KernelEnv, SEC};
use crate::{Json, TOOLS};
use ktest::{TestResult, kassert, ktest};

const TOKEN: &str = "ktest-gate-token-mc8";

/// Every registered tool has its place in the table; nothing raw, no program start, no file access is offered.
fn offer_table() -> TestResult {
    let mut gate = Vec::new();
    for t in TOOLS.entries() {
        if let Offer::Gate = wire::offer(t.name) {
            gate.push(t.name);
            kassert!(t.safety == Safety::Read || t.name == "ktest" || t.name == "stop", "{} offered with class {:?}", t.name, t.safety);
        }
    }
    for name in ["read_phys", "msr_read", "mmio_read", "mmio_write", "pci_read", "pci_write", "run", "kill", "write_file", "read_file", "send_keys", "screen", "t_echo", "t_panic"] {
        kassert!(matches!(wire::offer(name), Offer::Dropped(_)), "{name} is offered");
    }
    for name in ["version", "hw_info", "metrics", "events", "log", "ktest", "output", "stop", "reload_info", "system_list"] {
        kassert!(gate.contains(&name), "{name} is not registered in this kernel or not offered");
    }
    Ok(())
}
ktest!("mcp.gate.offer_table", Safe, offer_table);

fn text_of(r: &Result<ToolResult, String>) -> String {
    match r {
        Ok(Ok(v)) => v.iter().map(|o| if let crate::ToolOutput::Text(t) = o { t.as_str() } else { "" }).collect::<Vec<_>>().join("\n"),
        other => format!("{other:?}"),
    }
}

/// hello, list, auth, calls of the real `version`, `hw_info` and `ktest` (Safe only), installer EPERM,
/// answered counting into `mcp.answered`.
fn session() -> TestResult {
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr.port = 8766;
    let g = Gate::new(cfg, Arc::new(KernelEnv), TOOLS.entries(), None, Mode::Both);
    let r = (|| -> TestResult {
        let a = AtomicBool::new(false);
        let errno = |e: Errno| format!("errno {e}");
        let h = wire::decode_hello_answer(&g.command(&a, b"hello").map_err(errno)?).ok_or("hello answer")?;
        kassert!(h.mode == Mode::Both && h.port == 8766, "hello {h:?}");
        let l = wire::decode_list_answer(&g.command(&a, b"list").map_err(errno)?).ok_or("list answer")?;
        let has = |n: &str| l.iter().find(|t| t.name == n);
        kassert!(has("hw_info").is_some_and(|t| !t.installer) && has("reload").is_some_and(|t| t.installer), "list lacks hw_info or reload");
        kassert!(has("read_phys").is_none() && has("run").is_none() && has("write_file").is_none(), "list offers a dropped tool");
        let reg = crate::find("hw_info").ok_or("hw_info not registered")?;
        kassert!(has("hw_info").is_some_and(|t| t.schema == reg.input_schema && t.description == reg.description), "hw_info listed differently");
        let call = |tool: &str, args: &str| wire::encode_call(&Request { tool: tool.into(), args: Json::parse(args).unwrap_or(Json::Null), timeout_ms: 20_000, client: String::new(), peer: "10.0.2.2:1".into() });
        kassert!(g.command(&a, &call("version", "{}")) == Err(EACCES), "a call before auth");
        kassert!(g.command(&a, &wire::encode_auth("10.0.2.2:1", b"not-the-token")) == Err(EACCES), "a wrong token");
        g.command(&a, &wire::encode_auth("10.0.2.2:1", TOKEN.as_bytes())).map_err(errno)?;
        let v = wire::decode_result(&g.command(&a, &call("version", "{}")).map_err(errno)?).ok_or("version result")?;
        kassert!(text_of(&v).starts_with("BitsOS V2 kernel"), "version {}", text_of(&v));
        let hw = wire::decode_result(&g.command(&a, &call("hw_info", r#"{"section":"cpu"}"#)).map_err(errno)?).ok_or("hw_info result")?;
        kassert!(matches!(hw, Ok(Ok(_))), "hw_info {hw:?}");
        kassert!(g.command(&a, &call("read_phys", r#"{"addr":0,"len":16}"#)) == Err(EPERM), "read_phys through the gate");
        kassert!(g.command(&a, &call("run", r#"{"path":"/bin/true"}"#)) == Err(EPERM), "run through the gate");
        let inst = wire::encode_install(&Request { tool: "reload".into(), args: Json::Obj(Vec::new()), timeout_ms: 1000, client: String::new(), peer: "10.0.2.2:1".into() });
        kassert!(g.command(&a, &inst) == Err(EPERM), "the installer answered");
        if crate::tools::run_ktests::run_active() {
            // Inside a ktest run over MCP (the Mini-PC): a nested run would only be refused.
            return Ok(());
        }
        let k = wire::decode_result(&g.command(&a, &call("ktest", r#"{"filter":"kreg.mcp_tools_unique,mcp.class_probe.display"}"#)).map_err(errno)?).ok_or("ktest result")?;
        let t = text_of(&k);
        kassert!(t.contains("ok kreg.mcp_tools_unique"), "the Safe test did not run: {t}");
        kassert!(t.contains("skip mcp.class_probe.display") && t.contains(crate::tools::run_ktests::NOT_SAFE), "a QemuOnly test ran through the gate: {t}");
        let before = crate::conn::ANSWERED.get();
        g.command(&a, b"answered").map_err(errno)?;
        kassert!(crate::conn::ANSWERED.get() > before && g.command(&a, b"answered") == Err(EPERM), "answered");
        Ok(())
    })();
    g.shutdown(2 * SEC);
    r
}
ktest!("mcp.gate.session", Safe, session);

/// The node: root only, ENXIO while no gate is armed (mcp=kernel); /proc/bitsos/mcp names the mode.
fn node() -> TestResult {
    use vfs::abi::{O_RDONLY, O_RDWR};
    use vfs::inode::Cred;
    use vfs::ops::Ctx;
    let ns = vfs::root_ns();
    let subject = policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0000, uid: 0 };
    let root = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 0, gid: 0 }, umask: 0o022, subject };
    let user = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0o022, subject: policy::Subject { uid: 1000, ..subject } };
    kassert!(user.open(None, wire::NODE.as_bytes(), O_RDWR, 0).err() == Some(EACCES), "uid 1000 opened the gate");
    let p = root.open(None, wire::PROC_FILE.as_bytes(), O_RDONLY, 0).map_err(|e| format!("open {}: {e}", wire::PROC_FILE))?;
    let mut buf = [0u8; 128];
    let n = p.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    match armed() {
        None => {
            kassert!(text == "mode=kernel port=0\n", "{} says {text:?}", wire::PROC_FILE);
            kassert!(root.open(None, wire::NODE.as_bytes(), O_RDWR, 0).err() == Some(ENXIO), "the gate opened under mcp=kernel");
        }
        Some(g) => {
            kassert!(text.starts_with(&format!("mode={} port={} confirmer=", g.mode.name(), g.port)), "{} says {text:?}", wire::PROC_FILE);
            let f = root.open(None, wire::NODE.as_bytes(), O_RDWR, 0).map_err(|e| format!("open the gate: {e}"))?;
            kassert!(f.write(b"hello") == Ok(5), "hello");
            let n = f.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            let h = core::str::from_utf8(&buf[..n]).ok().and_then(wire::decode_hello_answer).ok_or("hello answer")?;
            kassert!(h.mode == g.mode && h.port == g.port, "hello {h:?}");
            kassert!(f.write(b"bogus") == Err(EINVAL) && f.write(b"answered") == Err(EPERM), "errnos");
        }
    }
    Ok(())
}
ktest!("mcp.gate.node", Safe, node);

static GRANT_TOLD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn grant_open(until: u64, _: &str) {
    GRANT_TOLD.store(until, Ordering::SeqCst);
}
fn grant_close(_: &str) {
    GRANT_TOLD.store(0, Ordering::SeqCst);
}
fn grant_caller() -> Result<u32, String> {
    Ok(1)
}

/// MC-10: a gate of the test's own (never the armed one) with the kernel's real wall and monotonic clocks: the
/// installer's table opens every tool to the granted client only, the NVMe tick goes to a recorder (never to
/// the block layer: this runs on the Mini-PC) until the grant's end, and an empty table closes both.
fn grant() -> TestResult {
    let Some(wall) = time::wall::realtime_ns().map(|ns| (ns / 1_000_000_000) as i64) else {
        return ktest::fail("no wall clock (RTC) in this boot: no grant is taken without one");
    };
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr.port = 8767;
    let g = Gate::new(cfg, Arc::new(KernelEnv), TOOLS.entries(), None, Mode::Service)
        .with_check(grant_caller)
        .with_nvme_sink(access::NvmeSink { open: grant_open, close: grant_close });
    let r = (|| -> TestResult {
        let x = AtomicBool::new(false);
        let errno = |e: Errno| format!("errno {e}");
        g.command_as(9, &x, &inst::encode_claim()).map_err(errno)?;
        let (a, b) = ("a1".repeat(32), "b2".repeat(32));
        let offered = |c: &str| -> Result<bool, String> {
            let l = wire::decode_list_answer(&g.command(&x, &wire::encode_list_for(c)).map_err(errno)?).ok_or("list answer")?;
            Ok(l.iter().any(|t| t.name == "run"))
        };
        kassert!(!offered(&a)? && !offered(&b)?, "run offered without a grant");
        let gr = mcp_wire::access::Grant { client: a.clone(), until: wall + 600, set_at: wall, nvme: true };
        let ans = g.command_as(9, &x, &mcp_wire::access::encode_table(&[gr])).map_err(errno)?;
        kassert!(ans.starts_with("{\"taken\":1,\"refused\":[]"), "the table: {ans}");
        kassert!(offered(&a)? && !offered(&b)? && !offered("")?, "run is not offered to the granted client only");
        let told = GRANT_TOLD.load(Ordering::SeqCst);
        let now = ksync::now_ns().unwrap_or(0);
        kassert!(told > now && told <= now + 601 * SEC, "the NVMe tick told {told}, now {now}");
        g.command_as(9, &x, &mcp_wire::access::encode_table(&[])).map_err(errno)?;
        kassert!(!offered(&a)?, "run still offered after the empty table");
        kassert!(GRANT_TOLD.load(Ordering::SeqCst) == 0, "the NVMe tick stayed open after the revoke");
        Ok(())
    })();
    g.shutdown(2 * SEC);
    r
}
ktest!("mcp.gate.grant", Safe, grant);

/// MC-10: the real block layer's run-time NVMe opener behind a grant with the tick: open while the grant is
/// live, closed by the empty table. QemuOnly: it opens NVMe writes for a moment.
fn grant_block() -> TestResult {
    let Some(wall) = time::wall::realtime_ns().map(|ns| (ns / 1_000_000_000) as i64) else {
        return ktest::fail("no wall clock (RTC) in this boot");
    };
    if block::policy::nvme_runtime_open() {
        return ktest::fail("the run-time NVMe gate is open already (a live grant): not touched");
    }
    let t = access::Table::new();
    let gr = mcp_wire::access::Grant { client: "c3".repeat(32), until: wall + 60, set_at: wall, nvme: true };
    let taken = t.set(alloc::vec![gr], Some(wall), ksync::now_ns().unwrap_or(0)).taken;
    let open = block::policy::nvme_runtime_open();
    t.set(Vec::new(), Some(wall), ksync::now_ns().unwrap_or(0));
    let closed = !block::policy::nvme_runtime_open();
    kassert!(taken == 1 && open, "the tick did not open the block layer's NVMe gate (taken {taken})");
    kassert!(closed, "the empty table did not close it");
    Ok(())
}
ktest!("mcp.gate.grant_block", QemuOnly, grant_block);
