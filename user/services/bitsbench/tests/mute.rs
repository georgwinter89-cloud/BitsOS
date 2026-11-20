//! Nothing may be audible: the run's sound guard, and the repair after a run that never restored.

use bitsbench::ton::{self, emu::Cards};
use std::cell::RefCell;
use std::rc::Rc;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitsbench-mute-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cards(v: Vec<Option<[i64; 2]>>) -> Cards {
    Cards(Rc::new(RefCell::new(v)), Rc::new(RefCell::new(false)))
}

#[test]
fn muted_for_the_run_and_restored_after() {
    let d = tmp("run");
    // Card 0 on, card 1 missing, card 2 left channel off already.
    let c = cards(vec![Some([1, 1]), None, Some([0, 1])]);
    {
        let _m = ton::mute(&c, &d).unwrap();
        assert_eq!(*c.0.borrow(), vec![Some([0, 0]), None, Some([0, 0])]);
        assert!(d.join("ton.json").is_file(), "the old values are kept before anything is switched");
    }
    assert_eq!(*c.0.borrow(), vec![Some([1, 1]), None, Some([0, 1])], "back to what it was, not simply on");
    assert!(!d.join("ton.json").exists());
    assert_eq!(ton::describe(&c), "card0 Master Playback Switch=1,1\ncard2 Master Playback Switch=0,1\n");
}

#[test]
fn a_run_that_never_restored_is_put_right_at_the_next_start() {
    let d = tmp("crash");
    let c = cards(vec![Some([1, 0])]);
    let m = ton::mute(&c, &d).unwrap();
    std::mem::forget(m);
    assert_eq!(*c.0.borrow(), vec![Some([0, 0])]);
    assert_eq!(ton::recover(&c, &d), 1);
    assert_eq!(*c.0.borrow(), vec![Some([1, 0])]);
    assert!(!d.join("ton.json").exists());
    assert_eq!(ton::recover(&c, &d), 0, "nothing left to do");
}

#[test]
fn no_card_or_a_refused_switch_means_no_run_and_nothing_left_off() {
    let d = tmp("none");
    assert!(ton::mute(&cards(vec![]), &d).is_err());
    let c = cards(vec![Some([1, 1])]);
    *c.1.borrow_mut() = true;
    assert!(ton::mute(&c, &d).is_err(), "the write was refused");
    assert_eq!(*c.0.borrow(), vec![Some([1, 1])]);
    // The restore was refused too: ton.json stays for the next start.
    assert!(d.join("ton.json").is_file());
    *c.1.borrow_mut() = false;
    assert_eq!(ton::recover(&c, &d), 1);
}

#[test]
fn the_ioctl_numbers_are_linuxs() {
    // _IOWR('U', 0x12/0x13, struct snd_ctl_elem_value) with its 1224 bytes on x86-64.
    assert_eq!(ton::CTL_ELEM_READ, 0xc4c8_5512);
    assert_eq!(ton::CTL_ELEM_WRITE, 0xc4c8_5513);
}
