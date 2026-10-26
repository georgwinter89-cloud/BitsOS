//! The tools that only look: `read_file`, `list_dir`, `stat`.

use super::{Fnv, Kind, READ_MAX, canonical, fail, kind_name, kind_of, path_arg, policy_path, refuse, utf8};
use crate::tools::{bad, number, opt_bool, opt_str};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, b64, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::{Ctx, Path};

/// One read buffer of `stat`'s hash (a bound on memory, not a count).
const HASH_CHUNK: usize = 64 * 1024;
/// What `stat` hashes without a `hash_max` argument (a bound on one call's work, not a count: the caller can
/// raise it; a longer file is refused, never hashed in part).
pub(crate) const HASH_MAX_DEFAULT: u64 = 256 * 1024 * 1024;

fn lookup(path: &str, follow: bool) -> Result<(alloc::sync::Arc<vfs::MountNs>, Path), ToolError> {
    let ns = vfs::root_ns();
    let p = Ctx::kernel(&ns).lookup(None, path.as_bytes(), follow).map_err(|e| fail(path, e))?;
    Ok((ns, p))
}

/// WL-4 CR 4 (R8 i): the stick's CONFIG.TXT as the file tools show it, every secret value as `***`
/// (`fs_fat::protect::redact_config`; install_kernel's `config` puts the stick's values back for `***`).
fn config_view(canon: &str, p: &Path) -> Option<Result<Vec<u8>, i64>> {
    if !canon.eq_ignore_ascii_case("/boot/BITSOS2/CONFIG.TXT") {
        return None;
    }
    Some(read_range(p, 0, fs_fat::protect::CONFIG_MAX).map(|d| fs_fat::protect::redact_config(&d)))
}

/// Reads up to `want` bytes at `offset` of a file node (stops early at its end).
pub(crate) fn read_range(p: &Path, offset: u64, want: u64) -> Result<Vec<u8>, i64> {
    let f = vfs::node::file_of(&p.dentry);
    let want = usize::try_from(want).map_err(|_| EINVAL)?;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(want).map_err(|_| ENOMEM)?;
    out.resize(want, 0);
    let mut got = 0;
    while got < want {
        let n = f.read_at(offset + got as u64, &mut out[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    out.truncate(got);
    Ok(out)
}

/// MC-4b: a whole regular file for another tool's use (`install_kernel`'s `kernel_path`/`initrd_path`: a bundle
/// uploaded with `write_file`), at most `limit` bytes, under the read rules of `read_file` (B3 path check;
/// device nodes and proc files refused: they are no uploads). A file that changes size while it is read is
/// refused rather than handed on in part.
pub fn read_whole(ctx: &ToolContext, tool: &str, path: &str, limit: u64) -> Result<Vec<u8>, ToolError> {
    if !path.starts_with('/') || path.len() > super::PATH_MAX || path.bytes().any(|b| b == 0) {
        return Err(ToolError::BadArgument(format!("{path} (an absolute path, at most {} bytes, no NUL)", super::PATH_MAX)));
    }
    let (ns, p) = lookup(path, true)?;
    policy_path(ctx, tool, path, false, false)?;
    match kind_of(&p) {
        Kind::Regular => {}
        Kind::Dir => return Err(fail(path, EISDIR)),
        k => return Err(refuse(tool, path, format!("{} is a {}, not a file (upload it with write_file)", canonical(&ns, &p), kind_name(k)))),
    }
    // Size, mtime and ctime before and after the read: a writer meanwhile (write_file, a program) changes at
    // least one of them, also when the size stays the same, so a mix of old and new bytes is never handed on.
    // The caller's hash in the answer (install_kernel names it, install.mjs compares it) is the end-to-end proof.
    let before = stamp(&p);
    let size = before.0;
    if size > limit {
        return Err(refuse(tool, path, format!("{} has {size} bytes, more than this call can hold now ({limit} bytes)", canonical(&ns, &p))));
    }
    // One byte more than the size: a file that grew meanwhile shows it.
    let data = read_range(&p, 0, size + 1).map_err(|e| fail(path, e))?;
    let after = stamp(&p);
    if data.len() as u64 != size || after != before {
        klog::event!(Warn, "mcp", "file.read_whole_changed", tool = tool, path = path, size = size, got = data.len());
        return Err(ToolError::Failed(format!("{}: changed while it was read ({size} bytes, then {}); try again", canonical(&ns, &p), data.len())));
    }
    klog::event!(Info, "mcp", "file.read_whole", tool = tool, path = path, bytes = data.len());
    Ok(data)
}

/// What a write changes: size, mtime, ctime (seconds and nanoseconds).
fn stamp(p: &Path) -> (u64, i64, u32, i64, u32) {
    let a = p.attr();
    (a.size, a.mtime.sec, a.mtime.nsec, a.ctime.sec, a.ctime.nsec)
}

fn read_file(ctx: &ToolContext, args: &Json) -> ToolResult {
    let path = path_arg(args, "path")?;
    let offset = number(args, "offset")?.unwrap_or(0);
    let length = number(args, "length")?;
    if length.is_some_and(|l| l > READ_MAX) {
        return Err(ToolError::BadArgument(format!("length (at most {READ_MAX} per call; read on with offset)")));
    }
    if offset > i64::MAX as u64 {
        return Err(bad("offset"));
    }
    let text = match opt_str(args, "encoding")? {
        None | Some("base64") => false,
        Some("text") => true,
        Some(_) => return Err(bad("encoding")),
    };
    let (ns, p) = lookup(path, true)?;
    policy_path(ctx, "read_file", path, false, false)?;
    match kind_of(&p) {
        Kind::Regular | Kind::Proc => {}
        Kind::Dir => return Err(fail(path, EISDIR)),
        k => {
            return Err(refuse("read_file", path, format!("{} is a {}: reading it could wait or change device state; read_file reads files", canonical(&ns, &p), kind_name(k))));
        }
    }
    let want = length.unwrap_or(READ_MAX);
    let (size, data) = match config_view(&canonical(&ns, &p), &p) {
        Some(view) => {
            let all = view.map_err(|e| fail(path, e))?;
            let from = (offset as usize).min(all.len());
            let to = from.saturating_add(want as usize).min(all.len());
            (all.len() as u64, all[from..to].to_vec())
        }
        None => {
            let size = vfs::node::file_of(&p.dentry).size();
            // A regular file's buffer is no larger than what is left of it (a proc file's size may be stale: full
            // want).
            let room = if kind_of(&p) == Kind::Regular { want.min(size.saturating_sub(offset)) } else { want };
            (size, read_range(&p, offset, room).map_err(|e| fail(path, e))?)
        }
    };
    let end = offset + data.len() as u64;
    // A proc file's size is what it rendered at the last open; its end is where a read gives nothing.
    let eof = (data.len() as u64) < want || end >= size;
    klog::event!(Debug, "mcp", "file.read", path = path, offset = offset, bytes = data.len());
    let mut fields = vec![
        ("path", Json::str(&canonical(&ns, &p))),
        ("size", Json::Num(size as f64)),
        ("offset", Json::Num(offset as f64)),
        ("length", Json::Num(data.len() as f64)),
        ("eof", Json::Bool(eof)),
    ];
    if !eof {
        fields.push(("next_offset", Json::Num(end as f64)));
    }
    if text {
        fields.push(("text", Json::Str(utf8(&data))));
    } else {
        let mut s = String::with_capacity(b64::encoded_len(data.len()));
        b64::encode_into(&data, &mut s);
        fields.push(("data", Json::Str(s)));
    }
    Ok(vec![ToolOutput::Json(Json::obj(fields))])
}

mcp_tool!(
    name: "read_file",
    owner: "MC",
    safety: Read,
    description: "Reads a file of the running BitsOS (its RAM file system: /bin, /tmp, /proc files, ...). Answers JSON {path, size, offset, length, eof, next_offset?, data (base64) or text}. At most 2 MiB per call (length; default: up to 2 MiB from offset); read on with offset = next_offset. Device nodes are refused.",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"absolute path"},"offset":{"type":["integer","string"],"description":"first byte (default 0)"},"length":{"type":["integer","string"],"description":"bytes to read, at most 2097152 (default: that much or to the end)"},"encoding":{"type":"string","enum":["base64","text"],"description":"base64 (default) or text (UTF-8, invalid bytes replaced)"}},"required":["path"]}"#,
    handler: read_file,
);

fn type_char(mode: u32) -> char {
    match mode & uapi::S_IFMT {
        uapi::S_IFDIR => 'd',
        uapi::S_IFLNK => 'l',
        uapi::S_IFCHR => 'c',
        uapi::S_IFIFO => 'p',
        0o060000 => 'b',
        0o140000 => 's',
        _ => '-',
    }
}

fn type_word(mode: u32) -> &'static str {
    match mode & uapi::S_IFMT {
        uapi::S_IFDIR => "dir",
        uapi::S_IFLNK => "symlink",
        uapi::S_IFCHR => "chardev",
        uapi::S_IFIFO => "fifo",
        0o060000 => "blockdev",
        0o140000 => "socket",
        _ => "file",
    }
}

fn list_dir(ctx: &ToolContext, args: &Json) -> ToolResult {
    let path = path_arg(args, "path")?;
    let (ns, p) = lookup(path, true)?;
    if !p.dentry.is_dir() {
        return Err(fail(path, ENOTDIR));
    }
    policy_path(ctx, "list_dir", path, false, false)?;
    let mut entries: Vec<(Vec<u8>, Path)> =
        p.dentry.children_from(0, usize::MAX).into_iter().map(|(_, d)| (d.name(), ns.cross_down(Path { mnt: p.mnt.clone(), dentry: d }))).collect();
    // FS-2: a synthetic directory (/proc, /sys) also has computed names (/proc/<pid>); one that vanished
    // between the listing and its lookup is left out.
    if let vfs::inode::Data::SynthDir(sd) = &p.dentry.inode.data {
        let listed = vfs::synth::list_from(&p.dentry, sd, vfs::synth::DYN_BASE, usize::MAX).map_err(|e| fail(path, e))?;
        for l in listed {
            if let Ok(Some(d)) = vfs::synth::lookup(&p.dentry, sd, &l.name) {
                entries.push((l.name, Path { mnt: p.mnt.clone(), dentry: d }));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let canon = canonical(&ns, &p);
    let mut lines = vec![format!("{canon}: {} entries ({})", entries.len(), p.dentry.sb.fstype)];
    for (name, e) in &entries {
        let a = e.attr();
        let mut l = format!("{}{:04o} {:>5}:{:<5} {:>12} {}", type_char(a.mode), a.mode & 0o7777, a.uid, a.gid, a.size, utf8(name));
        match kind_of(e) {
            Kind::Dir => l.push('/'),
            Kind::Symlink => {
                if let Ok(t) = e.dentry.inode.readlink() {
                    l.push_str(" -> ");
                    l.push_str(&utf8(&t));
                }
            }
            _ => {}
        }
        lines.push(l);
    }
    crate::tools::text(lines)
}

mcp_tool!(
    name: "list_dir",
    owner: "MC",
    safety: Read,
    description: "Lists a directory of the running BitsOS: one line per entry (type+mode, uid:gid, size, name; directories end in /, links show their target), sorted by name.",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"absolute path of a directory, e.g. \"/\" or \"/tmp\""}},"required":["path"]}"#,
    handler: list_dir,
);

fn stat(ctx: &ToolContext, args: &Json) -> ToolResult {
    let path = path_arg(args, "path")?;
    let follow = opt_bool(args, "follow")?.unwrap_or(true);
    let hash = opt_bool(args, "hash")?.unwrap_or(false);
    let hash_max = number(args, "hash_max")?;
    let (ns, p) = lookup(path, follow)?;
    policy_path(ctx, "stat", path, false, false)?;
    let a = p.attr();
    let k = kind_of(&p);
    let mut f = vec![
        ("path", Json::str(&canonical(&ns, &p))),
        ("type", Json::str(type_word(a.mode))),
        ("mode", Json::Str(format!("{:04o}", a.mode & 0o7777))),
        ("size", Json::Num(a.size as f64)),
        ("nlink", Json::Num(a.nlink as f64)),
        ("uid", Json::Num(a.uid as f64)),
        ("gid", Json::Num(a.gid as f64)),
        ("ino", Json::Num(a.ino as f64)),
        ("fs", Json::str(p.dentry.sb.fstype)),
        ("mtime", Json::Str(format!("{}.{:09}", a.mtime.sec, a.mtime.nsec))),
    ];
    if k == Kind::Symlink
        && let Ok(t) = p.dentry.inode.readlink()
    {
        f.push(("target", Json::Str(utf8(&t))));
    }
    if let Some(view) = config_view(&canonical(&ns, &p), &p)
        && k == Kind::Regular
    {
        // The size and hash of what read_file shows (no length or hash of a secret value).
        let view = view.map_err(|e| fail(path, e))?;
        if let Some(s) = f.iter_mut().find(|(n, _)| *n == "size") {
            s.1 = Json::Num(view.len() as f64);
        }
        f.push(("redacted", Json::Bool(true)));
        if hash {
            let mut h = Fnv::new();
            h.update(&view);
            f.push(("fnv1a64", Json::Str(format!("{:#018x}", h.0))));
            f.push(("hashed_bytes", Json::Num(view.len() as f64)));
        }
        return Ok(vec![ToolOutput::Json(Json::obj(f))]);
    }
    if hash {
        if !matches!(k, Kind::Regular | Kind::Proc) {
            return Err(refuse("stat", path, format!("hash: {} is a {}, not a file", canonical(&ns, &p), kind_name(k))));
        }
        // B4: a sparse tmpfs file can claim up to i64::MAX bytes (write_file at a far offset) and a proc file has
        // no size until it reads empty: the hash reads at most hash_max bytes and refuses a longer file.
        let limit = hash_max.unwrap_or(HASH_MAX_DEFAULT);
        let too_long = |what: String| {
            refuse("stat", path, format!("hash: {} {what}, more than hash_max {limit}; pass a larger hash_max to hash it", canonical(&ns, &p)))
        };
        if k == Kind::Regular && a.size > limit {
            return Err(too_long(format!("has {} bytes", a.size)));
        }
        let mut h = Fnv::new();
        let mut at = 0u64;
        loop {
            if ctx.stop_requested() {
                return Err(ToolError::Failed(format!("stopped while hashing at byte {at}")));
            }
            // One byte past the limit tells a file that ends exactly there from one that goes on.
            let want = (HASH_CHUNK as u64).min(limit.saturating_add(1) - at);
            let d = read_range(&p, at, want).map_err(|e| fail(path, e))?;
            if d.is_empty() {
                break;
            }
            h.update(&d);
            at += d.len() as u64;
            if at > limit {
                return Err(too_long(String::from("reads on past its size")));
            }
        }
        f.push(("fnv1a64", Json::Str(format!("{:#018x}", h.0))));
        f.push(("hashed_bytes", Json::Num(at as f64)));
    }
    Ok(vec![ToolOutput::Json(Json::obj(f))])
}

mcp_tool!(
    name: "stat",
    owner: "MC",
    safety: Read,
    description: "Facts about one path of the running BitsOS as JSON: type, mode (octal), size, links, owner, inode, file system, mtime, a link's target. hash:true adds the file's FNV-1a 64 (fnv1a64, the hash reload_info and fetch show) to compare an upload with the original; a file longer than hash_max (default 256 MiB) is refused. follow:false looks at a link itself. For /boot/BITSOS2/CONFIG.TXT size and fnv1a64 are of the redacted view read_file shows (secrets as ***), marked redacted:true.",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"absolute path"},"follow":{"type":"boolean","description":"follow a final symbolic link (default true)"},"hash":{"type":"boolean","description":"also hash the content (FNV-1a 64)"},"hash_max":{"type":["integer","string"],"description":"hash refuses a file longer than this many bytes (default 268435456)"}},"required":["path"]}"#,
    handler: stat,
);
