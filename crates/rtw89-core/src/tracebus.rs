//! The host trace bus (feature `std`): every access goes as V1's 16-byte op
//! record to the JS fake chip (V1 `wlan-fake.mjs` `fakeChip`, which drives
//! `js/bringup-fake.mjs`'s FakeMiniPc) over any byte stream: a Unix socket, a
//! TCP socket or a child's pipes. The protocol is V1 `fake.asm`'s: reads,
//! config reads and memory reads get 4 bytes back, a poll 4 bytes (0 passed),
//! bulk writes and notes carry `c` bytes after the record, a bulk read gets
//! `c` bytes back. DMA addresses come from 0x610000 up in the JS driver's
//! order, so the trace equals the golden file without any normalisation.

use std::io::{Read, Write};
use std::string::String;
use std::time::Instant;
use std::vec::Vec;

use crate::bus::{Bus, BusError, BusResult, MemWidth, Width};
use crate::membus::{DMA_BASE, DMA_END, bump_alloc};
use crate::trace::{Op, OpKind, TraceEntry};

/// What the fake chip is told about the driver's state (fake.asm FAKE_*)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeState {
    /// the channel set (d.chan.ch)
    Chan = 1,
    /// 1 while scanning (d.sta.scanning)
    Scan = 2,
    /// our MAC address: its first 4 bytes
    StaLo = 3,
    /// its last 2
    StaHi = 4,
}

pub struct TraceBus<S: Read + Write> {
    stream: S,
    out: Vec<u8>,
    dma_next: u32,
    /// regions handed out and not given back: when the last goes (a stop freed the rings), the
    /// next bring-up gets the same addresses again, as V1's fixed WLAN_DMA area per run
    dma_live: usize,
    trace: Vec<TraceEntry>,
    start: Instant,
    /// set by the first stream failure: the stream is then out of step with the
    /// fake, so every later call fails with it and nothing is sent again
    failed: Option<BusError>,
}

impl<S: Read + Write> TraceBus<S> {
    pub fn new(stream: S) -> TraceBus<S> {
        TraceBus {
            stream,
            out: Vec::new(),
            dma_next: DMA_BASE,
            dma_live: 0,
            trace: Vec::new(),
            start: Instant::now(),
            failed: None,
        }
    }

    /// Every op and note sent so far, in order
    pub fn trace(&self) -> &[TraceEntry] {
        &self.trace
    }

    /// The trace as golden-file lines
    pub fn lines(&self) -> Vec<String> {
        crate::trace::lines(&self.trace)
    }

    pub fn into_inner(mut self) -> BusResult<S> {
        self.send_out()?;
        Ok(self.stream)
    }

    /// Waits until the fake chip handled everything sent (OP_SYNC)
    pub fn sync(&mut self) -> BusResult<()> {
        self.head(OpKind::Sync, 0, 0)?;
        self.dword().map(|_| ())
    }

    /// The fake drops the ops it still queues (OP_DROP: the JS stop after a failure)
    pub fn drop_queued(&mut self) -> BusResult<()> {
        self.head(OpKind::Drop, 0, 0)
    }

    /// What the fake chip holds at a register, outside the trace (OP_PEEK)
    pub fn peek(&mut self, addr: u32) -> BusResult<u32> {
        self.head(OpKind::Peek, addr, 0)?;
        self.dword()
    }

    /// Driver state the fake chip looks at (OP_STATE)
    pub fn state(&mut self, what: FakeState, value: u32) -> BusResult<()> {
        self.head(OpKind::State, what as u32, value)
    }

    /// The stream, for a look (tests)
    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    /// The error that broke the stream, if one did
    pub fn failed(&self) -> Option<&BusError> {
        self.failed.as_ref()
    }

    fn fail(&mut self, e: BusError) -> BusError {
        self.out.clear();
        self.failed.get_or_insert(e).clone()
    }

    fn check(&self) -> BusResult<()> {
        match &self.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn send_out(&mut self) -> BusResult<()> {
        self.check()?;
        if self.out.is_empty() {
            return Ok(());
        }
        // after a partial write the fake has an unknown part: never resend
        if self.stream.write_all(&self.out).is_err() || self.stream.flush().is_err() {
            return Err(self.fail(BusError::Transport));
        }
        self.out.clear();
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> BusResult<()> {
        self.send_out()?;
        if self.stream.read_exact(buf).is_err() {
            return Err(self.fail(BusError::Gone));
        }
        Ok(())
    }

    fn dword(&mut self) -> BusResult<u32> {
        let mut b = [0u8; 4];
        self.recv(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// A traced op: recorded here and sent
    fn op(&mut self, kind: OpKind, size: u8, ms: u16, a: u32, b: u32, c: u32) -> BusResult<()> {
        self.check()?;
        let op = Op::new(kind, size, ms, a, b, c);
        self.trace.push(TraceEntry::Op(op));
        self.out.extend_from_slice(&op.record());
        Ok(())
    }

    /// One of the fake's own records (fake.asm `fake_head`: size 0, ms 0, b 0)
    fn head(&mut self, kind: OpKind, a: u32, c: u32) -> BusResult<()> {
        self.check()?;
        self.out
            .extend_from_slice(&Op::new(kind, 0, 0, a, 0, c).record());
        Ok(())
    }

    fn len32(n: usize) -> BusResult<u32> {
        u32::try_from(n).map_err(|_| BusError::Refused {
            kind: "length",
            addr: 0,
            len: u32::MAX,
        })
    }
}

impl<S: Read + Write> Bus for TraceBus<S> {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.op(OpKind::Rd, w.bytes(), 0, addr, 0, 0)?;
        self.dword()
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.op(OpKind::Wr, w.bytes(), 0, addr, 0, val & w.mask())?;
        Ok(())
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.op(OpKind::Rmw, w.bytes(), 0, addr, clear, set)?;
        Ok(())
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.op(OpKind::Poll, w.bytes(), ms, addr, mask, want)?;
        if self.dword()? == 0 {
            return Ok(());
        }
        let last = self.peek(addr)? & w.mask();
        Err(BusError::PollTimeout {
            addr,
            width: w,
            mask,
            want,
            last,
        })
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.op(OpKind::Delay, 0, 0, us, 0, 0)?;
        Ok(())
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.op(OpKind::Cfgr, 4, 0, off.into(), 0, 0)?;
        self.dword()
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.op(OpKind::Cfgw, 4, 0, off.into(), 0, val)?;
        Ok(())
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        let a = bump_alloc(&mut self.dma_next, DMA_END, size, align)
            .ok_or(BusError::DmaExhausted { size, align })?;
        self.dma_live += 1;
        Ok(a)
    }
    fn dma_free(&mut self, _addr: u32, _size: usize) -> BusResult<()> {
        // the JS kit never frees within a run; once every region is back (a whole stop), the
        // next run starts at DMA_BASE again (V1 K6's off-and-on on the fake did the same)
        self.dma_live = self.dma_live.saturating_sub(1);
        if self.dma_live == 0 {
            self.dma_next = DMA_BASE;
        }
        Ok(())
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        let (b, c) = match w {
            MemWidth::M8 => (0, val as u32 & 0xff),
            MemWidth::M32 => (0, val as u32),
            MemWidth::M64 => ((val >> 32) as u32, val as u32),
        };
        self.op(OpKind::Mwr, w as u8, 0, addr, b, c)?;
        Ok(())
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.op(OpKind::Mrd, 4, 0, addr, 0, 0)?;
        self.dword()
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        let n = Self::len32(data.len())?;
        self.head(OpKind::BulkW, addr, n)?;
        self.out.extend_from_slice(data);
        Ok(())
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        let n = Self::len32(out.len())?;
        self.head(OpKind::BulkR, addr, n)?;
        self.recv(out)
    }
    fn now_ms(&mut self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        let n = Self::len32(text.len())?;
        self.head(OpKind::Note, 0, n)?;
        self.trace.push(TraceEntry::Note(text.into()));
        self.out.extend_from_slice(text.as_bytes());
        Ok(())
    }
    fn flush(&mut self) -> BusResult<()> {
        self.head(OpKind::Flush, 0, 0)
    }
}
