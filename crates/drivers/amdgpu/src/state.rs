//! GX-7b live glue: the bring-up register owner over BAR5 with its gate shut (it writes nothing in this item),
//! the classification of the GPU's start state through it (`gpucore::classify`, pure), and the IOMMU's enable
//! bit (AMD-Vi: ACPI IVRS gives the IOMMU's registers; its control register is read, never written).

use crate::dcn::{self, DcnRegs};
use crate::gpucore::bringup::{Bringup, Gate, LOG_KEEP, Totals};
use crate::gpucore::classify::{self, State};
use crate::gpucore::disc::Discovery;
use crate::gpucore::psp::Hw;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::Mutex;

/// Why the live owner's gate is shut in this item.
pub const READ_ONLY: &str = "GX-7b reads only; the first real writes come with GX-7d";

/// The live register half under the owner: reads any classifier register in BAR5 (never the index/data pairs),
/// writes nothing, no GPU memory (GX-7d adds the carve-out mapping, the HDP flush and the clock).
pub struct LiveHw {
    regs: &'static DcnRegs,
    /// The only bytes it reads: the classifier's addresses from the discovery table.
    allowed: Vec<u32>,
}

/// BAR5 registers no wide read may touch: the NBIO RSMU alias, the AZ endpoint pair, the MM_INDEX window.
pub fn wide_readable(byte: u32) -> bool {
    byte.is_multiple_of(4)
        && !dcn314_regs::bar5::NBIO_RSMU_ALIAS.contains(&byte)
        && byte != dcn::AZ_ENDPOINT_INDEX
        && byte != dcn::AZ_ENDPOINT_DATA
        && !matches!(byte, dcn::MM_INDEX | dcn::MM_DATA | dcn::MM_INDEX_HI)
}

impl Hw for LiveHw {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        (wide_readable(byte) && self.allowed.contains(&byte)).then(|| self.regs.read(byte).ok()).flatten()
    }
    fn reg_write(&mut self, _byte: u32, _value: u32) -> bool {
        false
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
    fn delay_us(&mut self, _us: u64) {}
}

/// The owner's totals of this boot (kept across probes so a refused write would stay visible).
static TOTALS: Mutex<Totals> = Mutex::named("amdgpu.bringup", Totals { writes: 0, refused: 0, failed: 0, mismatched: 0, mem_refused: 0, registers: 0 });
const WAIT_NS: u64 = 100_000_000;

/// Classifies through a fresh owner with its gate shut; returns the state and the owner's totals.
pub fn classify_live(regs: &'static DcnRegs, d: &Discovery) -> (State, Totals) {
    let allowed = classify::addresses(d).into_iter().filter_map(|(_, b)| b).collect();
    let mut owner = Bringup::new(LiveHw { regs, allowed }, Gate::Refuse(READ_ONLY), LOG_KEEP);
    // GX-7f: with the stop note this boot or the kernel before left (a clean stop looks like a crash otherwise).
    let note = crate::psplive::current_note();
    // GX-7i2: and this boot's own traces (the doorbell aperture its SDMA stage turned on), before the Linux signs.
    let st = classify::classify_traced(d, &mut |b| owner.reg_read(b), note.as_ref(), crate::psplive::own_traces());
    let t = owner.log().totals();
    let all = match TOTALS.lock_timeout(WAIT_NS) {
        Some(mut g) => {
            g.writes += t.writes;
            g.refused += t.refused;
            g.failed += t.failed;
            g.mismatched += t.mismatched;
            g.mem_refused += t.mem_refused;
            g.registers = g.registers.max(t.registers);
            *g
        }
        None => t,
    };
    klog::event!(Info, "gx", "state.classified", start = st.start.name(), gc = format!("{:?}", st.gc).as_str(), writes = all.writes);
    (st, all)
}

/// The state section's lines.
pub fn lines(st: &State) -> Vec<String> {
    let mut v = alloc::vec![format!("state: {}", st.start.name())];
    for w in &st.why {
        v.push(format!("state why: {w}"));
    }
    v.push(format!("state gc: {:?}", st.gc));
    for f in &st.findings {
        let at = f.byte.map_or(String::from("-"), |b| format!("@{b:#x}"));
        let val = f.value.map_or(String::from("-"), |x| format!("{x:#010x}"));
        v.push(format!("state reg {} {} {at} = {val}: {}", f.group.name(), f.name, f.meaning));
    }
    v
}

/// IOMMU control register mappings (one page per IOMMU, once per boot).
static IOMMU: Mutex<Vec<(u64, hal::mmio::IoMem)>> = Mutex::named("amdgpu.iommu", Vec::new());

/// AMD-Vi: per IOMMU of the IVRS table, its control register (MMIO base + 0x18, bit 0 IommuEn) and the extended
/// feature register (+0x30). Read-only.
pub fn iommu_lines() -> Vec<String> {
    let Some(ivrs) = acpi::live::tables().and_then(|t| t.ivrs.as_ref()) else {
        return alloc::vec![String::from("iommu: no IVRS table (no AMD-Vi described by the firmware)")];
    };
    if ivrs.iommus.is_empty() {
        return alloc::vec![String::from("iommu: IVRS lists no IOMMU")];
    }
    let mut v = Vec::new();
    // The firmware describes one IOMMU once per IVHD type it supports (0x10/0x11/0x40): one line per IOMMU.
    let mut seen: Vec<(u16, u64)> = Vec::new();
    for u in &ivrs.iommus {
        if seen.contains(&(u.device_id, u.base)) {
            continue;
        }
        seen.push((u.device_id, u.base));
        let types: Vec<String> = ivrs.iommus.iter().filter(|x| (x.device_id, x.base) == (u.device_id, u.base)).map(|x| format!("{:#x}", x.ivhd_type)).collect();
        let id = format!("{:02x}:{:02x}.{}", u.device_id >> 8, u.device_id >> 3 & 0x1F, u.device_id & 7);
        v.push(match control(u.base) {
            Ok((ctl, efr)) => format!(
                "iommu {id} (IVHD {}) at {:#x}: control {ctl:#018x}, IommuEn {} (translation {}), EFR {efr:#018x}",
                types.join("/"),
                u.base,
                ctl & 1,
                if ctl & 1 != 0 { "ON: device DMA is translated" } else { "off" }
            ),
            Err(e) => format!("iommu {id} at {:#x}: {e}", u.base),
        });
    }
    v
}

fn control(base: u64) -> Result<(u64, u64), String> {
    if base == 0 || !base.is_multiple_of(4096) {
        return Err(format!("base {base:#x} not a page: not read"));
    }
    let mut maps = IOMMU.lock_timeout(WAIT_NS).ok_or_else(|| String::from("busy"))?;
    let i = match maps.iter().position(|(p, _)| *p == base) {
        Some(i) => i,
        None => {
            let io = hal::mmio::map_device(base, 4096, hal::paging::CacheType::Uncached).map_err(|e| format!("map: {e:?}"))?;
            maps.push((base, io));
            maps.len() - 1
        }
    };
    let io = &maps[i].1;
    let rd = |o: usize| -> Result<u64, String> {
        let lo = io.read32(o).map_err(|e| format!("{e:?}"))?;
        let hi = io.read32(o + 4).map_err(|e| format!("{e:?}"))?;
        Ok(u64::from(hi) << 32 | u64::from(lo))
    };
    Ok((rd(0x18)?, rd(0x30)?))
}
