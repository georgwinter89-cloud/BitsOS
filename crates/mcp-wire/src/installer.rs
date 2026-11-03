//! MC-9a: the installer's side of the gate `/dev/bitsos/mcp`. bitsmcp's `install` requests (`gate::Command::
//! Install`) are not run at once: the kernel queues each one for the installer service `bitsinstall`, the only
//! process the kernel lets claim that role (its executable is the very `/bin/bitsinstall` this boot's initramfs
//! brought, unchanged; the kernel checks it at `claim` and at every later command). The installer asks a person
//! (bitscomp's dialog, MC-9b) or finds a standing grant, and tells the kernel yes or no; only after a yes does
//! the kernel run the tool, and the answer goes back to bitsmcp's waiting `install`. Without an installer every
//! `install` is EPERM (as in MC-8a).
//!
//! | command (installer session) | answer |
//! |---|---|
//! | `claim` | `{"installer":"claimed","pid":N}`; EPERM when the caller is not the boot's `/bin/bitsinstall` running as root, EBUSY while another session holds the role |
//! | `next <wait_ms>` | the oldest request not handed out yet, `{"id","tool","kind","arguments","timeout_ms","peer"}`, or `none` after `wait_ms` (at most `NEXT_WAIT_MAX_MS`); EPERM without the claim |
//! | `decide {"id","yes","by","reason"}` | `ok`; ENOENT for an id that is not waiting (answered, timed out, its caller gone); EPERM without the claim |
//! | `grants {"grants":[{"client","until","set_at","nvme"}]}` | MC-10: `ok`, the full-access table replaced (`crate::access`); EINVAL for a grant the clock rule refuses; EPERM without the claim |
//!
//! A request waits at most `DECISION_WAIT_MS` for its decision (then it is refused, `by` = `kernel`), and every
//! step is a kernel event (`mcp svc.install_asked`, `svc.install_decided`, `svc.call`), so the journal keeps
//! who asked for what and who decided how.

use crate::{Json, json};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The longest `next` wait (the installer loops; B4: no unbounded wait in the kernel).
pub const NEXT_WAIT_MAX_MS: u64 = 10_000;
/// How long a request waits for the installer's decision (queue + the person's answer).
pub const DECISION_WAIT_MS: u64 = 300_000;
/// The dialog's default and longest time (so a decision always comes before `DECISION_WAIT_MS`).
pub const DIALOG_DEFAULT_MS: u64 = 60_000;
pub const DIALOG_MAX_MS: u64 = 240_000;
/// The installer's executable as the boot brings it.
pub const EXE: &str = "/bin/bitsinstall";

/// What a request does, for the dialog and for grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// install_kernel, install_system: a new kernel or system generation on the boot stick.
    Install,
    /// system_pin: which generation starts.
    Pin,
    /// reload: a new kernel now, without a reset.
    Reload,
    /// reboot.
    Reboot,
    /// A guarded setting (settingsd's `settings_set`, `settings_undo`).
    Settings,
    /// Opening the NVMe gate (no tool yet; never granted, always asked).
    Nvme,
    /// Anything else the installer is handed (a changing tool of a root service): always asked.
    Other,
    /// MC-10: full access for a paired client (`request_access`): always asked, the answer is Georg's choice of
    /// time (`crate::access`), never a standing grant of this list.
    Access,
    /// MC-10: pairing a new client under `mcp=service` (`pair_client`): always asked.
    Pair,
}

pub const KINDS: [Kind; 9] = [Kind::Install, Kind::Pin, Kind::Reload, Kind::Reboot, Kind::Settings, Kind::Nvme, Kind::Other, Kind::Access, Kind::Pair];

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Install => "install",
            Kind::Pin => "pin",
            Kind::Reload => "reload",
            Kind::Reboot => "reboot",
            Kind::Settings => "settings",
            Kind::Nvme => "nvme",
            Kind::Other => "other",
            Kind::Access => "access",
            Kind::Pair => "pair",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        KINDS.into_iter().find(|k| k.name() == s)
    }
    /// Whether a standing grant may cover it. The NVMe gate, unknown actions, full access and pairings are
    /// always asked.
    pub fn grantable(self) -> bool {
        !matches!(self, Kind::Nvme | Kind::Other | Kind::Access | Kind::Pair)
    }
    pub fn of(tool: &str) -> Kind {
        match tool {
            "install_kernel" | "install_system" => Kind::Install,
            "system_pin" => Kind::Pin,
            "reload" => Kind::Reload,
            "reboot" => Kind::Reboot,
            "nvme_gate" => Kind::Nvme,
            crate::access::REQUEST_ACCESS => Kind::Access,
            crate::access::PAIR_CLIENT => Kind::Pair,
            t if t.starts_with("settings_") => Kind::Settings,
            _ => Kind::Other,
        }
    }
}

/// A request as the installer gets it from `next`.
#[derive(Clone, Debug, PartialEq)]
pub struct Asked {
    pub id: u64,
    pub tool: String,
    pub kind: Kind,
    pub args: Json,
    pub timeout_ms: u64,
    pub peer: String,
    /// MC-10: the asking client's key ("" for a plain connection).
    pub client: String,
}

pub fn encode_asked(a: &Asked) -> String {
    let mut s = format!("{{\"id\":{},\"tool\":", a.id);
    json::escape_into(&a.tool, &mut s);
    s.push_str(&format!(",\"kind\":\"{}\",\"arguments\":", a.kind.name()));
    s.push_str(&a.args.to_string());
    s.push_str(&format!(",\"timeout_ms\":{},\"peer\":", a.timeout_ms));
    json::escape_into(&a.peer, &mut s);
    if !a.client.is_empty() {
        s.push_str(&format!(",\"client\":\"{}\"", a.client));
    }
    s.push('}');
    s
}

/// `next`'s answer: Some(None) for `none`.
pub fn decode_next_answer(text: &str) -> Option<Option<Asked>> {
    if text.trim() == "none" {
        return Some(None);
    }
    let j = Json::parse(text).ok()?;
    let tool = String::from(j.get("tool")?.as_str()?);
    Some(Some(Asked {
        id: j.get("id")?.as_u64()?,
        kind: Kind::parse(j.get("kind")?.as_str()?).unwrap_or(Kind::of(&tool)),
        tool,
        args: j.get("arguments").cloned().unwrap_or(Json::Obj(Vec::new())),
        timeout_ms: j.get("timeout_ms").and_then(Json::as_u64).unwrap_or(crate::rpc::DEFAULT_TIMEOUT_MS),
        peer: String::from(j.get("peer").and_then(Json::as_str).unwrap_or("-")),
        client: String::from(j.get("client").and_then(Json::as_str).filter(|c| crate::access::is_client(c)).unwrap_or("")),
    }))
}

/// The installer's word on one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub id: u64,
    pub yes: bool,
    /// How it was decided: `dialog`, `grant`, `no_session`, `timeout`, `refused` (short, `[a-z_]`).
    pub by: String,
    /// What the person or the rule said (printable, cut at `REASON_MAX`).
    pub reason: String,
}

const BY_MAX: usize = 16;
pub const REASON_MAX: usize = 200;

fn clean(s: &str, max: usize) -> String {
    s.chars().take(max).map(|c| if c.is_control() { ' ' } else { c }).collect()
}

pub fn encode_decide(d: &Decision) -> Vec<u8> {
    let mut s = format!("decide {{\"id\":{},\"yes\":{},\"by\":", d.id, d.yes);
    json::escape_into(&clean(&d.by, BY_MAX), &mut s);
    s.push_str(",\"reason\":");
    json::escape_into(&clean(&d.reason, REASON_MAX), &mut s);
    s.push('}');
    s.into_bytes()
}

pub(crate) fn parse_decide(rest: &[u8]) -> Option<Decision> {
    let j = Json::parse(core::str::from_utf8(rest).ok()?).ok()?;
    let by = j.get("by")?.as_str()?;
    if by.is_empty() || by.len() > BY_MAX || !by.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return None;
    }
    Some(Decision { id: j.get("id")?.as_u64()?, yes: j.get("yes")?.as_bool()?, by: String::from(by), reason: clean(j.get("reason").and_then(Json::as_str).unwrap_or(""), REASON_MAX) })
}

pub(crate) fn parse_next(rest: &[u8]) -> Option<u64> {
    let ms: u64 = core::str::from_utf8(rest).ok()?.trim().parse().ok()?;
    Some(ms.min(NEXT_WAIT_MAX_MS))
}

pub fn encode_claim() -> Vec<u8> {
    b"claim".to_vec()
}
pub fn encode_next(wait_ms: u64) -> Vec<u8> {
    format!("next {}", wait_ms.min(NEXT_WAIT_MAX_MS)).into_bytes()
}
pub fn encode_claim_answer(pid: u32) -> String {
    format!("{{\"installer\":\"claimed\",\"pid\":{pid}}}")
}

/// What bitsmcp's caller reads when the installer (or the kernel's bound) said no.
pub fn refused_text(tool: &str, by: &str, reason: &str) -> String {
    let how = match by {
        "dialog" => "the person at the machine said no",
        "no_session" => "nobody can confirm it: no desktop session is running, and the text console cannot ask (no real keyboard, or a program has the screen)",
        "timeout" => "nobody answered the confirmation in time",
        "kernel" => "no decision came",
        "grant" => "a standing grant refused it",
        _ => "the installer refused it",
    };
    if reason.is_empty() { format!("{tool} was not done: {how}") } else { format!("{tool} was not done: {how} ({reason})") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{Command, parse_command};

    #[test]
    fn kinds_and_grants() {
        assert_eq!(Kind::of("install_kernel"), Kind::Install);
        assert_eq!(Kind::of("install_system"), Kind::Install);
        assert_eq!(Kind::of("system_pin"), Kind::Pin);
        assert_eq!(Kind::of("settings_set"), Kind::Settings);
        assert_eq!(Kind::of("nvme_gate"), Kind::Nvme);
        assert_eq!(Kind::of("frobnicate"), Kind::Other);
        assert!(Kind::Install.grantable() && Kind::Reload.grantable() && !Kind::Nvme.grantable() && !Kind::Other.grantable());
        assert_eq!((Kind::of("request_access"), Kind::of("pair_client")), (Kind::Access, Kind::Pair));
        assert!(!Kind::Access.grantable() && !Kind::Pair.grantable(), "full access and pairings are always asked");
        for k in KINDS {
            assert_eq!(Kind::parse(k.name()), Some(k));
        }
        assert_eq!(Kind::parse("all"), None);
    }

    #[test]
    fn commands_both_ways() {
        assert_eq!(parse_command(b"claim"), Some(Command::Claim));
        assert_eq!(parse_command(b"claim x"), None);
        assert_eq!(parse_command(b"next 250"), Some(Command::Next { wait_ms: 250 }));
        assert_eq!(parse_command(b"next 99999999"), Some(Command::Next { wait_ms: NEXT_WAIT_MAX_MS }));
        assert_eq!(parse_command(b"next"), None);
        assert_eq!(parse_command(b"next -1"), None);
        let d = Decision { id: 7, yes: true, by: "dialog".into(), reason: "Georg: ja\n".into() };
        let Some(Command::Decide(back)) = parse_command(&encode_decide(&d)) else { panic!("decide") };
        assert_eq!((back.id, back.yes, back.by.as_str(), back.reason.as_str()), (7, true, "dialog", "Georg: ja "));
        for bad in [&b"decide {}"[..], b"decide {\"id\":1,\"yes\":true,\"by\":\"Dialog\"}", b"decide {\"id\":1,\"yes\":\"y\",\"by\":\"dialog\"}", b"decide x"] {
            assert_eq!(parse_command(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        let a = Asked { id: 3, tool: "reload".into(), kind: Kind::Reload, args: Json::parse(r#"{"url":"http://10.0.2.2:8770/"}"#).unwrap(), timeout_ms: 5000, peer: "10.0.2.2:4000".into(), client: String::new() };
        assert_eq!(decode_next_answer(&encode_asked(&a)), Some(Some(a.clone())));
        let c = Asked { client: "f".repeat(64), kind: Kind::Access, tool: "request_access".into(), ..a };
        assert_eq!(decode_next_answer(&encode_asked(&c)), Some(Some(c)));
        assert_eq!(decode_next_answer("none"), Some(None));
        assert_eq!(decode_next_answer("{"), None);
    }

    #[test]
    fn refusal_texts_say_who() {
        assert!(refused_text("reload", "dialog", "").contains("said no"));
        assert!(refused_text("reload", "timeout", "60 s").ends_with("(60 s)"));
        assert!(refused_text("reload", "no_session", "").contains("no desktop session"));
    }
}
