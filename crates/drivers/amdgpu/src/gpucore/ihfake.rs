//! GX-7n: the IH block in the fakes: what `ih_v6_0` and the chip agree on. An enabled ring takes entries at its write
//! pointer in the fake's memory, writes the pointer back, and sends a message when armed; with RPTR_REARM only the read
//! pointer's update arms it again, and a message follows at once if entries are still there (the bug of forgetting that
//! shows here as on the chip: one message, then none). Past the read pointer it overwrites and sets RB_OVERFLOW. The
//! engines' `Event::Interrupt`s (GR-2) and the hub's faults arrive only with their enable bits set.

use super::ih::IhAddrs;
use crate::hw::ih::{IhIo, IrqLine, Woke};
use crate::ipmod::ih6::{self, Iv, f};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use drm::fence::{FenceCb, Waiter};

pub struct IhFake {
    pub a: Option<IhAddrs>,
    pub wptr: u32,
    pub rptr: u32,
    pub overflow: bool,
    armed: bool,
    /// Messages sent and entries written, this fake's whole life.
    pub messages: u64,
    pub written: u64,
    pub refused: u64,
    /// Entries are written and no message goes out.
    pub lose_messages: bool,
    pub msi: Option<Arc<dyn Fn() + Send + Sync>>,
    /// How far the engines' events and the hub's faults are read: SDMA, GFX, MES, faults.
    pub seen: [usize; 4],
}

impl IhFake {
    pub fn new(d: &super::disc::Discovery) -> IhFake {
        IhFake { a: IhAddrs::from_discovery(d), wptr: 0, rptr: 0, overflow: false, armed: true, messages: 0, written: 0, refused: 0, lose_messages: false, msi: None, seen: [0; 4] }
    }
    pub fn owns_ptr(&self, byte: u32) -> bool {
        self.a.is_some_and(|a| byte == a.rptr || byte == a.wptr)
    }
    pub fn read_ptr(&self, byte: u32) -> u32 {
        if self.a.is_some_and(|a| byte == a.rptr) { self.rptr } else { self.wptr | u32::from(self.overflow) }
    }
    pub fn is_cntl(&self, byte: u32) -> bool {
        self.a.is_some_and(|a| byte == a.cntl)
    }
    /// True: entries are still there, and with RPTR_REARM a message goes now.
    pub fn set_rptr(&mut self, v: u32) -> bool {
        self.rptr = v & (ih6::RING_BYTES - 1);
        self.armed = true;
        self.rptr != self.wptr
    }
    pub fn send(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if self.lose_messages {
            return;
        }
        self.messages += 1;
        if let Some(m) = &self.msi {
            m();
        }
    }
    /// The ring off: pointers at 0 by the driver's writes, the next message free.
    pub fn off(&mut self) {
        self.armed = true;
    }
}

/// A fault's or an engine's entry.
pub fn iv(client: u8, src: u8, ring_id: u8, vmid: u8, data0: u32) -> Iv {
    Iv { client, src, ring_id, vmid, data: [data0, 0, 0, 0], ..Iv::default() }
}

/// What a fake's message wakes, the same trait the chip's MSI line answers.
pub struct TestLine {
    wait: Arc<Waiter>,
    pending: AtomicBool,
    released: AtomicBool,
    messages: AtomicU64,
    /// Waits ended by a message or a timeout, for the tests.
    pub waits: AtomicU32,
}

impl TestLine {
    pub fn new() -> Arc<TestLine> {
        Arc::new(TestLine { wait: Waiter::new(), pending: AtomicBool::new(false), released: AtomicBool::new(false), messages: AtomicU64::new(0), waits: AtomicU32::new(0) })
    }
    pub fn fire(&self) {
        self.messages.fetch_add(1, Ordering::AcqRel);
        self.pending.store(true, Ordering::Release);
        self.wait.call();
    }
}

impl IrqLine for TestLine {
    fn wait(&self, ns: u64) -> Woke {
        self.waits.fetch_add(1, Ordering::Relaxed);
        if self.released.load(Ordering::Acquire) {
            return Woke::Look;
        }
        if self.pending.swap(false, Ordering::AcqRel) {
            return Woke::Message;
        }
        if ns == 0 {
            return Woke::Timeout;
        }
        self.wait.sleep(ns);
        if self.pending.swap(false, Ordering::AcqRel) { Woke::Message } else { Woke::Timeout }
    }
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.wait.call();
    }
    fn messages(&self) -> u64 {
        self.messages.load(Ordering::Acquire)
    }
    fn describe(&self) -> String {
        format!("a fake's line ({} waits)", self.waits.load(Ordering::Relaxed))
    }
}

/// A ring in plain memory, driven by the test alone: for the reader's own rules and the edu device's real MSI.
pub struct MemRing {
    state: ksync::SpinLock<MemState>,
}

pub struct MemState {
    pub ring: alloc::vec::Vec<u32>,
    pub wb: u32,
    pub wreg: u32,
    pub cntl: u32,
    pub rptr: u32,
    pub rptr_writes: u32,
    pub clears: u32,
    /// Entries a chip writes while the read pointer is on its way (`set_rptr`): the classic race.
    pub on_rptr: alloc::vec::Vec<[u32; 8]>,
    /// The write-back does not move, only the register would.
    pub bad_reads: bool,
}

impl MemRing {
    pub fn new() -> Arc<MemRing> {
        let ring = alloc::vec![0u32; (ih6::RING_BYTES / 4) as usize];
        Arc::new(MemRing {
            state: ksync::SpinLock::named("gx.ih.mem_ring", MemState { ring, wb: 0, wreg: 0, cntl: 0, rptr: 0, rptr_writes: 0, clears: 0, on_rptr: alloc::vec::Vec::new(), bad_reads: false }),
        })
    }
    pub fn with<T>(&self, f: impl FnOnce(&mut MemState) -> T) -> T {
        f(&mut self.state.lock())
    }
    /// An entry at the write pointer, as the chip writes it (overwriting and flagging when it passes the read pointer).
    pub fn push(&self, e: [u32; 8]) {
        self.state.lock().push(e);
    }
}

impl MemState {
    pub fn push(&mut self, e: [u32; 8]) {
        let at = (self.wreg & !31) / 4;
        self.ring[at as usize..at as usize + 8].copy_from_slice(&e);
        let next = ((self.wreg & !31) + ih6::ENTRY_BYTES) & (ih6::RING_BYTES - 1);
        let over = next == self.rptr || self.wreg & f::RB_OVERFLOW != 0;
        self.wreg = next | u32::from(over);
        self.wb = self.wreg;
    }
}

impl IhIo for MemRing {
    fn entry(&self, off: u32, out: &mut [u32; 8]) -> bool {
        let s = self.state.lock();
        let at = (off / 4) as usize;
        match s.ring.get(at..at + 8) {
            Some(d) => {
                out.copy_from_slice(d);
                true
            }
            None => false,
        }
    }
    fn wptr_wb(&self) -> Option<u32> {
        let s = self.state.lock();
        (!s.bad_reads).then_some(s.wb)
    }
    fn wptr_reg(&self) -> Option<u32> {
        let s = self.state.lock();
        (!s.bad_reads).then_some(s.wreg)
    }
    fn cntl(&self) -> Option<u32> {
        Some(self.state.lock().cntl)
    }
    fn set_cntl(&self, v: u32) -> bool {
        let mut s = self.state.lock();
        s.cntl = v & !f::WPTR_OVERFLOW_CLEAR;
        if v & f::WPTR_OVERFLOW_CLEAR != 0 {
            s.clears += 1;
            s.wreg &= !f::RB_OVERFLOW;
            s.wb = s.wreg;
        }
        true
    }
    fn set_rptr(&self, v: u32) -> bool {
        let mut s = self.state.lock();
        s.rptr = v;
        s.rptr_writes += 1;
        let more = core::mem::take(&mut s.on_rptr);
        for e in more {
            s.push(e);
        }
        true
    }
}
