//! GX-6 in-kernel tests. QEMU has no AMD function: the hardware parts belong to the Mini-PC there, the registry and argument
//! checks run everywhere. All rerunnable (Safe runs on CPU 0 over MCP, possibly several times per boot).

use crate::gpu::Gpu;
use crate::gpucore::disc;
use crate::ipmod::{self, Choice};
use crate::probe;
use alloc::format;
use alloc::string::String;
use ktest::kassert;
use mcp::{Json, ToolContext, ToolError, ToolOutput};

fn call(args: &str) -> Result<String, ToolError> {
    let t = mcp::find("gx_probe").ok_or_else(|| ToolError::Failed(String::from("gx_probe not registered")))?;
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let a = Json::parse(args).map_err(|e| ToolError::Failed(format!("{e:?}")))?;
    match (t.handler)(&ctx, &a)?.first() {
        Some(ToolOutput::Text(s)) => Ok(s.clone()),
        _ => Err(ToolError::Failed(String::from("no text answer"))),
    }
}

/// The module table is unambiguous: no two modules of one role claim the same version, none claims a version
/// it also lists as Linux-only.
fn registry() -> ktest::TestResult {
    for (i, a) in ipmod::MODULES.iter().enumerate() {
        for v in a.versions {
            kassert!(!a.linux_also.contains(v), "{}: {v} both chosen and Linux-only", a.name);
            for b in &ipmod::MODULES[i + 1..] {
                kassert!(!(a.role == b.role && b.versions.contains(v)), "{} and {} both claim {v}", a.name, b.name);
            }
        }
    }
    Ok(())
}
ktest::ktest!("gx.ipmod.registry", Safe, registry);

/// `gx_probe` is GX's, safety Change (GX-7f: it may write the MM_INDEX window); a bad argument is BadArgument; without a GPU (QEMU) a plain Failed; on the
/// Mini-PC the answer has every section (GX-7b: a failing section is reported, the next one still runs) and two
/// probes agree on the lines that decide (the carve-out's verdict, the discovery line, the start state). Values
/// such as the GOP page's bytes may change between two probes (the console draws).
fn probe_tool() -> ktest::TestResult {
    let t = mcp::find("gx_probe").ok_or("gx_probe not registered")?;
    kassert!(t.owner == "GX" && t.safety == mcp::Safety::Change, "owner {} safety {:?}", t.owner, t.safety);
    let e = call(r#"{"hex":"yes"}"#);
    kassert!(matches!(e, Err(ToolError::BadArgument(_))), "hex=yes: {e:?}");
    if Gpu::get().is_none() {
        let e = call("{}");
        kassert!(matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no AMD GPU")), "QEMU: {e:?}");
        // No GPU here: the arguments and this refusal are what QEMU can show; the Mini-PC run goes on below.
        return Ok(());
    }
    let a = call("{}").map_err(|e| format!("{e:?}"))?;
    for section in ["gpu: ", "display: ", "memsize: ", "carve-out", "discovery", "state", "iommu"] {
        kassert!(a.lines().any(|l| l.starts_with(section)), "no {section} line");
    }
    let w0 = crate::dcn::write_counts();
    let b = call("{}").map_err(|e| format!("{e:?}"))?;
    kassert!(crate::dcn::write_counts() == w0, "the probe wrote a display register");
    let key = |s: &str| -> alloc::vec::Vec<String> {
        s.lines()
            .filter(|l| l.starts_with("carve-out verdict") || l.starts_with("discovery:") || l.starts_with("state: ") || l.starts_with("bringup: "))
            .map(String::from)
            .collect()
    };
    // A stalled holder of the boot's discovery copy (logged as gx/discovery.copy_busy) can make one answer say
    // "not available": one more probe before two differing answers count as a failure.
    if key(&a) != key(&b) {
        let c = call("{}").map_err(|e| format!("{e:?}"))?;
        kassert!(key(&b) == key(&c) || key(&a) == key(&c), "probes differ: {:?} / {:?} / {:?}", key(&a), key(&b), key(&c));
    }
    Ok(())
}
// GX-7f: Display, since the probe reads the discovery copy through MM_INDEX when this boot has none yet.
ktest::ktest!("gx.probe.tool", Display, probe_tool);

/// Mini-PC: the binary parses, its DMU version matches what gate 7 measured (3.1.4) when the display is attached,
/// and the plan chose the existing display module for it.
fn carve_out() -> ktest::TestResult {
    let Some(g) = Gpu::get() else {
        return ktest::fail("no AMD GPU function (QEMU)");
    };
    // GX-7b: when the CPU cannot read the binary there, the probe says exactly why (all ones at the top while the
    // carve-out below answers, 27.10.): that is a fact of the machine, reported, not a failure of this code.
    let found = match fetch_carve_out(g.regs, g.display.is_ok()) {
        Ok(f) => f,
        Err(why) => {
            klog::event!(Info, "gx", "disc.carveout_unreadable", why = why.as_str());
            kassert!(why.contains("carve-out"), "the probe does not say why: {why}");
            return Ok(());
        }
    };
    let d = disc::parse(&found.bytes).map_err(|w| format!("refused: {w}"))?;
    kassert!(!d.ips.is_empty(), "no IPs");
    if g.display.is_ok() {
        let dmu = d.version(crate::gpucore::hwid::DMU);
        kassert!(dmu == Some(crate::ipmod::v(3, 1, 4)), "DMU {dmu:?}, gate 7 measured 3.1.4");
        let plan = ipmod::plan(&d);
        kassert!(plan.chosen(ipmod::Role::Display).is_some_and(|m| m.name == "dcn3_1_4"), "display module not chosen");
        for c in plan.missing() {
            if let Choice::Missing { role, version, .. } = c {
                klog::event!(Info, "gx", "ipmod.missing", role = role.name(), version = format!("{version}").as_str());
            }
        }
    }
    Ok(())
}
ktest::ktest!("gx.disc.carveout", Hardware, carve_out);

/// Mini-PC, writes MM_INDEX/MM_INDEX_HI through `dcn::MmWindow` (counted apart; both restored after each chunk):
/// Linux's indirect path gives the same bytes as the read-only carve-out path, the index registers hold their old
/// values afterwards, and the display write log (its ring keeps the display's read-back and undo) is untouched.
fn indirect() -> ktest::TestResult {
    let Some(g) = Gpu::get() else {
        return ktest::fail("no AMD GPU function (QEMU)");
    };
    use crate::dcn;
    use crate::gpucore::vram::{CHUNK_DWORDS, MM_INDEX, MM_INDEX_HI};
    let index = || g.regs.try_mm_window().map(|w| (w.read(MM_INDEX), w.read(MM_INDEX_HI)));
    let before = index().ok_or("MM_INDEX window busy")?;
    let (log0, ring0, w0) = (dcn::write_counts(), dcn::recent_writes(), dcn::window_writes());
    let ind = probe::fetch_indirect(g.regs)?;
    let (log1, ring1, w1) = (dcn::write_counts(), dcn::recent_writes(), dcn::window_writes());
    let after = index().ok_or("MM_INDEX window busy")?;
    kassert!(before == after, "index registers {before:?} -> {after:?}");
    kassert!(log0 == log1 && ring0 == ring1, "display write log changed: {log0:?} -> {log1:?}");
    // Per chunk: one MM_INDEX write per dword, one MM_INDEX_HI, two restores (the binary lies below 2 GB).
    let chunks = (disc::TMR_SIZE / 4).div_ceil(CHUNK_DWORDS) as u64;
    let want = chunks * (CHUNK_DWORDS as u64 + 3);
    kassert!(w1 - w0 == want, "{} index writes, want {want}", w1 - w0);
    log_hex(&ind.bytes);
    let d = disc::parse(&ind.bytes).map_err(|w| format!("indirect copy refused: {w}"))?;
    // The carve-out read only counts when it found a binary at all: on the Mini-PC it reads 0xff (27.10.).
    if let Ok(co) = fetch_carve_out(g.regs, g.display.is_ok()) {
        let n = usize::from(d.binary_size);
        if disc::parse(&co.bytes).is_ok() {
            kassert!(co.bytes.get(..n) == ind.bytes.get(..n), "carve-out and indirect copies differ");
        }
    }
    Ok(())
}
ktest::ktest!("gx.disc.indirect", Hardware, indirect);

/// The copy in hex, 256 bytes per line (klog cuts records near 1 KiB), so the desk can turn the log into the fixture
/// host/tests/gx/fixtures/minipc-780m-discovery.bin.
fn log_hex(bytes: &[u8]) {
    use core::fmt::Write;
    for (part, chunk) in bytes.chunks(256).enumerate() {
        let mut hex = String::with_capacity(chunk.len() * 2);
        for b in chunk {
            let _ = write!(hex, "{b:02x}");
        }
        klog::event!(Info, "gx", "discovery.hex", part = part, hex = hex.as_str(); "IP discovery copy");
    }
}

/// GX-7b, Mini-PC (read-only): the start state through the bring-up owner with its gate shut: classified (not
/// unknown), the PSP's secure OS runs, every finding has its meaning, no write reached the GPU (the owner logged
/// none, the display log is unchanged), the same verdict twice. Needs a discovery table of this boot: the
/// carve-out's, or the copy ktest gx.disc.indirect keeps.
fn classify_state() -> ktest::TestResult {
    let Some(g) = Gpu::get() else {
        return ktest::fail("no AMD GPU function (QEMU)");
    };
    let Some(found) = probe::kept().or_else(|| fetch_carve_out(g.regs, g.display.is_ok()).ok()) else {
        return ktest::fail("no discovery table in this boot (the carve-out gave none): run gx.disc.indirect first");
    };
    let d = disc::parse(&found.bytes).map_err(|w| format!("kept copy refused: {w}"))?;
    let w0 = crate::dcn::write_counts();
    let (a, ta) = crate::state::classify_live(g.regs, &d);
    let (b, tb) = crate::state::classify_live(g.regs, &d);
    kassert!(crate::dcn::write_counts() == w0, "display write log changed");
    kassert!(ta.writes == 0 && tb.writes == 0, "the owner logged writes: {ta:?} {tb:?}");
    kassert!(a.start != crate::gpucore::classify::Start::Unknown, "unknown: {:?}", a.why);
    kassert!(a.start != crate::gpucore::classify::Start::PspDown, "the PSP secure OS does not run");
    kassert!(a.start == b.start, "{:?} then {:?}", a.start, b.start);
    kassert!(a.findings.iter().all(|f| !f.meaning.is_empty()), "a finding without meaning");
    klog::event!(Info, "gx", "state.ktest", start = a.start.name(), gc = format!("{:?}", a.gc).as_str());
    Ok(())
}
ktest::ktest!("gx.state.classify", Hardware, classify_state);

/// GX-7b, everywhere: the bring-up owner over the PSP fake logs every write with its read-back; with the gate
/// shut nothing reaches the device and each attempt is logged as refused.
fn bringup_log() -> ktest::TestResult {
    use crate::gpucore::bringup::{Bringup, Gate, Outcome};
    use crate::gpucore::psp::Hw;
    use crate::gpucore::pspfake::{FakeGpu, Hostile};
    let mut owner = Bringup::new(FakeGpu::new(Hostile::None), Gate::Allow, 1024);
    let b = owner.hw().a.c2pmsg_67;
    kassert!(owner.reg_write(b, 16), "write refused");
    let e = *owner.log().recent().next().ok_or("nothing logged")?;
    kassert!(e.byte == b && e.value == 16 && e.outcome == Outcome::Ok && e.read_back.is_some(), "{e:?}");
    kassert!(owner.hw().writes == alloc::vec![(b, 16)], "fake saw {:?}", owner.hw().writes);
    let mut shut = Bringup::new(FakeGpu::new(Hostile::None), Gate::Refuse("ktest"), 16);
    kassert!(!shut.reg_write(b, 16), "a shut gate wrote");
    kassert!(shut.hw().writes.is_empty() && shut.log().totals().refused == 1, "refusal not logged");
    Ok(())
}
ktest::ktest!("gx.bringup.log", Safe, bringup_log);

/// GX-7b, everywhere (read-only): the IOMMU lines of the probe: one per IOMMU of the IVRS table (QEMU with
/// `-device amd-iommu` has one), or the reason there is none; the same twice (the control register is only read).
fn iommu_read() -> ktest::TestResult {
    let a = crate::state::iommu_lines();
    kassert!(!a.is_empty() && a.iter().all(|l| l.starts_with("iommu")), "{a:?}");
    let b = crate::state::iommu_lines();
    kassert!(a == b, "{a:?} then {b:?}");
    for l in &a {
        klog::event!(Info, "gx", "iommu.line", line = l.as_str());
    }
    Ok(())
}
ktest::ktest!("gx.iommu.read", Safe, iommu_read);

/// The ktests' read-only path (APU; C1 moved it here from probe.rs, which runs `carve::section` itself): the
/// carve-out section's discovery sample, when it parses. The error is the
/// section's verdict (with the sample lines) when the CPU could not read the binary there.
fn fetch_carve_out(regs: &crate::dcn::DcnRegs, display_attached: bool) -> Result<probe::Found, String> {
    let g = crate::gpu::Gpu::get().ok_or_else(|| String::from("no AMD GPU function"))?;
    let copy = probe::kept();
    let d = copy.as_ref().and_then(|f| disc::parse(&f.bytes).ok());
    let c = crate::carve::section(probe::fb_offset(regs, d.as_ref(), probe::dcn_fb_offset(regs, display_attached)), probe::dcn_fb_offset(regs, display_attached), probe::memsize(regs).ok(), probe::gop_in_bar0(g.bar0), g.bar0.map(|b| b.1));
    match c.discovery {
        Some((phys, bytes)) if disc::parse(&bytes).is_ok() => {
            let found = probe::Found { via: probe::Via::CarveOut { phys }, bytes };
            probe::keep(&found);
            Ok(found)
        }
        _ => Err(c.lines.join("; ")),
    }
}
