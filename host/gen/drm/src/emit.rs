//! Renders one Rust module per header: aggregates with `Wire` impls, typedef aliases, constants, ioctl table.

use std::fmt::Write;

use crate::Item;
use crate::parse::{Agg, Kind, Model, Ty};

/// Rust keywords (2024 edition, strict and reserved) that C member names may collide with.
const KEYWORDS: [&str; 50] = [
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let",
    "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

pub fn ident(n: &str) -> String {
    if KEYWORDS.contains(&n) { format!("r#{n}") } else { n.to_string() }
}

pub fn rust_ty(m: &Model, t: &Ty) -> String {
    match t {
        Ty::Prim(p) => p.rust().to_string(),
        Ty::Ptr => "u64".to_string(),
        Ty::Arr(e, n) => format!("[{}; {n}]", rust_ty(m, e)),
        Ty::Agg(i) => m.aggs[*i].rust.clone(),
    }
}

/// Rust field name of member `k` of `a`.
pub fn field_name(m: &Model, a: &Agg, k: usize) -> String {
    let mb = &a.members[k];
    match &mb.name {
        Some(n) => ident(n),
        None => match crate::parse::base_agg(&mb.ty) {
            Ty::Agg(c) => match m.aggs[c].kind {
                Kind::Anon { k, .. } => format!("anon{k}"),
                _ => "anon".to_string(),
            },
            _ => "anon".to_string(),
        },
    }
}

pub fn c_desc(m: &Model, a: &Agg) -> String {
    let kw = if a.union { "union" } else { "struct" };
    match &a.kind {
        Kind::Tagged(t) => format!("`{kw} {t}`"),
        Kind::Inline { parent, path } => format!("untagged `{kw}` of member `{}.{path}`", m.aggs[*parent].rust),
        Kind::Anon { parent, k } => format!("anonymous `{kw}` member of `{}` (field `anon{k}`)", m.aggs[*parent].rust),
    }
}

fn agg(m: &Model, a: &Agg, label: &str, s: &mut String) {
    let _ = writeln!(s, "/// {} from {label}.", c_desc(m, a));
    if a.union {
        union(m, a, s);
        return;
    }
    let fields: Vec<(String, String, u64)> =
        (0..a.members.len()).map(|k| (field_name(m, a, k), rust_ty(m, &a.members[k].ty), a.members[k].offset)).collect();
    let _ = writeln!(s, "#[repr(C)]\n#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub struct {} {{", a.rust);
    for (f, t, _) in &fields {
        let _ = writeln!(s, "    pub {f}: {t},");
    }
    let _ = writeln!(s, "}}\n\nimpl Wire for {} {{\n    const ZERO: Self = Self {{", a.rust);
    for (f, t, _) in &fields {
        let _ = writeln!(s, "        {f}: <{t} as Wire>::ZERO,");
    }
    let _ = writeln!(s, "    }};\n\n    fn decode(b: &[u8]) -> Option<Self> {{\n        let b = b.get(..Self::SIZE)?;\n        Some(Self {{");
    for (f, t, o) in &fields {
        let _ = writeln!(s, "            {f}: <{t} as Wire>::decode(b.get({o}..)?)?,");
    }
    let _ = writeln!(s, "        }})\n    }}\n\n    fn encode(&self, out: &mut [u8]) -> Option<()> {{");
    let _ = writeln!(s, "        let o = out.get_mut(..Self::SIZE)?;\n        o.fill(0);");
    for (f, _, o) in &fields {
        let _ = writeln!(s, "        self.{f}.encode(o.get_mut({o}..)?)?;");
    }
    let _ = writeln!(s, "        Some(())\n    }}\n}}\n");
    default(a, s);
}

fn default(a: &Agg, s: &mut String) {
    let _ = writeln!(s, "impl Default for {0} {{\n    fn default() -> Self {{\n        <{0} as Wire>::ZERO\n    }}\n}}\n", a.rust);
}

/// A C union becomes its bytes plus typed accessors: reading a Rust union field would need `unsafe`.
fn union(m: &Model, a: &Agg, s: &mut String) {
    let (n, r) = (a.size, &a.rust);
    let _ = writeln!(s, "#[repr(C, align({}))]\n#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub struct {r} {{\n    pub bytes: [u8; {n}],\n}}\n", a.align);
    let _ = writeln!(s, "impl Wire for {r} {{\n    const ZERO: Self = Self {{ bytes: [0; {n}] }};\n");
    let _ = writeln!(s, "    fn decode(b: &[u8]) -> Option<Self> {{\n        Some(Self {{ bytes: <[u8; {n}] as Wire>::decode(b)? }})\n    }}\n");
    let _ = writeln!(s, "    fn encode(&self, out: &mut [u8]) -> Option<()> {{\n        self.bytes.encode(out)\n    }}\n}}\n");
    default(a, s);
    let _ = writeln!(s, "impl {r} {{");
    for k in 0..a.members.len() {
        let f = field_name(m, a, k);
        let t = rust_ty(m, &a.members[k].ty);
        let plain = f.trim_start_matches("r#");
        let _ = writeln!(s, "    /// Reads member `{plain}`.\n    pub fn {f}(&self) -> {t} {{");
        let _ = writeln!(s, "        <{t} as Wire>::decode(&self.bytes).unwrap_or(<{t} as Wire>::ZERO)\n    }}\n");
        let _ = writeln!(s, "    /// Writes member `{plain}`; every other byte of the union becomes zero.\n    pub fn set_{plain}(&mut self, v: &{t}) {{");
        let _ = writeln!(s, "        self.bytes = [0; {n}];\n        let _ = v.encode(&mut self.bytes);\n    }}\n");
    }
    let _ = writeln!(s, "}}\n");
}

pub fn literal(it: &Item) -> String {
    if it.ioctl.is_some() {
        format!("{:#010x}", it.val)
    } else if it.val < 0 || it.val < 10 {
        it.val.to_string()
    } else {
        format!("{:#x}", it.val)
    }
}

pub struct ModuleIn<'a> {
    pub label: &'a str,
    pub module: &'a str,
    pub file: usize,
    pub header: &'a str,
    pub aggs: &'a [usize],
    pub typedefs: &'a [usize],
    pub items: &'a [Item],
    pub skipped: &'a [(String, String)],
    pub scope_note: Option<&'a str>,
}

pub fn module(m: &Model, mi: &ModuleIn) -> String {
    let mut s = String::from(mi.header);
    if let Some(n) = mi.scope_note {
        let _ = writeln!(s, "// Scope: {n}");
    }
    if !mi.skipped.is_empty() {
        let _ = writeln!(s, "// Macros of {} that are not generated:", mi.label);
        for (n, why) in mi.skipped {
            let _ = writeln!(s, "//   {n}: {why}");
        }
    }
    let _ = writeln!(s, "\n//! `{}` as Rust: layouts, constants and ioctl numbers for x86_64.\n", mi.label);
    let foreign = |t: &Ty| matches!(crate::parse::base_agg(t), Ty::Agg(c) if m.aggs[c].file != mi.file);
    let uses_other = mi.aggs.iter().any(|&i| m.aggs[i].members.iter().any(|mb| foreign(&mb.ty)))
        || mi.typedefs.iter().any(|&t| foreign(&m.typedefs[t].ty));
    if uses_other {
        let _ = writeln!(s, "use super::*;");
    }
    let _ = writeln!(s, "use crate::wire::Wire;\n");
    for &i in mi.aggs {
        agg(m, &m.aggs[i], mi.label, &mut s);
    }
    for &t in mi.typedefs {
        let td = &m.typedefs[t];
        let _ = writeln!(s, "/// `typedef {}` from {}.\npub type {} = {};\n", td.name, mi.label, td.name, rust_ty(m, &td.ty));
    }
    let mut last_enum = None;
    for it in mi.items {
        if it.enum_name.as_deref() != last_enum.as_deref() {
            if let Some(e) = &it.enum_name {
                let _ = writeln!(s, "// enum {}", if e.is_empty() { "(untagged)" } else { e });
            }
            last_enum = it.enum_name.clone();
        }
        let doc = match &it.ioctl {
            Some(t) if t.is_empty() => "/// ioctl without argument.\n".to_string(),
            Some(t) => format!("/// ioctl, argument `{t}`.\n"),
            None => String::new(),
        };
        let _ = writeln!(s, "{doc}pub const {}: {} = {};", it.name, it.rust_ty, literal(it));
    }
    let ioctls: Vec<&Item> = mi.items.iter().filter(|i| i.ioctl.is_some()).collect();
    if ioctls.is_empty() {
        return s;
    }
    let _ = writeln!(s, "\n/// Every ioctl number of {} with its name, in header order.", mi.label);
    let _ = writeln!(s, "pub const {}_IOCTLS: &[(&str, u32)] = &[", mi.module.to_ascii_uppercase());
    for it in ioctls {
        let _ = writeln!(s, "    (\"{0}\", {0}),", it.name);
    }
    let _ = writeln!(s, "];");
    s
}
