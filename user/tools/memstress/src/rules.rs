//! `memstress rules`: the errno and signal rules of file and memfd mappings that Wayland clients (wl_shm) and
//! Chromium-like programs meet, each checked against Linux's answer:
//! - mmap(MAP_SHARED, PROT_WRITE) of an O_RDONLY fd: EACCES; MAP_PRIVATE RW of it: ok (linux-6.6/mm/mmap.c:1292
//!   `do_mmap`, FMODE_WRITE only for shared writable maps).
//! - memfd seals (linux-6.6/mm/memfd.c): F_SEAL_WRITE while a shared writable map exists EBUSY (:200
//!   `memfd_wait_for_pins`/`mapping_deny_writable`), after SHRINK|GROW|WRITE ftruncate/pwrite/shared RW map EPERM,
//!   F_SEAL_SEAL forbids further seals (EPERM).
//! - mremap growth keeps the bytes; a partial munmap leaves a hole mincore calls ENOMEM (mm/mincore.c:261).
//! `memstress truncate` (its own mode: V2 differs here, see ul-UL-4b.md CR3): a store past the end of a shrunk
//! shared memfd mapping is SIGBUS (linux-6.6/mm/truncate.c `truncate_pagecache` unmaps the cut pages, the next
//! fault past i_size is VM_FAULT_SIGBUS in mm/shmem.c), checked in a child.

use crate::{ENOMEM, PAGE};
use std::os::unix::process::ExitStatusExt;
use usys::dev::SharedMap;
use usys::{Errno, Mapping, PROT_READ, PROT_WRITE};

const MFD_CLOEXEC: usize = 1;
const MFD_ALLOW_SEALING: usize = 2;
const F_ADD_SEALS: usize = 1033;
const F_GET_SEALS: usize = 1034;
const F_SEAL_SEAL: usize = 1;
const F_SEAL_SHRINK: usize = 2;
const F_SEAL_GROW: usize = 4;
const F_SEAL_WRITE: usize = 8;
const F_SEAL_FUTURE_WRITE: usize = 0x10;
const EBUSY: i32 = 16;
const SIGBUS: i32 = 7;

struct Checks(u32, u32);

impl Checks {
    fn line(&mut self, what: &str, r: Result<(), String>) {
        match r {
            Ok(()) => {
                self.0 += 1;
                println!("ok   {what}");
            }
            Err(e) => {
                self.1 += 1;
                println!("FAIL {what}: {e}");
            }
        }
    }
}

fn errno_is<T: std::fmt::Debug>(r: Result<T, Errno>, want: Errno) -> Result<(), String> {
    match r {
        Err(e) if e == want => Ok(()),
        Err(e) => Err(format!("{}, want {}", e.name(), want.name())),
        Ok(v) => Err(format!("succeeded ({v:?}), want {}", want.name())),
    }
}

fn ok<T>(r: Result<T, Errno>) -> Result<T, String> {
    r.map_err(|e| e.name())
}

/// A file in /dev/shm with two pages of bytes, opened read-only (the writer is closed, the name unlinked).
fn read_only_file() -> Result<std::fs::File, String> {
    let path = format!("/dev/shm/.memstress-rules.{}", std::process::id());
    let w = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("create {path}: {e}"));
    let r = w.as_ref().ok().map(|_| std::fs::File::open(&path));
    let _ = std::fs::remove_file(&path);
    let w = w?;
    w.set_len(2 * PAGE as u64).map_err(|e| format!("ftruncate: {e}"))?;
    r.ok_or("no reader")?.map_err(|e| format!("open {path}: {e}"))
}

fn file_maps(c: &mut Checks) {
    use std::os::fd::AsRawFd;
    match read_only_file() {
        Ok(f) => {
            let fd = f.as_raw_fd() as usize;
            c.line("mmap(MAP_SHARED, PROT_WRITE) of an O_RDONLY file: EACCES", errno_is(SharedMap::new(fd, PAGE, PROT_READ | PROT_WRITE, 0).map(|m| m.len()), usys::EACCES));
            c.line(
                "mmap(MAP_PRIVATE, PROT_READ|PROT_WRITE) of an O_RDONLY file: ok",
                ok(usys::mmap_probe(0, PAGE, PROT_READ | PROT_WRITE, usys::MAP_PRIVATE, fd, 0)).map(|_| ()),
            );
        }
        Err(e) => c.line("an O_RDONLY file in /dev/shm", Err(e)),
    }
}

fn seals(c: &mut Checks) {
    let fd = match usys::memfd_create("memstress-seals", MFD_CLOEXEC | MFD_ALLOW_SEALING) {
        Ok(fd) => fd,
        Err(e) => return c.line("memfd_create(MFD_ALLOW_SEALING)", Err(e.name())),
    };
    c.line("memfd_create(MFD_ALLOW_SEALING): F_GET_SEALS 0", ok(usys::fcntl(fd, F_GET_SEALS, 0)).and_then(|s| if s == 0 { Ok(()) } else { Err(format!("{s:#x}")) }));
    let stamp = [0x5au8; 64];
    c.line("ftruncate 2 pages, pwrite a stamp", ok(usys::ftruncate(fd, 2 * PAGE as i64)).and_then(|_| ok(usys::pwrite64(fd, &stamp, PAGE as i64))).map(|_| ()));
    match SharedMap::new(fd, 2 * PAGE, PROT_READ | PROT_WRITE, 0) {
        Ok(m) => {
            c.line("F_ADD_SEALS(WRITE) while a shared writable map exists: EBUSY", errno_is(usys::fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE), Errno(EBUSY)));
            drop(m);
        }
        Err(e) => c.line("mmap(MAP_SHARED, RW) of the memfd", Err(e.name())),
    }
    c.line(
        "F_ADD_SEALS(SHRINK|GROW|WRITE) after munmap: ok, F_GET_SEALS 0xe",
        ok(usys::fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE))
            .and_then(|_| ok(usys::fcntl(fd, F_GET_SEALS, 0)))
            .and_then(|s| if s == 0xe { Ok(()) } else { Err(format!("{s:#x}")) }),
    );
    c.line("sealed: ftruncate smaller EPERM", errno_is(usys::ftruncate(fd, PAGE as i64), usys::EPERM));
    c.line("sealed: ftruncate larger EPERM", errno_is(usys::ftruncate(fd, 4 * PAGE as i64), usys::EPERM));
    c.line("sealed: pwrite EPERM", errno_is(usys::pwrite64(fd, &stamp, 0), usys::EPERM));
    c.line("sealed: mmap(MAP_SHARED, PROT_WRITE) EPERM", errno_is(SharedMap::new(fd, PAGE, PROT_READ | PROT_WRITE, 0).map(|m| m.len()), usys::EPERM));
    c.line("sealed: a shared read-only map sees the stamp", {
        ok(SharedMap::new(fd, 2 * PAGE, PROT_READ, 0)).and_then(|m| {
            let mut b = [0u8; 64];
            m.read_at(PAGE, &mut b);
            if b == stamp { Ok(()) } else { Err("other bytes".into()) }
        })
    });
    c.line(
        "F_ADD_SEALS(SEAL), then F_ADD_SEALS(FUTURE_WRITE): EPERM",
        ok(usys::fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL)).and_then(|_| errno_is(usys::fcntl(fd, F_ADD_SEALS, F_SEAL_FUTURE_WRITE), usys::EPERM)),
    );
    let _ = usys::close(fd);
}

fn anon(c: &mut Checks) {
    c.line("mremap growth (MAYMOVE) of 2 to 64 pages keeps the bytes", {
        ok(Mapping::anon(2 * PAGE, PROT_READ | PROT_WRITE, 0)).and_then(|mut m| {
            m.set(0, 0x11);
            m.set(2 * PAGE - 1, 0x22);
            ok(m.remap(64 * PAGE, true))?;
            m.set(64 * PAGE - 1, 0x33);
            match (m.get(0), m.get(2 * PAGE - 1), m.get(64 * PAGE - 1), m.get(10 * PAGE)) {
                (Some(0x11), Some(0x22), Some(0x33), Some(0)) => Ok(()),
                got => Err(format!("{got:?}")),
            }
        })
    });
    c.line("munmap of the middle page: mincore ENOMEM there, the neighbours keep their bytes", {
        ok(Mapping::anon(3 * PAGE, PROT_READ | PROT_WRITE, 0)).and_then(|m| {
            for p in 0..3 {
                m.set(p * PAGE, p as u8 + 1);
            }
            ok(m.unmap_part(PAGE, PAGE))?;
            errno_is(m.mincore(PAGE, PAGE), Errno(ENOMEM))?;
            errno_is(m.mincore(0, 3 * PAGE), Errno(ENOMEM))?;
            match (m.get(0), m.get(2 * PAGE)) {
                (Some(1), Some(3)) => Ok(()),
                got => Err(format!("{got:?}")),
            }
        })
    });
}

/// The child of the SIGBUS check: a two-page shared memfd map, the file shrunk to one page, a store to the
/// second. Returns only when no signal came.
pub fn sigbus_child() -> i32 {
    // No core file for the expected fault (RLIMIT_CORE 0).
    let _ = usys::prlimit64(0, 4, Some((0, 0)));
    let fd = match usys::memfd_create("memstress-bus", MFD_CLOEXEC) {
        Ok(fd) => fd,
        Err(e) => {
            println!("memfd_create: {}", e.name());
            return 1;
        }
    };
    let m = usys::ftruncate(fd, 2 * PAGE as i64).and_then(|_| SharedMap::new(fd, 2 * PAGE, PROT_READ | PROT_WRITE, 0));
    let Ok(m) = m else {
        println!("setup: {}", m.err().map(|e| e.name()).unwrap_or_default());
        return 1;
    };
    m.write_at(PAGE, &[1]);
    if let Err(e) = usys::ftruncate(fd, PAGE as i64) {
        println!("ftruncate 1 page: {}", e.name());
        return 1;
    }
    m.write_at(PAGE, &[2]);
    println!("the store past the end came back");
    1
}

fn sigbus(c: &mut Checks) {
    c.line("a store past the end of a shrunk shared memfd map: SIGBUS", {
        std::env::current_exe()
            .map_err(|e| format!("current_exe: {e}"))
            .and_then(|exe| std::process::Command::new(exe).arg("truncate-child").output().map_err(|e| format!("spawn: {e}")))
            .and_then(|o| match o.status.signal() {
                Some(SIGBUS) => Ok(()),
                s => Err(format!("signal {s:?}, exit {:?}: {}", o.status.code(), String::from_utf8_lossy(&o.stdout).trim())),
            })
    });
}

pub fn run() -> i32 {
    let mut c = Checks(0, 0);
    file_maps(&mut c);
    seals(&mut c);
    anon(&mut c);
    println!("rules: {} ok, {} failed", c.0, c.1);
    if c.1 == 0 { 0 } else { 1 }
}

/// `memstress truncate`: 0 when the store past the end was SIGBUS, 1 when not.
pub fn truncate() -> i32 {
    let mut c = Checks(0, 0);
    sigbus(&mut c);
    if c.1 == 0 { 0 } else { 1 }
}
