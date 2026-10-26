//! `dcn_snapshot`: the display registers V1's G-1 snapshot covers (werkzeuge/plattform/gpu/messung/p1-decoded.txt,
//! `dcn314_regs::P1_SNAPSHOT`) plus what the gates and the KMS view read (every pipe, OTG, DIG, the DMCUB's
//! windows, the PSP/SMU state words), read once, as a compact JSON object; and the diff against a stored
//! reference. Reads only: a pipe whose power domains are gated is not read (V1 `dcn_powered`), a DP encoder's
//! stream registers only when its back end is on, never the NBIO seg0 aliases (X8) or the AZ index/data pair.
//!
//! Narrow (the default) reads only instances the Mini-PC has already been read at: OTG0/DTO0 (G-1), DIG0/DP0
//! (SN-5, saved-5.json), plus an OTG a powered pipe drives (what the attach gates read). `wide` adds the other
//! OTGs, DTOs, DIGs and DP encoders (Linux reads them all at init, dcn10_hw_sequencer.c:1333,
//! dcn31_hwseq.c:167-168, but after its own clock init): opt-in until a Mini-PC round has shown them harmless.
//!
//! Some bits change on their own (status, pending, sticky underflow, the PSP heartbeat C2PMSG_81 at ~150/s;
//! spec/gpu.md §2.2): `compare` ignores them, so a snapshot after a Reload can equal the reference.

use crate::attach::{self, Regs};
use crate::layout as l;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use dcn314_regs::{self as regs, bar5};

/// Which condition a register is read under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Global,
    /// Read only if pipe n's power domains are on.
    Pipe(u8),
    /// OTG n and its DP DTO: narrow reads only OTG0 and an OTG a powered pipe drives; wide reads all.
    Otg(u8),
    /// DIG n's front/back end: narrow reads only DIG0; wide reads all.
    Dig(u8),
    /// A DP encoder's stream registers: read only if DIG n is read and its back end is enabled.
    DpStream(u8),
}

impl Group {
    /// Read only in a wide snapshot (or, for an OTG, when a powered pipe drives it).
    pub fn wide_only(self) -> bool {
        matches!(self, Group::Otg(n) | Group::Dig(n) | Group::DpStream(n) if n != 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub byte: u32,
    pub group: Group,
}

/// The BAR5 registers outside the DCN header that the snapshot reads (names from `dcn314_regs::bar5`).
const EXTRA: &[(u32, &str)] = &[
    (bar5::RCC_CONFIG_MEMSIZE, "RCC_CONFIG_MEMSIZE"),
    (bar5::RCC_CONFIG_APER_SIZE, "RCC_CONFIG_APER_SIZE"),
    (bar5::MP0_C2PMSG_33, "MP0_C2PMSG_33"),
    (bar5::MP0_C2PMSG_81, "MP0_C2PMSG_81"),
    (bar5::DC_SMU_RESP, "MP1_C2PMSG_91"),
];

/// A register name: the DCN header's, or one of the BAR5 extras.
pub fn name(byte: u32) -> &'static str {
    if let Some(&(_, n)) = EXTRA.iter().find(|&&(b, _)| b == byte) {
        return n;
    }
    regs::at_byte(byte).next().map_or("?", |r| r.name)
}

/// The pipe a header register belongs to (HUBP/HUBPREQ/HUBPRET/CURSOR0_/DPP/CNVC/DSCL/CM instance n).
fn pipe_of(name: &str) -> Option<u8> {
    const PREFIXES: &[&str] =
        &["HUBPREQ", "HUBPRET", "HUBP", "CURSOR0_", "DPP_TOP", "CNVC_CFG", "CNVC_CUR", "DSCL", "CM"];
    let rest = PREFIXES.iter().find_map(|p| name.strip_prefix(p))?;
    let d = rest.bytes().next()?;
    (d.is_ascii_digit() && rest.as_bytes().get(1) == Some(&b'_')).then_some(d - b'0')
}

/// Whether `byte` may be read at all by the snapshot: never the NBIO aliases, the AZ pair, past the display window.
pub fn readable(byte: u32) -> bool {
    !bar5::NBIO_RSMU_ALIAS.contains(&byte)
        && byte != crate::dcn::AZ_ENDPOINT_INDEX
        && byte != crate::dcn::AZ_ENDPOINT_DATA
        && byte % 4 == 0
        && byte < regs::DISPLAY_WINDOW_END
}

/// The registers, sorted by offset, each once.
pub fn registers() -> Vec<Entry> {
    let mut m: BTreeMap<u32, Group> = BTreeMap::new();
    let mut add = |byte: u32, group: Group| {
        m.entry(byte).or_insert(group);
    };
    for &(b, _) in regs::P1_SNAPSHOT {
        add(b, pipe_of(name(b)).map_or(Group::Global, Group::Pipe));
    }
    for &(b, _) in EXTRA {
        add(b, Group::Global);
    }
    for p in 0..l::PIPES {
        for r in [
            l::HUBP_CNTL,
            l::HUBP_VMID,
            l::HUBP_ADDR,
            l::HUBP_ADDR_HI,
            l::HUBP_SURF,
            l::HUBP_FLIP,
            l::HUBP_FLIP2,
            l::HUBP_INUSE,
            l::HUBP_INUSE_HI,
            l::HUBP_VIEWPORT,
            l::HUBP_SURFACE_CONFIG,
            l::HUBP_PITCH,
        ] {
            add(l::hubp(p, r), Group::Pipe(p as u8));
        }
    }
    for k in 0..l::OTGS {
        for r in [l::OTG_CONTROL, l::OTG_LOCK, l::OTG_H_TOTAL, l::OTG_V_TOTAL, l::OTG_H_BLANK, l::OTG_V_BLANK] {
            add(l::otg(k, r), Group::Otg(k as u8));
        }
        add(l::dto_cntl(k), Group::Otg(k as u8));
        add(l::dto_phase(k), Group::Otg(k as u8));
        add(l::dto_modulo(k), Group::Otg(k as u8));
    }
    for d in 0..4 {
        add(l::pg_status(d), Group::Global);
    }
    add(l::DMCUB_CNTL, Group::Global);
    for w in l::DMCUB_WINDOWS {
        for b in [Some(w.top), w.base, Some(w.offset), Some(w.offset_high)].into_iter().flatten() {
            add(b, Group::Global);
        }
    }
    for d in 0..l::DIGS {
        for r in [l::DIG_FE_CNTL, l::DIG_BE_CNTL, l::DIG_BE_EN_CNTL] {
            add(l::dig(d, r), Group::Dig(d as u8));
        }
        add(l::dp(d, l::DP_LINK_CNTL), Group::DpStream(d as u8));
        add(l::dp(d, l::DP_VID_STREAM_CNTL), Group::DpStream(d as u8));
    }
    m.into_iter().filter(|&(b, _)| readable(b)).map(|(byte, group)| Entry { byte, group }).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub regs: BTreeMap<u32, u32>,
    /// What was not read, and why (a gated pipe, an instance outside the narrow scope, a refused read).
    pub skipped: Vec<String>,
    /// Whether the other OTGs/DTOs/DIGs/DP encoders were read too (the scope; see the module comment).
    pub wide: bool,
}

/// Whether instance `n` of `group` is in the scope: always in a wide snapshot; narrow: instance 0, and an OTG a
/// powered pipe drives (`driven`, a bit mask).
fn in_scope(group: Group, wide: bool, driven: u32) -> bool {
    match group {
        Group::Otg(k) => wide || k == 0 || driven & (1 << k) != 0,
        g => wide || !g.wide_only(),
    }
}

/// Reads every register of the scope once (pipes only when powered, DP stream registers only when their DIG's
/// back end is on).
pub fn take(r: &impl Regs, wide: bool) -> Snapshot {
    let mut s = Snapshot { wide, ..Snapshot::default() };
    let mut pipe_on: [Option<bool>; l::PIPES as usize] = [None; l::PIPES as usize];
    let driven = if wide { 0 } else { attach::driven_otgs(r) };
    let mut out_of_scope: BTreeMap<(u8, u8), ()> = BTreeMap::new();
    for e in registers() {
        if !in_scope(e.group, wide, driven) {
            match e.group {
                Group::Otg(n) => out_of_scope.insert((0, n), ()),
                Group::Dig(n) | Group::DpStream(n) => out_of_scope.insert((1, n), ()),
                _ => None,
            };
            continue;
        }
        let ok = match e.group {
            Group::Global | Group::Otg(_) | Group::Dig(_) => true,
            Group::Pipe(p) => {
                let slot = &mut pipe_on[p as usize % l::PIPES as usize];
                *slot.get_or_insert_with(|| {
                    let on = attach::powered(r, u32::from(p)).unwrap_or(false);
                    if !on {
                        s.skipped.push(format!("pipe{p}: power domain gated"));
                    }
                    on
                })
            }
            Group::DpStream(d) => {
                let en = r.read(l::dig(u32::from(d), l::DIG_BE_EN_CNTL)).unwrap_or(0);
                en & 1 != 0
            }
        };
        if !ok {
            continue;
        }
        match r.read(e.byte) {
            Some(v) => {
                s.regs.insert(e.byte, v);
            }
            None => s.skipped.push(format!("{:05X} {}: read refused", e.byte, name(e.byte))),
        }
    }
    for &(kind, n) in out_of_scope.keys() {
        s.skipped.push(if kind == 0 {
            format!("OTG{n}: no powered pipe drives it, not read (wide reads it)")
        } else {
            format!("DIG{n}: not read (wide reads it)")
        });
    }
    s
}

/// The snapshot as one compact JSON object (keys: 5-digit hex BAR5 offsets, values 8-digit hex).
pub fn to_json(s: &Snapshot, function: &str, attach: &str) -> String {
    let mut o = String::from("{\"format\":\"dcn-snapshot/1\",\"function\":");
    mcp::json::escape_into(function, &mut o);
    o += ",\"attach\":";
    mcp::json::escape_into(attach, &mut o);
    o += &format!(",\"wide\":{},\"read\":{},\"skipped\":[", s.wide, s.regs.len());
    for (i, k) in s.skipped.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        mcp::json::escape_into(k, &mut o);
    }
    o += "],\"regs\":{";
    for (i, (b, v)) in s.regs.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o += &format!("\"{b:05X}\":\"{v:08X}\"");
    }
    o += "}}";
    o
}

/// What a reference covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// V1's G-1 lines: 179 registers; a register only read now is outside its scope, not a difference.
    G1,
    /// A `dcn_snapshot` answer, narrow or wide (an answer without "wide" is from before the narrow scope: wide).
    Snapshot { wide: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub regs: BTreeMap<u32, u32>,
    pub scope: Scope,
}

/// A reference: a `dcn_snapshot` JSON answer, or G-1 lines `0xOFFSET 0xVALUE ...` (p1-decoded.txt).
pub fn parse_reference(text: &str) -> Result<Reference, String> {
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).ok();
    let t = text.trim_start();
    if t.starts_with('{') {
        let j = mcp::Json::parse(t).map_err(|e| format!("reference is not JSON: {e:?}"))?;
        let Some(mcp::Json::Obj(members)) = j.get("regs") else {
            return Err(String::from("reference JSON has no \"regs\" object"));
        };
        let mut m = BTreeMap::new();
        for (k, v) in members {
            let (Some(b), Some(x)) = (hex(k), v.as_str().and_then(hex)) else {
                return Err(format!("reference entry {k:?} is not hex offset -> hex value"));
            };
            m.insert(b, x);
        }
        let wide = !matches!(j.get("wide"), Some(mcp::Json::Bool(false)));
        return Ok(Reference { regs: m, scope: Scope::Snapshot { wide } });
    }
    let mut m = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let mut w = line.split_whitespace();
        let (Some(a), Some(b)) = (w.next(), w.next()) else {
            continue;
        };
        match (hex(a), hex(b)) {
            (Some(b0), Some(v)) => {
                m.insert(b0, v);
            }
            _ => return Err(format!("reference line {}: not `0xOFFSET 0xVALUE`", n + 1)),
        }
    }
    if m.is_empty() {
        return Err(String::from("reference holds no registers"));
    }
    Ok(Reference { regs: m, scope: Scope::G1 })
}

/// How a register is compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Every bit except `ignore`.
    Bits { ignore: u32 },
    /// Only zero against non-zero (the PSP heartbeat: "test != 0, never compare").
    NonZero,
}

/// Field names that change by themselves: pending updates, sticky event/underflow/error flags, interrupt and lock
/// status, outstanding requests, memory light-sleep states (spec/gpu.md §2.2, G/doku/90-fallen.md X30). Power
/// and link status are compared (stable at rest; a change there is real). Checked field by field in
/// `tests::snap::volatile_fields_are_exactly`.
fn volatile_field(f: &str) -> bool {
    const STATUS: &[&str] = &["UNDERFLOW", "TIMEOUT", "ERR", "LOCK", "PWAIT", "INT_STATUS"];
    f.ends_with("PENDING")
        || f.contains("OCCURRED")
        || f.contains("NO_OUTSTANDING")
        || f.contains("MEM_PWR_STATE")
        || (f.ends_with("_STATUS") && STATUS.iter().any(|p| f.contains(p)))
}

pub fn rule(byte: u32) -> Rule {
    if byte == bar5::MP0_C2PMSG_81 {
        return Rule::NonZero;
    }
    let ignore =
        regs::at_byte(byte).flat_map(|r| r.fields.iter()).filter(|f| volatile_field(f.name)).fold(0, |m, f| m | f.mask);
    Rule::Bits { ignore }
}

/// Registers whose value follows the GOP mode (surface size, scaler, request and prefetch timing): different
/// against G-1 (1024x768 surface) once the GOP runs the native mode; "mode-independent" is everything else.
pub fn mode_dependent(name: &str) -> bool {
    const PARTS: &[&str] = &[
        "VIEWPORT",
        "PITCH",
        "SCALE_RATIO",
        "FILTER_INIT",
        "RECOUT",
        "MPC_SIZE",
        "AUTOCAL",
        "SCL_MODE",
        "SCL_TAP",
        "DSCL_CONTROL",
        "LB_",
        "REQ_SIZE",
        "BLANK_OFFSET",
        "DST_",
        "PREFETCH",
        "VBLANK_PARAMETERS",
        "FLIP_PARAMETERS",
        "NOM_PARAMETERS",
        "PER_LINE",
        "REF_FREQ",
        "DELTA_DRQ",
        "TTU",
        "SURFACE_ADDRESS",
        "EARLIEST_INUSE",
        "SURFACE_CONFIG",
        "TILING",
        "DET",
        "COMPBUF",
        "EXPANSION",
        "PIXEL_FORMAT",
        "FORMAT_CONTROL",
    ];
    PARTS.iter().any(|p| name.contains(p))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub byte: u32,
    pub reference: u32,
    pub now: u32,
    pub mode: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub compared: usize,
    pub differ: Vec<Diff>,
    pub only_reference: Vec<u32>,
    pub only_now: Vec<u32>,
    /// On one side only because the other side's scope does not cover them (a G-1 reference; a narrow against a
    /// wide snapshot): shown, not a difference.
    pub out_of_scope: Vec<u32>,
}

impl Report {
    /// Equal = the same registers read within both scopes, no difference outside the volatile bits. A pipe that
    /// powered up or down, or a DP back end switched on or off, is a difference (a real change of state).
    pub fn equal(&self) -> bool {
        self.differ.is_empty() && self.only_reference.is_empty() && self.only_now.is_empty()
    }
}

/// `now` against `reference`; `now_wide`: the scope `now` was taken with.
pub fn compare(reference: &Reference, now: &BTreeMap<u32, u32>, now_wide: bool) -> Report {
    let groups: BTreeMap<u32, Group> = registers().into_iter().map(|e| (e.byte, e.group)).collect();
    let wide_only = |b: &u32| groups.get(b).is_some_and(|g| g.wide_only());
    // Which side's extra registers are outside the other's scope.
    let (ref_extra_ok, now_extra_ok): (&dyn Fn(&u32) -> bool, &dyn Fn(&u32) -> bool) = match reference.scope {
        Scope::G1 => (&|_| false, &|_| true),
        Scope::Snapshot { wide } if wide && !now_wide => (&wide_only, &|_| false),
        Scope::Snapshot { wide } if !wide && now_wide => (&|_| false, &wide_only),
        Scope::Snapshot { .. } => (&|_| false, &|_| false),
    };
    let mut r = Report::default();
    for (&b, &x) in &reference.regs {
        let Some(&y) = now.get(&b) else {
            if ref_extra_ok(&b) {
                r.out_of_scope.push(b);
            } else {
                r.only_reference.push(b);
            }
            continue;
        };
        r.compared += 1;
        let same = match rule(b) {
            Rule::NonZero => (x == 0) == (y == 0),
            Rule::Bits { ignore } => x & !ignore == y & !ignore,
        };
        if !same {
            r.differ.push(Diff { byte: b, reference: x, now: y, mode: mode_dependent(name(b)) });
        }
    }
    for b in now.keys().filter(|b| !reference.regs.contains_key(b)) {
        if now_extra_ok(b) {
            r.out_of_scope.push(*b);
        } else {
            r.only_now.push(*b);
        }
    }
    r.out_of_scope.sort_unstable();
    r
}

/// The diff as text: a first line `equal` or `differ ...`, then one line per difference with the changed fields.
pub fn render(r: &Report) -> String {
    let modes = r.differ.iter().filter(|d| d.mode).count();
    let mut s = if r.equal() {
        format!("equal: {} registers compared (volatile status bits ignored)", r.compared)
            + &if r.out_of_scope.is_empty() {
                String::from("\n")
            } else {
                format!(", {} outside the other side's scope not compared\n", r.out_of_scope.len())
            }
    } else {
        format!(
            "differ: {} of {} registers ({} mode-dependent, {} mode-independent), {} only in the reference, {} only now, {} outside the other side's scope\n",
            r.differ.len(),
            r.compared,
            modes,
            r.differ.len() - modes,
            r.only_reference.len(),
            r.only_now.len(),
            r.out_of_scope.len()
        )
    };
    for d in &r.differ {
        s += &format!(
            "{:05X} {} {} ref {:#010x} now {:#010x}",
            d.byte,
            name(d.byte),
            if d.mode { "mode" } else { "fixed" },
            d.reference,
            d.now
        );
        if let Some(ri) = regs::at_byte(d.byte).next() {
            for f in ri.fields {
                let (a, b) = (f.get(d.reference), f.get(d.now));
                if a != b {
                    s += &format!(" {}={:#x}->{:#x}", f.name, a, b);
                }
            }
        }
        s.push('\n');
    }
    for (what, list) in [("only in the reference", &r.only_reference), ("only now", &r.only_now)] {
        if !list.is_empty() {
            s += what;
            s.push(':');
            for b in list.iter() {
                s += &format!(" {b:05X}");
            }
            s.push('\n');
        }
    }
    s
}
