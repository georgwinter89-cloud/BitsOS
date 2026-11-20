//! EINS-C (K6) ktest `fs.trash.scenario` (Run("fs/60-papierkorb"), called over MCP by that scenario after its shell
//! part): the trash on the boot's real /data (ext4 in the stick's data.img): a tree's removal of a folder is one
//! entry, the tree may not restore it into a guarded place, the control node's `restore` puts it back, Georg's
//! `empty` removes it; nothing is left behind.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use vfs::inode::Cred;
use vfs::ops::Ctx;

const DIR: &str = "/data/home/1000/ktest-e1";
const PID: u32 = 0x7e20_0001;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn entries(ns: &vfs::MountNs) -> Vec<Vec<u8>> {
    ns.list_dir("/data/.papierkorb").map(|l| l.into_iter().map(|(n, _)| n).collect()).unwrap_or_default()
}

fn scenario() -> TestResult {
    let ns = vfs::root_ns();
    let top = ns.lookup_path(b"/data", true).map_err(|x| format!("no /data in this boot: {x}"))?;
    kassert!(top.dentry.sb.fstype == "ext4", "/data is {}", top.dentry.sb.fstype);
    let _ = ns.remove_tree(DIR);
    let r = run(&ns);
    let _ = ns.remove_tree(DIR);
    r
}

fn run(ns: &vfs::MountNs) -> TestResult {
    let k = Ctx::kernel(ns);
    for d in [DIR, &format!("{DIR}/d")] {
        ns.mkdir_p(d).map_err(e)?;
    }
    for f in ["a", "d/b"] {
        ns.create_file_bytes(format!("{DIR}/{f}").as_bytes(), 0o644, f.as_bytes()).map_err(e)?;
    }
    for p in [String::from(DIR), format!("{DIR}/d"), format!("{DIR}/a"), format!("{DIR}/d/b")] {
        let at = ns.lookup_path(p.as_bytes(), false).map_err(e)?;
        k.setattr(&at, |m| vfs::setattr::chown(m, Cred::ROOT, 1000, 1000)).map_err(e)?;
    }
    let before = entries(ns);
    let tree = policy::tree::new(PID, time::now_ns()).ok_or("no tree")?;
    let mut t = Ctx::kernel(ns);
    t.cred = Cred { uid: 1000, gid: 1000 };
    t.subject = policy::Subject { program: policy::ProgramId(1), pid: PID, uid: 1000, tree: tree.id };
    t.unlink(None, format!("{DIR}/d/b").as_bytes()).map_err(e)?;
    t.rmdir(None, format!("{DIR}/d").as_bytes()).map_err(e)?;
    t.unlink(None, format!("{DIR}/a").as_bytes()).map_err(e)?;
    vfs::trash::end_batches_of(PID);
    let new: Vec<Vec<u8>> = entries(ns).into_iter().filter(|n| !before.contains(n)).collect();
    kassert!(new.len() == 1, "{} new entries", new.len());
    let id = String::from_utf8_lossy(&new[0]).into_owned();
    kassert!(ns.lookup(DIR).map_err(e)?.child_count() == 0, "the folder still holds something");
    let georg = vfs::trash::Caller { pid: PID + 1, tree: 0, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };
    let inside = vfs::trash::Caller { tree: tree.id, ..georg };
    kassert!(vfs::trash::command(&inside, format!("empty\n/data\n{id}\n").as_bytes()) == Err(uapi::errno::EPERM), "the tree emptied");
    // the guarded installer grants: the tree may not restore there (the kernel would make the name for it)
    let planted = vfs::trash::command(&inside, format!("restore-to\n/data\n{id}\n/data/bitsos-access\n").as_bytes());
    kassert!(planted == Err(uapi::errno::EPERM), "restore-to /data/bitsos-access gave {planted:?}");
    let said = vfs::trash::command(&georg, format!("restore\n/data\n{id}\n").as_bytes()).map_err(e)?;
    kassert!(said == b"moved=2\n", "restore said {:?}", String::from_utf8_lossy(&said));
    kassert!(ns.read_file(&format!("{DIR}/d/b"), 10).map_err(e)? == b"d/b", "d/b is not back");
    t.unlink(None, format!("{DIR}/a").as_bytes()).map_err(e)?;
    let again: Vec<Vec<u8>> = entries(ns).into_iter().filter(|n| !before.contains(n)).collect();
    kassert!(again.len() == 1, "{} entries after the second removal", again.len());
    let id = String::from_utf8_lossy(&again[0]).into_owned();
    let said = vfs::trash::command(&georg, format!("empty\n/data\n{id}\n").as_bytes()).map_err(e)?;
    kassert!(said == b"emptied=1\n", "empty said {:?}", String::from_utf8_lossy(&said));
    kassert!(entries(ns) == before, "the trash is not as before");
    Ok(())
}
ktest!("fs.trash.scenario", Run("fs/60-papierkorb"), scenario);
