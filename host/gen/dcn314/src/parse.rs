//! Parsers for dcn_3_1_4_offset.h, dcn_3_1_4_sh_mask.h and the G-1 snapshot text.
//! Strict: any `#define` line that is not understood is an error, so a changed header
//! cannot silently drop registers.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct RawReg {
    pub name: String,
    pub block: String,
    pub dword: u32,
    pub base_idx: Option<u32>,
    /// `ix` define: an index for an index/data register pair of its block, not a BAR5 offset.
    pub indirect: bool,
}

#[derive(Debug, Clone)]
pub struct RawField {
    pub name: String,
    pub shift: u32,
    pub mask: u32,
}

fn num(s: &str) -> Result<u64, String> {
    let t = s.trim_end_matches(['L', 'l', 'U', 'u']);
    let r = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        t.parse::<u64>()
    };
    r.map_err(|e| format!("bad number {s:?}: {e}"))
}

/// Registers in header order.
pub fn offsets(text: &str) -> Result<Vec<RawReg>, String> {
    let mut regs: Vec<RawReg> = Vec::new();
    let mut pos: BTreeMap<String, usize> = BTreeMap::new();
    let mut block = String::from("unnamed");
    for (ln, line) in text.lines().enumerate() {
        let t = line.trim();
        if let Some(b) = t.strip_prefix("// addressBlock:") {
            block = b.trim().to_string();
            continue;
        }
        let toks: Vec<&str> = t.split_whitespace().collect();
        if toks.first() != Some(&"#define") {
            continue;
        }
        if toks.len() == 2 && toks[1].starts_with('_') {
            continue; // include guard
        }
        let bad = || format!("offset.h:{}: unexpected define {t:?}", ln + 1);
        if toks.len() != 3 {
            return Err(bad());
        }
        let (name, indirect) = match (toks[1].strip_prefix("reg"), toks[1].strip_prefix("ix")) {
            (Some(n), _) => (n, false),
            (None, Some(n)) => (n, true),
            _ => return Err(bad()),
        };
        let v = num(toks[2]).map_err(|e| format!("offset.h:{}: {e}", ln + 1))?;
        if let Some(base) = name.strip_suffix("_BASE_IDX") {
            // Only `reg` defines carry a BASE_IDX, and it follows its register exactly once.
            let i = match (indirect, pos.get(base)) {
                (false, Some(&i)) => i,
                _ => return Err(format!("offset.h:{}: {} without its register", ln + 1, toks[1])),
            };
            if regs[i].base_idx.is_some() {
                return Err(format!("offset.h:{}: {} defined twice", ln + 1, toks[1]));
            }
            if v > u8::MAX as u64 {
                return Err(format!("offset.h:{}: BASE_IDX {v} too large", ln + 1));
            }
            regs[i].base_idx = Some(v as u32);
            continue;
        }
        if pos.contains_key(name) {
            return Err(format!("offset.h:{}: register {name} defined twice", ln + 1));
        }
        if v > u32::MAX as u64 {
            return Err(format!("offset.h:{}: offset too large", ln + 1));
        }
        pos.insert(name.to_string(), regs.len());
        regs.push(RawReg { name: name.to_string(), block: block.clone(), dword: v as u32, base_idx: None, indirect });
    }
    Ok(regs)
}

/// Fields per register name, in header order. `known` decides where the register name ends:
/// the longest `__`-separated prefix that is a known register (field names may contain `__`).
pub type FieldMap = BTreeMap<String, Vec<RawField>>;
/// (shift, mask, first-seen order) while both halves of a field are collected.
type Partial = (Option<u32>, Option<u32>, usize);

/// Returns the fields and the defines whose register is unknown.
pub fn fields(text: &str, known: &BTreeMap<String, usize>) -> Result<(FieldMap, Vec<String>), String> {
    let mut acc: BTreeMap<(String, String), Partial> = BTreeMap::new();
    let mut orphans = Vec::new();
    let mut order = 0usize;
    for (ln, line) in text.lines().enumerate() {
        let t = line.trim();
        let toks: Vec<&str> = t.split_whitespace().collect();
        if toks.first() != Some(&"#define") {
            continue;
        }
        if toks.len() == 2 && toks[1].starts_with('_') {
            continue;
        }
        if toks.len() != 3 {
            return Err(format!("sh_mask.h:{}: unexpected define {t:?}", ln + 1));
        }
        let full = toks[1];
        let v = num(toks[2]).map_err(|e| format!("sh_mask.h:{}: {e}", ln + 1))?;
        let (stem, is_shift) = if let Some(s) = full.strip_suffix("__SHIFT") {
            (s, true)
        } else if let Some(s) = full.strip_suffix("_MASK") {
            (s, false)
        } else {
            return Err(format!("sh_mask.h:{}: neither SHIFT nor MASK: {full}", ln + 1));
        };
        let mut split = None;
        let mut i = 0;
        while let Some(p) = stem[i..].find("__") {
            let at = i + p;
            if known.contains_key(&stem[..at]) {
                split = Some(at);
            }
            i = at + 1;
        }
        let Some(at) = split else {
            orphans.push(full.to_string());
            continue;
        };
        let key = (stem[..at].to_string(), stem[at + 2..].to_string());
        let e = acc.entry(key).or_insert_with(|| {
            order += 1;
            (None, None, order)
        });
        if v > u32::MAX as u64 {
            return Err(format!("sh_mask.h:{}: value too large", ln + 1));
        }
        let slot = if is_shift { &mut e.0 } else { &mut e.1 };
        if slot.replace(v as u32).is_some() {
            return Err(format!("sh_mask.h:{}: {full} defined twice", ln + 1));
        }
    }
    let mut out: BTreeMap<String, Vec<(usize, RawField)>> = BTreeMap::new();
    for ((reg, field), (shift, mask, ord)) in acc {
        let (Some(shift), Some(mask)) = (shift, mask) else {
            return Err(format!("{reg}__{field}: SHIFT or MASK missing"));
        };
        if shift >= 32 || mask == 0 {
            return Err(format!("{reg}__{field}: shift {shift} mask {mask:#x} invalid"));
        }
        let m = mask >> shift;
        if mask & ((1u32 << shift) - 1) != 0 || m & 1 == 0 || (m & m.wrapping_add(1)) != 0 {
            return Err(format!("{reg}__{field}: mask {mask:#x} not contiguous from bit {shift}"));
        }
        out.entry(reg).or_default().push((ord, RawField { name: field, shift, mask }));
    }
    let out = out
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by_key(|x| x.0);
            (k, v.into_iter().map(|x| x.1).collect())
        })
        .collect();
    Ok((out, orphans))
}

/// One line of p1-decoded.txt: BAR5 byte offset, value (names are checked by the crate's tests).
pub fn snapshot(text: &str) -> Result<Vec<(u32, u32)>, String> {
    let mut out = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(o), Some(v)) = (it.next(), it.next()) else {
            return Err(format!("p1:{}: short line", ln + 1));
        };
        let o = num(o).map_err(|e| format!("p1:{}: {e}", ln + 1))?;
        let v = num(v).map_err(|e| format!("p1:{}: {e}", ln + 1))?;
        if o > u32::MAX as u64 || v > u32::MAX as u64 {
            return Err(format!("p1:{}: value too large", ln + 1));
        }
        out.push((o as u32, v as u32));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_with_mask_inside() {
        let mut k = BTreeMap::new();
        k.insert("R0".to_string(), 0);
        let t = "#define R0__A_MASK__SHIFT 0x4\n#define R0__A_MASK_MASK 0x00000010L\n";
        let (f, o) = fields(t, &k).unwrap();
        assert!(o.is_empty());
        assert_eq!(f["R0"][0].name, "A_MASK");
        assert_eq!((f["R0"][0].shift, f["R0"][0].mask), (4, 0x10));
    }

    #[test]
    fn non_contiguous_mask_is_an_error() {
        let mut k = BTreeMap::new();
        k.insert("R0".to_string(), 0);
        let t = "#define R0__A__SHIFT 0x0\n#define R0__A_MASK 0x00000005L\n";
        assert!(fields(t, &k).is_err());
    }

    #[test]
    fn strict_offset_rules() {
        let err = |t: &str| offsets(t).unwrap_err();
        assert!(err("#define\n").contains("unexpected define"));
        assert!(err("#define regX\n").contains("unexpected define"));
        assert!(err("#define regX 1 2\n").contains("unexpected define"));
        assert!(err("#define mmX 0x1\n").contains("unexpected define"));
        assert!(err("#define regX 0xZZ\n").contains("bad number"));
        assert!(err("#define regX 0x100000000\n").contains("too large"));
        assert!(err("#define regX 1\n#define regX 2\n").contains("twice"));
        assert!(err("#define regX_BASE_IDX 2\n#define regX 1\n").contains("without its register"));
        assert!(err("#define regY 1\n#define regX_BASE_IDX 2\n").contains("without its register"));
        assert!(err("#define ixX 1\n#define ixX_BASE_IDX 2\n").contains("without its register"));
        assert!(err("#define regX 1\n#define regX_BASE_IDX 2\n#define regX_BASE_IDX 2\n").contains("twice"));
    }

    #[test]
    fn strict_field_rules() {
        let mut k = BTreeMap::new();
        k.insert("R0".to_string(), 0);
        let err = |t: &str| fields(t, &k).unwrap_err();
        assert!(err("#define\n").contains("unexpected define"));
        assert!(err("#define R0__A__SHIFT\n").contains("unexpected define"));
        assert!(err("#define R0__A_BITS 0x1\n").contains("neither SHIFT nor MASK"));
        assert!(err("#define R0__A__SHIFT 0x0\n").contains("missing"));
        assert!(err("#define R0__A_MASK 0x1L\n").contains("missing"));
        assert!(err("#define R0__A__SHIFT 0x0\n#define R0__A__SHIFT 0x0\n#define R0__A_MASK 1\n").contains("twice"));
        assert!(err("#define R0__A__SHIFT 0x20\n#define R0__A_MASK 0x1L\n").contains("invalid"));
        assert!(err("#define R0__A__SHIFT 0x0\n#define R0__A_MASK 0x0L\n").contains("invalid"));
        assert!(err("#define R0__A__SHIFT 0x1\n#define R0__A_MASK 0x1L\n").contains("not contiguous"));
        assert!(err("#define R0__A__SHIFT 0x0\n#define R0__A_MASK 0x1FFFFFFFFL\n").contains("too large"));
        let (_, orphans) = fields("#define Q__A__SHIFT 0x0\n", &k).unwrap();
        assert_eq!(orphans, ["Q__A__SHIFT"]);
    }

    #[test]
    fn strict_snapshot_rules() {
        assert!(snapshot("0x10\n").unwrap_err().contains("short line"));
        assert!(snapshot("0x10 zz\n").unwrap_err().contains("bad number"));
        assert!(snapshot("0x1FFFFFFFF 0\n").unwrap_err().contains("too large"));
        assert_eq!(snapshot("\n0x10 0x2 NAME\n").unwrap(), [(0x10, 2)]);
    }

    #[test]
    fn base_idx_attaches_to_its_register() {
        let t = "// addressBlock: b\n#define regX 0x0010\n#define regX_BASE_IDX 2\n";
        let r = offsets(t).unwrap();
        assert_eq!((r[0].dword, r[0].base_idx, r[0].block.as_str()), (0x10, Some(2), "b"));
    }
}
