//! AB-7b: a text file made when it is opened (a sysctl string, a sysfs attribute): the text comes from a
//! closure so a file found once and read later shows the value at its open, not at its lookup.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use vfs::synth::FileSource;
use vfs::{Cred, Errno};

type Make = Box<dyn Fn() -> Result<Vec<u8>, Errno> + Send + Sync>;
type Store = Box<dyn Fn(Cred, &[u8]) -> Result<usize, Errno> + Send + Sync>;

pub(super) struct Live {
    make: Make,
    store: Option<Store>,
    /// `st_size`: 0 for /proc files, one page for sysfs attributes, CPULIST_FILE_MAX_BYTES for cpu lists.
    size: u64,
}

/// Sysfs `st_size` of a text attribute and of a `*_list` CPU file (`CPULIST_FILE_MAX_BYTES` with NR_CPUS 8192).
pub(super) const PAGE: u64 = 4096;
pub(super) const CPULIST_MAX: u64 = 28672;

impl Live {
    pub(super) fn new(size: u64, make: impl Fn() -> Result<String, Errno> + Send + Sync + 'static) -> Self {
        Live { make: Box::new(move || make().map(String::into_bytes)), store: None, size }
    }
    /// A file whose text may hold any bytes (a host name set through sethostname).
    pub(super) fn bytes(size: u64, make: impl Fn() -> Result<Vec<u8>, Errno> + Send + Sync + 'static) -> Self {
        Live { make: Box::new(make), store: None, size }
    }
    /// A sysfs attribute whose value is fixed text.
    pub(super) fn text(s: impl Into<String>) -> Self {
        let s: String = s.into();
        Live::new(PAGE, move || Ok(s.clone()))
    }
    pub(super) fn writable(mut self, store: impl Fn(Cred, &[u8]) -> Result<usize, Errno> + Send + Sync + 'static) -> Self {
        self.store = Some(Box::new(store));
        self
    }
}

impl FileSource for Live {
    /// A file without a store refuses an open for writing, root's too: Linux's sysctl (`sysctl_perm`) and
    /// kernfs (`kernfs_fop_open`) both answer EACCES there.
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        if write && self.store.is_none() { Err(uapi::errno::EACCES) } else { Ok(()) }
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        (self.make)()
    }
    fn write(&self, who: Cred, data: &[u8]) -> Result<usize, Errno> {
        match &self.store {
            Some(f) => f(who, data),
            None => Err(uapi::errno::EIO),
        }
    }
    fn size(&self) -> u64 {
        self.size
    }
}
