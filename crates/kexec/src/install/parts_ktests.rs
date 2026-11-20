//! UPD-4 in-kernel test of the parts' store on the stick image of this run (QemuOnly: it writes below
//! `\BITSOS2\SYS\STORE\` and takes everything out again):
//! - `rb.install.parts_store`: a part written file by file is sealed only with the hash its files have, sealed it is
//!   present and never written again; a part whose files have another hash, a path escaping the part, a folder in
//!   the bin part and more files than announced are refused and leave no `.NEW` folder; the cleanup keeps a part an
//!   update holds and removes it, record first, once nothing holds or names it; a slot without FILES binds nothing.

use super::ktests::mounted;
use super::parts::{self, Begun, Writer};
use super::parts_rules as rules;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use mcp::sha256::{self, Sha256};

const NAME: &str = "ktestpart";

fn hash_of(files: &[(&str, &[u8])]) -> String {
    let mut h = Sha256::default();
    for (p, b) in files {
        let mut f = Sha256::default();
        f.update(b);
        h.update(rules::line(&sha256::hex(&f.finish()), b.len() as u64, p).as_bytes());
    }
    sha256::hex(&h.finish())
}

/// Writes `files` as part (`name`, `hash`) and seals it.
fn write(s: &fs_fat::sysapi::Sys<'_>, id: u64, name: &str, hash: &str, files: &[(&str, &[u8])]) -> Result<bool, String> {
    let bytes = files.iter().map(|f| f.1.len() as u64).sum();
    let mut w = match Writer::begin(s, id, &[], name, hash, files.len() as u64, 1, bytes)? {
        Begun::Present(_) => return Ok(true),
        Begun::Writing(w) => w,
    };
    for (p, b) in files {
        if let Err(t) = w.file(s, b.len() as u64, p).and_then(|more| if more { w.take(s, b).map(|_| ()) } else { Ok(()) }) {
            w.abort(s);
            return Err(t);
        }
    }
    w.seal(s).map(|_| false)
}

fn no_new(s: &fs_fat::sysapi::Sys<'_>) -> Result<(), String> {
    let l = s.store_list().map_err(|e| format!("listing STORE: {e}"))?;
    let left: Vec<&String> = l.iter().map(|e| &e.0).filter(|n| n.starts_with(NAME) && n.ends_with(".NEW")).collect();
    kassert!(left.is_empty(), "an unfinished part stayed: {left:?}");
    Ok(())
}

fn parts_store() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let files: [(&str, &[u8]); 2] = [("a.txt", b"one\n"), ("d/b file.bin", &[7u8; 5000])];
    let hash = hash_of(&files);
    let id = parts::session_id();
    let r = fs_fat::stick::with_sys(|s| -> TestResult {
        // Hostile first: another hash than the files have, a path out of the part, more files than announced.
        let wrong = hash_of(&[("a.txt", b"two\n")]);
        kassert!(write(s, id, NAME, &wrong, &files).is_err_and(|t| t.contains("hash")), "a part with another hash was sealed");
        no_new(s)?;
        kassert!(write(s, id, NAME, &hash, &[("../x", b"1"), ("a.txt", b"1")]).is_err(), "a path out of the part was taken");
        kassert!(write(s, id, rules::BIN, &hash, &[("sub/x", b"1"), ("y", b"1")]).is_err(), "a folder in the bin part was taken");
        kassert!(write(s, id, "settings", &hash, &files).is_err(), "a part named like the medium's own folder");
        let mut w = match Writer::begin(s, id, &[], NAME, &hash, 1, 0, 4)? {
            Begun::Writing(w) => w,
            Begun::Present(_) => return Err(String::from("present before it was written")),
        };
        kassert!(w.file(s, 4, "a.txt").is_ok() && w.take(s, b"one\n").is_ok(), "the first file");
        kassert!(w.file(s, 1, "z").is_err(), "a second file of a one-file part");
        w.abort(s);
        no_new(s)?;
        kassert!(parts::present(s, NAME, &hash).is_none(), "present after the refusals");
        // The part itself, then once more: present, not written again.
        kassert!(write(s, id, NAME, &hash, &files)? == false, "written");
        kassert!(parts::present(s, NAME, &hash).is_some_and(|p| p.files == 2 && p.bytes == 5004), "sealed with its counts");
        let back = s.store_read(&format!("{}/d/b file.bin", rules::folder(NAME, &hash)), 1 << 20).map_err(|e| format!("reading it back: {e}"))?;
        kassert!(back == [7u8; 5000], "the file reads back as written");
        kassert!(write(s, id, NAME, &hash, &files)? == true, "a sealed part is never written again");
        // Held by this session: kept; released and named by no slot: removed, record first.
        parts::hold(id, &rules::folder(NAME, &hash));
        parts::gc(s, &[])?;
        kassert!(parts::present(s, NAME, &hash).is_some(), "the cleanup removed a held part");
        parts::release(id);
        parts::gc(s, &[])?;
        kassert!(parts::present(s, NAME, &hash).is_none(), "the cleanup left a part no slot names");
        let l = s.store_list().map_err(|e| format!("listing STORE: {e}"))?;
        kassert!(!l.iter().any(|e| e.0.starts_with(NAME)), "left in STORE: {l:?}");
        Ok(())
    });
    parts::release(id);
    let _ = fs_fat::stick::with_sys(|s| {
        for e in s.store_list().unwrap_or_default() {
            if e.0.starts_with(NAME) {
                let _ = s.store_remove(&e.0);
            }
        }
    });
    r.map_err(|e| format!("no boot medium: {e}"))??;
    kassert!(super::parts_boot::binds_for(&vol, 99_999_998).is_empty(), "a slot without FILES binds something");
    Ok(())
}
ktest!("rb.install.parts_store", QemuOnly, parts_store);
