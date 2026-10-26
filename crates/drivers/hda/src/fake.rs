//! A register-level fake HD Audio controller for host tests, with QEMU intel-hda's access rules (a register is
//! found by its exact start address; a narrower access at the start is masked, anything else is lost and
//! traced as `BadAccess`), CORB/RIRB by "DMA" from fake memory (RINTCNT stall until RIRBSTS is cleared, as QEMU
//! does), immediate commands, stream descriptors that "play" by reading BDL entries and ring bytes into a sink
//! as the test advances them, IOC/INTSTS, and codecs that answer from `hdacodec` dumps (set verbs recorded,
//! power and pin-sense state kept). Knobs: AMD's LPIB = CBL at the wrap, a dead CORB, no UC/WC memory.

use crate::hw::{AllocError, DmaUse, Hw, Mem};
use crate::regs::*;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hdacodec::dump::Dump;
use ksync::SpinLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    BadAccess { off: usize, width: usize, write: bool },
    DmaWithoutBme,
    Enable(bool),
    PciOff,
    PciModify(u8, u32, u32),
    Alloc(usize, bool),
}

type Buf = Arc<SpinLock<Vec<u8>>>;

pub struct FakeMem {
    addr: u64,
    buf: Buf,
}

impl Mem for FakeMem {
    fn dev_addr(&self) -> u64 {
        self.addr
    }
    fn size(&self) -> usize {
        self.buf.lock().len()
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        let b = self.buf.lock();
        b.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn wr32(&self, off: usize, v: u32) -> bool {
        let mut b = self.buf.lock();
        b.get_mut(off..off + 4).map(|s| s.copy_from_slice(&v.to_le_bytes())).is_some()
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        let mut b = self.buf.lock();
        b.get_mut(off..off + src.len()).map(|s| s.copy_from_slice(src)).is_some()
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        let b = self.buf.lock();
        b.get(off..off + dst.len()).map(|s| dst.copy_from_slice(s)).is_some()
    }
    fn fill(&self, off: usize, n: usize, byte: u8) -> bool {
        let mut b = self.buf.lock();
        b.get_mut(off..off + n).map(|s| s.fill(byte)).is_some()
    }
}

pub struct FakeCodec {
    pub dump: Dump,
    /// Set verbs received, in order: (nid, verb).
    pub sets: Vec<(u8, u32)>,
    pub sense: BTreeMap<u8, u32>,
    power: BTreeMap<u8, u8>,
}

#[derive(Default, Clone)]
pub struct Sd {
    /// Control bytes 0-2 and status in byte 3, as QEMU keeps them.
    pub ctl: u32,
    lpib: u32,
    pub cbl: u32,
    pub lvi: u16,
    pub fmt: u16,
    bdpl: u32,
    bdpu: u32,
    idx: usize,
    off: u32,
    /// LPIB shows CBL until the next advance (AMD knob).
    at_wrap: bool,
    pub sink: Vec<u8>,
    /// Input descriptors (SN-9): what the codec "records", written into the ring by `advance`; zeros after it.
    pub source: Vec<u8>,
    src_pos: usize,
}

impl Sd {
    pub fn tag(&self) -> u8 {
        (self.ctl >> 20 & 0xF) as u8
    }
    pub fn running(&self) -> bool {
        self.ctl & SD_CTL_RUN as u32 != 0
    }
    pub fn bdl(&self) -> u64 {
        (self.bdpu as u64) << 32 | self.bdpl as u64
    }
}

#[derive(Default)]
struct Regs {
    gctl: u32,
    statests: u16,
    intctl: u32,
    corb: [u32; 2],
    corbwp: u16,
    corbrp: u16,
    corbctl: u8,
    corbsize: u8,
    rirb: [u32; 2],
    rirbwp: u16,
    rintcnt: u16,
    rirbctl: u8,
    rirbsts: u8,
    rirbsize: u8,
    rirb_count: u16,
    ic: u32,
    ir: u32,
    ics: u16,
    dpl: [u32; 2],
    sd: Vec<Sd>,
}

pub struct FakeHda {
    gcap: u16,
    regs: SpinLock<Regs>,
    pub codecs: SpinLock<Vec<Option<FakeCodec>>>,
    mem: SpinLock<BTreeMap<u64, Buf>>,
    next: AtomicU64,
    clock: AtomicU64,
    bus_master: AtomicBool,
    pub trace: SpinLock<Vec<Ev>>,
    pub amd_wrap: AtomicBool,
    pub corb_dead: AtomicBool,
    pub no_uc: AtomicBool,
    /// Engines ignore their stop (RUN/DMA bits and SRST stay); only a link reset stops them.
    pub stuck: AtomicBool,
}

impl FakeHda {
    /// `gcap` 0x4401 is QEMU's intel-hda (4 outputs, 4 inputs, 64-bit).
    pub fn new(gcap: u16) -> Arc<FakeHda> {
        let total = Gcap::parse(gcap).total();
        Arc::new(FakeHda {
            gcap,
            regs: SpinLock::new(Regs { corbsize: 0x42, rirbsize: 0x42, sd: vec![Sd::default(); total], ..Regs::default() }),
            codecs: SpinLock::new((0..16).map(|_| None).collect()),
            mem: SpinLock::new(BTreeMap::new()),
            next: AtomicU64::new(0x1_0000_0000),
            clock: AtomicU64::new(1_000_000),
            bus_master: AtomicBool::new(false),
            trace: SpinLock::new(Vec::new()),
            amd_wrap: AtomicBool::new(false),
            corb_dead: AtomicBool::new(false),
            no_uc: AtomicBool::new(false),
            stuck: AtomicBool::new(false),
        })
    }

    pub fn add_codec(&self, cad: u8, text: &str) {
        let dump = Dump::parse(text).expect("fixture parses");
        self.codecs.lock()[cad as usize] = Some(FakeCodec { dump, sets: Vec::new(), sense: BTreeMap::new(), power: BTreeMap::new() });
    }

    pub fn sets(&self, cad: u8) -> Vec<(u8, u32)> {
        self.codecs.lock()[cad as usize].as_ref().map_or_else(Vec::new, |c| c.sets.clone())
    }

    pub fn clear_sets(&self, cad: u8) {
        if let Some(c) = self.codecs.lock()[cad as usize].as_mut() {
            c.sets.clear();
        }
    }

    pub fn set_sense(&self, cad: u8, pin: u8, v: u32) {
        if let Some(c) = self.codecs.lock()[cad as usize].as_mut() {
            c.sense.insert(pin, v);
        }
    }

    pub fn sd(&self, i: usize) -> Sd {
        self.regs.lock().sd[i].clone()
    }

    /// Forces the run bit on (a stream a Reload left running).
    pub fn force_run(&self, i: usize) {
        self.regs.lock().sd[i].ctl |= SD_CTL_RUN as u32;
    }

    pub fn reg32(&self, off: usize) -> u32 {
        self.read(off, 4) as u32
    }

    pub fn bad_accesses(&self) -> Vec<Ev> {
        self.trace.lock().iter().filter(|e| matches!(e, Ev::BadAccess { .. })).cloned().collect()
    }

    pub fn has(&self, e: &Ev) -> bool {
        self.trace.lock().contains(e)
    }

    /// DMA buffers the driver still holds (the fake keeps its own reference to each).
    pub fn live_buffers(&self) -> usize {
        self.mem.lock().values().filter(|b| Arc::strong_count(b) > 1).count()
    }

    pub fn bus_master(&self) -> bool {
        self.bus_master.load(Ordering::Acquire)
    }

    pub fn mem_bytes(&self, addr: u64, n: usize) -> Option<Vec<u8>> {
        let (b, off) = self.mem_find(addr)?;
        let g = b.lock();
        g.get(off..off + n).map(|s| s.to_vec())
    }

    fn mem_find(&self, addr: u64) -> Option<(Buf, usize)> {
        let m = self.mem.lock();
        let (&base, b) = m.range(..=addr).next_back()?;
        let off = (addr - base) as usize;
        (off < b.lock().len()).then(|| (b.clone(), off))
    }

    fn dma_read(&self, addr: u64, n: usize) -> Option<Vec<u8>> {
        if !self.bus_master() {
            self.trace.lock().push(Ev::DmaWithoutBme);
            return None;
        }
        self.mem_bytes(addr, n)
    }

    fn dma_write(&self, addr: u64, d: &[u8]) -> bool {
        if !self.bus_master() {
            self.trace.lock().push(Ev::DmaWithoutBme);
            return false;
        }
        let Some((b, off)) = self.mem_find(addr) else { return false };
        let mut g = b.lock();
        g.get_mut(off..off + d.len()).map(|s| s.copy_from_slice(d)).is_some()
    }

    /// Bytes input descriptor `i` records from now on (SN-9).
    pub fn set_source(&self, i: usize, bytes: Vec<u8>) {
        let mut r = self.regs.lock();
        if let Some(s) = r.sd.get_mut(i) {
            s.source = bytes;
            s.src_pos = 0;
        }
    }

    fn dma_write32(&self, addr: u64, v: u32) -> bool {
        if !self.bus_master() {
            self.trace.lock().push(Ev::DmaWithoutBme);
            return false;
        }
        let Some((b, off)) = self.mem_find(addr) else { return false };
        let mut g = b.lock();
        g.get_mut(off..off + 4).map(|s| s.copy_from_slice(&v.to_le_bytes())).is_some()
    }

    fn codec_cmd(&self, word: u32) -> Option<u32> {
        let cad = (word >> 28) as usize;
        let nid = (word >> 20 & 0x7F) as u8;
        let verb = word & 0xF_FFFF;
        let mut cs = self.codecs.lock();
        let c = cs.get_mut(cad)?.as_mut()?;
        if hdacodec::verb::is_read_only(verb) {
            if verb == hdacodec::verb::GET_POWER_STATE
                && let Some(&p) = c.power.get(&nid)
            {
                return Some((p as u32) << 4 | p as u32);
            }
            if verb == hdacodec::verb::GET_PIN_SENSE
                && let Some(&s) = c.sense.get(&nid)
            {
                return Some(s);
            }
            return c.dump.get(cad as u8, nid, verb).flatten();
        }
        c.sets.push((nid, verb));
        if verb >> 8 == 0x705 {
            c.power.insert(nid, verb as u8 & 0xF);
        }
        Some(0)
    }

    fn rirb_push(&self, r: &mut Regs, res: u32, ex: u32) {
        let n = crate::link::ring_size(r.rirbsize).1;
        let wp = (r.rirbwp as usize + 1) % n;
        let base = (r.rirb[1] as u64) << 32 | r.rirb[0] as u64;
        if self.dma_write32(base + wp as u64 * 8, res) && self.dma_write32(base + wp as u64 * 8 + 4, ex) {
            r.rirbwp = wp as u16;
            r.rirb_count += 1;
            if r.rirb_count >= r.rintcnt.max(1) {
                r.rirbsts |= 1;
            }
        }
    }

    fn run_corb(&self, r: &mut Regs) {
        let n = crate::link::ring_size(r.corbsize).1;
        while r.corbctl & CORBCTL_RUN != 0
            && r.rirbctl & RIRBCTL_DMA != 0
            && !self.corb_dead.load(Ordering::Acquire)
            && r.corbrp != r.corbwp
            && r.rirb_count < r.rintcnt.max(1)
        {
            let rp = (r.corbrp as usize + 1) % n;
            let base = (r.corb[1] as u64) << 32 | r.corb[0] as u64;
            let Some(b) = self.dma_read(base + rp as u64 * 4, 4) else { return };
            r.corbrp = rp as u16;
            let word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if let Some(res) = self.codec_cmd(word) {
                self.rirb_push(r, res, word >> 28);
            }
        }
    }

    /// An unsolicited response from codec `cad` (if GCTL lets them through).
    pub fn unsol(&self, cad: u8, res: u32) {
        let mut r = self.regs.lock();
        if r.gctl & GCTL_UNSOL != 0 && r.rirbctl & RIRBCTL_DMA != 0 {
            self.rirb_push(&mut r, res, RIRB_EX_UNSOL | cad as u32);
        }
    }

    /// Plays `bytes` on every running stream: BDL walk, LPIB, IOC.
    pub fn advance(&self, bytes: u32) {
        let mut r = self.regs.lock();
        let iss = (self.gcap >> 8 & 0xF) as usize;
        for (k, s) in r.sd.iter_mut().enumerate().filter(|(_, s)| s.running()) {
            let input = k < iss;
            s.at_wrap = false;
            let mut left = bytes;
            while left > 0 && s.cbl > 0 {
                let Some(e) = self.dma_read(s.bdl() + s.idx as u64 * 16, 16) else { break };
                let w = |i: usize| u32::from_le_bytes([e[i], e[i + 1], e[i + 2], e[i + 3]]);
                let (addr, len, ioc) = ((w(4) as u64) << 32 | w(0) as u64, w(8), w(12) & 1 != 0);
                if len == 0 {
                    s.ctl |= (SD_STS_DESE as u32) << 24;
                    break;
                }
                let n = left.min(len - s.off).min(s.cbl - s.lpib);
                if input {
                    let d: Vec<u8> = (0..n as usize).map(|i| s.source.get(s.src_pos + i).copied().unwrap_or(0)).collect();
                    if !self.dma_write(addr + s.off as u64, &d) {
                        break;
                    }
                    s.src_pos += n as usize;
                } else {
                    let Some(d) = self.dma_read(addr + s.off as u64, n as usize) else { break };
                    s.sink.extend_from_slice(&d);
                }
                s.off += n;
                s.lpib += n;
                left -= n;
                if s.off >= len {
                    s.off = 0;
                    s.idx = (s.idx + 1) % (s.lvi as usize + 1);
                    if ioc {
                        s.ctl |= (SD_STS_BCIS as u32) << 24;
                    }
                }
                if s.lpib >= s.cbl {
                    s.lpib = 0;
                    s.idx = 0;
                    s.off = 0;
                    s.at_wrap = left == 0 && self.amd_wrap.load(Ordering::Acquire);
                }
            }
        }
    }

    fn intsts(&self, r: &Regs) -> u32 {
        let mut v = 0;
        for (i, s) in r.sd.iter().enumerate() {
            let sts = (s.ctl >> 24) as u8;
            if sts & SD_STS_BCIS != 0 && s.ctl as u8 & SD_CTL_IOCE != 0 {
                v |= 1 << i;
            }
        }
        if r.rirbsts & 1 != 0 && r.rirbctl & RIRBCTL_IRQ != 0 {
            v |= INT_CTRL_EN;
        }
        if v != 0 {
            v |= INT_GLOBAL_EN;
        }
        v
    }

    fn reset_controller(r: &mut Regs) {
        let n = r.sd.len();
        *r = Regs { corbsize: 0x42, rirbsize: 0x42, sd: vec![Sd::default(); n], ..Regs::default() };
    }

    fn bad(&self, off: usize, width: usize, write: bool) {
        self.trace.lock().push(Ev::BadAccess { off, width, write });
    }

    fn read(&self, off: usize, width: usize) -> u64 {
        let r = self.regs.lock();
        let reg = |size: usize, v: u64| -> Option<u64> { (width <= size).then_some(v & mask(width)) };
        let v = match off {
            GCAP => reg(2, self.gcap as u64),
            VMIN => reg(1, 0),
            VMAJ => reg(1, 1),
            GCTL => reg(4, r.gctl as u64),
            STATESTS => reg(2, r.statests as u64),
            INTCTL => reg(4, r.intctl as u64),
            INTSTS => reg(4, self.intsts(&r) as u64),
            CORBLBASE => reg(4, r.corb[0] as u64),
            CORBUBASE => reg(4, r.corb[1] as u64),
            CORBWP => reg(2, r.corbwp as u64),
            CORBRP => reg(2, r.corbrp as u64),
            CORBCTL => reg(1, r.corbctl as u64),
            CORBSIZE => reg(1, r.corbsize as u64),
            RIRBLBASE => reg(4, r.rirb[0] as u64),
            RIRBUBASE => reg(4, r.rirb[1] as u64),
            RIRBWP => reg(2, r.rirbwp as u64),
            RINTCNT => reg(2, r.rintcnt as u64),
            RIRBCTL => reg(1, r.rirbctl as u64),
            RIRBSTS => reg(1, r.rirbsts as u64),
            RIRBSIZE => reg(1, r.rirbsize as u64),
            IC => reg(4, r.ic as u64),
            IR => reg(4, r.ir as u64),
            ICS => reg(2, r.ics as u64),
            DPLBASE => reg(4, r.dpl[0] as u64),
            DPUBASE => reg(4, r.dpl[1] as u64),
            o if o >= SD_BASE && (o - SD_BASE) / SD_LEN < r.sd.len() => {
                let s = &r.sd[(o - SD_BASE) / SD_LEN];
                match (o - SD_BASE) % SD_LEN {
                    SD_CTL => reg(4, (s.ctl & 0xFF_FFFF) as u64),
                    SD_STS => reg(1, (s.ctl >> 24) as u64),
                    SD_LPIB => reg(4, if s.at_wrap { s.cbl } else { s.lpib } as u64),
                    SD_CBL => reg(4, s.cbl as u64),
                    SD_LVI => reg(2, s.lvi as u64),
                    SD_FIFOS => reg(2, 0xFF),
                    SD_FORMAT => reg(2, s.fmt as u64),
                    SD_BDLPL => reg(4, s.bdpl as u64),
                    SD_BDLPU => reg(4, s.bdpu as u64),
                    _ => None,
                }
            }
            _ => None,
        };
        drop(r);
        v.unwrap_or_else(|| {
            self.bad(off, width, false);
            0
        })
    }

    fn write(&self, off: usize, width: usize, v: u64) {
        let mut r = self.regs.lock();
        let ok = |size: usize| width <= size;
        let m = mask(width);
        let merge = |old: u64| (old & !m) | (v & m);
        let stuck = self.stuck.load(Ordering::Acquire);
        let handled = match off {
            GCTL if ok(4) => {
                let old = r.gctl;
                let new = merge(old as u64) as u32 & (GCTL_CRST | GCTL_UNSOL);
                if new & GCTL_CRST == 0 {
                    FakeHda::reset_controller(&mut r);
                } else if old & GCTL_CRST == 0 {
                    let cs = self.codecs.lock();
                    r.statests = cs.iter().enumerate().filter(|c| c.1.is_some()).fold(0, |a, (i, _)| a | 1 << i);
                }
                r.gctl = new;
                true
            }
            STATESTS if ok(2) => {
                r.statests &= !(v as u16);
                true
            }
            INTCTL if ok(4) => {
                r.intctl = merge(r.intctl as u64) as u32;
                true
            }
            INTSTS if ok(4) => true,
            CORBLBASE if ok(4) => {
                r.corb[0] = v as u32;
                true
            }
            CORBUBASE if ok(4) => {
                r.corb[1] = v as u32;
                true
            }
            CORBWP if ok(2) => {
                r.corbwp = v as u16 & 0xFF;
                self.run_corb(&mut r);
                true
            }
            CORBRP if ok(2) => {
                // QEMU: the reset bit clears the read pointer and never reads back.
                if v as u16 & CORBRP_RST != 0 {
                    r.corbrp = 0;
                }
                true
            }
            CORBCTL if ok(1) => {
                let keep = if stuck { r.corbctl & CORBCTL_RUN } else { 0 };
                r.corbctl = v as u8 & 3 | keep;
                self.run_corb(&mut r);
                true
            }
            CORBSIZE if ok(1) => {
                r.corbsize = (r.corbsize & 0xF0) | (v as u8 & 3);
                true
            }
            RIRBLBASE if ok(4) => {
                r.rirb[0] = v as u32;
                true
            }
            RIRBUBASE if ok(4) => {
                r.rirb[1] = v as u32;
                true
            }
            RIRBWP if ok(2) => {
                if v as u16 & RIRBWP_RST != 0 {
                    r.rirbwp = 0;
                }
                true
            }
            RINTCNT if ok(2) => {
                r.rintcnt = v as u16;
                true
            }
            RIRBCTL if ok(1) => {
                let keep = if stuck { r.rirbctl & RIRBCTL_DMA } else { 0 };
                r.rirbctl = v as u8 & 7 | keep;
                self.run_corb(&mut r);
                true
            }
            RIRBSTS if ok(1) => {
                r.rirbsts &= !(v as u8);
                if v as u8 & 1 != 0 {
                    r.rirb_count = 0;
                    self.run_corb(&mut r);
                }
                true
            }
            RIRBSIZE if ok(1) => {
                r.rirbsize = (r.rirbsize & 0xF0) | (v as u8 & 3);
                true
            }
            IC if ok(4) => {
                r.ic = v as u32;
                true
            }
            ICS if ok(2) => {
                let v = v as u16;
                if v & ICS_VALID != 0 {
                    r.ics &= !ICS_VALID;
                }
                if v & ICS_BUSY != 0 {
                    match self.codec_cmd(r.ic) {
                        Some(res) => {
                            r.ir = res;
                            r.ics = ICS_VALID;
                        }
                        None => r.ics = 0,
                    }
                }
                true
            }
            DPLBASE if ok(4) => {
                r.dpl[0] = v as u32;
                true
            }
            DPUBASE if ok(4) => {
                r.dpl[1] = v as u32;
                true
            }
            o if o >= SD_BASE && (o - SD_BASE) / SD_LEN < r.sd.len() => {
                let s = &mut r.sd[(o - SD_BASE) / SD_LEN];
                match (o - SD_BASE) % SD_LEN {
                    SD_CTL if ok(4) => {
                        let old = s.ctl;
                        let mut low = merge((old & 0xFF_FFFF) as u64) as u32 & 0xFF_FFFF;
                        if stuck && old & SD_CTL_RUN as u32 != 0 {
                            low = (low | SD_CTL_RUN as u32) & !(SD_CTL_SRST as u32);
                        }
                        if low & SD_CTL_SRST as u32 != 0 {
                            *s = Sd { ctl: SD_CTL_SRST as u32 | 0x20 << 24, sink: core::mem::take(&mut s.sink), ..Sd::default() };
                        } else {
                            s.ctl = (old & 0xFF00_0000) | low;
                            if low & SD_CTL_RUN as u32 != 0 && old & SD_CTL_RUN as u32 == 0 {
                                s.idx = 0;
                                s.off = 0;
                            }
                        }
                        true
                    }
                    SD_STS if ok(1) => {
                        s.ctl &= !((v as u32 & SD_STS_MASK as u32) << 24);
                        true
                    }
                    SD_CBL if ok(4) => {
                        s.cbl = v as u32;
                        true
                    }
                    SD_LVI if ok(2) => {
                        s.lvi = v as u16;
                        true
                    }
                    SD_FORMAT if ok(2) => {
                        s.fmt = v as u16;
                        true
                    }
                    SD_BDLPL if ok(4) => {
                        s.bdpl = v as u32;
                        true
                    }
                    SD_BDLPU if ok(4) => {
                        s.bdpu = v as u32;
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        drop(r);
        if !handled {
            self.bad(off, width, true);
        }
    }
}

fn mask(width: usize) -> u64 {
    if width >= 8 { !0 } else { (1u64 << (width * 8)) - 1 }
}

/// The driver's view of the fake.
pub struct FakeHw(pub Arc<FakeHda>);

impl Hw for FakeHw {
    type Mem = FakeMem;
    fn rd8(&self, off: usize) -> u8 {
        self.0.read(off, 1) as u8
    }
    fn rd16(&self, off: usize) -> u16 {
        self.0.read(off, 2) as u16
    }
    fn rd32(&self, off: usize) -> u32 {
        self.0.read(off, 4) as u32
    }
    fn wr8(&self, off: usize, v: u8) {
        self.0.write(off, 1, v as u64)
    }
    fn wr16(&self, off: usize, v: u16) {
        self.0.write(off, 2, v as u64)
    }
    fn wr32(&self, off: usize, v: u32) {
        self.0.write(off, 4, v as u64)
    }
    fn now_ns(&self) -> u64 {
        self.0.clock.load(Ordering::Acquire)
    }
    fn delay_us(&self, us: u64) {
        self.0.clock.fetch_add(us.max(1) * 1000, Ordering::AcqRel);
    }
    fn alloc(&self, len: usize, _usage: DmaUse, _limit32: bool, snoop: bool) -> Result<FakeMem, (AllocError, String)> {
        if !snoop && self.0.no_uc.load(Ordering::Acquire) {
            return Err((AllocError::Unsupported, "no UC/WC (knob)".into()));
        }
        self.0.trace.lock().push(Ev::Alloc(len, snoop));
        let addr = self.0.next.fetch_add((len as u64).div_ceil(4096) * 4096 + 4096, Ordering::AcqRel);
        let buf: Buf = Arc::new(SpinLock::new(vec![0; len]));
        self.0.mem.lock().insert(addr, buf.clone());
        Ok(FakeMem { addr, buf })
    }
    fn enable(&self, dma: bool) -> Result<(), String> {
        self.0.trace.lock().push(Ev::Enable(dma));
        self.0.bus_master.store(dma, Ordering::Release);
        Ok(())
    }
    fn pci_off(&self) {
        self.0.trace.lock().push(Ev::PciOff);
        self.0.bus_master.store(false, Ordering::Release);
    }
    fn pci_modify32(&self, off: u8, clear: u32, set: u32) {
        self.0.trace.lock().push(Ev::PciModify(off, clear, set));
    }
}
