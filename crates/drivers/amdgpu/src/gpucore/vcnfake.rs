//! GX-8v: a fake VCN 4.0.2 inside the chip fake (`pspfake::FakeGpu::vcn`): what Linux 7.3.1's vcn_v4_0.c expects
//! of the block and the 27.10. capture showed. Gated until PMFW powered it (PowerUpVcn: every register reads all
//! ones, writes are lost); UVD_POWER_STATUS reads 1 after the power-up, VCN_RB_ENABLE 0x1ff, UVD_STATUS 0xdeadbeef in
//! DPG mode. The VCPU boots only from what the driver gave it: VCN's firmware through the PSP (type 13), the SRAM
//! list (type 49) with the VCPU clock, the master interrupt, the context window and the shared page window, a shared
//! page with the unified-queue flag, PG_MODE, the ring's registers and RB1 on. Then a doorbell at dword 0x312 (with
//! NBIO's VCN range) or UVD_RB_WPTR runs the ring: END, NO_OP, IB (a unified IB: the checksum checked, encode
//! sessions opened and closed, decode messages read), FENCE (32 bits, VMID 0), TRAP (the IH entry), REG_WRITE and
//! REG_WAIT on MMHUB's registers. Every memory access goes through the hub fake's MMHUB (VMID 0: the system aperture;
//! 1-15: the VMID's walk). `VcnHostile` makes it misbehave in one way each.

use super::sdmafake::FakeBus;
use super::vcn::VcnAddrs;
use crate::ipmod::vcn4::{self, R, f};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::bus::GpuBus;
use amdgpu_fmt::vcn::{self as pkt, Packet, shared};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VcnHostile {
    /// The PSP answers the SRAM list with a non-zero status.
    SramRefused,
    /// Fences are never written (the IBs run).
    NoFence,
}

/// One IB the VCPU ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IbSeen {
    pub vmid: u8,
    pub addr: u64,
    pub engine: u32,
    pub checksum_ok: bool,
}

pub struct VcnFake {
    pub a: Option<VcnAddrs>,
    pub hostile: Vec<VcnHostile>,
    regs: BTreeMap<u32, u32>,
    /// VCN's firmware came through the PSP (LOAD_IP_FW type 13).
    pub fw_loaded: bool,
    /// The SRAM list as the PSP took it: (internal offset, value).
    pub sram: Option<Vec<(u32, u32)>>,
    /// The pointers as the ring's registers hold them (dwords, masked).
    pub rptr: u32,
    pub wptr: u32,
    pub doorbells: Vec<u32>,
    /// Writes and reads that reached a gated block (lost).
    pub off_access: Vec<u32>,
    /// What the VCPU ran.
    pub ends: u32,
    pub ibs: Vec<IbSeen>,
    pub fences: Vec<(u64, u32)>,
    pub traps: u32,
    pub ring_writes: Vec<(u32, u32)>,
    pub enc_sessions: Vec<u32>,
    pub dec_sessions: Vec<u32>,
    pub decodes: u32,
    /// What the VCPU refused (a bad checksum, a session it does not have, a fault): it stops there.
    pub errors: Vec<String>,
    /// The packets the VCPU took, for the ring test count.
    pub kicks: u32,
    /// Host-physical places the VCPU wrote (the fences).
    pub gpu_writes: Vec<(u64, usize)>,
    was_on: bool,
}

impl VcnFake {
    pub fn new(d: &super::disc::Discovery) -> VcnFake {
        VcnFake {
            a: VcnAddrs::from_discovery(d),
            hostile: Vec::new(),
            regs: BTreeMap::new(),
            fw_loaded: false,
            sram: None,
            rptr: 0,
            wptr: 0,
            doorbells: Vec::new(),
            off_access: Vec::new(),
            ends: 0,
            ibs: Vec::new(),
            fences: Vec::new(),
            traps: 0,
            ring_writes: Vec::new(),
            enc_sessions: Vec::new(),
            dec_sessions: Vec::new(),
            decodes: 0,
            errors: Vec::new(),
            kicks: 0,
            gpu_writes: Vec::new(),
            was_on: false,
        }
    }

    fn r_at(&self, byte: u32) -> Option<R> {
        let a = self.a?;
        R::ALL.into_iter().find(|&r| a.byte(r) == Some(byte))
    }

    /// Whether `byte` is one of VCN's registers.
    pub fn owns(&self, byte: u32) -> bool {
        self.r_at(byte).is_some()
    }

    pub fn get(&self, r: R) -> u32 {
        let cold = match r {
            R::PowerStatus => 1,
            R::RbEnable => 0x1ff,
            _ => 0,
        };
        self.a.and_then(|a| a.byte(r)).and_then(|b| self.regs.get(&b).copied()).unwrap_or(cold)
    }

    /// A register read; `on`: PMFW powered VCN.
    pub fn read(&mut self, byte: u32, on: bool) -> u32 {
        let Some(r) = self.r_at(byte) else { return 0 };
        if !on {
            self.off_access.push(byte);
            return u32::MAX;
        }
        match r {
            R::Status if self.get(R::PowerStatus) & f::PG_MODE != 0 => vcn4::DPG_STATUS_READ,
            R::RbRptr => self.rptr,
            R::RbWptr => self.wptr,
            _ => self.get(r),
        }
    }

    /// A register write; true: the ring has something new (the chip fake runs it).
    pub fn write(&mut self, byte: u32, value: u32, on: bool) -> bool {
        let Some(r) = self.r_at(byte) else { return false };
        if !on {
            self.off_access.push(byte);
            return false;
        }
        self.regs.insert(byte, value);
        match r {
            R::RbRptr => {
                self.rptr = value;
                false
            }
            R::RbWptr => {
                self.wptr = value;
                true
            }
            _ => false,
        }
    }

    /// PMFW's power state as the chip fake sees it each step: a power-down loses the block's state (a new start
    /// needs the whole sequence again; VCN's firmware stays in the TMR).
    pub fn power(&mut self, on: bool) {
        if !on && (self.was_on || !self.regs.is_empty()) {
            self.regs.clear();
            self.sram = None;
            self.rptr = 0;
            self.wptr = 0;
            self.enc_sessions.clear();
            self.dec_sessions.clear();
        }
        self.was_on = on;
    }

    /// The doorbell at `vcn4::DOORBELL_DW`; `routed`: NBIO's VCN range names it and the aperture is on.
    pub fn doorbell(&mut self, value: u32, routed: bool) -> bool {
        self.doorbells.push(value);
        if !routed || self.get(R::Rb1DbCtrl) != vcn4::rb1_db_ctrl(vcn4::DOORBELL_DW) {
            return false;
        }
        self.wptr = value;
        true
    }

    /// The PSP's LOAD_IP_FW of type 49: the status it answers.
    pub fn load_sram(&mut self, bytes: &[u8]) -> u32 {
        if self.hostile.contains(&VcnHostile::SramRefused) {
            return super::pspfake::ST_BAD_FW;
        }
        self.sram = Some(pkt::Sram::pairs(bytes));
        0
    }

    /// The SRAM list's last value for `r` (the VCPU's view of the register).
    fn sram_value(&self, r: R) -> Option<u32> {
        let off = pkt::dpg_offset(self.a?.byte(r)?);
        self.sram.as_ref()?.iter().rev().find(|p| p.0 == off).map(|p| p.1)
    }

    fn window(&self, lo: R, hi: R) -> Option<u64> {
        Some(u64::from(self.sram_value(lo)?) | u64::from(self.sram_value(hi)?) << 32)
    }

    /// Whether the VCPU runs: everything the start must give it is there.
    pub fn booted(&self, bus: &mut FakeBus<'_>) -> bool {
        if !self.fw_loaded {
            return false;
        }
        let vcpu = self.sram_value(R::VcpuCntl).is_some_and(|v| v & f::VCPU_CLK_EN != 0 && v & f::VCPU_BLK_RST == 0);
        let int = self.sram_value(R::MastintEn) == Some(f::MASTINT_VCPU_EN);
        let ctx = self.window(R::Cache2BarLo, R::Cache2BarHi).is_some_and(|c| c != 0 && self.sram_value(R::CacheSize2) == Some(pkt::CONTEXT_BYTES as u32));
        let Some(nc) = self.window(R::Nc0BarLo, R::Nc0BarHi) else { return false };
        let mut page = [0u8; shared::BYTES];
        let shared_ok = bus.read(0, nc, &mut page).is_ok() && u32::from_le_bytes([page[0], page[1], page[2], page[3]]) & shared::FLAG_UNIFIED_QUEUE != 0 && page[shared::SQ_IS_ENABLED] == 1;
        let pg = self.get(R::PowerStatus) & f::PG_MODE != 0;
        let ring = self.get(R::RbEnable) & f::RB1_EN != 0 && self.get(R::RbSize) as u64 == vcn4::RING_DW;
        vcpu && int && ctx && shared_ok && pg && ring
    }

    /// The VCPU runs its ring from the read pointer to the write pointer. `trap`: the IH entry of a TRAP.
    pub fn kick(&mut self, bus: &mut FakeBus<'_>, trap: &mut dyn FnMut()) {
        if !self.booted(bus) {
            return;
        }
        let base = u64::from(self.get(R::RbBaseLo)) | u64::from(self.get(R::RbBaseHi)) << 32;
        let size = self.get(R::RbSize);
        let mask = size - 1;
        let mut words = vec![0u32; size as usize];
        let mut raw = vec![0u8; size as usize * 4];
        if bus.fetch(0, base, &mut raw).is_err() {
            self.errors.push(String::from("ring fetch faulted"));
            return;
        }
        for (i, w) in words.iter_mut().enumerate() {
            *w = u32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]]);
        }
        let wptr = self.wptr & mask;
        let mut guard = 0;
        while self.rptr & mask != wptr && guard < size {
            guard += 1;
            let at = (self.rptr & mask) as usize;
            // A packet never straddles the end here: frames are 64-dword aligned and short.
            let Some((p, n)) = Packet::decode(&words[at..]) else {
                self.errors.push(String::from("truncated packet at the ring's end"));
                return;
            };
            if !self.packet(bus, p, trap) {
                return;
            }
            self.rptr = (self.rptr + n as u32) & mask;
        }
        self.kicks += 1;
    }

    /// One packet; false: the VCPU stops here (its read pointer stays on it).
    fn packet(&mut self, bus: &mut FakeBus<'_>, p: Packet, trap: &mut dyn FnMut()) -> bool {
        match p {
            Packet::Nop => true,
            Packet::End => {
                self.ends += 1;
                true
            }
            Packet::Trap => {
                self.traps += 1;
                trap();
                true
            }
            Packet::Fence { addr, seq } => {
                if !self.hostile.contains(&VcnHostile::NoFence) && bus.write(0, addr, &seq.to_le_bytes()).is_err() {
                    self.errors.push(alloc::format!("fence write at {addr:#x} faulted"));
                    return false;
                }
                self.fences.push((addr, seq));
                true
            }
            Packet::RegWrite { reg_dw, value } => {
                self.ring_writes.push((reg_dw, value));
                bus.reg_write(reg_dw, value)
            }
            Packet::RegWait { reg_dw, mask, value } => {
                let ok = bus.reg_read(reg_dw).is_some_and(|v| v & mask == value);
                if !ok {
                    self.errors.push(alloc::format!("REG_WAIT on {reg_dw:#x} never met"));
                }
                ok
            }
            Packet::Ib { vmid, addr, len_dw } => self.ib(bus, vmid, addr, len_dw),
            Packet::Unknown(x) => {
                self.errors.push(alloc::format!("unknown opcode {x:#x}"));
                false
            }
        }
    }

    fn ib(&mut self, bus: &mut FakeBus<'_>, vmid: u8, addr: u64, len_dw: u32) -> bool {
        let mut raw = vec![0u8; len_dw as usize * 4];
        if bus.fetch(vmid, addr, &mut raw).is_err() {
            self.errors.push(alloc::format!("IB fetch at {addr:#x} (VMID {vmid}) faulted"));
            return false;
        }
        let words: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let Some(u) = pkt::parse_unified(&words) else {
            self.errors.push(String::from("not a unified IB"));
            return false;
        };
        self.ibs.push(IbSeen { vmid, addr, engine: u.engine, checksum_ok: u.checksum_ok });
        if !u.checksum_ok {
            self.errors.push(String::from("unified IB checksum wrong"));
            return false;
        }
        match u.engine {
            pkt::ENGINE_ENCODE => self.enc(&u.body),
            pkt::ENGINE_DECODE => self.dec(bus, vmid, &u),
            e => {
                self.errors.push(alloc::format!("engine {e}"));
                false
            }
        }
    }

    /// The encoder's session info and op (amdgpu_vcn.c:953-961).
    fn enc(&mut self, b: &[u32]) -> bool {
        let (Some(&handle), Some(&op)) = (b.get(2), b.get(12)) else { return false };
        match op {
            pkt::ENC_OP_INIT => self.enc_sessions.push(handle),
            pkt::ENC_OP_CLOSE => match self.enc_sessions.iter().position(|&h| h == handle) {
                Some(i) => {
                    self.enc_sessions.remove(i);
                }
                None => {
                    self.errors.push(alloc::format!("close of encode session {handle} never opened"));
                    return false;
                }
            },
            _ => {}
        }
        true
    }

    /// A decode message: create (0) opens a session, decode (1) needs one, destroy (2) closes it.
    fn dec(&mut self, bus: &mut FakeBus<'_>, vmid: u8, u: &pkt::Unified) -> bool {
        let Some(at) = pkt::dec_msg_addr(u) else {
            self.errors.push(String::from("decode IB without a message"));
            return false;
        };
        let mut raw = [0u8; 24];
        if bus.read(vmid, at, &mut raw).is_err() {
            self.errors.push(alloc::format!("decode message at {at:#x} (VMID {vmid}) faulted"));
            return false;
        }
        let w = |i: usize| u32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]]);
        let (ty, handle) = (w(3), w(4));
        let open = self.dec_sessions.iter().position(|&h| h == handle);
        match (ty, open) {
            (0, _) => self.dec_sessions.push(handle),
            (1, Some(_)) => self.decodes += 1,
            (2, Some(i)) => {
                self.dec_sessions.remove(i);
            }
            _ => {
                self.errors.push(alloc::format!("decode message type {ty} for session {handle}"));
                return false;
            }
        }
        true
    }
}

/// MMHUB's client id for VCN's accesses in the fault status (the fake records it; the chip's value is not needed).
pub const CID_VCN: u32 = 0x26;
