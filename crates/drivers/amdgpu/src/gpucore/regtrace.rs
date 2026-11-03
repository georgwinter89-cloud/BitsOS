//! GX-7c: a Linux register trace of an amdgpu load (the ftrace text of the 27.10. capture,
//! plan/hw/gx-780m-linux-trace-*) as a list of register events. Pure and self-contained (only `alloc`), so the
//! host tool `regtrace` (host/gen/dcn314) includes this file as it is and the diff harness (`regdiff`) works on
//! the same events. Never panics on its input: a line it cannot read is counted, not fatal.
//!
//! The events (Linux 6.6+ amdgpu_trace.h; `gx_*` are the capture kit's kprobes on the indirect paths):
//! - `amdgpu_device_rreg: 0x1900, 0x00000de5, 0x00000000`: device id, BAR5 register in dwords, value;
//!   `amdgpu_device_wreg` likewise.
//! - `amdgpu_dc_rreg: reg=0x00003846, value=0x00000056`: Display Core logs each of its accesses a second time,
//!   right after the device event; the pair marks that device event as DC's (`Event::dc`).
//! - `gx_ind_rreg: (...) addr=0x3b10028` then `gx_ind_rreg_ret: (...) val=0x1`; `gx_ind_wreg: (...) addr=..
//!   val=..`: an SMN access through the PCIe index/data pair (`amdgpu_device_indirect_rreg`).
//! - `gx_port_rreg/_ret/wreg` with `reg=`: a PCIe port register (`amdgpu_device_pcie_port_rreg`).
//!
//! Every other line (comments, BO and IRQ events, DM events) is counted as other.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    SmnRead,
    SmnWrite,
    PortRead,
    PortWrite,
}

impl Kind {
    pub fn is_write(self) -> bool {
        matches!(self, Kind::Write | Kind::SmnWrite | Kind::PortWrite)
    }

    pub fn letter(self) -> &'static str {
        match self {
            Kind::Read => "R",
            Kind::Write => "W",
            Kind::SmnRead => "SR",
            Kind::SmnWrite => "SW",
            Kind::PortRead => "PR",
            Kind::PortWrite => "PW",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// 1-based line of the trace text.
    pub line: u32,
    /// The trace clock in microseconds.
    pub t_us: u64,
    pub kind: Kind,
    /// BAR5 dword (Read/Write), SMN address (Smn*), PCIe port register (Port*).
    pub reg: u32,
    pub value: u32,
    /// Display Core's own access (a DC event followed it).
    pub dc: bool,
}

impl Event {
    /// The BAR5 byte offset of a direct access.
    pub fn byte(&self) -> Option<u32> {
        if matches!(self.kind, Kind::Read | Kind::Write) { self.reg.checked_mul(4) } else { None }
    }
}

#[derive(Debug, Default)]
pub struct Trace {
    pub events: Vec<Event>,
    /// Lines that are other events or comments.
    pub other: u32,
    /// Lines of the register events above that did not parse (line numbers).
    pub bad: Vec<u32>,
    /// DC events without a device event to pair with.
    pub dc_unpaired: u32,
}

impl Trace {
    /// The direct writes as (BAR5 byte, value), DC's own left out unless `with_dc`.
    pub fn writes(&self, with_dc: bool) -> Vec<(u32, u32)> {
        self.events.iter().filter(|e| e.kind == Kind::Write && (with_dc || !e.dc)).filter_map(|e| Some((e.byte()?, e.value))).collect()
    }
}

/// Hex with or without 0x, ended by the first non-hex character.
fn hex_at(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let end = s.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(s.len());
    if end == 0 || end > 8 {
        return None;
    }
    u32::from_str_radix(&s[..end], 16).ok()
}

/// The hex number after `key` (`addr=`, `val=`, `reg=`, `value=`).
fn field(s: &str, key: &str) -> Option<u32> {
    let mut from = 0;
    // `reg=` must not match inside `rreg=` style words: the key starts the string or follows a space or '('.
    while let Some(i) = s[from..].find(key) {
        let at = from + i;
        let ok = at == 0 || matches!(s.as_bytes()[at - 1], b' ' | b'(' | b',');
        if ok {
            return hex_at(&s[at + key.len()..]);
        }
        from = at + key.len();
    }
    None
}

/// `31.765661` as microseconds.
fn time_us(s: &str) -> Option<u64> {
    let (sec, frac) = s.split_once('.')?;
    let sec: u64 = sec.parse().ok()?;
    if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut us: u64 = 0;
    for b in frac.bytes().chain(core::iter::repeat(b'0')).take(6) {
        us = us * 10 + u64::from(b - b'0');
    }
    sec.checked_mul(1_000_000)?.checked_add(us)
}

const EVENTS: &[&str] = &[
    "amdgpu_device_rreg",
    "amdgpu_device_wreg",
    "amdgpu_dc_rreg",
    "amdgpu_dc_wreg",
    "gx_ind_rreg",
    "gx_ind_rreg_ret",
    "gx_ind_wreg",
    "gx_port_rreg",
    "gx_port_rreg_ret",
    "gx_port_wreg",
];

/// (event name, timestamp, payload) of an ftrace line: `<task>-<pid> [cpu] flags <time>: <event>: <payload>`.
fn split(line: &str) -> Option<(&str, &str, &str)> {
    for &e in EVENTS {
        // The longer names share prefixes with the shorter ones: match ": name: " exactly.
        let mut pat_at = None;
        let mut from = 0;
        while let Some(i) = line[from..].find(e) {
            let at = from + i;
            let before = line[..at].ends_with(": ");
            let after = line[at + e.len()..].starts_with(": ");
            if before && after {
                pat_at = Some(at);
                break;
            }
            from = at + e.len();
        }
        let Some(at) = pat_at else { continue };
        let head = line[..at].trim_end().strip_suffix(':')?;
        let t = head.rsplit(' ').next()?;
        return Some((e, t, &line[at + e.len() + 2..]));
    }
    None
}

/// Parses the whole text. An indirect read whose `_ret` line is missing is dropped (counted bad).
pub fn parse(text: &str) -> Trace {
    let mut tr = Trace::default();
    let mut pending: Option<(u32, u64, Kind, u32)> = None;
    for (i, line) in text.lines().enumerate() {
        let ln = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let Some((ev, t, p)) = split(line) else {
            tr.other += 1;
            continue;
        };
        let Some(t_us) = time_us(t) else {
            tr.bad.push(ln);
            continue;
        };
        let push = |tr: &mut Trace, kind, reg, value| tr.events.push(Event { line: ln, t_us, kind, reg, value, dc: false });
        let ok = match ev {
            "amdgpu_device_rreg" | "amdgpu_device_wreg" => {
                let mut it = p.split(',');
                let (_did, reg, val) = (it.next(), it.next().and_then(hex_at), it.next().and_then(hex_at));
                match (reg, val) {
                    (Some(r), Some(v)) => {
                        push(&mut tr, if ev.ends_with("rreg") { Kind::Read } else { Kind::Write }, r, v);
                        true
                    }
                    _ => false,
                }
            }
            "amdgpu_dc_rreg" | "amdgpu_dc_wreg" => match (field(p, "reg="), field(p, "value=")) {
                (Some(r), Some(v)) => {
                    let kind = if ev.ends_with("rreg") { Kind::Read } else { Kind::Write };
                    // The device event comes right before; a few events of other CPUs may lie between.
                    let n = tr.events.len();
                    let hit = tr.events[n.saturating_sub(4)..].iter_mut().rev().find(|e| !e.dc && e.kind == kind && e.reg == r && e.value == v);
                    match hit {
                        Some(e) => e.dc = true,
                        None => tr.dc_unpaired += 1,
                    }
                    true
                }
                _ => false,
            },
            "gx_ind_rreg" | "gx_port_rreg" => {
                let key = if ev == "gx_ind_rreg" { "addr=" } else { "reg=" };
                if let Some((l, _, _, _)) = pending.take() {
                    tr.bad.push(l);
                }
                match field(p, key) {
                    Some(a) => {
                        pending = Some((ln, t_us, if ev == "gx_ind_rreg" { Kind::SmnRead } else { Kind::PortRead }, a));
                        true
                    }
                    None => false,
                }
            }
            "gx_ind_rreg_ret" | "gx_port_rreg_ret" => match (pending.take(), field(p, "val=")) {
                (Some((l, t0, kind, a)), Some(v)) => {
                    tr.events.push(Event { line: l, t_us: t0, kind, reg: a, value: v, dc: false });
                    true
                }
                _ => false,
            },
            _ => {
                let key = if ev == "gx_ind_wreg" { "addr=" } else { "reg=" };
                match (field(p, key), field(p, "val=")) {
                    (Some(a), Some(v)) => {
                        push(&mut tr, if ev == "gx_ind_wreg" { Kind::SmnWrite } else { Kind::PortWrite }, a, v);
                        true
                    }
                    _ => false,
                }
            }
        };
        if !ok {
            tr.bad.push(ln);
        }
    }
    if let Some((l, _, _, _)) = pending {
        tr.bad.push(l);
    }
    tr
}
