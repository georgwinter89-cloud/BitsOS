//! The tools that change files: `write_file` and `delete`, and the R8a rule for `CONFIG.TXT`.

use super::{Kind, Place, fail, kind_name, kind_of, make_parents, path_arg, perm_arg, place, policy_path, refuse, temp_name, write_all_at};
use crate::tools::{bad, number, opt_bool, opt_str};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, b64, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::abi::{O_APPEND, O_CREAT, O_EXCL, O_NOFOLLOW, O_WRONLY};
use vfs::{Ctx, MountNs};

/// Keys only Georg's own edit may add or change (R8a `nvme_write`, `stick_write`, `usb_msd_write` (FS-6, US-4)).
const GUARDED_KEYS: &[&str] = &["nvme_write", "stick_write", "usb_msd_write"];

/// How a write changes a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The whole file becomes the data.
    Replace,
    /// The data goes at this offset (the rest stays; a gap past the end reads as zeros).
    At(u64),
    Append,
}

/// The file content after `change` (pure; the CONFIG.TXT check needs the result before writing).
pub fn apply(old: &[u8], data: &[u8], change: Change) -> Vec<u8> {
    match change {
        Change::Replace => data.to_vec(),
        Change::Append => [old, data].concat(),
        Change::At(o) => {
            let o = o as usize;
            let mut v = old.to_vec();
            if v.len() < o + data.len() {
                v.resize(o + data.len(), 0);
            }
            v[o..o + data.len()].copy_from_slice(data);
            v
        }
    }
}

fn is_config_name(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"CONFIG.TXT")
}

/// The guarded lines of a CONFIG.TXT text (key lower-case, value trimmed), sorted.
fn guarded_lines(text: &[u8]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = text
        .split(|&b| b == b'\n')
        .filter_map(|l| {
            let l = String::from_utf8_lossy(l);
            let l = l.trim().trim_start_matches('\u{feff}');
            let (k, val) = l.split_once('=')?;
            let k = k.trim().to_ascii_lowercase();
            GUARDED_KEYS.contains(&k.as_str()).then(|| (k, String::from(val.trim())))
        })
        .collect();
    v.sort();
    v
}

/// R8a: a write may leave the `nvme_write=`/`stick_write=`/`usb_msd_write=` lines of a CONFIG.TXT exactly as they
/// are, or drop them, but never add or change one (only Georg's own edit turns them on). Err names the line.
pub fn config_guard(old: &[u8], new: &[u8]) -> Result<(), String> {
    let before = guarded_lines(old);
    for line in guarded_lines(new) {
        if !before.contains(&line) {
            return Err(format!(
                "CONFIG.TXT: the line {}={} would be added or changed; nvme_write, stick_write and usb_msd_write are set only by Georg himself (R8a)",
                line.0, line.1
            ));
        }
    }
    // FS-7 CR10 (INT-A2a): the kernel and bitsboot read a key's LAST line; a write that keeps every line but
    // reorders or drops them can still change that value (nvme_write=on above nvme_write=off). FS-7's guard checks it.
    if let Err(k) = fs_fat::protect::config_guard(old, new) {
        return Err(format!(
            "CONFIG.TXT: the value of {k} that counts (its last line) would change; nvme_write, stick_write and usb_msd_write are set only by Georg himself (R8a)"
        ));
    }
    Ok(())
}

/// The whole current content of an existing node (for the CONFIG.TXT check; such files are small).
fn content_of(pl: &Place) -> Result<Vec<u8>, ToolError> {
    match &pl.existing {
        None => Ok(Vec::new()),
        Some(p) => {
            let size = vfs::node::file_of(&p.dentry).size();
            super::read::read_range(p, 0, size).map_err(|e| fail(&pl.canon, e))
        }
    }
}

/// A whole-file write: a hidden sibling, filled, then renamed over the name (the old inode lives on for whoever
/// still has it open). Returns the new file's mode.
pub(crate) fn replace_with(ns: &MountNs, pl: &Place, perm: u32, fill: &mut dyn FnMut(&vfs::OpenFile) -> Result<(), ToolError>) -> Result<(), ToolError> {
    let c = Ctx::kernel(ns);
    let tmp = temp_name(&pl.name);
    let f = c.open(Some(&pl.dir), &tmp, O_WRONLY | O_CREAT | O_EXCL, perm).map_err(|e| fail(&pl.canon, e))?;
    let r = fill(&f).and_then(|()| c.rename(Some(&pl.dir), &tmp, Some(&pl.dir), &pl.name, 0).map_err(|e| fail(&pl.canon, e)));
    if r.is_err() {
        let _ = c.unlink(Some(&pl.dir), &tmp);
    }
    r
}

fn data_arg(args: &Json) -> Result<Vec<u8>, ToolError> {
    match (opt_str(args, "data")?, opt_str(args, "text")?) {
        (Some(b), None) => b64::decode(b).ok_or_else(|| ToolError::BadArgument(String::from("data (not valid base64)"))),
        (None, Some(t)) => Ok(t.as_bytes().to_vec()),
        _ => Err(ToolError::BadArgument(String::from("data (give exactly one of data (base64) and text)"))),
    }
}

/// A failed change is an event too (B6); refusals have their own (`file.refused`), bad arguments none.
fn logged(tool: &str, args: &Json, r: ToolResult) -> ToolResult {
    if let Err(ToolError::Failed(why)) = &r {
        let path = args.get("path").and_then(Json::as_str).unwrap_or("?");
        klog::event!(Warn, "mcp", "file.change_failed", tool = tool, path = path, reason = why.as_str());
    }
    r
}

fn write_file(ctx: &ToolContext, args: &Json) -> ToolResult {
    logged("write_file", args, write_file_inner(ctx, args))
}

fn write_file_inner(ctx: &ToolContext, args: &Json) -> ToolResult {
    let path = path_arg(args, "path")?;
    let data = data_arg(args)?;
    let change = match (number(args, "offset")?, opt_bool(args, "append")?.unwrap_or(false)) {
        (Some(_), true) => return Err(ToolError::BadArgument(String::from("offset (offset and append exclude each other)"))),
        (Some(o), false) => Change::At(o),
        (None, true) => Change::Append,
        (None, false) => Change::Replace,
    };
    if let Change::At(o) = change
        && o.checked_add(data.len() as u64).is_none_or(|end| end > i64::MAX as u64)
    {
        return Err(bad("offset"));
    }
    let create = opt_bool(args, "create")?.unwrap_or(true);
    let parents = opt_bool(args, "parents")?.unwrap_or(false);
    let perm = perm_arg(args, "mode")?;
    let ns = vfs::root_ns();
    if parents && create {
        make_parents(ctx, &ns, "write_file", path)?;
    }
    let pl = place(&ns, "write_file", path, false)?;
    if !create && pl.existing.is_none() {
        return Err(fail(path, ENOENT));
    }
    policy_path(ctx, "write_file", &pl.canon, true, pl.existing.is_none())?;
    if is_config_name(&pl.name) {
        let old = content_of(&pl)?;
        if let Change::At(o) = change
            && o > old.len() as u64 + (1 << 20)
        {
            return Err(refuse("write_file", path, String::from("CONFIG.TXT: an offset far past its end")));
        }
        config_guard(&old, &apply(&old, &data, change)).map_err(|why| refuse("write_file", path, why))?;
    }
    let new = pl.existing.is_none();
    let old_perm = pl.existing.as_ref().map(|p| p.dentry.inode.meta.mode() & 0o7777);
    let how = match change {
        Change::Replace => {
            let mode = perm.or(old_perm).unwrap_or(0o644);
            replace_with(&ns, &pl, mode, &mut |f| write_all_at(f, 0, &data).map_err(|e| fail(&pl.canon, e)))?;
            String::from(if new { "new file" } else { "replaced the file" })
        }
        Change::At(_) | Change::Append => {
            let c = Ctx::kernel(&ns);
            // O_NOFOLLOW: `place` saw a regular file; a link put there since is not followed (ELOOP).
            let flags = O_WRONLY | O_NOFOLLOW | if create { O_CREAT } else { 0 } | if change == Change::Append { O_APPEND } else { 0 };
            let f = c.open(Some(&pl.dir), &pl.name, flags, perm.unwrap_or(0o644)).map_err(|e| fail(path, e))?;
            match change {
                Change::At(o) => write_all_at(&f, o, &data).map_err(|e| fail(path, e))?,
                _ => {
                    let mut d = &data[..];
                    while !d.is_empty() {
                        let n = f.write(d).map_err(|e| fail(path, e))?;
                        if n == 0 {
                            return Err(fail(path, ENOSPC));
                        }
                        d = &d[n..];
                    }
                }
            }
            if let (Some(p), false) = (perm, new) {
                f.inode().inspect(|i| i.meta.set_perm(p));
            }
            match change {
                Change::At(o) => format!("at offset {o}{}", if new { ", new file" } else { "" }),
                _ => String::from(if new { "appended, new file" } else { "appended" }),
            }
        }
    };
    let (size, mode) = Ctx::kernel(&ns)
        .lookup(Some(&pl.dir), &pl.name, false)
        .map(|p| (p.attr().size, p.attr().mode & 0o7777))
        .map_err(|e| fail(path, e))?;
    klog::event!(Info, "mcp", "file.write", path = pl.canon.as_str(), bytes = data.len(), how = how.as_str(), size = size);
    Ok(vec![ToolOutput::Text(format!("write_file {}: {} bytes written ({how}); size now {size}, mode {mode:04o}", pl.canon, data.len()))])
}

mcp_tool!(
    name: "write_file",
    owner: "MC",
    safety: Change,
    description: "Writes a file in the RAM file system of the running BitsOS (tmpfs: /tmp, /bin, ...) or on its boot stick below /boot/BITSOS2/ (never SYS, CONFIG.TXT or STICK.ID there; never /dev, /proc, /sys, the rest of the stick or a disk). data (base64) or text; without offset/append the whole file is replaced (written beside it, then renamed over it, so a running program keeps its copy); offset writes there, append at the end. create (default true) makes a missing file with mode (octal text, default 0644; \"0755\" for programs); parents:true makes missing directories. Requests are at most 256 KB: send big files in pieces with offset, or use fetch. A CONFIG.TXT never gains or changes an nvme_write=, stick_write= or usb_msd_write= line (R8a).",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"absolute path"},"data":{"type":"string","description":"the bytes as base64"},"text":{"type":"string","description":"the bytes as UTF-8 text (instead of data)"},"offset":{"type":["integer","string"],"description":"write at this byte (the rest of the file stays)"},"append":{"type":"boolean","description":"write at the end"},"create":{"type":"boolean","description":"make the file if it is missing (default true)"},"mode":{"type":"string","description":"permission bits as octal text for a new file, e.g. \"0755\""},"parents":{"type":"boolean","description":"make missing parent directories"}},"required":["path"]}"#,
    handler: write_file,
);

fn delete(ctx: &ToolContext, args: &Json) -> ToolResult {
    logged("delete", args, delete_inner(ctx, args))
}

fn delete_inner(ctx: &ToolContext, args: &Json) -> ToolResult {
    let path = path_arg(args, "path")?;
    let recursive = opt_bool(args, "recursive")?.unwrap_or(false);
    let ns = vfs::root_ns();
    let pl = place(&ns, "delete", path, true)?;
    let Some(target) = pl.existing.clone() else { return Err(fail(path, ENOENT)) };
    policy_path(ctx, "delete", &pl.canon, true, false)?;
    let k = kind_of(&target);
    let c = Ctx::kernel(&ns);
    let size = target.attr().size;
    let what = match k {
        Kind::Dir if recursive => {
            let children = target.dentry.child_count();
            ns.remove_tree(&pl.canon).map_err(|e| fail(path, e))?;
            format!("directory with {children} entries and everything below them")
        }
        Kind::Dir => {
            c.rmdir(Some(&pl.dir), &pl.name).map_err(|e| match e {
                ENOTEMPTY => ToolError::Failed(format!("{}: directory not empty (ENOTEMPTY); recursive:true removes it with everything below", pl.canon)),
                e => fail(path, e),
            })?;
            String::from("empty directory")
        }
        _ => {
            c.unlink(Some(&pl.dir), &pl.name).map_err(|e| fail(path, e))?;
            format!("{}, {size} bytes", kind_name(k))
        }
    };
    klog::event!(Info, "mcp", "file.deleted", path = pl.canon.as_str(), what = what.as_str());
    Ok(vec![ToolOutput::Text(format!("deleted {} ({what})", pl.canon))])
}

mcp_tool!(
    name: "delete",
    owner: "MC",
    safety: DeleteOverwrite,
    description: "Deletes a file, a symbolic link (not its target) or an empty directory in the RAM file system (tmpfs) of the running BitsOS or on its boot stick below /boot/BITSOS2/ (never SYS, CONFIG.TXT or STICK.ID there); recursive:true removes a directory with everything below it. Never in /dev, /proc, /sys, elsewhere, a mount point or a top-level entry of /.",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"absolute path"},"recursive":{"type":"boolean","description":"remove a directory with everything below it"}},"required":["path"]}"#,
    handler: delete,
);
