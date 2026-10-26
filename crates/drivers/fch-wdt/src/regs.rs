//! The AMD FCH watchdog's registers and the chip sequence (`V1:hw/watchdog.asm:103-154`, Linux
//! `drivers/watchdog/sp5100_tco.c`, layout `efch_mmio`). Everything here works over `Regs`, so the host tests
//! and the ktests drive the same sequence against `Fake`; the kernel uses `Mmio` (the ACPI MMIO page of the FCH).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// The FCH's ACPI MMIO block (`EFCH_PM_ACPI_MMIO_ADDR`, linux-6.6/drivers/watchdog/sp5100_tco.h:85).
pub const ACPI_MMIO: u64 = 0xFED8_0000;
/// PM registers inside it (`V1:hw/watchdog.asm:15`, sp5100_tco.h:86).
pub const PM_OFF: usize = 0x300;
/// Watchdog control (+0) and count (+4) inside it (`V1:hw/watchdog.asm:16`, sp5100_tco.h:87). Linux prefers
/// 0xFEB00000 and uses this one as the alternative; V1 used this one on the Mini-PC and it reset the machine.
pub const WDT_OFF: usize = 0xB00;
/// Bytes to map from `ACPI_MMIO` (through the count register).
pub const WINDOW: usize = WDT_OFF + 8;

/// PM[0] bit 7: decode the watchdog's MMIO and let its timer run (`EFCH_PM_DECODEEN_WDT_TMREN`).
pub const PM0_DECODE: u8 = 0x80;
/// PM[3] bits 2..3: watchdog disabled (`EFCH_PM_WATCHDOG_DISABLE`); bits 0..1 = 3: the count is in seconds.
pub const PM3_DISABLE: u8 = 0x0C;
pub const PM3_SECONDS: u8 = 0x03;
/// PM[4] (`EFCH_PM_ISACONTROL`) bit 1: the ACPI MMIO window is decoded (`EFCH_PM_ISACONTROL_MMIOEN`,
/// sp5100_tco.h:81-83). Linux takes 0xFED80B00 only when it is set (sp5100_tco.c:402-405); read through the same
/// window, so it can only confirm a window that answers. Logged, not required (V1 never read it).
pub const PM_ISACONTROL: usize = 4;
pub const PM4_MMIOEN: u8 = 0x02;

/// PM 0xC0..0xC3 `S5_RESET_STATUS`: why the last start happened (sticky bits, written 1 to clear; the names as
/// Linux 6.16 prints them, `arch/x86/kernel/cpu/amd.c` s5_reset_reason_txt). Read once at boot, so a watchdog
/// that set FIRED but did not reset the machine can be told apart from one whose reset hung.
pub const PM_S5_RESET_STATUS: usize = 0xC0;
pub const S5_RESET_REASONS: [(u32, &str); 21] = [
    (0, "thermal_pin"), (1, "power_button_4s"), (2, "shutdown_pin"), (4, "remote_asf_poweroff"),
    (9, "cpu_thermal"), (16, "reset_pin"), (17, "software_pci_reset"), (18, "cf9_4"), (19, "cf9_6"),
    (20, "cf9_e"), (21, "acpi_state_change"), (22, "kb_reset_pin"), (23, "cpu_shutdown"),
    (24, "failed_boot_timer"), (25, "watchdog_expired"), (26, "remote_asf_reset"), (27, "sync_flood_error"),
    (28, "reserved_28"), (29, "fch_mp1_warm_reset_handshake_failed"), (30, "parity_error"), (31, "sync_flood_software"),
];

/// The four bytes of `S5_RESET_STATUS` through the PM window.
pub fn s5_reset_status(r: &dyn Regs) -> u32 {
    (0..4).fold(0, |v, i| v | (r.pm_read8(PM_S5_RESET_STATUS + i) as u32) << (8 * i))
}

/// The names of the set bits ("-" for none).
pub fn s5_reset_reasons(v: u32) -> alloc::string::String {
    let names: alloc::vec::Vec<&str> = S5_RESET_REASONS.iter().filter(|(b, _)| v & (1 << b) != 0).map(|(_, n)| *n).collect();
    if names.is_empty() { alloc::string::String::from("-") } else { names.join(",") }
}

pub const RUN: u32 = 1 << 0;
/// Set by a watchdog reset; write 1 to clear.
pub const FIRED: u32 = 1 << 1;
/// Clear: the action is a reset (set: power off).
pub const POWEROFF: u32 = 1 << 2;
/// Read only: the firmware disabled the watchdog.
pub const DISABLED: u32 = 1 << 3;
/// Write 1: reload the counter (a kick).
pub const TRIGGER: u32 = 1 << 7;

/// The count register is 16 bits wide (`max_timeout = 0xffff`, sp5100_tco.c:537).
pub const MAX_SECONDS: u32 = 0xFFFF;

/// Byte/dword access to the PM block and the watchdog block.
pub trait Regs: Send + Sync {
    fn pm_read8(&self, off: usize) -> u8;
    fn pm_write8(&self, off: usize, v: u8);
    fn ctl_read(&self) -> u32;
    fn ctl_write(&self, v: u32);
    fn count_read(&self) -> u32;
    fn count_write(&self, v: u32);
}

/// The real block through an uncached window (`hal::mmio::map_device`). A failed access reads all ones and
/// writes nothing (the window is checked once at map time, so this does not happen).
pub struct Mmio(pub hal::mmio::IoMem);

impl Regs for Mmio {
    fn pm_read8(&self, off: usize) -> u8 {
        self.0.read8(PM_OFF + off).unwrap_or(0xFF)
    }
    fn pm_write8(&self, off: usize, v: u8) {
        let _ = self.0.write8(PM_OFF + off, v);
    }
    fn ctl_read(&self) -> u32 {
        self.0.read32(WDT_OFF).unwrap_or(u32::MAX)
    }
    fn ctl_write(&self, v: u32) {
        let _ = self.0.write32(WDT_OFF, v);
    }
    fn count_read(&self) -> u32 {
        self.0.read32(WDT_OFF + 4).unwrap_or(u32::MAX)
    }
    fn count_write(&self, v: u32) {
        let _ = self.0.write32(WDT_OFF + 4, v);
    }
}

/// A register block in memory that behaves like the chip where the driver can see it: FIRED is write-1-to-clear,
/// DISABLED is read only, a TRIGGER write is a kick (counted, reads back 0) and the counter "expires" when it
/// runs longer than its count without a kick (`would_fire`, with the test's clock).
pub struct Fake {
    pm: [AtomicU8; 8],
    ctl: AtomicU32,
    count: AtomicU32,
    kicks: AtomicU64,
    last_kick_ns: AtomicU64,
    writes: AtomicU64,
    /// The clock the fake stamps kicks with (tests pass theirs; `None` = 0).
    clock: Option<fn() -> u64>,
    decode_sticks: AtomicBool,
}

impl Fake {
    /// A block as firmware leaves it: decode off, count in an unknown unit, not running; `fired` / `disabled`
    /// as given.
    pub fn new(fired: bool, disabled: bool, clock: Option<fn() -> u64>) -> Fake {
        let f = Fake {
            pm: [const { AtomicU8::new(0) }; 8],
            ctl: AtomicU32::new(if fired { FIRED } else { 0 } | if disabled { DISABLED } else { 0 } | POWEROFF),
            count: AtomicU32::new(0),
            kicks: AtomicU64::new(0),
            last_kick_ns: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            clock,
            decode_sticks: AtomicBool::new(true),
        };
        f.pm[3].store(PM3_DISABLE, Ordering::Relaxed);
        f.pm[PM_ISACONTROL].store(PM4_MMIOEN, Ordering::Relaxed);
        f
    }
    /// The decode bit does not stay set (a chip that refuses: `enable` must report it).
    pub fn refuse_decode(&self) {
        self.decode_sticks.store(false, Ordering::Relaxed);
    }
    pub fn kicks(&self) -> u64 {
        self.kicks.load(Ordering::Acquire)
    }
    pub fn running(&self) -> bool {
        self.ctl.load(Ordering::Acquire) & RUN != 0
    }
    pub fn ctl(&self) -> u32 {
        self.ctl.load(Ordering::Acquire)
    }
    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Acquire)
    }
    pub fn pm(&self, off: usize) -> u8 {
        self.pm[off].load(Ordering::Acquire)
    }
    /// The real chip would have reset the machine by `now`: running, and no kick for longer than its count.
    pub fn would_fire(&self, now_ns: u64) -> bool {
        self.running() && now_ns.saturating_sub(self.last_kick_ns.load(Ordering::Acquire)) > self.count() as u64 * 1_000_000_000
    }
    fn now(&self) -> u64 {
        self.clock.map_or(0, |c| c())
    }
}

impl Regs for Fake {
    fn pm_read8(&self, off: usize) -> u8 {
        self.pm.get(off).map_or(0xFF, |r| r.load(Ordering::Acquire))
    }
    fn pm_write8(&self, off: usize, v: u8) {
        let v = if off == 0 && !self.decode_sticks.load(Ordering::Relaxed) { v & !PM0_DECODE } else { v };
        if let Some(r) = self.pm.get(off) {
            r.store(v, Ordering::Release);
        }
    }
    fn ctl_read(&self) -> u32 {
        self.ctl.load(Ordering::Acquire)
    }
    fn ctl_write(&self, v: u32) {
        self.writes.fetch_add(1, Ordering::AcqRel);
        let old = self.ctl.load(Ordering::Acquire);
        let mut new = (v & !(FIRED | DISABLED | TRIGGER)) | (old & DISABLED);
        if old & FIRED != 0 && v & FIRED == 0 {
            new |= FIRED;
        }
        if v & TRIGGER != 0 && new & RUN != 0 {
            self.kicks.fetch_add(1, Ordering::AcqRel);
            self.last_kick_ns.store(self.now(), Ordering::Release);
        }
        if old & RUN == 0 && new & RUN != 0 {
            self.last_kick_ns.store(self.now(), Ordering::Release);
        }
        self.ctl.store(new, Ordering::Release);
    }
    fn count_read(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }
    fn count_write(&self, v: u32) {
        self.count.store(v & MAX_SECONDS, Ordering::Release);
    }
}

/// A shared fake: the test keeps one handle to look at, the dog owns the other.
impl Regs for alloc::sync::Arc<Fake> {
    fn pm_read8(&self, off: usize) -> u8 {
        (**self).pm_read8(off)
    }
    fn pm_write8(&self, off: usize, v: u8) {
        (**self).pm_write8(off, v)
    }
    fn ctl_read(&self) -> u32 {
        (**self).ctl_read()
    }
    fn ctl_write(&self, v: u32) {
        (**self).ctl_write(v)
    }
    fn count_read(&self) -> u32 {
        (**self).count_read()
    }
    fn count_write(&self, v: u32) {
        (**self).count_write(v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipError {
    /// The decode/timer-enable bit did not stay set (Linux: "Failed to enable the timer").
    NoDecode,
    /// The firmware disabled the watchdog (control bit 3).
    Disabled,
    /// The control register reads all ones: nothing answers at that address.
    NoChip,
}

impl ChipError {
    pub fn text(self) -> &'static str {
        match self {
            ChipError::NoDecode => "the FCH did not keep the watchdog decode bit (PM[0] bit 7)",
            ChipError::Disabled => "disabled by the firmware (control bit 3)",
            ChipError::NoChip => "the control register reads all ones",
        }
    }
}

/// What the boot found in the control register before it changed anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootStatus {
    pub ctl: u32,
    pub count: u32,
    pub fired: bool,
    pub was_running: bool,
}

/// Decode on, count in seconds, not disabled (`V1:hw/watchdog.asm:104-109`, sp5100_tco.c:383-397 and
/// `tco_timer_enable_mmio`). Checks that the decode bit stayed.
pub fn enable(r: &dyn Regs) -> Result<(), ChipError> {
    let pm0 = r.pm_read8(0);
    if pm0 & PM0_DECODE == 0 {
        r.pm_write8(0, pm0 | PM0_DECODE);
    }
    if r.pm_read8(0) & PM0_DECODE == 0 {
        return Err(ChipError::NoDecode);
    }
    let pm3 = r.pm_read8(3);
    r.pm_write8(3, (pm3 & !PM3_DISABLE) | PM3_SECONDS);
    match r.ctl_read() {
        u32::MAX => Err(ChipError::NoChip),
        c if c & DISABLED != 0 => Err(ChipError::Disabled),
        _ => Ok(()),
    }
}

/// Reads the control register once and clears FIRED (written back as 1), leaving RUN as it was: the boot's
/// first look, before arming or stopping. Also when `enable` refused (V1 logs FIRED before it looks at DISABLED,
/// `V1:hw/watchdog.asm:112-118`); a register that reads all ones (nothing answers) has no FIRED.
pub fn read_and_clear_fired(r: &dyn Regs) -> BootStatus {
    let ctl = r.ctl_read();
    let answers = ctl != u32::MAX;
    let st = BootStatus { ctl, count: r.count_read(), fired: answers && ctl & FIRED != 0, was_running: answers && ctl & RUN != 0 };
    if st.fired {
        r.ctl_write(ctl & !TRIGGER);
    }
    st
}

/// Armed: reset as the action, `secs` in the counter, running and kicked once (`V1:hw/watchdog.asm:119-127`;
/// RUN and TRIGGER are separate writes as sp5100_tco.c:117 says they must be).
pub fn arm(r: &dyn Regs, secs: u32) -> Result<(), ChipError> {
    let v = r.ctl_read();
    if v == u32::MAX {
        return Err(ChipError::NoChip);
    }
    if v & DISABLED != 0 {
        return Err(ChipError::Disabled);
    }
    // FIRED stays in the value: written back as 1 it clears (V1 does the same).
    let v = v & !(POWEROFF | TRIGGER);
    r.ctl_write(v);
    r.count_write(secs.clamp(1, MAX_SECONDS));
    let v = v & !FIRED | RUN;
    r.ctl_write(v);
    r.ctl_write(v | TRIGGER);
    Ok(())
}

/// One kick (`V1:hw/watchdog.asm:180`, `tco_timer_ping`).
pub fn kick(r: &dyn Regs) {
    let v = r.ctl_read();
    if v != u32::MAX {
        r.ctl_write((v & !FIRED) | TRIGGER);
    }
}

/// A new count, then a kick so it counts from now (`V1:hw/watchdog.asm:wdt_grace`).
pub fn set_count_and_kick(r: &dyn Regs, secs: u32) {
    r.count_write(secs.clamp(1, MAX_SECONDS));
    kick(r);
}

/// Stopped (`V1:hw/watchdog.asm:wdt_stop`: RUN, TRIGGER and FIRED cleared in the written value).
pub fn stop(r: &dyn Regs) {
    let v = r.ctl_read();
    if v != u32::MAX {
        r.ctl_write(v & !(RUN | TRIGGER | FIRED));
    }
}

/// Whether the counter runs.
pub fn running(r: &dyn Regs) -> bool {
    let v = r.ctl_read();
    v != u32::MAX && v & RUN != 0
}
