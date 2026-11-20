//! GX-10: Linux's lines and a stage's accesses in plain words, for the first difference `compare` names and for a
//! report.

use super::fold::Did;
use super::{Section, What, name_of};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub fn answer(v: Option<u32>) -> String {
    v.map_or(String::from("nothing (the read failed)"), |v| format!("{v:#010x}"))
}

/// Linux's name for a dword: from this section first, then from any of the kernel's.
pub fn name(sec: &Section, dword: u32) -> String {
    let here = sec.lines.iter().find_map(|l| match l.what {
        What::Write { name, dword: d, .. } | What::Read { name, dword: d, .. } | What::Wait { name, dword: d, .. } if d == dword => Some(name),
        What::Hdp { mem_name, mem, .. } if mem == dword => Some(mem_name),
        _ => None,
    });
    match here.or_else(|| name_of(dword)) {
        Some(n) => String::from(n),
        None => format!("dword {dword:#x} (a register Linux touches in none of these sections)"),
    }
}

pub fn linux(w: &What) -> String {
    match *w {
        What::Write { name, value, .. } => format!("writes {value:#010x} to {name}"),
        What::Read { name, value, .. } => format!("reads {name} (the card answered {value:#010x})"),
        What::Wait { name, cond, last, reads, .. } => format!("waits on {name} until {cond} (the card answered {last:#010x} at read {reads})"),
        What::Hdp { flush, mem_name, .. } => format!("flushes the HDP (0 to dword {flush:#x}, then a read of {mem_name})"),
        What::Sleep { us, gap } => format!("sleeps {us} us (the trace is silent for {gap} us)"),
        What::Mem { offset, dwords } => format!("reads {dwords} dwords of VRAM at {offset:#x} through MM_INDEX"),
    }
}

pub fn did(sec: &Section, d: &Did) -> String {
    match *d {
        Did::Write { dword, value, reached: true } => format!("writes {value:#010x} to {}", name(sec, dword)),
        Did::Write { dword, value, reached: false } => format!("tries to write {value:#010x} to {}, which is refused", name(sec, dword)),
        Did::Read { dword, value } => format!("reads {} (its card answered {})", name(sec, dword), answer(value)),
        Did::Wait { dword, cond, reads, last, .. } => format!("waits on {} until {cond} (its card answered {} at read {reads})", name(sec, dword), answer(last)),
        Did::Hdp { flush, mem, .. } => format!("flushes the HDP (0 to dword {flush:#x}, then a read of {})", name(sec, mem)),
        Did::Sleep { us } => format!("sleeps {us} us"),
    }
}

/// A stage's accesses in the fixture's words, one line each (for a report or a failing test).
pub fn render(did: &[Did]) -> Vec<String> {
    let name = |d: u32| name_of(d).map_or(format!("{d:#x}"), String::from);
    did.iter()
        .map(|d| match *d {
            Did::Write { dword, value, reached } => format!("W {} {dword:#x} {value:#010x}{}", name(dword), if reached { "" } else { " refused" }),
            Did::Read { dword, value } => format!("R {} {dword:#x} {}", name(dword), answer(value)),
            Did::Wait { dword, cond, bound, reads, last } => format!("WAIT {} {dword:#x} until {cond}, at most {bound}: last={} reads={reads}", name(dword), answer(last)),
            Did::Hdp { flush, mem, value } => format!("HDP {flush:#x} {} {mem:#x} {}", name(mem), answer(value)),
            Did::Sleep { us } => format!("SLEEP us={us}"),
        })
        .collect()
}
