//! A release asset (`.tar.zst`) read as a stream: decompressed by `ruzstd` (pure Rust, no C library to ship) and
//! split by `tar::Reader`, so nothing is unpacked on the host before it goes onto the stick.

use crate::tar;
use std::io::{BufReader, Read};
use std::path::Path;

pub type Stream = tar::Reader<Box<dyn Read>>;

pub fn open(p: &Path) -> Result<Stream, String> {
    let f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let d = ruzstd::decoding::StreamingDecoder::new(BufReader::with_capacity(1 << 20, f)).map_err(|e| format!("{}: not a zstd archive: {e}", p.display()))?;
    Ok(tar::Reader::new(Box::new(BufReader::with_capacity(1 << 20, d))))
}

/// One member's bytes (small files: VENDOR.txt, PKGLIST.TXT), None when the archive has no such member.
pub fn member(p: &Path, name: &str, max: u64) -> Result<Option<Vec<u8>>, String> {
    let mut s = open(p)?;
    while let Some(e) = s.next_entry().map_err(|e| format!("{}: {e}", p.display()))? {
        if e.path == name && e.kind == tar::EntryKind::File {
            if e.size > max {
                return Err(format!("{}: {name} is larger than {max} bytes", p.display()));
            }
            let mut v = Vec::new();
            (&mut s).read_to_end(&mut v).map_err(|e| format!("{}: {e}", p.display()))?;
            return Ok(Some(v));
        }
    }
    Ok(None)
}
