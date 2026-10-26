//! `/sys/bitsos/<name>`: BitsOS's own runtime attributes (CL-1 change request 3), the place where settingsd
//! applies kernel.* keys at once. Linux sysfs semantics (linux-6.6/fs/sysfs/file.c sysfs_kf_write,
//! fs/kernfs/file.c kernfs_fop_write_iter): mode 0644; a read gives the value and a newline; every write is the
//! whole value (one trailing newline dropped) wherever the file position is, an empty write is 0, and a write
//! is cut at 4096 bytes (PAGE_SIZE, file.c:304); O_CREAT|O_TRUNC on an
//! existing attribute is accepted and ignored (the synth layer does that), so `echo v > /sys/bitsos/x` works.
//! Only uid 0 may write (EACCES otherwise); a value the setter refuses gives its errno. A read-only attribute
//! (`register_ro`) is mode 0444 and an open for writing is EACCES even for root (linux-6.6/fs/kernfs/file.c:
//! 646-660 kernfs_fop_open: no S_IWUGO or no write op).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// kernfs's write bound without atomic_write_len (linux-6.6/fs/kernfs/file.c:304).
const PAGE: usize = 4096;
use ksync::Mutex;
use uapi::errno::*;
use vfs::Errno;
use vfs::inode::Cred;
use vfs::synth::{DirSource, FileSource, SynthEntry};

#[derive(Clone, Copy)]
pub struct Attr {
    pub get: fn() -> String,
    /// None: read-only.
    pub set: Option<fn(&str) -> Result<(), Errno>>,
}

static ATTRS: Mutex<Vec<(&'static str, Attr)>> = Mutex::new(Vec::new());

/// Adds (or replaces) `/sys/bitsos/<name>`.
pub fn register(name: &'static str, get: fn() -> String, set: fn(&str) -> Result<(), Errno>) {
    add(name, Attr { get, set: Some(set) });
}

/// Adds (or replaces) the read-only `/sys/bitsos/<name>` (mode 0444).
pub fn register_ro(name: &'static str, get: fn() -> String) {
    add(name, Attr { get, set: None });
}

fn add(name: &'static str, attr: Attr) {
    let mut a = ATTRS.lock();
    a.retain(|(n, _)| *n != name);
    a.push((name, attr));
    a.sort_by(|x, y| x.0.cmp(y.0));
}

impl FileSource for Attr {
    fn may_open(&self, who: Cred, write: bool) -> Result<(), Errno> {
        if write && (who.uid != 0 || self.set.is_none()) { Err(EACCES) } else { Ok(()) }
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        let mut v = (self.get)().into_bytes();
        v.push(b'\n');
        Ok(v)
    }
    fn write(&self, who: Cred, data: &[u8]) -> Result<usize, Errno> {
        self.write_at(who, 0, data)
    }
    fn write_at(&self, who: Cred, pos: u64, data: &[u8]) -> Result<usize, Errno> {
        let Some(set) = self.set.filter(|_| who.uid == 0) else { return Err(EACCES) };
        // sysfs_kf_write (fs/sysfs/file.c:127-137) ignores the position, so a second write() on one fd stores too.
        let _ = pos;
        if data.is_empty() {
            return Ok(0);
        }
        let data = &data[..data.len().min(PAGE)];
        let text = core::str::from_utf8(data).map_err(|_| EINVAL)?;
        set(text.strip_suffix('\n').unwrap_or(text))?;
        Ok(data.len())
    }
    fn size(&self) -> u64 {
        PAGE as u64
    }
}

struct Dir;

impl DirSource for Dir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(ATTRS.lock().iter().map(|(n, a)| SynthEntry::file(n, if a.set.is_some() { 0o644 } else { 0o444 }, Arc::new(*a))).collect())
    }
}

fn dir() -> Arc<dyn DirSource> {
    Arc::new(Dir)
}
vfs::procdir!("/sys/bitsos", dir);

// The kernel's own attributes: the klog levels and the console's level (settingsd's kernel.loglevel and
// kernel.console_log). Registered at the Service initcall, once the console exists.

fn loglevel_get() -> String {
    klog::levels()
}

fn loglevel_set(v: &str) -> Result<(), Errno> {
    klog::set_levels(v.trim()).map_err(|bad| {
        klog::event!(Info, "klog", "levels.refused", source = "sysfs", bad = bad);
        EINVAL
    })?;
    klog::event!(Info, "vfs", "sys_bitsos.set", name = "loglevel", value = klog::levels());
    Ok(())
}

fn console_log_get() -> String {
    String::from(console::logfilter::name(console::log_level()))
}

fn console_log_set(v: &str) -> Result<(), Errno> {
    let n = console::logfilter::parse(v).ok_or(EINVAL)?;
    console::set_log_level(n, "sysfs");
    klog::event!(Info, "vfs", "sys_bitsos.set", name = "console_log", value = console::logfilter::name(n));
    Ok(())
}

fn start() -> Result<(), &'static str> {
    register("loglevel", loglevel_get, loglevel_set);
    register("console_log", console_log_get, console_log_set);
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "sys-bitsos", start);

/// `/sys/bitsos/console_log` through the file calls: root writes the current value back (O_CREAT|O_TRUNC as
/// `echo v >` does) and reads it again, a bad value is EINVAL, a write at offset 1 stores as Linux does, an empty write is 0, uid 1000 may read
/// but not open for writing. Rerunnable: it only writes the value that is already set.
fn sys_bitsos() -> ktest::TestResult {
    use alloc::format;
    use ktest::kassert;
    use vfs::abi::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
    use vfs::ops::Ctx;
    let ns = vfs::root_ns();
    let subject = policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0000, uid: 0 };
    let root = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 0, gid: 0 }, umask: 0o022, subject };
    let user = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0o022, subject: policy::Subject { uid: 1000, ..subject } };
    let path = b"/sys/bitsos/console_log";
    let now = console_log_get();
    let f = root.open(None, path, O_WRONLY | O_CREAT | O_TRUNC, 0o644).map_err(|e| format!("open for writing: {e}"))?;
    let line = format!("{now}\n");
    kassert!(f.write(line.as_bytes()) == Ok(line.len()), "write back {now}");
    kassert!(f.write(b"bogus\n") == Err(EINVAL), "a bad value was taken");
    kassert!(f.pwrite(1, now.as_bytes()) == Ok(now.len()), "a write at offset 1 was refused (Linux ignores the position)");
    kassert!(f.write(b"") == Ok(0), "an empty write was not 0");
    let r = user.open(None, path, O_RDONLY, 0).map_err(|e| format!("open as uid 1000: {e}"))?;
    let mut buf = [0u8; 32];
    let n = r.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    kassert!(&buf[..n] == line.as_bytes(), "read {:?}, want {line:?}", core::str::from_utf8(&buf[..n]));
    kassert!(user.open(None, path, O_WRONLY, 0).err() == Some(EACCES), "uid 1000 opened it for writing");
    kassert!(ns.lookup("/sys/bitsos/loglevel").is_ok(), "/sys/bitsos/loglevel missing");
    Ok(())
}
ktest::ktest!("fs.sys.bitsos", Safe, sys_bitsos);
