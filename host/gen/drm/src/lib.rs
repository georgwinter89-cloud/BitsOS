//! drm-gen: turns the DRM, amdgpu, sync_file and dma-buf user API headers into crates/drm/src/uapi/*.rs, plus a
//! C probe (host/gen/drm/tests/probe.c) and the Rust facts it is compared with (crates/drm/tests/data/).
#![forbid(unsafe_code)]

pub mod emit;
pub mod eval;
pub mod parse;
pub mod pp;
pub mod probe;

use std::path::Path;

use parse::{Ty, base_agg};

/// The inputs, relative to the include root, and the Rust module each becomes.
pub const INPUTS: [(&str, &str); 5] = [
    ("libdrm/drm.h", "drm"),
    ("libdrm/drm_mode.h", "drm_mode"),
    ("libdrm/amdgpu_drm.h", "amdgpu"),
    ("linux/sync_file.h", "sync_file"),
    ("linux/dma-buf.h", "dma_buf"),
];

/// What the inputs must be, by FNV-1a 64; another header version is refused until someone reviews it here.
pub const SET_NAME: &str = "libdrm 2.4.134 (drm.h, drm_mode.h, amdgpu_drm.h) and linux-api-headers 7.1 (sync_file.h, dma-buf.h)";
pub const SET: [u64; 5] = [0x83f9_1d64_1712_60e5, 0xece7_73ab_c6f1_245a, 0xda31_3432_4f73_335c, 0xa56e_4b66_b06c_3b7e, 0xd866_5ecf_979d_9713];

/// drm_mode.h is KMS: a render node needs none of it, so only types reachable from drm.h's ioctl numbers are kept.
const FILTERED: usize = 1;

pub const PROBE_C: &str = "host/gen/drm/tests/probe.c";
pub const C_LAYOUT: &str = "crates/drm/tests/data/c-layout.txt";

pub struct Item {
    pub name: String,
    pub val: i128,
    pub rust_ty: &'static str,
    pub ioctl: Option<String>,
    pub ioctl_rust: Option<String>,
    pub enum_name: Option<String>,
    pub file: usize,
}

pub struct Output {
    pub files: Vec<(String, Vec<u8>)>, // path relative to the repository root
    pub probe_c: String,
    pub types: usize,
    pub consts: usize,
    pub ioctls: usize,
}

pub fn fnv1a64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| (h ^ x as u64).wrapping_mul(0x0100_0000_01b3))
}

fn rust_int(v: &eval::Val) -> &'static str {
    match v.t {
        eval::CTy::Int if v.v < 0 => "i32",
        eval::CTy::Int | eval::CTy::UInt => "u32",
        eval::CTy::Long if v.v < 0 => "i64",
        eval::CTy::Long | eval::CTy::ULong => "u64",
    }
}

pub fn generate(texts: &[Vec<u8>]) -> Result<Output, String> {
    if texts.len() != INPUTS.len() {
        return Err("wrong number of inputs".into());
    }
    let mut strs = Vec::new();
    for (k, t) in texts.iter().enumerate() {
        let h = fnv1a64(t);
        if SET[k] != 0 && h != SET[k] {
            return Err(format!("{} (FNV-1a 64 {h:016x}) is not the reviewed header of {SET_NAME} ({:016x})", INPUTS[k].0, SET[k]));
        }
        strs.push(std::str::from_utf8(t).map_err(|e| format!("{}: {e}", INPUTS[k].0))?);
    }
    let ins: Vec<pp::Input> = INPUTS.iter().zip(&strs).map(|(i, t)| pp::Input { label: i.0, text: t }).collect();
    let pp = pp::Pp::run(&ins)?;
    let m = parse::parse(&pp)?;

    let (mut items, mut skipped) = (Vec::new(), vec![Vec::new(); INPUTS.len()]);
    for (idx, mac) in pp.macros.iter().enumerate() {
        if pp.index.get(&mac.name) != Some(&idx) || mac.file == FILTERED || mac.body.is_empty() {
            continue;
        }
        if mac.params.is_some() {
            skipped[mac.file].push((mac.name.clone(), "function-like macro".to_string()));
            continue;
        }
        if mac.body.iter().any(|t| matches!(t, pp::Tok::Str(_))) {
            skipped[mac.file].push((mac.name.clone(), "string".to_string()));
            continue;
        }
        match eval::eval(&pp, &m, &[pp::Tok::Id(mac.name.clone())]) {
            Ok(d) => {
                let ioctl_rust = match &d.ioctl_ty {
                    Some(t) if !t.is_empty() => Some(emit::rust_ty(&m, &m.type_name(t)?)),
                    _ => None,
                };
                let ioctl = d.ioctl_ty.as_ref().map(|t| t.iter().map(tok_text).collect::<Vec<_>>().join(" "));
                let rust_ty = if ioctl.is_some() { "u32" } else { rust_int(&d.val) };
                items.push(Item { name: mac.name.clone(), val: d.val.v, rust_ty, ioctl, ioctl_rust, enum_name: None, file: mac.file });
            }
            Err(e) => skipped[mac.file].push((mac.name.clone(), e)),
        }
    }
    for e in m.enumerators.iter().filter(|e| e.file != FILTERED) {
        if items.iter().any(|i| i.name == e.name) {
            return Err(format!("{} is both a macro and an enumerator", e.name));
        }
        let rust_ty = rust_int(&e.val);
        items.push(Item { name: e.name.clone(), val: e.val.v, rust_ty, ioctl: None, ioctl_rust: None, enum_name: Some(e.enum_name.clone()), file: e.file });
    }

    // Aggregates: everything of the unfiltered inputs, plus what those and the ioctl numbers refer to.
    let mut keep = vec![false; m.aggs.len()];
    fn mark(m: &parse::Model, keep: &mut [bool], i: usize) {
        if !keep[i] {
            keep[i] = true;
            for mb in &m.aggs[i].members {
                if let Ty::Agg(c) = base_agg(&mb.ty) {
                    mark(m, keep, c);
                }
            }
        }
    }
    for (i, a) in m.aggs.iter().enumerate() {
        if a.defined && a.file != FILTERED {
            mark(&m, &mut keep, i);
        }
    }
    for it in items.iter().filter(|i| i.ioctl_rust.is_some()) {
        let toks = pp::lex(it.ioctl.as_deref().unwrap_or(""))?;
        if let Ty::Agg(i) = m.type_name(&toks)? {
            mark(&m, &mut keep, i);
        }
    }

    let mut common = format!("// @generated by host/gen/drm (cargo run -p drm-gen). Do not edit by hand.\n// Source: {SET_NAME}, x86_64.\n");
    for (k, t) in texts.iter().enumerate() {
        common.push_str(&format!("// input {}: {} bytes, FNV-1a 64 {:016x}\n", INPUTS[k].0, t.len(), fnv1a64(t)));
    }
    let mut files = Vec::new();
    for (k, (label, module)) in INPUTS.iter().enumerate() {
        let aggs: Vec<usize> = (0..m.aggs.len()).filter(|&i| keep[i] && m.aggs[i].file == k).collect();
        let tds: Vec<usize> = (0..m.typedefs.len())
            .filter(|&t| {
                let td = &m.typedefs[t];
                td.file == k && k != FILTERED && !matches!(base_agg(&td.ty), Ty::Agg(i) if !keep[i])
            })
            .collect();
        let its: Vec<Item> = items.iter().filter(|i| i.file == k).map(clone_item).collect();
        let mut header = common.clone();
        header.push_str(&format!("//\n// Licence notice of {label}, verbatim:\n"));
        for l in pp.notices[k].lines() {
            header.push_str(format!("// {l}").trim_end());
            header.push('\n');
        }
        let note = (k == FILTERED).then_some("only the types that drm.h's ioctl numbers refer to; KMS constants are not generated.");
        let mi = emit::ModuleIn { label, module, file: k, header: &header, aggs: &aggs, typedefs: &tds, items: &its, skipped: &skipped[k], scope_note: note };
        files.push((format!("crates/drm/src/uapi/{module}.rs"), emit::module(&m, &mi).into_bytes()));
    }
    let all: Vec<usize> = (0..m.aggs.len()).filter(|&i| keep[i]).collect();
    let (probe_c, table) = probe::render(&m, &all, &items, &common);
    files.push(("crates/drm/tests/data/layout_table.rs".to_string(), table.into_bytes()));
    files.push((PROBE_C.to_string(), probe_c.clone().into_bytes()));
    Ok(Output { files, probe_c, types: all.len(), consts: items.len(), ioctls: items.iter().filter(|i| i.ioctl.is_some()).count() })
}

fn clone_item(i: &Item) -> Item {
    Item {
        name: i.name.clone(),
        val: i.val,
        rust_ty: i.rust_ty,
        ioctl: i.ioctl.clone(),
        ioctl_rust: i.ioctl_rust.clone(),
        enum_name: i.enum_name.clone(),
        file: i.file,
    }
}

fn tok_text(t: &pp::Tok) -> String {
    match t {
        pp::Tok::Id(s) | pp::Tok::Num(s) | pp::Tok::P(s) => s.clone(),
        pp::Tok::Chr(c) => format!("'{}'", *c as u8 as char),
        pp::Tok::Str(s) => format!("\"{s}\""),
    }
}

/// Compiles and runs the probe against the headers under `include`. Ok(None) when no C compiler exists.
pub fn run_probe(probe_c: &str, include: &Path, work: &Path) -> Result<Option<String>, String> {
    std::fs::create_dir_all(work).map_err(|e| format!("{}: {e}", work.display()))?;
    let (src, exe) = (work.join("probe.c"), work.join("probe"));
    std::fs::write(&src, probe_c).map_err(|e| format!("{}: {e}", src.display()))?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let mut cmd = std::process::Command::new(&cc);
    // No -Werror: a newer compiler's new warning must not fail the gate; the printed facts are what is compared.
    cmd.arg("-std=gnu11").arg("-Wall").arg(format!("-I{}", include.join("libdrm").display()));
    if include != Path::new("/usr/include") {
        cmd.arg(format!("-I{}", include.display()));
    }
    let out = match cmd.arg("-o").arg(&exe).arg(&src).output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{cc}: {e}")),
    };
    if !out.status.success() {
        return Err(format!("{cc} failed on the probe:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    let run = std::process::Command::new(&exe).output().map_err(|e| format!("{}: {e}", exe.display()))?;
    if !run.status.success() {
        return Err("probe exited with an error".into());
    }
    String::from_utf8(run.stdout).map(Some).map_err(|e| e.to_string())
}
