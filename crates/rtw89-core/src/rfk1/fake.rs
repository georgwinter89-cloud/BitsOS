//! Host tests only: test-rfk1-fake.mjs's `RfFake` in Rust (no driver code). A chip with a small
//! RF register file behind SWSI, chosen answers for BB/MAC registers and RF registers, "busy
//! once" registers, and a simulated clock in us that only delays move. Every op is recorded as
//! a trace entry (the lines of bringup-golden.txt); [`RfFake::ops`] decodes them into RF/BB
//! operations as the JS `decode` does.
//!
//! As the JS FakeDev: polls pass without a read (unless their address is in `fail_polls`, the
//! FakeDev's failing `onPoll`), an RMW reads through the answers, a register never written
//! reads 0. rfk2's tests use it as rfk2-tchip.mjs's `makeChip` too.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::bus::{Bus, BusError, BusResult, MemWidth, Width, field, lowbit, prep};
use crate::membus::{DMA_BASE, DMA_END, bump_alloc};
use crate::phy::io::BB;
use crate::tables::regs::*;
use crate::trace::{Op, OpKind, TraceEntry};

const RFMASK: u32 = 0xfffff;
const SW_DATA: u32 = BB + R_SWSI_DATA_V1;
const SW_MASK: u32 = BB + R_SWSI_BIT_MASK_V1;
const SW_RADDR: u32 = BB + R_SWSI_READ_ADDR_V1;
const SW_V1: u32 = BB + R_SWSI_V1;
const RF_BASE: [u32; 2] = [0xe000, 0xf000];

/// Where an answer or a "busy once" rule sits
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// a BB register (offset from 0x10000)
    Bb(u32),
    /// a MAC register
    Mac(u32),
    /// an RF register behind SWSI: path, address
    Rf(u32, u32),
}

/// The register file as answers see it
#[derive(Debug, Default, Clone)]
pub struct Regs(BTreeMap<u32, u32>);

impl Regs {
    pub fn get(&self, a: u32, w: Width) -> u32 {
        let word = self.0.get(&(a & !3)).copied().unwrap_or(0);
        (word >> ((a & 3) * 8)) & w.mask()
    }
    fn put(&mut self, a: u32, w: Width, v: u32) {
        let base = a & !3;
        let sh = (a & 3) * 8;
        let m = w.mask().wrapping_shl(sh);
        let old = self.0.get(&base).copied().unwrap_or(0);
        self.0.insert(base, (old & !m) | (v.wrapping_shl(sh) & m));
    }
    /// A BB register's stored value
    pub fn bb(&self, addr: u32) -> u32 {
        self.get(BB + addr, Width::W32)
    }
}

pub type AnsFn = Box<dyn FnMut(&Regs) -> u32>;
pub type RfAnsFn = Box<dyn FnMut(&Regs, u32) -> u32>;
pub type SlowFn = Box<dyn Fn(u32) -> u32>;

pub enum Answer {
    Val(u32),
    F(AnsFn),
}

pub enum RfAnswer {
    Val(u32),
    /// gets the reads of this register so far
    F(RfAnsFn),
}

/// A decoded op (JS test-rfk1-fake.mjs `decode`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DOp {
    RfW {
        path: u32,
        addr: u32,
        mask: u32,
        data: u32,
    },
    RfR {
        path: u32,
        addr: u32,
    },
    BbW {
        addr: u32,
        v: u32,
    },
    BbM {
        addr: u32,
        clear: u32,
        set: u32,
    },
    BbR {
        addr: u32,
    },
    BbPoll {
        addr: u32,
        mask: u32,
        want: u32,
    },
    Delay {
        us: u32,
    },
    /// anything else, as its trace line
    Other(String),
}

pub struct RfFake {
    pub regs: Regs,
    pub rf: BTreeMap<(u32, u32), u32>,
    pub answers: BTreeMap<Key, Answer>,
    pub rf_ans: BTreeMap<(u32, u32), RfAnswer>,
    pub slow: BTreeMap<Key, SlowFn>,
    /// polls of these addresses fail (rfk2-tchip.mjs `failPoll`); every other poll passes
    pub fail_polls: alloc::collections::BTreeSet<u32>,
    rf_reads: BTreeMap<(u32, u32), u32>,
    slow_seen: bool,
    /// simulated time in us
    pub sim_us: u64,
    pub trace: Vec<TraceEntry>,
    /// the simulated time each trace entry ran at
    pub times: Vec<u64>,
    mem: BTreeMap<u32, u8>,
    dma_next: u32,
}

impl Default for RfFake {
    fn default() -> RfFake {
        RfFake::new()
    }
}

impl RfFake {
    /// A chip that reads 0 everywhere
    pub fn new() -> RfFake {
        RfFake {
            regs: Regs::default(),
            rf: BTreeMap::new(),
            answers: BTreeMap::new(),
            rf_ans: BTreeMap::new(),
            slow: BTreeMap::new(),
            fail_polls: alloc::collections::BTreeSet::new(),
            rf_reads: BTreeMap::new(),
            slow_seen: false,
            sim_us: 0,
            trace: Vec::new(),
            times: Vec::new(),
            mem: BTreeMap::new(),
            dma_next: DMA_BASE,
        }
    }

    /// test-rfk1-fake.mjs `fresh`: the answers that let every calibration finish at its first
    /// poll and the RF state after RF init
    pub fn fresh() -> RfFake {
        let mut f = RfFake::new();
        good_answers(&mut f);
        good_rf(&mut f);
        f
    }

    pub fn answer(&mut self, k: Key, v: u32) -> &mut Self {
        self.answers.insert(k, Answer::Val(v));
        self
    }
    pub fn answer_fn(&mut self, k: Key, f: impl FnMut(&Regs) -> u32 + 'static) -> &mut Self {
        self.answers.insert(k, Answer::F(Box::new(f)));
        self
    }
    pub fn rf_answer_fn(
        &mut self,
        path: u32,
        addr: u32,
        f: impl FnMut(&Regs, u32) -> u32 + 'static,
    ) -> &mut Self {
        self.rf_ans.insert((path, addr), RfAnswer::F(Box::new(f)));
        self
    }
    pub fn slow_fn(&mut self, k: Key, f: impl Fn(u32) -> u32 + 'static) -> &mut Self {
        self.slow.insert(k, Box::new(f));
        self
    }
    /// An RF register's value (behind SWSI, or the direct window for ADSEL addresses)
    pub fn rf_set(&mut self, path: u32, addr: u32, v: u32) {
        if addr & 0x10000 != 0 {
            self.set(rf_direct(path, addr), Width::W32, v);
        } else {
            self.rf.insert((path, addr), v);
        }
    }
    /// An RF register as the driver would read it now, without counting a read
    pub fn peek(&self, path: u32, addr: u32) -> u32 {
        if addr & 0x10000 != 0 {
            return self.regs.get(rf_direct(path, addr), Width::W32) & RFMASK;
        }
        self.rf.get(&(path, addr)).copied().unwrap_or(0)
    }
    pub fn bb(&self, addr: u32) -> u32 {
        self.regs.bb(addr)
    }
    /// A register's value as the chip holds it at the start (no trace op, JS `d._set`)
    pub fn poke(&mut self, a: u32, w: Width, v: u32) {
        self.regs.put(a, w, v);
    }

    /// The recorded trace as golden-file lines
    pub fn lines(&self) -> Vec<String> {
        crate::trace::lines(&self.trace)
    }

    fn record(&mut self, kind: OpKind, size: u8, ms: u16, a: u32, b: u32, c: u32) {
        self.trace
            .push(TraceEntry::Op(Op::new(kind, size, ms, a, b, c)));
        self.times.push(self.sim_us);
    }

    fn set(&mut self, a: u32, w: Width, v: u32) {
        self.regs.put(a, w, v);
        if a != SW_RADDR && a != SW_MASK {
            self.slow_seen = false;
        }
        if a == SW_DATA {
            self.rf_write(v);
        }
    }

    fn rf_write(&mut self, v: u32) {
        let path = field(v, B_SWSI_DATA_PATH_V1);
        let addr = field(v, B_SWSI_DATA_ADDR_V1);
        let val = field(v, B_SWSI_DATA_VAL_V1);
        let mask = if field(v, B_SWSI_DATA_BIT_MASK_EN_V1) != 0 {
            self.regs.get(SW_MASK, Width::W32) & RFMASK
        } else {
            RFMASK
        };
        let old = self.rf.get(&(path, addr)).copied().unwrap_or(0);
        let mut nv = ((old & !mask) | (val & mask)) & RFMASK;
        if addr == RR_RCKC && nv == 0x240 {
            nv |= 0x11 << 10; // the RCK result appears
        }
        self.rf.insert((path, addr), nv);
    }

    fn slowly(&mut self, k: Key, v: u32) -> u32 {
        match self.slow.get(&k) {
            Some(f) if !self.slow_seen => {
                self.slow_seen = true;
                f(v)
            }
            _ => v,
        }
    }

    fn rf_get(&mut self, path: u32, addr: u32) -> u32 {
        if addr & 0x10000 != 0 {
            return self.regs.get(rf_direct(path, addr), Width::W32) & RFMASK;
        }
        let n = self.rf_reads.get(&(path, addr)).copied().unwrap_or(0);
        self.rf_reads.insert((path, addr), n + 1);
        let v = match self.rf_ans.get_mut(&(path, addr)) {
            None => self.rf.get(&(path, addr)).copied().unwrap_or(0),
            Some(RfAnswer::Val(v)) => *v,
            Some(RfAnswer::F(f)) => f(&self.regs, n),
        };
        self.slowly(Key::Rf(path, addr), v)
    }

    // test-rfk1-fake.mjs `answer`: the value a read of `a` sees, `None` for the stored one
    fn answer_of(&mut self, a: u32) -> Option<u32> {
        if a == SW_V1 {
            let ra = self.regs.get(SW_RADDR, Width::W32);
            let path = field(ra, B_SWSI_READ_ADDR_PATH_V1);
            let addr = field(ra, B_SWSI_READ_ADDR_ADDR_V1);
            return Some(self.rf_get(path, addr) | B_SWSI_R_DATA_DONE_V1);
        }
        let k = if a >= BB {
            Key::Bb(a - BB)
        } else {
            Key::Mac(a)
        };
        let v = match self.answers.get_mut(&k) {
            None => None,
            Some(Answer::Val(v)) => Some(*v),
            Some(Answer::F(f)) => Some(f(&self.regs)),
        };
        if self.slow.contains_key(&k) {
            let base = v.unwrap_or_else(|| self.regs.get(a, Width::W32));
            return Some(self.slowly(k, base));
        }
        v
    }

    fn read_val(&mut self, a: u32, w: Width) -> u32 {
        match self.answer_of(a) {
            Some(v) => v & w.mask(),
            None => self.regs.get(a, w),
        }
    }

    /// The trace as RF/BB operations (JS `decode`): SWSI writes and reads become RF ops (the
    /// SWSI polls, the read's 2 us and the direct write's 1 us go), BB ops lose the 0x10000
    pub fn ops(&self) -> Vec<DOp> {
        decode(&self.trace)
    }
}

fn rf_direct(path: u32, addr: u32) -> u32 {
    BB + RF_BASE.get(path as usize).copied().unwrap_or(0) + ((addr & 0xff) << 2)
}

/// The JS `decode` of a trace
pub fn decode(trace: &[TraceEntry]) -> Vec<DOp> {
    let mut out = Vec::new();
    let mut skip: Option<u32> = None;
    let mut sw_mask = RFMASK;
    for e in trace {
        let TraceEntry::Op(op) = e else { continue };
        if let Some(us) = skip.take() {
            if op.kind == OpKind::Delay && op.a == us {
                continue;
            }
            out.push(DOp::Other(String::from(
                "(the RF access's delay is missing)",
            )));
        }
        let off = op.a.wrapping_sub(BB);
        let dir = if op.a >= BB && (0xe000..0xe400).contains(&off) {
            Some(0)
        } else if op.a >= BB && (0xf000..0xf400).contains(&off) {
            Some(1)
        } else {
            None
        };
        match (op.kind, op.a) {
            (OpKind::Poll | OpKind::Rd, SW_V1) => {}
            (OpKind::Rmw, SW_MASK) => sw_mask = op.c & RFMASK,
            (OpKind::Wr, SW_DATA) => {
                let mask = if field(op.c, B_SWSI_DATA_BIT_MASK_EN_V1) != 0 {
                    sw_mask
                } else {
                    RFMASK
                };
                out.push(DOp::RfW {
                    path: field(op.c, B_SWSI_DATA_PATH_V1),
                    addr: field(op.c, B_SWSI_DATA_ADDR_V1),
                    mask,
                    data: (field(op.c, B_SWSI_DATA_VAL_V1) & mask) >> lowbit(mask).min(31),
                });
            }
            (OpKind::Rmw, SW_RADDR) => {
                out.push(DOp::RfR {
                    path: field(op.c, B_SWSI_READ_ADDR_PATH_V1),
                    addr: field(op.c, B_SWSI_READ_ADDR_ADDR_V1),
                });
                skip = Some(2);
            }
            (OpKind::Rmw, _) if dir.is_some() => {
                out.push(DOp::RfW {
                    path: dir.unwrap_or(0),
                    addr: 0x10000 | ((off & 0x3ff) >> 2),
                    mask: op.b,
                    data: op.c >> lowbit(op.b).min(31),
                });
                skip = Some(1);
            }
            (OpKind::Rd, _) if dir.is_some() => out.push(DOp::RfR {
                path: dir.unwrap_or(0),
                addr: 0x10000 | ((off & 0x3ff) >> 2),
            }),
            (OpKind::Delay, _) => out.push(DOp::Delay { us: op.a }),
            (OpKind::Wr, a) if a >= BB => out.push(DOp::BbW { addr: off, v: op.c }),
            (OpKind::Rmw, a) if a >= BB => out.push(DOp::BbM {
                addr: off,
                clear: op.b,
                set: op.c,
            }),
            (OpKind::Rd, a) if a >= BB => out.push(DOp::BbR { addr: off }),
            (OpKind::Poll, a) if a >= BB => out.push(DOp::BbPoll {
                addr: off,
                mask: op.b,
                want: op.c,
            }),
            _ => out.push(DOp::Other(op.line())),
        }
    }
    out
}

impl Bus for RfFake {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.record(OpKind::Rd, w.bytes(), 0, addr, 0, 0);
        Ok(self.read_val(addr, w))
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        let val = val & w.mask();
        self.record(OpKind::Wr, w.bytes(), 0, addr, 0, val);
        self.set(addr, w, val);
        Ok(())
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.record(OpKind::Rmw, w.bytes(), 0, addr, clear, set);
        let v = (self.read_val(addr, w) & !clear) | set;
        self.set(addr, w, v);
        Ok(())
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.record(OpKind::Poll, w.bytes(), ms, addr, mask, want);
        if self.fail_polls.contains(&addr) {
            // the JS FakeDev's failing onPoll: no read happens
            return Err(BusError::PollTimeout {
                addr,
                width: w,
                mask,
                want,
                last: self.regs.get(addr, w),
            });
        }
        Ok(())
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.record(OpKind::Delay, 0, 0, us, 0, 0);
        self.sim_us += u64::from(us);
        Ok(())
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.record(OpKind::Cfgr, 4, 0, off.into(), 0, 0);
        Ok(0)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.record(OpKind::Cfgw, 4, 0, off.into(), 0, val);
        Ok(())
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        bump_alloc(&mut self.dma_next, DMA_END, size, align)
            .ok_or(BusError::DmaExhausted { size, align })
    }
    fn dma_free(&mut self, _addr: u32, _size: usize) -> BusResult<()> {
        Ok(())
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        let (b, c) = ((val >> 32) as u32, val as u32);
        self.record(OpKind::Mwr, w as u8, 0, addr, b, c);
        let bytes = val.to_le_bytes();
        for (i, &x) in bytes.iter().take(w as usize).enumerate() {
            self.mem.insert(addr.wrapping_add(i as u32), x);
        }
        Ok(())
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.record(OpKind::Mrd, 4, 0, addr, 0, 0);
        let mut b = [0u8; 4];
        self.mem_read_bytes(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        for (i, &x) in data.iter().enumerate() {
            self.mem.insert(addr.wrapping_add(i as u32), x);
        }
        Ok(())
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        for (i, x) in out.iter_mut().enumerate() {
            *x = self
                .mem
                .get(&addr.wrapping_add(i as u32))
                .copied()
                .unwrap_or(0);
        }
        Ok(())
    }
    fn now_ms(&mut self) -> u64 {
        self.sim_us / 1000
    }
}

// test-rfk1-fake.mjs goodAnswers
fn good_answers(f: &mut RfFake) {
    let idx = |r: &Regs, reg: u32, mask: u32| field(r.bb(reg), mask);
    f.answer(Key::Bb(0xbff8), 0x55) // NCTL done
        .answer(
            Key::Bb(R_DRCK_RS),
            B_DRCK_RS_DONE | prep(B_DRCK_RS_LPS, 0x15),
        )
        .answer(
            Key::Bb(R_ADDCKR0),
            0x1 | prep(B_ADDCKR0_A0, 0x123) | prep(B_ADDCKR0_A1, 0x2a5),
        )
        .answer(
            Key::Bb(R_ADDCKR1),
            0x1 | prep(B_ADDCKR1_A0, 0x0f0) | prep(B_ADDCKR1_A1, 0x0c3),
        )
        .answer(Key::Bb(R_DACK_S0P0), B_DACK_S0P0_OK)
        .answer(Key::Bb(R_DACK_S0P1), B_DACK_S0P1_OK)
        .answer(Key::Bb(R_DACK_S1P0), B_DACK_S1P0_OK)
        .answer(Key::Bb(R_DACK_S1P1), B_DACK_S1P1_OK)
        .answer_fn(Key::Bb(R_DACK_S0P2), move |r| {
            B_DACK_S0P2_OK | prep(B_DACK_S0M0, 0x40 + idx(r, R_DCOF0, B_DCOF0_V))
        })
        .answer_fn(Key::Bb(R_DACK_S0P3), move |r| {
            B_DACK_S0P3_OK | prep(B_DACK_S0M1, 0x50 + idx(r, R_DCOF8, B_DCOF8_V))
        })
        .answer_fn(Key::Bb(R_DACK10S), move |r| {
            B_DACK_S1P2_OK | prep(B_DACK10S, 0x60 + idx(r, R_DACK10, B_DACK10))
        })
        .answer_fn(Key::Bb(R_DACK11S), move |r| {
            B_DACK_S1P3_OK | prep(B_DACK11S, 0x70 + idx(r, R_DACK11, B_DACK11))
        })
        .answer(Key::Bb(R_DACK_BIAS00), prep(B_DACK_BIAS00, 0x201))
        .answer(Key::Bb(R_DACK_BIAS01), prep(B_DACK_BIAS01, 0x1a5))
        .answer(Key::Bb(R_DACK_DADCK00), prep(B_DACK_DADCK00, 0x3c))
        .answer(Key::Bb(R_DACK_DADCK01), prep(B_DACK_DADCK01, 0x7e))
        .answer(Key::Bb(R_DACK_BIAS10), prep(B_DACK_BIAS10, 0x2c6))
        .answer(Key::Bb(R_DACK_BIAS11), prep(B_DACK_BIAS11, 0x0d7))
        .answer(Key::Bb(R_DACK_DADCK10), prep(B_DACK_DADCK10, 0x48))
        .answer(Key::Bb(R_DACK_DADCK11), prep(B_DACK_DADCK11, 0x59))
        .answer(Key::Bb(R_DBG32_D), 0xffe003) // dc_re -2, dc_im 3
        .answer(Key::Bb(R_CIRST), prep(B_CIRST_SYN, 1))
        .answer(Key::Mac(R_AX_CTN_TXEN), 0xffff);
}

// test-rfk1-fake.mjs goodRf: the RF state after RF init, as far as the calibrations look at it
fn good_rf(f: &mut RfFake) {
    for p in [0u32, 1] {
        f.rf.insert((p, RR_RCKS), 0x8); // RCK done
        f.rf.insert((p, RR_SYNFB), RR_SYNFB_LK); // LO locked
        f.rf.insert(
            (p, RR_TXMO),
            prep(RR_TXMO_COI, 0x10) | prep(RR_TXMO_COQ, 0x10),
        );
        f.rf.insert(
            (p, RR_LOKVB),
            prep(RR_LOKVB_COI, 0x20) | prep(RR_LOKVB_COQ, 0x20),
        );
        f.rf.insert((p, RR_RSV1), 0x3);
        f.rf.insert((p, RR_DCK), 0x6);
        f.rf.insert((p, RR_MOD), 0x30000 + p);
        f.rf.insert((p, RR_CFGCH), 0x01c01 | (p << 19));
    }
}
