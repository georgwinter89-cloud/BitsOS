//! The gate's descriptors. Each open of `/dev/bitsos/mcp` is one session (one command at a time), so the
//! service opens a pool of them as root at its start (the node is root-only; open files stay usable after the
//! drop) and every request leases one for its commands. A lease waits a bounded time for a free one.

use std::io::{Read, Write};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// errno values the service names (Linux's numbers).
pub const EPERM: i32 = 1;
pub const EIO: i32 = 5;
pub const EACCES: i32 = 13;

/// One gate session: a command in, its answer (text) or the write's errno out.
pub trait Link: Send {
    fn command(&mut self, line: &[u8]) -> Result<String, i32>;
}

/// A session on the node.
pub struct DevLink(std::fs::File);

impl DevLink {
    pub fn open(path: &str) -> std::io::Result<DevLink> {
        std::fs::OpenOptions::new().read(true).write(true).open(path).map(DevLink)
    }
}

impl Link for DevLink {
    fn command(&mut self, line: &[u8]) -> Result<String, i32> {
        // One write(2) is one command; the node takes it whole or answers an errno.
        match self.0.write(line) {
            Ok(n) if n == line.len() => {}
            Ok(_) => return Err(EIO),
            Err(e) => return Err(e.raw_os_error().unwrap_or(EIO)),
        }
        let mut out = Vec::new();
        self.0.read_to_end(&mut out).map_err(|e| e.raw_os_error().unwrap_or(EIO))?;
        String::from_utf8(out).map_err(|_| EIO)
    }
}

/// Free sessions.
pub struct Pool {
    free: Mutex<Vec<Box<dyn Link>>>,
    cv: Condvar,
    size: usize,
}

impl Pool {
    pub fn new(links: Vec<Box<dyn Link>>) -> Pool {
        let size = links.len();
        Pool { free: Mutex::new(links), cv: Condvar::new(), size }
    }
    pub fn size(&self) -> usize {
        self.size
    }
    /// A free session within `wait`, or None.
    pub fn take(&self, wait: Duration) -> Option<Lease<'_>> {
        let g = self.free.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, _) = self.cv.wait_timeout_while(g, wait, |v| v.is_empty()).unwrap_or_else(|e| e.into_inner());
        let link = g.pop()?;
        Some(Lease { pool: self, link: Some(link) })
    }
}

/// A session lent to one request; back into the pool when dropped.
pub struct Lease<'a> {
    pool: &'a Pool,
    link: Option<Box<dyn Link>>,
}

impl Lease<'_> {
    pub fn command(&mut self, line: &[u8]) -> Result<String, i32> {
        match self.link.as_mut() {
            Some(l) => l.command(line),
            None => Err(EIO),
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(l) = self.link.take() {
            self.pool.free.lock().unwrap_or_else(|e| e.into_inner()).push(l);
            self.pool.cv.notify_one();
        }
    }
}
