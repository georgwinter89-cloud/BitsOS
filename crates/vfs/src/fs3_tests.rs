//! FS-3 host tests with threads: blocking reads and writes and their wake-ups, EOF and EPIPE when the other side
//! goes, SIGPIPE through the hook, PIPE_BUF atomicity under several writers, FIFO opens that wait for the other
//! side, the kernel's own FIFO opens (never waiting), readiness and its wake counter (FS-4's edges), eventfd's
//! semaphore mode across threads. The single-threaded semantics are checked against Linux in cases.txt.

extern crate std;

use crate::eventfd::EventFd;
use crate::mount_ns::MountNs;
use crate::node::{BufSink, BufSource, File};
use crate::pipe::{self, Fifo, PipeEnd};
use crate::poll::{Events, Pollable};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uapi::errno::*;

static SIGPIPES: AtomicU64 = AtomicU64::new(0);
fn count_sigpipe() {
    SIGPIPES.fetch_add(1, Ordering::SeqCst);
}

fn read_all(r: &PipeEnd, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut b = vec![0u8; 7000];
    while out.len() < n {
        let k = r.read_stream(&mut b, false).unwrap();
        if k == 0 {
            break;
        }
        out.extend_from_slice(&b[..k]);
    }
    out
}

#[test]
fn blocking_read_wakes_on_write_and_eof() {
    let (r, w) = pipe::kernel_pipe();
    let t = thread::spawn(move || {
        let mut b = [0u8; 16];
        let n = r.read_stream(&mut b, false).unwrap();
        let first = b[..n].to_vec();
        // Blocks again until the writer is gone: EOF.
        let eof = r.read_stream(&mut b, false).unwrap();
        (first, eof)
    });
    thread::sleep(Duration::from_millis(30));
    assert_eq!(w.write_stream(b"ping", false).unwrap(), 4);
    thread::sleep(Duration::from_millis(30));
    drop(w);
    let (first, eof) = t.join().unwrap();
    assert_eq!(first, b"ping");
    assert_eq!(eof, 0);
}

#[test]
fn blocking_write_moves_more_than_the_capacity() {
    let (r, w) = pipe::kernel_pipe();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 251) as u8).collect();
    let d2 = data.clone();
    let t = thread::spawn(move || {
        // One write of 300 KB through a 64 KiB pipe: it blocks until the reader made room, several times.
        let n = w.write_stream(&d2, false).unwrap();
        assert_eq!(n, d2.len());
    });
    let got = read_all(&r, data.len());
    t.join().unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data, "bytes came out in another order");
}

#[test]
fn reader_gone_wakes_a_blocked_writer_with_the_partial_count_and_sigpipe() {
    pipe::set_sigpipe(count_sigpipe);
    let (r, w) = pipe::kernel_pipe();
    let before = SIGPIPES.load(Ordering::SeqCst);
    let t = thread::spawn(move || w.write_stream(&vec![1u8; 100_000], false));
    // The writer fills the pipe and waits for room; the reader goes away.
    let start = Instant::now();
    while r.pipe().bytes() < 65536 {
        assert!(start.elapsed() < Duration::from_secs(10), "the writer did not fill the pipe");
        thread::yield_now();
    }
    drop(r);
    let n = t.join().unwrap();
    assert_eq!(n, Ok(65536), "Linux returns what it wrote when the reader goes during the write");
    assert!(SIGPIPES.load(Ordering::SeqCst) > before, "SIGPIPE was not raised");
    // A write with no reader at all: EPIPE (and SIGPIPE again).
    let (r2, w2) = pipe::kernel_pipe();
    drop(r2);
    let before = SIGPIPES.load(Ordering::SeqCst);
    assert_eq!(w2.write_stream(b"x", false), Err(EPIPE));
    assert_eq!(SIGPIPES.load(Ordering::SeqCst), before + 1);
}

#[test]
fn small_writes_of_many_writers_stay_whole() {
    let (r, w) = pipe::kernel_pipe();
    const WRITERS: u8 = 4;
    const RECORDS: usize = 500;
    const SIZE: usize = 700;
    let mut ts = Vec::new();
    for id in 0..WRITERS {
        let w = w.clone();
        ts.push(thread::spawn(move || {
            let rec = vec![b'a' + id; SIZE];
            for _ in 0..RECORDS {
                assert_eq!(w.write_stream(&rec, false).unwrap(), SIZE);
            }
        }));
    }
    drop(w);
    let total = WRITERS as usize * RECORDS * SIZE;
    let reader = thread::spawn(move || read_all(&r, usize::MAX));
    for t in ts {
        t.join().unwrap();
    }
    let got = reader.join().unwrap();
    assert_eq!(got.len(), total);
    // Every 700-byte record is one writer's (writes up to PIPE_BUF are never interleaved).
    for (i, rec) in got.chunks(SIZE).enumerate() {
        assert!(rec.iter().all(|&b| b == rec[0]), "record {i} is mixed");
    }
}

#[test]
fn readiness_and_wake_counter() {
    let (r, w) = pipe::kernel_pipe();
    assert_eq!(File::events(&*r).0, 0);
    assert_eq!(File::events(&*w).0, Events::OUT);
    let rq = r.poll_head().unwrap();
    let wq = w.poll_head().unwrap();
    assert!(!core::ptr::eq(rq, wq), "a pipe has a read queue and a write queue (rd_wait, wr_wait)");
    let (seq0, wseq0) = (rq.seq(), wq.seq());
    w.write_stream(b"a", false).unwrap();
    let seq1 = rq.seq();
    assert!(seq1 > seq0);
    // Linux wakes readers on every write, also into a pipe that has data already: an edge each time.
    w.write_stream(b"b", false).unwrap();
    assert!(rq.seq() > seq1);
    // Writes wake rd_wait only (the writers' queue moves when a read empties a full pipe).
    assert_eq!(wq.seq(), wseq0);
    assert_eq!(Pollable::events(&*r).0, Events::IN);
    assert!(core::ptr::eq(Pollable::wait_queue(&*r), &**rq));
    // A waiter for POLLIN on an empty pipe sleeps until the write.
    let mut b = [0u8; 8];
    r.read_stream(&mut b, false).unwrap();
    let r2 = r.clone();
    let t = thread::spawn(move || crate::poll::wait_for(&*r2, Events::IN, Some(5_000_000_000)));
    thread::sleep(Duration::from_millis(20));
    w.write_stream(b"c", false).unwrap();
    assert!(t.join().unwrap().contains(Events::IN));
    drop(w);
    assert_eq!(File::events(&*r).0, Events::IN | Events::HUP);
}

#[test]
fn fifo_open_waits_for_the_other_side() {
    let f = Arc::new(Fifo::new());
    let f2 = f.clone();
    let reader = thread::spawn(move || {
        let start = Instant::now();
        let end = f2.open(crate::abi::O_RDONLY, false).unwrap();
        (start.elapsed(), end)
    });
    thread::sleep(Duration::from_millis(50));
    let w = f.open(crate::abi::O_WRONLY, false).unwrap();
    let (waited, r) = reader.join().unwrap();
    assert!(waited >= Duration::from_millis(40), "O_RDONLY returned before a writer came ({waited:?})");
    assert_eq!(w.write_stream(b"via fifo", false).unwrap(), 8);
    let mut b = [0u8; 16];
    assert_eq!(r.read_stream(&mut b, false).unwrap(), 8);
    // A writer waits for a reader the same way.
    drop((r, w));
    assert!(f.current().is_none(), "the pipe outlived its last end");
    let f3 = f.clone();
    let writer = thread::spawn(move || f3.open(crate::abi::O_WRONLY, false).map(|_| ()));
    thread::sleep(Duration::from_millis(30));
    let r = f.open(crate::abi::O_RDONLY | crate::abi::O_NONBLOCK, false).unwrap();
    writer.join().unwrap().unwrap();
    drop(r);
    assert_eq!(f.open(crate::abi::O_WRONLY | crate::abi::O_NONBLOCK, false).err(), Some(ENXIO));
    assert_eq!(f.open(3, false).err(), Some(EINVAL));
}

#[test]
fn kernel_fifo_opens_never_wait() {
    let ns = MountNs::new_empty();
    ns.create_fifo(b"/k/fifo", 0o600).unwrap();
    // O_RDONLY without a writer returns at once for the kernel (B4); O_WRONLY without a reader is ENXIO.
    let r = ns.open_flags(b"/k/fifo", crate::abi::O_RDONLY, 0).unwrap();
    assert!(r.file.mode() & uapi::S_IFMT == uapi::S_IFIFO);
    drop(r);
    assert_eq!(ns.open_flags(b"/k/fifo", crate::abi::O_WRONLY, 0).err(), Some(ENXIO));
    let rw = ns.open_flags(b"/k/fifo", crate::abi::O_RDWR, 0).unwrap();
    assert_eq!(rw.write(b"k").unwrap(), 1);
    // The node itself is no open file: looking at it reads nothing and opens nothing.
    let node = ns.open("/k/fifo").unwrap();
    assert_eq!(node.read_at(0, &mut [0u8; 4]), Err(EINVAL));
    assert_eq!(node.size(), 0);
}

#[test]
fn eventfd_semaphore_across_threads() {
    let e = EventFd::new(0, crate::eventfd::EFD_SEMAPHORE);
    let e2 = e.clone();
    let t = thread::spawn(move || {
        let mut got = 0u64;
        for _ in 0..3 {
            let mut b = [0u8; 8];
            assert_eq!(e2.read_stream(&mut b, false).unwrap(), 8);
            got += u64::from_le_bytes(b);
        }
        got
    });
    thread::sleep(Duration::from_millis(20));
    e.write_stream(&3u64.to_le_bytes(), false).unwrap();
    assert_eq!(t.join().unwrap(), 3, "each read takes 1");
    assert_eq!(e.count(), 0);
    // A writer that would pass u64::MAX - 1 waits until a reader takes the count.
    let e = EventFd::new(0, 0);
    e.write_stream(&(u64::MAX - 2).to_le_bytes(), false).unwrap();
    assert_eq!(e.write_stream(&5u64.to_le_bytes(), true), Err(EAGAIN));
    let e3 = e.clone();
    let w = thread::spawn(move || e3.write_stream(&5u64.to_le_bytes(), false));
    thread::sleep(Duration::from_millis(20));
    let mut b = [0u8; 8];
    e.read_stream(&mut b, false).unwrap();
    assert_eq!(u64::from_le_bytes(b), u64::MAX - 2);
    assert_eq!(w.join().unwrap(), Ok(8));
    assert_eq!(e.count(), 5);
    // The kernel's signal saturates and shows POLLERR at u64::MAX.
    e.signal(u64::MAX);
    assert_eq!(e.count(), u64::MAX);
    assert!(File::events(&*e).contains(Events::ERR | Events::IN));
    assert!(!File::events(&*e).contains(Events::OUT));
}

#[test]
fn iov_sources_and_packets() {
    let (r, w) = pipe::kernel_pipe();
    // A write made of three segments is one write (atomic, one packet with O_DIRECT).
    let n = w.write_iter(&mut BufSource::segments(vec![&b"ab"[..], &b""[..], &b"cd"[..]]), crate::abi::O_DIRECT).unwrap().unwrap();
    assert_eq!(n, 4);
    w.write_iter(&mut BufSource::new(b"ef"), crate::abi::O_DIRECT).unwrap().unwrap();
    let mut b = [0u8; 3];
    assert_eq!(r.read_iter(&mut BufSink::new(&mut b), 0).unwrap().unwrap(), 3);
    assert_eq!(&b, b"abc");
    // The rest of the first packet ("d") was dropped with it.
    let mut b = [0u8; 8];
    let k = r.read_iter(&mut BufSink::new(&mut b), 0).unwrap().unwrap();
    assert_eq!(&b[..k], b"ef");
}

#[test]
fn a_fifo_pipe_without_ends_is_dead_even_while_still_referenced() {
    // The last end has gone but its pipe is still referenced (the window while the last end is being dropped,
    // or a kernel user holding it): a new open must get a fresh pipe, never the old session's bytes.
    let f = Arc::new(Fifo::new());
    let rw = f.open(crate::abi::O_RDWR, false).unwrap();
    assert_eq!(rw.write_stream(b"old session", true).unwrap(), 11);
    let held = f.current().expect("an end is open");
    drop(rw);
    assert!(f.current().is_none(), "a pipe without ends counts as gone");
    let rw = f.open(crate::abi::O_RDWR | crate::abi::O_NONBLOCK, false).unwrap();
    let mut b = [0u8; 16];
    assert_eq!(rw.read_stream(&mut b, true), Err(EAGAIN), "the new session saw the old bytes");
    assert!(!Arc::ptr_eq(&held, &f.current().unwrap()));
    // ENXIO is decided before anything is counted: the pipe of a refused open is not kept alive.
    drop(rw);
    assert_eq!(f.open(crate::abi::O_WRONLY | crate::abi::O_NONBLOCK, false).err(), Some(ENXIO));
    assert!(f.current().is_none());
}

#[test]
fn new_pipes_follow_pipe_max_size() {
    // alloc_pipe_info: 16 pages, fewer for an unprivileged creator when pipe-max-size is below 64 KiB.
    assert_eq!(pipe::initial_slots(1 << 20, false), 16);
    assert_eq!(pipe::initial_slots(65536, false), 16);
    assert_eq!(pipe::initial_slots(16384, false), 4);
    assert_eq!(pipe::initial_slots(4096, false), 1);
    assert_eq!(pipe::initial_slots(4096, true), 16);
}

