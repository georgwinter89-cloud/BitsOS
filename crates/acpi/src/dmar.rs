//! The Intel VT-d table (DMAR) and the state its IOMMUs were handed over in (GP B7). BitsOS has no IOMMU driver:
//! this only reads, once, before any initcall, so before any driver can turn bus mastering on. The rules (table
//! walk, status bits, verdict) are `gp_platform::dmar`; here are the notes, the guarded register reads, the boot
//! lines and `dma_gate`, which every DMA driver's enable path asks.
//!
//! A machine without the table (the Mini-PC, every AMD machine, plain QEMU) gets `NoTable`: no register is read, no
//! line is logged and the gate is open, as before.

use crate::sdt::{Gas, Space};
use crate::{Note, Notes};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use gp_platform::dmar as rules;
pub use rules::{DmaVerdict, Dmar, Drhd, UnitRegs, UnitState};
use ksync::Once;

pub const SIG: [u8; 4] = *b"DMAR";

pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Option<Dmar> {
    let p = rules::parse(t);
    for x in &p.problems {
        notes.push(Note::table(SIG, phys, x.what, x.at));
    }
    p.dmar
}

/// One unit's registers through `read32` (`None` from it: that address could not be read). Five or six reads,
/// none of them with a side effect (version, capabilities, global status, protected-memory enable).
pub fn read_unit(u: &Drhd, read32: &mut dyn FnMut(u64) -> Option<u32>) -> Option<UnitRegs> {
    let mut r64 = |off: u64| Some(read32(u.address.checked_add(off)?)? as u64 | (read32(u.address.checked_add(off + 4)?)? as u64) << 32);
    let cap = r64(rules::REG_CAP)?;
    let ecap = r64(rules::REG_ECAP)?;
    let ver = read32(u.address + rules::REG_VER)?;
    let gsts = read32(u.address + rules::REG_GSTS)?;
    // All ones: nothing answers there, and a capability word of all ones must not send us to PMEN.
    let pmen = if cap != u64::MAX && rules::has_protected_regions(cap) { Some(read32(u.address + rules::REG_PMEN)?) } else { None };
    Some(UnitRegs { ver, cap, ecap, gsts, pmen })
}

/// What the boot found: the table, each unit with what its registers said, and the verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Survey {
    pub table: Dmar,
    pub units: Vec<(Drhd, Option<UnitRegs>, UnitState)>,
    pub verdict: DmaVerdict,
}

pub fn survey(table: &Dmar, read32: &mut dyn FnMut(u64) -> Option<u32>) -> Survey {
    let units: Vec<_> = table
        .units
        .iter()
        .map(|u| {
            let regs = read_unit(u, read32);
            (*u, regs, rules::unit_state(regs))
        })
        .collect();
    let states: Vec<UnitState> = units.iter().map(|u| u.2).collect();
    Survey { table: table.clone(), units, verdict: rules::verdict(Some(&states)) }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

impl Survey {
    /// One line per unit and the verdict line, for `hw_info` and the Gaming-PC's report page (wave B10).
    pub fn lines(&self) -> Vec<String> {
        let t = &self.table;
        let mut v = alloc::vec![format!(
            "acpi: DMAR width {} flags {:#x} (platform opt-in {}), units {}, rmrr {}, atsr {}, satc {}",
            t.host_address_width, t.flags, t.platform_opt_in(), t.units.len(), t.rmrr, t.atsr, t.satc
        )];
        for (u, regs, st) in &self.units {
            v.push(match (st, regs) {
                (UnitState::Present { version, translation, interrupt_remapping, compat_interrupts, protected_memory, .. }, Some(r)) => format!(
                    "acpi: DMAR unit {:#x} ver {}.{} gsts {:#010x} translation {} interrupt_remapping {}{} protected_memory {}{}",
                    u.address,
                    version.0,
                    version.1,
                    r.gsts,
                    on_off(*translation),
                    on_off(*interrupt_remapping),
                    if *interrupt_remapping && !*compat_interrupts { " (plain interrupts blocked)" } else { "" },
                    on_off(*protected_memory),
                    if u.include_all() { " (all devices)" } else { "" }
                ),
                (UnitState::Absent, _) => format!("acpi: DMAR unit {:#x} does not answer (all ones)", u.address),
                _ => format!("acpi: DMAR unit {:#x} not readable", u.address),
            });
        }
        v.push(format!("acpi: DMA {}", verdict_text(self.verdict)));
        v
    }
}

/// The verdict in a sentence (English: `hw_info` and the log; the report page has its own German text).
pub fn verdict_text(v: DmaVerdict) -> &'static str {
    match v {
        DmaVerdict::NoTable => "free: no DMAR table",
        DmaVerdict::Free => "free: no IOMMU translates or protects memory",
        DmaVerdict::Unknown => "state unknown: an IOMMU could not be read; drivers start and the hardware decides",
        DmaVerdict::TranslationOn => "BLOCKED: the firmware left DMA translation on; no DMA driver starts",
        DmaVerdict::ProtectedMemoryOn => "BLOCKED: the firmware left a protected memory region on; no DMA driver starts",
    }
}

static SURVEY: Once<Option<Survey>> = Once::new();

/// Reads the units of the table (if any) through the register guard and logs what it found. Called once from
/// `live::init`, after the tables are kept and before the first initcall.
pub(crate) fn boot(table: Option<&Dmar>) {
    let s = SURVEY.get_or_init(|| {
        // `check_mem` first: it logs why the guard refused an address, which `read_reg` alone would not say.
        table.map(|t| {
            survey(t, &mut |addr| {
                crate::live::check_mem(addr, 4).ok()?;
                crate::live::read_reg(&Gas { space: Space::Memory, bit_width: 32, bit_offset: 0, access_size: 3, address: addr }, 4)
            })
        })
    });
    let Some(s) = s else { return };
    log(s);
}

fn log(s: &Survey) {
    use klog::Hex;
    let t = &s.table;
    klog::event!(Info, "acpi", "dmar", width = t.host_address_width, flags = Hex(t.flags as u64), opt_in = t.platform_opt_in(), units = t.units.len(), rmrr = t.rmrr, atsr = t.atsr, satc = t.satc);
    for (u, regs, st) in &s.units {
        match (st, regs) {
            (UnitState::Present { version, translation, root_table_set, queued_invalidation, interrupt_remapping, compat_interrupts, protected_memory }, Some(r)) => {
                klog::event!(Info, "acpi", "dmar.unit", base = Hex(u.address), include_all = u.include_all(), major = version.0, minor = version.1, cap = Hex(r.cap), ecap = Hex(r.ecap), gsts = Hex(r.gsts as u64),
                    translation = *translation, root_table = *root_table_set, queued_invalidation = *queued_invalidation, interrupt_remapping = *interrupt_remapping, compat_interrupts = *compat_interrupts,
                    protected_memory = *protected_memory);
                if rules::blocks_plain_interrupts(st) {
                    klog::event!(Warn, "acpi", "dmar.interrupts_remapped", base = Hex(u.address); "the firmware left interrupt remapping on without the compatibility format: device interrupts may not arrive");
                }
            }
            (UnitState::Absent, _) => klog::event!(Warn, "acpi", "dmar.unit_absent", base = Hex(u.address); "the IOMMU the table names reads all ones"),
            _ => klog::event!(Warn, "acpi", "dmar.unit_unreadable", base = Hex(u.address); "the IOMMU's registers could not be read (register guard or mapping)"),
        }
    }
    if s.verdict.blocks_dma() {
        klog::event!(Error, "acpi", "dma.blocked", verdict = s.verdict.name(); "the firmware left an IOMMU active and BitsOS has no IOMMU driver: no DMA driver starts (disks, USB, network, sound stay off)");
    } else {
        klog::event!(Info, "acpi", "dma.state", verdict = s.verdict.name(), blocked = false);
    }
}

/// What the boot found (`None`: no DMAR table, or `init` has not run).
pub fn found() -> Option<&'static Survey> {
    SURVEY.get().and_then(|s| s.as_ref())
}

/// The verdict for this boot. Before `acpi::init` (nobody enables a device that early) and without a table: free.
pub fn verdict() -> DmaVerdict {
    match KTEST_BLOCK.load(core::sync::atomic::Ordering::Acquire) {
        1 => DmaVerdict::TranslationOn,
        2 => DmaVerdict::ProtectedMemoryOn,
        _ => found().map_or(DmaVerdict::NoTable, |s| s.verdict),
    }
}

/// ktests only (`cp.dmar.enable_refused`): no machine in the suite has an IOMMU its firmware left translating, so
/// the test of PCI's refusal answers with a blocking verdict for its own length. It can only block: a verdict that
/// frees DMA is not taken, so nothing can open the gate the boot's survey closed. `None` ends it.
pub fn ktest_block(v: Option<DmaVerdict>) {
    let code = match v {
        Some(DmaVerdict::TranslationOn) => 1,
        Some(DmaVerdict::ProtectedMemoryOn) => 2,
        _ => 0,
    };
    KTEST_BLOCK.store(code, core::sync::atomic::Ordering::Release);
}

static KTEST_BLOCK: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// May a driver let its device do DMA (set bus master)? `Err` names why not. PCI's enable path asks this before
/// it sets the bus-master bit; with no DMAR table the answer is always yes.
pub fn dma_gate() -> Result<(), DmaVerdict> {
    let v = verdict();
    if v.blocks_dma() { Err(v) } else { Ok(()) }
}
