//! Georg's rules for programs (owner: PM; contract B3, V2-PLAN §1a).
//!
//! One hook, `check(subject, access)`, sits wherever a program reaches beyond its own package and data folder:
//! file open/create outside them, socket connect/bind/listen, opening a device by class, signals to other
//! programs, MCP action calls. Every connection is also recorded (`record_conn`) for the connection view.
//! Lanes call the hook at their entry points (FS-1, NW-5, IN-4, SN-4, DP-2, AB, MC-7); the checkers check it.
//!
//! PM-1: rule sets per program (`rules`), decisions allow/ask/deny, a fast path without a lock while no
//! rule can refuse (`table`), `ask` parks the caller with a deadline (`ask`). Defaults (B3: Georg decides, no
//! prompts by default): BitsOS itself and every program without rules are allowed, their connections
//! recorded. An engine installed with `set_engine` (tests) replaces the rule table entirely.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::OnceRef;

mod ask;
mod conn;
mod dns;
mod ktests;
mod ktests_view;
pub mod rules;
mod table;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_view;
mod udp;
mod view;

pub use ask::{answer, ask_timeout_ns, pending_asks, set_ask_listener, set_ask_timeout_ns, AskInfo, AskListener};
pub use conn::{conn_keep, record_conn, recent_connections, set_conn_keep, ConnKind, ConnRecord};
pub use dns::{dns_keep, dns_names, note_dns_answer, parse_answer, set_dns_keep};
pub use udp::{set_udp_peer_keep, udp_peer_keep, UdpPeers};
pub use view::{
    connection_counts, connections, forget_test_connections, open as open_conn, open_counted, proto_name, record_live_ns, set_owners, set_record_live_ns,
    ConnCounters, ConnFilter, ConnHandle, ConnInfo, Owners,
};
pub use rules::{Decision, DeviceRule, NameRule, NetRule, PathRule, RuleSet, SignalRule};
pub use table::{generation, generation_tag, learned_rules, rules_of, set_default_rules, set_rules, Cache};

/// Which program a process belongs to (B2: set from the package manifest at execve; 0 = BitsOS itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ProgramId(pub u32);

impl ProgramId {
    pub const SYSTEM: ProgramId = ProgramId(0);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subject {
    pub program: ProgramId,
    pub pid: u32,
    pub uid: u32,
}

impl Subject {
    pub const KERNEL: Subject = Subject { program: ProgramId::SYSTEM, pid: 0, uid: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceClass {
    Microphone,
    /// Sound playback (SN-4: /dev/snd/pcmC0D0p, controlC0).
    Audio,
    Camera,
    InputGrab,
    ScreenCapture,
    RawUsb,
    Disk,
    Gpu,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetProto {
    Tcp,
    Udp,
    Unix,
    Other,
}

/// An IP address (v4 mapped into the last 4 bytes with a v4 flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpAddr {
    V4([u8; 4]),
    V6([u8; 16]),
}

#[derive(Clone, Copy, Debug)]
pub enum Access<'a> {
    /// Open or create outside the program's own package and data folder.
    Path { path: &'a str, write: bool, create: bool },
    NetConnect { proto: NetProto, addr: IpAddr, port: u16 },
    NetBind { proto: NetProto, port: u16 },
    NetListen { port: u16 },
    Device { class: DeviceClass, name: &'a str },
    Signal { target_pid: u32, signal: u32 },
    McpAction { tool: &'a str },
    /// A scheduling grant beyond the program's own threads (SC-7a, B4): today only the compositor grant that
    /// fb0's scanout holder asks for (`FBIO_BITSOS_COMPOSITOR`, gopfb fbdev/compositor.rs).
    Scheduling { grant: &'a str },
}

/// Coarse class of an access (accounting, defaults per class).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessClass {
    Path,
    Net,
    Device,
    Signal,
    McpAction,
    Scheduling,
}

pub const CLASSES: usize = 6;
pub const ALL_CLASSES: [AccessClass; CLASSES] =
    [AccessClass::Path, AccessClass::Net, AccessClass::Device, AccessClass::Signal, AccessClass::McpAction, AccessClass::Scheduling];

impl Access<'_> {
    pub fn class(&self) -> AccessClass {
        match self {
            Access::Path { .. } => AccessClass::Path,
            Access::NetConnect { .. } | Access::NetBind { .. } | Access::NetListen { .. } => AccessClass::Net,
            Access::Device { .. } => AccessClass::Device,
            Access::Signal { .. } => AccessClass::Signal,
            Access::McpAction { .. } => AccessClass::McpAction,
            Access::Scheduling { .. } => AccessClass::Scheduling,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied {
    /// Shown to Georg and in the log, e.g. "Brave darf nicht auf das Mikrofon zugreifen (Regel: nie)".
    pub reason: String,
}

/// An engine that replaces the rule table (host tests and ktests install their own). `check` may block.
pub trait Engine: Sync {
    fn check(&self, subject: &Subject, access: &Access<'_>) -> Result<(), Denied>;
}

static ENGINE: OnceRef<dyn Engine> = OnceRef::new();

pub fn set_engine(e: &'static dyn Engine) {
    ENGINE.set(e);
}

static COUNTS: [AtomicU64; CLASSES] = [const { AtomicU64::new(0) }; CLASSES];
static DENIED: [AtomicU64; CLASSES] = [const { AtomicU64::new(0) }; CLASSES];
klog::counter!(DENIALS, "policy.denied");

#[inline]
fn is_kernel(s: &Subject) -> bool {
    s.program == ProgramId::SYSTEM && s.pid == 0
}

/// The B3 hook. Kernel-internal accesses (Subject::KERNEL) are always allowed and not counted.
pub fn check(subject: &Subject, access: &Access<'_>) -> Result<(), Denied> {
    if is_kernel(subject) {
        return Ok(());
    }
    let class = access.class();
    COUNTS[class as usize].fetch_add(1, Ordering::Relaxed);
    if let Some(e) = ENGINE.get() {
        return e.check(subject, access);
    }
    if table::all_allow(class) {
        return Ok(());
    }
    finish(subject, access, table::decide(subject.program, access))
}

/// As `check`, with the calling process's `Cache` (no lock while its program's class is unconditional).
pub fn check_cached(cache: &Cache, subject: &Subject, access: &Access<'_>) -> Result<(), Denied> {
    if is_kernel(subject) {
        return Ok(());
    }
    let class = access.class();
    COUNTS[class as usize].fetch_add(1, Ordering::Relaxed);
    if let Some(e) = ENGINE.get() {
        return e.check(subject, access);
    }
    if table::all_allow(class) {
        return Ok(());
    }
    let v = match cache.unconditional(subject.program, class) {
        Some(Decision::Ask) | None => table::decide(subject.program, access),
        Some(d) => table::Verdict { decision: d, on_timeout: d, timeout_ns: None },
    };
    finish(subject, access, v)
}

fn finish(subject: &Subject, access: &Access<'_>, v: table::Verdict) -> Result<(), Denied> {
    let (d, timed_out) = match v.decision {
        Decision::Ask => ask::ask(subject, access, v.on_timeout, v.timeout_ns.unwrap_or_else(ask_timeout_ns)),
        d => (d, false),
    };
    match d {
        Decision::Allow => Ok(()),
        _ => {
            DENIED[access.class() as usize].fetch_add(1, Ordering::Relaxed);
            DENIALS.inc();
            Err(Denied { reason: ask::reason(subject, access, timed_out) })
        }
    }
}

/// For callers that must build an expensive description first (vfs builds the path): true when every access
/// of `class` by this subject is allowed whatever its details; the access then counts as checked. False: build
/// the access and call `check`.
#[inline]
pub fn allows_any(subject: &Subject, class: AccessClass) -> bool {
    if is_kernel(subject) {
        return true;
    }
    if ENGINE.get().is_some() {
        return false;
    }
    let yes = table::all_allow(class) || table::unconditional(subject.program, class) == Some(Decision::Allow);
    if yes {
        COUNTS[class as usize].fetch_add(1, Ordering::Relaxed);
    }
    yes
}

/// The access in words, for prompts and refusals.
pub fn describe(a: &Access<'_>) -> String {
    match *a {
        Access::Path { path, write, create } => {
            let how = match (write, create) {
                (_, true) => "anlegen",
                (true, false) => "schreiben",
                (false, false) => "lesen",
            };
            format!("Datei {path} {how}")
        }
        Access::NetConnect { proto, addr, port } => format!("Verbindung {proto:?} zu {} Port {port}", ip_text(addr)),
        Access::NetBind { proto, port } => format!("{proto:?}-Port {port} belegen"),
        Access::NetListen { port } => format!("auf Port {port} lauschen"),
        Access::Device { class, name } => format!("Gerät {class:?} ({name}) öffnen"),
        Access::Signal { target_pid, signal } => format!("Signal {signal} an Prozess {target_pid}"),
        Access::McpAction { tool } => format!("Aktion {tool}"),
        Access::Scheduling { grant } => format!("Vorrang {grant}"),
    }
}

fn ip_text(a: IpAddr) -> String {
    match a {
        IpAddr::V4(b) => format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
        IpAddr::V6(b) => {
            let mut s = String::new();
            for (i, w) in b.chunks(2).enumerate() {
                if i > 0 {
                    s.push(':');
                }
                s.push_str(&format!("{:x}", u16::from_be_bytes([w[0], w[1]])));
            }
            s
        }
    }
}

pub fn decision_name(d: Decision) -> &'static str {
    match d {
        Decision::Allow => "allow",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    }
}

pub fn class_name(c: AccessClass) -> &'static str {
    match c {
        AccessClass::Path => "path",
        AccessClass::Net => "net",
        AccessClass::Device => "device",
        AccessClass::Signal => "signal",
        AccessClass::McpAction => "mcp_action",
        AccessClass::Scheduling => "scheduling",
    }
}

/// Program accesses checked per class (allowed + asked + denied).
pub fn counts() -> [(AccessClass, u64); CLASSES] {
    ALL_CLASSES.map(|c| (c, COUNTS[c as usize].load(Ordering::Relaxed)))
}

/// Accesses refused per class (rule `deny`, or an ask answered no or timed out to deny).
pub fn denied() -> [(AccessClass, u64); CLASSES] {
    ALL_CLASSES.map(|c| (c, DENIED[c as usize].load(Ordering::Relaxed)))
}

