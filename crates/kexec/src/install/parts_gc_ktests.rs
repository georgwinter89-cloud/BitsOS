//! UPD-4 in-kernel tests of the parts' cleanup and repair on the stick image of this run (QemuOnly: they make test
//! generations far above any real one and test parts below `\BITSOS2\SYS\STORE\`, and take everything out again):
//! - `rb.install.parts_gc`: two slots whose FILES name their own parts and one shared part: the cleanup keeps every
//!   part a slot names, removes the part no slot names and an unfinished `.NEW` folder; once one slot is pruned its
//!   own part goes and the shared one stays.
//! - `rb.install.parts_present_held`: a sealed part no slot names yet (left by an update that stopped before its
//!   install) that `part` finds present is held from then on, so another part's cleanup never removes it.
//! - `rb.install.parts_repair`: a damaged record (empty, as a power cut can leave it) does not block the part for
//!   good: the part is written again and sealed with a whole record.

use super::ktests::mounted;
use super::parts::{self, Begun, Writer};
use super::parts_rules::{self as rules, List};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fs_fat::sysapi::Sys;
use ktest::{TestResult, kassert, ktest};
use mcp::sha256::{self, Sha256};

const PREFIX: &str = "ktestgc";
/// Test generations: far above any real one, below fs-fat's highest number.
const SLOT_A: u32 = 99_999_991;
const SLOT_B: u32 = 99_999_992;

fn hash_of(files: &[(&str, &[u8])]) -> String {
    let mut h = Sha256::default();
    for (p, b) in files {
        let mut f = Sha256::default();
        f.update(b);
        h.update(rules::line(&sha256::hex(&f.finish()), b.len() as u64, p).as_bytes());
    }
    sha256::hex(&h.finish())
}

/// Writes `files` as part `name` and seals it; its hash.
pub(super) fn put(s: &Sys<'_>, id: u64, name: &str, files: &[(&str, &[u8])]) -> Result<String, String> {
    let hash = hash_of(files);
    let mut l = List::default();
    for (p, b) in files {
        l.add(p, false, b.len() as u64)?;
    }
    let mut w = match Writer::begin(s, id, &[], name, &hash, l.count, l.dirs(), l.bytes)? {
        Begun::Present(_) => return Ok(hash),
        Begun::Writing(w) => w,
    };
    for (p, b) in files {
        if let Err(t) = w.file(s, b.len() as u64, p).and_then(|more| if more { w.take(s, b).map(|_| ()) } else { Ok(()) }) {
            w.abort(s);
            return Err(t);
        }
    }
    w.seal(s).map(|_| hash)
}

fn names(parts: &[(&str, &String)]) -> Vec<u8> {
    let mut v: Vec<(String, String)> = parts.iter().map(|(n, h)| (String::from(*n), (*h).clone())).collect();
    v.sort();
    rules::files_render(&v).into_bytes()
}

fn slot(s: &Sys<'_>, n: u32, parts: &[(&str, &String)]) -> Result<(), String> {
    s.create_generation(n).map_err(|e| format!("making test generation {n}: {e}"))?;
    s.write_file(n, rules::FILES, &names(parts)).map_err(|e| format!("its FILES: {e}"))
}

fn clean(s: &Sys<'_>) {
    for n in [SLOT_A, SLOT_B] {
        let _ = s.remove_generation(n);
    }
    for e in s.store_list().unwrap_or_default() {
        if e.0.starts_with(PREFIX) {
            let _ = s.store_remove(&e.0);
        }
    }
}

fn run(body: impl FnOnce(&Sys<'_>) -> TestResult) -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let r = fs_fat::stick::with_sys(|s| {
        clean(s);
        let r = body(s);
        clean(s);
        r
    });
    r.map_err(|e| format!("no boot medium: {e}"))?
}

fn parts_gc() -> TestResult {
    run(|s| {
        let id = parts::session_id();
        let (a, b, shared, lone) = (format!("{PREFIX}-a"), format!("{PREFIX}-b"), format!("{PREFIX}-s"), format!("{PREFIX}-o"));
        let ha = put(s, id, &a, &[("x/a", b"slot a\n")])?;
        let hb = put(s, id, &b, &[("b", b"slot b\n")])?;
        let hs = put(s, id, &shared, &[("s/t/u", b"both\n")])?;
        let ho = put(s, id, &lone, &[("o", b"nobody\n")])?;
        parts::release(id);
        slot(s, SLOT_A, &[(&a, &ha), (&shared, &hs)])?;
        slot(s, SLOT_B, &[(&b, &hb), (&shared, &hs)])?;
        // An unfinished part nobody holds (an update that died while writing).
        let stale = rules::new_folder(&format!("{PREFIX}-n"), &ho);
        s.store_mkdir(&format!("{stale}/f")).map_err(|e| format!("making {stale}: {e}"))?;
        parts::gc(s, &[])?;
        for (n, h) in [(&a, &ha), (&b, &hb), (&shared, &hs)] {
            kassert!(parts::present(s, n, h).is_some(), "the cleanup removed {n}, which a kept slot names");
        }
        kassert!(parts::present(s, &lone, &ho).is_none(), "the cleanup kept {lone}, which no slot names");
        let l = s.store_list().map_err(|e| format!("listing STORE: {e}"))?;
        kassert!(!l.iter().any(|e| e.0 == stale), "the cleanup kept the unfinished {stale}");
        kassert!(!l.iter().any(|e| e.0 == rules::record_name(&lone, &ho)), "the record of {lone} stayed");
        // Slot B pruned: its own part goes, the shared one stays for slot A.
        s.remove_generation(SLOT_B).map_err(|e| format!("removing test generation {SLOT_B}: {e}"))?;
        parts::gc(s, &[])?;
        kassert!(parts::present(s, &b, &hb).is_none(), "the pruned slot's own part stayed");
        kassert!(parts::present(s, &shared, &hs).is_some() && parts::present(s, &a, &ha).is_some(), "the cleanup removed a part slot {SLOT_A} names");
        Ok(())
    })
}
ktest!("rb.install.parts_gc", QemuOnly, parts_gc);

fn parts_present_held() -> TestResult {
    run(|s| {
        let first = parts::session_id();
        let (left, next) = (format!("{PREFIX}-left"), format!("{PREFIX}-next"));
        let files: [(&str, &[u8]); 1] = [("left", b"sealed by an update that stopped\n")];
        let hl = put(s, first, &left, &files)?;
        parts::release(first);
        // The next update finds it present: from then on it is that update's.
        let id = parts::session_id();
        let found = matches!(Writer::begin(s, id, &[], &left, &hl, 1, 0, files[0].1.len() as u64)?, Begun::Present(_));
        kassert!(found, "a sealed part was not found present");
        // Another part of the same update; its cleanup (as when the medium is full) leaves the present one alone.
        put(s, id, &next, &[("n", b"next\n")])?;
        parts::gc(s, &[])?;
        let kept = parts::present(s, &left, &hl).is_some();
        parts::release(id);
        kassert!(kept, "the cleanup removed a present part the update had found and holds");
        Ok(())
    })
}
ktest!("rb.install.parts_present_held", QemuOnly, parts_present_held);

fn parts_repair() -> TestResult {
    run(|s| {
        let id = parts::session_id();
        let name = format!("{PREFIX}-r");
        let files: [(&str, &[u8]); 2] = [("a", b"repair\n"), ("d/b", b"me\n")];
        let h = put(s, id, &name, &files)?;
        let rec = rules::record_name(&name, &h);
        s.store_remove(&rec).map_err(|e| format!("removing the record: {e}"))?;
        s.store_put(&rec, b"").map_err(|e| format!("writing an empty record: {e}"))?;
        kassert!(parts::present(s, &name, &h).is_none(), "a part with an empty record counts as present");
        let again = put(s, id, &name, &files);
        parts::release(id);
        kassert!(again.is_ok(), "a damaged record blocks the part: {again:?}");
        kassert!(parts::present(s, &name, &h).is_some_and(|p| p.files == 2), "the part is not whole again");
        Ok(())
    })
}
ktest!("rb.install.parts_repair", QemuOnly, parts_repair);
