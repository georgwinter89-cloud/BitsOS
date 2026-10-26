//! CPU temperatures as Linux' k10temp reads them (linux-6.6/drivers/hwmon/k10temp.c): Tctl from SMN
//! `0x59800`, per-CCD temperatures from `0x59800 + ccd_offset + 4*i` where bit 11 says the CCD exists. SMN is
//! reached through the root complex 00:00.0's index/data pair at config 0x60/0x64
//! (linux-6.6/arch/x86/kernel/amd_nb.c:174-215). Only reads: the index write selects a register, nothing
//! on the chip changes. Not AMD Zen or no AMD root complex (QEMU): no temperatures, never a guess.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const REPORTED_TEMP_CTRL: u32 = 0x0005_9800;
const CUR_TEMP_SHIFT: u32 = 21;
const RANGE_SEL: u32 = 1 << 19;
const TJ_SEL: u32 = 3 << 16;
const CCD_VALID: u32 = 1 << 11;
const CCD_MASK: u32 = 0x7ff;
const AMD_PCI: u16 = 0x1022;


/// Family and model from CPUID leaf 1 EAX (extended model for families 6 and 0xF, as Linux `x86_model`).
pub fn family_model(eax: u32) -> (u32, u32) {
    let base = (eax >> 8) & 0xf;
    let family = if base == 0xf { base + ((eax >> 20) & 0xff) } else { base };
    let model = ((eax >> 4) & 0xf) | if base == 0xf || base == 6 { ((eax >> 16) & 0xf) << 4 } else { 0 };
    (family, model)
}

/// Zen families k10temp reads through SMN, and their CCD register offset and how many CCDs to probe
/// (linux-6.6/drivers/hwmon/k10temp.c:423-480). `Some((None, 0))`: Tctl only.
pub fn layout(family: u32, model: u32) -> Option<(Option<u32>, u32)> {
    let ccd = match (family, model) {
        (0x17 | 0x18, 0x01 | 0x08 | 0x11 | 0x18) => Some((0x154, 4)),
        (0x17 | 0x18, 0x31 | 0x60 | 0x68 | 0x71) => Some((0x154, 8)),
        (0x17 | 0x18, 0xa0..=0xaf) => Some((0x300, 8)),
        (0x17 | 0x18, _) => None,
        (0x19, 0x00..=0x01 | 0x21 | 0x50..=0x5f) => Some((0x154, 8)),
        (0x19, 0x40..=0x4f) => Some((0x300, 8)),
        (0x19, 0x60..=0x7f) => Some((0x308, 8)),
        (0x19, 0x10..=0x1f | 0xa0..=0xaf) => Some((0x300, 12)),
        (0x19 | 0x1a, _) => None,
        _ => return None,
    };
    Some(ccd.map_or((None, 0), |(o, n)| (Some(o), n)))
}

/// Tctl in thousandths of a degree (k10temp `get_raw_temp`; below 0 shown as 0 like Linux).
pub fn tctl_mc(reg: u32) -> u64 {
    let t = (reg >> CUR_TEMP_SHIFT) as i64 * 125;
    let t = if reg & RANGE_SEL != 0 || reg & TJ_SEL == TJ_SEL { t - 49_000 } else { t };
    t.max(0) as u64
}

/// A CCD's temperature in thousandths of a degree (k10temp.c:227).
pub fn ccd_mc(reg: u32) -> u64 {
    ((reg & CCD_MASK) as i64 * 125 - 49_000).max(0) as u64
}

/// Why one SMN read gave no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Smn {
    /// Another reader had the index/data pair: skip this value, try again later.
    Busy,
    /// The config access itself failed.
    Failed,
}

/// Reads one SMN register. The kernel's port is `RootSmn`; tests drive `Zen` with a made-up one.
pub trait SmnPort {
    fn read(&self, addr: u32) -> Result<u32, Smn>;
}

/// SMN through the AMD root complex 00:00.0 (linux-6.6/arch/x86/kernel/amd_nb.c:174-215).
pub struct RootSmn;

impl SmnPort for RootSmn {
    fn read(&self, addr: u32) -> Result<u32, Smn> {
        pci::smn_read(addr).ok_or(Smn::Failed)
    }
}

/// Why probing found no sensors: `Again` (an SMN read was busy: probe in the next round) or absent for
/// this boot, with the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Again,
    Absent(&'static str),
}

/// The root complex must be AMD's (QEMU's q35 has Intel's): SMN exists only there.
pub fn root_ok(vendor: Option<u16>) -> Result<(), Probe> {
    match vendor {
        None => Err(Probe::Absent("no root complex at 00:00.0")),
        Some(AMD_PCI) => Ok(()),
        Some(_) => Err(Probe::Absent("00:00.0 is not AMD's root complex")),
    }
}

pub struct Zen<P: SmnPort = RootSmn> {
    port: P,
    /// SMN address and name of each temperature.
    regs: Vec<(u32, String)>,
}

impl Zen<RootSmn> {
    /// Finds the sensors, or says why there are none (yet).
    pub fn detect() -> Result<Zen, Probe> {
        Zen::on_root(Zen::cpu().map_err(Probe::Absent)?)
    }

    /// The CPU part: an AMD Zen family k10temp reads by SMN, and its CCD layout.
    pub fn cpu() -> Result<(Option<u32>, u32), &'static str> {
        let v = hal::cpu::cpuid(0, 0);
        if (v.ebx, v.edx, v.ecx) != (0x6874_7541, 0x6974_6e65, 0x444d_4163) {
            return Err("not an AMD CPU");
        }
        let (family, model) = family_model(hal::cpu::cpuid(1, 0).eax);
        layout(family, model).ok_or("not a Zen family k10temp reads by SMN")
    }

    /// The PCI part (after the kernel's PCI scan): the root complex and which CCD sensors answer.
    pub fn on_root(layout: (Option<u32>, u32)) -> Result<Zen, Probe> {
        let root = pci::function(pci::Loc::new(0, 0, 0, 0));
        root_ok(root.as_ref().map(|r| r.vendor))?;
        root.ok_or(Probe::Absent("no root complex at 00:00.0"))?;
        Zen::probe(RootSmn, layout)
    }
}

impl<P: SmnPort> Zen<P> {
    /// Tctl must answer; each CCD sensor whose valid bit is set joins. A busy read anywhere means
    /// `Again`: deciding on it would lose Tctl or a CCD for the whole boot.
    pub fn probe(port: P, (ccd, count): (Option<u32>, u32)) -> Result<Zen<P>, Probe> {
        let check = |r: Result<u32, Smn>| match r {
            Err(Smn::Busy) => Err(Probe::Again),
            other => Ok(other.ok()),
        };
        check(port.read(REPORTED_TEMP_CTRL))?.ok_or(Probe::Absent("SMN not readable"))?;
        let mut regs = alloc::vec![(REPORTED_TEMP_CTRL, String::from("tctl"))];
        if let Some(off) = ccd {
            for i in 0..count {
                let at = REPORTED_TEMP_CTRL + off + 4 * i;
                if check(port.read(at))?.is_some_and(|r| r & CCD_VALID != 0) {
                    regs.push((at, format!("ccd{i}")));
                }
            }
        }
        Ok(Zen { port, regs })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.regs.iter().map(|r| r.1.as_str())
    }

    /// Every sensor that answered, in thousandths of a degree (a busy or failed read is left out this time).
    pub fn read(&self) -> Vec<(String, u64)> {
        let mut out = Vec::with_capacity(self.regs.len());
        for (i, (addr, name)) in self.regs.iter().enumerate() {
            if let Ok(r) = self.port.read(*addr) {
                out.push((name.clone(), if i == 0 { tctl_mc(r) } else { ccd_mc(r) }));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_and_model_of_known_cpus() {
        // Ryzen 7 8845HS (Phoenix): family 0x19 model 0x75, stepping 2.
        assert_eq!(family_model(0x00a7_0f52), (0x19, 0x75));
        assert_eq!(layout(0x19, 0x75), Some((Some(0x308), 8)));
        assert_eq!(layout(0x1a, 0x24), Some((None, 0)));
        assert_eq!(layout(0x17, 0x90), Some((None, 0)));
        assert_eq!(layout(0x06, 0x9a), None, "Intel family 6");
        assert_eq!(family_model(0x0009_06a3), (6, 0x9a));
    }

    #[test]
    fn register_values_become_degrees_as_in_k10temp() {
        // 50.000 C: 400 steps of 0.125.
        assert_eq!(tctl_mc(400 << 21), 50_000);
        // Range select: 49 below.
        assert_eq!(tctl_mc((400 << 21) | RANGE_SEL), 1_000);
        assert_eq!(tctl_mc((300 << 21) | TJ_SEL), 0, "below 0 shows 0");
        assert_eq!(ccd_mc(CCD_VALID | 800), 51_000);
    }

    /// A made-up SMN: registers by address, and how many of the next reads come back busy.
    struct Fake {
        regs: alloc::collections::BTreeMap<u32, u32>,
        busy: core::cell::Cell<u32>,
    }

    impl SmnPort for Fake {
        fn read(&self, addr: u32) -> Result<u32, Smn> {
            if self.busy.get() > 0 {
                self.busy.set(self.busy.get() - 1);
                return Err(Smn::Busy);
            }
            self.regs.get(&addr).copied().ok_or(Smn::Failed)
        }
    }

    fn fake(busy: u32) -> Fake {
        let regs = [(REPORTED_TEMP_CTRL, 400 << 21), (REPORTED_TEMP_CTRL + 0x308, CCD_VALID | 800), (REPORTED_TEMP_CTRL + 0x308 + 4, 0)];
        Fake { regs: regs.into_iter().collect(), busy: core::cell::Cell::new(busy) }
    }

    #[test]
    fn probing_finds_tctl_and_valid_ccds_and_a_busy_read_means_again() {
        let layout = (Some(0x308), 2);
        let z = Zen::probe(fake(0), layout).unwrap();
        assert_eq!(z.names().collect::<Vec<_>>(), ["tctl", "ccd0"], "ccd1 has no valid bit");
        assert_eq!(z.read(), [(String::from("tctl"), 50_000), (String::from("ccd0"), 51_000)]);
        // Busy on Tctl or on a CCD: decide in a later round, never "absent" for the boot.
        assert_eq!(Zen::probe(fake(1), layout).err(), Some(Probe::Again));
        let mut f = fake(0);
        f.regs.remove(&REPORTED_TEMP_CTRL);
        assert_eq!(Zen::probe(f, layout).err(), Some(Probe::Absent("SMN not readable")));
        // A busy read while sampling leaves that one value out.
        let z = Zen::probe(fake(0), layout).unwrap();
        z.port.busy.set(1);
        assert_eq!(z.read(), [(String::from("ccd0"), 51_000)]);
    }

    #[test]
    fn only_amds_root_complex_has_smn() {
        assert_eq!(root_ok(Some(0x1022)), Ok(()));
        assert_eq!(root_ok(Some(0x8086)), Err(Probe::Absent("00:00.0 is not AMD's root complex")));
        assert_eq!(root_ok(None), Err(Probe::Absent("no root complex at 00:00.0")));
    }
}
