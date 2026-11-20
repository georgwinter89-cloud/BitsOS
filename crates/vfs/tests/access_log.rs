//! DB2: the access log on the host. One test (the rings are per CPU and this binary's threads share them): the
//! hooks allocate nothing on any of their three ways (a new slot, a fold into the last record, a full ring), a
//! repeat folds, a full ring counts what it loses but keeps room for exec and fork records, a record holds no
//! strong reference to a mount or a file, and the reader merges per program image and folder, names a forked
//! child by its parent's program and writes one JSON line per entry. The merge's ordering rules (an older exec or
//! fork read late) and its check against the live programs after a lost exec have their own tests.

use std::sync::Arc;
use vfs::abi;
use vfs::access::{self, Kind, NetAddr, NetTally, drain, ring};
use vfs::inode::Cred;
use vfs::ops::Ctx;
use vfs::MountNs;
use mm_core::host_count::heap_blocks;

#[global_allocator]
static ALLOC: mm_core::host_count::Counting = mm_core::host_count::Counting;

const PID: u32 = 4242;

fn prog(ns: &MountNs) -> Ctx<'_> {
    Ctx { ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0o022, subject: policy::Subject { program: policy::ProgramId(1), pid: PID, uid: 1000, tree: 0 } }
}

fn text_of(recs: Vec<access::Rec>, info: &dyn Fn(u32) -> Option<(Vec<u8>, u64)>) -> String {
    let mut known = std::collections::BTreeMap::new();
    drain::render(&drain::merge(recs, &mut known, info, false), true, 0, 0)
}

/// A record as a hook would make it, at time `t`.
fn rec(kind: Kind, pid: u32, t: u64, obj: access::Obj) -> access::Rec {
    access::Rec { kind, flags: 0, pid, uid: 1000, n: 1, t0: t, t1: t, tx: 0, rx: 0, obj }
}

/// An exec record of `path` made by hand (the other tests use the rings: this binary's tests run at once).
fn exe_rec(ns: &MountNs, pid: u32, t: u64, path: &[u8]) -> access::Rec {
    let p = Ctx::kernel(ns).lookup(None, path, true).unwrap();
    let seen = access::Seen { mnt: Arc::downgrade(&p.mnt), dentry: Arc::downgrade(&p.dentry), up: std::sync::Weak::new(), name: None };
    rec(Kind::Exec, pid, t, access::Obj::Path(seen))
}

#[test]
fn hooks_allocate_nothing_and_the_reader_merges_per_program_and_folder() {
    ring::prepare().unwrap();
    let ns = Arc::new(MountNs::new_empty());
    let k = Ctx::kernel(&ns);
    for d in [&b"/home"[..], b"/home/anna", b"/home/anna/Bilder", b"/bin"] {
        k.mkdir(None, d, 0o777).unwrap();
    }
    for f in [&b"/home/anna/Bilder/a.jpg"[..], b"/home/anna/Bilder/b.jpg", b"/bin/x"] {
        drop(k.open(None, f, abi::O_CREAT | abi::O_WRONLY, 0o666).unwrap());
    }
    let c = prog(&ns);
    let a = k.lookup(None, b"/home/anna/Bilder/a.jpg", true).unwrap();
    let b = k.lookup(None, b"/home/anna/Bilder/b.jpg", true).unwrap();
    let net = NetAddr::from_sockaddr(6, &[2, 0, 1, 187, 93, 184, 216, 34]).unwrap();
    let tally = NetTally::default();
    tally.set_peer(PID, 1000, net);
    drop(ring::take_all());

    // Every way through a hook, with the ring filling up and overflowing: not one heap block.
    let lost0 = access::dropped();
    let (mounts, dentries) = (Arc::strong_count(&a.mnt), Arc::strong_count(&a.dentry));
    let blocks = heap_blocks();
    for i in 0..ring::CAP + 50 {
        access::opened(&c, if i % 2 == 0 { &a } else { &b }, uapi::S_IFREG, false, false);
        access::opened(&c, &a, uapi::S_IFREG, false, false);
        access::net(Kind::Connect, PID, 1000, net);
        access::right(Kind::CapRaise, PID, 1000, 1 << 21);
    }
    tally.sent(5);
    tally.closed();
    let hook_blocks = heap_blocks() - blocks;
    assert_eq!(hook_blocks, 0, "the hooks asked the heap for {hook_blocks} blocks");
    assert!(access::dropped() > lost0, "a full ring lost nothing");
    // A record keeps the mount and the file only weakly (a strong one kept a used mount busy for umount).
    assert_eq!((Arc::strong_count(&a.mnt), Arc::strong_count(&a.dentry)), (mounts, dentries), "a record holds the mount or the file");
    // Exec and fork records still find room in the full ring.
    access::forked(PID, 1000, PID + 7);
    let full = ring::take_all();
    assert_eq!(full.len(), ring::CAP - ring::RESERVE + 1);
    assert!(full.iter().any(|r| r.kind == Kind::Fork), "the fork found no room");

    // A repeat folds into one record.
    for _ in 0..5 {
        access::opened(&c, &a, uapi::S_IFREG, false, false);
    }
    let folded = ring::take_all();
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0].n, 5);

    // Real opens as a program: reads per folder, a create with its name, a device-less folder read.
    drop(c.open(None, b"/home/anna/Bilder/a.jpg", abi::O_RDONLY, 0).unwrap());
    drop(c.open(None, b"/home/anna/Bilder/b.jpg", abi::O_RDONLY, 0).unwrap());
    drop(c.open(None, b"/home/anna/Bilder", abi::O_RDONLY | abi::O_DIRECTORY, 0).unwrap());
    drop(c.open(None, b"/home/anna/neu.txt", abi::O_CREAT | abi::O_WRONLY, 0o644).unwrap());
    drop(c.open(None, b"/home/anna/Bilder/a.jpg", abi::O_WRONLY, 0).unwrap());
    c.rename(None, b"/home/anna/neu.txt", None, b"/home/anna/alt.txt", 0).unwrap();
    c.unlink(None, b"/home/anna/alt.txt").unwrap();
    // The kernel's own opens are not a program's.
    drop(k.open(None, b"/home/anna/Bilder/a.jpg", abi::O_RDONLY, 0).unwrap());
    let x = k.lookup(None, b"/bin/x", true).unwrap();
    access::exec(PID, 1000, &x, false);
    access::forked(PID, 1000, PID + 1);
    access::right(Kind::Ptrace, PID + 1, 1000, 77);
    let info = |p: u32| (p == PID).then(|| (b"/bin/old".to_vec(), 1));
    let text = text_of(ring::take_all(), &info);
    let lines: Vec<&str> = text.lines().collect();
    let has = |w: &str| lines.iter().any(|l| l.contains(w));
    assert!(has("\"k\":\"read\",\"dir\":\"/home/anna/Bilder\",\"n\":3"), "{text}");
    // The created file was renamed and removed before the reader came: still counted in its folder, under the name
    // it had when it was made (folders are what the page shows).
    assert!(has("\"k\":\"create\",\"dir\":\"/home/anna\",\"name\":\"neu.txt\",\"n\":1"), "{text}");
    assert!(has("\"k\":\"write\",\"dir\":\"/home/anna/Bilder\",\"name\":\"a.jpg\",\"n\":1"), "{text}");
    assert!(has("\"k\":\"rename\",\"dir\":\"/home/anna\",\"n\":1"), "{text}");
    assert!(has("\"k\":\"unlink\",\"dir\":\"/home/anna\",\"n\":1"), "{text}");
    // Before its exec the pid ran /bin/old (from the live table), afterwards /bin/x; the child runs /bin/x too.
    assert!(has(&format!("{{\"p\":{PID},\"start\":1,\"uid\":1000,\"exe\":\"/bin/old\"}}")), "{text}");
    assert!(has("\"exe\":\"/bin/x\""), "{text}");
    let child = lines.iter().find(|l| l.contains(&format!("\"p\":{},", PID + 1))).expect("child program line");
    assert!(child.contains("\"exe\":\"/bin/x\""), "{child}");
    assert!(has(&format!("\"pid\":{},", PID + 1)) && has("\"k\":\"ptrace\",\"target\":77"), "{text}");
    assert!(lines.last().unwrap().starts_with("{\"s\":1,\"on\":true"), "{text}");
    assert_eq!(lines.iter().filter(|l| l.contains("\"k\":\"read\"")).count(), 1, "the kernel's own read was recorded: {text}");

    // The network line and the last batch of the full ring.
    let text = text_of(full, &info);
    assert!(text.contains("\"k\":\"connect\",\"proto\":\"tcp\",\"addr\":\"93.184.216.34\",\"port\":443"), "{text}");
    assert!(text.contains("\"k\":\"caps\",\"caps\":2097152"), "{text}");

    // Switched off: nothing is kept.
    access::set_on(false);
    drop(c.open(None, b"/home/anna/Bilder/a.jpg", abi::O_RDONLY, 0).unwrap());
    assert!(ring::take_all().is_empty());
    access::set_on(true);
}

#[test]
fn an_older_exec_or_fork_read_late_does_not_replace_a_newer_image() {
    let ns = MountNs::new_empty();
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/bin", 0o777).unwrap();
    for f in [&b"/bin/sh"[..], b"/bin/curl"] {
        drop(k.open(None, f, abi::O_CREAT | abi::O_WRONLY, 0o666).unwrap());
    }
    let (parent, child) = (9001, 9002);
    // Both still run; the child's process began at its fork (t=100).
    let info = |p: u32| match p {
        9001 => Some((b"/bin/sh".to_vec(), 10)),
        9002 => Some((b"/bin/curl".to_vec(), 100)),
        _ => None,
    };
    let mut known = std::collections::BTreeMap::new();
    known.insert(parent, (b"/bin/sh".to_vec(), 10));
    // First read: the child's exec at t=200 and a connect at t=300 (the parent's fork at t=100 waits in a ring
    // that was emptied before it arrived).
    let addr = access::Obj::Net(NetAddr::from_sockaddr(6, &[2, 0, 1, 187, 1, 2, 3, 4]).unwrap());
    let first = vec![exe_rec(&ns, child, 200, b"/bin/curl"), rec(Kind::Connect, child, 300, addr.clone())];
    drain::merge(first, &mut known, &info, false);
    // Second read: the late fork, then the child connects again.
    let second = vec![rec(Kind::Fork, parent, 100, access::Obj::Num(child as u64)), rec(Kind::Connect, child, 400, addr)];
    let text = drain::render(&drain::merge(second, &mut known, &info, false), true, 0, 0);
    assert!(text.contains(&format!("{{\"p\":{child},\"start\":200,\"uid\":1000,\"exe\":\"/bin/curl\"}}")), "{text}");
    assert!(!text.contains("/bin/sh"), "{text}");
    // An exec older than the known image does not replace it either.
    let third = vec![exe_rec(&ns, child, 150, b"/bin/sh"), rec(Kind::Connect, child, 500, access::Obj::Num(1))];
    drain::merge(third, &mut known, &info, false);
    assert_eq!(known.get(&child).map(|k| k.1), Some(200));
}

#[test]
fn after_a_lost_exec_the_live_program_names_what_follows() {
    let (parent, child) = (9101, 9102);
    // The fork was kept, the child's exec of /bin/curl (its process started at t=105) was lost.
    let live = |p: u32| match p {
        9101 => Some((b"/bin/sh".to_vec(), 10)),
        9102 => Some((b"/bin/curl".to_vec(), 105)),
        _ => None,
    };
    let recs = || vec![rec(Kind::Fork, parent, 100, access::Obj::Num(child as u64)), rec(Kind::CapRaise, child, 300, access::Obj::Num(4))];
    let mut known = std::collections::BTreeMap::new();
    let text = drain::render(&drain::merge(recs(), &mut known, &live, true), true, 0, 0);
    assert!(text.contains(&format!("{{\"p\":{child},\"start\":105,\"uid\":1000,\"exe\":\"/bin/curl\"}}")), "{text}");
    // Without a loss the table stands (before its exec the child runs its parent's program).
    let mut known = std::collections::BTreeMap::new();
    let text = drain::render(&drain::merge(recs(), &mut known, &live, false), true, 0, 0);
    assert!(text.contains(&format!("{{\"p\":{child},\"start\":10,\"uid\":1000,\"exe\":\"/bin/sh\"}}")), "{text}");
}

#[test]
fn addresses_read_as_people_write_them() {
    let v6 = NetAddr::from_sockaddr(6, &[10, 0, 0, 80, 0, 0, 0, 0, 0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap();
    assert_eq!(drain::addr_text(&v6), "2001:db8:0:0:0:0:0:1");
    assert_eq!(v6.port, 80);
    let mapped = NetAddr::from_sockaddr(6, &[10, 0, 0, 80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 100, 7]).unwrap();
    assert_eq!(drain::addr_text(&mapped), "192.168.100.7");
    assert!(NetAddr::from_sockaddr(6, &[1, 0, b'/', b'x']).is_none(), "AF_UNIX is not an inet address");
    let mut s = String::new();
    drain::json_str(&mut s, b"a\"b\\c\nd\xff");
    assert_eq!(s, "\"a\\\"b\\\\c\\u000ad\u{fffd}\"");
}
