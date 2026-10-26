extern crate std;
use super::*;
use crate::linux::Space;
use crate::vma::VmaKind;
use std::collections::BTreeMap;

/// The pure model: the Linux-semantics layer plus a byte store for page contents.
struct Model {
    s: Space,
    bytes: BTreeMap<u64, u8>,
}

impl Model {
    fn drop_bytes(&mut self, lo: u64, hi: u64) {
        let keys: Vec<u64> = self.bytes.range(lo..hi).map(|(k, _)| *k).collect();
        for k in keys {
            self.bytes.remove(&k);
        }
    }
}

impl MemApi for Model {
    fn mmap(&mut self, addr: u64, len: u64, prot: u64, flags: u64) -> Result<u64, Errno> {
        let p = self.s.plan_mmap(addr, len, prot, flags, 0)?;
        self.drop_bytes(p.addr, p.addr + p.len);
        self.s.commit_mmap(&p, VmaKind::Anonymous)?;
        Ok(p.addr)
    }
    fn munmap(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        let (s, e) = self.s.plan_munmap(addr, len)?;
        self.s.commit_munmap(s, e)?;
        self.drop_bytes(s, e);
        Ok(())
    }
    fn mprotect(&mut self, addr: u64, len: u64, prot: u64) -> Result<(), Errno> {
        let Some(p) = self.s.plan_mprotect(addr, len, prot)? else { return Ok(()) };
        if p.applied_end > p.start {
            self.s.commit_mprotect(&p)?;
        }
        p.error.map_or(Ok(()), Err)
    }
    fn brk(&mut self, addr: u64) -> u64 {
        let p = self.s.plan_brk(addr);
        if let crate::linux::BrkPlan::Move { unmap: Some((s, e)), .. } = p {
            self.drop_bytes(s, e);
        }
        self.s.commit_brk(&p).unwrap()
    }
    fn store(&mut self, addr: u64, byte: u8) -> bool {
        let ok = self.s.tree.find(addr).is_some_and(|v| v.prot.write);
        if ok {
            self.bytes.insert(addr, byte);
        }
        ok
    }
    fn load(&mut self, addr: u64) -> Option<u8> {
        self.s.tree.find(addr).filter(|v| v.prot.accessible())?;
        Some(self.bytes.get(&addr).copied().unwrap_or(0))
    }
    fn ranges(&self, lo: u64, hi: u64) -> Vec<(u64, u64, Prot)> {
        self.s.tree.overlapping(lo, hi).iter().map(|v| (v.start, v.end, v.prot)).collect()
    }
}

#[test]
fn the_model_matches_linux() {
    let heap = 0x10_0000_0000;
    let mut m = Model { s: Space::new(crate::MMAP_TOP), bytes: BTreeMap::new() };
    m.s.init_brk(heap);
    let got = run(SCRIPT, &mut m, heap);
    if let Some(d) = first_difference(&got, LINUX_EXPECTED) {
        panic!("{d}\n--- model output ---\n{got}");
    }
}

#[test]
fn differences_are_reported_by_line() {
    assert_eq!(first_difference("a\nb\n", "a\nb\n"), None);
    assert_eq!(first_difference("a\nc\n", "a\nb\n").unwrap(), "line 2: got `c`, Linux `b`");
    assert!(first_difference("a\n", "a\nb\n").unwrap().contains("<end>"));
}

#[test]
fn bad_lines_are_marked_not_skipped() {
    let mut m = Model { s: Space::new(crate::MMAP_TOP), bytes: BTreeMap::new() };
    let out = run("mmap B 0x1000 rw private|anon|bogus\nfrobnicate\n", &mut m, 0);
    assert_eq!(out.matches("?? bad line").count(), 2);
}
