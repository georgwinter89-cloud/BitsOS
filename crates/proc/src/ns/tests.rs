//! SX-2 host tests: user namespace rules (create, map writes, setgroups, capabilities across levels) and pid
//! namespace numbering. Linux 6.6 behaviour as the ABI programs see it on CachyOS.

use super::idmap::IdMap;
use super::pid::{self, init_pid_ns};
use super::user::{self, MapKind, Opened, init_user_ns, render_map, write_map, write_setgroups};
use crate::cred::{self, Cap, Cred};
use alloc::sync::Arc;
use uapi::errno::*;

fn user(uid: u32) -> Cred {
    Cred::new(uid, uid)
}

/// `unshare(CLONE_NEWUSER)` by `c`: the new namespace and the credentials in it.
fn unshare(c: &Cred) -> (Arc<user::UserNs>, Arc<Cred>) {
    let n = Arc::new(super::set::new_user_cred(c, false).unwrap());
    (n.user_ns.clone(), n)
}

fn opened(ns: &Arc<user::UserNs>, opener: &Arc<Cred>) -> Opened {
    Opened { ns: ns.clone(), opener: opener.clone() }
}

#[test]
fn unprivileged_maps_like_linux() {
    let me = user(1000);
    let (ns, inside) = unshare(&me);
    assert_eq!((ns.level, ns.owner, ns.group), (1, 1000, 1000));
    // Nothing mapped yet: the ids read as the overflow id, the creator holds every capability in it.
    assert_eq!((ns.uid(1000), ns.gid(1000)), (65534, 65534));
    assert!(cred::ns_capable(&inside, &ns, Cap::SysAdmin) && !cred::capable(&inside, Cap::SysAdmin));
    let o = opened(&ns, &inside);
    // Root of the parent needs CAP_SETFCAP, another id CAP_SETUID over the parent: both EPERM.
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 0 1\n", &inside), Err(EPERM));
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 1001 1\n", &inside), Err(EPERM));
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 1000 2\n", &inside), Err(EPERM), "more than one id");
    assert_eq!(write_map(&o, MapKind::Uid, 1, b"0 1000 1\n", &inside), Err(EINVAL), "not at position 0");
    assert_eq!(write_map(&o, MapKind::Uid, 0, &[b'1'; 4096], &inside), Err(EINVAL), "a page or more");
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 1000\n", &inside), Err(EINVAL));
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 1000 1\n", &inside), Ok(9));
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 1000 1\n", &inside), Err(EPERM), "written once");
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"garbage", &inside), Err(EPERM), "written once beats a bad text");
    assert_eq!((ns.to_kernel(MapKind::Uid, 0), ns.uid(1000), ns.uid(0)), (Some(1000), 0, 65534));
    // The gid map needs setgroups denied first (an unprivileged writer).
    assert_eq!(write_map(&o, MapKind::Gid, 0, b"0 1000 1", &inside), Err(EPERM));
    assert!(!ns.may_setgroups());
    assert_eq!(write_setgroups(&o, 0, b"deny"), Ok(4));
    assert_eq!(write_setgroups(&o, 0, b"allow\n"), Err(EPERM), "deny is for good");
    assert_eq!(write_setgroups(&o, 0, b"deny  \n"), Ok(7));
    assert_eq!(write_setgroups(&o, 0, b"denyx"), Err(EINVAL));
    assert_eq!(write_setgroups(&o, 0, b"deny\n\n\n\n"), Err(EINVAL), "8 bytes or more");
    assert_eq!(write_map(&o, MapKind::Gid, 0, b"0 1000 1", &inside), Ok(8));
    assert_eq!(write_setgroups(&o, 0, b"deny"), Err(EPERM), "after the gid map");
    assert!(!ns.may_setgroups(), "denied: setgroups(2) stays EPERM");
    // Project ids need no capability.
    assert_eq!(write_map(&o, MapKind::Projid, 0, b"0 0 100", &inside), Ok(7));
    // The map as the process inside reads it (lower ids in the parent's numbering) and as the parent does.
    assert_eq!(render_map(&ns, MapKind::Uid, &ns), "         0       1000          1\n");
    assert_eq!(render_map(&ns, MapKind::Uid, init_user_ns()), "         0       1000          1\n");
}

#[test]
fn root_writes_ranges_and_owner_rules() {
    let root = Arc::new(user(0));
    let (ns, inside) = unshare(&root);
    // From inside, ranges need CAP_SETUID over the parent, which the namespace's creds do not have there.
    assert_eq!(write_map(&opened(&ns, &inside), MapKind::Uid, 0, b"0 0 1\n1 100000 65536\n", &inside), Err(EPERM));
    // The parent's root (newuidmap's place) has CAP_SETFCAP and CAP_SETUID there: any range.
    let o = opened(&ns, &root);
    assert_eq!(write_map(&o, MapKind::Uid, 0, b"0 0 1\n1 100000 65536\n", &root), Ok(21));
    assert_eq!(ns.to_kernel(MapKind::Uid, 7), Some(100006));
    // Allowed setgroups stays allowed with a gid map written by a privileged writer.
    assert_eq!(write_map(&o, MapKind::Gid, 0, b"0 0 65536", &root), Ok(9));
    assert!(ns.may_setgroups());
    // A nested namespace: the lower ids are the parent namespace's, mapped down to kernel ids.
    let in_ns = Cred { euid: 0, uid: 0, suid: 0, fsuid: 0, ..(*inside).clone() };
    let (inner, inner_c) = unshare(&in_ns);
    assert_eq!(inner.level, 2);
    let o2 = opened(&inner, &inner_c);
    assert_eq!(write_map(&o2, MapKind::Uid, 0, b"5 0 1", &inner_c), Ok(5));
    assert_eq!(inner.to_kernel(MapKind::Uid, 5), Some(0));
    assert_eq!(write_map(&opened(&inner, &inner_c), MapKind::Uid, 0, b"5 0 1", &inner_c), Err(EPERM));
}

#[test]
fn openers_and_capabilities_across_levels() {
    let a = Arc::new(user(1000));
    let (ns_a, in_a) = unshare(&a);
    let (ns_b, in_b) = unshare(&a);
    // The owner outside (same euid, the parent namespace) holds every capability in its child.
    assert!(cred::ns_capable(&a, &ns_a, Cap::SysAdmin));
    // A sibling namespace's process has none in the other, and may not even open-write its map.
    assert!(!cred::ns_capable(&in_b, &ns_a, Cap::SysAdmin));
    assert_eq!(write_map(&opened(&ns_a, &in_b), MapKind::Uid, 0, b"0 1000 1", &in_b), Err(EPERM));
    assert_eq!(write_setgroups(&opened(&ns_a, &in_b), 0, b"deny"), Err(EPERM));
    assert_eq!(user::may_open_setgroups(&ns_a, &in_b), Err(EPERM));
    // Another unprivileged user of the parent namespace has nothing there either.
    let other = Arc::new(user(1001));
    assert!(!cred::ns_capable(&other, &ns_a, Cap::SysAdmin));
    assert_eq!(write_map(&opened(&ns_a, &other), MapKind::Uid, 0, b"0 1000 1", &other), Err(EPERM));
    // The parent writes the child's map (opener in the parent): its own euid, one id.
    assert_eq!(write_map(&opened(&ns_a, &a), MapKind::Uid, 0, b"0 1000 1", &a), Ok(8));
    let _ = in_a;
    // A namespace's root after the map but without stored sets (an execve): the id rule gives its caps.
    let after_exec = Cred { caps: None, ..(*in_a).clone() };
    assert!(cred::ns_capable(&after_exec, &ns_a, Cap::SysAdmin) && !cred::capable(&after_exec, Cap::SysAdmin));
    assert!(!cred::ns_capable(&in_b, init_user_ns(), Cap::SysAdmin), "a child namespace has nothing in its parent");
    let _ = ns_b;
}

#[test]
fn levels_and_unmapped_creators() {
    let mut c = Arc::new(user(0));
    for level in 1..=33 {
        let (ns, inside) = unshare(&c);
        assert_eq!(ns.level, level);
        // Written from the parent side: the creator holds CAP_SETUID/CAP_SETGID there (from inside, a gid map
        // needs setgroups denied first, as on Linux).
        write_map(&opened(&ns, &c), MapKind::Uid, 0, b"0 0 1", &c).unwrap();
        assert_eq!(write_map(&opened(&ns, &inside), MapKind::Gid, 0, b"0 0 1", &inside), Err(EPERM));
        write_map(&opened(&ns, &c), MapKind::Gid, 0, b"0 0 1", &c).unwrap();
        c = inside;
    }
    assert_eq!(user::create(&c, false).map(|_| ()), Err(ENOSPC), "a 34th level");
    // A creator whose euid has no name in its namespace may not make one below.
    let (_, inside) = unshare(&user(1000));
    assert_eq!(user::create(&inside, false).map(|_| ()), Err(EPERM));
}

#[test]
fn capset_and_setuid_in_a_namespace() {
    let (_, inside) = unshare(&user(1000));
    let full = crate::prctl_state::CAP_FULL_SET;
    assert_eq!(cred::cap_sets(&inside, full), (full, full));
    // Chromium drops to CAP_SYS_ADMIN only, then to nothing.
    let admin = 1u64 << Cap::SysAdmin as u32;
    let one = cred::capset(&inside, full, [admin, admin, 0]).unwrap().unwrap();
    assert_eq!(cred::cap_sets(&one, full), (admin, admin));
    assert_eq!(cred::capset(&one, full, [full, full, 0]), Err(EPERM), "permitted cannot grow");
    let none = cred::capset(&one, full, [0, 0, 0]).unwrap().unwrap();
    assert!(!cred::ns_capable(&none, &none.user_ns, Cap::SysAdmin));
    // setuid inside needs a mapped target id (the syscall turns it into a kernel id first).
    let (ns, inside) = unshare(&user(1000));
    write_map(&opened(&ns, &inside), MapKind::Uid, 0, b"0 1000 1", &inside).unwrap();
    let k = ns.to_kernel(MapKind::Uid, 0).unwrap();
    let n = cred::set_uid(&inside, k).unwrap().unwrap();
    assert_eq!(n.euid, 1000);
}

#[test]
fn pid_numbers() {
    let init = init_pid_ns();
    let ns = pid::create(init, init_user_ns(), 0).unwrap();
    assert_eq!(ns.level, 1);
    ns.alloc(5000, None, true).unwrap();
    assert_eq!((ns.nr(5000), ns.id(1), ns.reaper()), (Some(1), Some(5000), 5000));
    ns.alloc(5001, None, false).unwrap();
    assert_eq!(ns.nr(5001), Some(2));
    assert_eq!(ns.alloc(5002, Some(2), true), Err(EEXIST));
    ns.alloc(5002, Some(40), true).unwrap();
    assert_eq!(ns.nr(5002), Some(40));
    // A namespace below: numbers on both levels; the outer level counts on.
    let inner = pid::create(&ns, init_user_ns(), 0).unwrap();
    inner.alloc(5003, None, true).unwrap();
    assert_eq!((inner.nr(5003), ns.nr(5003), inner.chain(5003)), (Some(1), Some(3), alloc::vec![1, 3]));
    assert!(ns.is_ancestor_of(&inner) && !inner.is_ancestor_of(&ns) && init.is_ancestor_of(&inner));
    assert_eq!(ns.members().len(), 4);
    // The init's end closes the namespace: no new member (ENOMEM), numbers of others still answer.
    ns.disable();
    assert_eq!(ns.alloc(5004, None, true), Err(ENOMEM));
    assert_eq!(inner.alloc(5004, None, true), Ok(()), "the inner namespace is its own");
    inner.free(5004);
    inner.free(5003);
    assert_eq!((inner.nr(5003), ns.nr(5003), inner.adding()), (None, None, false));
    for id in [5000, 5001, 5002] {
        ns.free(id);
    }
    assert!(ns.members().is_empty());
    // Numbers count on after a free (cyclic, as `idr_alloc_cyclic`); the freed one is not reused at once.
    let w = pid::create(init, init_user_ns(), 0).unwrap();
    for id in 0..3 {
        w.alloc(9000 + id, None, true).unwrap();
    }
    w.free(9001);
    w.alloc(9003, None, true).unwrap();
    assert_eq!((w.nr(9002), w.nr(9003)), (Some(3), Some(4)));
    assert_eq!(pid::create(&inner, &user::create(&user(0), false).unwrap(), 0).map(|n| n.level), Ok(3));
    assert_eq!(pid::create(&inner_root_less(), init_user_ns(), 0).map(|_| ()), Err(EINVAL));
}

/// A pid namespace owned by a child user namespace: the initial user namespace is not at or below it.
fn inner_root_less() -> Arc<pid::PidNs> {
    let (uns, _) = unshare(&user(1000));
    pid::create(init_pid_ns(), &uns, 0).unwrap()
}

#[test]
fn idmap_identity_of_init() {
    let i = init_user_ns();
    assert_eq!((i.uid(1234), i.to_kernel(MapKind::Gid, 7), i.to_kernel(MapKind::Uid, u32::MAX)), (1234, Some(7), None));
    assert_eq!(IdMap::identity().extents().len(), 1);
}

/// `inc_ucount`: a charge past the limit is ENOSPC, dropping charges gives them back, and the initial level
/// counts nested namespaces for the owner of the outermost one.
#[test]
fn namespace_counts() {
    use super::count::{Counted, charge, used, used_in};
    // A namespace of its own with a limit of 2 for net namespaces: the charges at its level stop there.
    let uid = 0x5a2_0001;
    let (ns, _) = unshare(&user(uid));
    ns.limits.set(Counted::Net, 2).unwrap();
    let a = charge(Counted::Net, &ns, 7).unwrap();
    let b = charge(Counted::Net, &ns, 7).unwrap();
    assert_eq!(charge(Counted::Net, &ns, 7).map(|_| ()), Err(ENOSPC), "the third past a limit of 2");
    assert_eq!(charge(Counted::Uts, &ns, 7).map(|_| ()), Ok(()), "another kind counts apart");
    assert_eq!(used_in(ns.inum, Counted::Net, 7), 2);
    assert_eq!(used(Counted::Net, uid), 2, "the initial level counts them for the namespace's owner");
    drop((a, b));
    assert_eq!(used_in(ns.inum, Counted::Net, 7), 0);
    assert!(charge(Counted::Net, &ns, 7).is_ok(), "room again");
    drop(ns);
    // A user namespace of uid 0x5a2_0002 and one nested in it (made there by its root): both count for 0x5a2_0002.
    let owner = 0x5a2_0002;
    let (outer, inside) = unshare(&user(owner));
    assert_eq!(used(Counted::User, owner), 1, "the outer namespace is charged to its creator");
    let _ = inside;
    drop(outer);
}

/// SX-3b: every level counts against its own namespace's limit (`inc_ucount`), as bwrap's --disable-userns
/// relies on: a limit of 1 written in a namespace allows one namespace made there per user, and a refused charge
/// gives the levels it already took back.
#[test]
fn namespace_limits_per_level() {
    use super::count::{Counted, INT_MAX, charge, used, used_in};
    let uid = 0x5a3_0001;
    let (l1, _) = unshare(&user(uid));
    assert_eq!(l1.limits.get(Counted::User), INT_MAX, "a new namespace starts at INT_MAX");
    assert_eq!(l1.limits.set(Counted::User, INT_MAX + 1), Err(uapi::errno::EINVAL));
    l1.limits.set(Counted::User, 1).unwrap();
    let before = used(Counted::User, uid);
    let a = charge(Counted::User, &l1, uid).unwrap();
    assert_eq!(used_in(l1.inum, Counted::User, uid), 1);
    assert_eq!(used(Counted::User, uid), before + 1, "the initial level counts it for the owner of l1");
    assert_eq!(charge(Counted::User, &l1, uid).map(|_| ()), Err(ENOSPC), "past l1's limit of 1");
    assert_eq!(used(Counted::User, uid), before + 1, "the refused charge gave the initial level back");
    assert!(charge(Counted::Pid, &l1, uid).is_ok(), "another kind has its own limit");
    let other = charge(Counted::User, &l1, uid + 1).unwrap();
    assert_eq!(used_in(l1.inum, Counted::User, uid + 1), 1, "another user in l1 counts apart");
    drop((a, other));
    assert_eq!(used_in(l1.inum, Counted::User, uid), 0);
    assert_eq!(used(Counted::User, uid), before);
}
