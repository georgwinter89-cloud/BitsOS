//! bitsact: a BitsOS program offers its actions to Claude as MCP tools (V2-PLAN §1a B5). On top of mcpreg (MC-6):
//! the tools are registered with the kernel over `/run/bitsos/mcp.sock`; every call runs on a thread of its own
//! with its deadline and cancel flag ([`Ctx`]); actions that change something (`Safety::Change`,
//! `Safety::DeleteOverwrite`) pass the permission gate first ([`perm`]): a settings key per action that is `deny`
//! until Georg allows it. The library side of AC-1 that AC-3 (the Brave adapter) needs; the manifest, the C header
//! and the `app.<name>.<action>` names of MC-7 come with AC-1.
//!
//! ```no_run
//! use bitsact::{Action, Config, Ctx, Handler, Reply, Safety};
//! struct Echo;
//! impl Handler for Echo {
//!     fn call(&self, ctx: &Ctx) -> Reply {
//!         match ctx.args().str("text") { Ok(t) => Reply::text(t), Err(r) => r }
//!     }
//! }
//! let a = Action::new("demo_echo", "Echo text.", r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#, Safety::Read);
//! bitsact::serve(&Config::new("demo"), vec![a], std::sync::Arc::new(Echo), bitsact::events::Events::system("demo"))?;
//! # Ok::<(), mcpreg::Error>(())
//! ```
#![forbid(unsafe_code)]

pub mod args;
pub mod base64;
pub mod events;
pub mod perm;

pub use mcpreg::{Call, Content, Error, Json, Reply, Safety};

use args::Args;
use events::{Events, Level, q};
use mcpreg::{Event, Replier, Service, Tool};
use perm::{Gate, Permissions};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One action: its tool name (unique among all tools; not `app.` (MC-7's) nor `settings*`), what it does for the
/// caller, its JSON schema and safety. `gate` texts name the permission key for Georg (change actions).
pub struct Action {
    pub name: String,
    pub description: String,
    pub schema: String,
    pub safety: Safety,
    pub gate_de: String,
    pub gate_en: String,
}

impl Action {
    pub fn new(name: &str, description: &str, schema: &str, safety: Safety) -> Action {
        Action {
            name: name.into(),
            description: description.into(),
            schema: schema.into(),
            safety,
            gate_de: format!("Claude darf {name} ausführen"),
            gate_en: format!("Claude may run {name}"),
        }
    }

    /// The texts of the permission key (settings page).
    pub fn gate_text(mut self, de: &str, en: &str) -> Action {
        self.gate_de = de.into();
        self.gate_en = en.into();
        self
    }

    pub fn gated(&self) -> bool {
        self.safety != Safety::Read
    }
}

/// A running call.
pub struct Ctx {
    pub call: Call,
    replier: Replier,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Ctx {
    pub fn args(&self) -> Args<'_> {
        Args(&self.call.arguments)
    }

    /// The caller stopped waiting (deadline or `stop`): the work should end; an answer is dropped.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// When the answer must be on its way: the caller's `timeout_ms`, less a margin for the answer's way back.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Err when the call was cancelled or its deadline passed (for wait loops), with the reason as the answer.
    pub fn alive(&self, what: &str) -> Result<(), Reply> {
        if self.cancelled() {
            return Err(Reply::error(format!("{what}: the call was cancelled")));
        }
        if Instant::now() >= self.deadline {
            return Err(Reply::error(format!(
                "{what}: not done within the call's timeout_ms ({} ms); call again with a larger timeout_ms",
                self.call.timeout_ms
            )));
        }
        Ok(())
    }

    /// Progress text, shown by the kernel's `output` tool while the call runs.
    pub fn progress(&self, text: &str) {
        let _ = self.replier.progress(self.call.id, text);
    }
}

pub trait Handler: Send + Sync + 'static {
    fn call(&self, ctx: &Ctx) -> Reply;
}

pub struct Config {
    /// The service's name (owner of its tools and permission keys): `[a-z][a-z0-9_-]{0,31}`.
    pub service: String,
    pub mcp_socket: String,
    pub settings_socket: String,
}

impl Config {
    /// Sockets from `BITSOS_MCP_SOCK` / `BITSOS_SETTINGS_SOCK` or the system's.
    pub fn new(service: &str) -> Config {
        Config {
            service: service.into(),
            mcp_socket: std::env::var("BITSOS_MCP_SOCK").unwrap_or_else(|_| mcpreg::DEFAULT_SOCKET.into()),
            settings_socket: std::env::var("BITSOS_SETTINGS_SOCK").unwrap_or_else(|_| perm::SETTINGS_SOCKET.into()),
        }
    }
}

/// The deadline of a call that came now: `timeout_ms` less a tenth (at most 500 ms) for the answer's way back.
/// A call without one (0) has the kernel's default, 10 s (crates/mcp/src/rpc.rs DEFAULT_TIMEOUT_MS).
pub fn deadline_of(timeout_ms: u64) -> Instant {
    let timeout_ms = if timeout_ms == 0 { 10_000 } else { timeout_ms };
    let margin = (timeout_ms / 10).min(500);
    Instant::now() + Duration::from_millis(timeout_ms.saturating_sub(margin))
}

/// Registers the actions and answers their calls until the kernel closes the connection. Prints
/// "registered <n>" on stdout once the tools are listed (scenarios wait for it).
pub fn serve(cfg: &Config, actions: Vec<Action>, handler: Arc<dyn Handler>, ev: Events) -> Result<(), Error> {
    let tools = actions.iter().map(|a| Tool::new(&a.name, &a.description, &a.schema, a.safety)).collect::<Result<Vec<_>, _>>()?;
    let gates = actions.iter().filter(|a| a.gated()).map(|a| Gate { tool: a.name.clone(), de: a.gate_de.clone(), en: a.gate_en.clone() }).collect();
    let perms = Arc::new(Permissions::new(&cfg.service, &cfg.settings_socket, gates, ev.clone()));
    if let Err(e) = perms.register() {
        // Not fatal: the gate refuses until settingsd answers, and registers again at the first gated call.
        ev.emit(Level::Warn, "perm.register_failed", &format!("why={}", q(&e)));
    }
    let mut svc = Service::connect_at(&cfg.mcp_socket, &cfg.service)?;
    let names = svc.register(&tools)?;
    ev.emit(Level::Info, "registered", &format!("tools={} socket={}", names.len(), q(&cfg.mcp_socket)));
    println!("registered {}", names.len());
    let actions: Arc<HashMap<String, Action>> = Arc::new(actions.into_iter().map(|a| (a.name.clone(), a)).collect());
    let running: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::default();
    while let Some(e) = svc.next()? {
        match e {
            Event::Cancelled { id, reason } => {
                if let Some(f) = running.lock().ok().and_then(|m| m.get(&id).cloned()) {
                    f.store(true, Ordering::Release);
                }
                ev.emit(Level::Info, "call.cancelled", &format!("id={id} reason={}", q(&reason)));
            }
            Event::Call(call) => {
                let flag = Arc::new(AtomicBool::new(false));
                if let Ok(mut m) = running.lock() {
                    m.insert(call.id, flag.clone());
                }
                let ctx = Ctx { deadline: deadline_of(call.timeout_ms), call, replier: svc.replier(), cancelled: flag };
                let (id, name) = (ctx.call.id, ctx.call.name.clone());
                let (h, acts, perms, ev2, run2) = (handler.clone(), actions.clone(), perms.clone(), ev.clone(), running.clone());
                let r = std::thread::Builder::new().name(format!("call-{id}")).spawn(move || {
                    let reply = run_one(&ctx, &acts, &perms, &*h, &ev2);
                    if let Err(e) = ctx.replier.reply(id, reply) {
                        ev2.emit(Level::Warn, "reply.failed", &format!("id={id} why={}", q(&e.to_string())));
                    }
                    if let Ok(mut m) = run2.lock() {
                        m.remove(&id);
                    }
                });
                if let Err(e) = r {
                    ev.emit(Level::Error, "call.no_thread", &format!("id={id} tool={} why={}", q(&name), q(&e.to_string())));
                    let _ = svc.replier().reply(id, Reply::error(format!("{name}: no thread for the call ({e})")));
                    if let Ok(mut m) = running.lock() {
                        m.remove(&id);
                    }
                }
            }
        }
    }
    ev.emit(Level::Warn, "ended", "why=\"the kernel closed the connection\"");
    Ok(())
}

fn run_one(ctx: &Ctx, acts: &HashMap<String, Action>, perms: &Permissions, h: &dyn Handler, ev: &Events) -> Reply {
    let Some(a) = acts.get(&ctx.call.name) else {
        return Reply::error(format!("{}: not an action of this service", ctx.call.name));
    };
    if a.gated()
        && let Err(why) = perms.check(&a.name)
    {
        return Reply::error(why);
    }
    let t0 = Instant::now();
    // A panic in an action (a bug) still answers, so the caller does not wait until its deadline.
    let r = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.call(ctx))) {
        Ok(r) => r,
        Err(p) => {
            let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            ev.emit(Level::Error, "call.panicked", &format!("tool={} why={}", q(&a.name), q(&what)));
            Reply::error(format!("{}: the action failed inside the service (a bug: {what})", a.name))
        }
    };
    let how = match &r {
        Reply::Ok(_) => "ok",
        Reply::Error(_) => "error",
        Reply::BadArgument(_) => "bad_argument",
    };
    let level = if how == "ok" { Level::Info } else { Level::Warn };
    ev.emit(level, "call", &format!("tool={} result={how} ms={}", q(&a.name), t0.elapsed().as_millis()));
    r
}
