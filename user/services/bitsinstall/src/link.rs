//! The installer's session at the gate `/dev/bitsos/mcp` (one open, root only): `claim` once, then `next` and
//! `decide` in a loop (`mcp_wire::installer`). The kernel checks at every command that the caller still is the
//! boot's /bin/bitsinstall.

use mcp_wire::installer::{self as inst, Asked, Decision};
use std::io::{Read, Write};

pub const EPERM: i32 = 1;
pub const EIO: i32 = 5;
pub const EBUSY: i32 = 16;

/// One gate session: a command in, its answer or the write's errno out.
pub trait Link: Send {
    fn command(&mut self, line: &[u8]) -> Result<String, i32>;
}

pub struct DevLink(std::fs::File);

impl DevLink {
    pub fn open(path: &str) -> std::io::Result<DevLink> {
        std::fs::OpenOptions::new().read(true).write(true).open(path).map(DevLink)
    }
}

impl Link for DevLink {
    fn command(&mut self, line: &[u8]) -> Result<String, i32> {
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

/// `claim`: the pid the kernel saw.
pub fn claim(l: &mut dyn Link) -> Result<u32, i32> {
    let t = l.command(&inst::encode_claim())?;
    mcp_wire::Json::parse(&t).ok().and_then(|j| j.get("pid").and_then(mcp_wire::Json::as_u64)).map(|p| p as u32).ok_or(EIO)
}

/// The next request within `wait_ms` (Ok(None): none came).
pub fn next(l: &mut dyn Link, wait_ms: u64) -> Result<Option<Asked>, i32> {
    let t = l.command(&inst::encode_next(wait_ms))?;
    inst::decode_next_answer(&t).ok_or(EIO)
}

pub fn decide(l: &mut dyn Link, d: &Decision) -> Result<(), i32> {
    l.command(&inst::encode_decide(d)).map(|_| ())
}

/// MC-10: the full-access table for the kernel; the answer says what it took and refused.
pub fn grants(l: &mut dyn Link, list: &[mcp_wire::access::Grant]) -> Result<String, i32> {
    l.command(&mcp_wire::access::encode_table(list))
}
