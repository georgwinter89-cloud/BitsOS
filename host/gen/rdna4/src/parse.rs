//! Parsers for `<block>_offset.h` and `<block>_sh_mask.h`. Strict: a `#define` that is not understood is an error, so a
//! changed header cannot silently drop a register.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `reg`/`mm`: a dword offset below one of the IP's discovery bases.
    Mmio,
    /// `ix`: an index for an index/data pair of its block.
    Indirect,
    /// `cfg`: a byte offset in PCI configuration space (NBIF).
    Config,
}

#[derive(Debug, Clone)]
pub struct RawReg {
    pub name: String,
    pub kind: Kind,
    pub offset: u32,
    pub base_idx: Option<u8>,
    pub fields: Vec<RawField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawField {
    pub name: String,
    pub shift: u32,
    pub mask: u32,
}

fn num(s: &str) -> Result<u64, String> {
    let t = s.trim_end_matches(['L', 'l', 'U', 'u']);
    let r = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => t.parse::<u64>(),
    };
    r.map_err(|e| format!("bad number {s:?}: {e}"))
}

/// The `#define NAME VALUE` lines; include guards (one token after `#define`) are skipped.
fn defines<'a>(text: &'a str, file: &'a str) -> impl Iterator<Item = Result<(usize, &'a str, u64), String>> + 'a {
    text.lines().enumerate().filter_map(move |(ln, line)| {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.first() != Some(&"#define") || (toks.len() == 2 && toks[1].starts_with('_')) {
            return None;
        }
        if toks.len() != 3 {
            return Some(Err(format!("{file}:{}: unexpected define {:?}", ln + 1, line.trim())));
        }
        Some(num(toks[2]).map(|v| (ln + 1, toks[1], v)).map_err(|e| format!("{file}:{}: {e}", ln + 1)))
    })
}

fn split_kind(define: &str) -> Option<(&str, Kind)> {
    // `regFOO` and the older `mmFOO` name the same thing; `ix` and `cfg` are other address spaces.
    for (p, k) in [("reg", Kind::Mmio), ("mm", Kind::Mmio), ("ix", Kind::Indirect), ("cfg", Kind::Config)] {
        if let Some(n) = define.strip_prefix(p) {
            return Some((n, k));
        }
    }
    None
}

/// Registers in header order. Every MMIO register must carry its `_BASE_IDX`, except the ones `headerless` names: Linux's
/// header forgot theirs, so nothing says below which base they lie, and they are left out.
/// `stray` names whole defines that are skipped (a `_BASE_IDX` whose register the header does not define).
pub fn offsets(text: &str, headerless: &[&str], stray: &[&str]) -> Result<Vec<RawReg>, String> {
    let mut strays = 0;
    let mut regs: Vec<RawReg> = Vec::new();
    let mut pos: BTreeMap<(String, bool), usize> = BTreeMap::new();
    for d in defines(text, "offset.h") {
        let (ln, define, v) = d?;
        if stray.contains(&define) {
            strays += 1;
            continue;
        }
        let (name, kind) = split_kind(define).ok_or_else(|| format!("offset.h:{ln}: unknown prefix in {define}"))?;
        let mmio = kind == Kind::Mmio;
        if let Some(base) = name.strip_suffix("_BASE_IDX").filter(|b| pos.contains_key(&((*b).to_string(), true))) {
            let r = &mut regs[pos[&(base.to_string(), true)]];
            if !mmio || r.base_idx.is_some() || v > 7 {
                return Err(format!("offset.h:{ln}: {define}: second, misplaced or impossible BASE_IDX"));
            }
            r.base_idx = Some(v as u8);
            continue;
        }
        if pos.insert((name.to_string(), mmio), regs.len()).is_some() {
            return Err(format!("offset.h:{ln}: {define} defined twice"));
        }
        let offset = u32::try_from(v).map_err(|_| format!("offset.h:{ln}: offset too large"))?;
        regs.push(RawReg { name: name.to_string(), kind, offset, base_idx: None, fields: Vec::new() });
    }
    let bare = |r: &RawReg| r.kind == Kind::Mmio && r.base_idx.is_none();
    if let Some(r) = regs.iter().find(|r| bare(r) && !headerless.contains(&r.name.as_str())) {
        return Err(format!("offset.h: {} has no BASE_IDX", r.name));
    }
    if let Some(n) = headerless.iter().find(|n| !regs.iter().any(|r| bare(r) && r.name == **n)) {
        return Err(format!("offset.h: {n} is listed as lacking a BASE_IDX and has one (or is gone): correct the list"));
    }
    if strays != stray.len() {
        return Err(format!("offset.h: {strays} of the {} stray defines found: correct the list", stray.len()));
    }
    regs.retain(|r| !bare(r));
    Ok(regs)
}

/// Attaches the fields of `sh_mask.h` to their registers (the longest `__`-separated prefix that names a register;
/// field names may contain `__`). Returns the defines whose register the offset header does not have. The registers
/// `wide` names are 64 bits wide in the mask header: their fields are left out (a `Field` is 32 bits), any other wide
/// mask is an error.
/// Second list: fields the header defines by half (a SHIFT without its MASK or the reverse), left out as well.
pub fn fields(text: &str, regs: &mut [RawReg], wide: &[&str]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut halves = Vec::new();
    let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, r) in regs.iter().enumerate() {
        // An MMIO register wins over an `ix`/`cfg` one of the same name: the mask header does not say which it means.
        if r.kind == Kind::Mmio || !by_name.contains_key(r.name.as_str()) {
            by_name.insert(r.name.as_str(), i);
        }
    }
    let mut acc: Vec<(usize, String, Option<u32>, Option<u32>)> = Vec::new();
    let mut at: BTreeMap<(usize, String), usize> = BTreeMap::new();
    let mut orphans = Vec::new();
    for d in defines(text, "sh_mask.h") {
        let (ln, full, v) = d?;
        let (stem, is_shift) = match (full.strip_suffix("__SHIFT"), full.strip_suffix("_MASK")) {
            (Some(s), _) => (s, true),
            (None, Some(s)) => (s, false),
            _ => return Err(format!("sh_mask.h:{ln}: neither SHIFT nor MASK: {full}")),
        };
        let split = stem.match_indices("__").filter(|(p, _)| by_name.contains_key(&stem[..*p])).map(|(p, _)| p).last();
        let Some(p) = split else {
            orphans.push(full.to_string());
            continue;
        };
        if wide.contains(&&stem[..p]) {
            continue;
        }
        let v = u32::try_from(v).map_err(|_| format!("sh_mask.h:{ln}: {full} does not fit 32 bits"))?;
        let key = (by_name[&stem[..p]], stem[p + 2..].to_string());
        let i = *at.entry(key.clone()).or_insert_with(|| {
            acc.push((key.0, key.1.clone(), None, None));
            acc.len() - 1
        });
        let slot = if is_shift { &mut acc[i].2 } else { &mut acc[i].3 };
        if slot.replace(v).is_some() {
            return Err(format!("sh_mask.h:{ln}: {full} defined twice"));
        }
    }
    drop(by_name);
    for (reg, name, shift, mask) in acc {
        let (Some(shift), Some(mask)) = (shift, mask) else {
            halves.push(format!("{}__{name}", regs[reg].name));
            continue;
        };
        if shift > 31 || (mask != 0 && mask.trailing_zeros() != shift) {
            return Err(format!("sh_mask.h: {}__{name}: shift {shift} does not fit mask {mask:#x}", regs[reg].name));
        }
        regs[reg].fields.push(RawField { name, shift, mask });
    }
    Ok((orphans, halves))
}
