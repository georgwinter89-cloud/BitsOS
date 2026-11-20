//! GP-GPU2: the kernel glue of the stages, and the MCP entry (`gx_bringup {"device":..,"stage":..}`). The only file of
//! the card's driver whose `Hw` writes: BAR5 registers, and VRAM through a window of BAR0 mapped uncached around each
//! access (the 16 GiB are never one mapping). Every write reaches this `Hw` only through the bring-up owner and the
//! stage's guard (`stage::run`). Up to the `ih` stage no PCI configuration write: the function is not enabled and
//! bus master is not touched (the PSP's buffers all lie in VRAM, `vram`; the firmware left the command register at
//! 0x0006, memory and bus master on, the journal of 11.11.: "enabling device (0006 -> 0007)"). Wave 42: the `nbif`
//! stage sets AtomicOp requester enable in the card's own Device Control 2, the `ih` stage has the PCI core enable
//! the card's own function for sending and take its MSI vector, and the `gmc` and `ih` stages hand the card system
//! pages (`host::Host`); no bridge is ever written.
//!
//! What needs configuration space happens BEFORE the stage, outside its guard and the session's lock
//! (`hostprep::prepare` over `livepci`: there that chain is the gate), and the stage finds the answers in `Prep`.
//! Wave 43: BAR2's first page takes a doorbell write (the copy engines' 64-bit doorbells; the IH's, when a stage
//! reads the IH rings itself; since wave 44 MES's two, 0x16 and 0x18), each only as the stage's guard lists it.
//!
//! One stage call per card at a time (`Call`), so the stage's order check the chain was given still holds when
//! the stage runs. What the chain changed stands in the answer whether the stage passes or stops.

use super::attach::BarFact;
use super::bootnote;
use super::host::Host;
use super::hostprep::{For, Prep};
use super::stage::{self, Ctx, Opts, Stage};
use super::{liveih, livepci};
use crate::devs::{Device, Family, Key};
use crate::gpucore::fwload::StickSource;
use crate::gpucore::psp::{Hw, Timeouts};
use alloc::format;
use alloc::string::String;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use rdna4_core::bootnote::BootNote;

struct StageHw {
    bar5: IoMem,
    bar0: BarFact,
    /// BAR2, the doorbells, and its first page once a stage rang one (wave 43; mapped for the call, not per write:
    /// a stage that reads the IH rings itself rings their doorbell at every look).
    bar2: Option<BarFact>,
    bells: Option<IoMem>,
    dev: &'static Device,
    /// The PCI side's answers, gathered before the session's lock (`hostprep::prepare`).
    prep: Prep,
    /// The lowest stack address at which the stage reached the card (`depth::stack_at`): the measurement run
    /// reports how deep the real card's stages go, which the wave-42 stack budget could only take from the fake.
    /// The RX 9070 XT's own `Hw`: the 780M's path never comes here.
    lowest: usize,
}

fn not_asked<T>(what: &str) -> Result<T, String> {
    Err(format!("{what}: not asked, a step before it said no"))
}

impl Host for StageHw {
    fn dma_survey(&mut self) -> Result<(), String> {
        livepci::dma_survey()
    }
    fn atomics(&mut self) -> Result<String, String> {
        self.prep.atomics.clone().unwrap_or_else(|| not_asked("AtomicOps to the root port"))
    }
    fn bus_master(&mut self) -> Option<bool> {
        self.prep.bus_master
    }
    fn bridges_forward(&mut self) -> Result<(), String> {
        self.prep.path.clone().unwrap_or_else(|| not_asked("the bridges above the card"))
    }
    fn enable_dma(&mut self) -> Result<String, String> {
        self.prep.enabled.clone().unwrap_or_else(|| not_asked("the function's enable"))
    }
    fn msi(&mut self) -> Result<String, String> {
        self.prep.msi.clone().unwrap_or_else(|| not_asked("the MSI vector"))
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        // The piece the chain took before it enabled the function, once; any other is taken now.
        match self.prep.pages.take_if(|(b, _)| *b == bytes) {
            Some((_, at)) => Some(at),
            None => liveih::pages(self.dev, bytes),
        }
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        liveih::sys_read(self.dev, at, buf)
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        liveih::sys_write(self.dev, at, data)
    }
}

impl StageHw {
    fn deeper(&mut self) {
        self.lowest = self.lowest.min(super::depth::stack_at());
    }

    fn doorbells(&mut self, index_dw: u32, bytes: usize) -> Option<&IoMem> {
        const PAGE: usize = 4096;
        let bar2 = self.bar2.filter(|b| b.bytes >= PAGE as u64 && index_dw as usize * 4 + bytes <= PAGE)?;
        if self.bells.is_none() {
            self.bells = hal::mmio::map_device(bar2.phys, PAGE, CacheType::Uncached).ok();
        }
        self.bells.as_ref()
    }

    fn window(&self, at: u64, len: usize) -> Option<(IoMem, usize)> {
        let end = at.checked_add(len as u64).filter(|e| *e <= self.bar0.bytes)?;
        let first = at & !0xFFF;
        let bytes = (end - first).next_multiple_of(0x1000) as usize;
        match hal::mmio::map_device(self.bar0.phys + first, bytes, CacheType::Uncached) {
            Ok(io) => Some((io, (at - first) as usize)),
            Err(e) => {
                klog::event!(Warn, "gx", "rdna4.window", phys = klog::Hex(self.bar0.phys + first), bytes = bytes, why = format!("{e:?}").as_str());
                None
            }
        }
    }
}

impl Hw for StageHw {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.deeper();
        self.bar5.read32(byte as usize).ok()
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.deeper();
        self.bar5.write32(byte as usize, value).is_ok()
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.deeper();
        let Some((io, skip)) = self.window(at, buf.len()) else { return false };
        for (i, chunk) in buf.chunks_mut(4).enumerate() {
            let Ok(v) = io.read32(skip + i * 4) else { return false };
            chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
        }
        true
    }
    fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.deeper();
        let Some((io, skip)) = self.window(at, data.len()) else { return false };
        // Dword stores (BAR0 is uncached here); a tail shorter than a dword is padded with what lies there.
        for (i, chunk) in data.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            if chunk.len() < 4 {
                let Ok(old) = io.read32(skip + i * 4) else { return false };
                w = old.to_le_bytes();
            }
            w[..chunk.len()].copy_from_slice(chunk);
            if io.write32(skip + i * 4, u32::from_le_bytes(w)).is_err() {
                return false;
            }
        }
        // The PSP reads what the CPU wrote: the stores are posted before the next register write (a doorbell-like
        // C2PMSG_67) only after a read of the same window (PCIe ordering: a read flushes posted writes).
        let _ = io.read32(skip);
        true
    }
    /// The windows are uncached (ordered already); a full fence also drains write-combining buffers, should a
    /// window ever be mapped WC, before the TLB flush's register write (as `psplive`'s `Hw`).
    fn store_barrier(&mut self) {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    /// Wave 43: Linux's `WDOORBELL64` and `WDOORBELL32` (amdgpu_doorbell_mgr.c:103-112, :65-74): one store at byte
    /// `index * 4` of BAR2. Which dwords a stage may ring is its guard's list; here only BAR2's first page is
    /// reached, which holds every doorbell the card's blocks were given so far (SDMA's 0x200 and 0x214, the IH's
    /// 0x2f0 and 0x2f2, MES's 0x16 and 0x18).
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        self.deeper();
        self.doorbells(index_dw, 8).is_some_and(|io| io.write64(index_dw as usize * 4, value).is_ok())
    }
    fn doorbell_write32(&mut self, index_dw: u32, value: u32) -> bool {
        self.deeper();
        self.doorbells(index_dw, 4).is_some_and(|io| io.write32(index_dw as usize * 4, value).is_ok())
    }
    /// Linux's `udelay(1)` between the reads of a register wait and its `mdelay(20)` after a command are busy
    /// waits (amdgpu_psp.c:629; psp_v14_0.c:229, :259, :328), and so they are here: a sleep would stretch a wait of
    /// 100000 reads to the scheduler's grain per read, and the secure OS's 20 ms past the moment Linux looks at
    /// C2PMSG_81. The fence's poll between them (Linux's `usleep_range(60, 100)`, amdgpu_psp.c:761) sleeps.
    fn delay_us(&mut self, us: u64) {
        if us < 60 || (crate::gpucore::trail::SLEEP_US..=super::ring14::SETTLE_US).contains(&us) {
            time::delay_us(us);
        } else {
            sched::sleep_ns(us.saturating_mul(1000));
        }
    }
}

/// The stages' names, in their order.
fn names() -> String {
    Stage::ALL.map(Stage::name).join(", ")
}

/// The argument of the card's stages: `stage` (required).
pub fn parse(args: &mcp::Json) -> Result<Opts, String> {
    let s = args.get("stage").and_then(|v| v.as_str()).ok_or_else(|| format!("stage ({})", names()))?;
    let stage = Stage::parse(s).ok_or_else(|| format!("stage '{s}': one of {}", names()))?;
    Ok(Opts { stage })
}

/// Cards with a stage call under way.
static IN_CALL: ksync::Mutex<alloc::vec::Vec<Key>> = ksync::Mutex::named("amdgpu.rdna4.call", alloc::vec::Vec::new());

/// One stage call of a card, from its order check to its answer. Not a lock held across the call (PCI's locks
/// lie outside every driver lock): a second call meanwhile is refused instead of made to wait.
struct Call(Key);

impl Call {
    fn begin(key: Key) -> Option<Call> {
        let mut c = IN_CALL.lock();
        if c.contains(&key) {
            return None;
        }
        c.push(key);
        Some(Call(key))
    }
}

/// Whether a stage call of the card is under way (the measurement run's whole run counts as one).
pub fn in_call(key: Key) -> bool {
    IN_CALL.lock().contains(&key)
}

impl Drop for Call {
    fn drop(&mut self) {
        IN_CALL.lock().retain(|k| *k != self.0);
    }
}

/// A hold on the card's stage calls: while it lives no other stage call of this card begins (a second one is
/// refused, not queued). `gx_bringup` takes one per call; the measurement run (`messlive`) takes one for its whole
/// run, so no MCP call can slip a stage in between two of its stages and change the chain.
pub struct Claim(Call);

impl Claim {
    pub fn take(key: Key) -> Option<Claim> {
        Call::begin(key).map(Claim)
    }

    /// The card it holds.
    pub fn key(&self) -> Key {
        self.0.0
    }
}

/// One stage call as it went: the stage's result and what the glue around it did.
pub struct Ran {
    pub d: stage::Done,
    /// Changes of the card's own configuration space made before the stage (`hostprep`), outside its guard.
    pub changed: alloc::vec::Vec<String>,
    /// `ih` passed: whether the thread that reads the rings started.
    pub reader: Option<Result<(), String>>,
    /// The bootloader's progress could not be handed on (no free note slot).
    pub note_lost: bool,
    /// The lowest stack address at which the stage reached the card (`usize::MAX`: it never did).
    pub lowest: usize,
    /// From the order check to the stage's end.
    pub us: u64,
}

/// MCP: one stage on one card. The answer's first line is the plain one.
pub fn tool(dev: &'static Device, args: &mcp::Json) -> Result<String, String> {
    if dev.family != Family::Rdna4 {
        return Err(format!("{} is no RX 9070 XT", dev.card_name()));
    }
    let o = parse(args)?;
    let claim = Claim::take(dev.key).ok_or_else(|| format!("{}: another stage call of this card is running; ask again when it has answered", dev.card_name()))?;
    let ran = run_one(dev, o.stage, &claim)?;
    Ok(answer(&dev.card_name(), o.stage, &ran))
}

/// One stage on the card, the one way any caller drives it (`gx_bringup`, the measurement run): the order
/// check, the PCI side before the session's lock, the Reload hooks, then `stage::run` behind the owner and the
/// stage's guard under the session's one lock. `claim` is the caller's hold on this card's stage calls.
pub fn run_one(dev: &'static Device, stage: Stage, claim: &Claim) -> Result<Ran, String> {
    if claim.key() != dev.key {
        return Err(format!("{}: the caller holds the stage calls of {}, not of this card", dev.card_name(), claim.key()));
    }
    let o = Opts { stage };
    let t0 = time::now_ns();
    let card = super::live::card(dev).ok_or_else(|| format!("{} has no PCI function", dev.card_name()))?;
    let att = card.state.as_ref().map_err(|s| format!("{}: the read-only attach stopped ({s}); no stage runs", dev.card_name()))?;
    let (Some(bar0), Some(bar5)) = (card.facts.bar0, card.facts.bar5) else { return Err(String::from("BAR0 or BAR5 missing")) };
    let io = hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached).map_err(|e| format!("BAR5: {e:?}"))?;
    let notes = crate::psplive::note_io();
    let c = Ctx { index: dev.index, display: dev.display, facts: &card.facts, att, gop: crate::gpu::gop(), inherited: notes.and_then(|n| n.take_note_for(dev.key)), t: Timeouts::LINUX };
    // PCI's locks lie outside the session's: what `nbif` and `ih` need of configuration space comes first. No
    // other stage call of this card runs until this one answers, so the order check still holds in the stage.
    let (what, would_run) = {
        let s = dev.stages.0.lock();
        // The same two checks the stage makes: its place in the chain, and its own (once in a boot).
        let in_order = stage::in_order(&s, o.stage).is_ok();
        match o.stage {
            Stage::Nbif => (For::Nbif, in_order && super::nbifstage::order(&s.nb).is_ok()),
            Stage::Ih => (For::Ih { bytes: super::ihstage::HOST_BYTES }, in_order && super::ihstage::order(&s.nb).is_ok()),
            _ => (For::Other, false),
        }
    };
    let prep = livepci::prepare(dev, what, would_run);
    // A Reload after `gchub` leaves GFXHUB as Linux's fini does (`gchuboff`): the hook stands before the stage's
    // first write, and a stage whose hook cannot be set does not run.
    if o.stage == Stage::GcHub && dev.display {
        let runs = {
            let s = dev.stages.0.lock();
            stage::in_order(&s, o.stage).is_ok() && !s.gc.hub
        };
        if runs {
            super::livegc::arm_quiesce(dev).map_err(|e| format!("{}: the Reload hook for the graphics block's hub could not be set ({e}); the stage does not run", dev.card_name()))?;
        }
    }
    let changed = prep.changes.clone();
    // Wave 43: from the moment the copy engines may be started a Reload stops them first (`livesdma`), whether
    // the stage then passes or stops with a ring on. Asked with the stage's own two checks, as above.
    let mut sdma_hook = None;
    if o.stage == Stage::Sdma && dev.display {
        let runs = {
            let s = dev.stages.0.lock();
            stage::in_order(&s, o.stage).is_ok() && super::sdmastage::order(&s.sdma).is_ok()
        };
        if runs {
            let hook = super::livesdma::arm_quiesce(dev).map_err(|e| format!("{}: the Reload hook for the copy engines could not be registered ({e}); the stage does not run", dev.card_name()))?;
            sdma_hook = Some(hook);
        }
    }
    // Wave 44: from the moment the command processor may be unhalted a Reload halts it (`livecp`), whether the
    // stage then passes or stops with an engine unhalted. Registered before MES's hook, so it runs after MES's
    // halt and before GFXHUB's, Linux's order in gfx_v12_0_hw_fini (gfx_v12_0.c:3902, :3914, :3917).
    let mut cp_hook = None;
    if o.stage == Stage::GcCp && dev.display {
        let runs = {
            let s = dev.stages.0.lock();
            stage::in_order(&s, o.stage).is_ok() && super::cpstage::order(&s.cp).is_ok()
        };
        if runs {
            let hook = super::livecp::arm_quiesce(dev).map_err(|e| format!("{}: the Reload hook for the command processor could not be registered ({e}); the stage does not run", dev.card_name()))?;
            cp_hook = Some(hook);
        }
    }
    // Wave 44: from the moment MES may be started a Reload halts it (`livemes`), asked as above.
    let mut mes_hook = None;
    if o.stage == Stage::Mes && dev.display {
        let runs = {
            let s = dev.stages.0.lock();
            stage::in_order(&s, o.stage).is_ok() && super::messtage::order(&s.mes).is_ok()
        };
        if runs {
            mes_hook = Some(super::livemes::arm_quiesce(dev).map_err(|e| format!("{}: the Reload hook for MES could not be registered ({e}); the stage does not run", dev.card_name()))?);
        }
    }
    // Wave 44: from the moment the rings' interrupts may be enabled a Reload clears them (`livering`), asked as
    // above; registered after MES's, so it runs before MES's halt, as Linux's hw_fini puts them (gfx_v12_0.c:3888).
    let mut ring_hook = None;
    if o.stage == Stage::GcRing && dev.display {
        let runs = {
            let s = dev.stages.0.lock();
            stage::in_order(&s, o.stage).is_ok() && super::ringstage::order(&s.rings).is_ok()
        };
        if runs {
            ring_hook = Some(super::livering::arm_quiesce(dev).map_err(|e| format!("{}: the Reload hook for the rings' interrupts could not be registered ({e}); the stage does not run", dev.card_name()))?);
        }
    }
    let mut s = dev.stages.0.lock();
    if let Some(hook) = ring_hook {
        s.rings.reload = Some(hook);
    }
    if let Some(hook) = cp_hook {
        s.cp.reload = Some(hook);
    }
    if let Some(hook) = sdma_hook {
        // The stage tells the hook when it sets out to write the block; until then the hook touches nothing.
        s.sdma.reload = Some(hook);
    }
    if let Some(hook) = mes_hook {
        s.mes.reload = Some(hook);
    }
    if s.boot_before.is_none() {
        s.boot_before = notes.and_then(|io| bootnote::inherited(io, dev.key));
    }
    // The bootloader's note (`rdna4_core::bootnote`) goes out before the stage sends, so a kernel that ends in the
    // middle of the loads has told the next one, and again with what really went out.
    let boot_io = notes.filter(|_| o.stage == Stage::Sos && dev.display);
    let told_before = boot_io.map(|io| bootnote::put(io, dev.key, &BootNote::IN_FLIGHT));
    let (d, hw) = stage::run(&mut s, o, &c, StageHw { bar5: io, bar0, bar2: card.facts.bar2, bells: None, dev, prep, lowest: usize::MAX }, &mut StickSource);
    let lowest = hw.lowest;
    drop(hw);
    let told_after = boot_io.map(|io| bootnote::put(io, dev.key, &bootnote::hand_on(&s)));
    drop(s);
    let mut reader = None;
    if d.pass && o.stage == Stage::Ih {
        // From here the rings are read at each message, and a Reload switches them off first.
        reader = Some(liveih::start(dev));
    }
    if let (Some(n), Some(io)) = (d.note, notes) {
        let _ = io.put_note_for(dev.key, &n);
    }
    klog::event!(Info, "gx", "rdna4.stage", card = dev.index, stage = o.stage.name(), pass = d.pass, writes = d.totals.writes, refused = d.refused.len(); "a card stage ran");
    let note_lost = told_before == Some(false) || told_after == Some(false);
    Ok(Ran { d, changed, reader, note_lost, lowest, us: time::now_ns().saturating_sub(t0) / 1000 })
}

/// The answer of one stage call, as `gx_bringup` gives it: the plain line, PASS or FAIL, the writes, what the
/// glue changed, the stage's lines (its `linuxseq` line last but for the glue's own).
pub fn answer(card: &str, stage: Stage, ran: &Ran) -> String {
    let d = &ran.d;
    let mut out = format!("{}\ngx_bringup {card} {}: {}\n", d.line, stage.name(), if d.pass { "PASS" } else { "FAIL" });
    out.push_str(&format!("writes: {} register writes ({} failed), {} refused by the stage's list\n", d.totals.writes, d.totals.failed, d.refused.len()));
    for r in &d.refused {
        out.push_str(&format!("refused: {r:?}\n"));
    }
    // Outside the stage's guard and write log, so named here whether the stage passed or stopped.
    for c in &ran.changed {
        out.push_str(&format!("changed before the stage, in the card's own configuration space: {c}\n"));
    }
    for l in &d.lines {
        out.push_str(l);
        out.push('\n');
    }
    if let Some(r) = &ran.reader {
        out.push_str(&match r {
            Ok(()) => String::from("ih: the rings are read by the thread amdgpu-ih7 at each message\n"),
            Err(e) => format!("ih: the rings are on, but nothing reads them: {e}\n"),
        });
    }
    if ran.note_lost {
        out.push_str("note: the bootloader's progress could not be handed on (no free note slot): a kernel after a Reload would not know of it\n");
    }
    out
}
