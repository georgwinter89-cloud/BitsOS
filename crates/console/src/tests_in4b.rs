//! Host tests of the IN-4 fixer round: hostile ioctl arguments (faulting pointers, huge and zero sizes), the VT
//! ioctls of `vtioctl`, the orphaned-group background check, the held echo bound, `RecordMem`'s bounds, and the
//! soft autorepeat evdev clients get.

use crate::evdev::{EV_KEY, EV_SYN, EvdevSink, SYN_REPORT};
use crate::evdevfs::{Handle, ioc};
use crate::ldisc::{HELD_ECHO_MAX, Ldisc};
use crate::termios::{Termios, ioc::*};
use crate::tests_in4::{BASE, Fake, Mem, io, keyboard_on, read_all};
use crate::tty::Tty;
use crate::vtioctl::*;
use uapi::errno::*;
use vfs::sys::UserMem;

/// An address no fake memory has.
const BAD: u64 = 0x10;
const MS: u64 = 1_000_000;

#[test]
fn every_terminal_ioctl_with_a_bad_pointer_is_efault_and_changes_nothing() {
    let tty = Tty::new();
    let p = Fake::new(&[(100, 100, 100)], 100);
    let m = Mem::new();
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 0), Ok(0));
    let before = tty.termios();
    let cmds = [
        TCGETS, TCSETS, TCSETSW, TCSETSF, TCGETS2, TCSETS2, TCSETSW2, TCSETSF2, TCGETA, TCSETA, TCSETAW, TCSETAF, TIOCGWINSZ,
        TIOCSWINSZ, TIOCGEXCL, TIOCGETD, TIOCSETD, TIOCGDEV, TIOCINQ, TIOCOUTQ, TIOCGSOFTCAR, TIOCSSOFTCAR, TIOCGPGRP,
        TIOCSPGRP, TIOCGSID, KDGETMODE, KDGKBMODE, KDGKBTYPE, VT_GETSTATE, VT_OPENQRY, VT_GETMODE, VT_SETMODE,
        VT_SETACTIVATE, VT_GETHIFONTMASK, KDGKBLED, KDGETLED, KDGKBMETA, TIOCLINUX,
    ];
    for cmd in cmds {
        assert_eq!(io(&tty, &p, &m, cmd, BAD), Err(EFAULT), "cmd {cmd:#x}");
        assert_eq!(io(&tty, &p, &m, cmd, u64::MAX - 1), Err(EFAULT), "cmd {cmd:#x} at the top of the address space");
    }
    assert_eq!(tty.termios(), before, "no faulting call changed the settings");
    assert_eq!(tty.winsize(), None, "nor the window size");
    // A bad command number: ENOTTY, never EFAULT (the argument is not looked at).
    assert_eq!(io(&tty, &p, &m, 0x5604, BAD), Err(ENOTTY), "VT_SENDSIG is not in Linux 6.6's vt_ioctl");
    assert_eq!(io(&tty, &p, &m, 0xFFFF_FFFF, BAD), Err(ENOTTY));
}

#[test]
fn vt_ioctls_with_one_vt() {
    let tty = Tty::new();
    let p = Fake::new(&[(100, 100, 100), (200, 200, 200)], 200);
    let m = Mem::new();
    // Without the terminal (and without privilege): the state queries work, the changes do not.
    assert_eq!(io(&tty, &p, &m, VT_SETMODE, BASE), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, VT_ACTIVATE, 1), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, VT_RELDISP, 1), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, KDSKBLED, 0), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, KDSETLED, 0), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, KIOCSOUND, 0), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, KDKBDREP, BASE), Err(EPERM), "KDKBDREP needs CAP_SYS_TTY_CONFIG");
    assert_eq!(io(&tty, &p, &m, VT_LOCKSWITCH, 0), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, TIOCLINUX, BASE), Err(EPERM));
    m.put(0, &[0xEE; 8]);
    assert_eq!(io(&tty, &p, &m, VT_GETSTATE, BASE), Ok(0));
    assert_eq!(m.at(0, 6), [1, 0, 0xEE, 0xEE, 3, 0], "v_active 1, v_signal untouched, v_state 0b11");
    assert_eq!(io(&tty, &p, &m, VT_OPENQRY, BASE), Ok(0));
    assert_eq!(m.i32_at(0), -1, "the one VT is in use");
    assert_eq!(io(&tty, &p, &m, VT_GETMODE, BASE), Ok(0));
    assert_eq!(m.at(0, 8), [0; 8], "VT_AUTO");
    assert_eq!(io(&tty, &p, &m, VT_DISALLOCATE, 1), Err(EBUSY));
    assert_eq!(io(&tty, &p, &m, VT_DISALLOCATE, 5), Ok(0));
    assert_eq!(io(&tty, &p, &m, VT_DISALLOCATE, 64), Err(ENXIO));
    assert_eq!(io(&tty, &p, &m, KDGKBMETA, BASE), Ok(0));
    assert_eq!(m.i32_at(0), K_METABIT as i32);
    assert_eq!(io(&tty, &p, &m, KDSKBMETA, 7), Err(EINVAL));
    assert_eq!(io(&tty, &p, &m, VT_GETHIFONTMASK, BASE), Ok(0));
    assert_eq!(m.at(0, 2), [0, 0]);
    // With the terminal: the mode is stored, VT 1 is active, other VTs cannot be had.
    p.be(100);
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 0), Ok(0));
    assert_eq!(io(&tty, &p, &m, VT_RELDISP, 1), Err(EINVAL), "not in VT_PROCESS mode");
    m.put(16, &[5, 0, 10, 0, 12, 0, 0, 0]);
    assert_eq!(io(&tty, &p, &m, VT_SETMODE, BASE + 16), Err(EINVAL), "mode 5");
    m.put(16, &[VT_PROCESS, 0, 10, 0, 12, 0, 9, 0]);
    assert_eq!(io(&tty, &p, &m, VT_SETMODE, BASE + 16), Ok(0));
    assert_eq!(io(&tty, &p, &m, VT_GETMODE, BASE + 32), Ok(0));
    assert_eq!(m.at(32, 8), [VT_PROCESS, 0, 10, 0, 12, 0, 0, 0], "frsig is dropped");
    assert_eq!(io(&tty, &p, &m, VT_RELDISP, VT_ACKACQ), Ok(0));
    assert_eq!(io(&tty, &p, &m, VT_RELDISP, 1), Err(EINVAL), "no switch is pending");
    assert_eq!(io(&tty, &p, &m, VT_ACTIVATE, 1), Ok(0));
    assert_eq!(io(&tty, &p, &m, VT_WAITACTIVE, 1), Ok(0));
    assert_eq!(io(&tty, &p, &m, VT_ACTIVATE, 2), Err(ENXIO));
    assert_eq!(io(&tty, &p, &m, VT_ACTIVATE, 0), Err(ENXIO));
    assert_eq!(io(&tty, &p, &m, KIOCSOUND, 0), Ok(0));
    assert_eq!(io(&tty, &p, &m, KDSKBLED, 0x80), Err(EINVAL));
    assert_eq!(io(&tty, &p, &m, KDSETLED, 0xFF), Ok(0), "lights follow the flags");
    m.put(48, &[12]);
    assert_eq!(io(&tty, &p, &m, TIOCLINUX, BASE + 48), Ok(0), "TIOCL_GETFGCONSOLE: VT index 0");
    m.put(48, &[99]);
    assert_eq!(io(&tty, &p, &m, TIOCLINUX, BASE + 48), Err(EINVAL));
    m.put(48, &[7]);
    assert_eq!(io(&tty, &p, &m, TIOCLINUX, BASE + 48), Ok(0));
    assert_eq!(m.at(48, 1), [0], "no mouse reporting");
    // Back to VT_AUTO for the other tests (the mode is the VT's, global).
    m.put(16, &[VT_AUTO, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(io(&tty, &p, &m, VT_SETMODE, BASE + 16), Ok(0));
    // KDKBDREP by root: delay and period 0 only ask.
    let root = Fake::new(&[(1, 1, 1)], 1).with_uid(0);
    m.put(64, &[0; 8]);
    assert_eq!(io(&tty, &root, &m, KDKBDREP, BASE + 64), Ok(0));
    let (d, r) = crate::repeat::rate_ms();
    assert_eq!((m.i32_at(64), m.i32_at(68)), (d as i32, r as i32));
}

#[test]
fn background_check_of_an_orphaned_group_is_eio() {
    let p = Fake::new(&[(100, 100, 100), (101, 101, 100)], 100);
    let mut ctl = crate::jobs::Ctl::default();
    let me = p.caller(100);
    ctl.sctty(me.as_ref(), 0, true).unwrap();
    ctl.pgrp = Some(101);
    p.orphaned.lock().push(100);
    let mut send = crate::jobs::Send::default();
    assert_eq!(ctl.check_change(me.as_ref(), &p, crate::jobs::SIGTTOU, &mut send), Err(EIO));
    assert_eq!(ctl.check_change(me.as_ref(), &p, crate::jobs::SIGTTIN, &mut send), Err(EIO));
    assert!(send.to.is_empty(), "no signal nobody could ever answer");
    // An ignored SIGTTOU still lets the change through first (is_ignored comes before the orphan check).
    p.ignored.lock().push(crate::jobs::SIGTTOU);
    assert_eq!(ctl.check_change(me.as_ref(), &p, crate::jobs::SIGTTOU, &mut send), Ok(()));
}

#[test]
fn held_echo_is_bounded_while_output_is_stopped() {
    let mut l = Ldisc::new(Termios::vt());
    l.publish();
    l.tco(true);
    let mut lost = 0;
    for _ in 0..5000 {
        // Type and erase: the input buffer stays empty, the echo grows ("a", then "\b \b").
        let fx = l.receive(b"a\x7f", false);
        lost += fx.echo_lost as usize;
        l.publish();
    }
    assert_eq!(lost, 1, "one warning per stop");
    assert!(l.tco(false));
    let held = l.take_held_echo();
    assert_eq!(held.len(), HELD_ECHO_MAX, "the oldest echo was dropped, the newest kept");
    assert!(held.ends_with(b"a\x08 \x08"));
    // The next stop warns again.
    l.tco(true);
    let mut lost = 0;
    for _ in 0..5000 {
        lost += l.receive(b"a\x7f", false).echo_lost as usize;
    }
    assert_eq!(lost, 1);
}

#[test]
fn record_mem_keeps_to_the_answer_struct() {
    // Without a process hook (host): writes outside the struct go to the caller, which fails here.
    let m = crate::RecordMem::new(0x1000, 16);
    assert!(m.write(0x1000, &[1; 16]));
    assert!(!m.write(0x1008, &[2; 16]), "past the struct: not recorded at the wrong place");
    assert!(!m.write(0x0FFF, &[3]), "below it");
    assert!(!m.write(u64::MAX - 2, &[4; 8]), "an end that overflows: refused, no panic");
    let m2 = crate::RecordMem::new(u64::MAX - 4, 16);
    assert!(!m2.write(u64::MAX - 1, &[5; 8]), "base near the top: the end overflows");
    assert_eq!(m.take(), [1; 16]);
    let e = crate::RecordMem::for_ioc(0x2000, ioc::EVIOCGMASK);
    assert!(e.write(0x2000, &[0; 16]));
    assert!(!e.write(0x2010, &[0; 1]), "EVIOCGMASK's struct is 16 bytes");
}

#[test]
fn evdev_ioctls_with_hostile_arguments() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard_on(&S, "hostile kbd");
    let h = Handle::open(&k).unwrap();
    let m = Mem::new();
    let cmds = [
        ioc::EVIOCGVERSION, ioc::EVIOCGID, ioc::EVIOCGREP, ioc::EVIOCSREP, ioc::EVIOCGEFFECTS, ioc::EVIOCSCLOCKID, ioc::EVIOCGMASK,
        ioc::EVIOCSMASK, ioc::EVIOCGKEYCODE, ioc::EVIOCSKEYCODE, ioc::EVIOCGKEYCODE_V2, ioc::ior(ioc::NAME, 64), ioc::ior(ioc::PHYS, 64),
        ioc::ior(ioc::UNIQ, 64), ioc::ior(ioc::BIT, 8), ioc::ior(ioc::BIT + 1, 96), ioc::ior(ioc::KEY, 96), ioc::ior(ioc::LED, 8),
        ioc::ior(ioc::PROP, 8), ioc::iow(ioc::EVIOCSFF_NR, 48),
    ];
    for cmd in cmds {
        assert_eq!(h.ioctl(cmd, BAD, &m), Err(EFAULT), "cmd {cmd:#x}");
        assert_eq!(h.ioctl(cmd, u64::MAX - 3, &m), Err(EFAULT), "cmd {cmd:#x} at the top");
    }
    // Zero-length variable reads copy nothing and cannot fault (Linux answers 0).
    assert_eq!(h.ioctl(ioc::ior(ioc::NAME, 0), BAD, &m), Ok(0));
    assert_eq!(h.ioctl(ioc::ior(ioc::BIT + 1, 0), BAD, &m), Ok(0));
    // Types and commands outside the table.
    assert_eq!(h.ioctl(ioc::ior(ioc::BIT + 0x1e, 8), BASE, &m), Err(EINVAL), "no such event type");
    assert_eq!(h.ioctl(ioc::ior(0x7f, 8), BASE, &m), Err(EINVAL));
    assert_eq!(h.ioctl(0x5401, BASE, &m), Err(EINVAL), "a terminal ioctl on an event node");
    // EVIOCGMASK with codes_size 4 GiB - 1: the mask, then zeros until the memory ends: EFAULT, no huge
    // allocation. Linux writes the same (`clear_user` of the rest).
    let mut mask = [0u8; 16];
    mask[0..4].copy_from_slice(&2u32.to_le_bytes());
    mask[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    mask[8..16].copy_from_slice(&(BASE + 1024).to_le_bytes());
    m.put(0, &mask);
    assert_eq!(h.ioctl(ioc::EVIOCGMASK, BASE, &m), Err(EFAULT));
    assert_eq!(m.at(1024, 8), [0xFF; 8], "REL has no mask: all bits");
    assert_eq!(m.at(1032, 64), [0; 64], "then zeros");
    // A codes_size bigger than the mask but inside memory: mask, zeros, Ok.
    mask[4..8].copy_from_slice(&24u32.to_le_bytes());
    m.put(1024, &[0xEE; 32]);
    m.put(0, &mask);
    assert_eq!(h.ioctl(ioc::EVIOCGMASK, BASE, &m), Ok(0));
    assert_eq!(m.at(1024, 32), [[0xFF; 8].as_slice(), &[0; 16], &[0xEE; 8]].concat());
    // An unknown type: only zeros (cnt 0).
    mask[0..4].copy_from_slice(&0x1fu32.to_le_bytes());
    mask[4..8].copy_from_slice(&8u32.to_le_bytes());
    m.put(0, &mask);
    assert_eq!(h.ioctl(ioc::EVIOCGMASK, BASE, &m), Ok(0));
    assert_eq!(m.at(1024, 8), [0; 8]);
    drop(h);
    k.unregister(1);
}

#[test]
fn evdev_clients_get_soft_autorepeat() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard_on(&S, "repeat kbd");
    let a = Handle::open(&k).unwrap();
    let b = Handle::open(&k).unwrap();
    let m = Mem::new();
    let t0 = 1_000 * MS;
    k.emit(t0, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    read_all(&a);
    read_all(&b);
    // The device's delay (500 ms), then its period (33 ms): EV_KEY value 2 and SYN_REPORT value 1.
    assert_eq!(a.repeat_due(t0 + 499 * MS), Some(t0 + 500 * MS));
    assert_eq!(read_all(&a), []);
    assert_eq!(a.repeat_due(t0 + 500 * MS), Some(t0 + 533 * MS));
    assert_eq!(read_all(&a), [(EV_KEY, 30, 2), (EV_SYN, SYN_REPORT, 1)]);
    // A grab gives the repeats to the grabbing client alone.
    assert_eq!(b.ioctl(ioc::EVIOCGRAB, 1, &m), Ok(0));
    assert_eq!(a.repeat_due(t0 + 533 * MS), Some(t0 + 566 * MS));
    assert_eq!(read_all(&a), [], "not the grabber");
    assert_eq!(b.repeat_due(t0 + 533 * MS), Some(t0 + 566 * MS));
    assert_eq!(read_all(&b), [(EV_KEY, 30, 2), (EV_SYN, SYN_REPORT, 1)]);
    assert_eq!(b.ioctl(ioc::EVIOCGRAB, 0, &m), Ok(0));
    // A release stops it.
    k.emit(t0 + 600 * MS, &[(EV_KEY, 30, 0), (EV_SYN, SYN_REPORT, 0)]);
    read_all(&a);
    assert_eq!(a.repeat_due(t0 + 2_000 * MS), None);
    assert_eq!(read_all(&a), []);
    // EVIOCSREP: period 0 turns it off.
    m.put(0, &[500u32.to_le_bytes(), 0u32.to_le_bytes()].concat());
    assert_eq!(a.ioctl(ioc::EVIOCSREP, BASE, &m), Ok(0));
    k.emit(t0 + 3_000 * MS, &[(EV_KEY, 31, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(a.repeat_due(t0 + 9_000 * MS), None);
    // A device that let go of the key without the client seeing it (unplug) does not repeat.
    m.put(0, &[100u32.to_le_bytes(), 20u32.to_le_bytes()].concat());
    assert_eq!(a.ioctl(ioc::EVIOCSREP, BASE, &m), Ok(0));
    k.emit(t0 + 10_000 * MS, &[(EV_KEY, 31, 0), (EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    read_all(&a);
    k.unregister(t0 + 10_050 * MS);
    assert_eq!(a.repeat_due(t0 + 10_200 * MS), None);
}
