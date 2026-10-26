//! An in-memory chip for host tests (kit/rtwdev.mjs `FakeDev` in Rust):
//! registers, config space and DMA memory are plain storage (0 when never
//! written), every op is recorded as a trace entry, reads can be forced and
//! polls can be made to fail. No JS needed; for the golden trace use the trace bus.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use crate::bus::{Bus, BusError, BusResult, MemWidth, Width};
use crate::trace::{Op, OpKind, TraceEntry};

/// Where `dma_alloc` starts: the JS kit's DMA region (kit/wlkit.mjs `DMA`)
pub const DMA_BASE: u32 = 0x61_0000;
/// Where it ends (kit/wlkit.mjs `DMA_END`): the fake's window, not a count limit
pub const DMA_END: u32 = 0x70_0000;

/// kit/rtwdev.mjs `Dev.dma`: the next address at `align` from `next`, or `None`
/// when the request does not fit below `end` (or `align` is not a power of two)
pub fn bump_alloc(next: &mut u32, end: u32, size: usize, align: usize) -> Option<u32> {
    if align == 0 || !align.is_power_of_two() || align > 1 << 31 {
        return None;
    }
    let size = u32::try_from(size).ok()?;
    let mask = align as u32 - 1;
    let a = next.checked_add(mask)? & !mask;
    let after = a.checked_add(size)?;
    if after > end {
        return None;
    }
    *next = after;
    Some(a)
}

#[derive(Debug, Default)]
pub struct MemBus {
    pub regs: BTreeMap<u32, u32>,
    pub cfg: BTreeMap<u16, u32>,
    pub mem: BTreeMap<u32, u8>,
    /// register -> value every read returns, whatever was written
    pub forced: BTreeMap<u32, u32>,
    /// registers whose polls time out
    pub failing_polls: BTreeSet<u32>,
    /// polls that timed out so far
    pub poll_failures: usize,
    pub trace: Vec<TraceEntry>,
    pub dma_next: u32,
    /// DMA regions handed out and not given back (address -> size)
    pub dma_live: BTreeMap<u32, usize>,
    /// the op that would get this trace index fails with `BusError::Gone` instead (not recorded)
    pub fail_at_op: Option<usize>,
    pub clock_ms: u64,
}

impl MemBus {
    pub fn new() -> MemBus {
        MemBus {
            dma_next: DMA_BASE,
            ..MemBus::default()
        }
    }

    /// The recorded trace as golden-file lines
    pub fn lines(&self) -> Vec<String> {
        crate::trace::lines(&self.trace)
    }

    fn op(&mut self, kind: OpKind, size: u8, ms: u16, a: u32, b: u32, c: u32) -> BusResult<()> {
        if self.fail_at_op == Some(self.trace.len()) {
            self.fail_at_op = None;
            return Err(BusError::Gone);
        }
        self.trace
            .push(TraceEntry::Op(Op::new(kind, size, ms, a, b, c)));
        Ok(())
    }

    /// What a read of `a` would return, without recording an op
    pub fn peek(&self, a: u32, w: Width) -> u32 {
        self.get(a, w)
    }

    /// Stores `v` at `a` without recording an op (test hooks that model a chip's own changes)
    pub fn poke(&mut self, a: u32, w: Width, v: u32) {
        self.put(a, w, v);
    }

    fn get(&self, a: u32, w: Width) -> u32 {
        if let Some(v) = self.forced.get(&a) {
            return *v & w.mask();
        }
        let word = self.regs.get(&(a & !3)).copied().unwrap_or(0);
        (word >> ((a & 3) * 8)) & w.mask()
    }

    fn put(&mut self, a: u32, w: Width, v: u32) {
        let base = a & !3;
        let sh = (a & 3) * 8;
        let m = w.mask().wrapping_shl(sh);
        let old = self.regs.get(&base).copied().unwrap_or(0);
        self.regs
            .insert(base, (old & !m) | (v.wrapping_shl(sh) & m));
    }
}

impl Bus for MemBus {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.op(OpKind::Rd, w.bytes(), 0, addr, 0, 0)?;
        Ok(self.get(addr, w))
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        // recorded as the trace bus sends it: cut to the width
        let val = val & w.mask();
        self.op(OpKind::Wr, w.bytes(), 0, addr, 0, val)?;
        self.put(addr, w, val);
        Ok(())
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.op(OpKind::Rmw, w.bytes(), 0, addr, clear, set)?;
        let v = (self.get(addr, w) & !clear) | set;
        self.put(addr, w, v);
        Ok(())
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.op(OpKind::Poll, w.bytes(), ms, addr, mask, want)?;
        if self.failing_polls.contains(&addr) {
            self.poll_failures += 1;
            return Err(BusError::PollTimeout {
                addr,
                width: w,
                mask,
                want,
                last: self.get(addr, w),
            });
        }
        Ok(())
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.op(OpKind::Delay, 0, 0, us, 0, 0)?;
        self.clock_ms += u64::from(us / 1000);
        Ok(())
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.op(OpKind::Cfgr, 4, 0, off.into(), 0, 0)?;
        Ok(self.cfg.get(&off).copied().unwrap_or(0))
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.op(OpKind::Cfgw, 4, 0, off.into(), 0, val)?;
        self.cfg.insert(off, val);
        Ok(())
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        let a = bump_alloc(&mut self.dma_next, DMA_END, size, align)
            .ok_or(BusError::DmaExhausted { size, align })?;
        self.dma_live.insert(a, size);
        Ok(a)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        match self.dma_live.get(&addr) {
            Some(s) if *s == size => {
                self.dma_live.remove(&addr);
                Ok(())
            }
            _ => Err(BusError::Refused {
                kind: "dma_free",
                addr,
                len: u32::try_from(size).unwrap_or(u32::MAX),
            }),
        }
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        let (b, c) = match w {
            MemWidth::M8 => (0, val as u32 & 0xff),
            MemWidth::M32 => (0, val as u32),
            MemWidth::M64 => ((val >> 32) as u32, val as u32),
        };
        self.op(OpKind::Mwr, w as u8, 0, addr, b, c)?;
        for i in 0..w as u32 {
            self.mem
                .insert(addr.wrapping_add(i), (val >> (8 * i)) as u8);
        }
        Ok(())
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.op(OpKind::Mrd, 4, 0, addr, 0, 0)?;
        let mut v = 0u32;
        for i in 0..4 {
            v |= u32::from(self.mem.get(&addr.wrapping_add(i)).copied().unwrap_or(0)) << (8 * i);
        }
        Ok(v)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        for (i, b) in data.iter().enumerate() {
            self.mem.insert(addr.wrapping_add(i as u32), *b);
        }
        Ok(())
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self
                .mem
                .get(&addr.wrapping_add(i as u32))
                .copied()
                .unwrap_or(0);
        }
        Ok(())
    }
    fn now_ms(&mut self) -> u64 {
        self.clock_ms
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        self.trace.push(TraceEntry::Note(text.into()));
        Ok(())
    }
}
