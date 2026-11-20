//! CRASHDISK (owner RB): the panic record's write to the boot ESP with the driver's own state left alone. The
//! panicking CPU may have stopped the others inside the driver (its locks held, its queues half-rung), so this
//! path resets the controller and builds one admin and one I/O queue pair in memory set aside at boot, polled (the
//! I/O CQ without interrupts), every wait bounded: Linux 7.3.1 has no NVMe panic write (pstore/blk takes a
//! driver's `panic_write`, fs/pstore/zone.c `psz_zone_write`; mtd's is mtd_panic_write); its reset and queue
//! set-up follow drivers/nvme/host/pci.c `nvme_disable_ctrl`/`nvme_pci_configure_admin_queue`/
//! `nvme_create_queue` as the driver's bring-up does.
//!
//! What may be written is fixed at boot: the controller's boot-partition window (the ESP `nvme_boot=` names, open
//! only while /boot there is writable) becomes a `WriteWindow` of that range alone, and the window's generation is
//! kept; a later close or change of it refuses every write (`ready`). Every command is built through that window
//! (`IoCmd::write`), and every run is checked before the reset, so a bad run writes nothing at all.

use crate::cmd::{AdminCmd, Cqe, Encode, IoCmd, WriteWindow};
use crate::ctrl::{Controller, State};
use crate::hw::{DmaMem, Hw};
use crate::queue::QueuePair;
use crate::{prp, regs};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use kexec::crash::disk::{Fail, Run};

/// Entries of each panic queue (one command at a time; two would do).
const DEPTH: u16 = 4;
/// The longest the reset may wait for RDY, whatever CAP.TO says (the panic's own reset follows).
const READY_MAX_NS: u64 = 10_000_000_000;

/// Borrowed DMA memory as a queue's (the queue pairs live on the panic path's stack, the memory in the writer).
struct Lent<'a, M: DmaMem>(&'a M);

impl<M: DmaMem> DmaMem for Lent<'_, M> {
    fn dev_addr(&self) -> u64 {
        self.0.dev_addr()
    }
    fn size(&self) -> usize {
        self.0.size()
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        self.0.rd32(off)
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        self.0.wr64(off, v)
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        self.0.copy_in(off, src)
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        self.0.copy_out(off, dst)
    }
    fn zero(&self) -> bool {
        self.0.zero()
    }
}

pub struct PanicWriter<H: Hw> {
    ctrl: Arc<Controller<H>>,
    name: String,
    nsid: u32,
    bs: usize,
    /// The boot ESP's range alone.
    window: WriteWindow,
    /// The controller's boot-window generation when this was made.
    window_gen: u64,
    asq: H::Mem,
    acq: H::Mem,
    iosq: H::Mem,
    iocq: H::Mem,
    list: H::Mem,
    data: H::Mem,
    /// Bytes one command moves at most (MDTS, one PRP list page), whole blocks.
    xfer: usize,
    busy: AtomicBool,
}

fn fail(stage: &'static str, status: u32) -> Fail {
    Fail { stage, status }
}

impl<H: Hw> PanicWriter<H> {
    /// At boot, while /boot on the ESP is writable: memory for the queues and a `bytes` buffer. Err: no open boot
    /// window on namespace `nsid`, an unusable namespace, `bytes` not whole blocks, no DMA memory.
    pub fn new(ctrl: Arc<Controller<H>>, nsid: u32, bytes: usize) -> Result<PanicWriter<H>, String> {
        let window_gen = ctrl.boot_gen();
        let w = ctrl.window().filter(|w| w.nsid() == nsid).ok_or("no write window on this namespace")?;
        let (first, blocks) = w.boot().ok_or("the boot partition's window is not open (nvme_boot= does not let /boot be written)")?;
        let info = ctrl.info();
        let ns = info.namespaces.iter().find(|n| n.nsid == nsid && n.unusable.is_none()).ok_or("the namespace is not usable")?;
        let bs = ns.block_size() as usize;
        if bytes == 0 || !bytes.is_multiple_of(bs) || bs > regs::PAGE {
            return Err(format!("{bytes} bytes are not whole blocks of {bs}"));
        }
        let xfer = (info.max_transfer.min(prp::max_bytes() - regs::PAGE).max(regs::PAGE) / bs) * bs;
        let dma = |len: usize| ctrl.hw.alloc_dma(len.max(regs::PAGE).next_multiple_of(regs::PAGE));
        let w = PanicWriter {
            name: format!("{}n{nsid}", ctrl.name),
            nsid,
            bs,
            window: WriteWindow::new(nsid, first, blocks),
            window_gen,
            asq: dma(regs::PAGE)?,
            acq: dma(regs::PAGE)?,
            iosq: dma(regs::PAGE)?,
            iocq: dma(regs::PAGE)?,
            list: dma(regs::PAGE)?,
            data: dma(bytes)?,
            xfer,
            busy: AtomicBool::new(false),
            ctrl,
        };
        Ok(w)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn capacity(&self) -> usize {
        self.data.size()
    }
    pub fn block_size(&self) -> usize {
        self.bs
    }
    pub fn allowed(&self) -> (u64, u64) {
        self.window.range()
    }
    pub fn put(&self, off: usize, b: &[u8]) -> bool {
        self.data.copy_in(off, b)
    }
    pub fn get(&self, off: usize, b: &mut [u8]) -> bool {
        self.data.copy_out(off, b)
    }

    /// Lock-free: the controller is there and its boot window is the one this writer was made for.
    pub fn ready(&self) -> Result<(), &'static str> {
        if self.ctrl.state() == State::Removed {
            return Err("the controller was removed");
        }
        if !self.ctrl.boot_open() || self.ctrl.boot_gen() != self.window_gen {
            return Err("the boot partition's window closed or changed since boot");
        }
        Ok(())
    }

    /// The commands of `runs`, each at most `xfer` bytes: (lba, buffer offset, bytes). Pure check of every one.
    fn each(&self, runs: &[Run], f: &mut dyn FnMut(u64, usize, usize) -> Result<(), Fail>) -> Result<(), Fail> {
        for r in runs {
            let total = r.blocks as usize * self.bs;
            let mut done = 0;
            while done < total {
                let n = (total - done).min(self.xfer);
                f(r.lba + (done / self.bs) as u64, r.off + done, n)?;
                done += n;
            }
        }
        Ok(())
    }

    fn check(&self, lba: u64, off: usize, n: usize) -> Result<(), Fail> {
        let inside = off.checked_add(n).is_some_and(|e| e <= self.data.size()) && off.is_multiple_of(4);
        if !inside || IoCmd::write(&self.window, self.nsid, lba, (n / self.bs) as u32, 0, 0, true).is_none() {
            return Err(fail("outside the boot partition or the buffer", 0));
        }
        Ok(())
    }

    fn rd64(&self, off: usize) -> u64 {
        self.ctrl.hw.rd32(off) as u64 | (self.ctrl.hw.rd32(off + 4) as u64) << 32
    }
    fn wr64(&self, off: usize, v: u64) {
        self.ctrl.hw.wr32(off, v as u32);
        self.ctrl.hw.wr32(off + 4, (v >> 32) as u32);
    }

    fn wait_csts(&self, want_rdy: bool, end: u64, clock: &dyn Fn() -> u64) -> Result<(), Fail> {
        loop {
            let c = self.ctrl.hw.rd32(regs::CSTS);
            if c == u32::MAX {
                return Err(fail("the controller answers all ones (gone)", 0));
            }
            if want_rdy && c & regs::CSTS_CFS != 0 {
                return Err(fail("controller fatal status after the enable", 0));
            }
            if (c & regs::CSTS_RDY != 0) == want_rdy {
                return Ok(());
            }
            if clock() > end {
                return Err(fail(if want_rdy { "not ready after the enable" } else { "not stopped after the disable" }, 0));
            }
            core::hint::spin_loop();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run<C: Encode>(&self, q: &mut QueuePair<Lent<'_, H::Mem>, C>, cmd: &C, stage: &'static str, dbs: (usize, usize), end: u64, clock: &dyn Fn() -> u64) -> Result<Cqe, Fail> {
        let (cid, tail, _) = q.push(cmd).map_err(|_| fail(stage, 0))?;
        self.ctrl.hw.wr32(dbs.0, tail as u32);
        loop {
            match q.pop() {
                Ok(Some((c, head))) => {
                    self.ctrl.hw.wr32(dbs.1, head as u32);
                    if c.cid == cid {
                        return if c.ok() { Ok(c) } else { Err(fail(stage, (c.sct() as u32) << 8 | c.sc() as u32)) };
                    }
                }
                Ok(None) => {}
                Err(_) => return Err(fail(stage, 0)),
            }
            if clock() > end {
                return Err(fail(stage, 0));
            }
            core::hint::spin_loop();
        }
    }

    /// The panic write (see the module doc). No lock, no heap, no sleep.
    pub fn write(&self, runs: &[Run], clock: &dyn Fn() -> u64, bound_ns: u64) -> Result<(), Fail> {
        self.ready().map_err(|s| fail(s, 0))?;
        self.each(runs, &mut |lba, off, n| self.check(lba, off, n))?;
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(fail("a panic write runs already", 0));
        }
        let end = clock().saturating_add(bound_ns);
        let hw = &self.ctrl.hw;
        let cap = regs::Cap { raw: self.rd64(regs::CAP) };
        let stride = cap.stride();
        if regs::doorbell(1, true, stride) + 4 > hw.window() {
            return Err(fail("the doorbells lie outside BAR0", 0));
        }
        let ready_end = end.min(clock().saturating_add(cap.ready_timeout_ns().min(READY_MAX_NS)));
        // Reset: whatever the driver had running stops; the controller forgets its queues.
        let cc = hw.rd32(regs::CC);
        if cc == u32::MAX {
            return Err(fail("the controller answers all ones (gone)", 0));
        }
        if cc & regs::CC_EN != 0 {
            hw.wr32(regs::CC, cc & !(regs::CC_EN | regs::CC_SHN_MASK));
        }
        self.wait_csts(false, ready_end, clock)?;
        let mut admin: QueuePair<Lent<'_, H::Mem>, AdminCmd> =
            QueuePair::new(0, DEPTH, Lent(&self.asq), Lent(&self.acq)).ok_or(fail("admin queue memory", 0))?;
        let mut io: QueuePair<Lent<'_, H::Mem>, IoCmd> = QueuePair::new(1, DEPTH, Lent(&self.iosq), Lent(&self.iocq)).ok_or(fail("I/O queue memory", 0))?;
        hw.wr32(regs::AQA, (DEPTH as u32 - 1) | (DEPTH as u32 - 1) << 16);
        self.wr64(regs::ASQ, self.asq.dev_addr());
        self.wr64(regs::ACQ, self.acq.dev_addr());
        hw.wr32(regs::CC, regs::cc_config());
        hw.wr32(regs::CC, regs::cc_config() | regs::CC_EN);
        let ready_end = end.min(clock().saturating_add(cap.ready_timeout_ns().min(READY_MAX_NS)));
        self.wait_csts(true, ready_end, clock)?;
        let adb = (regs::doorbell(0, false, stride), regs::doorbell(0, true, stride));
        // The count the controller granted before stays (NVMe 1.4 §5.21.1.7); asked again as Linux does at each
        // reset, its answer does not matter.
        let _ = self.run(&mut admin, &AdminCmd::set_num_queues(1, 1), "set the queue count", adb, end, clock);
        self.run(&mut admin, &AdminCmd::create_cq(1, DEPTH, self.iocq.dev_addr(), 0, false), "create the I/O completion queue", adb, end, clock)?;
        self.run(&mut admin, &AdminCmd::create_sq(1, DEPTH, self.iosq.dev_addr(), 1), "create the I/O submission queue", adb, end, clock)?;
        let iodb = (regs::doorbell(1, false, stride), regs::doorbell(1, true, stride));
        let base = self.data.dev_addr();
        self.each(runs, &mut |lba, off, n| {
            let first = base + off as u64;
            let p = prp::plan(first, n, prp::contig_page(first, 1), self.list.dev_addr()).ok_or(fail("PRPs", 0))?;
            for k in 1..=p.list_len {
                if !self.list.wr64((k - 1) * 8, prp::contig_page(first, k)) {
                    return Err(fail("PRP list", 0));
                }
            }
            let cmd = IoCmd::write(&self.window, self.nsid, lba, (n / self.bs) as u32, p.prp1, p.prp2, true)
                .ok_or(fail("outside the boot partition or the buffer", 0))?;
            self.run(&mut io, &cmd, "write", iodb, end, clock).map(|_| ())
        })?;
        self.run(&mut io, &IoCmd::flush(self.nsid), "flush", iodb, end, clock)?;
        Ok(())
    }

    /// Boot: a read through the driver's normal path (the extent check compares it with the file system's view).
    pub fn read(&self, lba: u64, out: &mut [u8]) -> Result<(), String> {
        self.ctrl.read(self.nsid, lba, out, true).map_err(|e| e.text())
    }
}

impl<H: Hw + 'static> kexec::crash::disk::Device for PanicWriter<H> {
    fn name(&self) -> &str {
        PanicWriter::name(self)
    }
    fn capacity(&self) -> usize {
        PanicWriter::capacity(self)
    }
    fn block_size(&self) -> usize {
        PanicWriter::block_size(self)
    }
    fn allowed(&self) -> (u64, u64) {
        PanicWriter::allowed(self)
    }
    fn put(&self, off: usize, data: &[u8]) -> bool {
        PanicWriter::put(self, off, data)
    }
    fn get(&self, off: usize, out: &mut [u8]) -> bool {
        PanicWriter::get(self, off, out)
    }
    fn ready(&self) -> Result<(), &'static str> {
        PanicWriter::ready(self)
    }
    fn write(&self, runs: &[Run], clock: &dyn Fn() -> u64, bound_ns: u64) -> Result<(), Fail> {
        PanicWriter::write(self, runs, clock, bound_ns)
    }
    fn read(&self, lba: u64, out: &mut [u8]) -> Result<(), String> {
        PanicWriter::read(self, lba, out)
    }
}
