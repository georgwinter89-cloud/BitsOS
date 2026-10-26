//! Host tests of IN-7's pseudo-terminals: the slave node and its lock, both directions with echo and OPOST, flow
//! control without loss, the hang-up rules of both ends, the master's ioctls and poll.

use crate::ptm::MasterFile;
use crate::pts::SlaveFile;
use crate::pty::{self, PtmxDev, TO_MASTER_MAX, TO_SLAVE_MAX, new_encode_dev};
use crate::termios::{Termios, Winsize, flag, ioc::*};
use crate::tests_in4::{BASE, Fake, Mem};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::{Events, File};

const USER: vfs::inode::Cred = vfs::inode::Cred { uid: 1000, gid: 1000 };
const RDWR: u64 = 2;
const NOCTTY: u64 = 0o400;
const NB: bool = true;

fn no() -> bool {
    false
}

/// A master, unlocked (unlockpt), and its index.
fn open() -> (Arc<MasterFile>, u32) {
    let m = PtmxDev::open_master(USER).expect("ptmx");
    let (p, mem) = (Fake::new(&[], 0), Mem::new());
    assert_eq!(m.ioctl(TIOCGPTN, BASE, &mem, &p), Ok(0));
    let idx = mem.i32_at(0) as u32;
    mem.put(0, &0i32.to_le_bytes());
    assert_eq!(m.ioctl(TIOCSPTLCK, BASE, &mem, &p), Ok(0));
    (m, idx)
}

fn read_m(m: &MasterFile) -> Vec<u8> {
    let mut out = Vec::new();
    let mut b = [0u8; 1024];
    while let Ok(n) = m.read(&mut b, NB, &no) {
        out.extend_from_slice(&b[..n]);
    }
    out
}

fn read_s(s: &SlaveFile) -> Result<Vec<u8>, vfs::Errno> {
    let mut b = vec![0u8; 8192];
    let n = s.read(&mut b, NB, &no)?;
    b.truncate(n);
    Ok(b)
}

fn raw(m: &MasterFile) {
    let (p, mem) = (Fake::new(&[], 0), Mem::new());
    let mut t = Termios::pty();
    t.iflag = 0;
    t.oflag = 0;
    t.lflag = 0;
    mem.put(0, &t.to_termios());
    assert_eq!(m.ioctl(TCSETS, BASE, &mem, &p), Ok(0));
}

#[test]
fn the_slave_node_exists_locked_until_unlockpt() {
    let m = PtmxDev::open_master(USER).unwrap();
    let (p, mem) = (Fake::new(&[], 0), Mem::new());
    assert_eq!(m.ioctl(TIOCGPTN, BASE, &mem, &p), Ok(0));
    let idx = mem.i32_at(0) as u32;
    let d = vfs::root_ns().lookup(&alloc::format!("/dev/pts/{idx}")).expect("node");
    let mt = &d.inode.meta;
    assert_eq!((mt.mode(), mt.uid(), mt.gid()), (uapi::S_IFCHR | 0o600, 1000, 5), "CachyOS devpts: gid=5,mode=600");
    assert_eq!(m.open_slave(RDWR | NOCTTY).err(), Some(EIO), "locked (`TTY_PTY_LOCK`)");
    assert_eq!(m.ioctl(TIOCGPTLCK, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0), 1);
    mem.put(0, &0i32.to_le_bytes());
    assert_eq!(m.ioctl(TIOCSPTLCK, BASE, &mem, &p), Ok(0));
    assert!(m.open_slave(RDWR | NOCTTY).is_ok());
    assert_eq!(m.ioctl(TIOCGPTN, 0x10, &mem, &p), Err(EFAULT));
    drop(m);
    assert!(vfs::root_ns().lookup(&alloc::format!("/dev/pts/{idx}")).is_err(), "devpts_pty_kill");
}

#[test]
fn echo_and_output_processing_go_to_the_master() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    assert_eq!(m.write(b"hi\r", NB, &no), Ok(3));
    assert_eq!(read_m(&m), b"hi\r\n", "ECHO with ICRNL: the CR is echoed as CR LF");
    assert_eq!(read_s(&s), Ok(b"hi\n".to_vec()));
    assert_eq!(s.write(b"out\n", NB, &no), Ok(4));
    assert_eq!(read_m(&m), b"out\r\n", "ONLCR");
    assert_eq!(read_s(&s), Err(EAGAIN));
    assert_eq!(m.read(&mut [0u8; 4], NB, &no), Err(EAGAIN));
    // ^D on an empty line: end of file for the slave reader.
    assert_eq!(m.write(&[4], NB, &no), Ok(1));
    assert_eq!(read_s(&s), Ok(Vec::new()));
}

#[test]
fn the_start_settings_and_the_size_are_a_ptys() {
    let (m, idx) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    let (p, mem) = (Fake::new(&[], 0), Mem::new());
    assert_eq!(m.ioctl(TCGETS, BASE, &mem, &p), Ok(0));
    let t = Termios::pty().to_termios();
    assert_eq!(mem.at(0, 36), t.to_vec(), "TCGETS on the master is the slave's termios");
    assert_eq!(u32::from_le_bytes(t[8..12].try_into().unwrap()), 0o277, "B38400|CS8|CREAD, no HUPCL");
    assert_eq!(s.ioctl(TIOCGWINSZ, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.at(0, 8), [0u8; 8], "a fresh pty is 0x0");
    assert_eq!(s.ioctl(TIOCGDEV, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0) as u32, new_encode_dev(136, idx));
    assert_eq!(m.ioctl(TIOCGDEV, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0) as u32, new_encode_dev(136, idx), "the master answers the slave's number");
    // Parity and character size are the pty's own (`pty_set_termios`).
    let mut odd = Termios::pty();
    odd.cflag = flag::B38400 | flag::PARENB | 0o20; // CS7
    mem.put(0, &odd.to_termios());
    assert_eq!(s.ioctl(TCSETS, BASE, &mem, &p), Ok(0));
    assert_eq!(s.pair().tty().termios().cflag, flag::B38400 | flag::CS8 | flag::CREAD);
    // No VT calls and no master calls on the slave.
    for cmd in [KDGETMODE, KDGKBTYPE, TIOCGPTN, TIOCSPTLCK, TIOCSIG, 0x5600] {
        assert_eq!(s.ioctl(cmd, BASE, &mem, &p), Err(ENOTTY), "{cmd:#x}");
    }
    assert_eq!(new_encode_dev(136, 0x12345), 0x45 | (136 << 8) | (0x12300 << 12));
}

#[test]
fn master_writes_wait_for_room_and_nothing_is_lost() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    raw(&m);
    let data: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let first = m.write(&data, NB, &no).unwrap();
    assert_eq!(first, crate::ldisc::BUF_SIZE - 1 + TO_SLAVE_MAX, "the line discipline's room plus the flip buffer");
    assert_eq!(m.write(&data[first..], NB, &no), Err(EAGAIN));
    assert!(!m.poll_events().contains(Events::OUT));
    let mut got = Vec::new();
    let mut sent = first;
    while got.len() < data.len() {
        got.extend(read_s(&s).unwrap());
        if sent < data.len() {
            sent += m.write(&data[sent..], NB, &no).unwrap_or(0);
        }
    }
    assert_eq!(got, data, "in order, none dropped");
}

#[test]
fn slave_writes_wait_for_the_master_to_read() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    raw(&m);
    let data = vec![b'x'; 3 * TO_MASTER_MAX];
    assert_eq!(s.write(&data, NB, &no), Ok(TO_MASTER_MAX));
    assert_eq!(s.write(b"y", NB, &no), Err(EAGAIN));
    assert!(!s.poll_events().contains(Events::OUT));
    assert!(m.poll_events().contains(Events::IN));
    assert_eq!(read_m(&m).len(), TO_MASTER_MAX);
    assert!(s.poll_events().contains(Events::OUT));
}

#[test]
fn closing_the_master_hangs_the_slave_up() {
    let (m, idx) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    let (p, mem) = (Fake::new(&[], 0), Mem::new());
    assert_eq!(m.write(b"typed\r", NB, &no), Ok(6));
    drop(m);
    assert_eq!(read_s(&s), Ok(Vec::new()), "hung_up_tty_read: 0 (the input was flushed)");
    assert_eq!(s.write(b"x", NB, &no), Err(EIO));
    assert_eq!(s.ioctl(TCGETS, BASE, &mem, &p), Err(EIO));
    assert_eq!(s.ioctl(TIOCSPGRP, BASE, &mem, &p), Err(ENOTTY));
    let all = Events::IN | Events::OUT | Events::ERR | Events::HUP;
    assert_eq!(s.poll_events().0 & all, all);
    assert!(vfs::root_ns().lookup(&alloc::format!("/dev/pts/{idx}")).is_err());
    let pair = s.pair().clone();
    assert_eq!(crate::pts::PtsNode::new(&pair).open(RDWR).err(), Some(EIO), "no new slave once the master is gone");
}

#[test]
fn closing_the_last_slave_ends_the_masters_reads_after_the_data() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    let s2 = m.open_slave(RDWR | NOCTTY).unwrap();
    assert_eq!(s.write(b"bye", NB, &no), Ok(3));
    drop(s);
    assert!(!m.poll_events().contains(Events::HUP), "one slave is still open");
    drop(s2);
    assert!(m.poll_events().contains(Events::HUP));
    let mut b = [0u8; 16];
    assert_eq!(m.read(&mut b, NB, &no), Ok(3), "the data first");
    assert_eq!(m.read(&mut b, NB, &no), Err(EIO));
    assert_eq!(m.read(&mut b, false, &no), Err(EIO), "a blocking read too");
    assert_eq!(m.write(b"x", NB, &no), Ok(1), "the master still writes (the slave tty lives on)");
    let s3 = m.open_slave(RDWR | NOCTTY).unwrap();
    assert!(!m.poll_events().contains(Events::HUP), "`pty_open` clears TTY_OTHER_CLOSED");
    drop(s3);
}

#[test]
fn a_blocked_master_read_ends_on_abort() {
    let (m, _) = open();
    let _s = m.open_slave(RDWR | NOCTTY).unwrap();
    assert_eq!(m.read(&mut [0u8; 4], false, &|| true), Err(EINTR));
}

#[test]
fn the_window_size_from_the_master_signals_the_slaves_foreground_group() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    let p = Fake::new(&[(100, 100, 100), (101, 101, 100), (300, 300, 300)], 100);
    let mem = Mem::new();
    assert_eq!(s.ioctl(TIOCSCTTY, 0, &mem, &p), Ok(0));
    let w = Winsize { rows: 24, cols: 80, xpixel: 0, ypixel: 0 };
    mem.put(0, &w.to_bytes());
    p.be(300);
    assert_eq!(m.ioctl(TIOCSWINSZ, BASE, &mem, &p), Ok(0));
    assert_eq!(p.sent(), vec![(100, 28)], "SIGWINCH to the slave's foreground group");
    assert_eq!(m.ioctl(TIOCSWINSZ, BASE, &mem, &p), Ok(0));
    assert_eq!(p.sent(), vec![], "unchanged: no signal");
    mem.put(0, &[0xEE; 8]);
    assert_eq!(s.ioctl(TIOCGWINSZ, BASE, &mem, &p), Ok(0));
    assert_eq!(&mem.at(0, 4), &[24, 0, 80, 0]);
    // The master's job-control answers have no controlling-terminal check; the slave's do.
    assert_eq!(m.ioctl(TIOCGPGRP, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0), 100);
    assert_eq!(m.ioctl(TIOCGSID, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0), 100);
    assert_eq!(s.ioctl(TIOCGPGRP, BASE, &mem, &p), Err(ENOTTY), "process 300 is in another session");
    assert_eq!(m.ioctl(TIOCNOTTY, 0, &mem, &p), Err(ENOTTY));
    assert_eq!(m.ioctl(TIOCSIG, 2, &mem, &p), Ok(0));
    assert_eq!(p.sent(), vec![(100, 2)], "TIOCSIG: the slave's foreground group");
    assert_eq!(m.ioctl(TIOCSIG, 0, &mem, &p), Err(EINVAL));
    assert_eq!(m.ioctl(TIOCSIG, 65, &mem, &p), Err(EINVAL));
    let _ = pty::count();
}

#[test]
fn packet_mode_is_not_built_and_the_rest_answers_as_linux() {
    let (m, _) = open();
    let p = Fake::new(&[(100, 100, 100)], 100);
    let mem = Mem::new();
    mem.put(0, &1i32.to_le_bytes());
    assert_eq!(m.ioctl(TIOCPKT, BASE, &mem, &p), Err(ENOTTY));
    mem.put(0, &0i32.to_le_bytes());
    assert_eq!(m.ioctl(TIOCPKT, BASE, &mem, &p), Ok(0));
    assert_eq!(m.ioctl(TIOCGPKT, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0), 0);
    assert_eq!(m.ioctl(TIOCSTI, BASE, &mem, &p), Err(EIO), "legacy_tiocsti=0 without privilege");
    assert_eq!(m.ioctl(TCFLSH, 7, &mem, &p), Err(EINVAL));
    assert_eq!(m.ioctl(TCXONC, 9, &mem, &p), Err(EINVAL));
    assert_eq!(m.ioctl(KDGETMODE, BASE, &mem, &p), Err(ENOTTY));
    assert_eq!(m.ioctl(TIOCGPTPEER, RDWR, &mem, &p), Err(ENOTTY), "until the syscall layer's glue (CR2)");
    assert_eq!(m.ioctl(TIOCVHANGUP, 0, &mem, &p), Err(EPERM));
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    assert_eq!(s.write(b"abc", NB, &no), Ok(3));
    assert_eq!(m.ioctl(TIOCINQ, BASE, &mem, &p), Ok(0));
    assert_eq!(mem.i32_at(0), 3);
    assert_eq!(m.ioctl(TCFLSH, TCIFLUSH, &mem, &p), Ok(0));
    assert_eq!(m.read(&mut [0u8; 4], NB, &no), Err(EAGAIN));
    assert_eq!(pty::peer_path(&*m as &dyn File), Some(s.pair().path()));
    assert_eq!(pty::peer_path(&*s as &dyn File), None);
}

#[test]
fn indexes_are_reused_lowest_first_once_both_ends_are_gone() {
    let (a, ia) = open();
    let (b, ib) = open();
    assert_ne!(ia, ib);
    let s = a.open_slave(RDWR | NOCTTY).unwrap();
    drop(a);
    let (c, ic) = open();
    assert_ne!(ic, ia, "the slave still holds index {ia}");
    drop(s);
    drop((b, c));
}

/// Output with OPOST: ONLCR doubles a newline, XTABS makes a tab up to 8 spaces. The slave's pieces are sized so
/// the master's queue never grows past `TO_MASTER_MAX` by more than one character's expansion (fixer finding 3).
#[test]
fn slave_output_that_grows_stays_within_the_masters_queue() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    let nl = vec![b'\n'; 3 * TO_MASTER_MAX];
    let n = s.write(&nl, NB, &no).unwrap();
    let got = read_m(&m);
    assert!(got.len() <= TO_MASTER_MAX + 1 && got.len() == 2 * n, "{} bytes for {n} newlines", got.len());
    assert!(got.len() >= TO_MASTER_MAX - 1, "the queue was filled: {}", got.len());
    let (p, mem) = (Fake::new(&[], 0), Mem::new());
    let mut t = Termios::pty();
    t.oflag |= flag::XTABS;
    mem.put(0, &t.to_termios());
    assert_eq!(s.ioctl(TCSETS, BASE, &mem, &p), Ok(0));
    let tabs = vec![b'\t'; TO_MASTER_MAX];
    let n = s.write(&tabs, NB, &no).unwrap();
    let got = read_m(&m);
    assert!(got.len() <= TO_MASTER_MAX + 7 && got.len() == 8 * n, "{} bytes for {n} tabs", got.len());
}

/// Several threads writing into one master never put more than `TO_SLAVE_MAX` into the flip buffer: the room
/// check and the append are one step (fixer finding 2).
#[test]
fn master_writers_on_several_threads_stay_within_the_flip_buffer() {
    extern crate std;
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    raw(&m);
    let total: usize = (0..8)
        .map(|_| {
            let m = m.clone();
            std::thread::spawn(move || {
                let mut n = 0;
                for _ in 0..64 {
                    n += m.write(&[b'a'; 700], NB, &no).unwrap_or(0);
                }
                n
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .sum();
    assert_eq!(total, crate::ldisc::BUF_SIZE - 1 + TO_SLAVE_MAX, "the line discipline's room plus the flip buffer");
    drop(s);
}

/// A cooked slave whose reader drains it while nobody reads the master: the master's writes all go through (no
/// wait for its own unread echo) and the echo stops at `TO_MASTER_MAX + ECHO_ROOM`, as on CachyOS (fixer finding
/// 1: Linux discards echo there too, linux-6.6/drivers/tty/n_tty.c:736).
#[test]
fn unread_echo_never_blocks_the_masters_writes() {
    let (m, _) = open();
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    let mut line = vec![b'y'; 63];
    line.push(b'\r');
    let mut slave_got = 0;
    for _ in 0..1000 {
        assert_eq!(m.write(&line, NB, &no), Ok(64));
        while let Ok(b) = read_s(&s) {
            slave_got += b.len();
        }
    }
    assert_eq!(slave_got, 64_000);
    let echo = read_m(&m);
    assert!(echo.len() <= TO_MASTER_MAX + pty::ECHO_ROOM && echo.len() > TO_MASTER_MAX, "echo {}", echo.len());
}

/// The master's write before any slave was opened and after the last slave closed is taken (Linux's slave count
/// stays 1 while the master is open, so `n_tty_write`'s `!tty->link->count` EIO, n_tty.c:2378, never applies to a
/// master; CachyOS: 3 bytes written both times). Rejected fixer finding 4.
#[test]
fn the_master_writes_without_an_open_slave() {
    let (m, _) = open();
    assert_eq!(m.write(b"abc", NB, &no), Ok(3));
    let s = m.open_slave(RDWR | NOCTTY).unwrap();
    drop(s);
    assert_eq!(m.write(b"abc", NB, &no), Ok(3));
}
