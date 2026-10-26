//! Host tests only: a register-level fake NVMe controller. Doorbell writes run the queued commands at once and
//! post their completions with the phase tag; DMA goes through the `VecMem` buffers the driver allocated (found
//! by device address, as a bus would). Every opcode placed in a queue is recorded, so the tests can assert
//! that no media-writing command ever reached the device. Injections: lost completions, an error status for an
//! LBA, a controller that never gets ready, a fatal status, a gone device, no shutdown completion.

use crate::hw::{DmaMem, Hw, IrqMode, VecMem};
use crate::regs;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ksync::SpinLock;

#[derive(Clone)]
pub struct FakeNs {
    pub nsid: u32,
    pub shift: u8,
    pub ms: u16,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
struct Sq {
    addr: u64,
    depth: u16,
    head: u16,
    cqid: u16,
}
#[derive(Clone, Copy, Default)]
struct Cq {
    addr: u64,
    depth: u16,
    tail: u16,
    phase: bool,
}

#[derive(Default)]
pub struct Inject {
    /// Completions of this many next I/O commands are never posted.
    pub drop_io: u32,
    /// Reads touching this LBA fail with Unrecovered Read Error (media, SCT 2 / SC 81h).
    pub bad_lba: Option<u64>,
    pub never_ready: bool,
    pub fatal: bool,
    pub gone: bool,
    pub no_shutdown: bool,
    /// Identify Namespace answers garbage (format index outside the list, 255 formats).
    pub hostile_ns: bool,
    /// The next this many I/O completions carry a CID no command has.
    pub wrong_cid: u32,
    /// The next this many I/O completions report an SQ head beyond the ring.
    pub bad_sq_head: u32,
    /// The next this many I/O completions are posted twice.
    pub dup_io: u32,
}

/// Where a one-shot test hook runs (outside the fake's lock, so it may call into the controller).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookAt {
    /// Inside `enable(dma)`, after the enable took effect.
    Enable(bool),
    /// After an I/O submission doorbell was rung.
    IoDoorbell,
}

pub type Hook = alloc::boxed::Box<dyn FnOnce() + Send>;

pub struct St {
    pub cap: u64,
    pub vs: u32,
    pub cc: u32,
    pub csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    pub now: u64,
    pub mdts: u8,
    pub vwc: u8,
    pub serial: String,
    pub ns: Vec<FakeNs>,
    sqs: Vec<(u16, Sq)>,
    cqs: Vec<(u16, Cq)>,
    pub seen_admin: Vec<u8>,
    pub seen_io: Vec<u8>,
    pub inject: Inject,
    pub mems: Vec<VecMem>,
    pub next_addr: u64,
    pub irq: IrqMode,
    pub enables: u32,
    pub shutdowns: u32,
    pub pci_offs: u32,
    /// Each `enable(dma)` with whether CC.EN was on at that moment.
    pub enable_calls: Vec<(bool, bool)>,
    pub fail_alloc_above: Option<usize>,
    /// Bus master as `enable(true)` / `pci_off` left it.
    pub bus_master: bool,
    /// BAR0 size.
    pub bar: usize,
    pub hook: Option<(HookAt, Hook)>,
}

#[derive(Clone)]
pub struct FakeHw {
    pub st: Arc<SpinLock<St>>,
}

/// Pattern byte of namespace `nsid` at byte offset `off`.
pub fn pattern(nsid: u32, off: u64) -> u8 {
    crate::test_pattern(nsid as u8, off)
}

pub fn ns(nsid: u32, shift: u8, blocks: u64) -> FakeNs {
    let len = (blocks << shift) as usize;
    FakeNs { nsid, shift, ms: 0, data: (0..len as u64).map(|o| pattern(nsid, o)).collect() }
}

impl FakeHw {
    /// Runs `f` once at `at` (see `HookAt`).
    pub fn hook(&self, at: HookAt, f: impl FnOnce() + Send + 'static) {
        self.st.lock().hook = Some((at, alloc::boxed::Box::new(f)));
    }
    fn take_hook(st: &mut St, at: HookAt) -> Option<Hook> {
        if st.hook.as_ref().is_some_and(|h| h.0 == at) { st.hook.take().map(|h| h.1) } else { None }
    }
    pub fn new(namespaces: Vec<FakeNs>) -> FakeHw {
        // MQES 1023, CQR, TO 20 (10 s), DSTRD 0, CSS NVM (bit 37), MPSMIN 0, MPSMAX 4.
        let cap = 0x3FF | (1 << 16) | (20 << 24) | (1u64 << 37) | (4u64 << 52);
        FakeHw {
            st: Arc::new(SpinLock::new(St {
                cap,
                vs: 0x0001_0400,
                cc: 0,
                csts: 0,
                aqa: 0,
                asq: 0,
                acq: 0,
                now: 1_000_000,
                mdts: 5,
                vwc: 1,
                serial: String::from("FAKE0001"),
                ns: namespaces,
                sqs: Vec::new(),
                cqs: Vec::new(),
                seen_admin: Vec::new(),
                seen_io: Vec::new(),
                inject: Inject::default(),
                mems: Vec::new(),
                next_addr: 0x1_0000_0000,
                irq: IrqMode::MsiX(2),
                enables: 0,
                shutdowns: 0,
                pci_offs: 0,
                enable_calls: Vec::new(),
                fail_alloc_above: None,
                bus_master: false,
                bar: 0x4000,
                hook: None,
            })),
        }
    }
}

fn find(st: &St, addr: u64) -> Option<(VecMem, usize)> {
    st.mems.iter().find(|m| addr >= m.addr && addr < m.addr + m.size() as u64).map(|m| (m.clone(), (addr - m.addr) as usize))
}
fn bus_write(st: &St, addr: u64, data: &[u8]) -> bool {
    find(st, addr).is_some_and(|(m, o)| m.copy_in(o, data))
}
fn bus_read(st: &St, addr: u64, out: &mut [u8]) -> bool {
    find(st, addr).is_some_and(|(m, o)| m.copy_out(o, out))
}
fn bus_u64(st: &St, addr: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    bus_read(st, addr, &mut b).then(|| u64::from_le_bytes(b))
}

/// The pages of a PRP pair for `len` bytes (NVMe 1.4 §4.3).
fn prp_pages(st: &St, prp1: u64, prp2: u64, len: usize) -> Option<Vec<(u64, usize)>> {
    let page = regs::PAGE as u64;
    let first = (page - (prp1 % page)).min(len as u64) as usize;
    let mut out = vec![(prp1, first)];
    let mut left = len - first;
    if left == 0 {
        return Some(out);
    }
    if left <= regs::PAGE {
        out.push((prp2, left));
        return Some(out);
    }
    let mut i = 0u64;
    while left > 0 {
        let a = bus_u64(st, prp2 + 8 * i)?;
        let n = left.min(regs::PAGE);
        out.push((a, n));
        left -= n;
        i += 1;
    }
    Some(out)
}

impl St {
    fn reset_queues(&mut self) {
        self.sqs.clear();
        self.cqs.clear();
        let d = (self.aqa & 0xFFF) as u16 + 1;
        self.sqs.push((0, Sq { addr: self.asq, depth: d, head: 0, cqid: 0 }));
        let cd = ((self.aqa >> 16) & 0xFFF) as u16 + 1;
        self.cqs.push((0, Cq { addr: self.acq, depth: cd, tail: 0, phase: true }));
    }

    fn post(&mut self, cqid: u16, sqid: u16, sq_head: u16, cid: u16, status: u16, dw0: u32) {
        let Some(i) = self.cqs.iter().position(|c| c.0 == cqid) else { return };
        let mut cq = self.cqs[i].1;
        let mut e = [0u8; 16];
        e[0..4].copy_from_slice(&dw0.to_le_bytes());
        e[8..10].copy_from_slice(&sq_head.to_le_bytes());
        e[10..12].copy_from_slice(&sqid.to_le_bytes());
        let d3 = cid as u32 | (cq.phase as u32) << 16 | (status as u32) << 17;
        e[12..16].copy_from_slice(&d3.to_le_bytes());
        bus_write(self, cq.addr + cq.tail as u64 * 16, &e);
        cq.tail += 1;
        if cq.tail == cq.depth {
            cq.tail = 0;
            cq.phase = !cq.phase;
        }
        self.cqs[i].1 = cq;
    }

    fn ring(&mut self, qid: u16, tail: u16) {
        if self.csts & regs::CSTS_RDY == 0 {
            return;
        }
        let Some(i) = self.sqs.iter().position(|s| s.0 == qid) else { return };
        let mut sq = self.sqs[i].1;
        while sq.head != tail % sq.depth.max(1) {
            let mut e = [0u8; 64];
            bus_read(self, sq.addr + sq.head as u64 * 64, &mut e);
            sq.head = (sq.head + 1) % sq.depth;
            self.sqs[i].1.head = sq.head;
            let dw = |k: usize| u32::from_le_bytes([e[4 * k], e[4 * k + 1], e[4 * k + 2], e[4 * k + 3]]);
            let op = e[0];
            let cid = u16::from_le_bytes([e[2], e[3]]);
            let prp1 = dw(6) as u64 | (dw(7) as u64) << 32;
            let prp2 = dw(8) as u64 | (dw(9) as u64) << 32;
            let (status, dw0, post) = if qid == 0 {
                self.seen_admin.push(op);
                let (s, d) = self.admin(op, dw(1), prp1, [dw(10), dw(11)]);
                (s, d, true)
            } else {
                self.seen_io.push(op);
                let (s, d) = self.io(op, dw(1), prp1, prp2, [dw(10), dw(11), dw(12)]);
                let post = if self.inject.drop_io > 0 {
                    self.inject.drop_io -= 1;
                    false
                } else {
                    true
                };
                (s, d, post)
            };
            if post {
                let mut cid = cid;
                let mut head = sq.head;
                if qid != 0 && self.inject.wrong_cid > 0 {
                    self.inject.wrong_cid -= 1;
                    cid ^= 0x5A5A;
                }
                if qid != 0 && self.inject.bad_sq_head > 0 {
                    self.inject.bad_sq_head -= 1;
                    head = 0xFFFF;
                }
                self.post(sq.cqid, qid, head, cid, status, dw0);
                if qid != 0 && self.inject.dup_io > 0 {
                    self.inject.dup_io -= 1;
                    self.post(sq.cqid, qid, head, cid, status, dw0);
                }
            }
        }
    }

    fn admin(&mut self, op: u8, nsid: u32, prp1: u64, cdw: [u32; 2]) -> (u16, u32) {
        match op {
            0x06 => {
                let mut p = vec![0u8; 4096];
                match cdw[0] & 0xFF {
                    1 => {
                        p[0] = 0x36;
                        p[1] = 0x1B;
                        let sn = self.serial.as_bytes();
                        p[4..4 + sn.len().min(20)].copy_from_slice(&sn[..sn.len().min(20)]);
                        p[24..34].copy_from_slice(b"Fake NVMe ");
                        p[64..68].copy_from_slice(b"1.0 ");
                        p[77] = self.mdts;
                        p[80..84].copy_from_slice(&self.vs.to_le_bytes());
                        let nn = self.ns.iter().map(|n| n.nsid).max().unwrap_or(0);
                        p[516..520].copy_from_slice(&nn.to_le_bytes());
                        p[525] = self.vwc;
                    }
                    0 => {
                        if let Some(n) = self.ns.iter().find(|n| n.nsid == nsid) {
                            let blocks = (n.data.len() >> n.shift) as u64;
                            p[0..8].copy_from_slice(&blocks.to_le_bytes());
                            p[8..16].copy_from_slice(&blocks.to_le_bytes());
                            if self.inject.hostile_ns {
                                p[25] = 255;
                                p[26] = 0x6F;
                            } else {
                                p[25] = 1;
                                p[26] = 1;
                                p[128 + 2] = 9;
                                p[132..134].copy_from_slice(&n.ms.to_le_bytes());
                                p[132 + 2] = n.shift;
                            }
                        }
                    }
                    2 => {
                        let mut ids: Vec<u32> = self.ns.iter().map(|n| n.nsid).filter(|&i| i > nsid).collect();
                        ids.sort();
                        for (k, id) in ids.iter().take(1024).enumerate() {
                            p[4 * k..4 * k + 4].copy_from_slice(&id.to_le_bytes());
                        }
                    }
                    _ => return (0x02, 0),
                }
                bus_write(self, prp1, &p);
                (0, 0)
            }
            0x09 if cdw[0] & 0xFF == 7 => (0, cdw[1]),
            0x0A if cdw[0] & 0xFF == 6 => (0, self.vwc as u32 & 1),
            0x05 => {
                let qid = cdw[0] as u16;
                let depth = (cdw[0] >> 16) as u16 + 1;
                self.cqs.retain(|c| c.0 != qid);
                self.cqs.push((qid, Cq { addr: prp1, depth, tail: 0, phase: true }));
                (0, 0)
            }
            0x01 => {
                let qid = cdw[0] as u16;
                let depth = (cdw[0] >> 16) as u16 + 1;
                let cqid = (cdw[1] >> 16) as u16;
                if !self.cqs.iter().any(|c| c.0 == cqid) {
                    // Command specific: completion queue invalid.
                    return (0x100, 0);
                }
                self.sqs.retain(|s| s.0 != qid);
                self.sqs.push((qid, Sq { addr: prp1, depth, head: 0, cqid }));
                (0, 0)
            }
            0x02 => {
                let numd = ((cdw[0] >> 16) | (cdw[1] << 16)) as usize + 1;
                let mut p = vec![0u8; (numd * 4).min(4096)];
                if cdw[0] & 0xFF == 2 && p.len() >= 512 {
                    p[1..3].copy_from_slice(&310u16.to_le_bytes());
                    p[3] = 100;
                    p[4] = 10;
                    p[112] = 7;
                    p[128] = 42;
                    p[144] = 3;
                }
                bus_write(self, prp1, &p);
                (0, 0)
            }
            0x00 | 0x04 => (0, 0),
            _ => (0x01, 0),
        }
    }

    fn io(&mut self, op: u8, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 3]) -> (u16, u32) {
        let Some(n) = self.ns.iter().find(|n| n.nsid == nsid).cloned() else { return (0x0B, 0) };
        match op {
            0x00 => (0, 0),
            0x02 => {
                let slba = cdw[0] as u64 | (cdw[1] as u64) << 32;
                let nlb = (cdw[2] & 0xFFFF) as u64 + 1;
                let blocks = (n.data.len() >> n.shift) as u64;
                if slba.checked_add(nlb).is_none_or(|e| e > blocks) {
                    return (0x80, 0);
                }
                if self.inject.bad_lba.is_some_and(|b| b >= slba && b < slba + nlb) {
                    return (0x281, 0);
                }
                let len = (nlb << n.shift) as usize;
                let Some(pages) = prp_pages(self, prp1, prp2, len) else { return (0x04, 0) };
                let mut at = (slba << n.shift) as usize;
                for (a, l) in pages {
                    if !bus_write(self, a, &n.data[at..at + l]) {
                        return (0x04, 0);
                    }
                    at += l;
                }
                (0, 0)
            }
            _ => (0x01, 0),
        }
    }
}

impl Hw for FakeHw {
    type Mem = VecMem;
    fn rd32(&self, off: usize) -> u32 {
        let st = self.st.lock();
        if st.inject.gone {
            return !0;
        }
        match off {
            regs::CAP => st.cap as u32,
            0x04 => (st.cap >> 32) as u32,
            regs::VS => st.vs,
            regs::CC => st.cc,
            regs::CSTS => st.csts | if st.inject.fatal { regs::CSTS_CFS } else { 0 },
            _ => 0,
        }
    }
    fn wr32(&self, off: usize, v: u32) {
        let mut st = self.st.lock();
        if st.inject.gone {
            return;
        }
        match off {
            regs::AQA => st.aqa = v,
            regs::ASQ => st.asq = (st.asq & !0xFFFF_FFFF) | v as u64,
            0x2C => st.asq = (st.asq & 0xFFFF_FFFF) | (v as u64) << 32,
            regs::ACQ => st.acq = (st.acq & !0xFFFF_FFFF) | v as u64,
            0x34 => st.acq = (st.acq & 0xFFFF_FFFF) | (v as u64) << 32,
            regs::CC => {
                let was = st.cc;
                st.cc = v;
                if was & regs::CC_EN == 0 && v & regs::CC_EN != 0 {
                    st.enables += 1;
                    if !st.inject.never_ready {
                        st.csts = regs::CSTS_RDY;
                        st.reset_queues();
                    }
                } else if was & regs::CC_EN != 0 && v & regs::CC_EN == 0 {
                    st.csts = 0;
                    st.sqs.clear();
                    st.cqs.clear();
                }
                if v & regs::CC_SHN_MASK != 0 && v & regs::CC_EN != 0 {
                    st.shutdowns += 1;
                    if !st.inject.no_shutdown {
                        st.csts |= regs::CSTS_SHST_CMPLT;
                    }
                }
            }
            o if o >= regs::DOORBELLS => {
                if o + 4 > st.bar {
                    return;
                }
                let idx = (o - regs::DOORBELLS) / (4usize << ((st.cap >> 32) & 0xF));
                if idx.is_multiple_of(2) {
                    st.ring((idx / 2) as u16, v as u16);
                    if idx >= 2 {
                        let h = Self::take_hook(&mut st, HookAt::IoDoorbell);
                        drop(st);
                        if let Some(f) = h {
                            f();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    fn window(&self) -> usize {
        self.st.lock().bar
    }
    fn now_ns(&self) -> u64 {
        let mut st = self.st.lock();
        st.now += 1000;
        st.now
    }
    fn pause_us(&self, us: u64, _may_sleep: bool) {
        self.st.lock().now += us * 1000;
    }
    fn relax(&self) {}
    fn alloc_dma(&self, len: usize) -> Result<VecMem, String> {
        let mut st = self.st.lock();
        if st.fail_alloc_above.is_some_and(|m| len > m) {
            return Err(String::from("fake: no memory"));
        }
        let addr = st.next_addr;
        st.next_addr += (len as u64).next_multiple_of(4096) + 4096;
        let m = VecMem { addr, bytes: Arc::new(SpinLock::new(vec![0xEE; len])) };
        st.mems.push(m.clone());
        Ok(m)
    }
    fn enable(&self, dma: bool) -> Result<(), String> {
        let mut st = self.st.lock();
        let en = st.cc & regs::CC_EN != 0;
        st.enable_calls.push((dma, en));
        if dma {
            st.bus_master = true;
        }
        let h = Self::take_hook(&mut st, HookAt::Enable(dma));
        drop(st);
        if let Some(f) = h {
            f();
        }
        Ok(())
    }
    fn pci_off(&self) {
        let mut st = self.st.lock();
        st.pci_offs += 1;
        st.bus_master = false;
    }
    fn setup_irqs(&self, _max: u16) -> IrqMode {
        self.st.lock().irq
    }
    fn free_irqs(&self) {}
    fn wait_irq(&self, _i: u16, timeout_ns: u64) -> bool {
        let mut st = self.st.lock();
        if st.irq == IrqMode::Polled {
            return false;
        }
        st.now += timeout_ns;
        true
    }
    fn irq_count(&self, _i: u16) -> u64 {
        0
    }
}
