//! MC-8a: the gate `/dev/bitsos/mcp`, the kernel's narrow interface for the user-space MCP service `bitsmcp`
//! (commands, answers and the offer table: `mcp_wire::gate`). Armed by `start_kernel` when CONFIG.TXT says
//! `mcp=both` or `mcp=service`; otherwise opening the node is ENXIO. Root only (0600, uid 0 at open: the
//! service opens its descriptors before it gives root up). Each open is a session: one command per write(2),
//! taken whole (`write_iter`), its answer read back from the start; a second write while one runs is EBUSY.
//!
//! A call runs through the server's own machinery (`conn::run_tool` on a listener-less `Shared`): workers of
//! class Normal, the call's deadline (+0.25 s, then "Still running (call n)"), `output`/`stop` over the gate's
//! own calls. The writer waits in bounded slices and gives up when it is killed (the call goes on and can be
//! read with `output`). Every call and installer request is an event (`mcp svc.call`, `svc.install_refused`).
//! `/proc/bitsos/mcp` (0444) says `mode=<m> port=<p>` for init (starts the service) and the service itself.
//!
//! MC-9a: `install` goes to the installer (`installer`, `identity`; protocol `mcp_wire::installer`): one session
//! that passed `claim` (the boot's `/bin/bitsinstall`, root) takes the requests and decides them; the tool runs
//! only after its yes, in the asking session's write, and an action after the answer (reload, reboot) runs in its
//! own thread once the answer had time to travel back through bitsmcp.
//!
//! MC-10: a client (its TLS key, `Request::client`) with a live full-access grant (`access`, the installer's
//! table) gets every kernel tool as a call: the kernel server's whole set, `run`, files, `screen`, `send_keys`,
//! installs and reboot without another question (the grant was Georg's yes), every ktest class, `stop`/`output`
//! by pid. `request_access` and `pair_client` are questions for the installer that run no tool.

use crate::catalog::{Catalog, Tool};
use crate::server::{Env, ServerConfig, Shared};
use crate::{Json, Safety, ToolContext, ToolEntry, ToolError, ToolResult, rpc};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use mcp_wire::gate::{self as wire, Command, Hello, Listed, Mode, Offer, Request};
use mcp_wire::installer as inst;
use uapi::errno::*;
use vfs::{Errno, File};

/// The gate's `ktest`: the Safe-class runner (listed with the registered tool's description and schema).
static KTEST_SAFE: ToolEntry = ToolEntry {
    name: "ktest",
    description: "",
    input_schema: r#"{"type":"object"}"#,
    owner: "MC",
    safety: Safety::Change,
    handler: crate::tools::run_ktests::ktest_safe_tool,
};

/// A failed `auth` waits this long, so a service cannot try tokens fast.
const AUTH_FAIL_NS: u64 = 100_000_000;
/// An encoded result above this is answered as too big (text is at most 6 bytes per character once escaped).
const RESULT_MAX: usize = 6 * rpc::ANSWER_MAX + 65_536;
/// An action after an installer answer waits this much longer than on the kernel server (the answer goes
/// kernel -> bitsmcp -> TLS -> the client).
const GATE_AFTER_EXTRA_NS: u64 = 500_000_000;

pub struct Gate {
    sh: Arc<Shared>,
    mode: Mode,
    port: u16,
    inst: installer::Broker,
    /// `/bin/bitsinstall` as this boot brought it (measured when the kernel arms the gate).
    measured: Option<identity::Measured>,
    /// Tests: who the caller is, instead of the process layer and `measured`.
    check: Option<fn() -> Result<u32, String>>,
    /// The pid that claimed the installer's role (0: nobody), named in `/proc/bitsos/installer`.
    holder_pid: AtomicU32,
    /// MC-10: the full-access grants (the installer's table) and the wall clock they are judged by.
    access: access::Table,
    wall: fn() -> Option<i64>,
}

/// The wall clock (Unix s): CLOCK_REALTIME, the RTC's time at boot plus the monotonic time since.
fn real_wall() -> Option<i64> {
    time::wall::realtime_ns().map(|ns| (ns / 1_000_000_000) as i64)
}

impl Gate {
    /// A gate over `tools` (and the services' registry) for the service on `cfg.addr.port`.
    pub(crate) fn new(cfg: ServerConfig, env: Arc<dyn Env>, tools: &'static [ToolEntry], services: Option<Arc<crate::svc::Registry>>, mode: Mode) -> Gate {
        let port = cfg.addr.port;
        Gate {
            sh: Shared::headless(cfg, env, tools, services),
            mode,
            port,
            inst: installer::Broker::new(),
            measured: None,
            check: None,
            holder_pid: AtomicU32::new(0),
            access: access::Table::new(),
            wall: real_wall,
        }
    }

    /// The installer's executable measured now (the kernel arms the gate before any program runs).
    pub(crate) fn measure_installer(mut self) -> Gate {
        self.measured = identity::Measured::of(inst::EXE);
        if self.measured.is_none() {
            klog::event!(Warn, "mcp", "installer.not_measured", path = inst::EXE; "no installer in this boot: every install request is EPERM");
        }
        self
    }

    /// Tests: `f` is the wall clock.
    #[allow(dead_code)]
    pub(crate) fn with_wall(mut self, f: fn() -> Option<i64>) -> Gate {
        self.wall = f;
        self
    }

    /// Tests: the NVMe tick goes to `sink` instead of the block layer.
    #[allow(dead_code)]
    pub(crate) fn with_nvme_sink(mut self, sink: access::NvmeSink) -> Gate {
        self.access = access::Table::with_sink(sink);
        self
    }

    /// MC-10: whether `client` holds a live full-access grant now.
    fn granted(&self, client: &str) -> bool {
        self.access.live(client)
    }

    /// Tests: `f` says who calls.
    #[allow(dead_code)]
    pub(crate) fn with_check(mut self, f: fn() -> Result<u32, String>) -> Gate {
        self.check = Some(f);
        self
    }

    fn caller_ok(&self) -> Result<u32, String> {
        match self.check {
            Some(f) => f(),
            None => identity::judge(self.measured.as_ref(), identity::current()),
        }
    }

    /// A session ended (its seat): a held installer role is given back and what waited for it refused.
    pub(crate) fn seat_closed(&self, seat: u64) {
        if self.inst.release(seat) {
            self.holder_pid.store(0, Ordering::Release);
            klog::event!(Warn, "mcp", "installer.released", seat = seat; "the installer's session ended; install requests are EPERM until it claims again");
        }
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.sh.clone()
    }

    /// Ends a gate that is not armed (tests): calls are asked to stop, idle workers end; waits up to `timeout_ns`.
    pub(crate) fn shutdown(&self, timeout_ns: u64) -> bool {
        let h = crate::server::ServerHandle::of(self.sh.clone());
        h.stop();
        h.wait_stopped(timeout_ns)
    }

    fn catalog(&self) -> Catalog<'_> {
        self.sh.catalog()
    }

    /// Whether the gate offers `t` and how (`Some(true)`: to the installer); `granted`: to a client with full
    /// access (every tool a call).
    fn offered(t: &Tool, granted: bool) -> Option<bool> {
        match t {
            Tool::Static(e) => match wire::offer_for(e.name, granted) {
                Offer::Gate => Some(false),
                Offer::Installer => Some(true),
                Offer::Dropped(_) => None,
            },
            Tool::Service(s) => match wire::service_offer_for(s.safety, s.uid == 0, granted)? {
                Offer::Gate => Some(false),
                Offer::Installer => Some(true),
                Offer::Dropped(_) => None,
            },
        }
    }

    fn list(&self, granted: bool) -> String {
        let tools: Vec<Listed> = self
            .catalog()
            .sorted()
            .into_iter()
            .filter_map(|t| {
                let installer = Self::offered(&t, granted)?;
                Some(Listed {
                    name: String::from(t.name()),
                    description: String::from(t.description()),
                    schema: String::from(t.input_schema()),
                    owner: String::from(t.owner()),
                    safety: t.safety(),
                    verified: t.safety_verified(),
                    pid: t.service_pid(),
                    service: t.is_service(),
                    installer,
                })
            })
            .collect();
        wire::encode_list_answer(&tools)
    }

    /// One command of a session without a seat of its own (tests); Ok is the answer text, Err the write's errno.
    pub(crate) fn command(&self, authed: &AtomicBool, line: &[u8]) -> Result<String, Errno> {
        self.command_as(0, authed, line)
    }

    /// One command of session `seat`.
    pub(crate) fn command_as(&self, seat: u64, authed: &AtomicBool, line: &[u8]) -> Result<String, Errno> {
        let Some(cmd) = wire::parse_command(line) else {
            klog::event!(Info, "mcp", "svc.bad_command", bytes = line.len());
            return Err(EINVAL);
        };
        // MC-10: what the clock rule ended goes first, whatever the command.
        self.access.sweep((self.wall)(), self.sh.now());
        match cmd {
            Command::Hello => Ok(wire::encode_hello_answer(&Hello { mode: self.mode, port: self.port, conns: self.sh.cfg.conns, version: String::from(env!("CARGO_PKG_VERSION")) })),
            Command::List { client } => Ok(self.list(self.granted(&client))),
            Command::Access { client } => {
                if !authed.load(Ordering::Acquire) {
                    return Err(EACCES);
                }
                let offset = time::wall::utc_offset_minutes();
                Ok(mcp_wire::access::encode_status(&self.access.status(&client, (self.wall)(), self.sh.now(), offset)))
            }
            Command::Grants(list) => {
                self.installer_seat(seat, "grants")?;
                let t = self.access.set(list, (self.wall)(), self.sh.now());
                let mut out = format!("{{\"taken\":{},\"refused\":[", t.taken);
                for (i, (c, why)) in t.refused.iter().enumerate() {
                    out.push_str(&format!("{}{{\"client\":\"{c}\",\"why\":\"{}\"}}", if i > 0 { "," } else { "" }, why.name()));
                }
                out.push_str("]}");
                Ok(out)
            }
            Command::Auth { peer, token } => {
                if mcp_wire::http::same_token(&token, self.sh.cfg.token.as_bytes()) {
                    authed.store(true, Ordering::Release);
                    return Ok(String::from("ok\n"));
                }
                authed.store(false, Ordering::Release);
                klog::event!(Info, "mcp", "svc.auth_failed", peer = peer.as_str());
                self.sh.env.sleep_ns(AUTH_FAIL_NS);
                Err(EACCES)
            }
            Command::Answered => {
                if !authed.swap(false, Ordering::AcqRel) {
                    return Err(EPERM);
                }
                crate::conn::ANSWERED.inc();
                Ok(String::from("ok\n"))
            }
            Command::Install(r) => {
                if !authed.load(Ordering::Acquire) {
                    return Err(EACCES);
                }
                self.install(seat, r)
            }
            Command::Claim => self.claim(seat),
            Command::Next { wait_ms } => {
                self.installer_seat(seat, "next")?;
                let abort = || self.sh.stop.load(Ordering::Acquire) || vfs::dentry::killed() || !self.inst.holds(seat);
                Ok(match self.inst.next(&*self.sh.env, wait_ms.saturating_mul(1_000_000), &abort) {
                    Some(a) => inst::encode_asked(&a),
                    None => String::from("none\n"),
                })
            }
            Command::Decide(d) => {
                self.installer_seat(seat, "decide")?;
                let (id, yes) = (d.id, d.yes);
                let p = self.inst.decide(d).inspect_err(|_| klog::event!(Info, "mcp", "installer.decide_unknown", id = id))?;
                klog::event!(Info, "mcp", "installer.decided", id = id, tool = p.req.tool.as_str(), yes = yes);
                Ok(String::from("ok\n"))
            }
            Command::Call(r) => {
                if !authed.load(Ordering::Acquire) {
                    return Err(EACCES);
                }
                self.call(r)
            }
        }
    }

    /// `claim`: the role for this seat when the caller is the boot's installer.
    fn claim(&self, seat: u64) -> Result<String, Errno> {
        let pid = match self.caller_ok() {
            Ok(pid) => pid,
            Err(why) => {
                klog::event!(Warn, "mcp", "installer.claim_refused", seat = seat; "{}", why.as_str());
                return Err(EPERM);
            }
        };
        if seat == 0 {
            return Err(EPERM);
        }
        self.inst.claim(seat).inspect_err(|_| klog::event!(Warn, "mcp", "installer.claim_busy", pid = pid; "another session holds the installer's role"))?;
        self.holder_pid.store(pid, Ordering::Release);
        klog::event!(Info, "mcp", "installer.claimed", pid = pid, seat = seat);
        Ok(inst::encode_claim_answer(pid))
    }

    /// The installer's own commands: the seat holds the role and the caller still is the installer (a
    /// descriptor handed to another program does not carry the role).
    fn installer_seat(&self, seat: u64, what: &str) -> Result<(), Errno> {
        if !self.inst.holds(seat) {
            return Err(EPERM);
        }
        if let Err(why) = self.caller_ok() {
            klog::event!(Warn, "mcp", "installer.caller_refused", command = what; "{}", why.as_str());
            return Err(EPERM);
        }
        Ok(())
    }

    /// bitsmcp's `install`: queued for the installer, run after its yes. MC-10: a client with full access runs it
    /// at once (the grant was the yes); `request_access` and `pair_client` are questions without a tool.
    fn install(&self, seat: u64, r: Request) -> Result<String, Errno> {
        if r.tool == mcp_wire::access::REQUEST_ACCESS || r.tool == mcp_wire::access::PAIR_CLIENT {
            return self.question(seat, r);
        }
        let granted = self.granted(&r.client);
        let tool = match self.catalog().find(&r.tool) {
            Some(t) if granted => {
                klog::event!(Info, "mcp", "svc.install_decided", tool = r.tool.as_str(), decision = "yes", by = "access", peer = r.peer.as_str(); "full access for this client");
                return self.run(t, r, true, true);
            }
            Some(t) if Self::offered(&t, false) == Some(true) => t,
            found => {
                klog::event!(Info, "mcp", "svc.install_refused", tool = r.tool.as_str(), peer = r.peer.as_str(), known = found.is_some(); "not an installer tool: EPERM");
                return Err(EPERM);
            }
        };
        if self.inst.holds(seat) {
            // The installer cannot ask itself.
            return Err(EPERM);
        }
        let Some(p) = self.inst.ask(r.clone()) else {
            klog::event!(Info, "mcp", "svc.install_refused", tool = r.tool.as_str(), peer = r.peer.as_str(), listed = true; "no installer runs: EPERM");
            return Err(EPERM);
        };
        klog::event!(Info, "mcp", "svc.install_asked", id = p.id, tool = r.tool.as_str(), kind = p.kind.name(), peer = r.peer.as_str());
        let abort = || self.sh.stop.load(Ordering::Acquire) || vfs::dentry::killed();
        let d = self.inst.wait(&*self.sh.env, &p, inst::DECISION_WAIT_MS * 1_000_000, &abort);
        klog::event!(Info, "mcp", "svc.install_decided", id = p.id, tool = r.tool.as_str(), decision = if d.yes { "yes" } else { "no" }, by = d.by.as_str(), peer = r.peer.as_str(); "{}", d.reason.as_str());
        if !d.yes {
            return Ok(wire::encode_result(&Ok(Err(ToolError::Refused(inst::refused_text(&r.tool, &d.by, &d.reason))))));
        }
        self.run(tool, r, true, false)
    }

    /// MC-10: a question for the installer that runs no tool: `request_access` (the installer sets the grant in
    /// its table before it says yes; the answer names it) and `pair_client` (yes: bitsmcp pairs the client).
    fn question(&self, seat: u64, mut r: Request) -> Result<String, Errno> {
        let access = r.tool == mcp_wire::access::REQUEST_ACCESS;
        let refuse = |why: &str| Ok(wire::encode_result(&Ok(Err(ToolError::Refused(String::from(why))))));
        if access && r.client.is_empty() {
            klog::event!(Info, "mcp", "access.ask_refused", peer = r.peer.as_str(); "not a TLS client");
            return refuse("full access is only for a paired client over TLS (host/mcp/pair.mjs, then --tls)");
        }
        if access {
            let hours = match r.args.get("hours") {
                None | Some(Json::Null) => mcp_wire::access::DEFAULT_H,
                Some(v) => match v.as_i64().filter(|h| (1..=mcp_wire::access::MAX_S / 3600).contains(h)) {
                    Some(h) => h,
                    None => return Ok(wire::encode_result(&Ok(Err(ToolError::BadArgument(String::from("hours: 1 to 168")))))),
                },
            };
            let reason = mcp_wire::access::clean(r.args.get("reason").and_then(Json::as_str).unwrap_or(""), mcp_wire::access::REASON_MAX);
            r.args = Json::obj(alloc::vec![("hours", Json::Num(hours as f64)), ("reason", Json::Str(reason))]);
        }
        if self.inst.holds(seat) {
            return Err(EPERM);
        }
        let Some(p) = self.inst.ask(r.clone()) else {
            klog::event!(Info, "mcp", "svc.install_refused", tool = r.tool.as_str(), peer = r.peer.as_str(); "no installer runs: EPERM");
            return Err(EPERM);
        };
        klog::event!(Info, "mcp", "svc.install_asked", id = p.id, tool = r.tool.as_str(), kind = p.kind.name(), peer = r.peer.as_str());
        let abort = || self.sh.stop.load(Ordering::Acquire) || vfs::dentry::killed();
        let d = self.inst.wait(&*self.sh.env, &p, inst::DECISION_WAIT_MS * 1_000_000, &abort);
        klog::event!(Info, "mcp", "svc.install_decided", id = p.id, tool = r.tool.as_str(), decision = if d.yes { "yes" } else { "no" }, by = d.by.as_str(), peer = r.peer.as_str(); "{}", d.reason.as_str());
        if !d.yes {
            return Ok(wire::encode_result(&Ok(Err(ToolError::Refused(inst::refused_text(&r.tool, &d.by, &d.reason))))));
        }
        if !access {
            return Ok(wire::encode_result(&Ok(Ok(alloc::vec![crate::ToolOutput::Text(String::from("yes: confirmed at the machine"))]))));
        }
        self.access.sweep((self.wall)(), self.sh.now());
        let st = self.access.status(&r.client, (self.wall)(), self.sh.now(), time::wall::utc_offset_minutes());
        if !st.granted {
            klog::event!(Warn, "mcp", "access.yes_without_grant", id = p.id; "the installer said yes but its table holds no grant for the client");
            return Ok(wire::encode_result(&Ok(Err(ToolError::Failed(String::from("the installer said yes, but no grant for this client is in the kernel's table (see the installer's log)"))))));
        }
        Ok(wire::encode_result(&Ok(Ok(alloc::vec![crate::ToolOutput::Text(mcp_wire::access::status_text(&st))]))))
    }

    fn call(&self, r: Request) -> Result<String, Errno> {
        let granted = self.granted(&r.client);
        let tool = match self.catalog().find(&r.tool) {
            Some(t) if granted => return self.run(t, r, true, true),
            Some(t) if Self::offered(&t, false) == Some(false) => t,
            found => {
                klog::event!(Warn, "mcp", "svc.call", tool = r.tool.as_str(), peer = r.peer.as_str(), result = "not_offered", known = found.is_some());
                return Err(EPERM);
            }
        };
        self.run(tool, r, false, false)
    }

    /// Runs an offered tool (`installer`: confirmed, may act after its answer; `full`: a client with full
    /// access, as on the kernel's own server: programs by pid, every ktest class).
    fn run(&self, tool: Tool, r: Request, installer: bool, full: bool) -> Result<String, Errno> {
        let t0 = self.sh.now();
        let target = if full { None } else { wire::process_target(tool.name(), &r.args) };
        let res: Result<ToolResult, String> = if let Some(why) = target {
            Ok(Err(ToolError::Refused(String::from(why))))
        } else if let Err(reason) = rpc::policy_check(tool.name()) {
            Ok(Err(ToolError::Refused(reason)))
        } else {
            let run = match &tool {
                Tool::Static(e) if e.name == "ktest" && !full => Tool::Static(&KTEST_SAFE),
                other => other.clone(),
            };
            let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: t0.saturating_add(r.timeout_ms.saturating_mul(1_000_000)) };
            let sh = self.sh.clone();
            let (res, after) = crate::conn::run_tool(&self.sh, &run, ctx, &r.args, &r.peer, None, &|| sh.stop.load(Ordering::Acquire) || vfs::dentry::killed());
            match after {
                Some(f) if installer => self.after(f, tool.name(), &r.peer),
                // A plain call never acts after its answer (reboot and reload go to the installer).
                Some(_) => klog::event!(Warn, "mcp", "svc.after_dropped", tool = tool.name()),
                None => {}
            }
            res
        };
        let result = match &res {
            Err(_) => "still_running",
            Ok(Ok(_)) => "ok",
            Ok(Err(ToolError::BadArgument(_))) => "bad_argument",
            Ok(Err(ToolError::Refused(_))) => "refused",
            Ok(Err(ToolError::Failed(_))) => "failed",
        };
        let ms = self.sh.now().saturating_sub(t0) / 1_000_000;
        klog::event!(Info, "mcp", "svc.call", tool = tool.name(), peer = r.peer.as_str(), result = result, ms = ms, access = if full { "full" } else { "narrow" });
        let text = wire::encode_result(&res);
        if text.len() > RESULT_MAX {
            klog::event!(Warn, "mcp", "svc.result_too_big", tool = tool.name(), bytes = text.len());
            return Ok(wire::encode_result(&Ok(Err(ToolError::Failed(String::from(mcp_wire::rpc::TOO_BIG))))));
        }
        Ok(text)
    }

    /// A confirmed tool's action after its answer (reload, reboot): in its own thread once the answer had time
    /// to reach the client, as the kernel server does (`conn::Conn::run_after`).
    fn after(&self, f: crate::AfterAnswer, tool: &str, peer: &str) {
        let grace = self.sh.cfg.after_grace_ns.saturating_add(GATE_AFTER_EXTRA_NS);
        klog::event!(Info, "mcp", "svc.after_answer", tool = tool, peer = peer, grace_ms = grace / 1_000_000);
        let env = self.sh.env.clone();
        let slot = Arc::new(ksync::SpinLock::named("mcp.gate.after", Some(f)));
        let theirs = slot.clone();
        let run = alloc::boxed::Box::new(move || {
            env.sleep_ns(grace);
            let f = theirs.lock().take();
            if let Some(f) = f {
                f();
            }
        });
        if self.sh.env.spawn("mcp-gate-after", crate::server::ThreadKind::Worker, run).is_err() {
            // No thread: the answer is lost rather than the action taken without it ever being sent.
            klog::event!(Warn, "mcp", "svc.after_no_thread", tool = tool; "no thread for the action after the answer; not done");
            drop(slot.lock().take());
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The armed gate, the node and /proc/bitsos/mcp.

static GATE: ksync::Once<Gate> = ksync::Once::new();
static MODE: AtomicU8 = AtomicU8::new(0);
static PORT: AtomicU16 = AtomicU16::new(0);
static TEST_CONFIRMER: AtomicBool = AtomicBool::new(false);

/// Sets what `/proc/bitsos/mcp` names (`kernel`, port 0, while no gate is armed).
pub(crate) fn set_mode(m: Mode, port: u16) {
    let code = match m {
        Mode::Kernel => 0,
        Mode::Both => 1,
        Mode::Service => 2,
    };
    MODE.store(code, Ordering::Release);
    PORT.store(port, Ordering::Release);
}

/// Opens the gate (once per boot; a second call keeps the first gate). `test_confirmer`: CONFIG.TXT
/// `mcp_confirmer=test`, named in `/proc/bitsos/mcp` for bitsinstall.
pub(crate) fn arm(g: Gate, test_confirmer: bool) {
    let (m, p) = (g.mode, g.port);
    if GATE.is_completed() {
        klog::event!(Warn, "mcp", "gate.armed_twice"; "the first gate stays");
        return;
    }
    GATE.get_or_init(|| g);
    TEST_CONFIRMER.store(test_confirmer, Ordering::Release);
    set_mode(m, p);
    klog::event!(Info, "mcp", "gate.armed", mode = m.name(), port = p);
}

fn proc_render() -> String {
    let m = match MODE.load(Ordering::Acquire) {
        1 => Mode::Both,
        2 => Mode::Service,
        _ => Mode::Kernel,
    };
    let port = PORT.load(Ordering::Acquire);
    if m == Mode::Kernel {
        return format!("mode=kernel port={port}\n");
    }
    // MC-9a: who answers the installer's dialogs (bitsinstall reads it).
    let who = if TEST_CONFIRMER.load(Ordering::Acquire) { "test" } else { "bitscomp" };
    format!("mode={} port={port} confirmer={who}\n", m.name())
}
vfs::procfile!("/proc/bitsos/mcp", proc_render);

/// The pid holding the installer's role, if any (`/proc/bitsos/installer`).
pub(crate) fn installer_holder(g: &Gate) -> Option<u32> {
    Some(g.holder_pid.load(Ordering::Acquire)).filter(|&p| p != 0)
}

/// INT-W17 (MC-9b CR 5): `/proc/bitsos/installer` (0444) names the process holding the installer's role,
/// `holder=<pid>` or `holder=none`, so bitscomp takes the installer's socket only from that very process (it
/// cannot read the non-dumpable installer's exe link itself).
fn installer_render() -> String {
    match GATE.get().and_then(installer_holder) {
        Some(pid) => format!("holder={pid}\n"),
        None => String::from("holder=none\n"),
    }
}
vfs::procfile!("/proc/bitsos/installer", installer_render);

static SEATS: AtomicU64 = AtomicU64::new(1);

/// One open of the node.
struct Session {
    gate: &'static Gate,
    /// This open's number (the installer's role belongs to one open, not to a process).
    seat: u64,
    /// The last command's answer and the read position. A Mutex, not a SpinLock: a read copies up to several MB
    /// (B4). Freed as soon as it is read to the end, so an idle session holds nothing.
    answer: ksync::Mutex<(Vec<u8>, usize)>,
    busy: ksync::Mutex<()>,
    /// An `auth` passed since the last `answered`.
    authed: AtomicBool,
}

impl Session {
    fn write_all(&self, line: &[u8]) -> Result<usize, Errno> {
        let Some(_one) = self.busy.try_lock() else {
            klog::event!(Info, "mcp", "svc.concurrent_write"; "a second write while a command runs");
            return Err(EBUSY);
        };
        // The old answer goes first (its memory too), so it never belongs to this command.
        drop(core::mem::take(&mut *self.answer.lock()));
        // Built outside the lock and moved in (no copy); the lock covers only the swap.
        let text = self.gate.command_as(self.seat, &self.authed, line)?.into_bytes();
        let old = core::mem::replace(&mut *self.answer.lock(), (text, 0));
        drop(old);
        Ok(line.len())
    }
    fn read(&self, buf: &mut [u8]) -> usize {
        let mut a = self.answer.lock();
        let (data, pos) = &mut *a;
        let n = buf.len().min(data.len().saturating_sub(*pos));
        buf[..n].copy_from_slice(&data[*pos..*pos + n]);
        *pos += n;
        // Read to the end: the memory goes now (outside the lock), later reads see the end (0).
        let done = (*pos >= data.len() && !data.is_empty()).then(|| core::mem::take(&mut *a));
        drop(a);
        drop(done);
        n
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.gate.seat_closed(self.seat);
    }
}

fn dev() -> u64 {
    vfs::abi::encode_dev(wire::DEV.0, wire::DEV.1)
}

impl File for Session {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        Ok(self.read(buf))
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_all(buf)
    }
    fn write_iter(&self, from: &mut dyn vfs::node::IoSource, _status: u64) -> Option<Result<usize, Errno>> {
        let n = from.left();
        if n > wire::LINE_MAX {
            return Some(Err(EINVAL));
        }
        let mut line = Vec::new();
        if line.try_reserve_exact(n).is_err() {
            return Some(Err(ENOMEM));
        }
        line.resize(n, 0);
        if from.copy(&mut line) != n {
            return Some(Err(EFAULT));
        }
        Some(self.write_all(&line))
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        dev()
    }
}

/// The node itself; every open gets its own `Session`.
struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        dev()
    }
    fn open_node(&self, _flags: u64, who: vfs::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        // The mode already keeps others out; this holds when someone chmods the node.
        if who.uid != 0 {
            return Some(Err(EACCES));
        }
        let Some(gate) = GATE.get() else {
            // mcp=kernel (or no token): no service is wanted, as a device without its driver.
            return Some(Err(ENXIO));
        };
        Some(Ok(open_session(gate)))
    }
}

pub(crate) fn open_session(gate: &'static Gate) -> Arc<dyn File> {
    Arc::new(Session { gate, seat: SEATS.fetch_add(1, Ordering::Relaxed), answer: ksync::Mutex::named("mcp.gate.answer", (Vec::new(), 0)), busy: ksync::Mutex::named("mcp.gate.busy", ()), authed: AtomicBool::new(false) })
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/bitsos/mcp", make);

/// The armed gate.
pub(crate) fn armed() -> Option<&'static Gate> {
    GATE.get()
}

mod access;
pub mod identity;
mod installer;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod installer_tests;
#[cfg(test)]
mod access_tests;
mod ktests;
