//! execve and execveat (linux-6.6/fs/exec.c `do_execveat_common`, `bprm_execve`, `do_open_execat` :906,
//! `exec_binprm` :1773, `begin_new_exec` :1250, `de_thread` :1044; fs/binfmt_script.c `load_script`).
//!
//! The new program is loaded completely into a fresh address space, and every other allocation that can fail is
//! made, *before* anything of the caller changes, so every error (ENOENT, EACCES, ENOEXEC, E2BIG, ENOMEM)
//! returns to the old program intact; Linux has a point of no return inside `load_elf_binary` and kills the
//! process for errors after it. The one step left after V2's point (an fd table to unshare that became shared
//! during de_thread) ends the process with SIGSEGV on failure, as Linux does. Then: the other
//! threads end (de_thread), a vfork parent is released, the fd table is unshared if another process uses it
//! and loses its close-on-exec fds, caught signals go back to SIG_DFL, and the group's table entry gets the new
//! image (same pid; a non-leader thread takes the leader's id).

use crate::task::{self, Thread};
use crate::{Process, SpawnError, elf, exit, image};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use hal::entry::UserContext;
use mm::AddressSpace;
use uapi::errno::*;
use vfs::abi::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use vfs::{Ctx, Errno, Path};

/// Interpreter levels (`#!`) before ELOOP (`exec_binprm`: depth 0..=5 run a handler).
const MAX_DEPTH: usize = 5;
/// `BINPRM_BUF_SIZE`: how much of a script's first line the kernel reads.
const BINPRM_BUF_SIZE: usize = 256;

klog::counter!(EXECS, "proc.execs");

/// Argument strings in one buffer (a hostile argv of many short strings costs its bytes, not a heap block
/// per string).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArgList {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl ArgList {
    pub fn new() -> Self {
        ArgList::default()
    }
    pub fn push(&mut self, s: &[u8]) -> Result<(), Errno> {
        self.bytes.try_reserve(s.len()).map_err(|_| ENOMEM)?;
        self.ends.try_reserve(1).map_err(|_| ENOMEM)?;
        self.bytes.extend_from_slice(s);
        self.ends.push(self.bytes.len() as u32);
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.ends.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
    pub fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(|i| self.get(i))
    }
    /// The strings in the opposite order (the syscall layer copies them last first, as Linux).
    pub fn reversed(&self) -> Result<ArgList, Errno> {
        let mut r = ArgList::new();
        r.bytes.try_reserve(self.bytes.len()).map_err(|_| ENOMEM)?;
        for i in (0..self.len()).rev() {
            r.push(self.get(i))?;
        }
        Ok(r)
    }
}

/// What to run: the file (`dirfd` + `path` as execveat takes them) and the arguments, already copied from the
/// caller's memory with Linux's limits (the syscall layer does that; E2BIG and EFAULT are its answers).
pub struct ExecRequest {
    pub dirfd: i32,
    pub path: Vec<u8>,
    /// AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH (execveat); anything else is EINVAL.
    pub flags: u64,
    pub argv: ArgList,
    pub envp: ArgList,
}

/// Where the program file is, after the path walk.
pub enum ExecTarget {
    File(Path),
    /// An fd without a path (a pipe, a socket): not executable.
    NoPath,
}

/// The process's view for path walks: its mount view, root, cwd and credentials.
fn view(p: &Process) -> Ctx<'_> {
    let root = p.ns.root_path();
    let cwd = p.fds.fs.cwd().unwrap_or_else(|| root.clone());
    Ctx { ns: &p.ns, root, cwd, cred: p.fs_cred(), umask: p.fds.fs.umask(), subject: p.subject() }
}

/// `bprm->filename`: what AT_EXECFN and the new `comm` are made of.
fn filename(dirfd: i32, path: &[u8]) -> Vec<u8> {
    if path.first() == Some(&b'/') || dirfd == AT_FDCWD {
        return path.to_vec();
    }
    if path.is_empty() {
        return format!("/dev/fd/{dirfd}").into_bytes();
    }
    let mut f = format!("/dev/fd/{dirfd}/").into_bytes();
    f.extend_from_slice(path);
    f
}

/// `do_open_execat`'s walk: the file `dirfd`/`path` names.
fn lookup(p: &Process, c: &Ctx<'_>, dirfd: i32, path: &[u8], flags: u64) -> Result<ExecTarget, Errno> {
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(ENOENT);
    }
    if path.is_empty() {
        // AT_EMPTY_PATH (the syscall layer refused an empty path without it): the fd itself, or the cwd.
        if dirfd == AT_FDCWD {
            return Ok(ExecTarget::File(c.cwd.clone()));
        }
        let f = p.fds.get(dirfd as u32 as usize)?;
        return Ok(f.path.clone().map_or(ExecTarget::NoPath, ExecTarget::File));
    }
    let base = if path[0] == b'/' || dirfd == AT_FDCWD {
        None
    } else {
        let f = p.fds.get(if dirfd < 0 { usize::MAX } else { dirfd as usize })?;
        match &f.path {
            Some(d) if d.dentry.is_dir() => Some(d.clone()),
            _ => return Err(ENOTDIR),
        }
    };
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let found = c.lookup(base.as_ref(), path, follow)?;
    if !follow && found.dentry.inode.is_symlink() {
        return Err(ELOOP);
    }
    Ok(ExecTarget::File(found))
}

/// `may_open(MAY_EXEC)` and the regular-file check, B3; the file itself (AB-8: never read whole, its segments
/// map from the page cache).
fn open_exec(p: &Process, c: &Ctx<'_>, t: ExecTarget) -> Result<(Path, Arc<dyn vfs::File>, vfs::inode::DenyWrite), Errno> {
    let ExecTarget::File(path) = t else { return Err(EACCES) };
    let i = &path.dentry.inode;
    if !i.is_reg() {
        return Err(EACCES);
    }
    // FS-13 CR1: noexec before the permission bits, as Linux (linux-6.6/fs/namei.c:3263, fs/exec.c:935); this
    // covers the program, PT_INTERP and a script's interpreter alike.
    if path.noexec() {
        klog::event!(Info, "proc", "exec.noexec", pid = p.pid, mount = path.mnt.id);
        return Err(EACCES);
    }
    i.meta.permission(c.cred, vfs::inode::MAY_EXEC)?;
    // PM-1: no path text is built while the program has no path rule (allows_any is also true for KERNEL).
    if !policy::allows_any(&p.subject(), policy::AccessClass::Path) {
        let name = c.ns.path_of(&c.root, &path).unwrap_or_default();
        let text = String::from_utf8_lossy(&name);
        if let Err(d) = policy::check(&p.subject(), &policy::Access::Path { path: &text, write: false, create: false }) {
            klog::event!(Info, "proc", "exec.refused", pid = p.pid, path = &*text, reason = d.reason.as_str());
            return Err(EACCES);
        }
    }
    // `do_open_execat`: deny_write_access (ETXTBSY while the file is open for writing).
    let deny = i.deny_write()?;
    let f = vfs::node::file_of(&path.dentry);
    Ok((path, f, deny))
}

/// `prepare_binprm`: the first `BINPRM_BUF_SIZE` bytes (fewer for a short file).
fn head(f: &dyn vfs::File) -> Result<Vec<u8>, Errno> {
    let mut b = alloc::vec![0u8; BINPRM_BUF_SIZE];
    let mut got = 0;
    while got < b.len() {
        match f.read_at(got as u64, &mut b[got..])? {
            0 => break,
            n => got += n,
        }
    }
    b.truncate(got);
    Ok(b)
}

/// The errno for an interpreter whose headers `why` refuses: a short or failed read EIO (`elf_read` :430), not
/// an x86-64 ELF or bad program headers ELIBBAD (`load_elf_binary` :940-955), another type than ET_EXEC/ET_DYN
/// EINVAL (`load_elf_interp` :606; Linux finds that after the point of no return and the process dies with
/// SIGSEGV, V2 checks before it and execve returns EINVAL).
pub(crate) fn interp_errno(why: elf::ElfError) -> Errno {
    match why {
        elf::NOT_PROGRAM_TYPE => EINVAL,
        elf::SHORT_READ => uapi::errno::EIO,
        _ => uapi::ab::errno_more::ELIBBAD,
    }
}

/// `load_script`'s parse of a `#!` line over the first `BINPRM_BUF_SIZE` bytes, zero-padded like `bprm->buf`:
/// (interpreter, optional argument), or None when it is not a script. ENOEXEC for a line without an
/// interpreter or with a cut-off interpreter name. Everything after the name is one argument, as Linux does.
pub(crate) fn parse_shebang(head: &[u8]) -> Result<Option<(Vec<u8>, Option<Vec<u8>>)>, Errno> {
    let mut buf = [0u8; BINPRM_BUF_SIZE];
    let n = head.len().min(BINPRM_BUF_SIZE);
    buf[..n].copy_from_slice(&head[..n]);
    if buf[0] != b'#' || buf[1] != b'!' {
        return Ok(None);
    }
    let tab = |c: u8| c == b' ' || c == b'\t';
    let last = BINPRM_BUF_SIZE - 1;
    // `next_non_spacetab`/`next_terminator` look at [first, last] inclusive.
    let non_tab = |b: &[u8], from: usize, to: usize| (from..=to).find(|&i| !tab(b[i]));
    let term = |b: &[u8], from: usize, to: usize| (from..=to).find(|&i| tab(b[i]) || b[i] == 0);
    let mut i_end = match buf.iter().position(|&c| c == b'\n') {
        Some(i) => i,
        None => {
            let first = non_tab(&buf, 2, last).ok_or(ENOEXEC)?;
            // No later space, tab or NUL: the interpreter's name is cut off.
            term(&buf, first, last).ok_or(ENOEXEC)?;
            last
        }
    };
    while tab(buf[i_end - 1]) {
        i_end -= 1;
    }
    let i_name = match non_tab(&buf, 2, i_end) {
        Some(i) if i != i_end => i,
        _ => return Err(ENOEXEC),
    };
    let i_sep = term(&buf, i_name, i_end);
    let i_arg = match i_sep {
        Some(s) if buf[s] != 0 => non_tab(&buf, s, i_end),
        _ => None,
    };
    buf[i_end] = 0;
    if let (Some(s), Some(_)) = (i_sep, i_arg) {
        buf[s] = 0;
    }
    let cstr = |from: usize| -> Vec<u8> { buf[from..].iter().take_while(|&&c| c != 0).copied().collect() };
    Ok(Some((cstr(i_name), i_arg.map(cstr))))
}

/// The other threads end and are waited for (Linux `de_thread`). EAGAIN when another group action (an
/// exit_group, a kill, a second execve) is in progress: the caller is ending anyway.
fn de_thread(p: &Process, t: &Thread) -> Result<(), Errno> {
    let g = p.group();
    loop {
        if exit::alone(g) {
            return Ok(());
        }
        if g.group_exited() || g.dethreading() {
            return Err(EAGAIN);
        }
        g.begin_dethread(t.kthread());
        let others = exit::stop_siblings(g, t);
        for o in &others {
            while !o.wait_done(100_000_000) {
                if g.group_exited() {
                    g.end_dethread();
                    return Err(EAGAIN);
                }
                o.request_stop();
            }
        }
        g.end_dethread();
    }
}

/// A clean user FPU/SIMD state for the new program (Linux `fpu_flush_thread`).
fn clean_fpu() {
    let mut d = hal::xsave::read_user_regs();
    d.mxcsr = 0x1F80;
    d.fcw = 0x037F;
    d.regs.fill(0);
    d.kmask.fill(0);
    hal::xsave::load_user_regs(&d);
}

/// execve/execveat for thread `t` of image `p`. On success the thread continues in the new program (its `ctx`
/// is the new start); on an error nothing changed.
pub fn execve(p: &Arc<Process>, t: &Arc<Thread>, ctx: &mut UserContext, r: &ExecRequest) -> Result<(), Errno> {
    if r.flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let c = view(p);
    let fname = filename(r.dirfd, &r.path);
    let mut argv = r.argv.clone();
    let (mut file, mut f, mut deny) = open_exec(p, &c, lookup(p, &c, r.dirfd, &r.path, r.flags)?)?;
    // `BINPRM_FLAGS_PATH_INACCESSIBLE` (linux-6.6/fs/exec.c:1848): the name a script's interpreter would get,
    // /dev/fd/N/..., dies with the close-on-exec fd N, so a script there is ENOENT (fs/binfmt_script.c:93).
    let path_inaccessible = r.dirfd != AT_FDCWD && r.path.first() != Some(&b'/') && p.fds.cloexec(r.dirfd as u32 as usize).unwrap_or(false);
    // `bprm->interp`: the name the next interpreter gets as its script argument.
    let mut interp = fname.clone();
    let mut depth = 0;
    let elf = loop {
        match parse_shebang(&head(&*f)?)? {
            None => break image::Source::File { path: &file, file: &*f }.elf().map_err(|why| {
                klog::event!(Info, "proc", "exec.not_executable", pid = p.pid, path = String::from_utf8_lossy(&fname).as_ref(), why = why);
                ENOEXEC
            })?,
            Some((name, arg)) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(ELOOP);
                }
                if path_inaccessible {
                    return Err(ENOENT);
                }
                if name.is_empty() {
                    return Err(ENOENT);
                }
                // argv[0] goes; the interpreter, its argument and the script's name come first.
                let mut a = ArgList::new();
                a.push(&name)?;
                if let Some(x) = arg {
                    a.push(&x)?;
                }
                a.push(&interp)?;
                for s in argv.iter().skip(1) {
                    a.push(s)?;
                }
                argv = a;
                interp = name.clone();
                (file, f, deny) = open_exec(p, &c, lookup(p, &c, AT_FDCWD, &name, 0)?)?;
            }
        }
    };
    // AB-8: PT_INTERP is opened like the program (`open_exec`: path walk from the root and cwd, MAY_EXEC,
    // regular file, B3); its headers are checked here, before anything changes.
    let interp = match &elf.interp {
        None => None,
        Some(name) => {
            // Its deny (`_ideny`) lasts until the load is done, as Linux's `allow_write_access(interpreter)`.
            let (ip, ifile, _ideny) = open_exec(p, &c, lookup(p, &c, AT_FDCWD, name, 0)?).inspect_err(|&e| {
                klog::event!(Info, "proc", "exec.interp_refused", pid = p.pid, interp = String::from_utf8_lossy(name).as_ref(), errno = e);
            })?;
            // The interpreter's own PT_INTERP is not followed (Linux loads one level).
            let ie = image::Source::File { path: &ip, file: &*ifile }.interp_elf().map_err(|why| {
                klog::event!(Info, "proc", "exec.interp_refused", pid = p.pid, interp = String::from_utf8_lossy(name).as_ref(), why = why);
                interp_errno(why)
            })?;
            Some((ip, ifile, ie, _ideny))
        }
    };
    let stack = image::stack_size(p.limits.get(uapi::ab::rlimit::RLIMIT_STACK).map_or(uapi::ab::exec::STACK_RLIMIT, |l| l.cur));
    let mm = AddressSpace::with_fill(mm::Fill::OnFault).map_err(|_| ENOMEM)?;
    let mut a: Vec<&[u8]> = Vec::new();
    a.try_reserve(argv.len()).map_err(|_| ENOMEM)?;
    a.extend(argv.iter());
    let mut e: Vec<&[u8]> = Vec::new();
    e.try_reserve(r.envp.len()).map_err(|_| ENOMEM)?;
    e.extend(r.envp.iter());
    let prog = image::Image { src: image::Source::File { path: &file, file: &*f }, elf: &elf };
    let iimg = interp.as_ref().map(|(ip, ifile, ie, _)| image::Image { src: image::Source::File { path: ip, file: &**ifile }, elf: ie });
    // AB-13: the ids the new image gets (`cap_bprm_creds_from_file`); auxv shows them.
    let cred = Arc::new(p.cred().for_exec());
    let mut l = image::load(&mm, &prog, iimg.as_ref(), stack, &fname, &a, &e, &cred).map_err(|e| match e {
        SpawnError::NotExecutable(why) => {
            klog::event!(Info, "proc", "exec.not_loadable", pid = p.pid, path = String::from_utf8_lossy(&fname).as_ref(), why = why);
            ENOEXEC
        }
        other => other.errno(),
    })?;
    drop((a, e));
    drop(iimg);
    drop(prog);
    // FS-13: the program and its interpreter for the start-up preload's record (while the view is at hand).
    let preload = (c.root.clone(), file.clone(), interp.as_ref().map(|(ip, ..)| ip.clone()));
    drop(interp);
    drop(f);
    let exe = String::from_utf8_lossy(&c.ns.path_of(&c.root, &file).unwrap_or_else(|| fname.clone())).into_owned();
    let exe_path = file.clone();
    drop(c);
    // Everything that can fail is done before the caller changes: the argv copy for /proc and an own fd table
    // when another process shares this one (taken now; checked again after de_thread below).
    let name = crate::base_name(&String::from_utf8_lossy(&fname));
    let mut args: Vec<String> = Vec::new();
    args.try_reserve(argv.len()).map_err(|_| ENOMEM)?;
    args.extend(argv.iter().map(|s| String::from_utf8_lossy(s).into_owned()));
    let mut own_fds = if crate::fds_shared(&p.fds, p.pid) { Some(p.fds.duplicate()?) } else { None };

    // ---- the caller changes from here on (Linux `begin_new_exec`'s point of no return, fs/exec.c:1259) ------
    de_thread(p, t)?;
    let g = p.group();
    if !t.is_leader() {
        task::retid(t, g.tgid);
    }
    task::release_tid_words(t, p, crate::mm_shared(&p.mm, p.pid));
    if own_fds.is_none() && crate::fds_shared(&p.fds, p.pid) {
        // A sibling thread made a CLONE_FILES process between the look above and de_thread.
        match p.fds.duplicate() {
            Ok(d) => own_fds = Some(d),
            Err(e) => {
                // Past the point of no return Linux ends the process with SIGSEGV (`bprm_execve` :1877); the
                // old program cannot go on with its threads gone.
                klog::event!(Warn, "proc", "exec.failed_late", pid = p.pid, errno = e, what = "fd table");
                p.end_with(crate::ExitStatus::Signaled(uapi::ab::signal::SIGSEGV));
                return Err(e);
            }
        }
    }
    let fds = own_fds.map_or_else(|| p.fds.clone(), Arc::new);
    // AB-5: the POSIX record locks of the files close-on-exec closes go with them (`filp_close` →
    // `locks_remove_posix`).
    let closing = crate::locks::cloexec_files(&fds);
    fds.close_on_exec();
    crate::locks::closed_files(&fds, &closing);
    drop(closing);
    // `flush_signal_handlers` and `exit_itimers` (begin_new_exec): the POSIX timers go, the itimers stay.
    p.signals.with(|s| {
        s.reset_for_exec();
        s.drop_posix_timers();
    });
    t.reset_alt_stack();
    // `__set_task_comm(me, kbasename(bprm->filename))`.
    t.prctl().set_comm(crate::prctl_state::comm_of(name.as_bytes()));
    // `begin_new_exec`'s `commit_creds(bprm->cred)`. `cred` was taken before de_thread on purpose: in Linux
    // `bprm->cred` is the exec'ing task's own copy (`prepare_bprm_creds`), so a sibling's set*id while the
    // image loads changes only that sibling, which de_thread then ends. V2 keeps one set per group, so the
    // snapshot is what restores Linux's result (and keeps the auxv ids the same as the committed ones).
    g.set_cred(cred.clone());
    let new = crate::make_image(p.pid, name, exe, args, p.program, &cred, Arc::new(mm), fds, p.ns.clone(), g.clone());
    new.mm_flags.for_exec(&p.mm_flags);
    // `begin_new_exec`: dumpable again only when the effective ids are the real ones (else `suid_dumpable`, 0).
    if cred.euid != cred.uid || cred.egid != cred.gid {
        new.mm_flags.set_dumpable(0);
    }
    *new.saved_auxv.lock() = core::mem::take(&mut l.auxv);
    // AB-7a: /proc's view of the new image (`set_mm_exe_file`, the layout of `load_elf_binary`).
    *new.image_info.lock() = crate::procinfo::ImageInfo { exe: Some(exe_path), deny: Some(Arc::new(deny)), ..core::mem::take(&mut l.info) };
    let old = crate::replace_image(&new);
    g.set_image(&new);
    g.mark_exec();
    g.set_exit_signal(uapi::ab::signal::SIGCHLD);
    new.mm.activate();
    // A vfork parent sharing the old memory goes on now (Linux `exec_mm_release` → `complete_vfork_done`).
    g.vfork_done.complete();
    // AB-7c: the old image's peak stays the group's (`exec_mmap` → `setmax_mm_hiwater_rss`).
    g.note_maxrss(&p.mm);
    // The old image's memory goes now unless another process (that vfork parent) still runs on it.
    exit::release_memory(&p.mm, p.pid);
    // `set_mm_exe_file`: the old exe's deny-write goes with the old image (AB-8 CR2).
    let old_deny = p.image_info.lock().deny.take();
    drop(old_deny);
    drop(old);
    *ctx = UserContext::new(l.entry, l.rsp);
    clean_fpu();
    t.new_image.store(true, Ordering::Release);
    EXECS.inc();
    vfs::preload::exec_started(new.pid, &new.ns, &preload.0, &preload.1, preload.2.as_ref());
    klog::event!(Info, "proc", "exec", pid = new.pid, path = new.path.as_str(), entry = klog::Hex(l.entry), pie = elf.is_pie, bias = klog::Hex(l.bias), interp_base = klog::Hex(l.interp_base), argc = argv.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sb(s: &[u8]) -> Result<Option<(Vec<u8>, Option<Vec<u8>>)>, Errno> {
        parse_shebang(s)
    }

    #[test]
    fn shebang_lines_like_linux() {
        assert_eq!(sb(b"\x7fELF"), Ok(None));
        assert_eq!(sb(b"#!/bin/sh\necho"), Ok(Some((b"/bin/sh".to_vec(), None))));
        assert_eq!(sb(b"#! /bin/sh -e \necho"), Ok(Some((b"/bin/sh".to_vec(), Some(b"-e".to_vec())))));
        // Everything after the interpreter is one argument (Linux does not split it).
        assert_eq!(sb(b"#!/usr/bin/env python3 -u\n"), Ok(Some((b"/usr/bin/env".to_vec(), Some(b"python3 -u".to_vec())))));
        assert_eq!(sb(b"#!\t/bin/x\t\t\n"), Ok(Some((b"/bin/x".to_vec(), None))));
        assert_eq!(sb(b"#!   \n"), Err(ENOEXEC), "no interpreter");
        // A short file without a newline: the zero padding ends the line (an empty name, then ENOENT).
        assert_eq!(sb(b"#!"), Ok(Some((b"".to_vec(), None))));
        // No newline in a short file: the interpreter ends at the end of the file.
        assert_eq!(sb(b"#!/bin/sh"), Ok(Some((b"/bin/sh".to_vec(), None))));
        // A 256-byte line without newline and without a space after the name: cut off, ENOEXEC.
        let mut long = b"#!/".to_vec();
        long.extend(vec![b'a'; 300]);
        assert_eq!(sb(&long), Err(ENOEXEC));
        // ... with a space after the name the name is complete, the argument may be cut.
        let mut ok = b"#!/bin/sh ".to_vec();
        ok.extend(vec![b'x'; 300]);
        let (n, a) = sb(&ok).unwrap().unwrap();
        assert_eq!(n, b"/bin/sh");
        assert_eq!(a.unwrap().len(), 255 - 10, "the last buffer byte becomes the NUL");
    }

    #[test]
    fn filenames_like_bprm() {
        assert_eq!(filename(AT_FDCWD, b"x/y"), b"x/y");
        assert_eq!(filename(5, b"/abs"), b"/abs");
        assert_eq!(filename(5, b""), b"/dev/fd/5");
        assert_eq!(filename(7, b"rel/p"), b"/dev/fd/7/rel/p");
    }
}
