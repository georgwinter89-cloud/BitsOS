//! MC-10: full access for one paired client, for a time Georg chose at the Mini-PC's real keyboard. The grant
//! names the client by its key (the SHA-256 of its paired certificate, `client`), ends at a wall-clock time
//! (`until`, Unix seconds) and may also open the NVMe write gate (`nvme`, Georg's tick). Only the installer
//! `bitsinstall` makes one (from bitscomp's dialog, real input only) and keeps it on /data; it hands the kernel
//! the table (`grants` at the gate, `encode_table`); the kernel relays every kernel tool to that client while
//! the grant holds (`gate::offer_for`). No tool and no CONFIG.TXT key creates or extends one.
//!
//! The clock (`judge`): Unix time comes from the RTC (CLOCK_REALTIME), which is not trusted to move evenly. A
//! grant ends when its time is reached, when the clock stands before the moment it was set, when it is longer
//! than `MAX_S`, and when the wall clock moved against the monotonic clock by more than the grant's own length
//! since the grant was first seen in this boot (then its end time is not what Georg chose: it is refused rather
//! than trusted). A jump back by less than that does not lengthen it: the monotonic time since it was seen
//! counts too.

use crate::{Json, json};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The longest grant: seven days (the dialog's longest choice).
pub const MAX_S: i64 = 7 * 86_400;
/// The dialog's choices in hours; `DEFAULT_H` is preselected.
pub const CHOICES_H: [i64; 4] = [1, 8, 24, 168];
pub const DEFAULT_H: i64 = 24;
/// A reason the desk gives with `request_access` is cut to this many characters.
pub const REASON_MAX: usize = 200;
/// A client's name (its certificate's common name) as shown, cut to this many characters.
pub const NAME_MAX: usize = 40;

/// The service's own tools (bitsmcp answers them; the kernel never lists them).
pub const REQUEST_ACCESS: &str = "request_access";
pub const ACCESS_STATUS: &str = "access_status";
/// What bitsmcp asks the installer before it pairs a client under `mcp=service` (never a tool of the list).
pub const PAIR_CLIENT: &str = "pair_client";

pub const REQUEST_ACCESS_DESCRIPTION: &str = "Asks Georg at the Mini-PC's real keyboard for full access for this paired client (every kernel tool: run, files, screen, send_keys, install, reboot, every ktest class) for a time he chooses (1 h, 8 h, 24 h or 7 days; 24 h preselected). The dialog waits up to 4 minutes; the answer names the end time. hours: what you would like (shown to Georg); reason: one line for him. Nothing else can grant or extend it.";
pub const REQUEST_ACCESS_SCHEMA: &str = r#"{"type":"object","properties":{"hours":{"type":"integer","minimum":1,"maximum":168},"reason":{"type":"string","maxLength":200}}}"#;
pub const ACCESS_STATUS_DESCRIPTION: &str = "Whether this client has full access now, until when (Unix seconds and local time) and how long is left, and whether the grant also opens the NVMe write gate.";
pub const ACCESS_STATUS_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

/// One grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// SHA-256 of the client's certificate, 64 lower-case hex digits.
    pub client: String,
    pub until: i64,
    pub set_at: i64,
    pub nvme: bool,
}

/// Why a grant ended (or was not taken).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Expired,
    ClockBack,
    ClockJump,
    TooLong,
    Revoked,
    Replaced,
}

impl Ended {
    pub fn name(self) -> &'static str {
        match self {
            Ended::Expired => "expired",
            Ended::ClockBack => "clock_back",
            Ended::ClockJump => "clock_jump",
            Ended::TooLong => "too_long",
            Ended::Revoked => "revoked",
            Ended::Replaced => "replaced",
        }
    }
    pub fn text(self) -> &'static str {
        match self {
            Ended::Expired => "expired",
            Ended::ClockBack => "ended: the clock stands before the moment it was set",
            Ended::ClockJump => "ended: the clock jumped by more than the grant's own length",
            Ended::TooLong => "refused: longer than 7 days",
            Ended::Revoked => "revoked at the keyboard",
            Ended::Replaced => "replaced by a new grant",
        }
    }
}

/// When this boot (or this process) first saw a grant: the wall clock then and the monotonic seconds then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub wall: i64,
    pub mono: i64,
}

/// The clock rule (module text). `seen`: when the grant was first seen, `mono_now` the monotonic seconds now.
pub fn judge(g: &Grant, wall_now: i64, seen: Seen, mono_now: i64) -> Result<(), Ended> {
    let len = g.until.checked_sub(g.set_at).ok_or(Ended::TooLong)?;
    if len <= 0 || len > MAX_S {
        return Err(Ended::TooLong);
    }
    let elapsed = mono_now.saturating_sub(seen.mono).max(0);
    let expected = seen.wall.saturating_add(elapsed);
    if wall_now.saturating_sub(expected).saturating_abs() > len {
        return Err(Ended::ClockJump);
    }
    if wall_now < g.set_at {
        return Err(Ended::ClockBack);
    }
    // The end by the wall clock, and by the monotonic time since it was seen (a clock set back a little does not
    // stretch it).
    if wall_now >= g.until || elapsed >= g.until.saturating_sub(seen.wall) {
        return Err(Ended::Expired);
    }
    Ok(())
}

/// Seconds left by `judge`'s two ends (0 when it does not hold).
pub fn left_s(g: &Grant, wall_now: i64, seen: Seen, mono_now: i64) -> i64 {
    if judge(g, wall_now, seen, mono_now).is_err() {
        return 0;
    }
    let elapsed = mono_now.saturating_sub(seen.mono).max(0);
    g.until.saturating_sub(wall_now).min(g.until.saturating_sub(seen.wall).saturating_sub(elapsed)).max(0)
}

/// A client key's text form: 64 lower-case hex digits.
pub fn is_client(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The fingerprint as the dialog shows it: the first 16 hex digits in groups of four ("ab12 cd34 ef56 7890").
pub fn fp_short(fp: &str) -> String {
    let head: Vec<&str> = fp.as_bytes().chunks(4).take(4).filter_map(|c| core::str::from_utf8(c).ok()).collect();
    head.join(" ")
}

/// Text a client or the desk sent, made safe to show: printable, no line breaks, at most `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// The installer's table for the kernel: `grants {"grants":[...]}`.
pub fn encode_table(list: &[Grant]) -> Vec<u8> {
    let mut s = String::from("grants {\"grants\":[");
    for (i, g) in list.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{{\"client\":\"{}\",\"until\":{},\"set_at\":{},\"nvme\":{}}}", g.client, g.until, g.set_at, g.nvme));
    }
    s.push_str("]}");
    s.into_bytes()
}

/// The table as the kernel reads it (None: EINVAL). At most `TABLE_MAX` grants, each client once.
pub(crate) fn parse_table(rest: &[u8]) -> Option<Vec<Grant>> {
    let j = Json::parse(core::str::from_utf8(rest).ok()?).ok()?;
    let mut v: Vec<Grant> = Vec::new();
    for g in j.get("grants")?.as_array()? {
        let client = g.get("client")?.as_str()?;
        if !is_client(client) || v.iter().any(|o| o.client == client) {
            return None;
        }
        v.push(Grant { client: String::from(client), until: g.get("until")?.as_i64()?, set_at: g.get("set_at")?.as_i64()?, nvme: g.get("nvme").and_then(Json::as_bool).unwrap_or(false) });
    }
    (v.len() <= TABLE_MAX).then_some(v)
}

/// More paired clients than this is no table the installer sends (one slot today, MC-8b).
pub const TABLE_MAX: usize = 64;

/// `access <client>`: bitsmcp asks for one client's state (`access_status`).
pub fn encode_access(client: &str) -> Vec<u8> {
    format!("access {client}").into_bytes()
}

/// The kernel's answer to `access`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub granted: bool,
    pub until: i64,
    pub left_s: i64,
    pub nvme: bool,
    /// The local end time as text ("2026-10-28 14:00"), empty without a grant.
    pub local: String,
    /// MC-11: whether the installer has handed the kernel its table since the start. Until then "no grant" only
    /// means "not known yet" (bitsinstall waits for the data volume, ~11 s on the Mini-PC). An answer without
    /// the field (a kernel before MC-11) counts as loaded.
    pub loaded: bool,
}

pub fn encode_status(s: &Status) -> String {
    let mut t = format!("{{\"granted\":{},\"until\":{},\"left_s\":{},\"nvme\":{},\"loaded\":{},\"local\":", s.granted, s.until, s.left_s, s.nvme, s.loaded);
    json::escape_into(&s.local, &mut t);
    t.push('}');
    t
}

pub fn decode_status(text: &str) -> Option<Status> {
    let j = Json::parse(text).ok()?;
    Some(Status {
        granted: j.get("granted")?.as_bool()?,
        until: j.get("until")?.as_i64()?,
        left_s: j.get("left_s")?.as_i64()?,
        nvme: j.get("nvme")?.as_bool()?,
        local: String::from(j.get("local").and_then(Json::as_str).unwrap_or("")),
        loaded: j.get("loaded").and_then(Json::as_bool).unwrap_or(true),
    })
}

/// MC-11: the first words of `access_status` while the installer has not reported; the desk's tools wait on them.
pub const NOT_LOADED: &str = "grants not loaded yet";

/// What `access_status` answers in words.
pub fn status_text(s: &Status) -> String {
    if !s.granted && !s.loaded {
        return format!("{NOT_LOADED}: the installer has not handed the kernel its table since the start (it waits for the data volume); ask again in a few seconds.");
    }
    if !s.granted {
        return String::from("no full access for this client: read tools only; installs need a yes at the Mini-PC's keyboard. Ask with request_access.");
    }
    let (h, m) = (s.left_s / 3600, s.left_s % 3600 / 60);
    let disk = if s.nvme { "; the NVMe write gate is open for the same time" } else { "; NVMe stays closed" };
    format!("full access for this client until {} (local time, unix {}), {h} h {m} min left{disk}", s.local, s.until)
}

/// Unix seconds plus `offset_min` minutes as "YYYY-MM-DD HH:MM" (proleptic Gregorian calendar).
pub fn local_text(unix: i64, offset_min: i32) -> String {
    let t = unix.saturating_add(offset_min as i64 * 60);
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Days to civil date (H. Hinnant's algorithm, public domain).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// The common name of a certificate's subject (DER), as the dialog names the client. None: no such field.
pub fn common_name(der: &[u8]) -> Option<String> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version?, serial, signature, issuer, validity,
    // subject, ... } ... } (RFC 5280 4.1).
    let (_, cert, _) = tlv(der)?;
    let (_, tbs, _) = tlv(cert)?;
    let mut rest = tbs;
    let (tag, _, r) = tlv(rest)?;
    if tag == 0xa0 {
        rest = r;
    }
    for _ in 0..4 {
        rest = tlv(rest)?.2; // serial, signature, issuer, validity
    }
    let (_, subject, _) = tlv(rest)?;
    let mut sets = subject;
    while !sets.is_empty() {
        let (_, set, r) = tlv(sets)?;
        sets = r;
        let (_, atv, _) = tlv(set)?;
        let (otag, oid, r) = tlv(atv)?;
        if otag == 0x06 && oid == [0x55, 0x04, 0x03] {
            let (_, v, _) = tlv(r)?;
            return core::str::from_utf8(v).ok().map(|s| clean(s, NAME_MAX));
        }
    }
    None
}

/// One DER TLV: (tag, contents, what follows). Definite lengths up to 4 bytes only.
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let first = *b.get(1)?;
    let (len, head) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut l = 0usize;
        for i in 0..n {
            l = (l << 8) | *b.get(2 + i)? as usize;
        }
        (l, 2 + n)
    };
    let end = head.checked_add(len)?;
    Some((tag, b.get(head..end)?, b.get(end..)?))
}

#[cfg(test)]
mod tests;
