//! GP-GPU2: the per-stage write allow list, below the bring-up owner (`gpucore::bringup`, which logs every write).
//! A write reaches the card only when (1) `rdna4_core::refuse::write_refused_at` has nothing against its register and
//! value (flash, SR-IOV, Mode1 reset, the SMU's debug mailbox, any SMU message but a reviewed question: refused by
//! construction, whatever a stage lists; the bootloader's two registers: only what the card's `bootseq::Gate` says
//! is Linux's next step, and nothing without a gate), (2) the stage lists its register, and (3) for memory, it lies
//! inside the stage's VRAM ranges (MC addresses). Everything else is refused before the card, kept, and klogged.
//! Doorbells are refused unless the stage lists them (wave 43; no stage of wave 42 lists any). Reads pass: they change nothing on this card; the gate is
//! shown the reads of C2PMSG_81 and C2PMSG_35, which are Linux's conditions for a bootloader command.
//!
//! Wave 42: the index pair reaches only the addresses its own stage reviewed (`nb::index_rules`: `nbif` its two
//! registers, `smu` and `pmfw` the firmware flags, every other stage none; nothing is put back into the index,
//! as Linux leaves it on its last address), its data register is written only with Linux's change
//! of the value read there, and the host side (`host::Host`: AtomicOp requester enable, bus master, the MSI
//! vector, system pages) changes only for a stage that lists it.
//!
//! Wave 43: the SMU's mailbox takes more than questions only for the stage that starts the power controller,
//! whose guard holds `rdna4_core::smugate::Gate`: it is shown every read and write of the mailbox and of the
//! thermal alert's registers, and the driver table as it is read out of VRAM, and lets a write through only as
//! the next bus access of Linux's start.
//! Wave 43: a doorbell passes only at a dword the stage lists (`with_doorbells`: SDMA's two for the engine's
//! stages, the IH's two for the stage that reads the IH rings itself), and a stage that names its own system
//! memory (`with_sys`) writes system pages only there and in what it takes itself while it runs.
//!
//! On the card the PCI side of `nbif` and `ih` is done by `hostprep::prepare` BEFORE the stage runs (PCI's locks
//! lie outside the session's): there that chain is the gate, and this guard only hands its answers on to the
//! stage that lists them (plan/spec/gpu-rdna4.md section 6, "The host side").

use super::host::Host;
use super::regs14::Regs14;
use crate::gpucore::psp::Hw;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::bootseq::Gate;
use rdna4_core::refuse;
use rdna4_core::smugate;

/// What one stage may write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allow {
    pub stage: &'static str,
    /// BAR5 bytes.
    pub regs: Vec<u32>,
    /// MC ranges.
    pub mem: Vec<Range<u64>>,
}

/// A change on the host side of the card (`host::Host`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostAsk {
    /// System pages the card will be told about.
    Pages,
    BusMaster,
    Msi,
    /// AtomicOp requester enable in the card's own Device Control 2 (`pci_enable_atomic_ops_to_root`).
    Atomics,
}

/// One address a stage may aim the index pair at, and Linux's change of the register there (`None`: the stage
/// only reads it). The data register is written only with `change` of what the stage read at that address.
#[derive(Clone, Copy, Debug)]
pub struct IndexRule {
    pub target: u32,
    pub change: Option<fn(u32) -> u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Reg { byte: u32, value: u32, why: &'static str },
    Mem { mc: u64, bytes: usize },
    Doorbell { index_dw: u32 },
    Host { ask: HostAsk },
    /// A write of system memory outside what the stage named as its own.
    Sys { at: u64, bytes: usize },
}

pub struct Guard<H> {
    pub hw: H,
    pub allow: Allow,
    regs: Regs14,
    pub refused: Vec<Refusal>,
    /// The only addresses this stage may aim the index pair at.
    index: Option<Vec<IndexRule>>,
    /// Where the pair is aimed (the last index write that reached the card), and what the data register read
    /// there: (address, value).
    aimed: Option<u32>,
    data_read: Option<(u32, u32)>,
    /// What of the host side the stage may change (nothing unless `with_host` lists it).
    host: Vec<HostAsk>,
    /// The card's bootloader state in this boot (the session's, lent to the stage that loads the secure OS).
    pub boot: Option<Gate>,
    /// Linux's start of the power controller as the bus showed it so far (the stage `pmfw`'s). On the heap: a
    /// guard lies on the stack of a stage call.
    pub smu: Option<alloc::boxed::Box<smugate::Gate>>,
    /// The doorbells (dword indices of BAR2) this stage may ring: none unless `with_doorbells` lists them.
    doorbells: Vec<u32>,
    /// The system memory this stage may write, once it named any (`with_sys`): those ranges and what it takes
    /// through this guard while it runs. `None`: wave 42's rule, any address for a stage that lists pages.
    sys: Option<Vec<Range<u64>>>,
}

impl<H: Hw> Guard<H> {
    pub fn new(hw: H, allow: Allow, regs: Regs14) -> Guard<H> {
        Guard { hw, allow, regs, refused: Vec::new(), index: None, aimed: None, data_read: None, host: Vec::new(), boot: None, smu: None, doorbells: Vec::new(), sys: None }
    }

    pub fn with_smu(mut self, smu: Option<alloc::boxed::Box<smugate::Gate>>) -> Guard<H> {
        self.smu = smu;
        self
    }

    pub fn with_gate(mut self, boot: Option<Gate>) -> Guard<H> {
        self.boot = boot;
        self
    }

    /// The doorbells this stage may ring, as dword indices of BAR2.
    pub fn with_doorbells(mut self, doorbells: Vec<u32>) -> Guard<H> {
        self.doorbells = doorbells;
        self
    }

    /// From now on the stage writes system memory only inside `own` and inside what it takes itself.
    pub fn with_sys(mut self, own: Vec<Range<u64>>) -> Guard<H> {
        self.sys = Some(own);
        self
    }

    fn doorbell_ok(&mut self, index_dw: u32) -> bool {
        if self.doorbells.contains(&index_dw) {
            return true;
        }
        klog::event!(Error, "gx", "rdna4.guard.doorbell", stage = self.allow.stage, index = klog::Hex(u64::from(index_dw)); "a doorbell refused: not on the stage's list");
        self.refused.push(Refusal::Doorbell { index_dw });
        false
    }

    /// The stage's list for the host side.
    pub fn with_host(mut self, asks: Vec<HostAsk>) -> Guard<H> {
        self.host = asks;
        self
    }

    /// From now on the index pair reaches only the addresses of `rules`, each as its rule says.
    pub fn with_index_rules(mut self, rules: Vec<IndexRule>) -> Guard<H> {
        self.index = Some(rules);
        self
    }

    /// A write of the pair's data register: Linux's change of what the stage read at the address it aimed at.
    fn data_verdict(&self, rules: &[IndexRule], value: u32) -> Option<&'static str> {
        let Some(change) = self.aimed.and_then(|a| rules.iter().find(|r| r.target == a)).and_then(|r| r.change) else {
            return Some("the index pair's data register is written only at an address where Linux changes a register");
        };
        match self.data_read {
            Some((at, read)) if Some(at) == self.aimed && change(read) == value => None,
            _ => Some("not Linux's change of the register behind the index pair (the value read there with Linux's field alone changed)"),
        }
    }

    /// Why this write would be refused, or `None`.
    pub fn verdict(&self, byte: u32, value: u32) -> Option<&'static str> {
        if let Some(why) = self.regs.reg_at(byte).and_then(|r| refuse::write_refused_with(r, value, self.boot.as_ref(), self.smu.as_deref())) {
            return Some(why);
        }
        if byte == self.regs.rsmu_index && self.index.as_ref().is_some_and(|t| !t.iter().any(|r| r.target == value)) {
            return Some("the index pair is aimed at an address this stage did not review");
        }
        if byte == self.regs.rsmu_data
            && let Some(rules) = &self.index
            && let Some(why) = self.data_verdict(rules, value)
        {
            return Some(why);
        }
        (!self.allow.regs.contains(&byte)).then_some("not on this stage's list")
    }

    fn host_ok(&mut self, ask: HostAsk) -> bool {
        if self.host.contains(&ask) {
            return true;
        }
        klog::event!(Error, "gx", "rdna4.guard.host", stage = self.allow.stage, ask = alloc::format!("{ask:?}").as_str(); "a host-side change refused: not on the stage's list");
        self.refused.push(Refusal::Host { ask });
        false
    }

    fn mem_ok(&self, mc: u64, bytes: usize) -> bool {
        let Some(end) = mc.checked_add(bytes as u64) else { return false };
        self.allow.mem.iter().any(|r| mc >= r.start && end <= r.end)
    }
}

impl<H: Hw> Hw for Guard<H> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        let v = self.hw.reg_read(byte);
        if let (Some(g), Some(v), Some(reg)) = (self.smu.as_mut(), v, self.regs.reg_at(byte)) {
            g.saw_read(reg, v);
        }
        if byte == self.regs.rsmu_data
            && let (Some(v), Some(at)) = (v, self.aimed)
        {
            self.data_read = Some((at, v));
        }
        if let (Some(g), Some(v)) = (self.boot.as_mut(), v) {
            if byte == self.regs.psp.c2pmsg_81 {
                g.saw_sign_of_life(v);
            } else if byte == self.regs.psp.c2pmsg_35 {
                g.saw_bootloader(v);
            }
        }
        v
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        if let Some(why) = self.verdict(byte, value) {
            klog::event!(Error, "gx", "rdna4.guard", stage = self.allow.stage, reg = self.regs.name(byte), byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)), why = why; "write refused before the card");
            self.refused.push(Refusal::Reg { byte, value, why });
            return false;
        }
        let reached = self.hw.reg_write(byte, value);
        if reached && byte == self.regs.rsmu_index {
            self.aimed = Some(value);
        }
        if reached && let (Some(g), Some(reg)) = (self.smu.as_mut(), self.regs.reg_at(byte)) {
            g.wrote(reg, value);
        }
        if reached && let (Some(g), Some(to)) = (self.boot.as_mut(), self.regs.reg_at(byte).and_then(refuse::bootloader_mailbox)) {
            g.wrote(to);
        }
        reached
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        let read = self.hw.mem_read(mc, buf);
        if read && let Some(g) = self.smu.as_mut() {
            g.saw_memory(mc, buf);
        }
        read
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        if !self.mem_ok(mc, data.len()) {
            klog::event!(Error, "gx", "rdna4.guard.mem", stage = self.allow.stage, mc = klog::Hex(mc), bytes = data.len(); "memory write outside the stage's ranges refused");
            self.refused.push(Refusal::Mem { mc, bytes: data.len() });
            return false;
        }
        self.hw.mem_write(mc, data)
    }
    /// Handed on (Linux's `mb()`, amdgpu_gart.c:499): the GART's table writes must have left the CPU before the
    /// TLB flush's registers are written. `mem_write64` stays the trait's default, which goes through `mem_write`
    /// and its list: no stage stores a single entry yet (plan/spec/gpu-rdna4.md section 6).
    fn store_barrier(&mut self) {
        self.hw.store_barrier()
    }
    fn now_us(&mut self) -> u64 {
        self.hw.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.hw.delay_us(us)
    }
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        self.doorbell_ok(index_dw) && self.hw.doorbell_write64(index_dw, value)
    }
    fn doorbell_write32(&mut self, index_dw: u32, value: u32) -> bool {
        self.doorbell_ok(index_dw) && self.hw.doorbell_write32(index_dw, value)
    }
}

impl<H: Hw + Host> Host for Guard<H> {
    fn dma_survey(&mut self) -> Result<(), String> {
        self.hw.dma_survey()
    }
    fn bus_master(&mut self) -> Option<bool> {
        self.hw.bus_master()
    }
    fn bridges_forward(&mut self) -> Result<(), String> {
        self.hw.bridges_forward()
    }
    fn atomics(&mut self) -> Result<String, String> {
        if self.host_ok(HostAsk::Atomics) { self.hw.atomics() } else { Err(String::from("not on this stage's list")) }
    }
    fn enable_dma(&mut self) -> Result<String, String> {
        if self.host_ok(HostAsk::BusMaster) { self.hw.enable_dma() } else { Err(String::from("not on this stage's list")) }
    }
    fn msi(&mut self) -> Result<String, String> {
        if self.host_ok(HostAsk::Msi) { self.hw.msi() } else { Err(String::from("not on this stage's list")) }
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        let at = if self.host_ok(HostAsk::Pages) { self.hw.pages(bytes) } else { None }?;
        if let (Some(own), Some(end)) = (self.sys.as_mut(), at.checked_add(bytes)) {
            own.push(at..end);
        }
        Some(at)
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.hw.sys_read(at, buf)
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        let Some(own) = &self.sys else { return self.host.contains(&HostAsk::Pages) && self.hw.sys_write(at, data) };
        if !at.checked_add(data.len() as u64).is_some_and(|end| own.iter().any(|r| at >= r.start && end <= r.end)) {
            klog::event!(Error, "gx", "rdna4.guard.sys", stage = self.allow.stage, at = klog::Hex(at), bytes = data.len(); "a write of system memory outside the stage's own refused");
            self.refused.push(Refusal::Sys { at, bytes: data.len() });
            return false;
        }
        self.hw.sys_write(at, data)
    }
}

/// MC addresses to VRAM offsets for a card's `Hw` (the attach's fake and glue read VRAM by offset): outside VRAM
/// nothing passes.
pub struct Mc<H> {
    pub hw: H,
    pub base: u64,
    pub bytes: u64,
}

impl<H: Hw> Mc<H> {
    fn off(&self, mc: u64, len: usize) -> Option<u64> {
        let o = mc.checked_sub(self.base)?;
        (o.checked_add(len as u64)? <= self.bytes).then_some(o)
    }
}

impl<H: Hw> Hw for Mc<H> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.hw.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.hw.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.off(mc, buf.len()).is_some_and(|o| self.hw.mem_read(o, buf))
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.off(mc, data.len()).is_some_and(|o| self.hw.mem_write(o, data))
    }
    fn store_barrier(&mut self) {
        self.hw.store_barrier()
    }
    fn now_us(&mut self) -> u64 {
        self.hw.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.hw.delay_us(us)
    }
    /// Wave 43: the guard above decides which doorbells pass; they are no memory, so nothing is translated.
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        self.hw.doorbell_write64(index_dw, value)
    }
    fn doorbell_write32(&mut self, index_dw: u32, value: u32) -> bool {
        self.hw.doorbell_write32(index_dw, value)
    }
}

/// The HDP flush Linux does after writing a frame (amdgpu_psp.c:3961, amdgpu_hdp.c:51-60): 0 into the flush
/// register, then a read of MEMSIZE. Above the bring-up owner, so the write is logged and goes through the guard.
/// HDP 7 has no invalidate (soc24 and hdp_v7_0 give none, hdp_v7_0.c:127-132): `invalidate_hdp` stays a no-op.
pub struct HdpFlush<'a> {
    pub hw: &'a mut dyn Hw,
    pub flush: u32,
    pub memsize: u32,
}

impl Hw for HdpFlush<'_> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.hw.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.hw.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.hw.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.hw.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.hw.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.hw.delay_us(us)
    }
    fn flush_hdp(&mut self) {
        if self.hw.reg_write(self.flush, 0) {
            let _ = self.hw.reg_read(self.memsize);
        }
    }
    fn wait_begin(&mut self, byte: u32, cond: crate::gpucore::psp::Cond, bound: crate::gpucore::psp::Bound) {
        self.hw.wait_begin(byte, cond, bound)
    }
    fn wait_end(&mut self) {
        self.hw.wait_end()
    }
}
