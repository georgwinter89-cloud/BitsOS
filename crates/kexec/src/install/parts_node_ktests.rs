//! UPD-4 in-kernel test of the update node's parts commands through `/dev/bitsos/update` itself (QemuOnly: it
//! writes below `\BITSOS2\SYS\STORE\` and takes everything out again):
//! - `rb.install.parts_node`: a part named like the medium's own folder, a path out of the part, a folder the part did
//!   not announce, fewer folders than announced and files whose hash is not the part's are each refused and leave
//!   nothing in STORE; after them the same session still writes the right part, seals it and names it once.

use super::ktests::mounted;
use super::parts_rules as rules;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use mcp::sha256::{self, Sha256};
use vfs::OpenFile;

const NAME: &str = "ktestnode";

fn hash_of(path: &str, b: &[u8]) -> String {
    let mut f = Sha256::default();
    f.update(b);
    let mut h = Sha256::default();
    h.update(rules::line(&sha256::hex(&f.finish()), b.len() as u64, path).as_bytes());
    sha256::hex(&h.finish())
}

/// Writes `b` (a command or a file's bytes) and reads the answer.
fn say(f: &Arc<OpenFile>, b: &[u8]) -> Result<String, String> {
    f.write(b).map_err(|e| format!("write {:?}: {e}", String::from_utf8_lossy(b)))?;
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).map_err(|e| format!("read the answer: {e}"))?;
    Ok(String::from(String::from_utf8_lossy(&buf[..n]).trim_end()))
}

fn store_empty() -> Result<(), String> {
    let left = fs_fat::stick::with_sys(|s| s.store_list().unwrap_or_default().into_iter().filter(|e| e.0.starts_with(NAME)).count()).map_err(|e| format!("{e}"))?;
    kassert!(left == 0, "{left} entries of the refused parts stayed in STORE");
    Ok(())
}

fn node_steps(f: &Arc<OpenFile>) -> TestResult {
    let (good, bad) = (hash_of("a", b"abc"), hash_of("a", b"abd"));
    let a = say(f, format!("part settings {good} 1 0 3").as_bytes())?;
    kassert!(a.contains("\"refused\""), "a part named settings: {a}");
    // A path out of the part.
    let a = say(f, format!("part {NAME} {good} 1 0 3").as_bytes())?;
    kassert!(a.contains("\"writing\""), "part: {a}");
    let a = say(f, b"file 3 ../a")?;
    kassert!(a.contains("\"refused\""), "a path out of the part: {a}");
    store_empty()?;
    // A folder the part did not announce.
    say(f, format!("part {NAME} {good} 1 0 3").as_bytes())?;
    let a = say(f, b"file 3 d/a")?;
    kassert!(a.contains("\"refused\""), "a folder that was not announced: {a}");
    store_empty()?;
    // Fewer folders than announced (the space check counted them).
    say(f, format!("part {NAME} {good} 1 1 3").as_bytes())?;
    say(f, b"file 3 a")?;
    let a = say(f, b"abc")?;
    kassert!(a.contains("\"stored\""), "the file's bytes: {a}");
    let a = say(f, b"seal")?;
    kassert!(a.contains("\"refused\"") && a.contains("folders"), "fewer folders than announced: {a}");
    store_empty()?;
    // Files whose hash is not the one the part is named for.
    say(f, format!("part {NAME} {bad} 1 0 3").as_bytes())?;
    say(f, b"file 3 a")?;
    say(f, b"abc")?;
    let a = say(f, b"seal")?;
    kassert!(a.contains("\"refused\"") && a.contains("hash"), "another hash: {a}");
    store_empty()?;
    // The same session after all that: the right part, sealed, named once.
    let a = say(f, format!("part {NAME} {good} 1 0 3").as_bytes())?;
    kassert!(a.contains("\"writing\""), "the right part: {a}");
    say(f, b"file 3 a")?;
    say(f, b"abc")?;
    let a = say(f, b"seal")?;
    kassert!(a.contains("\"sealed\""), "its seal: {a}");
    let a = say(f, format!("use {NAME} {good}").as_bytes())?;
    kassert!(a.contains("\"uses\":1"), "use: {a}");
    let a = say(f, format!("use {NAME} {good}").as_bytes())?;
    kassert!(a.contains("\"bad_request\""), "a part named twice: {a}");
    let a = say(f, b"status")?;
    kassert!(a.contains("\"running\""), "status after the parts: {a}");
    Ok(())
}

fn parts_node() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let ns = vfs::root_ns();
    let f = vfs::Ctx::kernel(&ns).open(None, b"/dev/bitsos/update", vfs::abi::O_RDWR, 0).map_err(|e| format!("open the node: {e}"))?;
    let r = node_steps(&f);
    drop(f);
    let _ = fs_fat::stick::with_sys(|s| {
        for e in s.store_list().unwrap_or_default() {
            if e.0.starts_with(NAME) {
                let _ = s.store_remove(&e.0);
            }
        }
    });
    r
}
ktest!("rb.install.parts_node", QemuOnly, parts_node);
