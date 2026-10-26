//! The trace bus over a broken or hostile stream (no node): every failure is
//! an error, never a panic, and after one nothing is sent again.

use std::io::{self, Read, Write};
use std::vec;
use std::vec::Vec;

use crate::bus::{Bus, BusError, MemWidth, Width};
use crate::tracebus::TraceBus;

/// A stream with canned replies; the writer takes `accept` bytes, then fails
struct Pipe {
    reply: Vec<u8>,
    at: usize,
    sent: Vec<u8>,
    accept: usize,
}

impl Pipe {
    fn new(reply: &[u8], accept: usize) -> Pipe {
        Pipe {
            reply: reply.to_vec(),
            at: 0,
            sent: Vec::new(),
            accept,
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.reply.len() - self.at);
        buf[..n].copy_from_slice(&self.reply[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let room = self.accept - self.sent.len();
        if room == 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        let n = buf.len().min(room);
        self.sent.extend_from_slice(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bus(reply: &[u8], accept: usize) -> TraceBus<Pipe> {
    TraceBus::new(Pipe::new(reply, accept))
}

#[test]
fn a_read_gets_its_dword() {
    let mut b = bus(&0x1234_5678u32.to_le_bytes(), usize::MAX);
    assert_eq!(b.r32(0x3F0), Ok(0x1234_5678));
    assert_eq!(b.lines(), vec!["rd32 0x000003F0 0x00000000 0x00000000"]);
}

#[test]
fn no_reply_is_gone_and_stays_gone() {
    let mut b = bus(&[], usize::MAX);
    assert_eq!(b.r32(0x3F0), Err(BusError::Gone));
    assert_eq!(
        b.w32(0x10, 1),
        Err(BusError::Gone),
        "writes fail too after that"
    );
    assert_eq!(b.note("x"), Err(BusError::Gone));
    assert_eq!(b.failed(), Some(&BusError::Gone));
    assert_eq!(b.lines().len(), 1, "nothing recorded after the failure");
}

#[test]
fn a_short_reply_is_gone() {
    let mut b = bus(&[1, 2], usize::MAX);
    assert_eq!(b.cfg_read(4), Err(BusError::Gone));
    assert_eq!(b.mem_read32(0x61_0000), Err(BusError::Gone));
    let mut out = [0u8; 8];
    let mut b = bus(&[1, 2, 3], usize::MAX);
    assert_eq!(b.mem_read_bytes(0x61_0000, &mut out), Err(BusError::Gone));
}

#[test]
fn a_failing_writer_is_transport_and_nothing_is_resent() {
    let mut b = bus(&[0; 64], 0);
    b.w32(0x10, 1).unwrap(); // only queued
    assert_eq!(b.r32(0x10), Err(BusError::Transport));
    assert_eq!(b.sync(), Err(BusError::Transport));
    assert!(b.into_inner().is_err());

    // a partial write: 20 of the 32 queued bytes went out, then the pipe broke
    let mut b = bus(&[0; 64], 20);
    b.w32(0x10, 1).unwrap();
    assert_eq!(b.r32(0x10), Err(BusError::Transport));
    assert_eq!(b.r32(0x10), Err(BusError::Transport));
    assert_eq!(b.flush(), Err(BusError::Transport));
    let Err(e) = b.into_inner() else {
        panic!("into_inner after a failure")
    };
    assert_eq!(e, BusError::Transport);
}

#[test]
fn partial_write_is_never_sent_twice() {
    let mut b = bus(&[0; 64], 20);
    b.w32(0x10, 1).unwrap();
    let _ = b.r32(0x10);
    let _ = b.peek(0x10);
    let _ = b.sync();
    assert_eq!(b.failed(), Some(&BusError::Transport));
    // the pipe has the first record once and 4 bytes of the second, nothing more
    let sent = &b.get_ref().sent;
    assert_eq!(sent.len(), 20);
    assert_eq!(sent[0], 1, "op wr");
    assert_eq!(sent[16], 2, "op rd, only its start");
}

#[test]
fn hostile_replies_never_panic() {
    // a poll's "failed" word that is no 0/1, then the peek
    let mut reply = Vec::new();
    reply.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    reply.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let mut b = bus(&reply, usize::MAX);
    assert_eq!(
        b.poll(Width::W8, 0x3F0, 0x3, 0, 50),
        Err(BusError::PollTimeout {
            addr: 0x3F0,
            width: Width::W8,
            mask: 3,
            want: 0,
            last: 0xff
        }),
        "the peeked value is cut to the width"
    );
    // a poll that fails and the peek gets no answer
    let mut b = bus(&1u32.to_le_bytes(), usize::MAX);
    assert_eq!(b.poll32(0x30, 1, 1, 20), Err(BusError::Gone));
}

#[test]
fn oversize_requests_are_errors() {
    let mut b = bus(&[], usize::MAX);
    assert_eq!(
        b.dma_alloc(usize::MAX, 8),
        Err(BusError::DmaExhausted {
            size: usize::MAX,
            align: 8
        })
    );
    assert!(matches!(
        b.dma_alloc(1 << 33, 8),
        Err(BusError::DmaExhausted { .. })
    ));
    assert!(matches!(
        b.dma_alloc(8, 3),
        Err(BusError::DmaExhausted { .. })
    ));
    assert_eq!(
        b.dma_alloc(8, 8),
        Ok(0x61_0000),
        "still usable after refusals"
    );
}

#[test]
fn writes_are_cut_to_their_width() {
    let mut b = bus(&[], usize::MAX);
    b.write(Width::W8, 0x20, 0x1ff).unwrap();
    b.mem_write(MemWidth::M8, 0x61_0000, 0x1ff).unwrap();
    assert_eq!(
        b.lines(),
        vec![
            "wr8 0x00000020 0x00000000 0x000000FF",
            "mwr8 0x00610000 0x00000000 0x000000FF"
        ]
    );
}
