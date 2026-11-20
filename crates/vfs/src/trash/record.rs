//! An entry's record, `record.json`: one JSON object, rewritten when the entry's top changes.
//!
//! `{"v":1,"id":"1763415302-7","fs":"/data","path":"/data/home/1000/proj","what":"unlink","time":1763415302,
//! "pid":412,"command":"rm","chat":"c-17","tree":3}`
//!
//! `path` is the common top of everything in `files/` (the removed file itself for an entry of one); `what` is
//! the first removal's kind; `time` the first removal (seconds since 1970, the kernel's clock); `chat` null when
//! the process had no chat label. A restore needs none of it (`files/` holds the paths): the record is for the
//! list and the page.

use super::batch::Batch;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Unlink,
    Rmdir,
    Rename,
}

impl What {
    pub fn as_str(self) -> &'static str {
        match self {
            What::Unlink => "unlink",
            What::Rmdir => "rmdir",
            What::Rename => "rename",
        }
    }
}

/// `b` as JSON (a string: control bytes and bytes that are not UTF-8 escaped).
pub(crate) fn json_str(out: &mut String, b: &[u8]) {
    out.push('"');
    for c in String::from_utf8_lossy(b).chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The path of the entry's top as the removing process saw it.
pub(super) fn top_path(fs: &[u8], top: &[Vec<u8>]) -> Vec<u8> {
    let mut p = if fs == b"/" { Vec::new() } else { fs.to_vec() };
    for c in top {
        p.push(b'/');
        p.extend_from_slice(c);
    }
    if p.is_empty() {
        p.push(b'/');
    }
    p
}

pub(super) fn text(b: &Batch) -> Vec<u8> {
    let mut s = String::from("{\"v\":1,\"id\":");
    json_str(&mut s, &b.id);
    s.push_str(",\"fs\":");
    json_str(&mut s, &b.fs);
    s.push_str(",\"path\":");
    json_str(&mut s, &top_path(&b.fs, &b.top));
    let _ = write!(s, ",\"what\":\"{}\",\"time\":{},\"pid\":{},\"command\":", b.what.as_str(), b.secs, b.pid);
    json_str(&mut s, b.command.as_bytes());
    s.push_str(",\"chat\":");
    match &b.chat {
        Some(c) => json_str(&mut s, c.as_bytes()),
        None => s.push_str("null"),
    }
    let _ = writeln!(s, ",\"tree\":{}}}", b.tree);
    s.into_bytes()
}
