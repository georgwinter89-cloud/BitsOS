//! Rule sets per program (PM-1): what a rule matches and which decision wins.
//!
//! Every class answers the same way: of the rules that match the access, the most specific wins (the longest
//! path prefix or name, the narrowest address and port range); between equally specific rules the strictest
//! (deny before ask before allow). No match: the class default of the set. Rules are unordered on purpose:
//! PM-3 keeps them as settings keys, which have no order either.

use crate::{Access, AccessClass, DeviceClass, IpAddr, NetProto, CLASSES};
use alloc::string::String;
use alloc::vec::Vec;

/// Ordered by strictness: `max` of two decisions is the stricter one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Decision {
    Allow,
    /// The program waits while Georg is asked (permd, PM-3); no answer by the deadline: the class default.
    Ask,
    Deny,
}

/// Path access modes (bits of `PathRule::modes`).
pub mod modes {
    pub const READ: u8 = 1;
    pub const WRITE: u8 = 2;
    pub const CREATE: u8 = 4;
    pub const ALL: u8 = READ | WRITE | CREATE;
}

/// Network directions (bits of `NetRule::dirs`).
pub mod dirs {
    pub const CONNECT: u8 = 1;
    pub const BIND: u8 = 2;
    pub const LISTEN: u8 = 4;
    pub const ALL: u8 = CONNECT | BIND | LISTEN;
}

/// `prefix` covers itself and everything below it ("/home/georg" covers "/home/georg/x", not
/// "/home/georgx"); "" and "/" cover every path. Matching is textual, so callers pass the canonical absolute
/// path (vfs: `path_of`). A path that is not canonical (relative, empty, "//", "." or ".." components) is
/// never trusted to miss a rule: it gets the strictest decision of every path rule of the mode and the class
/// default, and learned rules do not decide it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRule {
    pub prefix: String,
    pub modes: u8,
    pub decision: Decision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetRule {
    pub dirs: u8,
    /// None: any protocol. Listen is TCP (the only listener the stack asks for).
    pub proto: Option<NetProto>,
    /// Address and prefix length in bits; None: any address. Bind and listen carry no address, so a rule with
    /// an address only matches connects. A length over the address size counts as the full size (/32, /128);
    /// a v4 rule never covers a v6 address and the other way round.
    pub addr: Option<(IpAddr, u8)>,
    /// Inclusive port range; lo > hi matches nothing.
    pub ports: (u16, u16),
    pub decision: Decision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRule {
    pub class: DeviceClass,
    /// None: every device of the class; else a device name prefix ("pcmC0D0").
    pub name: Option<String>,
    pub decision: Decision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalRule {
    /// None: any signal (0 is also asked by sched_setattr and friends on another process).
    pub signal: Option<u32>,
    pub target_pid: Option<u32>,
    pub decision: Decision,
}

/// For MCP actions (tool name) and scheduling grants (grant name): the exact name, or a prefix ending in
/// `*` ("app.brave.*"); "*" covers all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRule {
    pub pattern: String,
    pub decision: Decision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSet {
    /// Per `AccessClass` (index = `class as usize`): the decision when no rule matches.
    pub defaults: [Decision; CLASSES],
    pub paths: Vec<PathRule>,
    pub net: Vec<NetRule>,
    pub devices: Vec<DeviceRule>,
    pub signals: Vec<SignalRule>,
    pub actions: Vec<NameRule>,
    pub scheduling: Vec<NameRule>,
    /// How long an `ask` of this program waits for Georg; None: `policy::ask_timeout_ns()`.
    pub ask_timeout_ns: Option<u64>,
}

impl Default for RuleSet {
    fn default() -> Self {
        RuleSet::allow_all()
    }
}

impl RuleSet {
    /// B3 default for every program until Georg sets rules: allow, connections recorded (`record_conn`).
    pub const fn allow_all() -> Self {
        RuleSet::with_defaults([Decision::Allow; CLASSES])
    }
    pub const fn with_defaults(defaults: [Decision; CLASSES]) -> Self {
        RuleSet { defaults, paths: Vec::new(), net: Vec::new(), devices: Vec::new(), signals: Vec::new(), actions: Vec::new(), scheduling: Vec::new(), ask_timeout_ns: None }
    }

    fn rules_of(&self, c: AccessClass) -> usize {
        match c {
            AccessClass::Path => self.paths.len(),
            AccessClass::Net => self.net.len(),
            AccessClass::Device => self.devices.len(),
            AccessClass::Signal => self.signals.len(),
            AccessClass::McpAction => self.actions.len(),
            AccessClass::Scheduling => self.scheduling.len(),
        }
    }

    /// The decision for every access of class `c` when it cannot depend on the access's details (no rules of
    /// that class), else None.
    pub fn unconditional(&self, c: AccessClass) -> Option<Decision> {
        (self.rules_of(c) == 0).then_some(self.defaults[c as usize])
    }

    /// What an ask that got no answer by its deadline becomes: the class default, or deny when that is
    /// itself `ask` (fail closed).
    pub fn on_timeout(&self, c: AccessClass) -> Decision {
        match self.defaults[c as usize] {
            Decision::Ask => Decision::Deny,
            d => d,
        }
    }

    pub fn decide(&self, a: &Access<'_>) -> Decision {
        let d = self.defaults[a.class() as usize];
        self.found(a, Some(d)).unwrap_or(d)
    }

    /// The decision of the rules alone (no class default): Some when rules match every mode the access
    /// needs. A non-canonical path never matches here.
    pub fn matched(&self, a: &Access<'_>) -> Option<Decision> {
        if let Access::Path { path, .. } = *a {
            if !canonical(path) {
                return None;
            }
        }
        self.found(a, None)
    }

    fn found(&self, a: &Access<'_>, fallback: Option<Decision>) -> Option<Decision> {
        match *a {
            Access::Path { path, write, create } => {
                let mut bits = 0;
                if write {
                    bits |= modes::WRITE;
                }
                if create {
                    bits |= modes::CREATE;
                }
                if bits == 0 {
                    bits = modes::READ;
                }
                let canon = canonical(path);
                // Each mode the access needs is decided on its own; the strictest wins.
                let mut d: Option<Decision> = None;
                for m in [modes::READ, modes::WRITE, modes::CREATE] {
                    if bits & m != 0 {
                        let mut rules = self.paths.iter().filter(|r| r.modes & m != 0);
                        let one = if canon {
                            best(rules.filter(|r| covers(&r.prefix, path)).map(|r| (r.prefix.trim_end_matches('/').len(), r.decision))).or(fallback)
                        } else {
                            rules.try_fold(fallback?, |x, r| Some(x.max(r.decision)))
                        };
                        let one = one?;
                        d = Some(d.map_or(one, |x| x.max(one)));
                    }
                }
                d
            }
            Access::NetConnect { proto, addr, port } => best(self.net.iter().filter(|r| net_matches(r, dirs::CONNECT, proto, Some(addr), port)).map(|r| (net_rank(r), r.decision))),
            Access::NetBind { proto, port } => best(self.net.iter().filter(|r| net_matches(r, dirs::BIND, proto, None, port)).map(|r| (net_rank(r), r.decision))),
            Access::NetListen { port } => best(self.net.iter().filter(|r| net_matches(r, dirs::LISTEN, NetProto::Tcp, None, port)).map(|r| (net_rank(r), r.decision))),
            Access::Device { class, name } => best(
                self.devices
                    .iter()
                    .filter(|r| r.class == class && r.name.as_deref().is_none_or(|n| name.starts_with(n)))
                    .map(|r| (r.name.as_ref().map_or(0, |n| n.len() + 1), r.decision)),
            ),
            Access::Signal { target_pid, signal } => best(
                self.signals
                    .iter()
                    .filter(|r| r.signal.is_none_or(|s| s == signal) && r.target_pid.is_none_or(|p| p == target_pid))
                    .map(|r| ((r.target_pid.is_some() as usize) * 2 + r.signal.is_some() as usize, r.decision)),
            ),
            Access::McpAction { tool } => best(self.actions.iter().filter_map(|r| name_rank(&r.pattern, tool).map(|k| (k, r.decision)))),
            Access::Scheduling { grant } => best(self.scheduling.iter().filter_map(|r| name_rank(&r.pattern, grant).map(|k| (k, r.decision)))),
        }
        .or(fallback)
    }
}

/// Absolute, no empty, "." or ".." component (a single trailing "/" is fine).
pub fn canonical(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    rest.is_empty() || rest.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Highest rank wins; equal rank: the strictest decision.
fn best(it: impl Iterator<Item = (usize, Decision)>) -> Option<Decision> {
    it.max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1))).map(|x| x.1)
}

pub fn covers(prefix: &str, path: &str) -> bool {
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        return true;
    }
    path.starts_with(p) && (path.len() == p.len() || path.as_bytes()[p.len()] == b'/')
}

fn net_matches(r: &NetRule, dir: u8, proto: NetProto, addr: Option<IpAddr>, port: u16) -> bool {
    r.dirs & dir != 0
        && r.proto.is_none_or(|p| p == proto)
        && (r.ports.0..=r.ports.1).contains(&port)
        && match (r.addr, addr) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some((net, bits)), Some(a)) => ip_in(a, net, bits),
        }
}

fn net_rank(r: &NetRule) -> usize {
    let addr = r.addr.map_or(0, |(_, b)| b as usize + 1);
    let ports = 65535 - (r.ports.1.saturating_sub(r.ports.0) as usize);
    let proto = r.proto.is_some() as usize;
    let dirs = 3 - r.dirs.count_ones().min(3) as usize;
    ((addr * 65536 + ports) * 2 + proto) * 4 + dirs
}

fn ip_in(a: IpAddr, net: IpAddr, bits: u8) -> bool {
    fn pre(a: &[u8], n: &[u8], bits: usize) -> bool {
        let bits = bits.min(a.len() * 8);
        let (whole, rest) = (bits / 8, bits % 8);
        a[..whole] == n[..whole] && (rest == 0 || (a[whole] ^ n[whole]) & (0xFFu8 << (8 - rest)) == 0)
    }
    match (a, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => pre(&a, &n, bits as usize),
        (IpAddr::V6(a), IpAddr::V6(n)) => pre(&a, &n, bits as usize),
        _ => false,
    }
}

/// Some(rank) when `pattern` covers `name`: the exact name ranks above every prefix.
fn name_rank(pattern: &str, name: &str) -> Option<usize> {
    if pattern == name {
        return Some(usize::MAX);
    }
    let p = pattern.strip_suffix('*')?;
    name.starts_with(p).then_some(p.len())
}

/// The rule an "always" answer adds: exactly this access (PM-3 shows and edits it as a setting).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Learned {
    Path(PathRule),
    Net(NetRule),
    Device(DeviceRule),
    Signal(SignalRule),
    Action(NameRule),
    Scheduling(NameRule),
}

impl Learned {
    pub fn of(a: &Access<'_>, decision: Decision) -> Learned {
        use alloc::string::ToString;
        match *a {
            Access::Path { path, write, create } => {
                let m = if write || create { (if write { modes::WRITE } else { 0 }) | (if create { modes::CREATE } else { 0 }) } else { modes::READ };
                Learned::Path(PathRule { prefix: path.to_string(), modes: m, decision })
            }
            Access::NetConnect { proto, addr, port } => {
                let bits = if matches!(addr, IpAddr::V4(_)) { 32 } else { 128 };
                Learned::Net(NetRule { dirs: dirs::CONNECT, proto: Some(proto), addr: Some((addr, bits)), ports: (port, port), decision })
            }
            Access::NetBind { proto, port } => Learned::Net(NetRule { dirs: dirs::BIND, proto: Some(proto), addr: None, ports: (port, port), decision }),
            Access::NetListen { port } => Learned::Net(NetRule { dirs: dirs::LISTEN, proto: Some(NetProto::Tcp), addr: None, ports: (port, port), decision }),
            Access::Device { class, name } => Learned::Device(DeviceRule { class, name: Some(name.to_string()), decision }),
            Access::Signal { target_pid, signal } => Learned::Signal(SignalRule { signal: Some(signal), target_pid: Some(target_pid), decision }),
            Access::McpAction { tool } => Learned::Action(NameRule { pattern: tool.to_string(), decision }),
            Access::Scheduling { grant } => Learned::Scheduling(NameRule { pattern: grant.to_string(), decision }),
        }
    }

    pub fn set_decision(&mut self, d: Decision) {
        match self {
            Learned::Path(r) => r.decision = d,
            Learned::Net(r) => r.decision = d,
            Learned::Device(r) => r.decision = d,
            Learned::Signal(r) => r.decision = d,
            Learned::Action(r) | Learned::Scheduling(r) => r.decision = d,
        }
    }

    /// Adds the rule; one for the same access (equal but for the decision) is replaced.
    pub fn add_to(self, s: &mut RuleSet) {
        fn put<T: Clone + PartialEq>(v: &mut Vec<T>, mut r: T, same: impl Fn(&T, &T) -> bool) {
            if let Some(old) = v.iter_mut().find(|o| same(o, &r)) {
                core::mem::swap(old, &mut r);
            } else {
                v.push(r);
            }
        }
        match self {
            Learned::Path(r) => put(&mut s.paths, r, |a, b| a.prefix == b.prefix && a.modes == b.modes),
            Learned::Net(r) => put(&mut s.net, r, |a, b| a.dirs == b.dirs && a.proto == b.proto && a.addr == b.addr && a.ports == b.ports),
            Learned::Device(r) => put(&mut s.devices, r, |a, b| a.class == b.class && a.name == b.name),
            Learned::Signal(r) => put(&mut s.signals, r, |a, b| a.signal == b.signal && a.target_pid == b.target_pid),
            Learned::Action(r) => put(&mut s.actions, r, |a, b| a.pattern == b.pattern),
            Learned::Scheduling(r) => put(&mut s.scheduling, r, |a, b| a.pattern == b.pattern),
        }
    }
}
