//! EINS-C (K5): the access log names the assistant's tree. A program line of a process in tree 7 carries
//! `"tree":7`; every other line is as before. Its own binary: the tree hook is set once per process.

use std::sync::Arc;
use vfs::access::{self, Kind, drain};
use vfs::ops::Ctx;
use vfs::MountNs;

fn exe_rec(ns: &MountNs, pid: u32, t: u64, path: &[u8]) -> access::Rec {
    let p = Ctx::kernel(ns).lookup(None, path, true).unwrap();
    let seen = access::Seen { mnt: Arc::downgrade(&p.mnt), dentry: Arc::downgrade(&p.dentry), up: std::sync::Weak::new(), name: None };
    access::Rec { kind: Kind::Exec, flags: 0, pid, uid: 1000, n: 1, t0: t, t1: t, tx: 0, rx: 0, obj: access::Obj::Path(seen) }
}

#[test]
fn a_program_of_the_tree_says_so() {
    access::set_tree_info(|pid| if pid == 501 { 7 } else { 0 });
    let ns = MountNs::new_empty();
    ns.create_file_bytes(b"/bin/rm", 0o755, b"x").unwrap();
    ns.create_file_bytes(b"/bin/foot", 0o755, b"x").unwrap();
    let recs = vec![exe_rec(&ns, 501, 10, b"/bin/rm"), exe_rec(&ns, 502, 20, b"/bin/foot")];
    let mut known = std::collections::BTreeMap::new();
    let text = drain::render(&drain::merge(recs, &mut known, &|_| None, false), true, 0, 0);
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("{\"p\":")).collect();
    assert_eq!(lines, ["{\"p\":501,\"start\":10,\"uid\":1000,\"exe\":\"/bin/rm\",\"tree\":7}", "{\"p\":502,\"start\":20,\"uid\":1000,\"exe\":\"/bin/foot\"}"], "{text}");
}
