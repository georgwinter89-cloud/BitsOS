//! Wave 43, item HB (the start of piece 13, piece 10's GFXHUB): the two stages that follow the loaded firmware
//! in Linux's `gfx_v12_0_hw_init` (gfx_v12_0.c:3767-3818), each one section of the trace:
//!
//! | stage | Linux | trace section | writes |
//! |---|---|---|---|
//! | gcboot | the wait for the RLC's autoload :3797-3804, the golden register :3807, `get_gb_addr_config` :3812, `gfx_v12_0_config_gfx_rs64` :3816 | gc-boot | DB_MEM_CONFIG; GRBM_GFX_CNTL, the six program counter starts, the reset pulses in CP_ME_CNTL and CP_MEC_RS64_CNTL |
//! | gchub | `gfx_v12_0_gfxhub_enable` :3603-3622 | gc-hub | GFXHUB's GART start, CP_DEBUG, the fault control, the HDP flush, the first TLB flush on GFXHUB's engine 17 |
//!
//! Both need what Linux has by then and neither names it: the firmware loaded and the power controller's features
//! on (the trace: the SMU's start and the display lie between the secure processor and gc-boot). The chain's order
//! (`stage::in_order`) says which stage stands before them; what each checks itself is only its own: a second
//! run, and for `gchub` the GART `gmc` made, whose table, default page and dummy page GFXHUB is told too
//! (`gmcstage::start_hub`, the one hub start over `hub41::GC`).
//!
//! `gcboot` sets where the three RS64 engines WILL start and pulses their pipes' resets; it does not unhalt them
//! (`rdna4_core::gc12`). The wait for the autoload is Linux's own loop over two registers, so it is no single
//! register's wait in the trail: its reads are plain reads there, and the section's one declared rule
//! (`Rule::PairPoll`) says that a card may need more rounds of them than Linux's card did
//! (plan/spec/gpu-rdna4.md section 6).
//!
//! A second `fw` in one boot starts the block anew: `before_fw` switches GFXHUB off first, as Linux's fini does
//! before its resume loads the firmware again, and only then is what the two stages did void.

use super::fwlist;
use super::gart12::{FlushRegs, Gart};
use super::gchuboff;
use super::guard::Allow;
use super::ihoff::{Off, OffIo};
use super::linuxseq::compare::{Compare, Diff};
use super::linuxseq::fold::Bus;
use super::linuxseq::rules::{Address, PairPoll, Rule};
use super::nb::{self, Io};
use super::nbregs::NbRegs;
use super::policy;
use super::regs14::Regs14;
use super::stage::{Ctx, Done, Stage, Stop, read_file, stop};
use crate::gpucore::fwload::FwSource;
use crate::gpucore::psp::{Hw, LINUX_POLLS};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::gc12::{self, Start, Starts};
use rdna4_core::hub41::{self, Chosen, Op};
use rdna4_core::nbreg::member;
use rdna4_core::regmap::Reg;
use rdna4_core::{layout, pte12};

/// What `gcboot` left: the block's configuration word as the card says it and where the engines will start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot {
    pub gb_addr_config: u32,
    pub starts: Starts,
    /// How many rounds of Linux's loop the autoload took (Linux's own start: one).
    pub rounds: u32,
}

/// The graphics block of one card in this boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// `gcboot` passed.
    pub boot: Option<Boot>,
    /// `gchub` passed: GFXHUB's context 0 is on. From here a GART bind or unbind flushes both hubs
    /// (`Session::flush_regs`). Linux's switch for that is `gfx.is_poweron`, set between the two stages
    /// (gfx_v12_0.c:3810); nothing binds between them, and GFXHUB takes no invalidation before its context 0.
    /// It falls only after the hub was switched off (`before_fw`), as Linux's does (:3917-3919).
    pub hub: bool,
}

impl State {
    pub const fn new() -> State {
        State { boot: None, hub: false }
    }

    pub fn describe(&self) -> String {
        format!(
            "graphics block {}, its hub {}",
            self.boot.map_or(String::from("not started"), |b| format!("started (autoload complete after {} rounds, GB_ADDR_CONFIG {:#x}, engines halted)", b.rounds, b.gb_addr_config)),
            if self.hub { "on (a bind flushes both hubs)" } else { "off" }
        )
    }
}

pub fn is_gc(stage: Stage) -> bool {
    matches!(stage, Stage::GcBoot | Stage::GcHub)
}

/// The trace section a stage of this item must equal.
pub fn section_of(stage: Stage) -> Option<&'static str> {
    match stage {
        Stage::GcBoot => Some("gc-boot"),
        Stage::GcHub => Some("gc-hub"),
        _ => None,
    }
}

/// What a stage of this item may write: Linux's writes of its section and nothing else. No memory, no host side.
pub fn allow(stage: Stage, s: &nb::State, n: &NbRegs) -> Allow {
    let mut regs: Vec<Option<u32>> = Vec::new();
    match stage {
        Stage::GcBoot => {
            regs.push(n.byte(gc12::DB_MEM_CONFIG));
            regs.extend(gc12::RS64_REGS.iter().map(|r| n.byte(*r)));
        }
        Stage::GcHub => {
            let h = &hub41::GC;
            hub41::gart_enable_regs(h, |r| regs.push(n.byte(r)));
            let _ = hub41::fault_enable_default(h, |op| -> Result<(), ()> {
                regs.push(n.byte(op.reg()));
                Ok(())
            });
            regs.push(Some(s.hdp_flush(n.hdp_flush)));
            regs.push(n.byte(member(h.eng0_req, hub41::GART_ENGINE, 1)));
        }
        _ => {}
    }
    Allow { stage: stage.name(), regs: regs.into_iter().flatten().collect(), mem: Vec::new() }
}

/// The registers of the stage's TLB flush: `gchub` flushes GFXHUB before the session says the hub is on.
pub fn flush_regs(stage: Stage, f: Option<FlushRegs>, n: &NbRegs) -> Option<FlushRegs> {
    if stage == Stage::GcHub { f.and_then(|f| f.with_gc(n)) } else { f }
}

/// The three addresses a hub is told, as its registers hold them: the page table base without its valid bit,
/// the default page, the fault's dummy page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Told {
    pub table: u64,
    pub default: u64,
    pub dummy: u64,
}

impl Told {
    /// What `gmc` chose for both hubs (`hub41::gart_enable`'s arithmetic over the session's GART).
    pub fn of(g: &Gart) -> Told {
        Told { table: g.table.start + g.l.base_offset, default: g.scratch.start + g.l.base_offset, dummy: g.dummy }
    }

    /// What MMHUB was told as the bus saw it: the addresses the three rules of `mmhub` held, in their order
    /// (`nbseq::rules`), for the comparison of a whole chain.
    pub fn seen(section: &str, held: &[Option<u64>]) -> Option<Told> {
        match *held {
            [Some(table), Some(default), Some(dummy)] if section == "mmhub" => Some(Told { table, default, dummy }),
            _ => None,
        }
    }
}

/// The declared differences of a section. `gc-boot` has one, which is no difference while the card answers as
/// it answered Linux: the autoload's loop may take more rounds (the engines' starts come from the firmware
/// files' headers, not from an address BitsOS chooses). `gc-hub` names the three addresses of `mmhub`, and each
/// must be exactly the one MMHUB was told (`told`; `None` without it: the section cannot be compared): Linux
/// hands both hubs one table, one default page and one dummy page, and its trace shows the same three values in
/// both sections.
pub fn linux_rules(section: &str, told: Option<&Told>) -> Option<Vec<Rule>> {
    if section != "gc-hub" {
        // gfx_v12_0.c:2337-2349: `adev->usec_timeout` rounds, CP_STAT == 0 and BOOTLOAD_COMPLETE.
        let done = gc12::f::BOOTLOAD_COMPLETE.mask;
        return Some(alloc::vec![Rule::PairPoll(PairPoll {
            first: gc12::CP_STAT.name,
            second: gc12::RLC_RLCS_BOOTLOAD_STATUS.name,
            first_is: (u32::MAX, 0),
            second_is: (done, done),
            max: LINUX_POLLS,
            why: "the autoload is done when the card says so: BitsOS reaches the wait sooner or later after AUTOLOAD_RLC than Linux did, and Linux's own loop reads the pair until it is",
        })]);
    }
    let t = told?;
    let pair = |lo, hi, shift, keep, at: u64, bytes, why| Rule::Address(Address { lo, hi: Some(hi), shift, keep, align: pte12::PAGE, within: at..at.saturating_add(bytes), bytes, why });
    Some(alloc::vec![
        pair("GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", "GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32", 0, 1, t.table, layout::GART_BYTES / pte12::PAGE * 8, "GFXHUB runs on the GART table MMHUB was told, where BitsOS's VRAM manager put it (with Linux's valid bit)"),
        pair("GCMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_LSB", "GCMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_MSB", 12, 0, t.default, pte12::PAGE, "GFXHUB's default page is the one MMHUB was told, where BitsOS's VRAM manager put it"),
        pair("GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_LO32", "GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_HI32", 12, 0, t.dummy, pte12::PAGE, "GFXHUB's dummy page is the system page of BitsOS's that MMHUB was told"),
    ])
}

/// A stage's trail against its whole section: `Ok` or the first difference in plain words. `gart`: the session's
/// GART, whose three addresses `gchub` must have named.
pub fn equal(d: &Done, regs: &Regs14, gart: Option<&Gart>) -> Result<(), String> {
    let name = section_of(d.stage).ok_or("no section for this stage")?;
    let sec = super::linuxseq::section(name)?;
    let rules = linux_rules(name, gart.map(Told::of).as_ref()).ok_or("no GART on record: what GFXHUB must be told is not known")?;
    let text = |e: Diff| format!("{e}");
    let mut c = Compare::new(&sec, &rules).map_err(text)?;
    c.feed_trail(&d.trail, d.trail_lost, &Bus::of(regs)).map_err(text)?;
    c.finish().map_err(text)
}

/// One file's start from its documented header (`fwlist::rs64_start`).
fn start_of(c: &Ctx, files: &mut dyn FwSource, role: fwlist::Role) -> Result<Start, Stop> {
    let plain = fwlist::file_of(role).ok_or_else(|| stop("eine Datei fehlt auf dem Stick", format!("no file for {role:?} in the card's list")))?;
    let name = fwlist::name_for(plain, c.facts.device, c.facts.revision);
    let b = read_file(files, name)?;
    let (lo, hi) = fwlist::rs64_start(&b).map_err(|e| stop("eine Datei auf dem Stick passt nicht", format!("{name}: {e:?}")))?;
    Ok(Start { lo, hi })
}

/// `gfx_v12_0_wait_for_rlc_autoload_complete` (gfx_v12_0.c:2331-2354): at most 100000 rounds of two reads 1 us
/// apart, until CP_STAT reads 0 and the bootload status has BOOTLOAD_COMPLETE; the rounds it took. A block
/// that is still coming up may answer all ones: that is "not yet" as it is for Linux, so these two reads do not
/// go through `psp::read` (which takes all ones for a card that is gone).
fn wait_autoload(io: &mut Io, out: &mut Vec<String>) -> Result<u32, Stop> {
    let (cp, boot) = (io.byte(gc12::CP_STAT)?, io.byte(gc12::RLC_RLCS_BOOTLOAD_STATUS)?);
    let hw = io.card.hw();
    let gone = |r: Reg| stop("Karte antwortet beim Lesen nicht", format!("{} does not read", r.name));
    let mut last = (0, 0);
    for round in 1..=LINUX_POLLS {
        let c = hw.reg_read(cp).ok_or_else(|| gone(gc12::CP_STAT))?;
        let b = hw.reg_read(boot).ok_or_else(|| gone(gc12::RLC_RLCS_BOOTLOAD_STATUS))?;
        if gc12::autoload_complete(c, b) {
            out.push(format!("gcboot: the autoload is complete (CP_STAT {c:#x}, RLC_RLCS_BOOTLOAD_STATUS {b:#x}) at round {round} of Linux's loop (Linux's own start: round 1)"));
            return Ok(round);
        }
        last = (c, b);
        hw.delay_us(1);
    }
    klog::event!(Error, "gx", "rdna4.gc.autoload_timeout", cp_stat = klog::Hex(u64::from(last.0)), bootload = klog::Hex(u64::from(last.1)); "rlc autoload: gc ucode autoload timeout");
    Err(stop("der Grafikteil der Karte ist nicht gestartet", format!("rlc autoload: gc ucode autoload timeout after {LINUX_POLLS} rounds of two reads 1 us apart (last CP_STAT {:#x}, RLC_RLCS_BOOTLOAD_STATUS {:#x}); Linux gives up the same way (gfx_v12_0.c:2351-2354)", last.0, last.1)))
}

/// The stage `gcboot`.
pub fn boot(g: &mut State, io: &mut Io, c: &Ctx, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    if g.boot.is_some() {
        return Err(stop("der Grafikteil ist schon vorbereitet", "gcboot ran in this boot: a second run is refused"));
    }
    // Linux's golden list depends on the revision (gfx_v12_0.c:3685-3688): revision 0 adds three writes
    // (`golden_settings_gc_12_0_rev0` :253-257), which nobody built. The attach read it as Linux does.
    if c.att.rev_id != Some(rdna4_core::devinfo::CHIP_REV) {
        let seen = c.att.rev_id.map_or(String::from("unread"), |r| format!("{r}"));
        return Err(stop("diese Karte hat eine andere Ausführung als die, für die der Treiber gebaut ist", format!("gcboot: this card is revision {seen} (STRAP0 bits 27:24, read at the attach); only the golden list of revision {} is built (gfx_v12_0.c:253-261 adds three writes for revision 0)", rdna4_core::devinfo::CHIP_REV)));
    }
    // Linux holds the three files since its sw_init (gfx_v12_0_init_microcode :589-665); here a stick without
    // one of them ends the stage before the card is touched.
    let starts = Starts { pfp: start_of(c, files, fwlist::Role::Pfp)?, me: start_of(c, files, fwlist::Role::Me)?, mec: start_of(c, files, fwlist::Role::Mec)? };
    let rounds = wait_autoload(io, out)?;
    io.op(Op::Modify(gc12::DB_MEM_CONFIG, gc12::golden_db_mem_config))?;
    // `adev->gfx.is_poweron = true` stands here in Linux (:3810); BitsOS's switch is `State::hub`.
    let gb_addr_config = io.read(gc12::GB_ADDR_CONFIG)?;
    if gb_addr_config == 0 {
        // Linux warns and goes on (:3812-3813).
        klog::event!(Warn, "gx", "rdna4.gc.gb_addr_config"; "Invalid gb_addr_config");
        out.push(String::from("gcboot: GB_ADDR_CONFIG reads 0: Linux warns (\"Invalid gb_addr_config\") and goes on, so does this"));
    }
    gc12::config_gfx_rs64(&starts, |op| io.op(op))?;
    out.push(format!(
        "gcboot: GB_ADDR_CONFIG {gb_addr_config:#x}; the engines will start at PFP {:#x}/{:#x}, ME {:#x}/{:#x}, MEC {:#x}/{:#x} (program counter start, its high word); their pipes' resets pulsed, all three still halted",
        starts.pfp.counter(),
        starts.pfp.counter_hi(),
        starts.me.counter(),
        starts.me.counter_hi(),
        starts.mec.counter(),
        starts.mec.counter_hi()
    ));
    g.boot = Some(Boot { gb_addr_config, starts, rounds });
    Ok(())
}

/// The stage `gchub`.
pub fn hub(g: &mut State, s: &nb::State, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    if g.hub {
        return Err(stop("der Speicher des Grafikteils ist schon eingerichtet", "gchub ran in this boot: a second run is refused"));
    }
    let Some(gart) = s.gart.as_ref() else { return Err(stop("Reihenfolge stimmt nicht", "no GART: GFXHUB is told the table gmc made")) };
    // The stage's flush names GFXHUB's engine or is none at all (`flush_regs`): no start without its flush.
    let f = io.flush()?;
    // The hub is told a system address (the fault's dummy page): the same question `gmc` asked before it.
    let survey = io.card.dma_survey();
    policy::may_send(io.driven, survey).map_err(|why| stop("der Rechner lässt die Karte nicht an den Arbeitsspeicher", why))?;
    let chosen = Chosen { table: gart.table.start, scratch: gart.scratch.start, dummy: gart.dummy };
    super::gmcstage::start_hub(io, &hub41::GC, &gart.l, &chosen, &f, out)?;
    out.push(format!("gchub: GFXHUB on the GART table at mc {:#x} (page table base {:#x}), default page at vram {:#x}, dummy page {:#x}: the ones MMHUB has", gart.l.mc(gart.table.start), hub41::pd_addr(&gart.l, gart.table.start), gart.scratch.start, gart.dummy));
    g.hub = true;
    Ok(())
}

/// A stage's `Hw` as the switch-off's registers.
struct HwOff<'a>(&'a mut dyn Hw);

impl OffIo for HwOff<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, us: u64) {
        self.0.delay_us(us)
    }
}

/// What `fw` may write besides its own list: GFXHUB's switch-off, and only while this boot's `gchub` left the
/// hub on (`before_fw`).
pub fn off_regs(stage: Stage, g: &State, n: &NbRegs) -> Vec<u32> {
    let mut regs = Vec::new();
    if stage == Stage::Fw && g.hub {
        let _ = hub41::gart_disable(&hub41::GC, |op| -> Result<(), ()> {
            regs.extend(n.byte(op.reg()));
            Ok(())
        });
    }
    regs
}

/// Before `fw` sends its first image a second time in one boot. The SMU's firmware and the autoload start the
/// graphics block anew, which Linux does only at a resume, after its suspend ran `gfx_v12_0_hw_fini`: that
/// switches GFXHUB off (`gfxhub_v12_0_gart_disable`, gfx_v12_0.c:3917) and only then lets a bind's flush leave
/// that hub out (`gfx.is_poweron = false`, :3919, with gmc_v12_0.c:308). So does this, in that order: the hub
/// off by the sequence the Reload hook runs, then what `gcboot` and `gchub` did is void and the chain asks for
/// both again. A hub that cannot be switched off stops the stage before anything is loaded over it.
pub fn before_fw(g: &mut State, c: &Ctx, hw: &mut dyn Hw, out: &mut Vec<String>) -> Result<(), Stop> {
    if g.hub {
        let d = &c.att.discovery;
        let n = Regs14::from(d).and_then(|r| NbRegs::from(d, &r)).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "GFXHUB's registers have no place in BAR5"))?;
        match gchuboff::gart_disable(&mut HwOff(hw), &n, &hub41::GC) {
            Off::Done => out.push(String::from("fw: the graphics block starts anew: GFXHUB switched off first (gfxhub_v12_0_gart_disable, as Linux's fini does before its resume loads the firmware again); gcboot and gchub are asked again")),
            Off::NotOn => out.push(String::from("fw: the graphics block starts anew: GFXHUB's context 0 was off already; gcboot and gchub are asked again")),
            Off::Gone => {
                return Err(stop(
                    "der Speicher des Grafikteils lässt sich nicht abschalten",
                    "GFXHUB does not answer or a write of its switch-off did not arrive: no firmware is loaded anew over a hub that may still be on (Linux switches it off first, gfx_v12_0.c:3917)",
                ));
            }
        }
    }
    *g = State::new();
    Ok(())
}

/// One stage of this item.
pub fn run(g: &mut State, s: &nb::State, stage: Stage, io: &mut Io, c: &Ctx, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    match stage {
        Stage::GcBoot => boot(g, io, c, files, out),
        Stage::GcHub => hub(g, s, io, out),
        _ => Err(stop("Reihenfolge stimmt nicht", "no stage of the graphics block")),
    }
}

/// The block's registers as they read now, for the stage `state`: only once `gcboot` passed (a block without
/// power is not asked).
pub fn state_line(g: &State, n: &NbRegs, hw: &mut dyn Hw) -> Option<String> {
    g.boot?;
    let mut rd = |r: Reg| n.byte(r).and_then(|b| hw.reg_read(b)).map_or(String::from("-"), |v| format!("{v:#x}"));
    Some(format!("gc registers: CP_ME_CNTL {}, CP_MEC_RS64_CNTL {}, GCVM_CONTEXT0_CNTL {}", rd(gc12::CP_ME_CNTL), rd(gc12::CP_MEC_RS64_CNTL), rd(hub41::GC.context0_cntl)))
}
