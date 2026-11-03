//! GX-7b: the bring-up register owner. Every register write of the GPU bring-up (PSP, SMU, GC, hubs, IH: GX-7d
//! on) goes through `Bringup`, which reads each write back and keeps it in a large log of its own; the display's
//! write log (`dcn`'s 64-slot ring) stays the display's, for its read-back and undo.
//!
//! `Bringup` wraps any `psp::Hw` (the fake in host tests and ktests, the live BAR5 owner on the Mini-PC) and is a
//! `psp::Hw` itself, so the PSP code runs over it unchanged. Writes pass only while its gate is open: the live
//! owner of GX-7b is built with the gate shut (`Gate::Refuse`), so this item writes nothing to the real GPU;
//! refused writes are logged too (refused memory writes are counted and klogged; their bytes are not kept).
//!
//! No fixed counts (R1): the log keeps the newest `keep` entries (chosen by the owner; `LOG_KEEP` by default) and
//! counts every write, per register, without limit.

use super::psp::Hw;
use alloc::collections::{BTreeMap, VecDeque};

/// The default number of recent writes kept in full (a whole GX-7d bring-up is a few hundred writes).
pub const LOG_KEEP: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Written; the read-back matched in the checked bits (or no bits are checked for this register).
    Ok,
    /// Written, but the checked bits read back differently.
    Mismatch,
    /// The device refused the write (outside the BAR, a fake's refusal).
    Failed,
    /// The gate was shut: nothing reached the device.
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Counts every write since the owner was made, refused ones included.
    pub seq: u64,
    pub byte: u32,
    pub value: u32,
    /// What the register read right after the write (`None`: refused, failed or unreadable).
    pub read_back: Option<u32>,
    pub outcome: Outcome,
}

/// Totals that stay stable between two reads when nothing is written (the probe's answer prints them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub writes: u64,
    pub refused: u64,
    pub failed: u64,
    pub mismatched: u64,
    /// Memory writes (rings, TMR) refused by a shut gate: not register writes, so not in `writes` or the log.
    pub mem_refused: u64,
    pub registers: usize,
}

pub struct WriteLog {
    keep: usize,
    recent: VecDeque<Entry>,
    totals: Totals,
    per_reg: BTreeMap<u32, u64>,
}

impl WriteLog {
    pub fn new(keep: usize) -> WriteLog {
        WriteLog { keep: keep.max(1), recent: VecDeque::new(), totals: Totals::default(), per_reg: BTreeMap::new() }
    }

    fn push(&mut self, byte: u32, value: u32, read_back: Option<u32>, outcome: Outcome) -> Entry {
        let e = Entry { seq: self.totals.writes, byte, value, read_back, outcome };
        self.totals.writes += 1;
        match outcome {
            Outcome::Ok => {}
            Outcome::Mismatch => self.totals.mismatched += 1,
            Outcome::Failed => self.totals.failed += 1,
            Outcome::Refused => self.totals.refused += 1,
        }
        *self.per_reg.entry(byte).or_insert(0) += 1;
        self.totals.registers = self.per_reg.len();
        if self.recent.len() == self.keep {
            self.recent.pop_front();
        }
        self.recent.push_back(e);
        e
    }

    pub fn totals(&self) -> Totals {
        self.totals
    }

    /// The kept entries, oldest first.
    pub fn recent(&self) -> impl Iterator<Item = &Entry> {
        self.recent.iter()
    }

    /// The kept entries from `seq` on (a step's own writes: take `totals().writes` before it).
    pub fn since(&self, seq: u64) -> impl Iterator<Item = &Entry> {
        self.recent.iter().filter(move |e| e.seq >= seq)
    }

    fn mem_refused(&mut self) {
        self.totals.mem_refused += 1;
    }

    /// Writes to `byte` ever (refused ones included).
    pub fn count(&self, byte: u32) -> u64 {
        self.per_reg.get(&byte).copied().unwrap_or(0)
    }
}

/// Whether writes reach the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Allow,
    /// Shut, with the reason the log and the probe give.
    Refuse(&'static str),
}

pub struct Bringup<H> {
    hw: H,
    log: WriteLog,
    gate: Gate,
    /// Bits that must read back as written, per register (absent: mailboxes and self-clearing bits, only logged).
    checks: BTreeMap<u32, u32>,
}

impl<H: Hw> Bringup<H> {
    pub fn new(hw: H, gate: Gate, keep: usize) -> Bringup<H> {
        Bringup { hw, log: WriteLog::new(keep), gate, checks: BTreeMap::new() }
    }

    pub fn log(&self) -> &WriteLog {
        &self.log
    }

    pub fn gate(&self) -> Gate {
        self.gate
    }

    pub fn hw(&mut self) -> &mut H {
        &mut self.hw
    }

    /// From now on a write to `byte` counts as a mismatch when `mask`'s bits read back differently.
    pub fn expect_read_back(&mut self, byte: u32, mask: u32) {
        self.checks.insert(byte, mask);
    }

    /// GX-7h: one write without the read-back (an invalidation engine's semaphore: reading it acquires it,
    /// gmc_v11_0.c:221-231), logged with no read-back.
    pub fn write_blind(&mut self, byte: u32, value: u32) -> Entry {
        if let Gate::Refuse(why) = self.gate {
            klog::event!(Debug, "gx", "bringup.write_refused", byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)), why = why);
            return self.log.push(byte, value, None, Outcome::Refused);
        }
        if !self.hw.reg_write(byte, value) {
            klog::event!(Warn, "gx", "bringup.write_failed", byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)));
            return self.log.push(byte, value, None, Outcome::Failed);
        }
        self.log.push(byte, value, None, Outcome::Ok)
    }

    /// One write with its read-back; the entry as logged.
    pub fn write(&mut self, byte: u32, value: u32) -> Entry {
        if let Gate::Refuse(why) = self.gate {
            klog::event!(Debug, "gx", "bringup.write_refused", byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)), why = why);
            return self.log.push(byte, value, None, Outcome::Refused);
        }
        if !self.hw.reg_write(byte, value) {
            klog::event!(Warn, "gx", "bringup.write_failed", byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)));
            return self.log.push(byte, value, None, Outcome::Failed);
        }
        let read_back = self.hw.reg_read(byte);
        let mask = self.checks.get(&byte).copied().unwrap_or(0);
        let outcome = match read_back {
            Some(r) if (r ^ value) & mask != 0 => Outcome::Mismatch,
            None if mask != 0 => Outcome::Mismatch,
            _ => Outcome::Ok,
        };
        if outcome == Outcome::Mismatch {
            klog::event!(Warn, "gx", "bringup.read_back", byte = klog::Hex(u64::from(byte)), wrote = klog::Hex(u64::from(value)),
                read = klog::Hex(u64::from(read_back.unwrap_or(u32::MAX))));
        }
        self.log.push(byte, value, read_back, outcome)
    }
}

impl<H: Hw> Hw for Bringup<H> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.hw.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        matches!(self.write(byte, value).outcome, Outcome::Ok | Outcome::Mismatch)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.hw.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        if let Gate::Refuse(why) = self.gate {
            klog::event!(Debug, "gx", "bringup.mem_write_refused", mc = klog::Hex(mc), bytes = data.len(), why = why);
            self.log.mem_refused();
            return false;
        }
        self.hw.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.hw.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.hw.delay_us(us)
    }
    fn flush_hdp(&mut self) {
        self.hw.flush_hdp()
    }
    fn invalidate_hdp(&mut self) {
        self.hw.invalidate_hdp()
    }
    fn mem_write64(&mut self, mc: u64, value: u64) -> bool {
        if let Gate::Refuse(why) = self.gate {
            klog::event!(Debug, "gx", "bringup.mem_write_refused", mc = klog::Hex(mc), bytes = 8, why = why);
            self.log.mem_refused();
            return false;
        }
        self.hw.mem_write64(mc, value)
    }
    fn store_barrier(&mut self) {
        self.hw.store_barrier()
    }    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        if let Gate::Refuse(why) = self.gate {
            klog::event!(Debug, "gx", "bringup.doorbell_refused", index = index_dw, why = why);
            return false;
        }
        self.hw.doorbell_write64(index_dw, value)
    }
}
