//! AB-8 ktests: dynamic programs. The image carries the glibc programs of user/abi-tests/dyn, CachyOS's `ls`
//! and `cat` and their library set in /bin (`cargo xtask abi-diff` builds them with the host's gcc); until the
//! image puts the libraries at their Linux paths (UL-5, desk change request) `link_libs` links /lib64 and
//! /usr/lib to them. Checked: the layout the loader makes (program at ELF_ET_DYN_BASE as file mappings, the
//! loader top-down below the mmap base, AT_BASE/AT_ENTRY/AT_PHDR, the break after the program), the programs'
//! own checks, execve of a dynamic program with a raised RLIMIT_STACK, and the PT_INTERP refusals. All `Safe`
//! and rerunnable (the links stay; own files under /tmp/ab8-*).

use crate::ktests::{PROG, ended, image, need};
use crate::stdio::{Capture, Input};
use crate::{ExitStatus, SpawnError, SpawnOptions, elf, load};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use mm::VmaKind;
use vfs::File;

/// The library set's place on Linux (as `host/xtask/src/abi_dyn.rs` `lib_path`).
fn lib_path(name: &str) -> String {
    if name.starts_with("ld-linux") { format!("/lib64/{name}") } else { format!("/usr/lib/{name}") }
}

/// Links the library set in /bin to /lib64 and /usr/lib where nothing is yet; Err(skip) when the image has no
/// dynamic programs.
fn link_libs() -> Result<(), TestResult> {
    let ns = vfs::root_ns();
    if ns.lookup("/bin/dyn-hello").is_err() {
        return Err(ktest::skip("no dynamic programs in this image (built without the host's gcc or the musl sysroot)"));
    }
    let names = ns.list_dir("/bin").map_err(|e| Err(format!("list /bin: {e}")))?;
    for (n, _) in names {
        let n = String::from_utf8_lossy(&n).into_owned();
        if !n.contains(".so") {
            continue;
        }
        let dest = lib_path(&n);
        if ns.lookup(&dest).is_ok() {
            continue;
        }
        let dir = &dest[..dest.rfind('/').unwrap_or(0)];
        ns.mkdir_p(dir).map_err(|e| Err(format!("mkdir {dir}: {e}")))?;
        ns.create_symlink(&dest, &format!("/bin/{n}")).map_err(|e| Err(format!("symlink {dest}: {e}")))?;
        klog::event!(Info, "ktest", "ab8.linked", path = dest.as_str());
    }
    Ok(())
}

/// Runs `path` with `argv` and `stdin`: (status, stdout, stderr).
fn run(path: &str, argv: &[&str], stdin: &[u8]) -> Result<(ExitStatus, String, String), String> {
    let out = Capture::new(1 << 20);
    let err = Capture::new(1 << 16);
    let env = ["PATH=/bin", "HOME=/", "ABI_TEST=1"];
    let mut o = SpawnOptions::new(argv, &env, PROG);
    o.stdin = Some(Input::new(stdin.to_vec()) as Arc<dyn File>);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.stderr = Some(err.clone() as Arc<dyn File>);
    let p = crate::spawn_with(path, &o).map_err(|e| format!("{path}: {e}"))?;
    let st = ended(&p, 120)?;
    Ok((st, String::from_utf8_lossy(&out.contents()).into_owned(), String::from_utf8_lossy(&err.contents()).into_owned()))
}

/// A program's own yes/no checks: exit 0, `last` as the last line, no line ending in ": no".
fn checks(path: &str, argv: &[&str], last: &str) -> TestResult {
    let (st, out, err) = run(path, argv, b"")?;
    kassert!(st == ExitStatus::Exited(0), "{path} {argv:?}: {}; stdout {out:?} stderr {err:?}", st.describe());
    kassert!(out.lines().last() == Some(last), "{path} {argv:?}: last line {:?}; stderr {err:?}", out.lines().last());
    let failed: Vec<&str> = out.lines().filter(|l| l.ends_with(": no")).collect();
    kassert!(failed.is_empty(), "{path} {argv:?}: failed checks {failed:?}");
    Ok(())
}

ktest!("ab.ab8.hello", Safe, hello);
fn hello() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    checks("/bin/dyn-hello", &["dyn-hello", "one", "two words"], "dyn hello: done")
}

ktest!("ab.ab8.threads", Safe, threads);
fn threads() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    let (st, out, err) = run("/bin/dyn-threads", &["dyn-threads"], b"")?;
    kassert!(st == ExitStatus::Exited(0), "{}; stderr {err:?}", st.describe());
    kassert!(out == "threads created: 8\ntotal: 160000\nreturns: 56\ntls per thread: yes\nerrno per thread: yes\ndyn threads: done\n", "{out:?}");
    Ok(())
}

// execve of a dynamic program by a dynamic program, after setrlimit(RLIMIT_STACK, 64 MiB): the new image
// recurses through 20 MiB of stack (AB-4 open point 3).
ktest!("ab.ab8.exec_stack_rlimit", Safe, exec_stack_rlimit);
fn exec_stack_rlimit() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    checks("/bin/dyn-hello", &["dyn-hello", "stack"], "dyn hello: done")
}

// CachyOS's own coreutils, unchanged.
ktest!("ab.ab8.coreutils", Safe, coreutils);
fn coreutils() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    let (st, out, err) = run("/bin/dyn-cat", &["dyn-cat"], b"first line\nsecond line\n")?;
    kassert!(st == ExitStatus::Exited(0) && out == "first line\nsecond line\n", "cat: {} {out:?} {err:?}", st.describe());
    let (st, out, err) = run("/bin/dyn-ls", &["dyn-ls", "-d", "/", "/dev/null", "/proc/self"], b"")?;
    kassert!(st == ExitStatus::Exited(0) && out == "/\n/dev/null\n/proc/self\n", "ls: {} {out:?} {err:?}", st.describe());
    let (st, out, _) = run("/bin/dyn-ls", &["dyn-ls", "/no/such/file"], b"")?;
    kassert!(st == ExitStatus::Exited(2) && out.is_empty(), "ls of a missing file: {}", st.describe());
    Ok(())
}

/// The value of auxv entry `key` in a saved auxv (key, value pairs).
fn aux(v: &[u64], key: u64) -> Option<u64> {
    v.chunks(2).find(|c| c[0] == key).and_then(|c| c.get(1).copied())
}

// The layout before the first instruction (held), against the plan computed here from the file's headers.
ktest!("ab.ab8.layout", Safe, layout);
fn layout() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    let ns = vfs::root_ns();
    let img = crate::read_program(&ns, "/bin/dyn-hello").map_err(|e| format!("{e}"))?;
    let e = elf::Elf::parse(&img, hal::USER_TOP)?;
    kassert!(e.interp.as_deref() == Some(&b"/lib64/ld-linux-x86-64.so.2"[..]), "PT_INTERP {:?}", e.interp);
    let plan = load::plan(&e, mm::MMAP_TOP, crate::STACK_TOP - crate::STACK_SIZE - crate::image::STACK_GAP)?;
    kassert!(plan.start == 0x5555_5555_4000, "a gcc PIE starts at ELF_ET_DYN_BASE: {:#x}", plan.start);
    let out = Capture::new(1 << 16);
    let argv = ["dyn-hello"];
    let mut o = SpawnOptions::new(&argv, &[], PROG);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.held = true;
    let p = crate::spawn_with("/bin/dyn-hello", &o).map_err(|e| format!("{e}"))?;
    let auxv = p.saved_auxv.lock().clone();
    let vmas = p.mm.vmas();
    let resident = p.mm.resident_pages();
    let brk = p.mm.current_brk();
    p.release();
    let st = ended(&p, 60)?;
    use uapi::auxv::*;
    let base = aux(&auxv, AT_BASE).unwrap_or(0);
    kassert!(aux(&auxv, AT_ENTRY) == Some(plan.entry), "AT_ENTRY {:?} plan {:#x}", aux(&auxv, AT_ENTRY), plan.entry);
    kassert!(aux(&auxv, AT_PHDR) == Some(plan.phdr), "AT_PHDR {:?}", aux(&auxv, AT_PHDR));
    kassert!(aux(&auxv, AT_SYSINFO_EHDR).is_none(), "no vDSO before AB-9");
    kassert!(base != 0 && base < mm::MMAP_TOP && mm::MMAP_TOP - base < (4 << 20), "AT_BASE {base:#x}: the loader top-down below the mmap base");
    kassert!(brk == plan.end, "break {brk:#x}, image end {:#x}", plan.end);
    // Program and loader are file mappings (no copied Image memory), the first program page at its address.
    kassert!(!vmas.iter().any(|v| v.kind == VmaKind::Image), "copied image memory: {vmas:?}");
    let at = |a: u64| vmas.iter().find(|v| v.start <= a && a < v.end).map(|v| (v.kind, v.prot));
    kassert!(matches!(at(plan.start), Some((VmaKind::File, p)) if p.read && !p.write), "program page 0: {:?}", at(plan.start));
    kassert!(matches!(at(base), Some((VmaKind::File, _))), "loader page 0: {:?}", at(base));
    kassert!(matches!(at(plan.entry), Some((VmaKind::File, p)) if p.exec), "program text: {:?}", at(plan.entry));
    // Lazy: the stack, the cleared data tails and page tables, not the files (ld.so alone is about 70 pages).
    kassert!(resident < 40, "{resident} pages resident before the first instruction");
    kassert!(st == ExitStatus::Exited(0), "{}: {:?}", st.describe(), String::from_utf8_lossy(&out.contents()));
    Ok(())
}

/// A static test program with a PT_INTERP naming `interp` (the PT_GNU_STACK header made PT_INTERP).
fn with_interp(interp: &str) -> Result<Vec<u8>, String> {
    with_interp_of(need(hal::entry::test_programs::stack_report())?, true, interp)
}

/// The same for `code` (a PIE when `pie`).
fn with_interp_of(code: &[u8], pie: bool, interp: &str) -> Result<Vec<u8>, String> {
    let mut img = image(code, pie);
    let at = img.len() as u64;
    img.extend_from_slice(interp.as_bytes());
    img.push(0);
    let o = 64 + 3 * 56;
    img[o..o + 4].copy_from_slice(&elf::PT_INTERP.to_le_bytes());
    img[o + 8..o + 16].copy_from_slice(&at.to_le_bytes());
    img[o + 32..o + 40].copy_from_slice(&(interp.len() as u64 + 1).to_le_bytes());
    Ok(img)
}

// Linux's errnos for a bad interpreter: missing ENOENT, a directory EACCES, not an x86-64 ELF ELIBBAD.
ktest!("ab.ab8.interp_refusals", Safe, interp_refusals);
fn interp_refusals() -> TestResult {
    let ns = vfs::root_ns();
    let before = crate::processes().len();
    let o = SpawnOptions::new(&["x"], &[], PROG);
    let e = crate::spawn_image_with("/ab8/x", &with_interp("/no/such/ld.so")?, &o).err();
    kassert!(e == Some(SpawnError::NotFound), "missing: {e:?}");
    let e = crate::spawn_image_with("/ab8/x", &with_interp("/bin")?, &o).err();
    kassert!(e == Some(SpawnError::Io(uapi::errno::EACCES)), "a directory: {e:?}");
    ns.mkdir_p("/tmp").map_err(|e| format!("mkdir /tmp: {e}"))?;
    ns.create_file("/tmp/ab8-notelf", 0o755, b"#!/bin/sh\nnot an ELF file at all, but long enough for a header....\n".to_vec()).map_err(|e| format!("{e}"))?;
    let e = crate::spawn_image_with("/ab8/x", &with_interp("/tmp/ab8-notelf")?, &o).err();
    let _ = ns.remove("/tmp/ab8-notelf");
    kassert!(e == Some(SpawnError::Io(uapi::ab::errno_more::ELIBBAD)), "not an ELF: {e:?}");
    // Shorter than an ELF header: EIO (Linux's `elf_read`).
    ns.create_file("/tmp/ab8-short", 0o755, b"\x7fELF\x02\x01\x01\0\0\0".to_vec()).map_err(|e| format!("{e}"))?;
    let e = crate::spawn_image_with("/ab8/x", &with_interp("/tmp/ab8-short")?, &o).err();
    let _ = ns.remove("/tmp/ab8-short");
    kassert!(e == Some(SpawnError::Io(uapi::errno::EIO)), "a short interpreter: {e:?}");
    // ET_REL: EINVAL (Linux finds it after the point of no return and kills the process with SIGSEGV).
    let mut rel = image(need(hal::entry::test_programs::stack_report())?, true);
    rel[16..18].copy_from_slice(&1u16.to_le_bytes());
    ns.create_file("/tmp/ab8-rel", 0o755, rel).map_err(|e| format!("{e}"))?;
    let e = crate::spawn_image_with("/ab8/x", &with_interp("/tmp/ab8-rel")?, &o).err();
    let _ = ns.remove("/tmp/ab8-rel");
    kassert!(e == Some(SpawnError::Io(uapi::errno::EINVAL)), "an ET_REL interpreter: {e:?}");
    // A C string: "\0x" is the empty path (ENOENT), "/bin\0x" is /bin (EACCES).
    let e = crate::spawn_image_with("/ab8/x", &with_interp("\0x")?, &o).err();
    kassert!(e == Some(SpawnError::NotFound), "an empty interpreter path: {e:?}");
    let e = crate::spawn_image_with("/ab8/x", &with_interp("/bin\0/no/such")?, &o).err();
    kassert!(e == Some(SpawnError::Io(uapi::errno::EACCES)), "a path cut at its first NUL: {e:?}");
    kassert!(crate::exec::interp_errno(elf::NOT_PROGRAM_TYPE) == uapi::errno::EINVAL && crate::exec::interp_errno(elf::SHORT_READ) == uapi::errno::EIO);
    kassert!(crate::processes().len() == before, "a refused start left a process");
    Ok(())
}

// musl's case (no shared musl in the sysroot yet, UL change request): an interpreter that carries a PT_INTERP
// of its own. Linux loads one level: the interpreter's PT_INTERP (here a missing file) is ignored and the
// thread starts at the interpreter's entry. The program is `faults` (argc 1: a read at 0, SIGSEGV if it ran),
// the interpreter `stack_report` (writes 8 bytes, exit 0).
ktest!("ab.ab8.one_level", Safe, one_level);
fn one_level() -> TestResult {
    let ns = vfs::root_ns();
    ns.mkdir_p("/tmp").map_err(|e| format!("mkdir /tmp: {e}"))?;
    let _ = ns.remove("/tmp/ab8-interp");
    ns.create_file("/tmp/ab8-interp", 0o755, with_interp("/no/such/ld.so")?).map_err(|e| format!("{e}"))?;
    let prog = with_interp_of(need(hal::entry::test_programs::faults())?, false, "/tmp/ab8-interp")?;
    let entry = elf::Elf::parse(&prog, hal::USER_TOP)?.entry;
    let out = Capture::new(1 << 12);
    let argv = ["ab8-prog"];
    let mut o = SpawnOptions::new(&argv, &[], PROG);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    let r = crate::spawn_image_with("/ab8/prog", &prog, &o);
    let _ = ns.remove("/tmp/ab8-interp");
    let p = r.map_err(|e| format!("spawn: {e}"))?;
    let auxv = p.saved_auxv.lock().clone();
    let st = ended(&p, 60)?;
    kassert!(st == ExitStatus::Exited(0) && out.contents().len() == 8, "{} with {} bytes out: not started at the interpreter's entry", st.describe(), out.contents().len());
    kassert!(aux(&auxv, uapi::auxv::AT_ENTRY) == Some(entry), "AT_ENTRY {:?}, the program's is {entry:#x}", aux(&auxv, uapi::auxv::AT_ENTRY));
    let base = aux(&auxv, uapi::auxv::AT_BASE).unwrap_or(0);
    kassert!(base != 0 && base < mm::MMAP_TOP, "AT_BASE {base:#x}");
    Ok(())
}

// The loader started as the program (`ld.so prog`, how musl's and glibc's loaders are tested): an ET_DYN
// without PT_INTERP, placed like a static-PIE; it then loads the program itself.
ktest!("ab.ab8.loader_direct", Safe, loader_direct);
fn loader_direct() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    let (st, out, err) = run("/lib64/ld-linux-x86-64.so.2", &["ld.so", "/bin/dyn-threads"], b"")?;
    kassert!(st == ExitStatus::Exited(0), "{}; stderr {err:?}", st.describe());
    kassert!(out == "threads created: 8\ntotal: 160000\nreturns: 56\ntls per thread: yes\nerrno per thread: yes\ndyn threads: done\n", "{out:?}");
    let (st, out, err) = run("/lib64/ld-linux-x86-64.so.2", &["ld.so", "/bin/dyn-cat"], b"via the loader\n")?;
    kassert!(st == ExitStatus::Exited(0) && out == "via the loader\n", "cat: {} {out:?} {err:?}", st.describe());
    Ok(())
}

// The file mappings' edges and glibc's startup calls, and execve of copies with a bad PT_INTERP: the errno
// returns to the caller, which keeps running (user/abi-tests/dyn/edges.c, also an ABI diff case).
ktest!("ab.ab8.edges", Safe, edges);
fn edges() -> TestResult {
    if let Err(skip) = link_libs() {
        return skip;
    }
    let ns = vfs::root_ns();
    ns.mkdir_p("/tmp").map_err(|e| format!("mkdir /tmp: {e}"))?;
    let (st, out, err) = run("/bin/dyn-edges", &["dyn-edges"], b"")?;
    kassert!(st == ExitStatus::Exited(0), "{}; stdout {out:?} stderr {err:?}", st.describe());
    kassert!(out.lines().last() == Some("dyn edges: done"), "last line {:?}; stderr {err:?}", out.lines().last());
    let failed: Vec<&str> = out.lines().filter(|l| l.ends_with(": no") || l.contains("no copy")).collect();
    kassert!(failed.is_empty(), "failed checks {failed:?}");
    let want = ["execve missing interpreter = -ENOENT", "execve directory interpreter = -EACCES", "execve interpreter cut at its first NUL = -EACCES", "execve interpreter not ELF = -errno 80", "execve interpreter shorter than a header = -errno 5", "execve interpreter without x = -EACCES", "getrandom bad flags = -EINVAL", "sched_getaffinity len 4 = -EINVAL", "clone3 size 8 = -EINVAL"];
    for w in want {
        kassert!(out.lines().any(|l| l == w), "missing {w:?} in {out:?}");
    }
    Ok(())
}
