//! The cross-check: a C program that prints sizeof/_Alignof/offsetof and constant values from the real headers,
//! and a Rust table of the same facts taken from the generated types. crates/drm/tests/layout.rs compares them.

use std::fmt::Write;

use crate::Item;
use crate::emit::field_name;
use crate::parse::{Kind, Model};

/// (top-level C type, member path prefix, C expression of this aggregate's offset in the top-level type).
fn cref(m: &Model, a: usize) -> (String, String, String) {
    let ag = &m.aggs[a];
    match &ag.kind {
        Kind::Tagged(t) => (format!("{} {t}", if ag.union { "union" } else { "struct" }), String::new(), "0".into()),
        Kind::Inline { parent, path } => {
            let (tt, pre, _) = cref(m, *parent);
            let base = format!("__builtin_offsetof({tt}, {pre}{path})");
            (tt, format!("{pre}{path}."), base)
        }
        Kind::Anon { parent, .. } => {
            let (tt, pre, _) = cref(m, *parent);
            let base = format!("__builtin_offsetof({tt}, {pre}{})", first_leaf(m, a));
            (tt, pre, base)
        }
    }
}

/// C name of the first named member reachable through anonymous members (it sits at the anonymous member's offset).
fn first_leaf(m: &Model, a: usize) -> String {
    let mb = &m.aggs[a].members[0];
    match (&mb.name, crate::parse::base_agg(&mb.ty)) {
        (Some(n), _) => n.clone(),
        (None, crate::parse::Ty::Agg(c)) => first_leaf(m, c),
        _ => String::new(),
    }
}

/// C type expression usable in sizeof, or None for anonymous aggregates (C cannot name them).
fn c_type(m: &Model, a: usize) -> Option<String> {
    match &m.aggs[a].kind {
        Kind::Tagged(_) => Some(cref(m, a).0),
        Kind::Inline { parent, path } => {
            let (tt, pre, _) = cref(m, *parent);
            Some(format!("__typeof__((({tt} *)0)->{pre}{path})"))
        }
        Kind::Anon { .. } => None,
    }
}

pub fn render(m: &Model, aggs: &[usize], items: &[Item], header: &str) -> (String, String) {
    let mut c = String::from(header); // `//` comments are C too (gnu11); the notices contain `*/`
    c.push_str("\n#include <fcntl.h>\n#include <stddef.h>\n#include <stdio.h>\n#include <drm.h>\n#include <drm_mode.h>\n#include <amdgpu_drm.h>\n");
    c.push_str("#include <linux/sync_file.h>\n#include <linux/dma-buf.h>\n\n");
    c.push_str("#define T(n, t) printf(\"type %s %zu %zu\\n\", n, sizeof(t), _Alignof(t))\n");
    c.push_str("#define F(t, n, e) printf(\"field %s %s %zu\\n\", t, n, (size_t)(e))\n");
    c.push_str("#define C(n) printf(\"const %s %lld\\n\", #n, (long long)(n))\n\nint main(void)\n{\n");
    let mut r = String::from(header);
    r.push_str("\n// Facts of the generated Rust types, compared with tests/data/c-layout.txt by tests/layout.rs.\n\n");
    let (mut types, mut fields, mut all) = (String::new(), String::new(), String::new());
    for &a in aggs {
        let ag = &m.aggs[a];
        let _ = writeln!(all, "        $m!({});", ag.rust);
        if let Some(ct) = c_type(m, a) {
            let _ = writeln!(c, "\tT(\"{}\", {ct});", ag.rust);
            let _ = writeln!(types, "    (\"{0}\", size_of::<{0}>(), align_of::<{0}>()),", ag.rust);
        }
        let (tt, pre, base) = cref(m, a);
        for (k, mb) in ag.members.iter().enumerate() {
            let f = field_name(m, ag, k);
            let plain = f.trim_start_matches("r#");
            let off = match (&mb.name, crate::parse::base_agg(&mb.ty)) {
                (Some(n), _) => format!("__builtin_offsetof({tt}, {pre}{n}) - ({base})"),
                (None, crate::parse::Ty::Agg(child)) => format!("({}) - ({base})", cref(m, child).2),
                _ => continue,
            };
            let _ = writeln!(c, "\tF(\"{}\", \"{plain}\", {off});", ag.rust);
            let rust_off = if ag.union { "0".to_string() } else { format!("offset_of!({}, {f})", ag.rust) };
            let _ = writeln!(fields, "    (\"{}\", \"{plain}\", {rust_off}),", ag.rust);
        }
    }
    let (mut consts, mut ioctls) = (String::new(), String::new());
    for it in items {
        let _ = writeln!(c, "\tC({});", it.name);
        let _ = writeln!(consts, "    (\"{0}\", {0} as i64),", it.name);
        if let Some(t) = &it.ioctl_rust {
            let _ = writeln!(ioctls, "    (\"{0}\", {0}, size_of::<{t}>()),", it.name);
        }
    }
    c.push_str("\treturn 0;\n}\n");
    let _ = write!(
        r,
        "pub const TYPES: &[(&str, usize, usize)] = &[\n{types}];\n\npub const FIELDS: &[(&str, &str, usize)] = &[\n{fields}];\n\n\
         pub const CONSTS: &[(&str, i64)] = &[\n{consts}];\n\n/// (name, number, size_of the argument's Rust type)\n\
         pub const IOCTLS: &[(&str, u32, usize)] = &[\n{ioctls}];\n\n\
         /// Calls `$m!(T)` for every generated type, named or anonymous.\n\
         macro_rules! for_each_type {{\n    ($m:ident) => {{\n{all}    }};\n}}\n"
    );
    (c, r)
}
