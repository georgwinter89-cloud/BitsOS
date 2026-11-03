//! regtrace (GX-7c): names, filters and diffs Linux's register trace of an amdgpu load (the ftrace capture of
//! plan/hw/gx-780m-linux-trace-*: amdgpu_device_rreg/wreg, amdgpu_dc_*, the kit's gx_ind_*/gx_port_* probes).
//!
//!   regtrace names [--bases F] [--tables D] [--reads] [--dc] [--match NAME]... [--from US] [--to US] TRACE
//!       One line per access: `line time_us R|W|SR|SW|PR|PW reg NAME = value [dc]`. Default: writes, without
//!       Display Core's own (each DC access is logged twice; --dc keeps them).
//!   regtrace phases [--bases F] [--tables D] TRACE
//!       Linux's writes per bring-up phase (PSP, SMU, MES, IMU/RLC, SDMA: regdiff's PHASES), named, in order.
//!   regtrace smu TRACE
//!       The PMFW messages on the driver's (66/82/90) and the display's (67/83/91) mailbox: id, argument,
//!       answer, the argument read back, latency and polls.
//!   regtrace diff --ours F [--bases F] [--tables D] TRACE
//!       Our write sequence (`F`: lines `<byte> <value>` in hex, an optional leading `W`; `#` comments) against
//!       Linux's, phase by phase: missing, extra and reordered writes. Exit 1 when a difference is unexplained.
//!   regtrace watch --ours F [--bases F] [--tables D]
//!       The scanout watcher over our sequence: every write to a DCN register, an MMHUB field the display uses
//!       or the display's PMFW mailbox. Exit 1 when there is one.
//!
//! TRACE may be zstd-compressed (`.zst`: read through `zstd -dc`). --bases defaults to the Mini-PC's discovery
//! (crates/drivers/amdgpu/tests/data/minipc-780m-ip.txt), --tables to host/gen/dcn314/tables.

#![forbid(unsafe_code)]

extern crate alloc;

#[allow(dead_code)]
#[path = "../amdnames.rs"]
mod amdnames;
#[allow(dead_code)]
#[path = "../../../../../crates/drivers/amdgpu/src/gpucore/regdiff.rs"]
mod regdiff;
#[allow(dead_code)]
#[path = "../../../../../crates/drivers/amdgpu/src/gpucore/regtrace.rs"]
mod regtrace;

use amdnames::{Ip, bases_from_lines, names};
use regtrace::{Kind, Trace};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

type ByteNames = BTreeMap<u32, Vec<String>>;

fn usage() -> String {
    String::from(
        "usage: regtrace names|phases|smu|diff|watch [--bases FILE] [--tables DIR] [--reads] [--dc] [--match NAME] [--from US] [--to US] [--ours FILE] [TRACE]",
    )
}

#[derive(Default)]
struct Opts {
    bases: Option<PathBuf>,
    tables: Option<PathBuf>,
    reads: bool,
    dc: bool,
    matches: Vec<String>,
    from: Option<u64>,
    to: Option<u64>,
    ours: Option<PathBuf>,
    trace: Option<PathBuf>,
}

fn opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--bases" => o.bases = Some(val()?.into()),
            "--tables" => o.tables = Some(val()?.into()),
            "--reads" => o.reads = true,
            "--dc" => o.dc = true,
            "--match" => o.matches.push(val()?),
            "--from" => o.from = Some(val()?.parse().map_err(|_| "--from: microseconds")?),
            "--to" => o.to = Some(val()?.parse().map_err(|_| "--to: microseconds")?),
            "--ours" => o.ours = Some(val()?.into()),
            _ if o.trace.is_none() && !a.starts_with("--") => o.trace = Some(a.into()),
            _ => return Err(format!("unknown argument {a}\n{}", usage())),
        }
    }
    Ok(o)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The trace text; `.zst` through `zstd -dc` (no zstd crate in the tree).
fn read_trace(p: &Path) -> Result<String, String> {
    if p.extension().is_some_and(|e| e == "zst") {
        let out = std::process::Command::new("zstd").arg("-dc").arg(p).output().map_err(|e| format!("zstd: {e}"))?;
        if !out.status.success() {
            return Err(format!("zstd -dc {}: {}", p.display(), String::from_utf8_lossy(&out.stderr).trim()));
        }
        return String::from_utf8(out.stdout).map_err(|e| format!("{}: {e}", p.display()));
    }
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn ips(o: &Opts) -> Result<Vec<Ip>, String> {
    let f = o.bases.clone().unwrap_or_else(|| root().join("crates/drivers/amdgpu/tests/data/minipc-780m-ip.txt"));
    bases_from_lines(&std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?)
}

/// BAR5 byte -> names from the tables `pick` lets through (rows filtered by `keep`).
fn byte_names(o: &Opts, pick: &dyn Fn(&str) -> bool, keep: &dyn Fn(&str) -> bool) -> Result<ByteNames, String> {
    let dir = o.tables.clone().unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tables"));
    let n = names(&ips(o)?, |t| {
        if !pick(t) {
            return Ok(String::new());
        }
        let p = dir.join(format!("{t}.tsv"));
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(text.lines().filter(|l| l.starts_with('#') || keep(l)).flat_map(|l| [l, "\n"]).collect())
    })?;
    Ok(n.by_dword.into_iter().filter_map(|(dw, v)| Some((dw.checked_mul(4)?, v))).collect())
}

fn all_names(o: &Opts) -> Result<ByteNames, String> {
    byte_names(o, &|_| true, &|_| true)
}

fn trace(o: &Opts) -> Result<Trace, String> {
    let p = o.trace.clone().ok_or_else(usage)?;
    let t = regtrace::parse(&read_trace(&p)?);
    eprintln!("regtrace: {} events, {} other lines, {} unreadable, {} DC events unpaired", t.events.len(), t.other, t.bad.len(), t.dc_unpaired);
    Ok(t)
}

fn label(n: &ByteNames, byte: u32) -> String {
    n.get(&byte).map_or_else(|| String::from("?"), |v| v.join("|"))
}

fn names_cmd(o: &Opts) -> Result<(), String> {
    let n = all_names(o)?;
    let t = trace(o)?;
    for e in &t.events {
        if (!o.reads && !e.kind.is_write()) || (e.dc && !o.dc) || o.from.is_some_and(|f| e.t_us < f) || o.to.is_some_and(|x| e.t_us > x) {
            continue;
        }
        let name = match e.byte() {
            Some(b) => label(&n, b),
            None => String::from(if matches!(e.kind, Kind::SmnRead | Kind::SmnWrite) { "smn" } else { "port" }),
        };
        if !o.matches.is_empty() && !o.matches.iter().any(|m| name.split('|').any(|x| x.starts_with(m.as_str()))) {
            continue;
        }
        println!("{} {} {} {:#07x} {name} = {:#010x}{}", e.line, e.t_us, e.kind.letter(), e.reg, e.value, if e.dc { " dc" } else { "" });
    }
    Ok(())
}

fn phases_cmd(o: &Opts) -> Result<(), String> {
    let n = all_names(o)?;
    let t = trace(o)?;
    let w = t.writes(false);
    for ph in regdiff::PHASES {
        let sel = regdiff::select(ph, &w, &n);
        println!("phase {}: {} writes", ph.name, sel.len());
        for (i, b, v) in sel {
            println!("  #{i} {b:#07x} {} = {v:#010x}", label(&n, b));
        }
    }
    Ok(())
}

/// One message on a mailbox: (time, id, argument, answer, time of the answer, argument read after, polls).
struct Msg {
    t: u64,
    id: u32,
    arg: Option<u32>,
    resp: Option<(u32, u64)>,
    out: Option<u32>,
    polls: u32,
    dc: bool,
}

fn smu_messages(t: &Trace, msg: u32, arg: u32, resp: u32) -> Vec<Msg> {
    let mut v: Vec<Msg> = Vec::new();
    let mut last_arg = None;
    for e in &t.events {
        match (e.kind, e.reg) {
            (Kind::Write, r) if r == arg => last_arg = Some(e.value),
            (Kind::Write, r) if r == msg => {
                v.push(Msg { t: e.t_us, id: e.value, arg: last_arg.take(), resp: None, out: None, polls: 0, dc: e.dc });
            }
            (Kind::Read, r) if r == resp => {
                if let Some(m) = v.last_mut() {
                    m.polls += 1;
                    if m.resp.is_none() && e.value != 0 {
                        m.resp = Some((e.value, e.t_us));
                    }
                }
            }
            (Kind::Read, r) if r == arg => {
                if let Some(m) = v.last_mut().filter(|m| m.resp.is_some() && m.out.is_none()) {
                    m.out = Some(e.value);
                }
            }
            _ => {}
        }
    }
    v
}

fn smu_cmd(o: &Opts) -> Result<(), String> {
    let t = trace(o)?;
    let base = ips(o)?.iter().find(|i| i.hw_id == amdnames::MP1 && i.inst == 0).and_then(|i| i.bases.get(1).copied()).ok_or("no MP1 segment 1")?;
    for (who, d) in [("driver", 0u32), ("display", 1)] {
        for m in smu_messages(&t, base + 0x282 + d, base + 0x292 + d, base + 0x29a + d) {
            let opt = |x: Option<u32>| x.map_or(String::from("-"), |v| format!("{v:#x}"));
            let (resp, lat) = m.resp.map_or((String::from("none"), String::from("-")), |(r, at)| (format!("{r:#x}"), format!("{}", at.saturating_sub(m.t))));
            println!("{who} {} msg {:#x} arg {} -> resp {resp} out {} after {lat} us, {} polls{}", m.t, m.id, opt(m.arg), opt(m.out), m.polls, if m.dc { " dc" } else { "" });
        }
    }
    Ok(())
}

/// `<byte> <value>` lines (hex), an optional leading `W`.
fn read_ours(text: &str) -> Result<Vec<(u32, u32)>, String> {
    let mut v = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let l = l.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        let t: Vec<&str> = l.split_whitespace().filter(|x| *x != "W").collect();
        let h = |s: &str| amdnames::hex(s).ok().and_then(|x| u32::try_from(x).ok());
        match t[..] {
            [b, val, ..] if h(b).is_some() && h(val).is_some() => v.push((h(b).unwrap_or(0), h(val).unwrap_or(0))),
            _ => return Err(format!("line {}: {l:?} is not `<byte> <value>`", i + 1)),
        }
    }
    Ok(v)
}

fn ours(o: &Opts) -> Result<Vec<(u32, u32)>, String> {
    let p = o.ours.clone().ok_or("--ours FILE is needed")?;
    read_ours(&std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)
}

fn diff_cmd(o: &Opts) -> Result<bool, String> {
    let n = all_names(o)?;
    let mine = ours(o)?;
    let t = trace(o)?;
    let r = regdiff::diff(regdiff::PHASES, &t.writes(false), &mine, &n, &[]);
    print!("{}", regdiff::render(&r));
    Ok(r.iter().all(|p| p.failures().next().is_none()))
}

/// The watcher's names: DCN's segments 1.. (segment 0 aliases NBIO) and `regdiff::DISPLAY_NAMES`.
fn display_names(o: &Opts) -> Result<ByteNames, String> {
    let mut n = byte_names(o, &|t| t.starts_with("dcn_"), &|l| l.split('\t').nth(1).is_some_and(|s| s != "0"))?;
    for (b, v) in all_names(o)? {
        if v.iter().any(|x| regdiff::DISPLAY_NAMES.contains(&x.as_str())) {
            n.insert(b, v);
        }
    }
    Ok(n)
}

fn watch_cmd(o: &Opts) -> Result<bool, String> {
    let n = display_names(o)?;
    let mine = ours(o)?;
    let mut hits = 0;
    for (i, &(b, v)) in mine.iter().enumerate() {
        if let Some(x) = n.get(&b) {
            hits += 1;
            println!("#{i} {b:#07x} {} = {v:#010x}: a display register", x.join("|"));
        }
    }
    println!("watch: {} writes, {hits} to display registers", mine.len());
    Ok(hits == 0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let o = match opts(args.get(1..).unwrap_or(&[])) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("regtrace: {e}");
            return ExitCode::FAILURE;
        }
    };
    let r = match args.first().map(String::as_str) {
        Some("names") => names_cmd(&o).map(|()| true),
        Some("phases") => phases_cmd(&o).map(|()| true),
        Some("smu") => smu_cmd(&o).map(|()| true),
        Some("diff") => diff_cmd(&o),
        Some("watch") => watch_cmd(&o),
        _ => Err(usage()),
    };
    match r {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("regtrace: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINES: &str = "# tracer: nop\n\
        bash-1 [004] .....    31.765661: amdgpu_device_rreg: 0x1900, 0x00016091, 0x000000b1\n\
        bash-1 [004] d..1.    31.982983: amdgpu_device_wreg: 0x1900, 0x00016085, 0x00339000\n\
        bash-1 [003] .....    32.611915: amdgpu_device_wreg: 0x1900, 0x00003846, 0x00000056\n\
        bash-1 [003] .....    32.611916: amdgpu_dc_wreg: reg=0x00003846, value=0x00000056\n\
        bash-1 [003] .....    32.578298: gx_ind_rreg: (amdgpu_device_indirect_rreg+0x4/0x80 [amdgpu]) addr=0x3b10028\n\
        bash-1 [003] .....    32.578303: gx_ind_rreg_ret: (smu_v13_0_check_fw_status+0x3a/0x50 [amdgpu] <- amdgpu_device_indirect_rreg) val=0x1\n\
        bash-1 [003] .....    32.578305: amdgpu_device_wreg: 0x1900, 0x0001629a, 0x00000000\n\
        bash-1 [003] .....    32.578305: amdgpu_device_wreg: 0x1900, 0x00016292, 0x00000000\n\
        bash-1 [003] .....    32.578306: amdgpu_device_wreg: 0x1900, 0x00016282, 0x00000003\n\
        bash-1 [003] .....    32.578308: amdgpu_device_rreg: 0x1900, 0x0001629a, 0x00000000\n\
        bash-1 [003] .....    32.578426: amdgpu_device_rreg: 0x1900, 0x0001629a, 0x00000001\n\
        bash-1 [003] .....    32.578427: amdgpu_device_rreg: 0x1900, 0x00016292, 0x00000008\n";

    #[test]
    fn names_phases_and_messages_from_an_excerpt() {
        let o = Opts::default();
        let n = all_names(&o).unwrap();
        assert_eq!(label(&n, 0x16085 * 4), "MP0_SMN_C2PMSG_69");
        let t = regtrace::parse(LINES);
        assert_eq!((t.events.len(), t.other, t.bad.len()), (10, 1, 0));
        assert!(t.events[2].dc && !t.events[1].dc);
        let w = t.writes(false);
        assert_eq!(w.len(), 4, "the DC write is left out");
        let psp = regdiff::select(&regdiff::PHASES[0], &w, &n);
        assert_eq!(psp, vec![(0, 0x16085 * 4, 0x339000)]);
        let m = smu_messages(&t, 0x16282, 0x16292, 0x1629a);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].id, m[0].arg, m[0].resp, m[0].out, m[0].polls), (3, Some(0), Some((1, 32_578_426)), Some(8), 2));
    }

    #[test]
    fn the_watcher_names_dcn_mmhub_and_the_display_mailbox() {
        let n = display_names(&Opts::default()).unwrap();
        // HUBP0's surface address (DCN segment 2), MMHUB's FB location, the display's PMFW mailbox.
        let hit = |name: &str| n.values().any(|v| v.iter().any(|x| x == name));
        assert!(hit("HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS") && hit("MMMC_VM_FB_LOCATION_BASE") && hit("MP1_SMN_C2PMSG_67"));
        assert!(!hit("MP1_SMN_C2PMSG_66") && !hit("MP0_SMN_C2PMSG_64"), "our mailboxes are not display registers");
        assert!(!n.contains_key(&(0x14 * 4)), "DCN segment 0 aliases NBIO");
        assert_eq!(read_ours("# ours\nW 0x58214 0x1\n0x1a8ec 0x8000\n").unwrap(), vec![(0x58214, 1), (0x1a8ec, 0x8000)]);
        assert!(read_ours("nonsense\n").is_err());
    }
}
