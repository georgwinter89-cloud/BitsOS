//! `/dev/bitsos/access`, the access log's reader (root only, 0600: it names every program's files and peers).
//! Each open file has its own text waiting: a read hands out whole lines of it, and when nothing waits it drains
//! the rings first (`drain::take_text`, which always ends with the status line). Writes: `on` / `off` switch the
//! recording (the service writes what Georg set in `access.enabled`). `/proc/bitsos/access` is the counters' line
//! for people and the Mini-PC checks.

use super::drain;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use uapi::errno::*;
use crate::{Cred, Errno, File};

struct Reader {
    /// (text, handed out up to).
    wait: ksync::Mutex<(Vec<u8>, usize)>,
}

impl Reader {
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut g = self.wait.lock();
        if g.1 >= g.0.len() {
            *g = (drain::take_text().into_bytes(), 0);
        }
        let rest = &g.0[g.1..];
        // Whole lines only; a first line longer than the buffer is EINVAL (as /dev/kmsg).
        let fit = rest[..rest.len().min(buf.len())].iter().rposition(|&b| b == b'\n').map(|i| i + 1).ok_or(EINVAL)?;
        buf[..fit].copy_from_slice(&rest[..fit]);
        g.1 += fit;
        Ok(fit)
    }
}

impl File for Reader {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read(buf)
    }
    fn read_stream(&self, buf: &mut [u8], _nonblock: bool) -> Result<usize, Errno> {
        self.read(buf)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        match core::str::from_utf8(buf).map(str::trim) {
            Ok("on") => super::set_on(true),
            Ok("off") => super::set_on(false),
            _ => return Err(EINVAL),
        }
        klog::event!(Info, "vfs", "access.switched", on = super::on());
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
}

/// A misc number from the dynamic range below /dev/bitsos/events' 240.
fn rdev_number() -> u64 {
    crate::abi::encode_dev(10, 239)
}

struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
    fn open_node(&self, _flags: u64, who: Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        // Root only, whatever the node's mode was changed to.
        if who.uid != 0 {
            return Some(Err(EACCES));
        }
        Some(Ok(open_reader()))
    }
}

pub(crate) fn open_reader() -> Arc<dyn File> {
    Arc::new(Reader { wait: ksync::Mutex::named("vfs.access.reader", (Vec::new(), 0)) })
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
crate::devnode!("/dev/bitsos/access", make);

fn status() -> String {
    let mut s = String::new();
    let _ = writeln!(s, "on={} lost={} records={} ring={}", u8::from(super::on()), super::dropped(), super::ring::kept(), super::ring::CAP);
    s
}
crate::procfile!("/proc/bitsos/access", status);
