//! A small xHCI controller model for host tests: capability/operational/runtime registers with the behaviour the
//! bring-up depends on (HCRST timing, CNR, HCH, USBLEGSUP handoff, CRCR abort, ERSTBA latching), DMA memory, a
//! command ring consumer that follows Link TRBs and cycle bits, and an event ring producer over the ERST.
//! Faults on request: CNR stuck, HSE on run, a controller that never halts, commands left unanswered, a command
//! ring that does not stop, a device that is gone (all ones).

use crate::hc::{Env, MS};
use crate::mem::DmaMem;
use crate::regs::{self, cmd, crcr, legacy, sts};
use crate::trb::{cc, ty, Trb, CYCLE, LINK_TC};
use ksync::WaitQueue;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::string::String;
use std::vec::Vec;

pub const CAPLEN: usize = 0x40;
pub const RTS: usize = 0x1000;
pub const DB: usize = 0x2000;
pub const XECP: usize = 0x3000;
pub const BAR: usize = 0x4000;
/// Time every register read takes in the model, so polls with deadlines end.
const READ_NS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Legacy {
    None,
    NotOwned,
    /// BIOS-owned, lets go this long after the OS asks.
    Releases(u64),
    /// BIOS-owned and never lets go.
    Never,
}

#[derive(Clone, Debug)]
pub struct Cfg {
    pub csz64: bool,
    pub scratchpads: u32,
    pub ports: u8,
    pub slots: u8,
    pub erst_max: u8,
    pub ppc: bool,
    pub ac64: bool,
    pub legacy: Legacy,
    pub cnr_stuck: bool,
    pub hse_on_run: bool,
    pub never_halts: bool,
    /// Port 1 shows CCS as the firmware left it (US-1 tests); USB tests attach real fake devices instead.
    pub boot_ccs: bool,
}

impl Cfg {
    /// The Mini-PC's controllers: 64-byte contexts, 2 scratchpad pages, AC64.
    pub fn minipc() -> Cfg {
        Cfg { csz64: true, scratchpads: 2, ports: 7, slots: 64, erst_max: 1, ppc: true, ac64: true, legacy: Legacy::NotOwned,
            cnr_stuck: false, hse_on_run: false, never_halts: false, boot_ccs: true }
    }
    /// QEMU's qemu-xhci: 32-byte contexts, no scratchpads, ERST Max 0.
    pub fn qemu() -> Cfg {
        Cfg { csz64: false, scratchpads: 0, ports: 8, slots: 64, erst_max: 0, ppc: false, ac64: true, legacy: Legacy::None,
            cnr_stuck: false, hse_on_run: false, never_halts: false, boot_ccs: true }
    }
    /// For USB tests: no phantom device on port 1.
    pub fn bare(self) -> Cfg {
        Cfg { boot_ccs: false, ..self }
    }
}

struct Er {
    segs: Vec<(u64, u32)>,
    seg: usize,
    idx: u32,
    pcs: bool,
}

pub struct Model {
    pub cfg: Cfg,
    pub time: u64,
    mem: BTreeMap<u64, Vec<u8>>,
    next_addr: u64,
    pub usbcmd: u32,
    pub usbsts: u32,
    pub config: u32,
    pub dcbaap: u64,
    pub crcr_ptr: u64,
    pub ccs: bool,
    pub crr: bool,
    pub portsc: Vec<u32>,
    pub legsup: u32,
    pub legctl: u32,
    bios_release_at: Option<u64>,
    reset_done_at: Option<u64>,
    cnr_clear_at: Option<u64>,
    hcrst_at: Option<u64>,
    pub early_read: bool,
    pub iman: u32,
    pub imod: u32,
    pub erstsz: u32,
    pub erstba: u64,
    pub erdp: u64,
    er: Option<Er>,
    /// Commands still to be left unanswered (the next N get no completion; the ring waits on the first).
    pub drop_commands: usize,
    pub crr_stuck: bool,
    pending_cmd: Option<u64>,
    pub ca_written: bool,
    pub ped_written: bool,
    pub bus_master: bool,
    /// How often bus master was switched on.
    pub bus_master_ons: usize,
    pub gone: bool,
    /// (offset, value) of every register write, in order.
    pub writes: Vec<(usize, u32)>,
    pub lost_events: usize,
    pub commands_seen: usize,
    /// When set: every 32-bit memory write the driver makes, in order (TRB hand-over order tests).
    pub mem_log: Option<Vec<(u64, u32)>>,
    pub usb: crate::fake_usb::World,
}

impl Model {
    pub fn new(cfg: Cfg) -> Model {
        // ID 1, next capability 4 dwords further.
        let legsup = match cfg.legacy {
            Legacy::Releases(_) | Legacy::Never => legacy::BIOS_OWNED | 0x0400 | 1,
            _ => 0x0400 | 1,
        };
        let ports = cfg.ports as usize;
        let boot_ccs = cfg.boot_ccs;
        Model {
            next_addr: if cfg.ac64 { 0x1_0000_0000 } else { 0x10_0000 },
            cfg,
            time: 0,
            mem: BTreeMap::new(),
            // As OVMF leaves it: running.
            usbcmd: cmd::RS,
            usbsts: 0,
            config: 0,
            dcbaap: 0,
            crcr_ptr: 0,
            ccs: false,
            crr: false,
            portsc: (0..ports).map(|i| if i == 0 && boot_ccs { 1 | 1 << 9 } else { 1 << 9 }).collect(),
            legsup,
            legctl: 0xE001_E011,
            bios_release_at: None,
            reset_done_at: None,
            cnr_clear_at: None,
            hcrst_at: None,
            early_read: false,
            iman: 0,
            imod: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            er: None,
            drop_commands: 0,
            crr_stuck: false,
            pending_cmd: None,
            ca_written: false,
            ped_written: false,
            bus_master: false,
            bus_master_ons: 0,
            gone: false,
            writes: Vec::new(),
            lost_events: 0,
            commands_seen: 0,
            mem_log: None,
            usb: Default::default(),
        }
    }

    // --- memory

    pub fn alloc(&mut self, len: usize, align: u64, limit: u64) -> Option<u64> {
        // Like the buddy allocator: power-of-two blocks aligned to their size (never cross 64 KiB below that).
        let size = (len.max(64) as u64).next_power_of_two().max(align);
        let addr = (self.next_addr + size - 1) & !(size - 1);
        if addr + size > limit {
            return None;
        }
        self.next_addr = addr + size;
        self.mem.insert(addr, std::vec![0u8; size as usize]);
        Some(addr)
    }

    /// Transfer data memory like the kernel's `alloc_data`: page-aligned, but placed 0x3000 past a 64 KiB
    /// boundary so a transfer of more than 52 KiB must be cut there.
    pub fn alloc_data(&mut self, len: usize, limit: u64) -> Option<u64> {
        let size = (len.max(1) as u64).div_ceil(4096) * 4096;
        let mut addr = (self.next_addr + 0xFFFF) & !0xFFFF;
        addr += 0x1_0000 - 0x3000;
        if addr + size > limit {
            return None;
        }
        self.next_addr = addr + size;
        self.mem.insert(addr, std::vec![0u8; size as usize]);
        Some(addr)
    }

    fn block(&mut self, addr: u64, n: usize) -> Option<&mut [u8]> {
        let (&base, v) = self.mem.range_mut(..=addr).next_back()?;
        let off = (addr - base) as usize;
        v.get_mut(off..off.checked_add(n)?)
    }

    pub fn rd_mem32(&mut self, addr: u64) -> Option<u32> {
        self.block(addr, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn rd_mem64(&mut self, addr: u64) -> Option<u64> {
        Some(self.rd_mem32(addr)? as u64 | (self.rd_mem32(addr + 4)? as u64) << 32)
    }
    pub fn wr_mem32(&mut self, addr: u64, v: u32) -> bool {
        if let Some(l) = self.mem_log.as_mut() {
            l.push((addr, v));
        }
        match self.block(addr, 4) {
            Some(b) => {
                b.copy_from_slice(&v.to_le_bytes());
                true
            }
            None => false,
        }
    }
    pub fn read_mem(&mut self, addr: u64, out: &mut [u8]) -> bool {
        match self.block(addr, out.len()) {
            Some(b) => {
                out.copy_from_slice(b);
                true
            }
            None => false,
        }
    }
    pub fn write_mem(&mut self, addr: u64, data: &[u8]) -> bool {
        match self.block(addr, data.len()) {
            Some(b) => {
                b.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
    pub fn rd_trb(&mut self, addr: u64) -> Option<Trb> {
        Some(Trb { param: self.rd_mem64(addr)?, status: self.rd_mem32(addr + 8)?, control: self.rd_mem32(addr + 12)? })
    }

    // --- time-driven state

    fn tick(&mut self) {
        let now = self.time;
        if self.reset_done_at.is_some_and(|t| now >= t) {
            self.reset_done_at = None;
            self.usbcmd &= !cmd::HCRST;
        }
        if !self.cfg.cnr_stuck && self.cnr_clear_at.is_some_and(|t| now >= t) {
            self.cnr_clear_at = None;
            self.usbsts &= !sts::CNR;
        }
        if self.bios_release_at.is_some_and(|t| now >= t) {
            self.bios_release_at = None;
            self.legsup &= !legacy::BIOS_OWNED;
        }
        self.usb_tick();
    }

    pub(crate) fn running_pub(&self) -> bool {
        self.running()
    }

    fn running(&self) -> bool {
        self.usbcmd & cmd::RS != 0 && self.usbsts & sts::HCH == 0
    }

    // --- registers

    pub fn read(&mut self, off: usize) -> u32 {
        self.time += READ_NS;
        if self.gone || off + 4 > BAR {
            return u32::MAX;
        }
        if self.hcrst_at.is_some_and(|t| self.time < t + MS) {
            self.early_read = true;
        }
        self.tick();
        let ps = self.cfg.ports as usize;
        let op = |r: usize| CAPLEN + r;
        match off {
            0x00 => 0x0120_0000 | CAPLEN as u32,
            0x04 => (ps as u32) << 24 | 4 << 8 | self.cfg.slots as u32,
            0x08 => (self.cfg.scratchpads >> 5) << 21 | (self.cfg.scratchpads & 0x1F) << 27 | (self.cfg.erst_max as u32) << 4,
            0x10 => {
                let xecp = if self.cfg.legacy == Legacy::None { XECP + 0x10 } else { XECP };
                // Contiguous Frame ID (bit 11): isochronous Frame IDs are honoured (SN-8a).
                (xecp as u32 / 4) << 16 | 1 << 11 | (self.cfg.ppc as u32) << 3 | (self.cfg.csz64 as u32) << 2 | self.cfg.ac64 as u32
            }
            0x14 => DB as u32,
            0x18 => RTS as u32,
            o if o == op(regs::USBCMD) => self.usbcmd,
            o if o == op(regs::USBSTS) => self.usbsts,
            o if o == op(regs::PAGESIZE) => 1,
            o if o == op(regs::CRCR) => (self.crr as u32) << 3,
            o if o == op(regs::CRCR) + 4 => 0,
            o if o == op(regs::CONFIG) => self.config,
            o if o >= op(regs::PORTSC_BASE) && o < op(regs::PORTSC_BASE) + 16 * ps && (o - op(regs::PORTSC_BASE)) % 16 == 0 => {
                self.portsc[(o - op(regs::PORTSC_BASE)) / 16]
            }
            // MFINDEX: microframes (125 us) of the model clock, 14 bits.
            o if o == RTS => ((self.time / 125_000) & 0x3FFF) as u32,
            o if o == RTS + 0x20 => self.iman,
            o if o == RTS + 0x24 => self.imod,
            o if o == RTS + 0x28 => self.erstsz,
            // Extended capabilities: legacy (0x3000, next 0x3010), USB 2 ports (0x3010, next 0x3030), USB 3 (0x3030).
            o if o == XECP => self.legsup,
            o if o == XECP + 4 => self.legctl,
            o if o == XECP + 0x10 => 0x0200_0802,
            o if o == XECP + 0x14 || o == XECP + 0x34 => regs::PROTOCOL_NAME_USB,
            o if o == XECP + 0x18 => ((ps / 2) as u32) << 8 | 1,
            o if o == XECP + 0x1C => 3,
            o if o == XECP + 0x30 => 0x0300_0002,
            o if o == XECP + 0x38 => ((ps - ps / 2) as u32) << 8 | (ps / 2 + 1) as u32,
            o if o == XECP + 0x3C => 4,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: usize, v: u32) {
        self.writes.push((off, v));
        if self.gone {
            return;
        }
        self.tick();
        let ps = self.cfg.ports as usize;
        let op = |r: usize| CAPLEN + r;
        match off {
            o if o == op(regs::USBCMD) => self.write_usbcmd(v),
            o if o == op(regs::USBSTS) => self.usbsts &= !(v & (sts::HSE | sts::EINT | sts::PCD)),
            o if o == op(regs::CRCR) => {
                if v & crcr::CA != 0 && self.crr {
                    self.ca_written = true;
                    if !self.crr_stuck {
                        if let Some(a) = self.pending_cmd.take() {
                            self.post(Trb { param: a, status: (cc::COMMAND_ABORTED as u32) << 24, control: (ty::COMMAND_COMPLETION as u32) << 10 });
                        }
                        let p = self.crcr_ptr;
                        self.post(Trb { param: p, status: (cc::COMMAND_RING_STOPPED as u32) << 24, control: (ty::COMMAND_COMPLETION as u32) << 10 });
                        self.crr = false;
                    }
                } else if !self.crr {
                    self.crcr_ptr = (self.crcr_ptr & !0xFFFF_FFFF) | (v & !0x3F) as u64;
                    self.ccs = v & crcr::RCS != 0;
                }
            }
            o if o == op(regs::CRCR) + 4 => {
                if !self.crr {
                    self.crcr_ptr = (self.crcr_ptr & 0xFFFF_FFFF) | (v as u64) << 32;
                }
            }
            o if o == op(regs::DCBAAP) => self.dcbaap = (self.dcbaap & !0xFFFF_FFFF) | v as u64,
            o if o == op(regs::DCBAAP) + 4 => self.dcbaap = (self.dcbaap & 0xFFFF_FFFF) | (v as u64) << 32,
            o if o == op(regs::CONFIG) => self.config = v,
            o if o >= op(regs::PORTSC_BASE) && o < op(regs::PORTSC_BASE) + 16 * ps && (o - op(regs::PORTSC_BASE)) % 16 == 0 => {
                self.write_portsc((o - op(regs::PORTSC_BASE)) / 16, v);
            }
            o if o == RTS + 0x20 => self.iman = (self.iman & !regs::iman::IE & !(v & regs::iman::IP)) | (v & regs::iman::IE),
            o if o == RTS + 0x24 => self.imod = v,
            o if o == RTS + 0x28 => self.erstsz = v,
            o if o == RTS + 0x30 => self.erstba = (self.erstba & !0xFFFF_FFFF) | v as u64,
            // ERSTBA latches on the high dword (QEMU does the same): the controller reads the table now.
            o if o == RTS + 0x34 => {
                self.erstba = (self.erstba & 0xFFFF_FFFF) | (v as u64) << 32;
                self.latch_erst();
            }
            o if o == RTS + 0x38 => self.erdp = (self.erdp & !0xFFFF_FFFF) | (v & !0xF) as u64,
            o if o == RTS + 0x3C => {
                self.erdp = (self.erdp & 0xFFFF_FFFF) | (v as u64) << 32;
                // Software made room: endpoints that waited for it go on (`run_ep`).
                if self.usb.waiting_for_events {
                    self.usb.waiting_for_events = false;
                    self.pump();
                }
            }
            o if o == DB => self.doorbell0(),
            o if o > DB && o <= DB + 4 * self.cfg.slots as usize && o % 4 == 0 => self.doorbell_slot(((o - DB) / 4) as u8, v as u8),
            o if o == XECP => {
                if v & legacy::OS_OWNED != 0
                    && self.legsup & legacy::OS_OWNED == 0
                    && let Legacy::Releases(after) = self.cfg.legacy
                {
                    self.bios_release_at = Some(self.time + after);
                }
                self.legsup = (self.legsup & !(legacy::OS_OWNED | legacy::BIOS_OWNED)) | (v & (legacy::OS_OWNED | legacy::BIOS_OWNED));
            }
            o if o == XECP + 4 => self.legctl = (v & 0x1FFF_FFFF) | (self.legctl & !(v & 0xE000_0000) & 0xE000_0000),
            _ => {}
        }
    }

    fn write_usbcmd(&mut self, v: u32) {
        if v & cmd::HCRST != 0 {
            self.usbcmd = cmd::HCRST;
            self.usbsts = sts::HCH | sts::CNR;
            self.hcrst_at = Some(self.time);
            self.reset_done_at = Some(self.time + 2 * MS);
            self.cnr_clear_at = Some(self.time + 3 * MS);
            self.config = 0;
            self.dcbaap = 0;
            self.crcr_ptr = 0;
            self.crr = false;
            self.iman = 0;
            self.imod = 4000;
            self.erstsz = 0;
            self.erstba = 0;
            self.erdp = 0;
            self.er = None;
            self.pending_cmd = None;
            self.reset_ports();
            return;
        }
        self.usbcmd = v & (cmd::RS | cmd::INTE | cmd::HSEE);
        if v & cmd::RS != 0 {
            self.usbsts &= !sts::HCH;
            if self.cfg.hse_on_run {
                self.usbsts |= sts::HSE;
            }
        } else if !self.cfg.never_halts {
            self.usbsts |= sts::HCH;
            self.crr = false;
        }
    }

    fn latch_erst(&mut self) {
        let n = self.erstsz & 0xFFFF;
        if n == 0 || n > 1 << self.cfg.erst_max {
            // Like QEMU's xhci_die for an ERSTSZ it cannot take.
            self.usbsts |= sts::HSE;
            return;
        }
        let mut segs = Vec::new();
        for i in 0..n as u64 {
            let (Some(base), Some(size)) = (self.rd_mem64(self.erstba + i * 16), self.rd_mem32(self.erstba + i * 16 + 8)) else {
                self.usbsts |= sts::HSE;
                return;
            };
            segs.push((base, size));
        }
        self.er = Some(Er { segs, seg: 0, idx: 0, pcs: true });
    }

    /// Free places in the event ring (the controller waits for software when it runs out, `run_ep`).
    pub fn event_room(&self) -> usize {
        let Some(er) = self.er.as_ref() else { return 0 };
        let total: u64 = er.segs.iter().map(|s| s.1 as u64).sum();
        let mut pos = 0u64;
        let mut deq = None;
        for (base, size) in &er.segs {
            if self.erdp >= *base && self.erdp < base + *size as u64 * 16 {
                deq = Some(pos + (self.erdp - base) / 16);
            }
            pos += *size as u64;
        }
        let enq: u64 = er.segs[..er.seg].iter().map(|s| s.1 as u64).sum::<u64>() + er.idx as u64;
        let deq = deq.unwrap_or(enq);
        (total - (enq + total - deq) % total - 1) as usize
    }

    /// Writes one event into the event ring (unless it is full) and raises EINT/IP.
    pub fn post(&mut self, mut t: Trb) {
        let Some(er) = self.er.as_ref() else {
            self.lost_events += 1;
            return;
        };
        let (base, size) = er.segs[er.seg];
        let at = base + er.idx as u64 * 16;
        let (mut nseg, mut nidx) = (er.seg, er.idx + 1);
        if nidx == size {
            nidx = 0;
            nseg = (nseg + 1) % er.segs.len();
        }
        let next = er.segs[nseg].0 + nidx as u64 * 16;
        if next == self.erdp {
            self.lost_events += 1;
            return;
        }
        let pcs = er.pcs;
        t.control = (t.control & !CYCLE) | pcs as u32;
        let ok = self.wr_mem32(at, t.param as u32)
            && self.wr_mem32(at + 4, (t.param >> 32) as u32)
            && self.wr_mem32(at + 8, t.status)
            && self.wr_mem32(at + 12, t.control);
        assert!(ok, "event ring segment outside the model's memory");
        let er = self.er.as_mut().unwrap();
        if nseg == 0 && nidx == 0 {
            er.pcs = !er.pcs;
        }
        er.seg = nseg;
        er.idx = nidx;
        self.usbsts |= sts::EINT;
        self.iman |= regs::iman::IP;
    }

    pub fn port_event(&mut self, port: u8) {
        self.post(Trb { param: (port as u64) << 24, status: (cc::SUCCESS as u32) << 24, control: (ty::PORT_STATUS_CHANGE as u32) << 10 });
    }

    fn doorbell0(&mut self) {
        if !self.running() || self.pending_cmd.is_some() {
            return;
        }
        self.crr = true;
        // Bounded by the ring memory the driver gave: a loop of links without commands ends here.
        for _ in 0..100_000 {
            let addr = self.crcr_ptr;
            let Some(t) = self.rd_trb(addr) else {
                self.usbsts |= sts::HSE;
                return;
            };
            if t.cycle() != self.ccs {
                return;
            }
            if t.trb_type() == ty::LINK {
                self.crcr_ptr = t.param;
                if t.control & LINK_TC != 0 {
                    self.ccs = !self.ccs;
                }
                continue;
            }
            self.commands_seen += 1;
            if self.drop_commands > 0 {
                self.drop_commands -= 1;
                self.pending_cmd = Some(addr);
                return;
            }
            self.crcr_ptr += 16;
            let (code, slot) = self.exec_command(&t);
            self.post(Trb { param: addr, status: (code as u32) << 24, control: (ty::COMMAND_COMPLETION as u32) << 10 | (slot as u32) << 24 });
            self.pump();
        }
    }
}

/// Runs once, at the next `relax` (a poll step), outside the model's lock: lets a test act in the middle of a
/// bring-up or a command, e.g. a Quiesce during the BIOS handoff wait.
pub type Hook = std::boxed::Box<dyn FnOnce() + Send>;

/// The test `Env`: the model behind a mutex (the `Env` bounds want Send + Sync).
#[derive(Clone)]
pub struct FakeEnv {
    pub m: Arc<Mutex<Model>>,
    pub on_relax: Arc<Mutex<Option<Hook>>>,
    /// Like `on_relax`, at the next `wait` (another context acting while the driver sleeps).
    pub on_wait: Arc<Mutex<Option<Hook>>>,
}

impl FakeEnv {
    pub fn new(cfg: Cfg) -> FakeEnv {
        FakeEnv { m: Arc::new(Mutex::new(Model::new(cfg))), on_relax: Arc::new(Mutex::new(None)), on_wait: Arc::new(Mutex::new(None)) }
    }
    pub fn model(&self) -> MutexGuard<'_, Model> {
        self.m.lock().unwrap()
    }
}

pub struct FakeMem {
    m: Arc<Mutex<Model>>,
    addr: u64,
    len: usize,
}

impl FakeMem {
    pub fn new(env: &FakeEnv, len: usize) -> FakeMem {
        let addr = env.model().alloc(len, 64, u64::MAX).unwrap();
        FakeMem { m: env.m.clone(), addr, len }
    }
}

impl DmaMem for FakeMem {
    fn dev_addr(&self) -> u64 {
        self.addr
    }
    fn size(&self) -> usize {
        self.len
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        if !off.is_multiple_of(4) || off.checked_add(4)? > self.len {
            return None;
        }
        self.m.lock().unwrap().rd_mem32(self.addr + off as u64)
    }
    fn wr32(&self, off: usize, v: u32) -> bool {
        if !off.is_multiple_of(4) || off.checked_add(4).is_none_or(|e| e > self.len) {
            return false;
        }
        self.m.lock().unwrap().wr_mem32(self.addr + off as u64, v)
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        off.is_multiple_of(8) && self.wr32(off, v as u32) && self.wr32(off + 4, (v >> 32) as u32)
    }
    fn read_bytes(&self, off: usize, out: &mut [u8]) -> bool {
        if off.checked_add(out.len()).is_none_or(|e| e > self.len) {
            return false;
        }
        self.m.lock().unwrap().read_mem(self.addr + off as u64, out)
    }
    fn write_bytes(&self, off: usize, data: &[u8]) -> bool {
        if off.checked_add(data.len()).is_none_or(|e| e > self.len) {
            return false;
        }
        self.m.lock().unwrap().write_mem(self.addr + off as u64, data)
    }
}

impl Env for FakeEnv {
    type Mem = FakeMem;
    fn read32(&self, off: usize) -> u32 {
        self.model().read(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.model().write(off, v)
    }
    fn bar_len(&self) -> usize {
        BAR
    }
    fn alloc(&self, len: usize, align: u64, limit: u64) -> Option<FakeMem> {
        let addr = self.model().alloc(len, align, limit)?;
        Some(FakeMem { m: self.m.clone(), addr, len })
    }
    fn alloc_data(&self, len: usize, limit: u64) -> Option<FakeMem> {
        let addr = self.model().alloc_data(len, limit)?;
        Some(FakeMem { m: self.m.clone(), addr, len })
    }
    fn now_ns(&self) -> u64 {
        self.model().time
    }
    fn delay_ns(&self, ns: u64) {
        self.model().time += ns;
    }
    fn sleep_ns(&self, ns: u64) {
        self.model().time += ns;
    }
    fn relax(&self, _may_sleep: bool) {
        self.model().time += 1000;
        let hook = self.on_relax.lock().unwrap().take();
        if let Some(h) = hook {
            h();
        }
    }
    fn wait(&self, _wq: &WaitQueue, cond: &mut dyn FnMut() -> bool, timeout_ns: u64) {
        let hook = self.on_wait.lock().unwrap().take();
        if let Some(h) = hook {
            h();
        }
        if !cond() {
            self.model().time += timeout_ns.min(MS);
        }
    }
    fn bus_master(&self, on: bool) -> Result<(), String> {
        let mut m = self.model();
        m.bus_master = on;
        m.bus_master_ons += on as usize;
        Ok(())
    }
}
