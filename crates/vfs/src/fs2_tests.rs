//! FS-2 host tests: the registries (`devnode!`, `procfile!`, `procdir!` as `devfs::populate_from` gets them),
//! the synthetic file systems' rules for programs and root, computed directories (merging, listing positions,
//! getcwd inside them), per-open snapshots and live attributes, and the terminal hook for IN.
//! `cases_test::dev_cases` runs host/tests/fs/dev-cases.txt on the view `fake_system` builds.

extern crate std;

use crate::cases_test::{program_view, Mem, SUBJECT, USER};
use crate::devfs::{self, Populated};
use crate::inode::Cred;
use crate::mount_ns::MountNs;
use crate::node::File;
use crate::synth::{DirSource, FileSource, FlatEntry, FlatKind, FlatView, SynthEntry};
use crate::sys::{self, Sys};
use crate::{DevNodeEntry, Errno, FdTable, ProcDirEntry, ProcFileEntry};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::format;
use std::sync::Mutex;
use uapi::errno::*;

// ---- stand-ins for the providers above vfs (vfs-sys: meminfo, cpuinfo, uptime, /proc/self, PCI's view)

fn meminfo() -> String {
    "MemTotal:       32000000 kB\nMemFree:        30000000 kB\nMemAvailable:   30000000 kB\nSwapTotal:             0 kB\n".to_string()
}
fn cpuinfo() -> String {
    "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: test cpu\nflags\t\t: fpu sse avx2 bmi2\n\n".to_string()
}
fn uptime() -> String {
    "12.34 0.00\n".to_string()
}

struct FakeSelf;
impl DirSource for FakeSelf {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(vec![SynthEntry::link("self", "100").cookie(1)])
    }
}
fn fake_self() -> Arc<dyn DirSource> {
    Arc::new(FakeSelf)
}

fn fake_pci() -> Vec<FlatEntry> {
    let d = |p: &str| FlatEntry { path: p.to_string(), kind: FlatKind::Dir };
    let t = |p: &str, v: &str| FlatEntry { path: p.to_string(), kind: FlatKind::Text(v.to_string()) };
    let fun = "devices/pci0000:00/0000:00:00.0";
    vec![
        d("bus"),
        d("bus/pci"),
        d("bus/pci/devices"),
        FlatEntry { path: "bus/pci/devices/0000:00:00.0".to_string(), kind: FlatKind::Link(format!("../../../{fun}")) },
        d("bus/pci/drivers"),
        d("devices"),
        d("devices/pci0000:00"),
        d(fun),
        t(&format!("{fun}/vendor"), "0x8086\n"),
        t(&format!("{fun}/class"), "0x060000\n"),
    ]
}
fn fake_pci_view() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(fake_pci))
}

/// Mounts /dev, /proc, /sys on `ns` with FS's devices and the stand-in providers.
pub(crate) fn fake_system(ns: &MountNs) -> Populated {
    let files = [
        ProcFileEntry { path: "/proc/meminfo", render: meminfo },
        ProcFileEntry { path: "/proc/cpuinfo", render: cpuinfo },
        ProcFileEntry { path: "/proc/uptime", render: uptime },
    ];
    let dirs = [
        ProcDirEntry { path: "/proc", make: fake_self },
        ProcDirEntry { path: "/sys", make: fake_pci_view },
        ProcDirEntry { path: "/proc/sys/fs", make: devfs::proc_sys_fs },
    ];
    devfs::populate_from(ns, &[], &files, &dirs)
}

// ---- a small syscall front end on the fake user memory

struct P<'a> {
    s: Sys<'a>,
    mem: &'a Mem,
}

impl P<'_> {
    fn call(&self, nr: usize, a: [u64; 5]) -> i64 {
        sys::call(&self.s, nr, [a[0], a[1], a[2], a[3], a[4], 0])
    }
    fn path(&self, p: &str) -> u64 {
        self.mem.cstr(p.as_bytes())
    }
    fn open(&self, p: &str, flags: u64, mode: u64) -> i64 {
        self.call(2, [self.path(p), flags, mode, 0, 0])
    }
    fn read(&self, fd: i64, n: usize) -> Result<Vec<u8>, i64> {
        let b = self.mem.alloc_zero(n);
        let r = self.call(0, [fd as u64, b, n as u64, 0, 0]);
        if r < 0 { Err(r) } else { Ok(self.mem.get(b, r as usize)) }
    }
    fn write(&self, fd: i64, data: &[u8]) -> i64 {
        let b = self.mem.alloc(data);
        self.call(1, [fd as u64, b, data.len() as u64, 0, 0])
    }
    fn close(&self, fd: i64) {
        assert_eq!(self.call(3, [fd as u64, 0, 0, 0, 0]), 0);
    }
    /// Every name of a directory, read with a `buf`-byte getdents64 buffer (several calls).
    fn names(&self, dir: &str, buf: usize) -> Vec<String> {
        let fd = self.open(dir, 0o200000, 0);
        assert!(fd >= 0, "open {dir}: {fd}");
        let b = self.mem.alloc_zero(buf);
        let mut out = Vec::new();
        loop {
            let r = self.call(217, [fd as u64, b, buf as u64, 0, 0]);
            assert!(r >= 0, "getdents64 {dir}: {r}");
            if r == 0 {
                break;
            }
            let data = self.mem.get(b, r as usize);
            let mut off = 0;
            while off < data.len() {
                let reclen = u16::from_le_bytes([data[off + 16], data[off + 17]]) as usize;
                let name = &data[off + 19..off + reclen];
                let end = name.iter().position(|&c| c == 0).unwrap();
                out.push(String::from_utf8(name[..end].to_vec()).unwrap());
                off += reclen;
            }
        }
        self.close(fd);
        out
    }
}

fn with_program<R>(ns: &Arc<MountNs>, cred: Cred, f: impl FnOnce(&P<'_>) -> R) -> R {
    let fds = FdTable::new();
    let mem = Mem::new();
    let p = P { s: Sys { mem: &mem, fds: &fds, ns, cred, subject: SUBJECT }, mem: &mem };
    f(&p)
}

// ---- the registries

static FALLBACK_OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// What IN's G0 console device does with output: keep it (the test reads it back).
struct Recorder;
impl File for Recorder {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        buf.iter_mut().for_each(|b| *b = b'k');
        Ok(buf.len().min(3))
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        FALLBACK_OUT.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
}
fn recorder() -> Arc<dyn File> {
    Arc::new(Recorder)
}

struct Foo;
impl File for Foo {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let n = buf.len().min(3);
        buf[..n].copy_from_slice(&b"foo"[..n]);
        Ok(n)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EIO)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o644
    }
}
fn foo() -> Arc<dyn File> {
    Arc::new(Foo)
}
fn foo2() -> Arc<dyn File> {
    Arc::new(Recorder)
}

fn render_x() -> String {
    "x\n".to_string()
}

struct Dev(&'static str);
impl DirSource for Dev {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(vec![SynthEntry::file(self.0, 0o444, Arc::new(crate::synth::Text(format!("{}\n", self.0)))), SynthEntry::dir("system", Arc::new(Sys2(self.0)))])
    }
}
struct Sys2(&'static str);
impl DirSource for Sys2 {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(vec![SynthEntry::file(&format!("from-{}", self.0), 0o444, Arc::new(crate::synth::Text(String::new())))])
    }
}
fn dev_a() -> Arc<dyn DirSource> {
    Arc::new(Dev("a"))
}
fn dev_b() -> Arc<dyn DirSource> {
    Arc::new(Dev("b"))
}

/// Duplicates are refused (the first registration wins, FS's own devices first), a terminal registration is
/// adopted as that node's fallback, procfiles land on proc and sysfs, two computed sources on one directory
/// are merged, a computed directory outside proc/sysfs is refused.
#[test]
fn registries_and_duplicates() {
    let ns = Arc::new(MountNs::new_empty());
    ns.mkdir_p("/tmp").unwrap();
    let devs = [
        DevNodeEntry { path: "/dev/null", make: foo },
        DevNodeEntry { path: "/dev/foo", make: foo },
        DevNodeEntry { path: "/dev/foo", make: foo2 },
        DevNodeEntry { path: "/dev/sub/bar", make: foo },
        DevNodeEntry { path: "/dev/tty0", make: recorder },
    ];
    let files = [ProcFileEntry { path: "/proc/x", render: render_x }, ProcFileEntry { path: "/sys/kernel/y", render: render_x }];
    let dirs = [
        ProcDirEntry { path: "/sys/devices", make: dev_a },
        ProcDirEntry { path: "/sys/devices", make: dev_b },
        ProcDirEntry { path: "/tmp/plain", make: dev_a },
    ];
    let st = devfs::populate_from(&ns, &devs, &files, &dirs);
    // 5 memory devices + 3 terminals + foo + sub/bar; refused: /dev/null again, the second /dev/foo,
    // the computed directory on tmpfs.
    assert_eq!((st.devnodes, st.adopted, st.procfiles, st.procdirs, st.failed), (10, 1, 2, 2, 3), "{st:?}");
    assert_eq!(devfs::builtin_paths().len(), 8);
    assert_eq!(ns.read_file("/dev/null", 10).unwrap(), b"", "the built-in /dev/null wins");
    let mut b = [0u8; 8];
    assert_eq!(ns.open("/dev/foo").unwrap().read_at(0, &mut b), Ok(3), "the first /dev/foo wins");
    assert_eq!(&b[..3], b"foo");
    assert!(ns.lookup("/dev/sub/bar").is_ok(), "directories below /dev are made");
    assert_eq!(ns.lookup_path(b"/dev", true).unwrap().mnt.sb.fstype, "devtmpfs");
    assert_eq!(ns.lookup_path(b"/proc", true).unwrap().mnt.sb.fstype, "proc");
    assert_eq!(ns.lookup_path(b"/sys", true).unwrap().mnt.sb.fstype, "sysfs");
    assert_eq!(ns.read_file("/proc/x", 10).unwrap(), b"x\n");
    assert_eq!(ns.read_file("/sys/kernel/y", 10).unwrap(), b"x\n");
    // Merged: both sources' files, one "system" directory with both sources' names.
    let mut top: Vec<Vec<u8>> = ns.list_dir("/sys/devices").unwrap().into_iter().map(|(n, _)| n).collect();
    top.sort();
    assert_eq!(top, vec![b"a".to_vec(), b"b".to_vec(), b"system".to_vec()]);
    let mut sys2: Vec<Vec<u8>> = ns.list_dir("/sys/devices/system").unwrap().into_iter().map(|(n, _)| n).collect();
    sys2.sort();
    assert_eq!(sys2, vec![b"from-a".to_vec(), b"from-b".to_vec()]);
    assert!(ns.lookup("/sys/devices/system/from-b").is_ok());
    assert_eq!(ns.lookup("/sys/devices/nothing").err(), Some(ENOENT));
    // tty0 (adopted) writes to the registered device until a backend is set: checked in tty_fallback_then_backend.
    let t = ns.lookup("/dev/tty0").unwrap().attr();
    assert_eq!((t.mode, t.rdev, t.uid, t.gid), (uapi::S_IFCHR | 0o620, crate::abi::encode_dev(4, 0), 1000, 5));
}

/// The rules of proc and sysfs for root (programs as uid 1000 are in dev-cases.txt): names come from the
/// kernel only, with Linux's error for each operation.
#[test]
fn fixed_names_for_root() {
    let (ns, _) = program_view();
    fake_system(&ns);
    with_program(&ns, Cred::ROOT, |p| {
        // proc: a missing name fails proc's lookup first (ENOENT), an existing one has no unlink/rename (EPERM).
        assert_eq!(p.open("/proc/new", 0o101, 0o644), -ENOENT);
        assert_eq!(p.call(83, [p.path("/proc/newdir"), 0o755, 0, 0, 0]), -ENOENT);
        assert_eq!(p.call(87, [p.path("/proc/meminfo"), 0, 0, 0, 0]), -EPERM);
        assert_eq!(p.call(87, [p.path("/proc/nothing"), 0, 0, 0, 0]), -ENOENT);
        assert_eq!(p.call(82, [p.path("/proc/meminfo"), p.path("/proc/uptime"), 0, 0, 0]), -EPERM);
        assert_eq!(p.call(82, [p.path("/proc/meminfo"), p.path("/proc/new"), 0, 0, 0]), -ENOENT);
        assert_eq!(p.call(88, [p.path("/tmp"), p.path("/proc/l"), 0, 0, 0]), -ENOENT);
        // O_TMPFILE needs a tmpfile method.
        assert_eq!(p.open("/proc", 0o20200002, 0o600), -EOPNOTSUPP);
        // sysfs (kernfs): creating a file EACCES, a directory or a link EPERM, removing EPERM.
        assert_eq!(p.open("/sys/new", 0o101, 0o644), -EACCES);
        assert_eq!(p.call(83, [p.path("/sys/newdir"), 0o755, 0, 0, 0]), -EPERM);
        assert_eq!(p.call(88, [p.path("/tmp"), p.path("/sys/l"), 0, 0, 0]), -EPERM);
        assert_eq!(p.call(84, [p.path("/sys/bus"), 0, 0, 0, 0]), -EPERM);
        // A proc text without a write method: root passes the mode check (0444, CAP_DAC_OVERRIDE), the write
        // itself fails.
        let fd = p.open("/proc/meminfo", 1, 0);
        assert!(fd >= 0, "root opens a 0444 proc file for writing: {fd}");
        assert!(p.write(fd, b"x") < 0);
        p.close(fd);
        // O_TRUNC on a proc file is accepted and changes nothing (`echo x > /proc/...`).
        let fd = p.open("/proc/uptime", 0o1001, 0);
        assert!(fd >= 0, "O_TRUNC on proc: {fd}");
        p.close(fd);
        assert_eq!(p.read(p.open("/proc/uptime", 0, 0), 100).unwrap(), b"12.34 0.00\n");
        // The devices are fine for root: a new node name in /dev is a tmpfs file.
        let fd = p.open("/dev/scratch", 0o101, 0o600);
        assert!(fd >= 0);
        p.close(fd);
        assert_eq!(p.call(87, [p.path("/dev/scratch"), 0, 0, 0, 0]), 0);
    });
}

/// Computed directories: every name exactly once with any buffer size (static names first, then the
/// sources), a position from one listing (lseek to a d_off) continues there, `..` and getcwd inside a
/// computed directory, a link out of a flat view resolves.
#[test]
fn computed_listing_and_paths() {
    let (ns, _) = program_view();
    fake_system(&ns);
    with_program(&ns, USER, |p| {
        let all = p.names("/proc", 4096);
        for small in [40usize, 64, 100, 200] {
            assert_eq!(p.names("/proc", small), all, "listing with a {small}-byte buffer");
        }
        assert_eq!(&all[..2], &[".".to_string(), "..".to_string()]);
        for want in ["meminfo", "cpuinfo", "uptime", "self"] {
            assert_eq!(all.iter().filter(|n| *n == want).count(), 1, "{want} in {all:?}");
        }
        assert_eq!(*all.last().unwrap(), "self", "computed names follow the static ones");
        // Positions: remember each record's d_off, seek back to one, the listing continues after it.
        let fd = p.open("/proc", 0o200000, 0);
        let b = p.mem.alloc_zero(4096);
        let r = p.call(217, [fd as u64, b, 4096, 0, 0]);
        let data = p.mem.get(b, r as usize);
        let mut recs = Vec::new();
        let mut off = 0;
        while off < data.len() {
            let d_off = i64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap());
            let reclen = u16::from_le_bytes([data[off + 16], data[off + 17]]) as usize;
            recs.push(d_off);
            off += reclen;
        }
        assert_eq!(recs.len(), all.len());
        for (i, &pos) in recs.iter().enumerate() {
            assert_eq!(p.call(8, [fd as u64, pos as u64, 0, 0, 0]), pos);
            let r = p.call(217, [fd as u64, b, 4096, 0, 0]);
            assert!(r >= 0);
            let rest = p.mem.get(b, r as usize);
            let first = if rest.is_empty() { None } else { Some(String::from_utf8(rest[19..19 + rest[19..].iter().position(|&c| c == 0).unwrap()].to_vec()).unwrap()) };
            assert_eq!(first.as_ref(), all.get(i + 1), "after position {pos}");
        }
        p.close(fd);
        // getcwd inside a computed directory and `..` out of it.
        let dir = "/sys/bus/pci/devices/0000:00:00.0";
        assert_eq!(p.call(80, [p.path(dir), 0, 0, 0, 0]), 0, "chdir through the link");
        let b = p.mem.alloc_zero(256);
        let r = p.call(79, [b, 256, 0, 0, 0]);
        assert!(r > 0);
        assert_eq!(p.mem.get(b, r as usize - 1), b"/sys/devices/pci0000:00/0000:00:00.0");
        assert_eq!(p.read(p.open("vendor", 0, 0), 64).unwrap(), b"0x8086\n");
        assert_eq!(p.call(80, [p.path("../.."), 0, 0, 0, 0]), 0);
        let r = p.call(79, [b, 256, 0, 0, 0]);
        assert_eq!(p.mem.get(b, r as usize - 1), b"/sys/devices");
        // A computed name has a stable inode number across lookups.
        let a = ns.lookup("/sys/devices/pci0000:00").unwrap().attr().ino;
        assert_eq!(ns.lookup("/sys/devices/pci0000:00").unwrap().attr().ino, a);
        assert_ne!(ns.lookup("/sys/bus").unwrap().attr().ino, a);
    });
}

/// `/proc/<number>` names as Linux's fs/proc/util.c `name_to_int` reads them (FS-2 fixer: `+5` was pid 5).
#[test]
fn proc_number_names() {
    use crate::synth::name_to_int;
    for (name, want) in [
        ("0", Some(0)),
        ("1", Some(1)),
        ("42", Some(42)),
        ("429496727", Some(429_496_727)),
        ("4294967279", Some(4_294_967_279)),
        ("4294967280", None), // Linux's guard: 429496728 before the last digit is refused
        ("4294967295", None),
        ("99999999999", None),
        ("", None),
        ("+5", None),
        ("-5", None),
        (" 5", None),
        ("5 ", None),
        ("05", None),
        ("00", None),
        ("5a", None),
        ("self", None),
        ("١", None), // Arabic-Indic digit one: not ASCII
    ] {
        assert_eq!(name_to_int(name.as_bytes()), want, "{name:?}");
    }
}

/// Many computed names from two sources on one directory (a `/proc` with tens of thousands of processes): each
/// name once, the overlap left to the first source; the listing is n log n (it was n² per getdents call).
struct Many(u32, u32);
impl DirSource for Many {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok((self.0..self.1).map(|n| SynthEntry::file(&format!("{n}"), 0o444, Arc::new(crate::synth::Text(String::new()))).cookie(16 + n as u64)).collect())
    }
}
fn many_a() -> Arc<dyn DirSource> {
    Arc::new(Many(0, 20_000))
}
fn many_b() -> Arc<dyn DirSource> {
    Arc::new(Many(10_000, 25_000))
}

#[test]
fn large_computed_listing() {
    let (ns, _) = program_view();
    let dirs = [ProcDirEntry { path: "/proc", make: many_a }, ProcDirEntry { path: "/proc", make: many_b }];
    devfs::populate_from(&ns, &[], &[], &dirs);
    with_program(&ns, Cred::ROOT, |p| {
        let all = p.names("/proc", 32 * 1024);
        assert_eq!(all.len(), 2 + 25_000);
        let mut set: Vec<&String> = all.iter().collect();
        set.sort();
        set.dedup();
        assert_eq!(set.len(), all.len(), "a name twice");
        assert_eq!(all[2], "0");
        assert_eq!(all[2 + 19_999], "19999", "the first source's names in its order");
        assert_eq!(all[2 + 20_000], "20000", "then the second source's new names");
    });
}

static RENDERS: AtomicUsize = AtomicUsize::new(0);

struct Counting;
impl FileSource for Counting {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        let n = RENDERS.fetch_add(1, Ordering::SeqCst);
        Ok(format!("render {n}\n{}", "z".repeat(5000)).into_bytes())
    }
}

/// Opens per caller: `may_open` sees who opens.
struct Private;
impl FileSource for Private {
    fn may_open(&self, who: Cred, _write: bool) -> Result<(), Errno> {
        if who.uid == 0 || who.uid == 1000 { Ok(()) } else { Err(EACCES) }
    }
    fn render(&self, who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(format!("uid {}\n", who.uid).into_bytes())
    }
}

/// A binary attribute read live at an offset.
struct Live;
impl FileSource for Live {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(Vec::new())
    }
    fn live_size(&self) -> Option<u64> {
        Some(256)
    }
    fn read_live(&self, _who: Cred, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (pos as usize + i) as u8;
        }
        Ok(buf.len())
    }
}

struct Files;
impl DirSource for Files {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(vec![
            SynthEntry::file("counting", 0o444, Arc::new(Counting)),
            SynthEntry::file("private", 0o444, Arc::new(Private)),
            SynthEntry::file("config", 0o444, Arc::new(Live)),
        ])
    }
}
fn files() -> Arc<dyn DirSource> {
    Arc::new(Files)
}

/// A text is made at each read at offset 0 (reads in pieces see one text; a read at 0 after lseek and a new
/// open make a new one, as seq_file), `may_open` decides per caller, live attributes read at their offset up to their size.
#[test]
fn snapshots_and_live_files() {
    let (ns, _) = program_view();
    devfs::populate_from(&ns, &[], &[], &[ProcDirEntry { path: "/proc/test", make: files }]);
    with_program(&ns, USER, |p| {
        let fd = p.open("/proc/test/counting", 0, 0);
        let a = p.read(fd, 100).unwrap();
        let mut rest = Vec::new();
        loop {
            let r = p.read(fd, 1000).unwrap();
            if r.is_empty() {
                break;
            }
            rest.extend(r);
        }
        assert_eq!(a.len() + rest.len(), "render 0\n".len() + 5000);
        assert_eq!(p.call(8, [fd as u64, 0, 0, 0, 0]), 0);
        assert_ne!(p.read(fd, 100).unwrap(), a, "a read at 0 after lseek renders again (seq_file, AB-7a CR10)");
        p.close(fd);
        let fd = p.open("/proc/test/counting", 0, 0);
        assert_ne!(p.read(fd, 100).unwrap(), a, "a new open renders again");
        p.close(fd);
        assert_eq!(p.read(p.open("/proc/test/private", 0, 0), 100).unwrap(), b"uid 1000\n");
        let fd = p.open("/proc/test/config", 0, 0);
        assert_eq!(p.call(8, [fd as u64, 250, 0, 0, 0]), 250);
        assert_eq!(p.read(fd, 100).unwrap(), vec![250, 251, 252, 253, 254, 255]);
        assert_eq!(p.read(fd, 100).unwrap(), b"");
    });
    with_program(&ns, Cred { uid: 1001, gid: 1001 }, |p| {
        assert_eq!(p.open("/proc/test/private", 0, 0), -EACCES);
    });
    with_program(&ns, Cred::ROOT, |p| {
        assert_eq!(p.read(p.open("/proc/test/private", 0, 0), 100).unwrap(), b"uid 0\n");
    });
}

/// The terminals: before a backend, output goes to the adopted device (IN's G0 console) or, without one, to
/// the kernel log; reads are end of file. After `set_console`, writes, reads (with O_NONBLOCK), ioctls and
/// poll readiness come from the backend; a second backend is refused.
#[test]
fn tty_fallback_then_backend() {
    let ns = Arc::new(MountNs::new_empty());
    ns.mkdir_p("/tmp").unwrap();
    devfs::populate_from(&ns, &[DevNodeEntry { path: "/dev/console", make: recorder }], &[], &[]);
    with_program(&ns, USER, |p| {
        // Before the backend (the only test that sets it runs these steps first).
        let fd = p.open("/dev/console", 2, 0);
        assert!(fd >= 0, "uid 1000 opens its console: {fd}");
        assert_eq!(p.write(fd, b"hello console\n"), 14);
        assert!(FALLBACK_OUT.lock().unwrap().ends_with(b"hello console\n"));
        assert_eq!(p.read(fd, 10).unwrap(), b"kkk", "reads go to the adopted device");
        let t = p.open("/dev/tty", 2, 0);
        assert_eq!(p.write(t, b"to the log\n"), 11, "no device: the kernel log");
        assert_eq!(p.read(t, 10).unwrap(), b"", "no device: end of file");
        assert_eq!(p.call(8, [t as u64, 0, 0, 0, 0]), -ESPIPE);
        assert_eq!(p.call(17, [t as u64, p.mem.alloc_zero(4), 4, 0, 0]), -ESPIPE, "pread on a terminal");
        assert!(crate::tty::set_console(&BACKEND));
        assert!(!crate::tty::set_console(&BACKEND), "a second backend is refused");
        assert_eq!(p.write(fd, b"now the VT\n"), 11);
        assert_eq!(&*BACKEND.out.lock().unwrap(), b"now the VT\n");
        assert!(!FALLBACK_OUT.lock().unwrap().ends_with(b"now the VT\n"));
        BACKEND.lines.lock().unwrap().push(b"line\n".to_vec());
        assert!(f_events_in(&ns));
        assert_eq!(p.read(fd, 10).unwrap(), b"line\n");
        // O_NONBLOCK with nothing typed: EAGAIN from the backend.
        let nb = p.open("/dev/tty", 2 | 0o4000, 0);
        assert_eq!(p.read(nb, 10), Err(-EAGAIN));
        // Blocking with nothing typed: the backend asks `abort` (no kill check installed on the host: false)
        // and gives up after its own bound.
        assert_eq!(p.read(t, 10), Err(-EINTR));
        let f = ns.open("/dev/tty").unwrap();
        assert_eq!(f.ioctl(0x5413, 0), Ok(vec![1, 2, 3, 4]));
        assert!(!f.events().contains(crate::Events::IN));
    });
}

fn f_events_in(ns: &MountNs) -> bool {
    ns.open("/dev/console").unwrap().events().contains(crate::Events::IN)
}

struct FakeVt {
    out: Mutex<Vec<u8>>,
    lines: Mutex<Vec<Vec<u8>>>,
}
impl crate::tty::ConsoleBackend for FakeVt {
    fn write(&self, buf: &[u8]) -> Result<usize, Errno> {
        self.out.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn read(&self, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if let Some(l) = self.lines.lock().unwrap().pop() {
            let n = l.len().min(buf.len());
            buf[..n].copy_from_slice(&l[..n]);
            return Ok(n);
        }
        if nonblock {
            return Err(EAGAIN);
        }
        for _ in 0..3 {
            if abort() {
                return Err(EINTR);
            }
        }
        Err(EINTR)
    }
    fn has_input(&self) -> bool {
        !self.lines.lock().unwrap().is_empty()
    }
    fn ioctl(&self, cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        if cmd == 0x5413 { Ok(vec![1, 2, 3, 4]) } else { Err(ENOTTY) }
    }
}
static BACKEND: FakeVt = FakeVt { out: Mutex::new(Vec::new()), lines: Mutex::new(Vec::new()) };

/// The device numbers and modes of every built-in node (the Linux numbers of drivers/char/mem.c and tty_io.c).
#[test]
fn device_numbers() {
    let ns = Arc::new(MountNs::new_empty());
    devfs::populate_from(&ns, &[], &[], &[]);
    let want = [
        ("/dev/null", 1, 3, 0o666, 0, 0),
        ("/dev/zero", 1, 5, 0o666, 0, 0),
        ("/dev/full", 1, 7, 0o666, 0, 0),
        ("/dev/random", 1, 8, 0o666, 0, 0),
        ("/dev/urandom", 1, 9, 0o666, 0, 0),
        ("/dev/tty", 5, 0, 0o666, 0, 5),
        ("/dev/console", 5, 1, 0o620, 1000, 5),
        ("/dev/tty0", 4, 0, 0o620, 1000, 5),
    ];
    for (path, maj, min, perm, uid, gid) in want {
        let a = ns.lookup(path).unwrap().attr();
        assert_eq!((a.mode, a.rdev, a.uid, a.gid), (uapi::S_IFCHR | perm, crate::abi::encode_dev(maj, min), uid, gid), "{path}");
    }
    let mut names: Vec<Vec<u8>> = ns.list_dir("/dev").unwrap().into_iter().map(|(n, _)| n).collect();
    names.sort();
    // The eight nodes and the devpts mount point (IN-7 CR3).
    assert_eq!(names.len(), 9);
    assert!(names.iter().any(|n| n == b"pts"));
    assert_eq!(ns.lookup_path(b"/dev/pts", true).unwrap().mnt.sb.fstype, "devpts");
    // urandom: different bytes on each read, all values in 64 KiB.
    let u = ns.open("/dev/urandom").unwrap();
    let mut a = vec![0u8; 65536];
    let mut b = vec![0u8; 65536];
    u.read_at(0, &mut a).unwrap();
    u.read_at(0, &mut b).unwrap();
    assert_ne!(a, b);
    let mut seen = [false; 256];
    a.iter().for_each(|&x| seen[x as usize] = true);
    assert!(seen.iter().all(|&s| s));
}
