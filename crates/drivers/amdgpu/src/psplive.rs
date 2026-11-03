//! GX-7d live glue: the first real writes to the 780M's PSP (the desk's MCP tool `gx_bringup`, never a ktest).
//!
//! - `PspHw`: the device under the bring-up owner. Registers through BAR5 (`DcnRegs::read`/`write_raw`: never the
//!   display's write log, never the MM_INDEX window), reads only the classifier's and the PSP mailbox's
//!   addresses, writes only the mailbox's five (C2PMSG_64/67/69/70/71). GPU memory is the carve-out through the
//!   CPU: MC `mc_base + off` is CPU `phys_base + off` (an APU's VRAM, Linux's `aper_base`, gmc_v11_0.c:720), mapped
//!   uncached in 2 MiB pieces on first use (only ranges the firmware map calls reserved; never the top 64 KiB the
//!   firmware closes to the CPU; never a piece that meets a range in use before the run: the GOP surface, A1,
//!   the cursor and the DMCUB windows are mapped write-combining elsewhere, and two memory types on one page
//!   are undefined, Intel SDM Vol. 3 11.12.4) and kept for the boot. The PSP's buffers take whole pieces of
//!   their own (`pspbringup::Pieces`), so every piece mapped here holds only PSP pages. Uncached stores are ordered and reach DRAM, so no HDP
//!   flush is needed (Linux skips it on x86-64 APUs too, amdgpu_device.c:5856-5858).
//! - Bus master on the GPU function and every bridge above it (`PciFunction::enable_device(true)`; the firmware
//!   leaves it off and DMA then fails with UR, CONTRACTS 6b); switched off again on the function after an
//!   undone run and by the Quiesce hook.
//! - The run itself is `gpucore::pspbringup::run` over `Bringup<Watch<PspHw>>` (`Mode::Block`, the GOP surface,
//!   A1 and every reserved range protected), with a DCN snapshot before and after (the picture must not move).
//! - Kept rings (`keep`) live in `LIVE`; the Quiesce hook destroys TMR and ring before Reload/reboot without
//!   allocating or sleeping (busy waits, `Timeouts::QUIESCE`, no lock but a try). Bus master goes off only when
//!   the PSP provably holds nothing (`quiesce_bus_master_off`): never under a run in progress, never after a
//!   failed undo (the PSP's DMA would then end in UR).
//! - The PSP's BOs come from a pool of this boot and go back to it, never to the manager: a piece once mapped
//!   uncached stays mapped (hal has no unmap), so no display BO may come into it later.
//! - A PSP that may still hold its buffers (failed undo, wedged ring) is quarantined: its BOs are never given
//!   back in this boot and every later run is refused until a reboot.
//! - GX-7e (`until=gfx`, M3): also the SMU's driver mailbox (C2PMSG_66/82/90; never the display's 67/83/91) and
//!   the SMN index with FIRMWARE_FLAGS' address only are writable, GC's flags/RLC/CP readable. A GC stage once
//!   per boot (`again` for more). GX-7f: once EnableGfxImu reached PMFW, TMR and ring stay (in `LIVE`, as a kept
//!   run); the Quiesce hook destroys them only when GC booted; `until=reset` (mode 2) is refused
//!   (`gfxup::MODE2_REFUSED`).
//! - GX-7f: the stop note (`handoff::StopNote`) goes to the next kernel through `NoteIo` (kexec's driver-note slot,
//!   bound by the kernel): what the last run, `until=stop` or the Quiesce hook did to ring, TMR, GC and bus master.
//! - GX-7h (`until=gart`): the GMC stage (`gpucore::gmc`). After a passed `until=gfx` (TMR and ring held for GC,
//!   GC booted) it continues on that run's owner; in a boot without one it runs PSP, GC and GMC in one go. The
//!   hub registers join the owner's lists: GFXHUB's plan, MMHUB's without the fields the display uses (those are
//!   never writable here, whatever the stage decides), the flush engines. The GART table's BO is kept for the
//!   boot (`GART_TABLE`) and reused by a later run (`again`). The picture is proven by the DCN snapshot, the
//!   display's write count and the OTG CRCs read before and after (read-only: a CRC that is off stays off).
//!   Quiesce leaves the hubs as they are: the next kernel reads VMID 0's state from the registers (classify) and
//!   reprograms it before any engine runs.
//! - GX-7i (`until=sdma`, M4): the SDMA stage (`gpucore::sdma`) after the GART (continuing a kept run, with the
//!   GMC stage first when this boot has no passed one). SDMA 0's registers and NBIO's doorbell side (the RSMU
//!   index only with the CSDMA range's address) join the owner's lists; the doorbell BAR (BAR2) is mapped
//!   uncached, one page, and only SDMA 0's doorbell may be rung; rung 3's system pages are MM-14 `DmaPages`
//!   (write-back, snooped), pinned for the device while bound. The SDMA buffer is kept for the boot (`SDMA_BO`).
//!   The engine is left running after a pass (Linux's state); `until=stop` and the Quiesce hook (in every boot
//!   an SDMA stage ran in, even under a busy run) halt it first (ring and IBs off, F32 halted: no engine runs on
//!   into the next kernel's memory). The CRC is a pass rule as for `until=gart`.
//! - GX-7j (`until=gfx_ring`, M5): the GFX stage (`gpucore::gfxring`: the GFX tail, ring 0 driven by the CP
//!   directly with MES idle) after the GART, as `until=sdma` (continuing a kept run; SDMA not needed). GC's CP and
//!   tail registers join the owner's lists (never CP_MES_CNTL; CP_MEC_CNTL since GX-7j2), GFX ring 0's doorbell
//!   (BAR2 dword 0x116) joins SDMA's as the only ones that may be rung; the GFX buffer is kept for the boot
//!   (`GFX_BO`). The CP stays running after a pass; `until=stop` and the Quiesce hook halt it first (CP_ME_CNTL
//!   CE/PFP/ME halt, then CP_MEC_CNTL MEC ME1 halt, raw BAR5 in the hook, armed before the stage's first write:
//!   `GFX_ARMED`), before SDMA and the PSP. GX-7j2: `variant` (e1a, e1b, e1c; `gfxring::Variant`) picks the
//!   order of the CP start; without it e1b.

use crate::bo::{self, BoManager, DISCOVERY_RESERVE};
use crate::dcn::{self, DcnRegs};
use crate::gpu::Gpu;
use crate::gpucore::bringup::{Bringup, Gate, LOG_KEEP};
use crate::gpucore::classify;
use crate::gpucore::disc::{self, Discovery};
use crate::gpucore::fwload::{self, StickSource};
use crate::gpucore::psp::{BoAlloc, Carve, Hw, Psp, PspError, Timeouts, Until};
use crate::gpucore::gfxring::{self, GfxAddrs, GfxBo, GfxRingInputs};
use crate::gpucore::gfxup;
use crate::gpucore::gmc::{self, GmcInputs, TableBo};
use crate::gpucore::mes::{self, MesAddrs, MesBo, MesInputs};
use crate::gpucore::handoff::{self, GcFate, RingFate, StopNote};
use crate::gpucore::pspbringup::{self, GfxInputs, Inputs, PIECE, Pool};
use crate::gpucore::sdma::{self, GartUse, NbioDoorbell, SdmaAddrs, SdmaBo, SdmaInputs, SysAlloc, SysPages};
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::vmring::{self, VmAddrs, VmUp};
use crate::gpucore::vram::CarvePlan;
use crate::gpucore::watch::{DisplayGuard, Mode, Watch};
use crate::ipmod::{self, psp13::{Addrs, Mailbox13}};
use crate::{probe, snapshot};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use ksync::{Mutex, SpinLock};

/// The carve-out's CPU mapping pieces.
pub const CHUNK: u64 = PIECE;
const PAGE_4K: u64 = 4096;
const WAIT_NS: u64 = 100_000_000;

/// The pieces mapped in this boot: (CPU phys, mapping). Never given back (VA is not reused); at most one per
/// 2 MiB of carve-out the PSP's buffers touch.
static CHUNKS: Mutex<Vec<(u64, Arc<hal::mmio::IoMem>)>> = Mutex::named("amdgpu.psp_chunks", Vec::new());

/// Where the carve-out lies (offsets from 0 to `size`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarveWin {
    pub mc_base: u64,
    pub phys_base: u64,
    pub size: u64,
}

impl CarveWin {
    /// The carve-out offset range of `[mc, mc + len)` when it lies below the CPU-closed top 64 KiB.
    pub fn offsets(&self, mc: u64, len: usize) -> Option<Range<u64>> {
        let off = mc.checked_sub(self.mc_base)?;
        let end = off.checked_add(len as u64)?;
        (end <= self.size.saturating_sub(DISCOVERY_RESERVE)).then_some(off..end)
    }

    /// The bytes of piece `c` the CPU may map: up to the CPU-closed top 64 KiB, `None` past it.
    pub fn piece_len(&self, c: u64) -> Option<u64> {
        let open = self.size.saturating_sub(DISCOVERY_RESERVE);
        let start = c.checked_mul(CHUNK)?;
        (start < open).then(|| CHUNK.min(open - start))
    }

    /// The 2 MiB pieces `r` touches: (piece index, offset in it, bytes, offset in `r`).
    pub fn pieces(r: Range<u64>) -> impl Iterator<Item = (u64, usize, usize, usize)> {
        let mut at = r.start;
        core::iter::from_fn(move || {
            if at >= r.end {
                return None;
            }
            let (c, within) = (at / CHUNK, at % CHUNK);
            let n = (CHUNK - within).min(r.end - at);
            let item = (c, within as usize, n as usize, (at - r.start) as usize);
            at += n;
            Some(item)
        })
    }
}

pub struct PspHw {
    regs: &'static DcnRegs,
    read_ok: Vec<u32>,
    write_ok: Vec<u32>,
    /// GX-7e: the SMN index may be written with FIRMWARE_FLAGS' address only.
    smn: Option<crate::ipmod::smu13::Addrs>,
    win: CarveWin,
    /// This owner's pieces (clones of `CHUNKS`' entries): the Quiesce path uses only these.
    pieces: Vec<(u64, Arc<hal::mmio::IoMem>)>,
    /// Carve-out ranges in use before the run (reserved, every BO): a piece that meets one is never mapped.
    forbid: Vec<Range<u64>>,
    /// Interrupts off (Quiesce): no new mapping, no lock, every delay a busy wait.
    pub atomic: bool,
    /// GX-7i: NBIO's RSMU index may be written with the CSDMA doorbell range's address only.
    nbio: Option<NbioDoorbell>,
    /// GX-7i: the doorbell BAR's address and its first page, mapped uncached on the first ring.
    doorbell: Option<(u64, Option<Arc<hal::mmio::IoMem>>)>,
    /// GX-7j: the doorbells this owner may ring (SDMA 0's, GFX ring 0's: dword indexes of BAR2's first page).
    doorbells_ok: Vec<u32>,
}

/// The GMC stage's registers on the live owner's lists (GX-7h): reads for both hubs, writes for GFXHUB's plan,
/// MMHUB's GART fields and the flush engines; never an MMHUB field the display uses.
pub fn gmc_lists(g: &GmcInputs, guard: &DisplayGuard) -> (Vec<u32>, Vec<u32>) {
    (g.addrs.readable(), g.addrs.writable(guard))
}

impl PspHw {
    /// `g`: the SMU/GC stage's addresses (M3 and mode 2); without it only the PSP's five registers are writable.
    pub fn new(regs: &'static DcnRegs, d: &Discovery, a: &Addrs, g: Option<&GfxInputs>, win: CarveWin, forbid: Vec<Range<u64>>) -> PspHw {
        PspHw::with_gmc(regs, d, a, g, None, win, forbid)
    }

    /// `new` with the GMC stage's lists (`gmc_lists`).
    pub fn with_gmc(regs: &'static DcnRegs, d: &Discovery, a: &Addrs, g: Option<&GfxInputs>, gm: Option<(Vec<u32>, Vec<u32>)>, win: CarveWin, forbid: Vec<Range<u64>>) -> PspHw {
        let mut read_ok: Vec<u32> = classify::addresses(d).into_iter().filter_map(|(_, b)| b).collect();
        read_ok.extend(a.readable());
        let mut write_ok: Vec<u32> = a.writable().to_vec();
        if let Some(g) = g {
            read_ok.extend(g.smu.readable());
            read_ok.extend(g.gc.readable());
            write_ok.extend(g.smu.writable());
            write_ok.extend(g.gc.writable());
        }
        let mut hw = PspHw { regs, read_ok, write_ok, smn: g.map(|g| g.smu), win, pieces: Vec::new(), forbid, atomic: false, nbio: None, doorbell: None, doorbells_ok: Vec::new() };
        if let Some((r, w)) = gm {
            hw.allow_gmc(r, w);
        }
        hw
    }

    /// GX-7h: the GMC stage's registers join the lists (a continuation on a kept owner).
    pub fn allow_gmc(&mut self, read: Vec<u32>, write: Vec<u32>) {
        for b in read {
            if !self.read_ok.contains(&b) {
                self.read_ok.push(b);
            }
        }
        for b in write {
            if !self.write_ok.contains(&b) {
                self.write_ok.push(b);
            }
        }
    }

    /// GX-7i: SDMA 0's registers and NBIO's doorbell side join the lists; `bar2`: the doorbell BAR's address.
    pub fn allow_sdma(&mut self, a: &SdmaAddrs, bar2: Option<u64>) {
        self.allow_gmc(a.readable(), a.writable());
        self.nbio = a.nbio;
        self.allow_doorbell(crate::ipmod::sdma6::DOORBELL_DW, bar2);
    }

    /// GX-7j: GC's CP and GFX tail registers and NBIO's doorbell aperture join the lists, and GFX ring 0's doorbell.
    pub fn allow_gfx(&mut self, a: &GfxAddrs, bar2: Option<u64>) {
        self.allow_gmc(a.readable(), a.writable());
        self.allow_doorbell(crate::ipmod::gfx11::DOORBELL_DW, bar2);
    }

    /// GX-7k: MES's and the HQDs' registers join the lists, and MES's doorbells (KIQ, scheduler, compute ring,
    /// GFX ring 0).
    pub fn allow_mes(&mut self, a: &MesAddrs, bar2: Option<u64>) {
        self.allow_gmc(a.readable(), a.writable());
        for i in a.doorbells() {
            self.allow_doorbell(i, bar2);
        }
    }

    /// GX-7m: the MES process queue's doorbell (`vmproc::DOORBELL_DW`).
    pub fn allow_vm_doorbell(&mut self, bar2: Option<u64>) {
        self.allow_doorbell(crate::gpucore::vmproc::DOORBELL_DW, bar2);
    }

    fn allow_doorbell(&mut self, index_dw: u32, bar2: Option<u64>) {
        if !self.doorbells_ok.contains(&index_dw) {
            self.doorbells_ok.push(index_dw);
        }
        if self.doorbell.is_none() {
            self.doorbell = bar2.map(|p| (p, None));
        }
    }

    /// Whether the owner may ring the doorbell at `index_dw` (pure: tested on the host): a named one in BAR2's first
    /// page (the mapping is one page).
    pub fn doorbell_ok(allowed: &[u32], index_dw: u32) -> bool {
        allowed.contains(&index_dw) && u64::from(index_dw) * 4 + 8 <= PAGE_4K
    }

    /// Whether the live owner may write `value` to `byte` (pure: tested on the host).
    pub fn may_write(write_ok: &[u32], smn: Option<&crate::ipmod::smu13::Addrs>, byte: u32, value: u32) -> bool {
        write_ok.contains(&byte) || smn.is_some_and(|s| s.smn_index_ok(byte, value))
    }

    fn piece(&mut self, c: u64) -> Option<Arc<hal::mmio::IoMem>> {
        let phys = self.win.phys_base.checked_add(c.checked_mul(CHUNK)?)?;
        if let Some((_, io)) = self.pieces.iter().find(|(p, _)| *p == phys) {
            return Some(io.clone());
        }
        if self.atomic {
            return None;
        }
        let mut all = CHUNKS.lock_timeout(WAIT_NS)?;
        let io = match all.iter().find(|(p, _)| *p == phys) {
            Some((_, io)) => io.clone(),
            None => {
                let len = self.win.piece_len(c)?;
                if let Some(r) = meets(c * CHUNK..c * CHUNK + len, &self.forbid) {
                    klog::event!(Error, "gx", "psp.carve.alias_refused", phys = klog::Hex(phys), start = klog::Hex(r.start), end = klog::Hex(r.end);
                        "piece meets a range mapped elsewhere: never two memory types on one page");
                    return None;
                }
                if let Err(why) = crate::carve::map_check(phys, len) {
                    klog::event!(Error, "gx", "psp.carve.map_refused", phys = klog::Hex(phys), why = why.as_str());
                    return None;
                }
                let io = match hal::mmio::map_device(phys, len as usize, hal::paging::CacheType::Uncached) {
                    Ok(io) => Arc::new(io),
                    Err(e) => {
                        klog::event!(Error, "gx", "psp.carve.map_failed", phys = klog::Hex(phys), why = format!("{e:?}").as_str());
                        return None;
                    }
                };
                klog::event!(Info, "gx", "psp.carve.mapped", phys = klog::Hex(phys), bytes = len; "uncached, for the PSP's buffers");
                all.push((phys, io.clone()));
                io
            }
        };
        self.pieces.push((phys, io.clone()));
        Some(io)
    }
}

impl Hw for PspHw {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        // GX-7i: the RSMU pair lies in the bytes no DCN code may touch (X8/R4); only the SDMA stage reads it, as
        // NBIO's port window, under the LIVE lock (no other user in the kernel).
        let rsmu = self.nbio.is_some_and(|n| byte == n.rsmu_index || byte == n.rsmu_data);
        ((crate::state::wide_readable(byte) || rsmu) && self.read_ok.contains(&byte)).then(|| self.regs.read(byte).ok()).flatten()
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        let ok = PspHw::may_write(&self.write_ok, self.smn.as_ref(), byte, value) || self.nbio.is_some_and(|n| n.index_ok(byte, value));
        ok && self.regs.write_raw(byte, value).is_ok()
    }
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        // Only the doorbells a stage named (SDMA 0's, GFX ring 0's: the first page of BAR2 holds every kernel doorbell).
        if !Self::doorbell_ok(&self.doorbells_ok, index_dw) {
            return false;
        }
        let Some((phys, io)) = self.doorbell.as_mut() else { return false };
        if io.is_none() {
            if self.atomic {
                return false;
            }
            match hal::mmio::map_device(*phys, PAGE_4K as usize, hal::paging::CacheType::Uncached) {
                Ok(m) => {
                    klog::event!(Info, "gx", "doorbell.mapped", phys = klog::Hex(*phys); "BAR2's first page, uncached");
                    *io = Some(Arc::new(m));
                }
                Err(e) => {
                    klog::event!(Error, "gx", "sdma.doorbell_map_failed", phys = klog::Hex(*phys), why = format!("{e:?}").as_str());
                    return false;
                }
            }
        }
        io.as_ref().is_some_and(|m| m.write64(index_dw as usize * 4, value).is_ok())
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        let Some(r) = self.win.offsets(mc, buf.len()) else { return false };
        for (c, within, n, at) in CarveWin::pieces(r) {
            let Some(io) = self.piece(c) else { return false };
            let dst = &mut buf[at..at + n];
            // Dwords where aligned (a fence is read in one access, never torn), bytes at the edges.
            let mut i = 0;
            while i < n {
                if (within + i) % 4 == 0 && i + 4 <= n {
                    let Ok(w) = io.read32(within + i) else { return false };
                    dst[i..i + 4].copy_from_slice(&w.to_le_bytes());
                    i += 4;
                } else {
                    let Ok(x) = io.read8(within + i) else { return false };
                    dst[i] = x;
                    i += 1;
                }
            }
        }
        true
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        let Some(r) = self.win.offsets(mc, data.len()) else { return false };
        for (c, within, n, at) in CarveWin::pieces(r) {
            let Some(io) = self.piece(c) else { return false };
            let src = &data[at..at + n];
            // Dwords where aligned (a quarter of the uncached transactions), bytes at the edges.
            let mut i = 0;
            while i < n {
                let ok = if (within + i) % 4 == 0 && i + 4 <= n {
                    let w = u32::from_le_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]);
                    let r = io.write32(within + i, w).is_ok();
                    i += 4;
                    r
                } else {
                    let r = io.write8(within + i, src[i]).is_ok();
                    i += 1;
                    r
                };
                if !ok {
                    return false;
                }
            }
        }
        true
    }
    fn mem_write64(&mut self, mc: u64, value: u64) -> bool {
        let Some(r) = self.win.offsets(mc, 8) else { return false };
        if r.start % 8 != 0 {
            return false;
        }
        let (c, within) = (r.start / CHUNK, (r.start % CHUNK) as usize);
        self.piece(c).is_some_and(|io| io.write64(within, value).is_ok())
    }
    fn store_barrier(&mut self) {
        // The pieces are uncached (ordered already); a full fence also drains write-combining buffers, should
        // a piece ever be mapped WC, before the TLB flush's register write.
        core::sync::atomic::fence(Ordering::SeqCst);
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    fn delay_us(&mut self, us: u64) {
        // R5: waits of 100 us and more sleep (the waits themselves end by the clock, not by the count).
        if self.atomic || us < 100 {
            time::delay_us(us);
        } else {
            sched::sleep_ns(us.saturating_mul(1000));
        }
    }
}

/// The PSP buffers of this boot (`pspbringup::Pool`): taken from the manager once, never given back to it.
static POOL: Mutex<Pool> = Mutex::named("amdgpu.psp_pool", Pool::new());

/// The device object's BO manager through the boot's PSP pool, each locked per call (the run sleeps between
/// allocations). Lock order: the pool, then the manager.
pub struct SharedBos<'a>(pub &'a SpinLock<BoManager>);

impl BoAlloc for SharedBos<'_> {
    fn alloc(&mut self, size: u64, align: u64) -> Result<(u32, Range<u64>), bo::BoError> {
        let mut pool = POOL.lock_timeout(WAIT_NS).ok_or(bo::BoError::Busy)?;
        pool.alloc(&mut *self.0.lock(), size, align)
    }
    fn release(&mut self, handle: u32) {
        // Back to the pool only; a pool that cannot be locked keeps the buffer busy (leaked, never reused).
        if let Some(mut pool) = POOL.lock_timeout(WAIT_NS) {
            pool.release(handle);
        }
    }
}

type Owner = Bringup<Watch<PspHw>>;

struct Live {
    owner: Owner,
    psp: Psp<Mailbox13>,
    /// What the run left GC in (`Unsettled`: the Quiesce hook leaves TMR and ring).
    gc: GcFate,
    addrs: Addrs,
    /// GX-7i: an SDMA stage ran on this owner: `until=stop` halts SDMA 0 before the PSP (the Quiesce hook halts it through `SDMA_ARMED`).
    sdma: Option<SdmaAddrs>,
    /// GX-7j: a GFX stage ran on this owner: `until=stop` halts the CP first (the Quiesce hook through `GFX_ARMED`).
    gfx: Option<GfxAddrs>,
    /// GX-7k: MES runs on this owner (its BO's MC base, legacy queues mapped): `until=stop` unmaps, dequeues and
    /// halts it before the CP (the Quiesce hook halts it through `MES_ARMED`).
    mes: Option<(MesAddrs, Option<u64>, bool)>,
    /// GX-7j3: the SMU and GC registers of the stop's unload (from discovery: sent only while `gc` is `Booted`).
    un: Option<handoff::UnloadAddrs>,
}

/// A ring kept by `keep` (with its owner, for the Quiesce hook and `until=stop`).
static LIVE: Mutex<Option<Live>> = Mutex::named("amdgpu.psp_live", None);
/// This driver switched the function's bus master on (the firmware had it off).
static BUS_MASTER_OURS: AtomicBool = AtomicBool::new(false);
static QUIESCE_ON: AtomicBool = AtomicBool::new(false);
/// M2 runs in this boot (a second load in one power cycle has no Linux model, V1-20 OPEN-13).
static M2_RUNS: AtomicU32 = AtomicU32::new(0);
/// Set when a run ended in a state only a reboot clears (ring never ready, a fence lost).
static REBOOT_NEEDED: AtomicBool = AtomicBool::new(false);
/// GX-7e: GC stages run in this boot (a second EnableGfxImu on a booted GC has no Linux model but resume).
static GFX_RUNS: AtomicU32 = AtomicU32::new(0);
/// GX-7e: the last GC stage failed after EnableGfxImu reached PMFW.
static GC_FAILED: AtomicBool = AtomicBool::new(false);
/// GX-7h: GMC stages run in this boot, and the GART table's BO (kept for the boot: the hubs point at it).
static GMC_RUNS: AtomicU32 = AtomicU32::new(0);
static GART_TABLE: SpinLock<Option<TableBo>> = SpinLock::named("amdgpu.gart_table", None);
/// GX-7i: the GART of this boot's last passed GMC stage (`None` after a failed one), the SDMA stages run, the
/// SDMA buffer (kept for the boot: an engine left running reads its ring) and the firmware's burst NOP (0 unknown,
/// 1 off, 2 on; read when a run loaded the firmware).
static GART_UP: SpinLock<Option<GartUse>> = SpinLock::named("amdgpu.gart_up", None);
static SDMA_RUNS: AtomicU32 = AtomicU32::new(0);
static SDMA_BO: SpinLock<Option<SdmaBo>> = SpinLock::named("amdgpu.sdma_bo", None);
static BURST_NOP: AtomicU8 = AtomicU8::new(0);
/// GX-7i: an SDMA stage began in this boot (armed before its first SDMA write): the Quiesce hook halts SDMA 0
/// through its registers. A `Once`, so the hook reads it without any lock (a run may hold every other one).
static SDMA_ARMED: ksync::Once<SdmaAddrs> = ksync::Once::new();

/// GX-7j: GFX stages run in this boot, the GFX buffer (kept for the boot: a CP left running reads its ring) and
/// the arming of the Quiesce halt (a `Once`, read without any lock).
static GFX_RING_RUNS: AtomicU32 = AtomicU32::new(0);
static GFX_BO: SpinLock<Option<GfxBo>> = SpinLock::named("amdgpu.gfx_bo", None);
static GFX_ARMED: ksync::Once<GfxAddrs> = ksync::Once::new();
/// GX-7k: MES stages run in this boot, the MES buffer (kept for the boot: MES reads its MQDs and rings while it
/// runs), the MES firmware starts of this boot's firmware load, MES left running by a run of this boot (with its
/// buffer's MC base and whether legacy queues are mapped), and the arming of the Quiesce halt.
static MES_RUNS: AtomicU32 = AtomicU32::new(0);
static MES_BO: SpinLock<Option<MesBo>> = SpinLock::named("amdgpu.mes_bo", None);
static MES_UC: SpinLock<[Option<u64>; 2]> = SpinLock::named("amdgpu.mes_uc", [None, None]);
static MES_UP: SpinLock<Option<(MesAddrs, Option<u64>, bool)>> = SpinLock::named("amdgpu.mes_up", None);

/// Host tests: `MES_UP` as a passed, kept gfx_mes leaves it (`Some`) or as at boot (`None`).
#[cfg(test)]
pub(crate) fn set_mes_up_for_test(v: Option<(MesAddrs, Option<u64>, bool)>) {
    *MES_UP.lock() = v;
}
static MES_ARMED: ksync::Once<MesAddrs> = ksync::Once::new();
/// Legacy queues MES did not unmap (a failed gfx_mes run or stop with MES deaf), or left by the VM stage's recovery
/// (`vmring::VmReport::stranded`): their HQDs stay active with no firmware owning them, so no stage may start the CP or its firmware again in this boot. Cleared by a passed
/// gfx_mes run (MES owns both queues again).
static MES_STRANDED: AtomicBool = AtomicBool::new(false);
/// GX-7m: this run's `ack=linux` (until=vm). Stored by every continue on a kept run before the stage runs, so the
/// initial value is never read; the default (linux for until=vm) is `parse_args`'s alone.
static VM_ACK_LINUX: AtomicBool = AtomicBool::new(false);
/// GX-7m: the test VM a passed `until=vm` left (until=stop, a later MES/GFX stage or until=vm tear it down).
static VM_UP: SpinLock<Option<(VmAddrs, VmUp)>> = SpinLock::named("amdgpu.vm_up", None);

/// GX-7m: the kept test VM torn down (its MES process queue removed, then base and LUT 0 by the CPU, a flush on
/// engine 17), bounded. A failed CPU part is put back for the next GFX/MES stage or until=vm to retry, except at a
/// stop (GC is unloaded after it: no later write to GFXHUB; a stale base in a context no queue uses is harmless);
/// a process queue MES did not remove counts as stranded.
fn vm_teardown<H: Hw>(owner: &mut Bringup<Watch<H>>, why: &str, lines: &mut Vec<String>) {
    let Some((a, up)) = VM_UP.lock().take() else { return };
    let t = vmring::teardown(owner, &a, &up, gmc::LINUX_FLUSH_US, crate::ipmod::mes11::API_TIMEOUT_US);
    lines.extend(t.lines);
    if t.stranded {
        note_stranded(&["a queue of the VM stage (the compute ring or the MES process queue)"]);
    }
    let retry = t.retry.filter(|_| why != "stop");
    if let Some(r) = retry {
        *VM_UP.lock() = Some((a, r));
    }
    lines.push(format!("{why}: the test VM (VMID {}) torn down first: {}", up.vmid, if t.ok { "done" } else if retry.is_some() { "a step failed (see above): kept for a retry" } else { "a step failed (see above)" }));
}

/// Notes a stop's or a run's stranded queues (never clears: only a passed gfx_mes does).
fn note_stranded(stranded: &[&'static str]) {
    if !stranded.is_empty() {
        MES_STRANDED.store(true, Ordering::Release);
    }
}

/// The MES stage's `arm` hook (`MesInputs::arm`): the Quiesce hook registered first, then armed.
fn arm_mes(a: MesAddrs) {
    register_quiesce();
    if !MES_ARMED.is_completed() {
        klog::event!(Info, "gx", "mes.armed"; "MES is halted by the Quiesce hook from now on");
    }
    MES_ARMED.get_or_init(|| a);
}

/// GX-7j: NBIO's doorbell aperture as this boot's first doorbell stage (SDMA or GFX) found it: what a stop puts back.
static APER_FIRST: SpinLock<Option<u32>> = SpinLock::named("amdgpu.aper_first", None);

/// The GFX stage's `arm` hook (`GfxRingInputs::arm`): the Quiesce hook registered first, then armed.
fn arm_gfx(a: GfxAddrs) {
    register_quiesce();
    if !GFX_ARMED.is_completed() {
        klog::event!(Info, "gx", "gfx.armed"; "the CP is halted by the Quiesce hook from now on");
    }
    GFX_ARMED.get_or_init(|| a);
}

/// The stage's `arm` hook (`SdmaInputs::arm`): the Quiesce hook registered first, then armed.
fn arm_sdma(a: SdmaAddrs) {
    register_quiesce();
    if !SDMA_ARMED.is_completed() {
        klog::event!(Info, "gx", "sdma.armed"; "SDMA 0 is halted by the Quiesce hook from now on");
    }
    SDMA_ARMED.get_or_init(|| a);
}

pub use crate::gpucore::handoff::NoteIo;

static NOTE_IO: ksync::Once<NoteIo> = ksync::Once::new();
static INHERITED: ksync::Once<handoff::Arrival> = ksync::Once::new();
/// The note this boot's last run, stop or Quiesce left (`None`: nothing done to the PSP in this boot).
static LAST: SpinLock<Option<StopNote>> = SpinLock::named("amdgpu.psp_note", None);
/// GX-7i2: NBIO's doorbell side (aperture, CSDMA range) as this boot's first SDMA stage found it.
static NBIO_FOUND: SpinLock<Option<sdma::NbioFound>> = SpinLock::named("amdgpu.nbio_found", None);
/// GX-7i2: this boot's SDMA stage turned the doorbell aperture on and no stop put it back (the classifier's trace).
static APERTURE_OURS: AtomicBool = AtomicBool::new(false);
/// The kernel before left bus master on (its note): taken over once by this boot's first run.
static BUS_MASTER_INHERITED: AtomicBool = AtomicBool::new(false);

/// Binds the note's way across a Reload (first binding wins). Unbound: no note is written or read. It also reads
/// the inherited note now (outside Quiesce, where `take` may allocate) and registers the Quiesce hook, so a kernel
/// that never runs gx_bringup still hands the note on to the next one (a chain of Reloads keeps it).
pub fn bind_note_io(io: NoteIo) {
    NOTE_IO.get_or_init(|| io);
    let _ = inherited_note();
    register_quiesce();
}

/// The note the kernel before handed on (read once, outside the Quiesce path).
pub fn inherited_note() -> Option<StopNote> {
    note_arrival().note()
}

/// GX-7i2: how the note arrived, read once per boot and logged at warn level then (visible at loglevel warn;
/// gx_probe shows it at any loglevel).
pub fn note_arrival() -> handoff::Arrival {
    *INHERITED.get_or_init(|| {
        let a = handoff::Arrival::of(NOTE_IO.get());
        log_arrival(&a, NOTE_IO.get().is_some());
        a
    })
}

/// GX-7i2: gx_probe's note lines: the note from the kernel before (how it arrived) and this boot's own.
pub fn note_lines() -> Vec<String> {
    note_lines_of(&note_arrival(), LAST.try_lock().map(|g| *g))
}

/// The two note lines from how the note arrived and this boot's own (`None`: its lock was busy).
pub(crate) fn note_lines_of(arrival: &handoff::Arrival, own: Option<Option<StopNote>>) -> Vec<String> {
    let own = match own {
        Some(n) => n.map_or(String::from("none (nothing done to the PSP in this boot)"), |n| n.text()),
        None => String::from("not read (a stop is setting it)"),
    };
    alloc::vec![format!("note from the kernel before: {}", arrival.text()), format!("note of this boot: {own}")]
}

/// The once-per-boot arrival line, at warn level so the desk sees it at loglevel warn.
pub(crate) fn log_arrival(a: &handoff::Arrival, bound: bool) {
    match a {
        handoff::Arrival::Inherited(n) => klog::event!(Warn, "gx", "note.inherited", note = n.text().as_str(); "stop note from the kernel before this Reload"),
        other => klog::event!(Warn, "gx", "note.none", bound = bound, why = other.text().as_str(); "no stop note from the kernel before"),
    }
}

/// GX-7i2: BitsOS's own traces for the classifier: this boot's SDMA stage turned the doorbell aperture on (and no
/// stop has put it back yet).
pub fn own_traces() -> classify::Own {
    classify::Own { doorbell_aperture: APERTURE_OURS.load(Ordering::Acquire) }
}

/// GX-7i2: after an SDMA stage: NBIO's doorbell side as this boot's first stage found it (kept for the stops), and
/// whether the aperture is on by this driver.
fn note_sdma(r: &sdma::SdmaReport) {
    let first = keep_aper_first(&mut APER_FIRST.lock(), r.nbio_found().map(|n| n.aper));
    if keep_nbio_found(&mut NBIO_FOUND.lock(), r) || aperture_ours(first, r.nbio.first().and_then(|x| x.2)) {
        APERTURE_OURS.store(true, Ordering::Release);
    }
}

/// GX-7j: after a GFX stage: the aperture as this boot's first doorbell stage found it, and whether it is on by
/// this driver now.
fn note_gfx(r: &gfxring::GfxReport) {
    let first = keep_aper_first(&mut APER_FIRST.lock(), r.aper_found());
    if aperture_ours(first, r.aper.1) {
        APERTURE_OURS.store(true, Ordering::Release);
    }
}

/// Keeps the first doorbell stage's found aperture (a later stage finds what an earlier one turned on); returns it.
pub(crate) fn keep_aper_first(kept: &mut Option<u32>, found: Option<u32>) -> Option<u32> {
    if kept.is_none() {
        *kept = found;
    }
    *kept
}

/// The aperture is on now by this driver: found off by the boot's first doorbell stage, on now.
pub(crate) fn aperture_ours(first: Option<u32>, now: Option<u32>) -> bool {
    first.is_some_and(|f| f & 1 == 0) && now.is_some_and(|v| v & 1 != 0)
}

/// Keeps the first stage's found values (a later stage finds only what an earlier one turned on); true when the
/// aperture is on now by this driver (found off by the first stage, on after this one).
pub(crate) fn keep_nbio_found(kept: &mut Option<sdma::NbioFound>, r: &sdma::SdmaReport) -> bool {
    if kept.is_none() {
        *kept = r.nbio_found();
    }
    let on_now = r.nbio.first().and_then(|x| x.2).is_some_and(|v| v & 1 != 0);
    kept.is_some_and(|f| f.aper & 1 == 0) && on_now
}

/// GX-7i2: after SDMA 0 is halted (until=stop, or a run that keeps nothing): what the stages turned on goes back to
/// what this boot's first stage found (`sdma::doorbell_back`), and the line says so.
fn sdma_back(owner: &mut Owner, sa: &SdmaAddrs, lines: &mut Vec<String>) {
    // GX-7j: the aperture back to what the boot's first doorbell stage found (a GFX stage may have come first).
    let first = *APER_FIRST.lock();
    let found = (*NBIO_FOUND.lock()).map(|n| sdma::NbioFound { aper: first.unwrap_or(n.aper), ..n });
    if put_doorbell_back(owner, sa, found, lines) {
        APERTURE_OURS.store(false, Ordering::Release);
    }
}

/// GX-7j: after the CP is halted (until=stop, or a run that keeps nothing): its doorbell off and, when no SDMA stage
/// puts it back after this (`aperture`), NBIO's aperture to what the boot's first doorbell stage found.
fn gfx_back(owner: &mut Owner, xa: &GfxAddrs, aperture: bool, lines: &mut Vec<String>) {
    let first = if aperture { *APER_FIRST.lock() } else { None };
    let (ok, l) = gfxring::doorbell_back(owner, xa, first);
    lines.extend(l);
    lines.push(format!("stop: GFX ring 0's doorbell side {}", if ok { "back as found" } else { "NOT fully back (see the lines above)" }));
    if ok && first.is_some_and(|f| f & 1 == 0) {
        APERTURE_OURS.store(false, Ordering::Release);
    }
}

/// `sdma_back` on any Hw (the fakes test it): true when the aperture is off again by this stop.
pub(crate) fn put_doorbell_back<H: crate::gpucore::psp::Hw>(owner: &mut crate::gpucore::bringup::Bringup<crate::gpucore::watch::Watch<H>>, sa: &SdmaAddrs, found: Option<sdma::NbioFound>, lines: &mut Vec<String>) -> bool {
    let Some(found) = found else {
        lines.push(String::from("stop: NBIO's doorbell side as found is unknown (the stage did not read it): left as it is"));
        return false;
    };
    let (ok, l) = sdma::doorbell_back(owner, sa, found);
    lines.extend(l);
    lines.push(format!("stop: SDMA 0's doorbell side {}", if ok { "back as found (the engine, its queue doorbell, the CSDMA range and the doorbell aperture this driver turned on are off again)" } else { "NOT fully back (see the lines above)" }));
    ok && found.aper & 1 == 0
}

/// What the classifier trusts: this boot's own last note, else the one handed on.
pub fn current_note() -> Option<StopNote> {
    let own = LAST.try_lock().and_then(|g| *g);
    own.or_else(inherited_note)
}

fn set_note(n: StopNote) {
    klog::event!(Info, "gx", "note.set", note = n.text().as_str());
    match LAST.try_lock() {
        Some(mut g) => *g = Some(n),
        None => klog::event!(Warn, "gx", "note.busy"; "stop note not updated (lock busy): the next kernel classifies from the registers"),
    }
}

fn note_of(hw: &mut dyn Hw, a: &Addrs, ring: RingFate, gc: GcFate, tmr_destroyed: bool) -> StopNote {
    handoff::read_note(hw, a, ring, gc, tmr_destroyed, BUS_MASTER_OURS.load(Ordering::Acquire))
}

/// GC as this boot last left it (a run that did not touch GC leaves it as it was).
fn gc_now() -> GcFate {
    current_note().map_or(GcFate::Untouched, |n| n.gc)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Run(Until),
    Stop,
    /// GX-7e: the mode-2 reset of GC (recovery only).
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Args {
    pub action: Action,
    pub keep: bool,
    pub again: bool,
/// GX-7j2: the CP start's order for `until=gfx_ring` (`None`: `Variant::DEFAULT`).
    pub variant: Option<gfxring::Variant>,
    /// GX-7m: the in-ring ack wait with until=vm: Linux's single WAIT_REG_MEM form (`vmring::AckWait::Linux`,
    /// the default since INT-W28) or `ack=helper` (WRITE_DATA, then the ack poll).
    pub ack_linux: bool,
}

/// PSPs whose buffers the PSP may still hold (failed undo, wedged ring): their BOs are never given back in this
/// boot (`Psp` has no Drop, so dropping it keeps them allocated), bus master stays on, runs are refused.
static QUARANTINED: AtomicU32 = AtomicU32::new(0);

struct PspQuiesce;

/// What the Quiesce hook met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuiesceSeen {
    /// A run (or `until=stop`) holds the PSP: it is mid-command, nothing is touched.
    Busy,
    /// No ring kept.
    Idle,
    /// The kept ring was stopped; `true` when the PSP provably holds nothing now.
    Stopped(bool),
    /// GX-7f: a ring held for an unsettled GC: left up (nothing destroyed under a possibly running RLC/IMU).
    Left,
}

/// Whether the Quiesce hook may switch bus master off on the function (when this driver switched it on): only
/// when the PSP provably holds nothing. A run in progress or a failed undo keeps it on: the PSP's DMA would
/// otherwise end in UR mid-command, the very failure bus master is for (the next kernel's stale path cleans up).
pub fn quiesce_bus_master_off(seen: QuiesceSeen, quarantined: bool) -> bool {
    match seen {
        QuiesceSeen::Busy => false,
        QuiesceSeen::Idle => !quarantined,
        QuiesceSeen::Stopped(quiet) => quiet && !quarantined,
        QuiesceSeen::Left => false,
    }
}

impl devmodel::Quiesce for PspQuiesce {
    fn name(&self) -> &str {
        "amdgpu.psp"
    }
    fn quiesce(&self) {
        // GX-7i: SDMA 0 halted first, whatever holds the PSP (a run in progress included): no engine may run on
        // into the next kernel, whose BOs may reuse the ring's and wptr shadow's carve-out pages.
        // GX-7k: MES before the CP (the scheduler stops feeding the queues before their engines halt).
        quiesce_mes();
        quiesce_gfx();
        quiesce_sdma();
        let (seen, stopped) = match LIVE.try_lock() {
            None => (QuiesceSeen::Busy, None),
            Some(mut g) => match g.as_mut() {
                None => (QuiesceSeen::Idle, None),
                Some(l) => {
                    // Past the owner (its log may allocate): the watcher and the device only.
                    let hw = l.owner.hw();
                    hw.hw().atomic = true;
                    // GX-7f: GC may still be booting from the TMR (Unsettled): nothing destroyed, the next kernel refuses.
                    // GX-7j3: a booted GC is unloaded toward PMFW first (bounded, no log), as Linux's shutdown does.
                    let (r, n) = handoff::quiesce_kept(&mut l.psp, hw, &l.addrs, l.gc, l.un.as_ref(), BUS_MASTER_OURS.load(Ordering::Acquire));
                    (r.map_or(QuiesceSeen::Left, QuiesceSeen::Stopped), Some(n))
                }
            },
        };
        if quiesce_bus_master_off(seen, QUARANTINED.load(Ordering::Acquire) > 0)
            && BUS_MASTER_OURS.swap(false, Ordering::AcqRel)
            && let Some(r) = DcnRegs::attached_now()
        {
            let _ = r.pci().disable_device();
        }
        // The note last (bus master as now left); Busy hands on nothing: the next kernel reads the registers.
        let ours = BUS_MASTER_OURS.load(Ordering::Acquire);
        let note = match seen {
            QuiesceSeen::Busy => None,
            QuiesceSeen::Idle => {
                // A busy lock (a stop setting it right now) counts as no own note: the inherited one goes on.
                let last = LAST.try_lock().and_then(|g| *g);
                handoff::idle_hand_on(last, INHERITED.get().and_then(handoff::Arrival::note), BUS_MASTER_INHERITED.load(Ordering::Acquire), ours)
            }
            QuiesceSeen::Stopped(_) | QuiesceSeen::Left => stopped.map(|n| StopNote { bus_master_left_on: ours, ..n }),
        };
        if let (Some(n), Some(io)) = (note, NOTE_IO.get()) {
            let _ = io.put_note(&n);
        }
    }
}

/// GX-7k: the Quiesce path's MES halt through the raw registers (no owner, no lock, no allocation, no wait).
fn quiesce_mes() {
    let Some(regs) = DcnRegs::attached_now() else { return };
    if let Some(ok) = quiesce_halt_mes(MES_ARMED.get(), &mut RawRegs(regs)) {
        klog::try_event!(Info, "gx", "mes.quiesce_halt", ok = ok; "MES halted before the hand-over");
    }
}

/// The Quiesce MES halt on `hw`: `None` when no MES stage began.
pub(crate) fn quiesce_halt_mes(armed: Option<&MesAddrs>, hw: &mut dyn Hw) -> Option<bool> {
    armed.map(|a| mes::halt(hw, a))
}

/// GX-7j: the Quiesce path's CP halt through the raw registers (no owner, no lock, no allocation, no wait).
fn quiesce_gfx() {
    let Some(regs) = DcnRegs::attached_now() else { return };
    if let Some(ok) = quiesce_halt_gfx(GFX_ARMED.get(), &mut RawRegs(regs)) {
        // INT-W26 (RB-16 CR-3a applied to GX-7j's halt too): `try_event!`, never waits in the Quiesce.
        klog::try_event!(Info, "gx", "gfx.quiesce_halt", ok = ok; "the CP halted before the hand-over");
    }
}

/// The Quiesce CP halt on `hw`: `None` when no GFX stage began.
pub(crate) fn quiesce_halt_gfx(armed: Option<&GfxAddrs>, hw: &mut dyn Hw) -> Option<bool> {
    armed.map(|a| gfxring::halt(hw, a))
}

/// GX-7i: the Quiesce path's SDMA halt through the raw registers (no owner, no lock, no allocation, no wait).
fn quiesce_sdma() {
    let Some(regs) = DcnRegs::attached_now() else { return };
    if let Some(ok) = quiesce_halt(SDMA_ARMED.get(), &mut RawRegs(regs)) {
        // RB-16: `try_event!` (one try of the ring lock, never waits): a Quiesce runs with the other CPUs parked
        // and interrupts off, where a plain event may wait for a lock a parked CPU holds.
        klog::try_event!(Info, "gx", "sdma.quiesce_halt", ok = ok; "SDMA 0 halted before the hand-over");
    }
}

/// The Quiesce halt on `hw` (BAR5 in the hook, a fake in the tests): `None` when no SDMA stage began.
pub(crate) fn quiesce_halt(armed: Option<&SdmaAddrs>, hw: &mut dyn Hw) -> Option<bool> {
    armed.map(|a| sdma::halt(hw, a))
}

/// Runs `f` while this module's run lock is held (`true`) when it was free: the tests' "a run holds the PSP".
pub(crate) fn with_live_held<T>(f: impl FnOnce(bool) -> T) -> T {
    let g = LIVE.try_lock();
    let r = f(g.is_some());
    drop(g);
    r
}

/// BAR5 registers only (the Quiesce halt): no memory, no waits.
struct RawRegs(&'static DcnRegs);

impl Hw for RawRegs {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        crate::state::wide_readable(byte).then(|| self.0.read(byte).ok()).flatten()
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.0.write_raw(byte, value).is_ok()
    }
    fn mem_read(&mut self, _mc: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn mem_write(&mut self, _mc: u64, _data: &[u8]) -> bool {
        false
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    fn delay_us(&mut self, us: u64) {
        time::delay_us(us);
    }
}

/// Runs `f` while no gx_bringup run or stop holds the PSP (`None` after the usual wait): the probe's MM_INDEX
/// fetch, so it never lands inside a run's before/after count of the display's writes.
pub fn while_idle<T>(f: impl FnOnce() -> T) -> Option<T> {
    let g = LIVE.lock_timeout(WAIT_NS)?;
    let r = f();
    drop(g);
    Some(r)
}

fn register_quiesce() {
    if !QUIESCE_ON.swap(true, Ordering::AcqRel) {
        devmodel::register_quiesce(Arc::new(PspQuiesce));
    }
}

/// Bus master off on the function again when this driver switched it on (bridges stay, as `disable_device` does)
/// and no PSP is quarantined.
fn bus_master_back(regs: &DcnRegs, out: &mut Vec<String>) {
    let quarantined = QUARANTINED.load(Ordering::Acquire) > 0;
    let ours = !quarantined && BUS_MASTER_OURS.swap(false, Ordering::AcqRel);
    let off = ours.then(|| regs.pci().disable_device_logged()).map(|d| (d.bus_master_was_on, d.refused, d.incomplete));
    out.push(bus_line(bus_choice(quarantined, ours, regs.pci().command() & 0x4 != 0), off));
}

/// What a stop (or a run's end) does with bus master, and the line it prints (GX-7f: every stop prints one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusChoice {
    /// A quarantined PSP may still use its buffers: left on.
    Quarantined,
    /// This driver switched it on (or took it over from the kernel before's note): off again.
    SwitchOff,
    /// On, but not by this driver: not ours to switch off.
    NotOurs,
    /// Off, as found.
    AsFound,
    /// A ring and TMR are kept: stays on.
    Kept,
}

pub fn bus_choice(quarantined: bool, ours: bool, on_now: bool) -> BusChoice {
    match (quarantined, ours, on_now) {
        (true, _, _) => BusChoice::Quarantined,
        (false, true, _) => BusChoice::SwitchOff,
        (false, false, true) => BusChoice::NotOurs,
        (false, false, false) => BusChoice::AsFound,
    }
}

/// `off`: what `disable_device` reported (bus master was on, refused, incomplete) for `SwitchOff`.
pub fn bus_line(c: BusChoice, off: Option<(bool, bool, bool)>) -> String {
    match c {
        BusChoice::Quarantined => String::from("bus master: left on (a quarantined PSP may still use its buffers; reboot)"),
        BusChoice::SwitchOff => {
            let (was_on, refused, incomplete) = off.unwrap_or((false, false, true));
            format!("bus master: off again on the function (was on: {was_on}, refused {refused}, incomplete {incomplete})")
        }
        BusChoice::NotOurs => String::from("bus master: left on, as found (it was on before this driver switched anything, and no note says the kernel before left it: not ours to switch off; the engines this driver started are stopped)"),
        BusChoice::AsFound => String::from("bus master: off (as found)"),
        BusChoice::Kept => String::from("bus master: stays on (TMR and ring are kept)"),
    }
}

/// Parks a PSP that may still hold its buffers: never freed, later runs refused until a reboot.
fn quarantine(p: Psp<Mailbox13>, out: &mut Vec<String>) {
    QUARANTINED.fetch_add(1, Ordering::AcqRel);
    REBOOT_NEEDED.store(true, Ordering::Release);
    for b in p.buffers() {
        out.push(format!("quarantined: carve-out {:#x}..{:#x} (MC {:#x}) stays allocated until a reboot", b.offset, b.offset + b.size, b.mc));
    }
    klog::event!(Error, "gx", "bringup.quarantine", buffers = p.buffers().count(); "PSP buffers kept out of the BO manager until a reboot");
    // No Drop on `Psp`: dropping it gives nothing back to the BO manager.
    drop(p);
}

fn dma_lines(regs: &DcnRegs, when: &str) -> Vec<String> {
    regs.pci()
        .dma_status()
        .iter()
        .map(|s| format!("dma {when}: {} command {:#06x} status {:#06x} bus master {} trouble {}", s.at, s.command, s.status, s.bus_master(), s.trouble()))
        .collect()
}

/// The tool: `Ok(text)` for every run (its first line says PASS, FAIL or REFUSED), `Err` when there is no GPU.
pub fn tool(args: Args) -> Result<String, String> {
    let g = Gpu::get().ok_or_else(|| String::from("no AMD GPU function (QEMU has none)"))?;
    let mut lines = Vec::new();
    let r = match args.action {
        Action::Stop => stop_kept(g, &mut lines),
        Action::Run(until) => run(g, until, args, &mut lines),
        Action::Reset => reset(args, &mut lines),
    };
    let head = match r {
        Ok(true) => "gx_bringup: PASS",
        Ok(false) => "gx_bringup: FAIL",
        Err(()) => "gx_bringup: REFUSED (no PSP or SMU register written)",
    };
    let mut s = String::from(head);
    s.push('\n');
    for l in lines {
        s += &l;
        s.push('\n');
    }
    Ok(s)
}

#[inline(never)]
fn stop_kept(g: &Gpu, lines: &mut Vec<String>) -> Result<bool, ()> {
    let Some(mut guard) = LIVE.lock_timeout(WAIT_NS) else {
        lines.push(String::from("busy: another run holds the PSP"));
        return Err(());
    };
    let Some(mut l) = guard.take() else {
        lines.push(String::from("no ring kept: nothing to stop"));
        bus_master_back(g.regs, lines);
        return Ok(true);
    };
    vm_teardown(&mut l.owner, "stop", lines);
    if let Some((ma, bo_mc, queues)) = l.mes.take() {
        let st = mes::stop(&mut l.owner, &ma, bo_mc, queues);
        note_stranded(&st.stranded);
        lines.extend(st.lines);
        lines.push(format!("stop: MES stopped first (legacy queues unmapped, pipes dequeued, MES halted): {}", if st.ok { "done" } else { "a step failed (see above)" }));
        *MES_UP.lock() = None;
    }
    if let Some(xa) = l.gfx.take() {
        let ok = gfxring::halt(&mut l.owner, &xa);
        lines.push(format!("stop: the CP halted first (CP_ME_CNTL CE/PFP/ME halt, CP_MEC_CNTL MEC ME1 halt): {}", if ok { "done" } else { "a write was refused" }));
        if ok {
            gfx_back(&mut l.owner, &xa, l.sdma.is_none(), lines);
        } else {
            lines.push(String::from("stop: the CP not halted: its doorbell side is left on"));
        }
    }
    if let Some(sa) = l.sdma.take() {
        let ok = sdma::halt(&mut l.owner, &sa);
        lines.push(format!("stop: SDMA 0 halted first (ring and IBs off, F32 halted): {}", if ok { "done" } else { "a write was refused" }));
        if ok {
            sdma_back(&mut l.owner, &sa, lines);
        } else {
            lines.push(String::from("stop: SDMA 0 not halted: its doorbell side is left on"));
        }
    }
    if l.gc == GcFate::Unsettled {
        lines.push(String::from("stop: GC did not boot after EnableGfxImu: TMR and ring destroyed now on the desk's word (the RLC/IMU may still have used them); reboot before more GPU work"));
    }
    // GX-7j3: Linux's order (gfx hw_fini, smu_hw_fini, psp_hw_fini): PMFW told of the unload and the RLC stopped
    // before the TMR the RLC/IMU read goes.
    let (gc, un) = handoff::unload_gc(&mut l.owner, l.gc, l.un.as_ref(), crate::gpucore::smu::SmuTimeouts::LINUX, false);
    if let Some(u) = un {
        lines.push(format!("stop: GC unloaded toward PMFW before the PSP (Linux's smu_hw_fini): {}", u.text()));
        if u.prepared.is_err_and(|e| e.needs_reboot()) {
            lines.push(String::from("stop: PMFW did not answer PrepareMp1ForUnload: its mailbox is stuck; reboot before more GPU work"));
            REBOOT_NEEDED.store(true, Ordering::Release);
        }
    }
    l.gc = gc;
    let r = l.psp.stop(&mut l.owner);
    lines.push(format!("stop: {}", match &r { Ok(()) => String::from("DESTROY_TMR and DESTROY_RINGS answered"), Err(e) => format!("FAILED {e:?}") }));
    let quiet = r.is_ok() && l.psp.quiet();
    if r.is_err() {
        REBOOT_NEEDED.store(true, Ordering::Release);
    }
    let (gc, addrs) = (l.gc, l.addrs);
    match (&g.bos, quiet) {
        (Some(bos), true) => l.psp.free(&mut SharedBos(bos)),
        // Without the manager the buffers cannot go back (the display left): they stay allocated either way.
        (None, true) => {}
        (_, false) => quarantine(l.psp, lines),
    }
    bus_master_back(g.regs, lines);
    // The note after bus master is settled (it says whether bus master is still ours and on).
    let n = note_of(&mut l.owner, &addrs, if quiet { RingFate::Destroyed } else { RingFate::NotDestroyed }, gc, r.is_ok());
    lines.push(format!("note for the next kernel: {}", n.text()));
    set_note(n);
    Ok(quiet)
}

/// Refusal with a reason (no PSP register written).
fn refuse(lines: &mut Vec<String>, why: String) -> Result<bool, ()> {
    klog::event!(Warn, "gx", "bringup.tool_refused", why = why.as_str());
    lines.push(why);
    Err(())
}

/// What the tool knows before a run, read under the `LIVE` lock (two calls at once see each other's verdicts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pre {
    pub reboot_needed: bool,
    pub quarantined: bool,
    pub display: Result<(), String>,
    pub has_bos: bool,
    pub until: Until,
    pub m2_runs: u32,
    pub again: bool,
    pub kept: bool,
    /// GX-7e: GC stages in this boot, the last one failed with GC touched, the call is the mode-2 reset.
    pub gfx_runs: u32,
    pub gc_failed: bool,
    pub reset: bool,
    /// GX-7h: GMC stages in this boot; the kept run booted GC (`until=gart` continues on it).
    pub gmc_runs: u32,
    pub kept_gc_booted: bool,
    /// GX-7i: SDMA stages in this boot; a GMC stage of this boot passed (its GART stands).
    pub sdma_runs: u32,
    pub gart_up: bool,
    /// GX-7j: GFX stages in this boot.
    pub gfx_ring_runs: u32,
    /// GX-7k: MES stages in this boot; MES left running by one of them.
    pub mes_runs: u32,
    pub mes_up: bool,
    /// GX-7k: legacy queues MES did not unmap are left in this boot.
    pub mes_stranded: bool,
    /// GX-7m: MES runs with the compute ring and GFX ring 0 mapped (a passed gfx_mes of this boot).
    pub mes_queues: bool,
}

/// `Pre` as the boot's statics say now (`run` under the `LIVE` lock, `reset`); `kept`: a run is kept, with GC
/// booted in it. Each spin lock is taken in a statement of its own and the copy used after: a guard taken inside
/// one expression (a struct literal's field) lives to the end of the statement, so a second `.lock()` of the same
/// `SpinLock` in it spins on its own CPU forever (INT-W28's `mes_up`/`mes_queues` pair hung every `gx_bringup`
/// on the Mini-PC, plan/notes/gx-GX-7j3-hang.md).
pub(crate) fn pre_now(until: Until, again: bool, kept: Option<bool>, display: Result<(), String>, has_bos: bool, reset: bool) -> Pre {
    let mes = *MES_UP.lock();
    let gart_up = !reset && GART_UP.lock().is_some();
    Pre {
        reboot_needed: REBOOT_NEEDED.load(Ordering::Acquire),
        quarantined: QUARANTINED.load(Ordering::Acquire) > 0,
        display,
        has_bos,
        until,
        m2_runs: M2_RUNS.load(Ordering::Acquire),
        again,
        kept: kept.is_some(),
        gfx_runs: GFX_RUNS.load(Ordering::Acquire),
        gc_failed: GC_FAILED.load(Ordering::Acquire),
        reset,
        gmc_runs: GMC_RUNS.load(Ordering::Acquire),
        kept_gc_booted: kept == Some(true),
        sdma_runs: SDMA_RUNS.load(Ordering::Acquire),
        gart_up,
        gfx_ring_runs: GFX_RING_RUNS.load(Ordering::Acquire),
        mes_runs: MES_RUNS.load(Ordering::Acquire),
        mes_up: mes.is_some(),
        mes_stranded: MES_STRANDED.load(Ordering::Acquire),
        mes_queues: mes.is_some_and(|m| m.2),
    }
}

/// The tool's refusals before anything is written, in this order.
pub fn preflight(p: &Pre) -> Result<(), String> {
    if p.reset {
        // GX-7f: refused (the 28.10. hang); a failed GC stage is recovered by a reboot.
        return Err(format!("{}{}", gfxup::MODE2_REFUSED, if p.gc_failed { " (a GC stage failed in this boot: reboot now)" } else { "" }));
    }
    if p.reboot_needed || p.quarantined {
        return Err(String::from("an earlier run in this boot left the PSP in a state only a reboot clears (see its answer): reboot first"));
    }
    if p.mes_stranded && p.until.loads_fw() {
        // GX-7k: no Linux model restarts the CP or its firmware on HQDs no firmware owns (Linux would reset the
        // GPU, and mode 2 is refused on 13.0.4); not even with again=true.
        return Err(String::from("a failed gfx_mes run, stop or VM stage left queues no firmware owns (see its answer: STRANDED): their HQDs stay active, so no stage may start the CP or its firmware again in this boot; until=stop is allowed, then reboot"));
    }
    if let Err(e) = &p.display {
        return Err(format!("display refused ({e}): the carve-out and the scanout surface are unknown"));
    }
    if !p.has_bos {
        return Err(String::from("no buffer-object manager (the display is not attached)"));
    }
    let gmc_again = || {
        // GX-7i: until=sdma reprograms the hubs only when this boot has no passed GMC stage.
        let gmc = p.until == Until::Gart || (p.until.after_gart() && !p.gart_up);
        if gmc && p.gmc_runs > 0 && !p.again {
            Err(String::from("the GMC stage ran in this boot already: the hubs hold its GART (again=true to program them again, as Linux's resume does)"))
        } else if p.until == Until::Sdma && p.sdma_runs > 0 && !p.again {
            Err(String::from("the SDMA stage ran in this boot already: SDMA 0 runs its ring (again=true to stop and start it again, as Linux's resume does)"))
        } else if p.until == Until::GfxRing && p.mes_up {
            Err(String::from("MES runs (until=mes or gfx_mes passed in this boot): the direct ring 0 of until=gfx_ring needs MES halted; until=stop first (it unmaps the queues and halts MES and the CP)"))
        } else if p.until == Until::GfxRing && p.gfx_ring_runs > 0 && !p.again {
            Err(String::from("the GFX ring stage ran in this boot already: the CP runs ring 0 (again=true to stop it as until=stop does and start it again from the PSP)"))
        } else if p.until == Until::Vm && !p.mes_queues {
            Err(String::from("until=vm continues a passed until=gfx_mes of this boot (MES running with the compute ring and GFX ring 0 mapped; SDMA 0 kept by an until=sdma before it): run until=gfx_mes with keep first"))
        } else if p.until.runs_mes() && p.mes_runs > 0 && !p.again {
            Err(String::from("the MES stage ran in this boot already: MES may run its queues (again=true to halt the CP and MES and start both again, as Linux's resume does)"))
        } else {
            Ok(())
        }
    };
    if p.kept {
        if p.until.runs_gmc() && p.kept_gc_booted {
            // GX-7h: continues on the kept run (GC booted, TMR and ring held).
            return gmc_again();
        }
        return Err(String::from(match p.until {
            Until::Gart => "a ring is kept from an earlier run but GC did not boot in it: until=gart continues only after a passed until=gfx (until=stop first)",
            Until::Sdma => "a ring is kept from an earlier run but GC did not boot in it: until=sdma continues only after a passed until=gfx (until=stop first)",
            Until::GfxRing => "a ring is kept from an earlier run but GC did not boot in it: until=gfx_ring continues only after a passed until=gfx (until=stop first)",
            Until::Mes | Until::GfxMes => "a ring is kept from an earlier run but GC did not boot in it: until=mes and gfx_mes continue only after a passed until=gfx (until=stop first)",
            Until::Vm => "a ring is kept from an earlier run but GC did not boot in it: until=vm continues only a passed until=gfx_mes (until=stop first)",
            _ => "a ring is kept from an earlier run: until=stop first",
        }));
    }
    if p.until == Until::Vm {
        return Err(String::from("until=vm continues a kept, passed until=gfx_mes of this boot (no ring is kept now): until=sdma and until=gfx_mes with keep first"));
    }
    if p.until.loads_fw() && p.m2_runs > 0 && !p.again {
        return Err(String::from("M2 ran in this boot already: a second firmware load in one power cycle has no Linux model (again=true to do it anyway)"));
    }
    if p.until.runs_gc() && p.gfx_runs > 0 && !p.again {
        return Err(String::from("the GC stage ran in this boot already: EnableGfxImu on a booted GC has no Linux model but resume (again=true to do it anyway)"));
    }
    gmc_again()
}

/// The picture before and after: the DCN snapshot unchanged and no display write counted. `(pass, lines)`.
///
/// GX-7m2 (29.10.: one bitscomp flip during a gx_ring run read as 4 display writes and a changed surface address):
/// the writes are attributed by owner (`dcn::Owner`). Every write that is not the compositor's own page flip or
/// cursor update counts and must be 0, as before; the compositor's are listed on a line of their own. In the
/// snapshot only the registers the compositor's own writes explain may differ, and only on the pipes it touched
/// during the run: a flip's HUBP surface address (and _HIGH), its flip control, and the hardware's copy of the
/// address the HUBP took (SURFACE_EARLIEST_INUSE and _HIGH); a cursor update's CURSOR0_/CNVC_CUR registers. They
/// are listed as the compositor's; every other difference fails the run as before (GX-D9).
pub fn picture(before: alloc::collections::BTreeMap<u32, u32>, after: &alloc::collections::BTreeMap<u32, u32>, m0: &dcn::Marks, m1: &dcn::Marks) -> (bool, Vec<String>) {
    let reference = snapshot::Reference { regs: before, scope: snapshot::Scope::Snapshot { wide: false } };
    let mut cmp = snapshot::compare(&reference, after, false);
    let others = m0.others_until(m1);
    let flips = m1.flips.wrapping_sub(m0.flips);
    let cursor = m1.cursor.wrapping_sub(m0.cursor);
    let flipped: Vec<u32> = m0.pipes_until(m1, true).collect();
    let moved: Vec<u32> = m0.pipes_until(m1, false).collect();
    use crate::layout as lay;
    let by_flip = |b: u32| flipped.iter().any(|&p| [lay::HUBP_ADDR, lay::HUBP_ADDR_HI, lay::HUBP_FLIP, lay::HUBP_INUSE, lay::HUBP_INUSE_HI].iter().any(|&r| b == lay::hubp(p, r)));
    let by_cursor = |b: u32| snapshot::cursor_pipe(b).is_some_and(|p| moved.contains(&u32::from(p)));
    let (theirs, ours): (Vec<snapshot::Diff>, Vec<snapshot::Diff>) = cmp.differ.drain(..).partition(|d| by_flip(d.byte) || by_cursor(d.byte));
    cmp.differ = ours;
    let mut lines = alloc::vec![format!("display writes during the run: {others} (must be 0)")];
    if flips + cursor > 0 {
        let pipes = |v: &[u32]| if v.is_empty() { String::from("no pipe named") } else { v.iter().map(|p| format!("pipe {p}")).collect::<Vec<_>>().join(", ") };
        lines.push(format!(
            "display writes of the compositor during the run (not this run's, not counted): {flips} by its page flips ({}), {cursor} by its cursor updates ({})",
            pipes(&flipped),
            pipes(&moved)
        ));
    }
    lines.push(format!("scanout (DCN snapshot before/after): {}", snapshot::render(&cmp).trim_end()));
    if !theirs.is_empty() {
        let r = snapshot::Report { compared: theirs.len(), differ: theirs, ..snapshot::Report::default() };
        lines.push(format!("scanout: changed by the compositor's own writes (not counted): {}", snapshot::render(&r).trim_end().replacen("differ: ", "", 1)));
    }
    (cmp.equal() && others == 0, lines)
}

/// The first carve-out range of `forbid` that meets `r`.
pub fn meets(r: Range<u64>, forbid: &[Range<u64>]) -> Option<Range<u64>> {
    forbid.iter().find(|f| f.start < r.end && r.start < f.end).cloned()
}

#[inline(never)]
fn run(g: &Gpu, until: Until, args: Args, lines: &mut Vec<String>) -> Result<bool, ()> {
    let Some(mut live) = LIVE.lock_timeout(WAIT_NS) else {
        return refuse(lines, String::from("busy: another run holds the PSP"));
    };
    let kept = live.as_ref().map(|l| l.gc == GcFate::Booted);
    let pre = pre_now(until, args.again, kept, g.display.as_ref().map(|_| ()).map_err(|e| format!("{e}")), g.bos.is_some(), false);
    if let Err(why) = preflight(&pre) {
        return refuse(lines, why);
    }
    let running = live.as_mut().is_some_and(|l| l.mes.is_none() && l.gfx.is_some_and(|a| gfxring::cp_running(&mut l.owner, &a)));
    if restart_by_stop(until, args.again, running) {
        drop(live);
        return restart(g, until, args, lines);
    }
    if live.is_some() {
        // Every continue on a kept, GC-booted run gets here (preflight): `until=gart`/`sdma`/`gfx_ring`, and also
        // `gfx_mes` and `vm` through `gart_continue`; the store gives `until=vm` its `ack` (default: parse_args).
        VM_ACK_LINUX.store(args.ack_linux, Ordering::Release);
        return gart_continue(g, &mut live, until, args.variant, lines);
    }
    fresh(g, until, args, &mut live, lines)
}

/// GX-7m2: a `gfx_ring` with `again` while the kept run's CP still runs GFX ring 0 directly (a passed e1b/e1c, no
/// stop since, MES not running) restarts through a whole stop. On 29.10. the restart on the running CP (halted
/// first, the pointers reset, the tail and the start again) failed the ring test 2 of 3 times (`rptr 0`, the CP
/// never fetched; plan/hw/gx-780m-minipc-2026-10-29/w28/l2-pre.txt); `until=stop` and then the same call passed
/// every time (three rounds, three Reloads). Linux has no restart of a running direct ring either.
pub fn restart_by_stop(until: Until, again: bool, cp_runs_direct_ring: bool) -> bool {
    until == Until::GfxRing && again && cp_runs_direct_ring
}

/// GX-7m2: the restart: `until=stop` (`stop_kept`: the CP halted, its doorbell off, SDMA, GC unloaded toward
/// PMFW, the PSP's DESTROY_TMR/DESTROY_RINGS), then, only when the stop passed, the run from the PSP on as after
/// a manual stop.
#[inline(never)]
fn restart(g: &Gpu, until: Until, args: Args, lines: &mut Vec<String>) -> Result<bool, ()> {
    lines.push(String::from(
        "gfx_ring again: GFX ring 0 of the earlier run still runs on the CP: stopped first as until=stop does (a restart on the running CP failed the ring test 2 of 3 times on 29.10.; after a stop it passed every time), then started from the PSP",
    ));
    match stop_kept(g, lines) {
        Ok(true) => lines.push(String::from("gfx_ring again: the stop passed; the run starts from the PSP")),
        r => {
            lines.push(String::from("gfx_ring again: the stop before the restart did not pass (see its lines): nothing was started"));
            return r.map(|_| false);
        }
    }
    // The stop wrote: a refusal from here on is a FAIL with its reason, not "nothing written".
    let Some(mut live) = LIVE.lock_timeout(WAIT_NS) else {
        lines.push(String::from("gfx_ring again: busy after the stop (another run holds the PSP): nothing started"));
        return Ok(false);
    };
    let pre = pre_now(until, args.again, live.as_ref().map(|l| l.gc == GcFate::Booted), g.display.as_ref().map(|_| ()).map_err(|e| format!("{e}")), g.bos.is_some(), false);
    if let Err(why) = preflight(&pre) {
        lines.push(format!("gfx_ring again: refused after the stop: {why}"));
        return Ok(false);
    }
    if live.is_some() {
        lines.push(String::from("gfx_ring again: a run was kept again after the stop (another caller): nothing started"));
        return Ok(false);
    }
    fresh(g, until, args, &mut live, lines)
}

/// A run with no kept one: discovery, firmware, the engine from the PSP on. Its own frame (GX-7m2): the locals are
/// large and a continue (`gart_continue`, e.g. `until=vm`) must not carry them on MCP's 64 KiB thread.
#[inline(never)]
fn fresh(g: &Gpu, until: Until, args: Args, live: &mut Option<Live>, lines: &mut Vec<String>) -> Result<bool, ()> {
    let (Ok(a), Some(bos)) = (&g.display, &g.bos) else {
        return refuse(lines, String::from("display or BO manager gone"));
    };
    // The discovery table of this boot (the carve-out's top is closed to the CPU: the indirect copy).
    let found = probe::kept().map(Ok).unwrap_or_else(|| probe::fetch_indirect(g.regs));
    let d = match found.and_then(|f| disc::parse(&f.bytes).map_err(|w| format!("{w}"))) {
        Ok(d) => d,
        Err(e) => return refuse(lines, format!("no discovery table: {e}")),
    };
    let Some(addrs) = Addrs::from_discovery(&d) else {
        return refuse(lines, String::from("no MP0 segment 1 in the discovery table: PSP registers unknown"));
    };
    let gfx = match (until.runs_gc(), GfxInputs::from_discovery(&d)) {
        (true, None) => return refuse(lines, String::from("no MP1/NBIF/GC/SMUIO bases in the discovery table: SMU and GC registers unknown")),
        (true, g) => g,
        _ => None,
    };
    // GX-7j3 fixer: the unload's registers come from discovery, not from this run's GC input: a later run
    // without a GC stage (until=psp) must still unload a GC an earlier stop left `Booted` (its unload refused).
    let un_addrs = GfxInputs::from_discovery(&d).map(|g| handoff::UnloadAddrs { smu: g.smu, gc: g.gc });
    let (st, _) = crate::state::classify_live(g.regs, &d);
    lines.push(format!("state: {}", st.start.name()));
    for w in &st.why {
        lines.push(format!("state why: {w}"));
    }
    if let Err(why) = pspbringup::admit(st.start) {
        return refuse(lines, String::from(why));
    }
    let win = match carve_window(g, a, &d) {
        Ok(w) => w,
        Err(e) => return refuse(lines, e),
    };
    let gmc_in = if until.runs_gmc() {
        match gmc_inputs(&d, &win) {
            Ok(m) => Some(m),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };
    let opts = ListOpts::PSP13_0_4;
    let plan = ipmod::plan(&d);
    let (fw, notes) = match fwload::load(&plan.files(), &mut StickSource, &opts) {
        Ok(x) => x,
        Err(e) => return refuse(lines, format!("firmware: {e:?} (the stick's /boot/BITSOS2/firmware)")),
    };
    let list = match fw.load_list(&opts) {
        Ok(l) => l,
        Err(e) => return refuse(lines, format!("firmware list: {e:?}")),
    };
    lines.push(format!("firmware: {} files, {} images to load ({} needs skipped, {} fallbacks)", fw.files.len(), list.len(), notes.skipped.len(), notes.fallbacks.len()));
    BURST_NOP.store(if sdma::burst_nop(&fw) { 2 } else { 1 }, Ordering::Release);
    let sys = LiveSys;
    let sdma_in = if until == Until::Sdma {
        match sdma_inputs(&d, &win, &sys) {
            Ok(s) => Some(s),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };
    let gfx_in = if until.runs_gfx_stage() {
        match gfx_inputs(&d, &win, until, args.variant, lines) {
            Ok(x) => Some(x),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };
    // GX-7k: the MES firmware starts of this load, kept for a later run that continues on the kept one.
    *MES_UC.lock() = fw.mes_uc_starts();
    let mes_in = if until.runs_mes() {
        match mes_inputs(&d, &win) {
            Ok(x) => Some(x),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };

    // The picture before: the DCN snapshot and the display's write count.
    let before = snapshot::take(g.regs, false);
    let dcn0 = dcn::marks();
    let crc0 = crc_now(g.regs, a);
    lines.extend(dma_lines(g.regs, "before"));
    let had_master = g.regs.pci().command() & 0x4 != 0;
    register_quiesce();
    if had_master && !BUS_MASTER_OURS.load(Ordering::Acquire) && inherited_note().is_some_and(|n| n.bus_master_left_on) && !BUS_MASTER_INHERITED.swap(true, Ordering::AcqRel) {
        BUS_MASTER_OURS.store(true, Ordering::Release);
        lines.push(String::from("bus master: found on, left by the kernel before (its note): this driver switches it off again"));
    }
    // Recorded before the switch: a failure half-way (some bridges set) is still ours to set back.
    if !had_master {
        BUS_MASTER_OURS.store(true, Ordering::Release);
    }
    match g.regs.pci().enable_device(true) {
        Ok(e) => {
            lines.push(format!("bus master: on (command {:#06x}; bridges switched on: {})", e.command, e.bridges_bus_master.iter().map(|l| format!("{l}")).collect::<Vec<_>>().join(" ")));
        }
        Err(e) => {
            lines.push(format!("enable_device failed: {e}"));
            bus_master_back(g.regs, lines);
            return refuse(lines, String::from("bus master could not be switched on: no PSP register written"));
        }
    }
    lines.extend(dma_lines(g.regs, "after enable"));

    // Everything in use before the run but the boot's PSP pool: protected by the watcher, never mapped by the
    // PSP's pieces.
    let Some(pool) = POOL.lock_timeout(WAIT_NS) else {
        bus_master_back(g.regs, lines);
        return refuse(lines, String::from("busy: the PSP buffer pool is locked"));
    };
    let forbid: Vec<Range<u64>> = bos.lock().used().filter(|r| !pool.holds(r)).collect();
    drop(pool);
    let mut guard = DisplayGuard::from_discovery(&d);
    guard.vram_mc = Some(win.mc_base);
    guard.protect(a.a0, a.bytes);
    guard.protect(a.a1, a.bytes);
    for r in &forbid {
        guard.protect(win.mc_base + r.start, r.end - r.start);
    }
    let lists = gmc_in.as_ref().map(|m| gmc_lists(m, &guard));
    let mut hw = PspHw::with_gmc(g.regs, &d, &addrs, gfx.as_ref(), lists, win, forbid);
    if let Some(s) = &sdma_in {
        hw.allow_sdma(&s.addrs, bar2(g));
    }
    if let Some(x) = &gfx_in {
        hw.allow_gfx(&x.addrs, bar2(g));
    }
    if let Some(m) = &mes_in {
        hw.allow_mes(&m.addrs, bar2(g));
    }
    let mut owner = Bringup::new(Watch::new(hw, guard, Mode::Block), Gate::Allow, LOG_KEEP);
    let inp = Inputs {
        addrs,
        carve: Carve { mc_base: win.mc_base, phys_base: win.phys_base },
        start: st.start,
        until,
        fw: &fw,
        list: &list,
        autoload: opts.autoload,
        keep: args.keep,
        t: Timeouts::LINUX,
        gfx,
        gmc: gmc_in,
        sdma: sdma_in,
        gfx_ring: gfx_in,
        mes: mes_in,
        vm: None,
        unload_first: pspbringup::unload_first(st.start, current_note().is_some_and(|n| n.gc == GcFate::Unloaded)),
    };
    let gfx_variant = inp.gfx_ring.as_ref().map(|x| x.variant).filter(|_| until == Until::GfxRing);
    let gc_before = gc_now();
    let mut out = pspbringup::run(&mut owner, &mut SharedBos(bos), &inp);
    if out.gfx.is_some() {
        GFX_RUNS.fetch_add(1, Ordering::AcqRel);
        GC_FAILED.store(pspbringup::gc_latch(GC_FAILED.load(Ordering::Acquire), &out), Ordering::Release);
    }
    if until.loads_fw() && out.report.steps.iter().any(|s| matches!(s, crate::gpucore::psp::Step::Load(_))) {
        M2_RUNS.fetch_add(1, Ordering::AcqRel);
    }
    if out.needs_reboot() {
        REBOOT_NEEDED.store(true, Ordering::Release);
    }
    if let Some(m) = &out.gmc {
        GMC_RUNS.fetch_add(1, Ordering::AcqRel);
        keep_table(m.table);
        *GART_UP.lock() = if m.passed() { inp.gmc.as_ref().and_then(|gi| pspbringup::gart_use(m, gi)) } else { None };
    }
    let sdma_ran = out.sdma.as_ref().map(|s| {
        SDMA_RUNS.fetch_add(1, Ordering::AcqRel);
        note_sdma(s);
        keep_sdma_bo(s.bo);
        s.left_running
    });
    let gfx_ran = out.gfx_ring.as_ref().map(|x| {
        GFX_RING_RUNS.fetch_add(1, Ordering::AcqRel);
        note_gfx(x);
        keep_gfx_bo(x.bo);
        x.left_running
    });
    let mes_up = out.mes.as_ref().and_then(|m| {
        MES_RUNS.fetch_add(1, Ordering::AcqRel);
        note_mes(m);
        keep_mes_bo(m.bo);
        let a = inp.mes.as_ref()?.addrs;
        // A failed halt counts too: until=stop tries again (without unmapping: MES did not pass).
        (m.left_running || m.halt_failed.is_some()).then(|| (a, m.layout.map(|l| l.bo_mc), m.queues && m.left_running))
    });
    if !out.mes_pre.is_empty() {
        *MES_UP.lock() = None;
    }
    if out.mes_pre_stranded {
        MES_STRANDED.store(true, Ordering::Release);
    }
    lines.extend(pspbringup::render(&out, &inp));
    let passed = out.passed();
    let quarantined_now = out.quarantined.is_some();
    if let Some(p) = out.quarantined.take() {
        quarantine(p, lines);
    }
    let gc = match &out.gfx {
        Some(x) if x.gc_touched && x.error.is_none() => GcFate::Booted,
        Some(x) if x.gc_touched => GcFate::Unsettled,
        _ => gc_before,
    };
    let touched_rings = out.report.steps.iter().any(|s| matches!(s, crate::gpucore::psp::Step::RingCreated { .. } | crate::gpucore::psp::Step::StaleRingDestroyed { .. }));
    let sdma_addrs = sdma_ran.and(inp.sdma.as_ref().map(|s| s.addrs));
    let gfx_addrs = gfx_ran.and(inp.gfx_ring.as_ref().map(|x| x.addrs));
    if let Some(psp) = out.kept.take() {
        let n = note_of(&mut owner, &addrs, RingFate::NotDestroyed, gc, false);
        set_note(n);
        lines.push(bus_line(BusChoice::Kept, None));
        *MES_UP.lock() = mes_up;
        *live = Some(Live { owner, psp, gc, addrs, sdma: sdma_addrs, gfx: gfx_addrs, mes: mes_up, un: un_addrs });
    } else {
        // No kept run to halt them later: MES, the CP and SDMA 0 are halted now (an engine must never outlive its
        // owner); MES first, while the CP still runs the queues it unmaps.
        if let Some((ma, bo_mc, queues)) = mes_up {
            let st = mes::stop(&mut owner, &ma, bo_mc, queues);
            note_stranded(&st.stranded);
            lines.extend(st.lines);
            lines.push(format!("mes: stopped now (no kept run holds it): {}", st.ok));
        }
        if let Some(xa) = gfx_addrs {
            let ok = gfxring::halt(&mut owner, &xa);
            lines.push(format!("gfx: the CP and MEC halted now (no kept run holds it): {ok}"));
            if ok {
                gfx_back(&mut owner, &xa, sdma_addrs.is_none(), lines);
            }
        }
        if let Some(sa) = sdma_addrs {
            let ok = sdma::halt(&mut owner, &sa);
            lines.push(format!("sdma: halted now (no kept run holds it): {ok}"));
            if ok {
                sdma_back(&mut owner, &sa, lines);
            }
        }
        bus_master_back(g.regs, lines);
        if touched_rings || gc != gc_before {
            let ring = if quarantined_now { RingFate::NotDestroyed } else { RingFate::Destroyed };
            set_note(note_of(&mut owner, &addrs, ring, gc, !quarantined_now));
        }
    }
    lines.extend(dma_lines(g.regs, "after the run"));

    // The picture after: nothing of the display may have changed.
    let after = snapshot::take(g.regs, false);
    let (same, pl) = picture(before.regs, &after.regs, &dcn0, &dcn::marks());
    lines.extend(pl);
    let (crc_same, cl) = crc_compare(&crc0, &crc_now(g.regs, a));
    lines.extend(cl);
    let pass = verdict(until, passed, same, crc_same);
    if let Some(v) = gfx_variant {
        lines.push(gfx_verdict_line(v, pass));
    }
    if until.runs_mes() {
        lines.push(mes_verdict_line(until, pass));
    }
    Ok(pass)
}

/// GX-7k: the run's last line for until=mes / gfx_mes (the whole run: stages, picture, CRC).
pub fn mes_verdict_line(until: Until, pass: bool) -> String {
    format!("mes verdict ({}): {}", if until == Until::GfxMes { "gfx_mes" } else { "mes" }, if pass { "PASS" } else { "FAIL" })
}

/// GX-7j2: the run's last line names the variant (the whole run: stages, picture, CRC).
pub fn gfx_verdict_line(v: gfxring::Variant, pass: bool) -> String {
    format!("gfx_ring verdict ({}): {}", v.name(), if pass { "PASS" } else { "FAIL" })
}

/// The run's verdict: the stages passed and the picture (DCN snapshot) unchanged; GX-7h: the OTG CRC is a pass
/// rule of the GMC stage (the riskiest step for the picture) and (GX-7i) of the SDMA stage; earlier milestones
/// only print it.
pub fn verdict(until: Until, stages: bool, picture_same: bool, crc_same: bool) -> bool {
    stages && picture_same && (crc_same || !until.runs_gmc())
}

/// GX-7h: the hubs and the carve-out for the GMC stage (the table of an earlier run of this boot reused).
fn gmc_inputs(d: &Discovery, win: &CarveWin) -> Result<GmcInputs, String> {
    let carve = amdgpu_fmt::addr::Carveout::new(win.mc_base, win.phys_base, win.size).ok_or_else(|| format!("carve-out {win:?} is not page aligned"))?;
    let mut m = GmcInputs::new(d, carve).ok_or_else(|| String::from("no GC/MMHUB bases in the discovery table: hub registers unknown"))?;
    // A blocking lock: it is held only for a copy of the handle, and a busy lock must never make the stage
    // allocate a second table (the first would leak and the hubs could point at an untracked table).
    m.table = *GART_TABLE.lock();
    Ok(m)
}

fn keep_table(t: Option<TableBo>) {
    if let Some(t) = t {
        *GART_TABLE.lock() = Some(t);
    }
}

fn keep_sdma_bo(b: Option<SdmaBo>) {
    if let Some(b) = b {
        *SDMA_BO.lock() = Some(b);
    }
}

fn keep_mes_bo(b: Option<MesBo>) {
    if let Some(b) = b {
        *MES_BO.lock() = Some(b);
    }
}

/// GX-7k: after a MES stage: the aperture as this boot's first doorbell stage found it, and whether it is on by
/// this driver now (MES turns it on: its rings are doorbell-driven).
fn note_mes(r: &mes::MesReport) {
    note_stranded(&r.stranded);
    if r.queues && r.left_running {
        MES_STRANDED.store(false, Ordering::Release);
    }
    let first = keep_aper_first(&mut APER_FIRST.lock(), r.aper_found());
    if aperture_ours(first, r.aper.1) {
        APERTURE_OURS.store(true, Ordering::Release);
    }
}

/// GX-7k: MES's registers, its buffer of an earlier run, the firmware starts of this boot's load, MES left by an
/// earlier run (stopped first), the Quiesce arming.
fn mes_inputs(d: &Discovery, win: &CarveWin) -> Result<MesInputs<'static>, String> {
    let carve = amdgpu_fmt::addr::Carveout::new(win.mc_base, win.phys_base, win.size).ok_or_else(|| format!("carve-out {win:?} is not page aligned"))?;
    let uc = *MES_UC.lock();
    let x = MesInputs::new(d, carve, uc).ok_or_else(|| String::from("no GC bases in the discovery table: MES registers unknown"))?;
    if x.addrs.aper_en.is_none() {
        return Err(String::from("no NBIF segment 2 in the discovery table: the doorbell aperture is unknown"));
    }
    let prev = (*MES_UP.lock()).map(|(_, bo, q)| (bo, q));
    Ok(MesInputs { bo: *MES_BO.lock(), arm: Some(&arm_mes), prev, ..x })
}

fn keep_gfx_bo(b: Option<GfxBo>) {
    if let Some(b) = b {
        *GFX_BO.lock() = Some(b);
    }
}

/// GX-7j: GC's CP registers, the GFX buffer of an earlier run, the Quiesce arming; GX-7j2: the variant (its line
/// says which and whether it was the default).
fn gfx_inputs(d: &Discovery, win: &CarveWin, until: Until, variant: Option<gfxring::Variant>, lines: &mut Vec<String>) -> Result<GfxRingInputs<'static>, String> {
    let carve = amdgpu_fmt::addr::Carveout::new(win.mc_base, win.phys_base, win.size).ok_or_else(|| format!("carve-out {win:?} is not page aligned"))?;
    let x = GfxRingInputs::new(d, carve).ok_or_else(|| String::from("no GC bases in the discovery table: CP registers unknown"))?;
    if x.addrs.aper_en.is_none() {
        return Err(String::from("no NBIF segment 2 in the discovery table: the doorbell aperture is unknown"));
    }
    let v = variant.unwrap_or(gfxring::Variant::DEFAULT);
    if until == Until::GfxRing {
        lines.push(format!("gfx_ring variant: {}{}", v.name(), if variant.is_none() { " (the default)" } else { "" }));
    } else {
        lines.push(String::from("gfx stage: Linux 7.1's cp_resume order before MES (mes)"));
    }
    Ok(GfxRingInputs { bo: *GFX_BO.lock(), arm: Some(&arm_gfx), variant: v, ..x })
}

/// GX-7i: the doorbell BAR's address (BAR2 of the function; `None` when it is not a memory BAR of a page or more).
fn bar2(g: &Gpu) -> Option<u64> {
    match g.regs.pci().bar(2) {
        Some(pci::Bar::Mem { phys, size, .. }) if size >= PAGE_4K && phys != 0 => Some(phys),
        _ => None,
    }
}

/// GX-7i: SDMA 0, its buffer of an earlier run, the firmware's burst NOP and rung 3's system pages.
fn sdma_inputs<'a>(d: &Discovery, win: &CarveWin, sys: &'a LiveSys) -> Result<SdmaInputs<'a>, String> {
    let carve = amdgpu_fmt::addr::Carveout::new(win.mc_base, win.phys_base, win.size).ok_or_else(|| format!("carve-out {win:?} is not page aligned"))?;
    let burst = BURST_NOP.load(Ordering::Acquire) == 2;
    let s = SdmaInputs::new(d, carve, burst).ok_or_else(|| String::from("no GC bases in the discovery table: SDMA registers unknown"))?;
    if s.addrs.nbio.is_none() {
        return Err(String::from("no NBIF segments 1-3 in the discovery table: the doorbell side is unknown"));
    }
    Ok(SdmaInputs { bo: *SDMA_BO.lock(), sys: Some(sys), arm: Some(&arm_sdma), ..s })
}

/// GX-7i: rung 3's system pages: MM-14 `DmaPages` (write-back, snooped), pinned for the device while held.
struct LiveSys;

struct LivePages(mm::dma_pages::DevicePin);

impl SysAlloc for LiveSys {
    fn alloc(&self, pages: u64) -> Result<alloc::boxed::Box<dyn SysPages>, String> {
        let p = mm::dma_pages::DmaPages::alloc(pages, mm::dma_pages::DmaPagesOptions::default()).map_err(|e| format!("DmaPages: {e:?}"))?;
        Ok(alloc::boxed::Box::new(LivePages(p.pin_for_device())))
    }
}

impl SysPages for LivePages {
    fn pages(&self) -> u64 {
        self.0.pages().pages()
    }
    fn device_addr(&self, page: u64) -> Option<u64> {
        self.0.pages().device_addr(page)
    }
    fn read(&mut self, off: u64, out: &mut [u8]) -> bool {
        self.0.pages().read(off, out)
    }
    fn write(&mut self, off: u64, data: &[u8]) -> bool {
        self.0.pages().write(off, data)
    }
    fn barrier(&mut self) {
        self.0.pages().write_barrier();
    }
}

/// GX-7h: `until=gart` after a passed `until=gfx`: the GMC stage on the kept owner (GC booted, TMR and ring held).
/// GX-7i: `until=sdma` the same, then the SDMA stage; the GMC stage only when this boot has no passed one.
#[inline(never)]
fn gart_continue(g: &Gpu, live: &mut Option<Live>, until: Until, variant: Option<gfxring::Variant>, lines: &mut Vec<String>) -> Result<bool, ()> {
    let (Ok(a), Some(bos), Some(l)) = (&g.display, &g.bos, live.as_mut()) else {
        return refuse(lines, String::from("display, BO manager or the kept run gone"));
    };
    let found = probe::kept().map(Ok).unwrap_or_else(|| probe::fetch_indirect(g.regs));
    let d = match found.and_then(|f| disc::parse(&f.bytes).map_err(|w| format!("{w}"))) {
        Ok(d) => d,
        Err(e) => return refuse(lines, format!("no discovery table: {e}")),
    };
    let win = match carve_window(g, a, &d) {
        Ok(w) => w,
        Err(e) => return refuse(lines, e),
    };
    let (gm, gi) = match (gmc_inputs(&d, &win), GfxInputs::from_discovery(&d)) {
        (Ok(m), Some(gi)) => (m, gi),
        (Err(e), _) => return refuse(lines, e),
        (_, None) => return refuse(lines, String::from("no GC bases in the discovery table")),
    };
    let sys = LiveSys;
    let sdma_in = if until == Until::Sdma {
        match sdma_inputs(&d, &win, &sys) {
            Ok(s) => Some(s),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };
    let gfx_in = if until.runs_gfx_stage() {
        match gfx_inputs(&d, &win, until, variant, lines) {
            Ok(x) => Some(x),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };
    let mes_in = if until.runs_mes() {
        match mes_inputs(&d, &win) {
            Ok(x) => Some(x),
            Err(e) => return refuse(lines, e),
        }
    } else {
        None
    };
    // GC must still be booted (GFXHUB lives in it; GFXOFF stays disallowed, but check).
    let (cp, boot) = (l.owner.reg_read(gi.gc.cp_stat), l.owner.reg_read(gi.gc.bootload));
    let booted = cp == Some(0) && boot.is_some_and(|b| b != u32::MAX && b & gfxup::BOOTLOAD_COMPLETE != 0);
    lines.push(format!("{}: continuing on the kept run (GC booted, TMR and ring held); CP_STAT {cp:x?} RLC_RLCS_BOOTLOAD_STATUS {boot:x?}", match until {
        Until::Sdma => "sdma",
        Until::GfxRing => "gfx_ring",
        Until::Mes => "mes",
        Until::GfxMes => "gfx_mes",
        Until::Vm => "vm",
        _ => "gart",
    }));
    if !booted {
        return refuse(lines, String::from("GC is not booted now: no hub register written"));
    }
    let gart_up = *GART_UP.lock();
    let run_gmc = until == Until::Gart || gart_up.is_none();
    if run_gmc {
        let (r, w) = gmc_lists(&gm, l.owner.hw().guard());
        l.owner.hw().hw().allow_gmc(r, w);
    }
    if let Some(s) = &sdma_in {
        l.owner.hw().hw().allow_sdma(&s.addrs, bar2(g));
    }
    if let Some(x) = &gfx_in {
        l.owner.hw().hw().allow_gfx(&x.addrs, bar2(g));
    }
    if let Some(m) = &mes_in {
        l.owner.hw().hw().allow_mes(&m.addrs, bar2(g));
    }
    let before = snapshot::take(g.regs, false);
    let dcn0 = dcn::marks();
    let crc0 = crc_now(g.regs, a);
    let (seq0, v0) = (l.owner.log().totals().writes, l.owner.hw().total());
    let granted0 = l.owner.hw().granted().len();
    let mut pass = true;
    let mut gu = gart_up;
    if run_gmc {
        let rep = gmc::enable(&mut l.owner, &mut pspbringup::Pieces(&mut SharedBos(bos)), &gm);
        GMC_RUNS.fetch_add(1, Ordering::AcqRel);
        keep_table(rep.table);
        gu = if rep.passed() { pspbringup::gart_use(&rep, &gm) } else { None };
        *GART_UP.lock() = gu;
        lines.extend(gmc::render(&rep));
        for c in pspbringup::gmc_checks(Some(&rep)) {
            lines.push(format!("{}: {} ({})", c.name, if c.pass { "PASS" } else { "FAIL" }, c.why));
        }
        pass &= rep.passed();
    } else {
        lines.push(String::from("gart: this boot's GART stands (a passed GMC stage): the hubs are not programmed again"));
    }
    let mut resumes = Vec::new();
    if let Some(s) = &sdma_in {
        let rep = match gu {
            Some(gu) if pass => {
                let rep = sdma::enable(&mut l.owner, &mut pspbringup::Pieces(&mut SharedBos(bos)), s, &gu);
                SDMA_RUNS.fetch_add(1, Ordering::AcqRel);
                note_sdma(&rep);
                keep_sdma_bo(rep.bo);
                l.sdma = Some(s.addrs);
                resumes = rep.resumes.iter().map(|&(db, _, x, y)| (db, (x - seq0) as usize, (y - seq0) as usize)).collect();
                lines.extend(sdma::render(&rep));
                Some(rep)
            }
            _ => None,
        };
        for c in pspbringup::sdma_checks(rep.as_ref()) {
            lines.push(format!("{}: {} ({})", c.name, if c.pass { "PASS" } else { "FAIL" }, c.why));
        }
        pass &= rep.as_ref().is_some_and(sdma::SdmaReport::passed);
    }
    let mut gmarks = Vec::new();
    // GX-7m: a later GFX or MES stage remaps the queues and may reuse the MES buffer: the test VM goes first.
    if pass && (gfx_in.is_some() || mes_in.is_some()) {
        vm_teardown(&mut l.owner, "gfx", lines);
    }
    // GX-7k: a MES that runs (an earlier run's, or Linux's) is stopped before the GFX stage halts the CP.
    if let Some(m) = &mes_in
        && pass
        && let Some(st) = mes::pre_stop(&mut l.owner, m)
    {
        note_stranded(&st.stranded);
        lines.extend(st.lines);
        l.mes = None;
        *MES_UP.lock() = None;
        pass &= st.ok;
    }
    let mut gfx_rep = None;
    if let Some(x) = &gfx_in {
        let rep = match gu {
            Some(_) if pass => {
                let rep = gfxring::enable(&mut l.owner, &mut pspbringup::Pieces(&mut SharedBos(bos)), &pspbringup::gfx_stage_inputs(x, until), &gm.addrs.gfx);
                GFX_RING_RUNS.fetch_add(1, Ordering::AcqRel);
                note_gfx(&rep);
                keep_gfx_bo(rep.bo);
                l.gfx = Some(x.addrs);
                gmarks = pspbringup::gfx_marks(&rep, seq0);
                lines.extend(gfxring::render(&rep));
                Some(rep)
            }
            _ => None,
        };
        for c in pspbringup::gfx_ring_checks(rep.as_ref()) {
            lines.push(format!("{}: {} ({})", c.name, if c.pass { "PASS" } else { "FAIL" }, c.why));
        }
        pass &= rep.as_ref().is_some_and(gfxring::GfxReport::passed);
        gfx_rep = rep;
    }
    if let Some(m) = &mes_in {
        let rep = match gfx_rep.as_ref() {
            Some(gr) if pass => {
                let mi = MesInputs { queues: until == Until::GfxMes, ..*m };
                let rep = mes::enable(&mut l.owner, &mut pspbringup::Pieces(&mut SharedBos(bos)), &mi, &gm.addrs.gfx, gr.layout.as_ref());
                MES_RUNS.fetch_add(1, Ordering::AcqRel);
                note_mes(&rep);
                keep_mes_bo(rep.bo);
                let up = (rep.left_running || rep.halt_failed.is_some()).then(|| (m.addrs, rep.layout.map(|x| x.bo_mc), rep.queues && rep.left_running));
                l.mes = up;
                *MES_UP.lock() = up;
                gmarks.extend(rep.marks.iter().map(|&(n, a, b)| (n, (a - seq0) as usize, (b - seq0) as usize)));
                lines.extend(mes::render(&rep));
                Some(rep)
            }
            _ => None,
        };
        for c in pspbringup::mes_checks(rep.as_ref()) {
            lines.push(format!("{}: {} ({})", c.name, if c.pass { "PASS" } else { "FAIL" }, c.why));
        }
        pass &= rep.as_ref().is_some_and(mes::MesReport::passed);
    }
    if until == Until::Vm {
        // GX-7m: the test VM on the queues the kept gfx_mes mapped and SDMA 0's kept queue.
        let rep = match vm_inputs(&d, &win, l.sdma) {
            Ok(vi) if pass => {
                l.owner.hw().hw().allow_gmc(vi.addrs.readable(), vi.addrs.writable());
                if let Some(s) = &vi.sdma {
                    l.owner.hw().hw().allow_sdma(&s.addrs, bar2(g));
                }
                l.owner.hw().hw().allow_vm_doorbell(bar2(g));
                let prev = VM_UP.lock().take().map(|(_, u)| u);
                let rep = vmring::enable(&mut l.owner, &vi, prev);
                *VM_UP.lock() = rep.kept.or(rep.retry).map(|u| (vi.addrs, u));
                if rep.stranded {
                    note_stranded(&["a queue of the VM stage (the compute ring or the MES process queue)"]);
                }
                lines.extend(vmring::render(&rep));
                Some(rep)
            }
            Ok(_) => None,
            Err(e) => {
                lines.push(format!("vm: not run: {e}"));
                None
            }
        };
        for c in pspbringup::vm_checks(rep.as_ref()) {
            lines.push(format!("{}: {} ({})", c.name, if c.pass { "PASS" } else { "FAIL" }, c.why));
        }
        pass &= rep.as_ref().is_some_and(vmring::VmReport::passed);
    }
    let violations = l.owner.hw().total() - v0;
    lines.push(format!("watch: {violations} writes to display registers or the scanout blocked"));
    for &(b, v, why) in &l.owner.hw().granted()[granted0..] {
        lines.push(format!("watch allowed: {b:#x} = {v:#x} ({why})"));
    }
    let entries: Vec<_> = l.owner.log().since(seq0).copied().collect();
    lines.push(format!("owner: {} writes in this stage", entries.len()));
    let sa = sdma_in.as_ref().map(|s| s.addrs);
    let xa = gfx_in.as_ref().map(|x| x.addrs);
    for (i, e) in entries.iter().enumerate() {
        lines.extend(pspbringup::sdma_marks(&resumes, i));
        lines.extend(pspbringup::gfx_mark_lines(&gmarks, i));
        lines.push(pspbringup::w_line(e, &pspbringup::reg_name_all(&l.addrs, Some(&gi), Some(&gm), sa.as_ref(), xa.as_ref(), e.byte)));
    }
    lines.extend(pspbringup::sdma_marks(&resumes, entries.len()));
    lines.extend(pspbringup::gfx_mark_lines(&gmarks, entries.len()));
    let after = snapshot::take(g.regs, false);
    let (same, pl) = picture(before.regs, &after.regs, &dcn0, &dcn::marks());
    lines.extend(pl);
    let (crc_same, cl) = crc_compare(&crc0, &crc_now(g.regs, a));
    lines.extend(cl);
    let pass = verdict(until, pass && violations == 0, same, crc_same);
    lines.push(format!("verdict: {}", if pass { "PASS" } else { "FAIL" }));
    if let Some(x) = gfx_in.as_ref().filter(|_| until == Until::GfxRing) {
        lines.push(gfx_verdict_line(x.variant, pass));
    }
    if until.runs_mes() {
        lines.push(mes_verdict_line(until, pass));
    }
    if until == Until::Vm {
        lines.push(vm_verdict_line(pass));
    }
    Ok(pass)
}

/// GX-7m: the VM stage's inputs from this boot's kept buffers: the MES and GFX stages' (gfx_mes), SDMA 0's queue
/// when a kept until=sdma left it (`sdma`: its registers).
fn vm_inputs(d: &Discovery, win: &CarveWin, sdma: Option<SdmaAddrs>) -> Result<vmring::VmInputs, String> {
    let carve = amdgpu_fmt::addr::Carveout::new(win.mc_base, win.phys_base, win.size).ok_or_else(|| format!("carve-out {win:?} is not page aligned"))?;
    let a = VmAddrs::from_discovery(d).ok_or_else(|| String::from("no GC/OSSSYS bases in the discovery table: GFXHUB and the IH LUT unknown"))?;
    let mb = (*MES_BO.lock()).ok_or_else(|| String::from("no MES buffer in this boot (until=gfx_mes first)"))?;
    let gb = (*GFX_BO.lock()).ok_or_else(|| String::from("no GFX buffer in this boot (until=gfx_mes first)"))?;
    let ml = mes::Layout::new(carve.mc_base() + mb.offset).ok_or_else(|| String::from("MES layout"))?;
    let gl = gfxring::Layout::new(carve.mc_base() + gb.offset).ok_or_else(|| String::from("GFX layout"))?;
    let side = match (sdma, *SDMA_BO.lock()) {
        (Some(addrs), Some(sb)) => sdma::Layout::new(carve.mc_base() + sb.offset).map(|layout| vmring::SdmaSide { addrs, layout, burst_nop: BURST_NOP.load(Ordering::Acquire) == 2 }),
        _ => None,
    };
    let token = u64::from(MES_RUNS.load(Ordering::Acquire)) << 32 | u64::from(GMC_RUNS.load(Ordering::Acquire));
    let mut vi = vmring::VmInputs::new(a, carve, mb, ml, gl, side, token);
    vi.ack = if VM_ACK_LINUX.load(Ordering::Acquire) { vmring::AckWait::Linux } else { vmring::AckWait::Helper };
    Ok(vi)
}

/// GX-7m: the run's last line for until=vm (the whole run: stages, picture, CRC).
pub fn vm_verdict_line(pass: bool) -> String {
    format!("vm verdict (vm): {}", if pass { "PASS" } else { "FAIL" })
}

/// The CRC of every OTG a driven pipe uses, read-only: `None` when the CRC is off or unreadable (turning it on
/// would be a display write).
fn crc_now(regs: &DcnRegs, a: &crate::attach::Attach) -> Vec<(u8, Option<u64>)> {
    let mut otgs: Vec<u8> = a.pipes.iter().map(|p| p.otg).collect();
    otgs.sort_unstable();
    otgs.dedup();
    otgs.into_iter().map(|k| (k, crate::flip::crc_read(regs, u32::from(k)).ok().flatten())).collect()
}

/// GX-7h: the OTG CRCs before and after (pure): every CRC that ran before reads the same after. `(pass, lines)`.
pub fn crc_compare(before: &[(u8, Option<u64>)], after: &[(u8, Option<u64>)]) -> (bool, Vec<String>) {
    let mut v = Vec::new();
    let mut pass = true;
    let mut compared = 0;
    for &(k, b) in before {
        let a = after.iter().find(|x| x.0 == k).and_then(|x| x.1);
        match (b, a) {
            (Some(b), Some(a)) => {
                compared += 1;
                pass &= a == b;
                v.push(format!("crc OTG{k}: before {b:#014x} after {a:#014x}: {}", if a == b { "same" } else { "CHANGED (the picture moved, or a console line was drawn)" }));
            }
            (Some(b), None) => {
                pass = false;
                v.push(format!("crc OTG{k}: before {b:#014x}, after off or unreadable"));
            }
            (None, _) => v.push(format!("crc OTG{k}: off (not compared: turning it on is a display write; the DCN snapshot stands)")),
        }
    }
    if before.is_empty() {
        v.push(String::from("crc: no driven OTG"));
    }
    v.push(format!("crc: {compared} compared, {}", if pass { "unchanged" } else { "CHANGED" }));
    (pass, v)
}

/// Errors after which the PSP may still be busy with our frame or never gets ready: a reboot, not a retry.
pub fn needs_reboot(e: &PspError) -> bool {
    pspbringup::needs_reboot(e)
}

/// The carve-out from the registers: CPU base from MMHUB's MMMC_VM_FB_OFFSET (cross-checked with the display's
/// DCN_VM_FB_OFFSET), MC base and size from the attach (FB location, MEMSIZE). Never constants.
fn carve_window(g: &Gpu, a: &crate::attach::Attach, d: &Discovery) -> Result<CarveWin, String> {
    let dcn = probe::dcn_fb_offset(g.regs, true);
    let (src, fb) = probe::fb_offset(g.regs, Some(d), dcn).ok_or_else(|| String::from("no FB offset register read"))?;
    if src != "MMMC_VM_FB_OFFSET" {
        return Err(format!("MMHUB's FB offset did not read (only {src}): carve-out base not proven"));
    }
    if let Some(x) = dcn
        && x & 0xFF_FFFF != fb & 0xFF_FFFF
    {
        return Err(format!("MMMC_VM_FB_OFFSET {fb:#x} and DCN_VM_FB_OFFSET {x:#x} differ: carve-out base unclear"));
    }
    let mb = probe::memsize(g.regs)?;
    let plan = CarvePlan::new(fb, mb).map_err(|e| format!("carve-out: {e:?}"))?;
    let size = bo::carve_size(a);
    Ok(CarveWin { mc_base: a.fb_first, phys_base: plan.base, size })
}

/// GX-7e/GX-7f: the mode-2 reset of GC. Refused before anything is written (`gfxup::MODE2_REFUSED`, `preflight`).
#[inline(never)]
fn reset(args: Args, lines: &mut Vec<String>) -> Result<bool, ()> {
    let pre = pre_now(Until::Gfx, args.again, None, Ok(()), true, true);
    let why = preflight(&pre).err().unwrap_or_else(|| String::from(gfxup::MODE2_REFUSED));
    refuse(lines, why)
}

/// The tool's arguments: `until` ring|psp|gfx|gart|sdma|gfx_ring|mes|gfx_mes|stop|reset (default ring), `keep`,
/// `again`, `variant` (gfx_ring only).
pub fn parse_args(args: &mcp::Json) -> Result<Args, String> {
    let flag = |k: &str| -> Result<bool, String> {
        match args.get(k).filter(|v| !v.is_null()) {
            None => Ok(false),
            Some(v) => v.as_bool().ok_or_else(|| format!("{k} (true or false)")),
        }
    };
    let action = match args.get("until").filter(|v| !v.is_null()) {
        None => Action::Run(Until::Ring),
        Some(v) => match v.as_str() {
            Some("ring" | "m1") => Action::Run(Until::Ring),
            Some("psp" | "m2" | "fw") => Action::Run(Until::Fw),
            Some("gfx" | "m3") => Action::Run(Until::Gfx),
            Some("gart" | "gmc") => Action::Run(Until::Gart),
            Some("sdma" | "m4") => Action::Run(Until::Sdma),
            Some("gfx_ring" | "m5") => Action::Run(Until::GfxRing),
            Some("mes") => Action::Run(Until::Mes),
            Some("gfx_mes") => Action::Run(Until::GfxMes),
            Some("vm") => Action::Run(Until::Vm),
            Some("stop") => Action::Stop,
            Some("reset") => Action::Reset,
            _ => return Err(String::from("until (ring, psp, gfx, gart, sdma, gfx_ring, mes, gfx_mes, vm, stop or reset)")),
        },
    };
    let variant = match args.get("variant").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match (v.as_str().and_then(gfxring::Variant::parse), action) {
            (Some(x), Action::Run(Until::GfxRing)) => Some(x),
            (Some(_), _) => return Err(String::from("variant (only with until=gfx_ring)")),
            (None, _) => return Err(String::from("variant (e1a, e1b or e1c)")),
        },
    };
    let ack_linux = match args.get("ack").filter(|v| !v.is_null()).map(|v| (v.as_str(), action)) {
        None => matches!(action, Action::Run(Until::Vm)),
        Some((Some("linux"), Action::Run(Until::Vm))) => true,
        Some((Some("helper"), Action::Run(Until::Vm))) => false,
        Some((Some("linux" | "helper"), _)) => return Err(String::from("ack (only with until=vm)")),
        Some(_) => return Err(String::from("ack (linux or helper)")),
    };
    Ok(Args { action, keep: flag("keep")?, again: flag("again")?, variant, ack_linux })
}
