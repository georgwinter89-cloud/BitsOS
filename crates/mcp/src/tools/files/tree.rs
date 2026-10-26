//! MC-push: the two tools behind `host/mcp/push.mjs` (a package tree onto the running machine's boot stick):
//! - `manifest` (Read): every entry of a directory tree with type, size and SHA-256 per file, in byte order of
//!   the relative path, in pages (`after`) so a big tree never makes one huge answer;
//! - `move_tree` (DeleteOverwrite): the commit of a push: the files of a staging tree moved over the same
//!   relative paths of a target tree (one rename each, so every file is either the old or the new one), the
//!   listed `remove` paths deleted, empty directories they leave pruned, the staging tree removed, everything
//!   written back. Re-running it after an interruption finishes the same work (already moved files are no
//!   longer in the staging tree, already removed paths count as gone).
//!
//! Both keep to the module's rules (`place`, `dir_refusal`: tmpfs, or the stick below `\BITSOS2\` without SYS,
//! CONFIG.TXT, STICK.ID) and never cross a mount. Into and out of `\BITSOS2\home\` is a move like any other
//! (MC-push2): fs-fat gives a renamed node and its subtree the owner and mode of the new place at once
//! (`FatOps::reowner`, INT-W7); only the home itself is never moved away.

use super::{Kind, PATH_MAX, canonical, dir_refusal, fail, fat_fold, in_stick_home, is_stick_home, kind_name, kind_of, make_parents, path_arg, place, policy_path, refuse, utf8};
use crate::sha256::{Sha256, hex};
use crate::tools::{bad, opt_bool, opt_str};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ksync::SpinLock;
use uapi::errno::*;
use vfs::{Ctx, MountNs, Path};

/// One read buffer while hashing (a bound on memory, not a count).
const CHUNK: usize = 64 * 1024;
/// About this many bytes of JSON per `manifest` answer (a bound on one answer, not a count: the rest follows
/// with `after`).
pub const ANSWER_BUDGET: usize = 1 << 20;
/// About this many file bytes hashed per `manifest` answer (a bound on one call's time, not a count: the rest
/// follows with `after`; one file bigger than it is still hashed whole, in its own page).
pub const HASH_BUDGET: u64 = 32 << 20;
/// How often a long hash run reports progress (`output` shows it once the caller got "Still running").
const REPORT_EVERY: u64 = 32 << 20;

fn lookup_nofollow(ns: &MountNs, path: &str) -> Result<Option<Path>, ToolError> {
    match Ctx::kernel(ns).lookup(None, path.as_bytes(), false) {
        Ok(p) => Ok(Some(p)),
        // A file where a directory of the path should be: nothing is there either.
        Err(ENOENT | ENOTDIR) => Ok(None),
        Err(e) => Err(fail(path, e)),
    }
}

/// Every entry below `root` (relative path, node), sorted by the relative path's bytes; mounts inside are
/// listed as such and not entered. Iterative (a deep tree cannot exhaust the stack); a tree too big for memory
/// is ENOMEM, not a panic, and a stop request ends the walk.
fn walk(ctx: &ToolContext, ns: &MountNs, root: &Path) -> Result<Vec<(String, Path, bool)>, ToolError> {
    let nomem = || ToolError::Failed(String::from("the tree has more entries than memory holds (ENOMEM)"));
    let mut out: Vec<(String, Path, bool)> = Vec::new();
    let mut stack: Vec<(String, Path)> = vec![(String::new(), root.clone())];
    while let Some((rel, dir)) = stack.pop() {
        if ctx.stop_requested() {
            return Err(ToolError::Failed(format!("stopped while listing the tree after {} entries", out.len())));
        }
        let kids = dir.dentry.children_from(0, usize::MAX);
        out.try_reserve(kids.len()).map_err(|_| nomem())?;
        for (_, d) in kids {
            let p = Path { mnt: dir.mnt.clone(), dentry: d };
            let name = utf8(&p.dentry.name());
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let mount = ns.is_mountpoint(&p);
            if p.dentry.is_dir() && !mount {
                stack.try_reserve(1).map_err(|_| nomem())?;
                stack.push((r.clone(), p.clone()));
            }
            out.push((r, p, mount));
        }
    }
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(out)
}

fn sha256_of(ctx: &ToolContext, p: &Path, size: u64, done: &mut u64, next_report: &mut u64) -> Result<(String, u64), ToolError> {
    let f = vfs::node::file_of(&p.dentry);
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(CHUNK).map_err(|_| ToolError::Failed(String::from("no memory for the hash buffer (ENOMEM)")))?;
    buf.resize(CHUNK, 0);
    let mut h = Sha256::new();
    let mut at = 0u64;
    loop {
        if ctx.stop_requested() {
            return Err(ToolError::Failed(format!("stopped while hashing at byte {at}")));
        }
        let n = f.read_at(at, &mut buf).map_err(|e| ToolError::Failed(format!("read at {at}: {} (errno {e})", super::errno_name(e))))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        at += n as u64;
        *done += n as u64;
        if *done >= *next_report {
            *next_report += REPORT_EVERY;
            ctx.output(&format!("manifest: {} MiB hashed\n", *done >> 20));
        }
        // B4: a file that grows while it is read (or claims less than it has) is read to its size at the start
        // plus one chunk, never for ever.
        if at > size.saturating_add(CHUNK as u64) {
            return Err(ToolError::Failed(format!("{} grows while it is hashed", at)));
        }
    }
    Ok((hex(&h.finish()), at))
}

fn manifest(ctx: &ToolContext, args: &Json) -> ToolResult {
    let path = path_arg(args, "path")?;
    let want_hash = match opt_str(args, "hash")?.unwrap_or("sha256") {
        "sha256" => true,
        "none" => false,
        _ => return Err(ToolError::BadArgument(String::from("hash (\"sha256\" or \"none\")"))),
    };
    let after = opt_str(args, "after")?;
    let missing_ok = opt_bool(args, "missing_ok")?.unwrap_or(false);
    let only = opt_str(args, "only")?;
    if only.is_some_and(|o| !rel_ok(o)) {
        return Err(ToolError::BadArgument(String::from("only (a relative path: no leading /, no empty, . or .. parts)")));
    }
    let ns = vfs::root_ns();
    let start = ctx.now_ns();
    let root = match Ctx::kernel(&ns).lookup(None, path.as_bytes(), true) {
        Ok(p) => p,
        Err(ENOENT) if missing_ok => {
            policy_path(ctx, "manifest", path, false, false)?;
            return Ok(vec![ToolOutput::Json(Json::obj(vec![("path", Json::str(path)), ("exists", Json::Bool(false)), ("entries", Json::Arr(Vec::new())), ("next", Json::Null)]))]);
        }
        Err(e) => return Err(fail(path, e)),
    };
    let canon = canonical(&ns, &root);
    policy_path(ctx, "manifest", &canon, false, false)?;
    if !root.dentry.is_dir() {
        return Err(fail(path, ENOTDIR));
    }
    let mut all = walk(ctx, &ns, &root)?;
    // MC-push2 (push.mjs --file): one entry of the tree, by FAT's name rule on the stick.
    if let Some(o) = only {
        let fold = |r: &str| -> String { r.split('/').map(|c| fat_fold(c.as_bytes())).collect::<Vec<_>>().join("/") };
        let vfat = root.dentry.sb.fstype == "vfat";
        all.retain(|(r, _, _)| if vfat { fold(r) == fold(o) } else { r == o });
    }
    let total_files = all.iter().filter(|(_, p, m)| !m && kind_of(p) == Kind::Regular).count();
    let total_bytes: u64 = all.iter().filter(|(_, p, m)| !m && kind_of(p) == Kind::Regular).map(|(_, p, _)| p.attr().size).sum();
    let from = match after {
        Some(a) => all.partition_point(|(r, _, _)| r.as_bytes() <= a.as_bytes()),
        None => 0,
    };
    let mut entries = Vec::new();
    let mut used = 0usize;
    let mut hashed = 0u64;
    let mut next_report = REPORT_EVERY;
    let mut next = Json::Null;
    for (i, (rel, p, mount)) in all.iter().enumerate().skip(from) {
        // A page ends at about ANSWER_BUDGET bytes of JSON or HASH_BUDGET hashed bytes (always after one entry),
        // so one call of a tree of big files stays short and a timeout never hits the same page again and again.
        if (used >= ANSWER_BUDGET || hashed >= HASH_BUDGET) && i > from {
            next = Json::str(&all[i - 1].0);
            break;
        }
        let k = kind_of(p);
        let a = p.attr();
        let mut f = vec![("path", Json::str(rel))];
        if *mount {
            f.push(("type", Json::str("mount point")));
        } else {
            f.push(("type", Json::str(kind_name(k))));
            if k == Kind::Regular {
                f.push(("size", Json::Num(a.size as f64)));
                if want_hash {
                    let (h, n) = sha256_of(ctx, p, a.size, &mut hashed, &mut next_report).map_err(|e| match e {
                        ToolError::Failed(t) => ToolError::Failed(format!("{canon}/{rel}: {t}")),
                        e => e,
                    })?;
                    if n != a.size {
                        return Err(ToolError::Failed(format!("{canon}/{rel}: {n} bytes read, size {} (changed while hashed)", a.size)));
                    }
                    f.push(("sha256", Json::Str(h)));
                }
            }
        }
        used += rel.len() + 110;
        entries.push(Json::obj(f));
    }
    let ms = ctx.now_ns().saturating_sub(start) / 1_000_000;
    klog::event!(Debug, "mcp", "manifest", path = canon.as_str(), entries = entries.len(), hashed = hashed, ms = ms);
    Ok(vec![ToolOutput::Json(Json::obj(vec![
        ("path", Json::str(&canon)),
        ("exists", Json::Bool(true)),
        ("fs", Json::str(root.dentry.sb.fstype)),
        ("files", Json::Num(total_files as f64)),
        ("bytes", Json::Num(total_bytes as f64)),
        ("hashed_bytes", Json::Num(hashed as f64)),
        ("ms", Json::Num(ms as f64)),
        ("entries", Json::Arr(entries)),
        ("next", next),
    ]))])
}

mcp_tool!(
    name: "manifest",
    owner: "MC",
    safety: Read,
    description: "Lists a directory tree of the running BitsOS as JSON for comparing it with a copy elsewhere (host/mcp/push.mjs): every entry below path with its relative path, type, size and the SHA-256 of each file (hash:\"none\" leaves the hashes out), sorted by the relative path's bytes, mounts not entered. A big tree comes in pages (about 1 MB of answer or 32 MiB hashed each, at least one entry): when next is not null, call again with after=next. missing_ok:true answers exists:false for a missing path instead of an error. only:\"<relative path>\" answers just that one entry (none when it is missing; FAT's case rule on the stick). Hashing reads every file (the boot stick: about its read speed); pass timeout_ms for big trees.",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"absolute path of a directory"},"hash":{"type":"string","enum":["sha256","none"],"description":"default sha256"},"after":{"type":"string","description":"continue after this relative path (the next of the previous answer)"},"missing_ok":{"type":"boolean","description":"a missing path answers exists:false"},"only":{"type":"string","description":"just this relative path's entry"},"timeout_ms":{"type":"integer","default":10000}},"required":["path"]}"#,
    handler: manifest,
);

/// A relative path of `remove`: components that are not empty, ".", "..", no NUL, at most PATH_MAX.
pub fn rel_ok(r: &str) -> bool {
    !r.is_empty() && r.len() <= PATH_MAX && !r.starts_with('/') && !r.bytes().any(|b| b == 0) && r.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Whether canonical path `p` is `root` or below it (ASCII case folded: FAT does not tell case apart, and
/// refusing a tmpfs pair that only differs in case is harmless).
pub fn within(p: &str, root: &str) -> bool {
    let (p, root) = (p.to_ascii_lowercase(), root.trim_end_matches('/').to_ascii_lowercase());
    p == root || (p.starts_with(&root) && p.as_bytes().get(root.len()) == Some(&b'/'))
}

/// Whether one canonical path is the other or below it.
pub fn overlaps(a: &str, b: &str) -> bool {
    within(a, b) || within(b, a)
}

/// How long one write-back of `move_tree` may take (B4: a hung or pulled stick must not hold the call for
/// ever; generous, the staged files went out in the background while they were fetched).
const SYNC_DEADLINE_NS: u64 = 15 * 60 * 1_000_000_000;

/// Every file and file system written back (`vfs::writeback::sync_all`, bounded). -> objects flushed.
fn write_back(when: &str) -> Result<usize, ToolError> {
    use crate::bounded::Bounded;
    // The host build (tests) has no kernel threads for `bounded::run`; its tmpfs writes back nothing.
    #[cfg(not(target_os = "none"))]
    let r = Bounded::Done(vfs::writeback::sync_all());
    #[cfg(target_os = "none")]
    let r = crate::bounded::run("mcp-tree-sync", SYNC_DEADLINE_NS, vfs::writeback::sync_all);
    match r {
        Bounded::Done(p) if p.failed == 0 => Ok(p.flushed),
        Bounded::Done(p) => Err(ToolError::Failed(format!("the write-back {when} the move failed for {} files or file systems (the stick?)", p.failed))),
        Bounded::TimedOut => Err(ToolError::Failed(format!("the write-back {when} the move did not finish within {} min (a hung or pulled stick?)", SYNC_DEADLINE_NS / 60_000_000_000))),
        Bounded::NoThread | Bounded::Busy => Err(ToolError::Failed(format!("no thread for the write-back {when} the move"))),
    }
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs_whole: u64,
    replaced: u64,
    removed: u64,
    gone: u64,
    pruned: u64,
}

fn errno_at(path: &str) -> impl Fn(i64) -> ToolError + '_ {
    move |e| fail(path, e)
}

/// The staged tree `from` moved over `to` (both existing directories on one mount), depth first.
fn merge(ctx: &ToolContext, ns: &MountNs, from: &Path, to: &Path, from_canon: &str, st: &mut Stats) -> Result<(), ToolError> {
    let c = Ctx::kernel(ns);
    let w = c.walker();
    let mut stack: Vec<(Path, Path, String)> = vec![(from.clone(), to.clone(), String::new())];
    while let Some((fd, td, rel)) = stack.pop() {
        for (_, d) in fd.dentry.children_from(0, usize::MAX) {
            if ctx.stop_requested() {
                return Err(ToolError::Failed(format!(
                    "stopped after {} files; the rest is still in {from_canon}: move_tree again finishes it",
                    st.files
                )));
            }
            let name = d.name();
            let child = Path { mnt: fd.mnt.clone(), dentry: d };
            let r = if rel.is_empty() { utf8(&name) } else { format!("{rel}/{}", utf8(&name)) };
            let at = format!("{from_canon}/{r}");
            if ns.is_mountpoint(&child) {
                return Err(refuse("move_tree", &at, format!("{at} is a mount point")));
            }
            let target = w.child(&td, &name).map_err(errno_at(&at))?;
            if let Some(t) = &target {
                if ns.is_mountpoint(t) || t.is_mount_root() {
                    return Err(refuse("move_tree", &at, format!("the target of {r} is a mount point")));
                }
            }
            match (child.dentry.is_dir(), target) {
                (true, Some(t)) if t.dentry.is_dir() => {
                    stack.push((child, t, r));
                    continue;
                }
                (is_dir, Some(t)) if t.dentry.is_dir() != is_dir || kind_of(&t) == Kind::Symlink => {
                    // A type change (file <-> directory): the old node goes first.
                    let tc = canonical(ns, &t);
                    let gone = if t.dentry.is_dir() { ns.remove_tree(&tc) } else { c.unlink(Some(&td), &name) };
                    gone.map_err(errno_at(&tc))?;
                    st.replaced += 1;
                }
                (false, Some(_)) => st.replaced += 1,
                _ => {}
            }
            c.rename(Some(&fd), &name, Some(&td), &name, 0).map_err(errno_at(&at))?;
            if child.dentry.is_dir() { st.dirs_whole += 1 } else { st.files += 1 }
        }
    }
    Ok(())
}

/// Deletes `to/<rel>` (ENOENT counts as gone) and then every directory above it inside `to` that is empty now.
fn remove_one(ns: &MountNs, to_canon: &str, rel: &str, st: &mut Stats) -> Result<(), ToolError> {
    let c = Ctx::kernel(ns);
    let abs = format!("{to_canon}/{rel}");
    let (parent_rel, name) = match rel.rsplit_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, rel),
    };
    let parent_abs = match parent_rel {
        Some(p) => format!("{to_canon}/{p}"),
        None => String::from(to_canon),
    };
    // A missing directory, or one that became a file in the move, holds nothing to remove any more.
    let Some(parent) = lookup_nofollow(ns, &parent_abs)?.filter(|p| p.dentry.is_dir()) else {
        st.gone += 1;
        return Ok(());
    };
    // A link on the way must not lead the delete out of the target tree.
    let pc = canonical(ns, &parent);
    if !within(&pc, to_canon) {
        return Err(refuse("move_tree", &abs, format!("remove {rel}: its directory resolves to {pc}, outside {to_canon}")));
    }
    if let Some(why) = dir_refusal(ns, &parent, name.as_bytes()) {
        return Err(refuse("move_tree", &abs, why));
    }
    match c.walker().child(&parent, name.as_bytes()).map_err(errno_at(&abs))? {
        None => st.gone += 1,
        Some(t) => {
            if ns.is_mountpoint(&t) || t.is_mount_root() {
                return Err(refuse("move_tree", &abs, format!("{abs} is a mount point")));
            }
            let gone = if t.dentry.is_dir() { ns.remove_tree(&abs) } else { c.unlink(Some(&parent), name.as_bytes()) };
            gone.map_err(errno_at(&abs))?;
            st.removed += 1;
        }
    }
    // Prune: the directories above, deepest first, while empty (never `to` itself).
    let mut cur = parent_rel;
    while let Some(p) = cur {
        let d = format!("{to_canon}/{p}");
        match lookup_nofollow(ns, &d)? {
            Some(dp) if dp.dentry.is_dir() && dp.dentry.child_count() == 0 => {
                c.rmdir(None, d.as_bytes()).map_err(errno_at(&d))?;
                st.pruned += 1;
            }
            _ => break,
        }
        cur = p.rsplit_once('/').map(|(a, _)| a);
    }
    Ok(())
}

/// The trees `move_tree` calls are working on now (canonical `from`, `to` as given, ASCII lower case). A caller
/// that got "Still running" and calls again (or a second push) must not rename in the same trees while the
/// first call still does: its renames would meet ENOENT halfway, or a remove_tree race them. Not a lock held
/// while waiting: a second call on an overlapping tree is answered EBUSY at once; other trees go on.
static MOVING: SpinLock<Vec<(String, String)>> = SpinLock::named("mcp.move_tree", Vec::new());

/// The claim of one `move_tree` call on its two trees, given back on every way out.
pub(super) struct Moving(String, String);

impl Moving {
    pub(super) fn take(from: &str, to: &str) -> Result<Moving, ToolError> {
        let key = |p: &str| p.trim_end_matches('/').to_ascii_lowercase();
        let (f, t) = (key(from), key(to));
        let mut m = MOVING.lock();
        if m.iter().any(|(a, b)| [a, b].iter().any(|x| overlaps(x, &f) || overlaps(x, &t))) {
            klog::event!(Info, "mcp", "tree.move_busy", from = from, to = to);
            return Err(ToolError::Failed(format!(
                "another move_tree is still working on {from} or {to} (EBUSY): wait for it (output with its call number) and run again"
            )));
        }
        m.try_reserve(1).map_err(|_| ToolError::Failed(String::from("no memory for the move_tree claim (ENOMEM)")))?;
        m.push((f.clone(), t.clone()));
        Ok(Moving(f, t))
    }
}

impl Drop for Moving {
    fn drop(&mut self) {
        let mut m = MOVING.lock();
        if let Some(i) = m.iter().position(|(a, b)| *a == self.0 && *b == self.1) {
            m.swap_remove(i);
        }
    }
}

fn move_tree(ctx: &ToolContext, args: &Json) -> ToolResult {
    let r = move_tree_inner(ctx, args);
    if let Err(ToolError::Failed(why)) = &r {
        let from = args.get("from").and_then(Json::as_str).unwrap_or("?");
        klog::event!(Warn, "mcp", "tree.move_failed", from = from, reason = why.as_str());
    }
    r
}

fn home_refused(path: &str) -> ToolError {
    refuse(
        "move_tree",
        path,
        String::from("/boot/BITSOS2/home itself is never moved: it is the user's home (what is in it may move, into and out of it)"),
    )
}

/// Whether a move crosses the user's home ("in", "out" or "no", for klog) and the note for the answer (fs-fat
/// gave the moved nodes the new place's owner and mode).
fn home_note(on_stick: bool, from: &str, to: &str) -> (&'static str, &'static str) {
    match (on_stick && in_stick_home(from), on_stick && in_stick_home(to)) {
        (false, true) => ("in", " (into the user's home: uid/gid 1000, directories 0700, files 0600)"),
        (true, false) => ("out", " (out of the user's home: root's, 0755)"),
        _ => ("no", ""),
    }
}

fn move_tree_inner(ctx: &ToolContext, args: &Json) -> ToolResult {
    let from = path_arg(args, "from")?;
    let to = path_arg(args, "to")?;
    let remove: Vec<&str> = match args.get("remove") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Arr(a)) => {
            let v: Option<Vec<&str>> = a.iter().map(|x| x.as_str().filter(|r| rel_ok(r))).collect();
            v.ok_or_else(|| ToolError::BadArgument(String::from("remove (relative paths: no leading /, no empty, . or .. parts)")))?
        }
        Some(_) => return Err(bad("remove")),
    };
    // The home itself by the path as given (also when it is missing: a refusal, never "nothing to move"), by the
    // canonical path again below.
    if is_stick_home(from) {
        return Err(home_refused(from));
    }
    let ns = vfs::root_ns();
    let start = ctx.now_ns();
    // Both ends under the module's rules: `place` checks the directory each name sits in (and that neither is a
    // mount point or a top-level entry of /).
    let src = match lookup_nofollow(&ns, from)? {
        Some(_) => {
            let pl = place(&ns, "move_tree", from, true)?;
            let p = pl.existing.clone().ok_or_else(|| fail(from, ENOENT))?;
            if !p.dentry.is_dir() {
                return Err(fail(from, ENOTDIR));
            }
            if p.dentry.sb.fstype == "vfat" && is_stick_home(&pl.canon) {
                return Err(home_refused(&pl.canon));
            }
            Some((pl, p))
        }
        None => None,
    };
    // The claim before anything changes (make_parents included); `from` by its canonical path when it is there.
    let _moving = Moving::take(src.as_ref().map_or(from, |(spl, _)| spl.canon.as_str()), to)?;
    let dst_exists = lookup_nofollow(&ns, to)?.is_some();
    if src.is_some() && !dst_exists {
        make_parents(ctx, &ns, "move_tree", to)?;
    }
    let dpl = place(&ns, "move_tree", to, dst_exists)?;
    if let Some(t) = &dpl.existing
        && !t.dentry.is_dir()
    {
        return Err(fail(to, ENOTDIR));
    }
    let on_stick = dpl.dir.dentry.sb.fstype == "vfat";
    if let Some((spl, _)) = &src {
        if overlaps(&spl.canon, &dpl.canon) {
            return Err(refuse("move_tree", from, format!("{} and {} are one inside the other (EINVAL)", spl.canon, dpl.canon)));
        }
        if !alloc::sync::Arc::ptr_eq(&spl.dir.mnt, &dpl.dir.mnt) {
            return Err(fail(to, EXDEV));
        }
        policy_path(ctx, "move_tree", &spl.canon, true, false)?;
    }
    policy_path(ctx, "move_tree", &dpl.canon, true, !dst_exists)?;
    // Crash-safe order: the staged data is on the disk before any entry of the target points at it.
    write_back("before")?;
    let mut st = Stats::default();
    let (home, note) = match &src {
        Some((spl, _)) => home_note(on_stick, &spl.canon, &dpl.canon),
        None => ("no", ""),
    };
    let how = match (&src, &dpl.existing) {
        (None, _) => format!("nothing left to move in {from}"),
        (Some((spl, _)), None) => {
            let c = Ctx::kernel(&ns);
            c.rename(Some(&spl.dir), &spl.name, Some(&dpl.dir), &dpl.name, vfs::abi::RENAME_NOREPLACE).map_err(errno_at(to))?;
            st.dirs_whole = 1;
            format!("{} moved whole to {} (the target was missing){note}", spl.canon, dpl.canon)
        }
        (Some((spl, sp)), Some(tp)) => {
            merge(ctx, &ns, sp, tp, &spl.canon, &mut st)?;
            ns.remove_tree(&spl.canon).map_err(errno_at(&spl.canon))?;
            format!("{} merged into {} and removed{note}", spl.canon, dpl.canon)
        }
    };
    if !remove.is_empty() && lookup_nofollow(&ns, &dpl.canon)?.is_some() {
        for r in &remove {
            if ctx.stop_requested() {
                return Err(ToolError::Failed(format!("stopped after {} of {} removals: move_tree again finishes them", st.removed + st.gone, remove.len())));
            }
            remove_one(&ns, &dpl.canon, r, &mut st)?;
        }
    } else {
        st.gone += remove.len() as u64;
    }
    let flushed = write_back("after")?;
    let ms = ctx.now_ns().saturating_sub(start) / 1_000_000;
    klog::event!(
        Info, "mcp", "tree.moved",
        from = from, to = dpl.canon.as_str(), files = st.files, dirs = st.dirs_whole, replaced = st.replaced, removed = st.removed, gone = st.gone, home = home, ms = ms
    );
    Ok(vec![ToolOutput::Text(format!(
        "move_tree: {how}: {} files and {} directories moved ({} replaced), {} removed ({} already gone), {} empty directories pruned; written back ({flushed} flushed) in {ms} ms",
        st.files, st.dirs_whole, st.replaced, st.removed, st.gone, st.pruned
    ))])
}

mcp_tool!(
    name: "move_tree",
    owner: "MC",
    safety: DeleteOverwrite,
    description: "The commit of host/mcp/push.mjs: moves every file of the directory from over the same relative path below the directory to (one rename each: each file is the old or the new one, a running program keeps the old one; a missing to is made by moving from whole), deletes the relative paths in remove below to and the directories they leave empty, removes from, and writes everything back. Running it again after an interruption finishes the same work; a second move_tree on an overlapping tree while one runs is answered EBUSY at once. The places are those write_file may write (tmpfs, the boot stick below /boot/BITSOS2/ without SYS, CONFIG.TXT, STICK.ID; into and out of /boot/BITSOS2/home the moved files take the owner and mode of the new place, the home itself is never moved), from and to on one file system and not one inside the other.",
    schema: r#"{"type":"object","properties":{"from":{"type":"string","description":"absolute path of the staged tree"},"to":{"type":"string","description":"absolute path of the target tree"},"remove":{"type":"array","items":{"type":"string"},"description":"paths relative to to that are deleted"},"timeout_ms":{"type":"integer","default":10000}},"required":["from","to"]}"#,
    handler: move_tree,
);
