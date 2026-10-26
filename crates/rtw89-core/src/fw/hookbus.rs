//! Host tests only: [`MemBus`] with test-fw.mjs's FakeDev hooks: `on_read`
//! answers a register read (kit FakeDev `onRead`), `on_poll` decides whether a
//! poll passes (`onPoll`; an error stands for a lost connection; a failed poll's
//! value for the error is the stored one, so the hook is asked once per access), and every
//! untraced memory write is kept (test-fw.mjs `d.writes`), so a test can check
//! the bytes of each H2C and FWDL piece.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::bus::{Bus, BusError, BusResult, MemWidth, Width};
use crate::membus::MemBus;

type ReadHook<'a> = Box<dyn FnMut(u32) -> Option<u32> + 'a>;
type PollHook<'a> = Box<dyn FnMut(u32, u32) -> Result<bool, BusError> + 'a>;

pub struct HookBus<'a> {
    pub mem: MemBus,
    on_read: ReadHook<'a>,
    on_poll: PollHook<'a>,
    /// every `mem_write_bytes`: address and bytes
    pub writes: Vec<(u32, Vec<u8>)>,
}

impl<'a> HookBus<'a> {
    pub fn new(mem: MemBus) -> HookBus<'a> {
        HookBus {
            mem,
            on_read: Box::new(|_| None),
            on_poll: Box::new(|_, _| Ok(true)),
            writes: Vec::new(),
        }
    }
    /// `f(addr)`: the value a read of `addr` returns instead of the stored one
    pub fn on_read(&mut self, f: impl FnMut(u32) -> Option<u32> + 'a) {
        self.on_read = Box::new(f);
    }
    /// `f(addr, mask)`: whether the poll passes; an error ends it with that error
    pub fn on_poll(&mut self, f: impl FnMut(u32, u32) -> Result<bool, BusError> + 'a) {
        self.on_poll = Box::new(f);
    }
    fn answer(&mut self, w: Width, addr: u32, stored: u32) -> u32 {
        (self.on_read)(addr).map_or(stored, |v| v & w.mask())
    }
}

impl Bus for HookBus<'_> {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        let v = self.mem.read(w, addr)?;
        Ok(self.answer(w, addr, v))
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.mem.write(w, addr, val)
    }
    // as FakeDev: the old value is what a read would answer (the hook asked once)
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        let old = self.mem.peek(addr, w);
        self.mem.rmw(w, addr, clear, set)?;
        let old = self.answer(w, addr, old);
        self.mem.poke(addr, w, (old & !clear) | set);
        Ok(())
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.mem.poll(w, addr, mask, want, ms)?;
        if (self.on_poll)(addr, mask)? {
            return Ok(());
        }
        self.mem.poll_failures += 1;
        let last = self.mem.peek(addr, w);
        Err(BusError::PollTimeout {
            addr,
            width: w,
            mask,
            want,
            last,
        })
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.mem.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.mem.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.mem.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.mem.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.mem.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.mem.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.mem.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.writes.push((addr, data.to_vec()));
        self.mem.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.mem.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.mem.now_ms()
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        self.mem.note(text)
    }
}
