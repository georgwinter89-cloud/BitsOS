//! FS-12 host tests: ioprio values as linux-6.6/block/ioprio.c checks them, the mapping onto the request
//! priorities, the per-thread table.

use crate::prio::*;
use devmodel::block::IoPriority::*;
use sched::Class;
use uapi::errno::{EINVAL, EPERM};

#[test]
fn check_matches_linux() {
    // `ioprio_check_cap`: RT needs the capability, RT/BE levels 0..7, NONE level 0, IDLE any data, 4..7 EINVAL.
    assert_eq!(check(value(IOPRIO_CLASS_RT, 0), false), Err(-EPERM));
    assert_eq!(check(value(IOPRIO_CLASS_RT, 7), true), Ok(()));
    assert_eq!(check(value(IOPRIO_CLASS_BE, 7), false), Ok(()));
    assert_eq!(check(value(IOPRIO_CLASS_BE, 4), false), Ok(()));
    // Only the low three bits are the level; hint bits above are not checked (6.6).
    assert_eq!(check(value(IOPRIO_CLASS_BE, 8), false), Ok(()));
    assert_eq!(check(value(IOPRIO_CLASS_IDLE, 0), false), Ok(()));
    assert_eq!(check(value(IOPRIO_CLASS_IDLE, 7), false), Ok(()));
    assert_eq!(check(value(IOPRIO_CLASS_NONE, 0), false), Ok(()));
    assert_eq!(check(value(IOPRIO_CLASS_NONE, 4), false), Err(-EINVAL));
    for class in 4..8 {
        assert_eq!(check(value(class, 0), true), Err(-EINVAL), "class {class}");
    }
    assert_eq!(check(-1, true), Err(-EINVAL), "class 7 = IOPRIO_CLASS_INVALID");
    assert_eq!(class_of(value(IOPRIO_CLASS_BE, 4)), IOPRIO_CLASS_BE);
    assert_eq!(level_of(value(IOPRIO_CLASS_BE, 4)), 4);
    assert_eq!(value(IOPRIO_CLASS_IDLE, 0), 0x6000);
    assert_eq!(value(IOPRIO_CLASS_BE, 4), 0x4004);
    assert_eq!(best(value(IOPRIO_CLASS_BE, 4), value(IOPRIO_CLASS_RT, 0)), value(IOPRIO_CLASS_RT, 0));
}

#[test]
fn classes_map_onto_priorities() {
    let none = IOPRIO_DEFAULT;
    assert_eq!(map(Class::KernelRt, none), Interactive);
    assert_eq!(map(Class::Interactive, none), Interactive);
    assert_eq!(map(Class::Normal, none), Normal);
    assert_eq!(map(Class::Background, none), Background);
    let (rt, be, idle) = (value(IOPRIO_CLASS_RT, 0), value(IOPRIO_CLASS_BE, 0), value(IOPRIO_CLASS_IDLE, 0));
    // RT raises only inside a grant (the scheduling class is Interactive), never on its own.
    assert_eq!(map(Class::Interactive, rt), Interactive);
    assert_eq!(map(Class::Normal, rt), Normal);
    assert_eq!(map(Class::Background, rt), Normal);
    assert_eq!(map(Class::Interactive, be), Normal);
    assert_eq!(map(Class::Background, be), Normal);
    for c in [Class::KernelRt, Class::Interactive, Class::Normal, Class::Background] {
        assert_eq!(map(c, idle), Background);
    }
    assert_eq!(higher(Normal, Background), Normal);
    assert_eq!(higher(Normal, Interactive), Interactive);
    // No scheduler thread (host): Normal.
    assert_eq!(current(), Normal);
}

#[test]
fn table_set_get_inherit() {
    // On the host no thread is alive, so `set` keeps only the entry it writes.
    let idle = value(IOPRIO_CLASS_IDLE, 0);
    assert_eq!(raw(1001), IOPRIO_DEFAULT);
    set(1001, idle).unwrap();
    assert_eq!(raw(1001), idle);
    set(1001, value(IOPRIO_CLASS_BE, 2)).unwrap();
    assert_eq!(raw(1001), value(IOPRIO_CLASS_BE, 2));
    inherit(1001, 1002);
    assert_eq!(raw(1002), value(IOPRIO_CLASS_BE, 2), "a new thread gets its creator's value");
    set(1002, IOPRIO_DEFAULT).unwrap();
    assert_eq!(raw(1002), IOPRIO_DEFAULT);
    inherit(1002, 1003);
    assert_eq!(raw(1003), IOPRIO_DEFAULT, "NONE is not copied");
    // NONE with hint bits is a set value (Linux keeps it).
    set(1004, 1 << 3).unwrap();
    assert_eq!(raw(1004), 1 << 3);
    set(1004, IOPRIO_DEFAULT).unwrap();
}

/// FS-8b: an I/O floor raises the calling thread's requests while it is held (nested floors: the highest).
#[test]
fn a_floor_lifts_the_thread_until_dropped() {
    assert_eq!(current(), Normal);
    {
        let _a = floor(Interactive);
        assert_eq!(current(), Interactive);
        {
            let _b = floor(Background);
            assert_eq!(current(), Interactive, "a lower floor changes nothing");
        }
        assert_eq!(current(), Interactive);
        let other = std::thread::spawn(current).join().unwrap();
        assert_eq!(other, Normal, "the floor is per thread");
    }
    assert_eq!(current(), Normal);
    let l = Lift::new();
    let _h = l.hold();
    assert_eq!(l.prio(), Normal);
    let _w = l.wait_as(Interactive);
    assert_eq!(l.prio(), Interactive, "the holder is lifted while an Interactive thread waits");
}
