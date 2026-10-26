//! Reboot and power-off (owner: CP, item CP-5; spec/core.md §4, §11.5; V1 `acpi/sleep.asm:14-129`, README
//! "Neustart und Ausschalten"; Linux `arch/x86/kernel/reboot.c`, ACPICA `hwsleep.c`).
//!
//! **Reboot** tries, 50 ms apart: the FADT reset register (Mini-PC: I/O 0xB2 <- 0xBE, a reset through SMM; q35:
//! 0xCF9 <- 6), the chipset's 0xCF9 (2, then 6: hard reset; then 2, then 0xE: full reset, Linux's cold-reboot
//! code, which also resets devices a hard reset may leave running), each keeping the register's other bits
//! (Linux `reboot.c:694-700`), the 8042's reset pulse where the FADT does not rule an 8042 out, and last the
//! triple fault, which resets every PC. **Power-off**: every GPE disabled and its status
//! cleared (an enabled wake GPE, e.g. the NIC's, would wake the machine at once), ACPI mode on if the firmware
//! left it off (SCI_EN 0 on the Mini-PC: ACPI_ENABLE to SMI_CMD), the fixed-event status cleared, then SLP_TYP
//! from `\_S5` with SLP_EN to PM1a/PM1b control (or the hardware-reduced sleep control register). After 1 s
//! still running: "hold the power button". Power-off is for the console command and QEMU only, never an MCP
//! tool (R8). Every register a table names passes the register guard first (`live::check_gas`).
//!
//! The callers quiesce the drivers first (`devmodel::quiesce_all`). `plan`s are pure and host-tested.

use crate::fadt::Fadt;
use crate::live;
use crate::s5::S5;
use crate::sdt::{Gas, Space};
use crate::Acpi;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The wait after each reset path (spec §4).
pub const STEP_WAIT_NS: u64 = 50_000_000;
/// How long the 8042's input buffer may stay full before the pulse goes out anyway (Linux `kb_wait`).
const KBC_READY_NS: u64 = 10_000_000;
const KBC_PORT: u16 = 0x64;
const KBC_PULSE_RESET: u8 = 0xFE;
const KBC_INPUT_FULL: u8 = 0x02;

/// One reset path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The FADT reset register in system I/O space (not 0xCF9).
    FadtIo { port: u16, value: u8 },
    /// The FADT reset register at 0xCF9 (q35 names it): its value through hal's 0xCF9 writer.
    FadtCf9 { value: u8 },
    /// The FADT reset register in memory space.
    FadtMem { addr: u64, value: u8 },
    /// 0xCF9: `SYS_RST` (2), then `code` (6 hard reset, 0xE full reset), other bits kept.
    Cf9 { code: u8 },
    /// The 8042 pulse; `declared`: the FADT names an 8042 (otherwise it only does not rule one out).
    Kbc { declared: bool },
    /// An empty IDT and `int3`.
    TripleFault,
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::FadtIo { port, value } => format!("FADT reset register I/O {port:#x} <- {value:#x}"),
            Step::FadtCf9 { value } => format!("FADT reset register I/O 0xcf9 <- {value:#x}"),
            Step::FadtMem { addr, value } => format!("FADT reset register memory {addr:#x} <- {value:#x}"),
            Step::Cf9 { code } if *code == hal::cpu::reset::CF9_CODE_HARD => String::from("0xcf9 <- 2, then 6"),
            Step::Cf9 { code } => format!("0xcf9 <- 2, then {code:#x} (full reset)"),
            Step::Kbc { declared } => format!(
                "keyboard controller 0x64 <- 0xfe ({})",
                if *declared { "the FADT names an 8042" } else { "not ruled out by the FADT" }
            ),
            Step::TripleFault => String::from("triple fault"),
        }
    }
    /// A path the firmware itself declares (the FADT reset register or a named 8042).
    pub fn declared(&self) -> bool {
        matches!(self, Step::FadtIo { .. } | Step::FadtCf9 { .. } | Step::FadtMem { .. } | Step::Kbc { declared: true })
    }
}

/// The reset paths in order, and the ones this machine does not offer (with why).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebootPlan {
    pub steps: Vec<Step>,
    pub skipped: Vec<String>,
}

impl RebootPlan {
    pub fn declared(&self) -> bool {
        self.steps.iter().any(Step::declared)
    }
}

/// What the FADT allows, in the order FADT reset register, 0xCF9, 8042, triple fault (pure; host-tested).
pub fn reboot_plan(fadt: Option<&Fadt>) -> RebootPlan {
    let mut p = RebootPlan::default();
    match fadt.map(|f| f.usable_reset()) {
        None => p.skipped.push(String::from("FADT reset register: no FADT")),
        Some(None) => p.skipped.push(String::from("FADT reset register: the FADT names none (or not as supported)")),
        Some(Some(r)) => match r.reg.space {
            Space::Io if r.reg.address == crate::guard::RESET_CF9 => p.steps.push(Step::FadtCf9 { value: r.value }),
            Space::Io if r.reg.address <= 0xFFFF => p.steps.push(Step::FadtIo { port: r.reg.address as u16, value: r.value }),
            Space::Memory => p.steps.push(Step::FadtMem { addr: r.reg.address, value: r.value }),
            s => p.skipped.push(format!("FADT reset register in {s:?} space at {:#x}: not supported", r.reg.address)),
        },
    }
    p.steps.push(Step::Cf9 { code: hal::cpu::reset::CF9_CODE_HARD });
    p.steps.push(Step::Cf9 { code: hal::cpu::reset::CF9_CODE_FULL });
    match fadt.and_then(|f| f.has_8042()) {
        Some(false) => p.skipped.push(String::from("keyboard controller: the FADT says there is none")),
        Some(true) => p.steps.push(Step::Kbc { declared: true }),
        None => p.steps.push(Step::Kbc { declared: false }),
    }
    p.steps.push(Step::TripleFault);
    p
}

/// The plan for this machine's tables.
pub fn live_reboot_plan() -> RebootPlan {
    reboot_plan(live::tables().and_then(|a| a.fadt.as_ref()))
}

/// One reset attempt through the register guard; false when the guard or the port refused it. Never returns
/// for the triple fault.
pub fn try_step(s: &Step) -> bool {
    match *s {
        Step::FadtIo { port, value } => live::check_io(port as u64, 1, false).is_ok() && hal::portio::outb(port, value).is_ok(),
        Step::FadtCf9 { value } => {
            if live::check_io(crate::guard::RESET_CF9, 1, true).is_err() {
                return false;
            }
            hal::cpu::reset::write_cf9(value);
            true
        }
        Step::FadtMem { addr, value } => {
            if live::check_mem(addr, 1).is_err() {
                return false;
            }
            match hal::mmio::map_device(addr, 1, hal::paging::CacheType::Uncached) {
                Ok(w) => w.write8(0, value).is_ok(),
                Err(_) => false,
            }
        }
        Step::Cf9 { code } => {
            let (arm, go) = hal::cpu::reset::cf9_writes(hal::cpu::reset::read_cf9(), code);
            hal::cpu::reset::write_cf9(arm);
            time::delay_us(50);
            hal::cpu::reset::write_cf9(go);
            true
        }
        Step::Kbc { .. } => {
            if !kbc_ready() {
                klog::event!(Warn, "power", "reboot.kbc_busy"; "the 8042 input buffer stayed full (or there is no 8042); pulsing anyway");
            }
            hal::portio::outb(KBC_PORT, KBC_PULSE_RESET).is_ok()
        }
        Step::TripleFault => hal::cpu::reset::triple_fault(),
    }
}

/// Waits (bounded, busy) until the 8042 can take a command.
fn kbc_ready() -> bool {
    let end = time::now_ns().saturating_add(KBC_READY_NS);
    loop {
        match hal::portio::inb(KBC_PORT) {
            Ok(st) if st & KBC_INPUT_FULL == 0 => return true,
            Err(_) => return false,
            Ok(_) => {}
        }
        if time::now_ns() >= end {
            return false;
        }
        time::delay_us(100);
    }
}

/// Resets the machine: every step of `plan`, `wait` (the caller's sleep or busy wait) 50 ms after each, the
/// triple fault last. Never returns. The caller quiesced the drivers.
pub fn reboot(plan: &RebootPlan, wait: &dyn Fn(u64)) -> ! {
    for (i, s) in plan.steps.iter().enumerate() {
        let what = s.describe();
        klog::event!(Info, "power", "reboot.step", n = i + 1, method = what.as_str());
        if matches!(s, Step::TripleFault) {
            klog::flush_emergency();
        }
        if !try_step(s) {
            klog::event!(Warn, "power", "reboot.refused", method = what.as_str(); "the register guard or port check refused this path");
            continue;
        }
        wait(STEP_WAIT_NS);
        klog::event!(Warn, "power", "reboot.still_here", method = what.as_str(); "no reset after 50 ms");
    }
    // Only reached if the triple fault step was missing from a hand-made plan.
    hal::cpu::reset::triple_fault()
}

// ---------------------------------------------------------------------------------------------------------
// Power-off

/// PM1 control: SLP_TYP (bits 10-12) and SLP_EN (bit 13); PM1 status: WAK_STS (bit 15).
pub const SLP_TYP_SHIFT: u32 = 10;
pub const SLP_TYP_MASK: u16 = 7 << 10;
pub const SLP_EN: u16 = 1 << 13;
pub const SCI_EN: u16 = 1;
/// ACPICA `ACPI_BITMASK_ALL_FIXED_STATUS`: timer, bus master, global lock, power and sleep button, RTC, wake.
pub const ALL_FIXED_STATUS: u16 = 0x8731;
/// Hardware-reduced sleep control: SLP_TYP in bits 2-4, SLP_EN bit 5.
pub const HWR_SLP_TYP_SHIFT: u32 = 2;
pub const HWR_SLP_EN: u8 = 1 << 5;
/// ACPICA waits up to 3 s for SCI_EN after ACPI_ENABLE.
pub const ACPI_ENABLE_WAIT_NS: u64 = 3_000_000_000;
/// V1: after 1 s still running, ask for the power button.
pub const OFF_WAIT_NS: u64 = 1_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PowerOffError {
    NoTables,
    HwReducedWithoutSleepControl,
    NoS5,
    NoPm1Control,
    /// The guard refused a register (its description).
    Refused(String),
    /// SCI_EN did not come on after ACPI_ENABLE.
    AcpiModeNotEntered,
    /// Everything was written and the machine still runs.
    StillRunning,
}

/// One GPE block: status bytes at `status`, enable bytes at `enable`, `len` each (I/O space).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpeBlock {
    pub status: u64,
    pub enable: u64,
    pub len: u32,
}

/// What power-off writes (pure; host-tested).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowerOffPlan {
    pub s5: S5,
    pub gpes: Vec<GpeBlock>,
    /// GPE blocks not in I/O space (left alone; noted).
    pub gpes_skipped: u32,
    /// (SMI_CMD port, ACPI_ENABLE value) when the firmware left ACPI mode off.
    pub acpi_enable: Option<(u16, u8)>,
    pub pm1a_evt: Option<Gas>,
    pub pm1b_evt: Option<Gas>,
    pub pm1a_cnt: Option<Gas>,
    pub pm1b_cnt: Option<Gas>,
    /// Hardware-reduced ACPI: the sleep control register instead of PM1.
    pub sleep_control: Option<Gas>,
}

fn gpe_block(g: Option<Gas>, len: u8) -> Option<Result<GpeBlock, ()>> {
    let g = g?;
    if len < 2 {
        return None;
    }
    if g.space != Space::Io {
        return Some(Err(()));
    }
    let half = len as u64 / 2;
    Some(Ok(GpeBlock { status: g.address, enable: g.address + half, len: half as u32 }))
}

/// The power-off plan for `a` (`sci_en`: SCI_EN as found at boot; `None` unknown).
pub fn poweroff_plan(a: &Acpi, sci_en: Option<bool>) -> Result<PowerOffPlan, PowerOffError> {
    let f = a.fadt.as_ref().ok_or(PowerOffError::NoTables)?;
    let (s5, _, _) = a.s5.ok_or(PowerOffError::NoS5)?;
    let mut gpes = Vec::new();
    let mut skipped = 0;
    for b in [gpe_block(f.gpe0, f.gpe0_len), gpe_block(f.gpe1, f.gpe1_len)].into_iter().flatten() {
        match b {
            Ok(b) => gpes.push(b),
            Err(()) => skipped += 1,
        }
    }
    if f.hw_reduced() {
        let sc = f.sleep_control.ok_or(PowerOffError::HwReducedWithoutSleepControl)?;
        return Ok(PowerOffPlan { s5, gpes, gpes_skipped: skipped, acpi_enable: None, pm1a_evt: None, pm1b_evt: None, pm1a_cnt: None, pm1b_cnt: None, sleep_control: Some(sc) });
    }
    let pm1a_cnt = f.pm1a_cnt.ok_or(PowerOffError::NoPm1Control)?;
    let acpi_enable = (sci_en == Some(false) && f.smi_cmd != 0 && f.smi_cmd <= 0xFFFF && f.acpi_enable != 0).then_some((f.smi_cmd as u16, f.acpi_enable));
    Ok(PowerOffPlan {
        s5,
        gpes,
        gpes_skipped: skipped,
        acpi_enable,
        pm1a_evt: f.pm1a_evt,
        pm1b_evt: f.pm1b_evt,
        pm1a_cnt: Some(pm1a_cnt),
        pm1b_cnt: f.pm1b_cnt,
        sleep_control: None,
    })
}

/// PM1 control with SLP_TYP = `typ` and SLP_EN as asked, other bits from `cur` (pure).
pub fn pm1_value(cur: u16, typ: u8, en: bool) -> u16 {
    (cur & !(SLP_TYP_MASK | SLP_EN)) | ((typ as u16 & 7) << SLP_TYP_SHIFT) | if en { SLP_EN } else { 0 }
}

fn refused(g: &Gas) -> PowerOffError {
    PowerOffError::Refused(format!("{} {:#x}", g.space.name(), g.address))
}

fn write_io(port: u64, bytes: u32, v: u32) -> Result<(), PowerOffError> {
    let g = Gas::io(port as u32, bytes as u8);
    live::check_gas(&g, bytes, false).map_err(|_| refused(&g))?;
    let p = port as u16;
    let r = match bytes {
        1 => hal::portio::outb(p, v as u8),
        2 => hal::portio::outw(p, v as u16),
        _ => hal::portio::outl(p, v),
    };
    r.map_err(|_| refused(&g))
}

/// Writes a register a table names (I/O or memory, 1/2/4 bytes) after the guard allowed it.
fn write_gas(g: &Gas, bytes: u32, v: u32) -> Result<(), PowerOffError> {
    live::check_gas(g, bytes, false).map_err(|_| refused(g))?;
    match g.space {
        Space::Io => write_io(g.address, bytes, v),
        Space::Memory => {
            let w = hal::mmio::map_device(g.address, bytes as usize, hal::paging::CacheType::Uncached).map_err(|_| refused(g))?;
            let r = match bytes {
                1 => w.write8(0, v as u8),
                2 => w.write16(0, v as u16),
                _ => w.write32(0, v),
            };
            r.map_err(|_| refused(g))
        }
        _ => Err(refused(g)),
    }
}

fn read16(g: &Gas) -> Option<u16> {
    live::read_reg(g, 2).map(|v| v as u16)
}

/// Powers the machine off (the console command; QEMU). Returns only when it did not work. Logs every step.
pub fn poweroff(wait: &dyn Fn(u64)) -> PowerOffError {
    let Some(a) = live::tables() else { return PowerOffError::NoTables };
    let plan = match poweroff_plan(a, live::sci_en_at_boot()) {
        Ok(p) => p,
        Err(e) => {
            klog::event!(Error, "power", "off.no_plan", error = format!("{e:?}"));
            return e;
        }
    };
    match run_poweroff(&plan, wait) {
        Ok(()) => {
            klog::event!(Error, "power", "off.still_running"; "power-off did not happen within 1 s: hold the power button");
            PowerOffError::StillRunning
        }
        Err(e) => {
            klog::event!(Error, "power", "off.failed", error = format!("{e:?}"));
            e
        }
    }
}

fn run_poweroff(p: &PowerOffPlan, wait: &dyn Fn(u64)) -> Result<(), PowerOffError> {
    // 1. Every GPE off and its status cleared (W1C), byte by byte.
    let mut gpe_bytes = 0;
    for b in &p.gpes {
        for i in 0..b.len as u64 {
            write_io(b.enable + i, 1, 0)?;
            write_io(b.status + i, 1, 0xFF)?;
            gpe_bytes += 1;
        }
    }
    klog::event!(Info, "power", "off.gpes", bytes = gpe_bytes, blocks = p.gpes.len(), skipped_blocks = p.gpes_skipped);
    // 2. ACPI mode, if the firmware left it off.
    if let Some((port, val)) = p.acpi_enable {
        let g = Gas::io(port as u32, 1);
        live::check_gas(&g, 1, false).map_err(|_| refused(&g))?;
        hal::portio::outb(port, val).map_err(|_| refused(&g))?;
        let end = time::now_ns().saturating_add(ACPI_ENABLE_WAIT_NS);
        let on = loop {
            if p.pm1a_cnt.as_ref().and_then(read16).is_some_and(|v| v & SCI_EN != 0) {
                break true;
            }
            if time::now_ns() >= end {
                break false;
            }
            time::delay_us(1000);
        };
        klog::event!(Info, "power", "off.acpi_enable", smi_cmd = klog::Hex(port as u64), value = klog::Hex(val as u64), sci_en = on);
        if !on {
            return Err(PowerOffError::AcpiModeNotEntered);
        }
    }
    // 3. Hardware-reduced ACPI: the sleep control register.
    if let Some(sc) = p.sleep_control {
        let v = ((p.s5.typ_a & 7) << HWR_SLP_TYP_SHIFT) | HWR_SLP_EN;
        klog::event!(Info, "power", "off.sleep_control", slp_typ = p.s5.typ_a);
        klog::flush_emergency();
        hal::cpu::wbinvd();
        write_gas(&sc, 1, v as u32)?;
        wait(OFF_WAIT_NS);
        return Ok(());
    }
    // 4. Fixed-event status cleared (wake status among them), then SLP_TYP, then SLP_EN (ACPICA order).
    for g in [p.pm1a_evt, p.pm1b_evt].into_iter().flatten() {
        write_gas(&g, 2, ALL_FIXED_STATUS as u32)?;
    }
    let cnt: Vec<(Gas, u8)> = [(p.pm1a_cnt, p.s5.typ_a), (p.pm1b_cnt, p.s5.typ_b)].into_iter().filter_map(|(g, t)| g.map(|g| (g, t))).collect();
    let cur: Vec<u16> = cnt.iter().map(|(g, _)| read16(g).unwrap_or(SCI_EN)).collect();
    klog::event!(Info, "power", "off.slp_typ", typ_a = p.s5.typ_a, typ_b = p.s5.typ_b, pm1_cnt = cnt.len());
    klog::flush_emergency();
    // ACPICA flushes the CPU cache before every sleep state above S1 (`hwsleep.c:114`, ACPI_FLUSH_CPU_CACHE).
    hal::cpu::wbinvd();
    for ((g, t), c) in cnt.iter().zip(&cur) {
        write_gas(g, 2, pm1_value(*c, *t, false) as u32)?;
    }
    for ((g, t), c) in cnt.iter().zip(&cur) {
        write_gas(g, 2, pm1_value(*c, *t, true) as u32)?;
    }
    wait(OFF_WAIT_NS);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fadt::{ResetReg, boot_arch, flags};

    const HARD: Step = Step::Cf9 { code: 0x06 };
    const FULL: Step = Step::Cf9 { code: 0x0E };

    #[test]
    fn cf9_texts() {
        assert_eq!(HARD.describe(), "0xcf9 <- 2, then 6");
        assert_eq!(FULL.describe(), "0xcf9 <- 2, then 0xe (full reset)");
        assert!(!HARD.declared() && !FULL.declared());
    }

    fn minipc() -> Fadt {
        crate::fadt::parse(&crate::testkit::fadt_minipc(0x1000), 0x2000, &mut crate::Notes::new()).expect("Mini-PC FADT")
    }

    #[test]
    fn minipc_reboot_order() {
        let p = reboot_plan(Some(&minipc()));
        assert_eq!(p.steps, alloc::vec![Step::FadtIo { port: 0xB2, value: 0xBE }, HARD, FULL, Step::TripleFault]);
        assert!(p.declared());
        assert!(p.skipped.iter().any(|m| m.contains("keyboard controller")));
    }

    #[test]
    fn q35_cf9_and_8042() {
        let mut f = minipc();
        f.reset = Some(ResetReg { reg: Gas { space: Space::Io, bit_width: 8, bit_offset: 0, access_size: 1, address: 0xCF9 }, value: 6 });
        f.boot_arch |= boot_arch::I8042;
        let p = reboot_plan(Some(&f));
        assert_eq!(p.steps, alloc::vec![Step::FadtCf9 { value: 6 }, HARD, FULL, Step::Kbc { declared: true }, Step::TripleFault]);
        f.flags &= !flags::RESET_REG_SUP;
        let p = reboot_plan(Some(&f));
        assert_eq!(p.steps[0], HARD, "an unsupported reset register is not used");
        assert!(p.skipped[0].contains("names none"));
        f.flags |= flags::RESET_REG_SUP;
        f.reset = Some(ResetReg { reg: Gas { space: Space::Memory, bit_width: 8, bit_offset: 0, access_size: 1, address: 0xFED8_0000 }, value: 0xE });
        assert_eq!(reboot_plan(Some(&f)).steps[0], Step::FadtMem { addr: 0xFED8_0000, value: 0xE });
        f.reset = Some(ResetReg { reg: Gas { space: Space::PciConfig, bit_width: 8, bit_offset: 0, access_size: 1, address: 0x1_0000 }, value: 1 });
        assert_eq!(reboot_plan(Some(&f)).steps[0], HARD, "PCI config space reset register: skipped");
    }

    #[test]
    fn without_a_fadt() {
        let p = reboot_plan(None);
        assert_eq!(p.steps, alloc::vec![HARD, FULL, Step::Kbc { declared: false }, Step::TripleFault]);
        assert!(!p.declared(), "guesses only");
        assert_eq!(p.steps.last(), Some(&Step::TripleFault), "the triple fault always ends the list");
    }

    fn tables(f: Fadt, s5: Option<S5>) -> Acpi {
        Acpi { fadt: Some(f), s5: s5.map(|s| (s, *b"DSDT", 0x1000)), ..Default::default() }
    }

    #[test]
    fn minipc_poweroff_plan() {
        let a = tables(minipc(), Some(S5 { typ_a: 5, typ_b: 5 }));
        let p = poweroff_plan(&a, Some(false)).unwrap();
        // GPE0 0x820, 8 bytes: status 0x820-0x823, enable 0x824-0x827.
        assert_eq!(p.gpes, alloc::vec![GpeBlock { status: 0x820, enable: 0x824, len: 4 }]);
        assert_eq!(p.acpi_enable, Some((0xB2, 0xA0)), "SCI_EN 0 at handover: ACPI_ENABLE through SMI_CMD");
        assert_eq!(p.pm1a_cnt.map(|g| g.address), Some(0x804));
        assert_eq!(p.pm1a_evt.map(|g| g.address), Some(0x800));
        assert!(p.pm1b_cnt.is_none() && p.sleep_control.is_none());
        assert_eq!(poweroff_plan(&a, Some(true)).unwrap().acpi_enable, None, "already in ACPI mode");
        assert_eq!(poweroff_plan(&a, None).unwrap().acpi_enable, None, "unknown: do not poke SMM");
    }

    #[test]
    fn poweroff_refusals() {
        assert_eq!(poweroff_plan(&tables(minipc(), None), Some(true)), Err(PowerOffError::NoS5));
        assert_eq!(poweroff_plan(&Acpi::default(), None), Err(PowerOffError::NoTables));
        let mut f = minipc();
        f.flags |= flags::HW_REDUCED_ACPI;
        assert_eq!(poweroff_plan(&tables(f.clone(), Some(S5 { typ_a: 5, typ_b: 5 })), None), Err(PowerOffError::HwReducedWithoutSleepControl));
        f.sleep_control = Some(Gas::io(0x404, 1));
        let p = poweroff_plan(&tables(f, Some(S5 { typ_a: 5, typ_b: 5 })), None).unwrap();
        assert_eq!(p.sleep_control.map(|g| g.address), Some(0x404));
        let mut f = minipc();
        f.pm1a_cnt = None;
        assert_eq!(poweroff_plan(&tables(f, Some(S5 { typ_a: 5, typ_b: 5 })), None), Err(PowerOffError::NoPm1Control));
    }

    #[test]
    fn pm1_bits() {
        assert_eq!(pm1_value(SCI_EN, 5, false), 0x1401);
        assert_eq!(pm1_value(SCI_EN, 5, true), 0x3401);
        assert_eq!(pm1_value(0xFFFF, 0, false), 0xC3FF, "old SLP_TYP and SLP_EN cleared");
        assert_eq!(pm1_value(0, 9, true), (1 << 10) | SLP_EN, "SLP_TYP is 3 bits");
    }
}
