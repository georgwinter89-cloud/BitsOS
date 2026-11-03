//! WY-3: the window manager's X connection (the socket given to Xwayland as `-wm`): never blocks. Requests are
//! queued and written as far as the socket takes them; what comes back is cut into replies (matched to the
//! request that asked by its sequence number), errors and events.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;

use crate::xproto::{self, Setup, u16_at, u32_at};

pub enum Msg<W> {
    Setup(Result<Setup, String>),
    Reply(W, Vec<u8>),
    /// An X error: the request's purpose when it expected a reply, its code and the major opcode.
    Error(Option<W>, u8, u8),
    Event(Vec<u8>),
}

pub struct XConn<W> {
    stream: UnixStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// The sequence number of the last request sent (the server counts the same way, from 1).
    seq: u64,
    pending: VecDeque<(u64, W)>,
    setup_done: bool,
    next_id: u32,
    id_base: u32,
    id_mask: u32,
    pub closed: bool,
}

impl<W> XConn<W> {
    pub fn new(stream: UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        let mut c = XConn {
            stream,
            inbuf: Vec::new(),
            out: Vec::new(),
            seq: 0,
            pending: VecDeque::new(),
            setup_done: false,
            next_id: 0,
            id_base: 0,
            id_mask: 0,
            closed: false,
        };
        c.out.extend_from_slice(&xproto::setup_request());
        c.flush();
        Ok(c)
    }

    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.stream)
    }

    /// A new resource id of this connection (None when the id range is used up).
    pub fn new_id(&mut self) -> Option<u32> {
        let step = self.id_mask & self.id_mask.wrapping_neg();
        let v = self.next_id.checked_mul(step.max(1))?;
        if v & !self.id_mask != 0 {
            return None;
        }
        self.next_id += 1;
        Some(self.id_base | v)
    }

    /// Queue a request; `want` is what to do with its reply (None: a request without reply).
    pub fn send(&mut self, req: Vec<u8>, want: Option<W>) {
        self.seq += 1;
        if let Some(w) = want {
            self.pending.push_back((self.seq, w));
        }
        self.out.extend_from_slice(&req);
    }

    /// Writes what the socket takes now; the rest stays for the next turn (the loop calls this every turn).
    pub fn flush(&mut self) {
        while !self.out.is_empty() && !self.closed {
            match self.stream.write(&self.out) {
                Ok(0) => self.closed = true,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => self.closed = true,
            }
        }
    }

    /// Everything readable now, cut into messages.
    pub fn read(&mut self) -> Vec<Msg<W>> {
        let mut buf = [0u8; 16384];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(n) => self.inbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
        let mut out = Vec::new();
        loop {
            if !self.setup_done {
                if self.inbuf.len() < 8 {
                    break;
                }
                let total = 8 + u16_at(&self.inbuf, 6) as usize * 4;
                if self.inbuf.len() < total {
                    break;
                }
                let b: Vec<u8> = self.inbuf.drain(..total).collect();
                let s = xproto::parse_setup(&b);
                if let Ok(s) = &s {
                    self.id_base = s.id_base;
                    self.id_mask = s.id_mask;
                    self.next_id = 1;
                }
                self.setup_done = true;
                out.push(Msg::Setup(s));
                continue;
            }
            if self.inbuf.len() < 32 {
                break;
            }
            let code = self.inbuf[0] & 0x7f;
            // Replies and generic events (35) carry more units than the 32 bytes.
            let total = if code == 1 || code == 35 { 32 + u32_at(&self.inbuf, 4) as usize * 4 } else { 32 };
            if self.inbuf.len() < total {
                break;
            }
            let b: Vec<u8> = self.inbuf.drain(..total).collect();
            let seq = u16_at(&b, 2);
            match code {
                1 => {
                    if let Some(w) = self.take(seq) {
                        out.push(Msg::Reply(w, b));
                    }
                }
                0 => {
                    let w = self.take(seq);
                    out.push(Msg::Error(w, b[1], b[10]));
                }
                _ => out.push(Msg::Event(b)),
            }
        }
        out
    }

    /// The pending purpose for the request with this (16-bit) sequence number; earlier ones that can no longer be
    /// answered (the server answers in order) are dropped.
    fn take(&mut self, seq16: u16) -> Option<W> {
        while let Some((s, _)) = self.pending.front() {
            if (*s & 0xffff) as u16 == seq16 {
                return self.pending.pop_front().map(|(_, w)| w);
            }
            // Before this one: never answered (cannot happen with a well-behaved server); after it: wait.
            let behind = (seq16.wrapping_sub((*s & 0xffff) as u16)) < 0x8000;
            if !behind {
                return None;
            }
            self.pending.pop_front();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_find_their_request_and_events_pass() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut c: XConn<&str> = XConn::new(a).unwrap();
        let mut setup = vec![0u8; 8];
        setup[0] = 1;
        let mut body = vec![0u8; 80];
        body[4..8].copy_from_slice(&0x0020_0000u32.to_le_bytes());
        body[8..12].copy_from_slice(&0x001f_ffffu32.to_le_bytes());
        body[20] = 1; // one screen
        body[32..36].copy_from_slice(&0x1e1u32.to_le_bytes());
        setup[6..8].copy_from_slice(&((body.len() / 4) as u16).to_le_bytes());
        setup.extend_from_slice(&body);
        b.write_all(&setup).unwrap();
        c.send(vec![0; 4], None);
        c.send(vec![0; 4], Some("second"));
        let mut reply = vec![0u8; 36];
        reply[0] = 1;
        reply[2] = 2;
        reply[4] = 1;
        let mut ev = vec![0u8; 32];
        ev[0] = xproto::MAP_REQUEST;
        b.write_all(&reply).unwrap();
        b.write_all(&ev).unwrap();
        let m = c.read();
        assert!(matches!(&m[0], Msg::Setup(Ok(s)) if s.root == 0x1e1 && s.id_base == 0x0020_0000));
        assert!(matches!(&m[1], Msg::Reply("second", r) if r.len() == 36));
        assert!(matches!(&m[2], Msg::Event(e) if e[0] == xproto::MAP_REQUEST));
        assert_eq!(c.new_id(), Some(0x0020_0001));
    }
}
