//! GX-7b: the carve-out as the CPU reads it (live part; the plan and the conclusions are `gpucore::vram`'s, pure).
//! Read-only: each sample is mapped uncached once per boot (4 KiB or 8 KiB; mappings are not given back, the
//! samples are few and fixed per carve-out address) and read, but only where the firmware memory map calls the
//! range reserved: never RAM (`hal::mmio` refuses it), never a range the map calls MMIO (an unknown device) or a hole.
//!
//! Where the carve-out lies: MMHUB's MMMC_VM_FB_OFFSET (Linux's source, gmc_v11_0.c:720-723); DCN_VM_FB_OFFSET
//! is read beside it as a cross-check. The base is proven independently by comparing the GOP framebuffer
//! (through BAR0, the boot mapping) with the carve-out at the same graphics-memory offset.

use crate::gpucore::disc;
use crate::gpucore::vram::{self, CarvePlan, Look, Sample};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::Mutex;

/// The sample mappings of this boot: (phys, bytes, mapping). A sleeping lock: mapping allocates.
static MAPS: Mutex<Vec<(u64, usize, hal::mmio::IoMem)>> = Mutex::named("amdgpu.carve", Vec::new());
const MAP_WAIT_NS: u64 = 100_000_000;

/// UEFI memory types (UEFI 2.10 table 7.10): only "reserved" ranges are read.
const EFI_RESERVED: u32 = 0;
const EFI_MMIO: u32 = 11;

fn efi_type_name(t: u32) -> &'static str {
    match t {
        0 => "reserved",
        1 | 2 => "loader",
        3 | 4 => "boot services",
        5 | 6 => "runtime services",
        7 => "free RAM",
        8 => "unusable",
        9 => "ACPI reclaim",
        10 => "ACPI NVS",
        11 => "MMIO",
        12 => "MMIO port space",
        13 => "PAL code",
        14 => "persistent",
        _ => "other",
    }
}

/// The firmware map's word on `[phys, phys + len)`: `Ok` when all of it is reserved; `Err` names what it is.
pub(crate) fn map_check(phys: u64, len: u64) -> Result<String, String> {
    let Some(m) = acpi::live::memory_map() else {
        return Err(String::from("no firmware memory map: not read"));
    };
    let end = phys.saturating_add(len);
    if m.covers(phys, end, |t| t == EFI_RESERVED) {
        return Ok(String::from("reserved"));
    }
    match m.find(phys) {
        None => Err(String::from("a hole in the firmware memory map: not read")),
        Some(r) if r.ty == EFI_MMIO => Err(format!("the firmware map calls {:#x}-{:#x} MMIO: not read (an unknown device)", r.start, r.end)),
        Some(r) => Err(format!("the firmware map calls {:#x}-{:#x} {} (type {}): not read", r.start, r.end, efi_type_name(r.ty), r.ty)),
    }
}

/// The firmware map around the carve-out, for the answer: every region `[base, base + size)` touches.
fn map_lines(base: u64, size: u64) -> Vec<String> {
    let Some(m) = acpi::live::memory_map() else {
        return alloc::vec![String::from("carve-out map: no firmware memory map")];
    };
    let end = base.saturating_add(size);
    let mut v: Vec<String> = m
        .regions()
        .iter()
        .filter(|r| r.start < end && base < r.end)
        .map(|r| format!("carve-out map: {:#x}-{:#x} {} (type {})", r.start, r.end, efi_type_name(r.ty), r.ty))
        .collect();
    if v.is_empty() {
        v.push(format!("carve-out map: {base:#x}-{end:#x} lies in no region of the firmware map"));
    }
    v
}

/// Reads `len` bytes at `phys` through the boot's mapping of it (made on first use).
fn read_phys(phys: u64, len: usize) -> Result<Vec<u8>, String> {
    map_check(phys, len as u64)?;
    let mut maps = MAPS.lock_timeout(MAP_WAIT_NS).ok_or_else(|| String::from("carve-out mappings busy"))?;
    let i = match maps.iter().position(|(p, l, _)| *p == phys && *l == len) {
        Some(i) => i,
        None => {
            let io = hal::mmio::map_device(phys, len, hal::paging::CacheType::Uncached).map_err(|e| {
                klog::event!(Warn, "gx", "carve.map_refused", phys = phys, why = format!("{e:?}").as_str());
                format!("map {phys:#x}: {e:?}")
            })?;
            klog::event!(Info, "gx", "carve.mapped", phys = phys, bytes = len);
            maps.push((phys, len, io));
            maps.len() - 1
        }
    };
    let io = &maps[i].2;
    let mut out = Vec::with_capacity(len);
    for o in (0..len).step_by(4) {
        out.extend_from_slice(&io.read32(o).map_err(|e| format!("read {o:#x}: {e:?}"))?.to_le_bytes());
    }
    Ok(out)
}

fn look_text(l: Look) -> String {
    match l {
        Look::AllOnes => String::from("all 0xff"),
        Look::AllZero => String::from("all zero"),
        Look::Data { nonzero } => format!("data ({nonzero} non-zero bytes)"),
    }
}

/// What the carve-out section found.
pub struct Carve {
    pub lines: Vec<String>,
    /// The discovery area's bytes, when they were read (parsed or not).
    pub discovery: Option<(u64, Vec<u8>)>,
}

/// The carve-out section: base and size from the registers, the firmware map, the samples, the GOP comparison
/// and the conclusion. `fb_offset`: MMHUB's register when known, else the display's; `gop`: (phys, bytes, offset
/// in graphics memory, BAR0's size) when the GOP framebuffer lies in BAR0.
pub fn section(fb_offset: Option<(&str, u32)>, dcn_fb_offset: Option<u32>, memsize_mb: Option<u32>, gop: Option<(u64, u64, u64)>, bar0_size: Option<u64>) -> Carve {
    let mut lines = Vec::new();
    let (Some((src, fb)), Some(mb)) = (fb_offset, memsize_mb) else {
        lines.push(format!(
            "carve-out: not located (FB offset {}, MEMSIZE {})",
            fb_offset.map_or(String::from("unknown"), |(s, v)| format!("{v:#x} from {s}")),
            memsize_mb.map_or(String::from("unknown"), |m| format!("{m} MiB"))
        ));
        return Carve { lines, discovery: None };
    };
    if let Some(l) = offset_line(src, fb, dcn_fb_offset) {
        lines.push(l);
    }
    let plan = match CarvePlan::new(fb, mb) {
        Ok(p) => p,
        Err(e) => {
            lines.push(format!("carve-out: no address ({e:?}) from {src} {fb:#x}, MEMSIZE {mb} MiB"));
            return Carve { lines, discovery: None };
        }
    };
    lines.push(format!("carve-out: CPU {:#x}-{:#x} ({mb} MiB, MEMSIZE), {src} {fb:#x}", plan.base, plan.base + plan.size - 1));
    if let Some(b) = bar0_size {
        let reach = if b >= plan.size { "all of it" } else { "not the top: the discovery area lies past BAR0" };
        lines.push(format!("carve-out through BAR0: {} MiB window, graphics memory 0..{:#x}: {reach}", b >> 20, b.min(plan.size)));
    }
    lines.extend(map_lines(plan.base, plan.size));
    let mut looks: [Option<Look>; 3] = [None; 3];
    let mut discovery = None;
    for s in &plan.samples {
        let phys = plan.phys(s);
        match read_phys(phys, s.len) {
            Ok(bytes) => {
                let l = vram::look(&bytes);
                lines.push(format!("carve-out sample {} at {phys:#x} (offset {:#x}, {} bytes): {}", s.what, s.offset, s.len, look_text(l)));
                let slot = match s.what {
                    vram::START => 0,
                    vram::BELOW => 1,
                    _ => 2,
                };
                looks[slot] = Some(l);
                if s.what == vram::DISCOVERY {
                    discovery = Some((phys, bytes));
                }
            }
            Err(e) => lines.push(format!("carve-out sample {} at {phys:#x}: {e}", s.what)),
        }
    }
    if let Some(g) = gop {
        lines.push(gop_line(&plan, g));
    }
    let parsed = discovery.as_ref().is_some_and(|(_, b)| disc::parse(b).is_ok());
    lines.push(format!("carve-out verdict: {}", vram::conclude(looks[0], looks[1], looks[2], parsed)));
    klog::event!(Info, "gx", "carve.probed", base = plan.base, mb = mb, parsed = parsed);
    Carve { lines, discovery }
}

/// The cross-check of the carve-out's offset: MMHUB's register (`src`, Linux's source) against the display's
/// DCN_VM_FB_OFFSET; when the offset came from the display itself there is nothing to compare (pure, host-tested).
pub fn offset_line(src: &str, fb: u32, dcn_fb_offset: Option<u32>) -> Option<String> {
    if src == "DCN_VM_FB_OFFSET" {
        return Some(format!("carve-out offset: DCN_VM_FB_OFFSET {fb:#x} (MMHUB's register needs a discovery table)"));
    }
    let d = dcn_fb_offset?;
    let agree = d & 0xFF_FFFF == fb & 0xFF_FFFF;
    Some(format!("carve-out offset: {src} {fb:#x}, DCN_VM_FB_OFFSET {d:#x}: {}", if agree { "agree" } else { "DIFFER (MMHUB is Linux's source)" }))
}

/// The GOP framebuffer through BAR0 against the carve-out at the same offset: equal bytes prove the base.
fn gop_line(plan: &CarvePlan, (gop_phys, gop_bytes, offset): (u64, u64, u64)) -> String {
    let len = 4096usize.min(gop_bytes as usize) & !3;
    if len == 0 || offset.saturating_add(len as u64) > plan.size {
        return String::from("carve-out base check: the GOP framebuffer lies outside the carve-out");
    }
    let Some(bi) = kexec::boot::boot_info().map(|(b, _)| b) else {
        return String::from("carve-out base check: no boot info");
    };
    let Some(fb) = bi.framebuffer().and_then(|f| hal::mmio::framebuffer(&f)).filter(|f| f.phys() == gop_phys) else {
        return String::from("carve-out base check: the boot mapping of the GOP framebuffer is gone");
    };
    let s = Sample { what: "GOP", offset, len };
    let phys = plan.phys(&s);
    let via_bar0 = |fb: &hal::mmio::IoMem| -> Option<Vec<u8>> {
        let mut v = Vec::with_capacity(len);
        for o in (0..len).step_by(4) {
            v.extend_from_slice(&fb.read32(o).ok()?.to_le_bytes());
        }
        Some(v)
    };
    // The console may draw between the two reads: one retry.
    for _ in 0..2 {
        let (Some(a), b) = (via_bar0(&fb), read_phys(phys, len)) else {
            return String::from("carve-out base check: the GOP framebuffer did not read");
        };
        let b = match b {
            Ok(b) => b,
            Err(e) => return format!("carve-out base check at {phys:#x}: {e}"),
        };
        if a == b {
            return format!(
                "carve-out base check: GOP framebuffer {gop_phys:#x} (BAR0) and carve-out {phys:#x} hold the same {len} bytes ({}): the base is right",
                look_text(vram::look(&a))
            );
        }
    }
    format!("carve-out base check: GOP framebuffer {gop_phys:#x} (BAR0) and carve-out {phys:#x} DIFFER: the base is not where the register says, or the CPU does not reach it")
}
