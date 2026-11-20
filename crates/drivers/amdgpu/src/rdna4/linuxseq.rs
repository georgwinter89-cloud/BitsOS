//! GX-10: Linux's own start of the card, section by section, as the thing a bring-up stage is compared with
//! (plan/spec/gpu-rdna4.md section 6). The fixtures in `linuxseq/` are cut from the register trace of Linux 7.3.1's
//! cold start on this very card (plan/hw/gaming-pc-gpu-2026-11-18/init.txt.zst) by host/gaming-pc/gx-trace-seq.py;
//! `tests/linuxseq_fresh.rs` fails when they are stale. A stage's accesses (the bring-up owner's trail,
//! `gpucore::trail`) equal a section when every write, read, wait and sleep is the same in the same order: the
//! register, the value, a wait's condition and the reads after which it gives up, a sleep's length. The only way to
//! differ is a declared rule per register name (`rules`).
//!
//! - this file: a fixture's lines;
//! - `fold`: a trail as Linux's trace would show it (the HDP flush pair, the index pair);
//! - `rules`: the declared differences and their own checks;
//! - `compare`: the comparison, the first difference (`words`: in plain words).

pub mod compare;
pub mod diff;
pub mod fold;
pub mod rules;
pub mod words;

use crate::gpucore::psp::Cond;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The sections a stage is compared with (the kernel carries only these). `sw-init` is here for its TLB flushes
/// after a GART bind (`bind_flush`); the rest of it belongs to later waves.
pub const SECTIONS: [(&str, &str); 13] = [
    ("nbif", include_str!("linuxseq/nbif.txt")),
    ("mmhub", include_str!("linuxseq/mmhub.txt")),
    ("sw-init", include_str!("linuxseq/sw-init.txt")),
    ("ih", include_str!("linuxseq/ih.txt")),
    ("psp-boot", include_str!("linuxseq/psp-boot.txt")),
    ("psp-ring", include_str!("linuxseq/psp-ring.txt")),
    ("smu", include_str!("linuxseq/smu.txt")),
    ("gc-boot", include_str!("linuxseq/gc-boot.txt")),
    ("gc-hub", include_str!("linuxseq/gc-hub.txt")),
    ("gc-cp", include_str!("linuxseq/gc-cp.txt")),
    ("mes", include_str!("linuxseq/mes.txt")),
    ("gc-ring", include_str!("linuxseq/gc-ring.txt")),
    ("sdma", include_str!("linuxseq/sdma.txt")),
];

/// What Linux did on one fixture line. Registers are dwords of BAR5, as Linux's trace counts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What<'a> {
    Write { name: &'a str, dword: u32, value: u32 },
    /// `times` reads in a row with this answer.
    Read { name: &'a str, dword: u32, value: u32, times: u32 },
    /// `reads` reads until `cond` held; Linux gives up after `max` reads 1 us apart; `seen` is every answer with
    /// its count (`0x..xN,..`).
    Wait { name: &'a str, dword: u32, cond: Cond, max: u32, last: u32, reads: u32, us: u64, seen: &'a str },
    /// Linux sleeps `us` after the write before this line (the source says so; the trace shows `gap` us of silence).
    Sleep { us: u64, gap: u64 },
    /// Linux's HDP flush (amdgpu_hdp.c:51-60): 0 into `flush`, then a read of the memory size register.
    Hdp { flush: u32, mem_name: &'a str, mem: u32, value: u32 },
    /// VRAM read through MM_INDEX/MM_DATA (only the discovery section has one).
    Mem { offset: u64, dwords: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line<'a> {
    /// The line of the fixture file, from 1.
    pub at: u32,
    /// The line of init.txt it comes from.
    pub init: u32,
    pub what: What<'a>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section<'a> {
    pub name: &'a str,
    /// The header line: the trace, its sha256, the generator.
    pub head: &'a str,
    pub lines: Vec<Line<'a>>,
}

fn hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

fn hex32(s: &str) -> Option<u32> {
    hex(s).and_then(|v| u32::try_from(v).ok())
}

fn field<'a>(t: &'a str, key: &str) -> Option<&'a str> {
    t.strip_prefix(key)?.strip_prefix('=')
}

fn line<'a>(t: &[&'a str]) -> Option<What<'a>> {
    Some(match *t {
        ["W", name, dword, value] => What::Write { name, dword: hex32(dword)?, value: hex32(value)? },
        ["R", name, dword, value] => What::Read { name, dword: hex32(dword)?, value: hex32(value)?, times: 1 },
        ["R", name, dword, value, times] => What::Read { name, dword: hex32(dword)?, value: hex32(value)?, times: times.strip_prefix('x')?.parse().ok().filter(|n| *n > 1)? },
        ["WAIT", name, dword, "changed", from, ref rest @ ..] => wait(name, dword, Cond::Changed { from: hex32(field(from, "from")?)? }, rest)?,
        ["WAIT", name, dword, mask, is, ref rest @ ..] => wait(name, dword, Cond::Masked { mask: hex32(field(mask, "mask")?)?, value: hex32(field(is, "is")?)? }, rest)?,
        ["SLEEP", us, gap] => What::Sleep { us: field(us, "us")?.parse().ok()?, gap: field(gap, "gap")?.parse().ok()? },
        ["HDP", flush, mem_name, mem, value] => What::Hdp { flush: hex32(flush)?, mem_name, mem: hex32(mem)?, value: hex32(value)? },
        ["MEM", offset, dwords] => What::Mem { offset: hex(offset)?, dwords: dwords.parse().ok()? },
        _ => return None,
    })
}

fn wait<'a>(name: &'a str, dword: &str, cond: Cond, rest: &[&'a str]) -> Option<What<'a>> {
    let [max, last, reads, us, seen] = *rest else { return None };
    let (max, last) = (field(max, "max")?.parse().ok()?, hex32(field(last, "last")?)?);
    Some(What::Wait { name, dword: hex32(dword)?, cond, max, last, reads: field(reads, "reads")?.parse().ok()?, us: field(us, "us")?.parse().ok()?, seen: field(seen, "seen")? })
}

/// A fixture's text as lines. A line that is not one of the generator's forms is an error naming it.
pub fn parse<'a>(name: &'a str, text: &'a str) -> Result<Section<'a>, String> {
    let mut rows = text.lines().enumerate().map(|(i, l)| (i as u32 + 1, l));
    let head = match rows.next() {
        Some((_, l)) if l.strip_prefix("# linuxseq ").and_then(|r| r.strip_prefix(name)).is_some_and(|r| r.starts_with(": ")) => l,
        other => return Err(format!("{name}.txt: the first line is not this section's header: {:?}", other.map(|o| o.1))),
    };
    let mut lines = Vec::new();
    for (at, l) in rows {
        if l.starts_with('#') {
            continue;
        }
        let t: Vec<&str> = l.split(' ').collect();
        let parsed = t.split_first().and_then(|(init, rest)| Some((init.parse::<u32>().ok()?, line(rest)?)));
        let Some((init, what)) = parsed else { return Err(format!("{name}.txt line {at} is no fixture line: {l}")) };
        // A wait's answers must fit its own condition, counts and bound, and a sleep the silence the trace shows: a
        // fixture edited by hand shows here.
        match what {
            What::Wait { cond, max, last, reads, seen, .. } if reads > max || !wait_fits(cond, last, reads, seen) => {
                return Err(format!("{name}.txt line {at}: the wait's answers do not fit its condition or its bound: {l}"));
            }
            What::Sleep { us, gap } if gap < us || !matches!(lines.last(), Some(Line { what: What::Write { .. }, .. })) => {
                return Err(format!("{name}.txt line {at}: no sleep after a write that the trace is silent for: {l}"));
            }
            _ => {}
        }
        lines.push(Line { at, init, what });
    }
    if lines.is_empty() {
        return Err(format!("{name}.txt holds no line"));
    }
    Ok(Section { name, head, lines })
}

/// The answers of a wait, in order: (value, how often in a row).
pub fn seen(s: &str) -> impl Iterator<Item = Option<(u32, u32)>> + '_ {
    s.split(',').map(|p| {
        let (v, n) = p.rsplit_once('x')?;
        Some((hex32(v)?, n.parse().ok()?))
    })
}

fn wait_fits(cond: Cond, last: u32, reads: u32, s: &str) -> bool {
    let mut total = 0u32;
    let mut end = None;
    for p in seen(s) {
        let Some((v, n)) = p else { return false };
        // Only the last answer meets the condition, once.
        if end.is_some() || n == 0 {
            return false;
        }
        if cond.met(v) {
            end = Some((v, n));
        }
        total = total.saturating_add(n);
    }
    end == Some((last, 1)) && total == reads
}

/// One of the kernel's sections, parsed.
pub fn section(name: &str) -> Result<Section<'static>, String> {
    let (n, text) = SECTIONS.iter().find(|(n, _)| *n == name).ok_or_else(|| format!("no section {name} in the kernel"))?;
    parse(n, text)
}

/// Linux's TLB flush after binding GART pages (`amdgpu_gart_invalidate_tlb`, amdgpu_gart.c:492-506, with
/// `gmc_v12_0_flush_gpu_tlb`, gmc_v12_0.c:305-330): the first of the 28 at the start of `sw-init`, from its two HDP
/// flushes to the read-back of MMVM_L2_BANK_SELECT_RESERVED_CID2 (gmc_v12_0.c:278-284).
pub fn bind_flush() -> Result<Vec<Line<'static>>, String> {
    let s = section("sw-init")?;
    let is = |l: &Line, write: bool| match l.what {
        What::Write { name, .. } => write && name == "MMVM_L2_BANK_SELECT_RESERVED_CID2",
        What::Read { name, times: 1, .. } => !write && name == "MMVM_L2_BANK_SELECT_RESERVED_CID2",
        _ => false,
    };
    let end = s.lines.windows(2).position(|w| is(&w[0], true) && is(&w[1], false)).ok_or("sw-init.txt holds no TLB flush")?;
    let unit = &s.lines[..end + 2];
    let shape: Vec<u8> = unit.iter().map(|l| match l.what { What::Hdp { .. } => b'H', What::Wait { .. } => b'T', What::Write { .. } => b'W', What::Read { .. } => b'R', _ => b'?' }).collect();
    if shape != b"HHTWTWRWR" {
        return Err(format!("sw-init.txt does not begin with Linux's TLB flush after a bind (its lines are {})", String::from_utf8_lossy(&shape)));
    }
    Ok(unit.to_vec())
}

/// Linux's TLB flush after binding GART pages once the graphics block's hub is on: `amdgpu_gart_invalidate_tlb`
/// (amdgpu_gart.c:492-506) walks every hub of `vmhubs_mask`, GFXHUB (bit 0) before MMHUB (bit 8,
/// gmc_v12_0.c:834-835), each with `gmc_v12_0_flush_gpu_tlb`. The trace holds no such flush by registers (Linux
/// binds nothing between GFXHUB's start and MES, and flushes through MES after it, gmc_v12_0.c:315-327), so the
/// unit is put together from trace lines and Linux's order alone: the bind flush's own HDP flush, GFXHUB's flush
/// as `gc-hub` ends with it (HDP flush, the request, the acknowledge's wait; no semaphore, :189-194), then
/// MMHUB's eight lines of `bind_flush`. It holds only until MES runs: from then on Linux's flush is a MES
/// command, which is what its first bind after GFXHUB's start (section sdma) uses, so the wave that builds MES
/// replaces this unit.
pub fn bind_flush_both() -> Result<Vec<Line<'static>>, String> {
    let mm = bind_flush()?;
    let gc = section("gc-hub")?;
    let tail = gc.lines.len().checked_sub(3).map(|at| &gc.lines[at..]).ok_or("gc-hub.txt is shorter than a flush")?;
    let is_flush = matches!(
        (tail[0].what, tail[1].what, tail[2].what),
        (What::Hdp { .. }, What::Write { name: "GCVM_INVALIDATE_ENG17_REQ", .. }, What::Wait { name: "GCVM_INVALIDATE_ENG17_ACK", .. })
    );
    if !is_flush {
        return Err(String::from("gc-hub.txt does not end with GFXHUB's TLB flush (HDP flush, request, acknowledge)"));
    }
    Ok(mm[..1].iter().chain(tail).chain(&mm[1..]).copied().collect())
}

/// Linux's TLB flush after binding GART pages once MES runs (wave 44): `amdgpu_gart_invalidate_tlb`'s HDP flush,
/// then per hub (GFXHUB, MMHUB) `gmc_v12_0_flush_gpu_tlb`'s HDP flush and the hub's invalidation as a MES
/// command, which is no register access (gmc_v12_0.c:305-330). On the bus: three HDP flushes and nothing else,
/// as the trace-load shows after every bind of the load (plan/hw/gaming-pc-gpu-2026-11-18/trace-load.txt.zst,
/// lines 106997-107002). The init trace has no such bind, so the unit is the HDP line of section `mes` three
/// times.
pub fn bind_flush_mes() -> Result<Vec<Line<'static>>, String> {
    let s = section("mes")?;
    let hdp = s.lines.iter().find(|l| matches!(l.what, What::Hdp { .. })).copied().ok_or("mes.txt holds no HDP flush")?;
    Ok(alloc::vec![hdp; 3])
}

/// Linux's name for a dword, from the kernel's sections (a stage's access that Linux made nowhere has none).
pub fn name_of(dword: u32) -> Option<&'static str> {
    SECTIONS.iter().filter_map(|(n, t)| parse(n, t).ok()).find_map(|s| {
        s.lines.iter().find_map(|l| match l.what {
            What::Write { name, dword: d, .. } | What::Read { name, dword: d, .. } | What::Wait { name, dword: d, .. } if d == dword => Some(name),
            What::Hdp { mem_name, mem, .. } if mem == dword => Some(mem_name),
            _ => None,
        })
    })
}

/// Linux's HDP flush pair (the remapped flush register, the memory size register) as its trace shows it.
pub fn hdp_pair() -> Option<(u32, u32)> {
    SECTIONS.iter().filter_map(|(n, t)| parse(n, t).ok()).find_map(|s| {
        s.lines.iter().find_map(|l| match l.what {
            What::Hdp { flush, mem, .. } => Some((flush, mem)),
            _ => None,
        })
    })
}
