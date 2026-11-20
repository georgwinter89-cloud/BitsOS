//! UPD-4: the parts' files on their way: each downloaded asset checked against its signed size and SHA-256 (read
//! from the work folder piece by piece, never whole in memory), then handed to the kernel's node file by file:
//! the bin part's files as they are, a tree's pieces unpacked (tar + zstd, bitspkg's zstd reader). Every path and
//! the list's content hash are checked here before the kernel checks them again.

use crate::flow::{Fail, Step, What};
use crate::manifest::{Entry, hex};
use crate::node::Node;
use crate::parts::{Kind, Part};
use crate::parts_rules::{self as rules, List};
use crate::verify::Why;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

const PIECE: usize = 1 << 20;
/// linux-7.3.1/include/uapi/asm-generic/fcntl.h:42 and :60 (x86_64 takes the generic values).
const O_NONBLOCK: i32 = 1 << 11;
const O_NOFOLLOW: i32 = 1 << 17;

/// A downloaded asset, opened as root: the work folder belongs to the fetch user, so a link there is never
/// followed and nothing but a plain file is read (a FIFO would block the update).
pub fn open_plain(path: &Path) -> Result<std::fs::File, Fail> {
    use std::os::unix::fs::OpenOptionsExt;
    let err = |x: std::io::Error| Fail::Local(format!("{}: {x}", path.display()));
    let f = std::fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW | O_NONBLOCK).open(path).map_err(err)?;
    if !f.metadata().map_err(err)?.is_file() {
        return Err(Fail::Local(format!("{}: not a plain file", path.display())));
    }
    Ok(f)
}

fn bad(name: &str, what: String) -> Fail {
    Fail::Check(Why::File { name: name.to_string(), what })
}

/// A downloaded asset against its manifest line.
pub fn check_file(e: &Entry, path: &Path) -> Result<(), Fail> {
    let mut f = open_plain(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; PIECE];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf).map_err(|x| Fail::Local(format!("{}: {x}", path.display())))?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    if n != e.size {
        return Err(bad(&e.name, format!("{n} bytes, the manifest says {}", e.size)));
    }
    if <[u8; 32]>::from(h.finalize()) != e.sha256 {
        return Err(bad(&e.name, String::from("its SHA-256 is not the signed one")));
    }
    Ok(())
}

/// Hashes what the node reads.
struct Hashing<'a> {
    inner: &'a mut dyn Read,
    h: Sha256,
}

impl Read for Hashing<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.h.update(&buf[..n]);
        Ok(n)
    }
}

/// The node's refusal: "no space" is the medium's, anything else the kernel's check.
pub fn node_fail(t: String) -> Fail {
    if t.contains("no space") || t.contains("KiB free") { Fail::NoSpace } else { Fail::Kernel(t) }
}

/// One part to the node (`files`: its assets in the work folder, in the part's order). Nothing is sent when the
/// medium holds it already.
pub fn send(node: &mut Node, p: &Part, files: &[PathBuf], step: &mut dyn FnMut(Step)) -> Result<(), Fail> {
    if node.part(&p.name, &p.hash, p.files, p.dirs, p.bytes).map_err(node_fail)? {
        return Ok(());
    }
    step(Step::Writing { what: What::of(&p.name), bytes: p.bytes });
    let mut list = List::default();
    let mut sum = Sha256::new();
    let mut one = |node: &mut Node, path: &str, size: u64, data: &mut dyn Read| -> Result<(), Fail> {
        list.add(path, p.kind == Kind::Bin, size).map_err(|t| bad(&p.name, t))?;
        let mut hr = Hashing { inner: data, h: Sha256::new() };
        node.put_file(path, size, &mut hr).map_err(node_fail)?;
        sum.update(rules::line(&hex(&hr.h.finalize()), size, path).as_bytes());
        Ok(())
    };
    match p.kind {
        Kind::Bin => {
            for (e, f) in p.assets.iter().zip(files) {
                let mut r = open_plain(f)?;
                one(node, &e.name, e.size, &mut r)?;
            }
        }
        Kind::Tree => {
            for f in files {
                let r = open_plain(f)?;
                let mut t = crate::tar::Reader::new(bitspkg::repo::unpack::Zstd::new(BufReader::with_capacity(PIECE, r)));
                while let Some(e) = t.next_entry().map_err(|x| bad(&p.name, x.to_string()))? {
                    if e.kind == crate::tar::EntryKind::Dir {
                        continue;
                    }
                    one(node, &e.path, e.size, &mut t)?;
                }
            }
        }
    }
    if list.count != p.files || list.bytes != p.bytes || list.dirs() != p.dirs {
        return Err(bad(
            &p.name,
            format!("{} files in {} folders with {} bytes, the manifest says {} in {} with {}", list.count, list.dirs(), list.bytes, p.files, p.dirs, p.bytes),
        ));
    }
    if hex(&sum.finalize()) != p.hash {
        return Err(bad(&p.name, String::from("its files are not the signed ones (content hash)")));
    }
    node.seal().map_err(node_fail)
}

/// A tree's pieces unpacked as BitsOS will: (content hash, files, dirs, bytes).
pub fn scan(pieces: &[PathBuf]) -> Result<(String, u64, u64, u64), String> {
    let mut list = List::default();
    let mut sum = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    for p in pieces {
        let f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut t = crate::tar::Reader::new(bitspkg::repo::unpack::Zstd::new(BufReader::new(f)));
        while let Some(e) = t.next_entry().map_err(|e| format!("{}: {e}", p.display()))? {
            if e.kind == crate::tar::EntryKind::Dir {
                continue;
            }
            list.add(&e.path, false, e.size).map_err(|x| format!("{}: {x}", p.display()))?;
            let mut h = Sha256::new();
            let mut n = 0u64;
            loop {
                let k = t.read(&mut buf).map_err(|x| format!("{}: {}: {x}", p.display(), e.path))?;
                if k == 0 {
                    break;
                }
                h.update(&buf[..k]);
                n += k as u64;
            }
            if n != e.size {
                return Err(format!("{}: {} ends early", p.display(), e.path));
            }
            sum.update(rules::line(&hex(&h.finalize()), e.size, &e.path).as_bytes());
        }
    }
    if list.count == 0 {
        return Err(String::from("a tree without files"));
    }
    Ok((hex(&sum.finalize()), list.count, list.dirs(), list.bytes))
}

/// Bytes of memory free for the work folder (`MemAvailable`; None: not known).
pub fn ram_free() -> Option<u64> {
    let t = std::fs::read_to_string("/proc/meminfo").ok()?;
    let l = t.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kib: u64 = l.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib << 10)
}
