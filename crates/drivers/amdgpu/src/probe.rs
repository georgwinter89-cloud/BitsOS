//! GX-6 probes for the desk on the Mini-PC: the discovery binary, its IP list (versions, instances, bases,
//! harvest), the modules `ipmod::plan` would choose, the firmware files they need. `report` is pure (host-tested);
//! the fetches are the live part. GX-7b made `run` (the `gx_probe` tool) a wide read-only probe that reports
//! every section even when one fails: the GPU's BARs, the carve-out (`carve`: where it is, what the CPU reads
//! there and why not), the discovery binary, the start state (`state`, `gpucore::classify`: PSP, SMU, hubs, IH,
//! doorbells, GC only when powered) and the IOMMU. It writes no register but the MM_INDEX window (GX-7f):
//!
//! - the carve-out path maps a few pages of the APU's carve-out (reserved memory, not RAM) uncached and reads;
//! - `fetch_indirect` is Linux's path: MM_INDEX writes through `dcn::MmWindow` (counted in `dcn::window_writes`,
//!   not in the display write log, whose ring the display's read-back and undo keep; both index registers put back
//!   after each 16-dword chunk). gx_bringup, the ktest `gx.disc.indirect` (class Display) and, since GX-7f,
//!   `gx_probe` itself (when this boot has no copy and the carve-out's top is closed, under the PSP run's lock) use
//!   it; the copy is kept for this boot.

use crate::dcn::{DcnRegs, MmWindow};
use crate::gpucore::bringup::Totals;
use crate::gpucore::disc::{self, Discovery, Note, Why};
use crate::gpucore::hwid;
use crate::gpucore::vram::{self, IndexRegs};
use crate::ip::{self, IpBlock, Source};
use crate::ipmod::{self, Choice, Firmware, State};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt::Write;
use ksync::Mutex;

/// How the binary was fetched and from where (CPU physical address, or position in graphics memory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    CarveOut { phys: u64 },
    Indirect { pos: u64 },
}

#[derive(Clone)]
pub struct Found {
    pub via: Via,
    pub bytes: Vec<u8>,
}

/// A status register read at a discovery address (name, BAR5 byte, value or why not).
pub struct RegRead {
    pub name: &'static str,
    pub byte: Option<u32>,
    pub value: Option<u32>,
}

fn hex_line(s: &mut String, bytes: &[u8]) {
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s.push('\n');
}

/// The `ip` line: `ip <NAME> <hw_id> <inst> <version> var <v> sub <s> harvested <yes|no> bases 0x...` (the trace
/// tool reads these lines as its bases).
pub fn ip_line(d: &Discovery, e: &disc::IpEntry) -> String {
    let mut s = format!(
        "ip {} {} {} {} var {} sub {} harvested {}",
        e.name().unwrap_or("?"),
        e.hw_id,
        e.instance,
        e.version,
        e.variant(),
        e.byte7 & 0xF,
        if d.harvested(e) { "yes" } else { "no" }
    );
    if e.hw_id == hwid::VCN && e.vcn_config != 0 {
        let _ = write!(s, " vcn_config {:#04x}", e.vcn_config);
    }
    s += " bases";
    for b in &e.bases {
        let _ = write!(s, " {b:#010x}");
    }
    s
}

fn plan_lines(s: &mut String, d: &Discovery) {
    let plan = ipmod::plan(d);
    for c in &plan.choices {
        let _ = match *c {
            Choice::Chosen { role, version, module } => writeln!(
                s,
                "module {} {version}: {} ({}; Linux {})",
                role.name(),
                module.name,
                match module.state {
                    State::Present(p) => format!("present: {p}"),
                    State::Planned(i) => format!("planned {i}"),
                },
                module.linux
            ),
            Choice::Missing { role, version, candidate } => writeln!(
                s,
                "module {} {version}: MISSING{}",
                role.name(),
                candidate.map_or(String::new(), |m| format!(" (Linux's {} serves it; nearest BitsOS module {})", m.linux, m.name))
            ),
            Choice::Harvested { role, version } => writeln!(s, "module {} {version}: harvested", role.name()),
            Choice::Absent { role } => writeln!(s, "module {}: no such IP", role.name()),
        };
    }
    for c in &plan.choices {
        if let Choice::Chosen { module, .. } = c
            && let Firmware::None(why) = module.firmware
        {
            let _ = writeln!(s, "firmware {}: none ({why})", module.name);
        }
    }
    for f in plan.firmware() {
        let _ = writeln!(s, "firmware {f}");
    }
}

/// The device-ID table (`ip::known_blocks`) against the discovery table: each documented version agrees or not.
fn table_lines(s: &mut String, d: &Discovery, table: &[IpBlock]) {
    let found = ip::from_discovery(d);
    for t in table {
        let Source::Table(_) = t.source else { continue };
        let got = found.iter().find(|b| b.kind == t.kind && b.instance == t.instance).map(|b| b.version);
        let _ = match got {
            Some(v) if v == t.version => writeln!(s, "table {:?} {}: agrees", t.kind, t.version),
            Some(v) => writeln!(s, "table {:?} {}: DIFFERS, discovery says {v}", t.kind, t.version),
            None => writeln!(s, "table {:?} {}: not in discovery", t.kind, t.version),
        };
    }
}

/// The probe's answer (pure). `hex`: the whole binary (up to `binary_size`, or all bytes when refused) as one
/// hex line, to store as the fixture host/tests/gx/fixtures/ (xxd -r -p).
pub fn report(found: &Found, parsed: &Result<Discovery, Why>, regs: &[RegRead], table: &[IpBlock], hex: bool) -> String {
    let mut s = String::new();
    let place = match found.via {
        Via::CarveOut { phys } => format!("carve-out at phys {phys:#x}"),
        Via::Indirect { pos } => format!("MM_INDEX window at VRAM {pos:#x}"),
    };
    let d = match parsed {
        Err(w) => {
            let _ = writeln!(s, "discovery: REFUSED ({w}), read via {place}, {} bytes", found.bytes.len());
            s += "head ";
            hex_line(&mut s, &found.bytes[..found.bytes.len().min(256)]);
            if hex {
                s += "hex ";
                hex_line(&mut s, &found.bytes);
            }
            return s;
        }
        Ok(d) => d,
    };
    let _ = writeln!(
        s,
        "discovery: ok via {place}, binary v{}.{} {} bytes, table v{}, {} die(s), {} IPs, 64-bit bases {}",
        d.binary_major,
        d.binary_minor,
        d.binary_size,
        d.table_version,
        d.dies,
        d.ips.len(),
        if d.bases_64 { "yes" } else { "no" }
    );
    for e in &d.ips {
        s += &ip_line(d, e);
        s.push('\n');
    }
    match &d.harvest {
        None => s += "harvest: no table\n",
        Some(h) if h.is_empty() => s += "harvest: none\n",
        Some(h) => {
            s += "harvest:";
            for x in h {
                let _ = write!(s, " {}.{}", hwid::name(x.hw_id).unwrap_or("?"), x.instance);
            }
            s.push('\n');
        }
    }
    if let Some(gc) = &d.gc {
        let _ = write!(s, "gc: v{}.{}", gc.major, gc.minor);
        for (n, v) in &gc.fields {
            let _ = write!(s, " {n}={v}");
        }
        s.push('\n');
    }
    if let Some(v) = &d.vcn {
        let _ = write!(s, "vcn: v{}.{}", v.major, v.minor);
        for i in &v.instances {
            let _ = write!(s, " inst {} fuses {:#x}", i.instance, i.fuses);
        }
        s.push('\n');
    }
    if let Some(m) = &d.mall {
        let _ = write!(s, "mall: v{}.{}", m.major, m.minor);
        for (n, v) in &m.fields {
            let _ = write!(s, " {n}={v}");
        }
        s.push('\n');
    }
    for n in &d.notes {
        let _ = match n {
            Note::MallUnchecked(w) => writeln!(s, "note: MALL table not used ({w}); Linux does not check it either"),
            Note::DieBeyondIpTable { die } => writeln!(s, "note: die {die}'s IP list lies outside the IP table's size"),
            Note::LinuxSkips { die, index, hw_id } => writeln!(s, "note: die {die} IP {index} (hw_id {hw_id}) is one Linux skips"),
            Note::Bases64BeforeV4 => writeln!(s, "note: 64-bit base flag in a table before v4"),
            Note::InfoVersion { table, major, minor } => writeln!(s, "note: {} table v{major}.{minor} not decoded", table.name()),
        };
    }
    plan_lines(&mut s, d);
    table_lines(&mut s, d, table);
    for r in regs {
        let _ = match (r.byte, r.value) {
            (Some(b), Some(v)) => writeln!(s, "reg {} @{b:#x} = {v:#010x}", r.name),
            (Some(b), None) => writeln!(s, "reg {} @{b:#x}: not readable", r.name),
            (None, _) => writeln!(s, "reg {}: no address (IP not in the table)", r.name),
        };
    }
    if hex {
        s += "hex ";
        hex_line(&mut s, &found.bytes[..usize::from(d.binary_size).min(found.bytes.len())]);
    }
    s
}

/// The status registers the probe reads at discovery addresses (all reads; Linux reads each of them).
pub fn reg_reads(d: &Discovery, read: impl Fn(u32) -> Option<u32>) -> Vec<RegRead> {
    use ipmod::{gmc11, psp13};
    let at = |hw: u16, (idx, dw): (usize, u32)| d.reg(hw, 0, idx, dw).and_then(|dword| dword.checked_mul(4));
    let list: [(&'static str, Option<u32>); 5] = [
        ("MP0_C2PMSG_81 (PSP sOS alive)", at(hwid::MP0, psp13::C2PMSG_81)),
        ("MP0_C2PMSG_64 (PSP ring ready: bit 31)", at(hwid::MP0, psp13::C2PMSG_64)),
        ("MMMC_VM_FB_OFFSET (MMHUB carve-out)", at(hwid::MMHUB, gmc11::MMHUB_FB_OFFSET)),
        // GX-7a: the bootloader's ready bit and the KM ring's write pointer (both read by psp_v13_0_4.c).
        ("MP0_C2PMSG_35 (PSP bootloader ready: bit 31)", at(hwid::MP0, psp13::C2PMSG_35)),
        ("MP0_C2PMSG_67 (PSP KM ring write pointer)", at(hwid::MP0, psp13::C2PMSG_67)),
    ];
    list.into_iter().map(|(name, byte)| RegRead { name, byte, value: byte.and_then(&read) }).collect()
}

// ---- live part (Mini-PC; QEMU has no AMD function) ----

/// The BAR5 owner as the indirect reader. Outside `locked` it refuses everything; inside, it reads and writes
/// only through the held `dcn::MmWindow` (the three window offsets; index writes counted, not read back:
/// MM_INDEX reads back the aperture bit its own way).
struct Window<'a> {
    regs: &'a DcnRegs,
    held: Option<MmWindow<'a>>,
    /// This fetch's own index writes (the global `dcn::window_writes` also counts other callers').
    writes: &'a Cell<u64>,
}

impl IndexRegs for Window<'_> {
    fn read(&self, byte: u32) -> Option<u32> {
        self.held.as_ref()?.read(byte).ok()
    }
    fn write(&self, byte: u32, value: u32) -> bool {
        let ok = self.held.as_ref().is_some_and(|w| w.write_index(byte, value).is_ok());
        if ok {
            self.writes.set(self.writes.get() + 1);
        }
        ok
    }
    fn locked<T>(&self, f: impl FnOnce(&Self) -> T) -> Option<T> {
        let held = Window { regs: self.regs, held: Some(self.regs.try_mm_window()?), writes: self.writes };
        Some(f(&held))
    }
}

pub(crate) fn memsize(regs: &DcnRegs) -> Result<u32, String> {
    regs.read(crate::layout::MEMSIZE).map_err(|e| format!("MEMSIZE: {e:?}"))
}

/// The discovery copy of this boot that parsed (from the carve-out or the indirect path), for the probe.
static COPY: Mutex<Option<Found>> = Mutex::named("amdgpu.disc_copy", None);
const COPY_WAIT_NS: u64 = 100_000_000;

pub(crate) fn keep(found: &Found) {
    if disc::parse(&found.bytes).is_err() {
        return;
    }
    match COPY.lock_timeout(COPY_WAIT_NS) {
        Some(mut c) => *c = Some(found.clone()),
        None => klog::event!(Warn, "gx", "discovery.copy_busy", what = "keep", wait_ns = COPY_WAIT_NS),
    }
}

/// The kept copy, if any (ktests and the probe). A lock timeout counts as no copy and is logged (the holder only
/// clones or stores, so it means a stalled holder, not a race this answer could win).
pub(crate) fn kept() -> Option<Found> {
    match COPY.lock_timeout(COPY_WAIT_NS) {
        Some(c) => c.clone(),
        None => {
            klog::event!(Warn, "gx", "discovery.copy_busy", what = "kept", wait_ns = COPY_WAIT_NS);
            None
        }
    }
}

/// Linux's path: MM_INDEX/MM_DATA (index writes counted in `dcn::window_writes`; index registers restored).
pub fn fetch_indirect(regs: &DcnRegs) -> Result<Found, String> {
    fetch_indirect_counted(regs).0
}

/// `fetch_indirect` with this call's own index-write count.
pub fn fetch_indirect_counted(regs: &DcnRegs) -> (Result<Found, String>, u64) {
    let n = Cell::new(0u64);
    let r = fetch_with(regs, &n);
    (r, n.get())
}

fn fetch_with(regs: &DcnRegs, n: &Cell<u64>) -> Result<Found, String> {
    let mb = memsize(regs)?;
    let pos = vram::discovery_pos(mb).ok_or_else(|| format!("MEMSIZE {mb} MB: no VRAM position (Linux asks ACPI)"))?;
    let read = vram::read_indirect(&Window { regs, held: None, writes: n }, pos, disc::TMR_SIZE);
    let writes = n.get();
    let bytes = read.map_err(|e| {
        klog::event!(Warn, "gx", "discovery.indirect_failed", pos = pos, index_writes = writes, why = format!("{e:?}").as_str());
        format!("indirect read: {e:?}")
    })?;
    klog::event!(Info, "gx", "discovery.indirect", pos = pos, bytes = bytes.len(), index_writes = writes);
    let found = Found { via: Via::Indirect { pos }, bytes };
    keep(&found);
    Ok(found)
}

/// The display's DCN_VM_FB_OFFSET (gate 7 proved DCN 3.1.4's layout, so only with the display attached).
pub(crate) fn dcn_fb_offset(regs: &DcnRegs, display_attached: bool) -> Option<u32> {
    display_attached.then(|| regs.read(dcn314_regs::dcn_vm_fb_offset::REG.byte()).ok()).flatten()
}

/// The carve-out's FB offset: MMHUB's (Linux's source) when a discovery table gives its address, else the display's.
pub(crate) fn fb_offset(regs: &DcnRegs, d: Option<&Discovery>, dcn: Option<u32>) -> Option<(&'static str, u32)> {
    let mmhub = d.and_then(|d| d.reg(hwid::MMHUB, 0, ipmod::gmc11::MMHUB_FB_OFFSET.0, ipmod::gmc11::MMHUB_FB_OFFSET.1))
        .and_then(|dw| dw.checked_mul(4))
        .filter(|&b| crate::state::wide_readable(b))
        .and_then(|b| regs.read(b).ok());
    mmhub.map(|v| ("MMMC_VM_FB_OFFSET", v)).or(dcn.map(|v| ("DCN_VM_FB_OFFSET", v)))
}

/// The GOP framebuffer when it lies in BAR0: (phys, bytes, offset in graphics memory).
pub(crate) fn gop_in_bar0(bar0: Option<(u64, u64)>) -> Option<(u64, u64, u64)> {
    let g = crate::gpu::gop()?;
    let (p, s) = bar0?;
    let off = g.phys.checked_sub(p).filter(|&o| o < s)?;
    Some((g.phys, g.bytes, off))
}

/// The whole probe on the live GPU, section by section: a failing section is reported and the next one runs.
/// `None`: no AMD function (QEMU).
pub fn run(hex: bool) -> Option<Result<String, String>> {
    let g = crate::gpu::Gpu::get()?;
    let mut s = String::new();
    let line = |s: &mut String, l: &str| {
        s.push_str(l);
        s.push('\n');
    };
    // Section 1: the device.
    let bars = (0..6u8)
        .filter_map(|i| match g.regs.pci().bar(i) {
            Some(b @ pci::Bar::Mem { .. }) => Some(format!(" BAR{i} {:#x} ({} KiB)", b.start(), b.size() >> 10)),
            _ => None,
        })
        .collect::<String>();
    line(&mut s, &format!("gpu: {} 1002:{:04X} rev {:#04x},{bars}", g.function(), g.device, g.revision));
    let display_ok = g.display.is_ok();
    line(&mut s, &match &g.display {
        Ok(a) => format!("display: attached (A0 offset {:#x}, MC {:#x}-{:#x})", a.a0_offset, a.fb_first, a.fb_last),
        Err(e) => format!("display: refused ({e}): DCN_VM_FB_OFFSET not used"),
    });
    let mb = memsize(g.regs);
    line(&mut s, &match &mb {
        Ok(m) => format!("memsize: {m} MiB (RCC_CONFIG_MEMSIZE)"),
        Err(e) => format!("memsize: {e}"),
    });
    // Section 2: the carve-out (MMHUB's offset needs a discovery table: a copy kept from earlier in this boot).
    let mut copy = kept();
    let d0 = copy.as_ref().and_then(|f| disc::parse(&f.bytes).ok());
    let dcn = dcn_fb_offset(g.regs, display_ok);
    let c = crate::carve::section(fb_offset(g.regs, d0.as_ref(), dcn), dcn, mb.as_ref().ok().copied(), gop_in_bar0(g.bar0), g.bar0.map(|b| b.1));
    // GX-7f: the 780M closes the carve-out's top to the CPU, so after a Reload no copy exists yet: fetch it the
    // way gx_bringup does (MM_INDEX/MM_DATA, the index registers restored), unless the carve-out gave one.
    let carve_ok = c.discovery.as_ref().is_some_and(|(_, b)| disc::parse(b).is_ok());
    // Under the PSP run's lock: a gx_bringup in progress compares the display's write counts around itself.
    let (c2, fetched) = probe_copy(copy, carve_ok, || crate::psplive::while_idle(|| fetch_indirect_counted(g.regs)));
    copy = c2;
    if let Some(l) = fetched {
        line(&mut s, &l);
    }
    let iommu = crate::state::iommu_lines();
    let mut state = |d: &Discovery| {
        let (st, totals) = crate::state::classify_live(g.regs, d);
        let mut v = crate::state::lines(&st);
        // GX-7i2: how the stop note arrived in this boot, visible at any loglevel.
        v.extend(crate::psplive::note_lines());
        v.push(bringup_line(&totals));
        v
    };
    let (text, to_keep) = assemble(s, c, copy, &ip::known_blocks(g.device), hex, &mut state, &iommu);
    if let Some(f) = &to_keep {
        keep(f);
    }
    Some(Ok(text))
}

/// The bring-up owner's totals line of the answer.
pub fn bringup_line(t: &Totals) -> String {
    format!(
        "bringup: {} writes ({} refused, {} failed, {} read back wrong, {} memory writes refused) over {} registers; gate shut: {}",
        t.writes, t.refused, t.failed, t.mismatched, t.mem_refused, t.registers, crate::state::READ_ONLY
    )
}

/// Whether the probe fetches the discovery copy itself (pure): only when this boot has none and the carve-out gave
/// none. `fetch` runs under the PSP run's lock (`None`: a run holds it). Returns the copy to use and the
/// 'discovery fetch:' line, if a fetch was tried.
pub(crate) fn probe_copy(copy: Option<Found>, carve_ok: bool, fetch: impl FnOnce() -> Option<(Result<Found, String>, u64)>) -> (Option<Found>, Option<String>) {
    if copy.is_some() || carve_ok {
        return (copy, None);
    }
    match fetch() {
        None => (None, Some(String::from("discovery fetch: skipped (a gx_bringup run holds the PSP and the MM_INDEX window: ask again after it)"))),
        Some((Ok(f), n)) => (Some(f), Some(format!("discovery fetch: through MM_INDEX/MM_DATA as gx_bringup does ({n} index writes, index restored; no other register written)"))),
        Some((Err(e), _)) => (None, Some(format!("discovery fetch: indirect read failed: {e}"))),
    }
}

/// Sections 2 to 5 after `head` (pure; the live reads come in as values and the `state` closure): the carve-out
/// lines, the discovery binary (the carve-out's when it parses, else the copy kept in this boot, else the
/// carve-out's bytes as REFUSED with why), the start state and the IOMMU. Every section is reported whatever the
/// others gave. Returns the answer and the carve-out copy to keep for this boot (it parsed).
pub(crate) fn assemble(
    head: String,
    c: crate::carve::Carve,
    copy: Option<Found>,
    table: &[IpBlock],
    hex: bool,
    state: &mut dyn FnMut(&Discovery) -> Vec<String>,
    iommu: &[String],
) -> (String, Option<Found>) {
    let mut s = head;
    let line = |s: &mut String, l: &str| {
        s.push_str(l);
        s.push('\n');
    };
    for l in &c.lines {
        line(&mut s, l);
    }
    // Section 3: the discovery binary, from the carve-out or the kept copy.
    let raw = c.discovery.map(|(phys, bytes)| Found { via: Via::CarveOut { phys }, bytes });
    let carve_ok = raw.as_ref().is_some_and(|f| disc::parse(&f.bytes).is_ok());
    let to_keep = if carve_ok { raw.clone() } else { None };
    let (found, kept_copy) = match (carve_ok, copy) {
        (true, _) => (raw, false),
        (false, Some(k)) => (Some(k), true),
        (false, None) => (raw, false),
    };
    let parsed = found.as_ref().map(|f| disc::parse(&f.bytes));
    match (&found, &parsed) {
        (Some(f), Some(p)) => {
            if kept_copy {
                line(&mut s, "discovery source: the indirect copy of this boot (gx_bringup's, ktest gx.disc.indirect's or this probe's fetch; the carve-out did not give one)");
            }
            match p {
                Ok(d) => klog::event!(Info, "gx", "discovery.parsed", ips = d.ips.len(), dies = d.dies, notes = d.notes.len()),
                Err(w) => klog::event!(Warn, "gx", "discovery.refused", why = format!("{w}").as_str()),
            }
            s += &report(f, p, &[], table, hex);
        }
        _ => line(&mut s, "discovery: not available (the carve-out gave no binary and the indirect fetch failed: see 'discovery fetch')"),
    }
    // Section 4: the start state (needs the table's addresses).
    match parsed {
        Some(Ok(d)) => {
            for l in state(&d) {
                line(&mut s, &l);
            }
        }
        _ => line(&mut s, "state: not classified (no discovery table: register addresses unknown)"),
    }
    // Section 5: the IOMMU.
    for l in iommu {
        line(&mut s, l);
    }
    (s, to_keep)
}
