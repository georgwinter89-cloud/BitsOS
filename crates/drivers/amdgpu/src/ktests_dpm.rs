//! GX-9b on the fakes: messages and their order against Linux, the refusal paths, GC asleep under the job half, the stop
//! and the tool's text. The fake's clock is virtual and the thread is off: every delay here is the fake's, none is judged.

use crate::dpm::{self, Dpm, Level, Next, Wire};
use crate::dpm_table::{self, GfxRange};
use crate::gpucore::gcfake::Power;
use crate::gpucore::psp::Hw;
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::smu::{self, SmuError, SmuTimeouts};
use crate::gpucore::smufake::{self, Client, SmuHostile, msg, resp};
use crate::hw::frame::RingType;
use crate::hw::jobs::JobCfg;
use crate::ipmod::smu13;
use crate::ktests_hw::Rig;
use crate::ktests_hw_jobs::{Cx, DATA, Mem, rig, run_one, write_ib};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use drm::gpu::{GpuDevice, Sensor};
use ktest::kassert;

/// The messages the driver's mailbox took since `from`: (id, argument).
pub(crate) fn taken_since(r: &Rig, from: usize) -> Vec<(u32, u32)> {
    r.fake(|f| f.smu.taken.iter().skip(from).filter(|t| t.0 == Client::Driver).map(|t| (t.1, t.2)).collect())
}

pub(crate) fn mark(r: &Rig) -> usize {
    r.fake(|f| f.smu.taken.len())
}

/// The fake's clock forward; the fake's GC follows it.
pub(crate) fn pass(r: &Rig, us: u64) {
    r.fake(|f| f.delay_us(us));
}

/// A running backend, DPM up and no message of GFXOFF yet.
pub(crate) fn up() -> Result<(Rig, Arc<Dpm>, Wire), String> {
    let r = rig(JobCfg::default())?;
    let (d, w) = r.start_dpm()?;
    Ok((r, d, w))
}

/// The delay passed and the Allow out.
pub(crate) fn allow(r: &Rig, d: &Dpm) -> ktest::TestResult {
    pass(r, dpm::GFX_OFF_DELAY_US);
    kassert!(d.tick() == Next::Idle, "the Allow was due");
    kassert!(d.flags().asleep.load(Ordering::Acquire), "not asleep");
    pass(r, 1_000_000);
    kassert!(r.fake(|f| f.gc.power) == Power::Off, "GC {:?}", r.fake(|f| f.gc.power));
    Ok(())
}

// ---- pure

pub(crate) fn tables() -> ktest::TestResult {
    let mut c = alloc::vec![0u8; dpm_table::DPMCLOCKS_BYTES];
    kassert!(dpm_table::gfx_range(&c).is_none(), "an empty table names no range");
    c[dpm_table::MIN_GFX_AT..dpm_table::MIN_GFX_AT + 4].copy_from_slice(&800u32.to_le_bytes());
    c[dpm_table::MAX_GFX_AT..dpm_table::MAX_GFX_AT + 4].copy_from_slice(&2700u32.to_le_bytes());
    kassert!(dpm_table::gfx_range(&c) == Some(GfxRange { min: 800, max: 2700 }));
    kassert!(dpm_table::gfx_range(&c[..300]).is_none(), "short");
    c[dpm_table::MAX_GFX_AT..dpm_table::MAX_GFX_AT + 4].copy_from_slice(&700u32.to_le_bytes());
    kassert!(dpm_table::gfx_range(&c).is_none(), "min above max");

    let mut m = alloc::vec![0u8; dpm_table::METRICS_BYTES];
    kassert!(dpm_table::metrics(&m).is_none(), "the wiped page came back");
    kassert!(dpm_table::metrics(&m[..20]).is_none(), "short");
    let mut put = |at: usize, v: u16| m[at..at + 2].copy_from_slice(&v.to_le_bytes());
    // The offsets offsetof gives for SmuMetrics_t (smu13_driver_if_v13_0_4.h).
    put(0, 2700);
    put(2, 1200);
    put(8, 2800);
    put(12, 5150);
    put(16, 1165);
    put(18, 762);
    put(80, 3925);
    put(82, 4150);
    put(84, 1 << 5);
    put(86, 9000);
    put(136, 8500);
    let v = dpm_table::metrics(&m).ok_or("no metrics")?;
    kassert!(v.gfx_mhz == 2700 && v.soc_mhz == 1200 && v.mem_mhz == 2800 && v.socket_mw == 9000, "{v:?}");
    kassert!(v.sensor(Sensor::GfxSclk) == Some(2700) && v.sensor(Sensor::GfxMclk) == Some(2800));
    kassert!(v.sensor(Sensor::GpuTemp) == Some(39_000) && v.sensor(Sensor::GpuLoad) == Some(51) && v.sensor(Sensor::GpuAvgPower) == Some(8));
    kassert!(v.sensor(Sensor::Vddgfx) == Some(1165) && v.sensor(Sensor::Vddnb) == Some(762));
    kassert!(v.sensor(Sensor::StablePstateSclk) == Some(0) && v.sensor(Sensor::PeakPstateSclk).is_none());
    kassert!(dpm_table::throttler_names(0) == "none" && dpm_table::throttler_names(v.throttler) == "THM_GFX");
    kassert!(dpm_table::throttler_names(1 | 1 << 14) == "SPL other 0x4000");
    kassert!(dpm_table::centi(3925) == "39.25" && dpm_table::milli(9005) == "9.005");
    Ok(())
}
ktest::ktest!("gx.fake.dpm_tables", Safe, tables);

/// The guard: bring-up's list unchanged; the running list holds GFX's limits and read-only tables, nothing else.
pub(crate) fn guard() -> ktest::TestResult {
    for id in [msg::ALLOW_GFXOFF, msg::SET_HARD_MIN_GFXCLK, msg::SET_SOFT_MAX_GFXCLK, msg::TRANSFER_TABLE_SMU2DRAM, msg::SET_DRIVER_DRAM_ADDR_LOW] {
        kassert!(!smu::allowed(id, 0) && !smu::allowed(id, 1000), "{id:#x} on bring-up's list");
    }
    for (id, arg) in [
        (msg::ALLOW_GFXOFF, 0),
        (msg::DISALLOW_GFXOFF, 0),
        (msg::SET_HARD_MIN_GFXCLK, 800),
        (msg::SET_SOFT_MAX_GFXCLK, 2700),
        (msg::GET_ENABLED_SMU_FEATURES, 1),
        (msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_SMU_METRICS),
        (msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_DPMCLOCKS),
        (msg::GFX_DEVICE_DRIVER_RESET, smu13::RESET_MODE_2),
    ] {
        kassert!(smu::allowed_running(id, arg), "{id:#x} {arg}");
    }
    // Mode 1, other clocks, Z-states, power limits, watermarks and every table towards PMFW, a clock id in the argument.
    for (id, arg) in [
        (msg::GFX_DEVICE_DRIVER_RESET, 1),
        (msg::TRANSFER_TABLE_DRAM2SMU, 1),
        (msg::TRANSFER_TABLE_SMU2DRAM, 1),
        (msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_SMU_METRICS | 1 << 16),
        (msg::ALLOW_ZSTATES, 0),
        (msg::SET_HARD_MIN_GFXCLK, 0),
        (msg::SET_SOFT_MAX_GFXCLK, 0),
        (msg::SET_SOFT_MAX_GFXCLK, 0x1_0000),
        (msg::SET_HARD_MIN_GFXCLK, 3 << 16 | 2700),
        (msg::GET_ENABLED_SMU_FEATURES, 2),
        (0x08, 0),    // SetHardMinVcn
        (0x09, 800),  // SetSoftMinGfxclk: not Linux's AUTO
        (0x13, 1200), // SetHardMinSocclkByFreq
        (0x14, 1200), // SetSoftMinFclk
        (0x1D, 1200), // SetSoftMaxSocclkByFreq
        (0x1E, 1200), // SetSoftMaxFclkByFreq
        (0x1F, 1200), // SetSoftMaxVcn
        (0x20, 100),  // SetPowerLimitPercentage
        (0x23, 1200), // SetHardMinFclkByFreq
        (0x24, 1200), // SetSoftMinSocclkByFreq
    ] {
        kassert!(!smu::allowed_running(id, arg), "{id:#x} {arg:#x} went through");
    }
    // Everything else on this mailbox, whatever the argument. The display's clocks (DCFCLK, DISPCLK, DPPCLK) have no id
    // on it at all (smu_v13_0_4_ppsmc.h:54-94): they go through the display's own mailbox, which `Addrs` does not name.
    let listed = [msg::GET_ENABLED_SMU_FEATURES, msg::SET_DRIVER_DRAM_ADDR_HIGH, msg::SET_DRIVER_DRAM_ADDR_LOW, msg::TRANSFER_TABLE_SMU2DRAM, msg::ALLOW_GFXOFF, msg::SET_HARD_MIN_GFXCLK, msg::SET_SOFT_MAX_GFXCLK];
    for id in (0..=0xffu32).filter(|i| !listed.contains(i) && !smu::ALLOWED.contains(i)) {
        for arg in [0, 1, 800, 2700, smu13::RESET_MODE_2, 1 << 16 | 800] {
            kassert!(!smu::allowed_running(id, arg), "{id:#x} {arg:#x} went through");
        }
    }
    let d = crate::gpucore::fakechip::discovery_780m();
    let a = smu13::Addrs::from_discovery(&d).ok_or("no addresses")?;
    let mut f = FakeGpu::new(Hostile::None);
    let r = smu::send_running(&mut f, &a, SmuTimeouts::RUNNING, 0x1E, 1200);
    kassert!(r == Err(SmuError::Forbidden { id: 0x1E, arg: 1200 }) && f.smu.taken.is_empty(), "{r:?}");
    let r = smu::send_quiet(&mut f, &a, SmuTimeouts::RUNNING, msg::ALLOW_GFXOFF, 0);
    kassert!(r == Err(SmuError::Forbidden { id: msg::ALLOW_GFXOFF, arg: 0 }) && f.smu.taken.is_empty(), "send_quiet stays on bring-up's list");
    Ok(())
}
ktest::ktest!("gx.fake.dpm_guard", Safe, guard);

/// The tool is registered, class read. Its answer on the real node is the Mini-PC's check; here only its refusal.
fn tool_entry() -> ktest::TestResult {
    let t = mcp::find("gpu").ok_or("no tool gpu")?;
    kassert!(t.safety == mcp::Safety::Read && t.owner == "GX", "{:?} {}", t.safety, t.owner);
    Ok(())
}
ktest::ktest!("gx.fake.dpm_tool_entry", Safe, tool_entry);

// ---- on the fake chain

/// The start's messages in Linux's order: the capture's load, then AUTO's two limits, then GFXOFF after the delay.
pub(crate) fn order() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    // A pin an earlier kernel left: AUTO opens the whole range again.
    r.fake(|f| {
        f.smu.soft_max = 1000;
        f.smu.hard_min = 1000;
    });
    let from = mark(&r);
    let (d, w) = r.start_dpm()?;
    let m = taken_since(&r, from);
    let ids: Vec<u32> = m.iter().map(|x| x.0).collect();
    kassert!(
        ids == [msg::SET_DRIVER_DRAM_ADDR_HIGH, msg::SET_DRIVER_DRAM_ADDR_LOW, msg::GET_ENABLED_SMU_FEATURES, msg::GET_ENABLED_SMU_FEATURES, msg::TRANSFER_TABLE_SMU2DRAM, msg::SET_HARD_MIN_GFXCLK, msg::SET_SOFT_MAX_GFXCLK],
        "{m:x?}"
    );
    // The capture's order of the same messages up to the table (it asks for the feature mask once more), and its number.
    let mut load: Vec<u32> = smufake::LINUX_LOAD.iter().map(|x| x.0).filter(|i| ids.contains(i)).take(7).collect();
    load.dedup();
    let mut ours = ids[..5].to_vec();
    ours.dedup();
    kassert!(load == ours, "the capture: {load:x?}, ours {ours:x?}");
    kassert!(m[2].1 == 0 && m[3].1 == 1 && m[4].1 == dpm_table::TABLE_DPMCLOCKS, "{m:x?}");
    let (lo, hi) = smufake::GFX_RANGE;
    kassert!(m[5].1 == lo && m[6].1 == hi, "AUTO names the table's range: {m:x?}");
    r.fake(|f| {
        kassert!(f.smu.hard_min == lo && f.smu.soft_max == hi, "limits {} {}", f.smu.hard_min, f.smu.soft_max);
        kassert!(!f.smu.overlaps.iter().any(|o| o.0 == Client::Driver), "overlaps {:?}", f.smu.overlaps);
        // The table in the carve-out, as PMFW addresses it; the display's mailbox untouched.
        let mc = u64::from(f.smu.dram.0) << 32 | u64::from(f.smu.dram.1);
        kassert!(mc >= f.fb_mc && mc < f.fb_mc + f.fb_size, "driver table at {mc:#x}");
        kassert!(f.smu.taken.iter().all(|t| t.0 == Client::Driver), "the display's mailbox was used");
        // gfx_v11_cntl_pg: safe mode in, power gating, its delay, safe mode out.
        let g = w.gc;
        let want = [(g.rlc_safe_mode, 3), (g.rlc_pg_cntl, f.gc.pg_writes.get(1).map_or(0, |x| x.1) | 1), (g.rlc_pg_delay_3, dpm::PG_DELAY_3), (g.rlc_safe_mode, 1)];
        kassert!(f.gc.pg_writes == want, "{:x?}", f.gc.pg_writes);
        Ok(())
    })?;
    kassert!(matches!(d.report().level, Level::Auto(GfxRange { min, max }) if min == lo && max == hi));
    kassert!(d.enabled(), "SENSOR answers");

    // GFXOFF: not before the delay; once after it; the count at 0.
    let at = mark(&r);
    pass(&r, dpm::GFX_OFF_DELAY_US / 2);
    kassert!(matches!(d.tick(), Next::AfterUs(_)), "before the delay");
    kassert!(taken_since(&r, at).is_empty(), "the Allow went early");
    allow(&r, &d)?;
    kassert!(taken_since(&r, at) == [(msg::ALLOW_GFXOFF, 0)], "{:x?}", taken_since(&r, at));
    kassert!(d.tick() == Next::Idle && taken_since(&r, at).len() == 1, "a second Allow");
    let rep = d.report();
    kassert!(rep.requests == 0 && rep.allows == 1 && rep.disallows == 0 && rep.pg_on, "requests {} allows {}", rep.requests, rep.allows);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_order", Safe, order);

/// The clocks follow the fake's load, and the tool reads them with GC asleep: it stays asleep.
pub(crate) fn tool() -> ktest::TestResult {
    let (r, d, _) = up()?;
    allow(&r, &d)?;
    let reads = r.fake(|f| f.gc.asleep_reads);
    let at = mark(&r);
    let t = crate::tools_gpu::text(&d.report());
    let (lo, hi) = smufake::GFX_RANGE;
    for want in [
        format!("level: auto, gfx {lo}..{hi} MHz from PMFW's table"),
        format!("clocks: gfx {lo} MHz (average {lo}), soc 1200 MHz, mem 2800 MHz"),
        String::from("activity: gfx 0.00 % (average 0.00 %)"),
        String::from("temperature: gfx 39.00 C, soc 41.50 C"),
        String::from("power: socket 9.000 W (average 8.500), apu 7.200 W"),
        String::from("voltage: vdd 1165 mV, soc 762 mV"),
        String::from("throttle: none"),
        String::from("metrics counter: 1"),
        String::from("gfxoff: allowed; requests 0; power gating on; allow 1, disallow 0, failed messages 0"),
        String::from("gfxoff status: 0 in GFXOFF"),
    ] {
        kassert!(t.contains(want.as_str()), "no '{want}' in:\n{t}");
    }
    kassert!(taken_since(&r, at) == [(msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_SMU_METRICS)], "{:x?}", taken_since(&r, at));
    kassert!(r.fake(|f| f.gc.power == Power::Off && f.gc.asleep_reads == reads), "the tool touched GC");

    r.fake(|f| f.smu.load = 63);
    let t = crate::tools_gpu::text(&d.report());
    kassert!(t.contains(format!("clocks: gfx {hi} MHz").as_str()) && t.contains("activity: gfx 63.00 %"), "under load:\n{t}");
    kassert!(r.gpu.dpm_enabled() && r.gpu.sensor(Sensor::GfxSclk) == Some(hi) && r.gpu.sensor(Sensor::GpuLoad) == Some(63));
    kassert!(r.gpu.sensor(Sensor::GpuTemp) == Some(39_000) && r.gpu.sensor(Sensor::GpuAvgPower) == Some(8));

    let n = crate::tools_gpu::answer(None);
    kassert!(matches!(&n, Err(mcp::ToolError::Failed(w)) if w.contains("no backend")), "no node");
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_tool", Safe, tool);

/// A job on a block that sleeps: the chain step wakes it first, every GC register reads, the Allow comes back after.
pub(crate) fn jobs() -> ktest::TestResult {
    let (r, d, _) = up()?;
    allow(&r, &d)?;
    let g = r.gpu.clone();
    let m = Mem::new(&g)?;
    let cx = Cx::new(&g);
    for ty in [RingType::Gfx, RingType::Compute, RingType::Sdma] {
        let (reads, off) = r.fake(|f| (f.gc.asleep_reads, f.gc.off_access.len()));
        kassert!(run_one(&g, &m, &cx, ty, &write_ib(ty, DATA + 16, 0xabcd_0001))? == 1, "{} failed", ty.name());
        kassert!(m.gtt_u32(16) == 0xabcd_0001, "{} wrote nothing", ty.name());
        kassert!(r.fake(|f| f.gc.asleep_reads == reads && f.gc.off_access.len() == off), "{}: GC touched asleep: {:x?}", ty.name(), r.fake(|f| f.gc.off_access.clone()));
        kassert!(!d.flags().asleep.load(Ordering::Acquire), "awake after {}", ty.name());
        // Idle again: the next look after the delay allows it again.
        pass(&r, dpm::GFX_OFF_DELAY_US);
        let _ = d.tick();
        pass(&r, dpm::GFX_OFF_DELAY_US);
        kassert!(d.tick() == Next::Idle, "after {}", ty.name());
        kassert!(d.flags().asleep.load(Ordering::Acquire), "Allow after {}", ty.name());
        pass(&r, 1_000_000);
    }
    let rep = d.report();
    kassert!(rep.requests == 0 && rep.failures == 0 && rep.allows == rep.disallows + 1, "allow {} disallow {}", rep.allows, rep.disallows);
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_jobs", Safe, jobs);

/// INFO's register read: a sleeping block is woken for it (amdgpu_kms.c:754); the request is back after it.
pub(crate) fn regs() -> ktest::TestResult {
    let (r, d, _) = up()?;
    allow(&r, &d)?;
    let reg = r.gpu.chip_profile_register().ok_or("no plain register in the profile")?;
    let reads = r.fake(|f| f.gc.asleep_reads);
    let v = r.gpu.read_register_for_test(&reg);
    kassert!(v.is_some_and(|x| x != u32::MAX), "read {v:x?}");
    kassert!(r.fake(|f| f.gc.asleep_reads == reads), "read while asleep");
    let rep = d.report();
    kassert!(rep.disallows == 1 && rep.requests == 0 && !rep.asleep, "disallow {} requests {}", rep.disallows, rep.requests);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_regs", Safe, regs);

/// Refusals: a failed message is logged and leaves the clocks; PMFW silent ends the messages, never the driver.
pub(crate) fn refused() -> ktest::TestResult {
    let (lo, hi) = smufake::GFX_RANGE;
    // HardMin refused: no SoftMax after it (Linux returns there), the limits as they were, GFXOFF and jobs go on.
    let r = rig(JobCfg::default())?;
    r.fake(|f| {
        f.smu.hostile.push(SmuHostile::Answer { id: msg::SET_HARD_MIN_GFXCLK, code: resp::FAILED });
        f.smu.soft_max = 1000;
    });
    let (d, _) = r.start_dpm()?;
    r.fake(|f| {
        kassert!(f.smu.hard_min == lo && f.smu.soft_max == 1000, "{} {}", f.smu.hard_min, f.smu.soft_max);
        kassert!(!f.smu.taken.iter().any(|t| t.1 == msg::SET_SOFT_MAX_GFXCLK), "SoftMax after a refused HardMin");
        Ok(())
    })?;
    kassert!(matches!(d.report().level, Level::Left(w) if w.contains("SetHardMinGfxClk")), "{:?}", d.report().level);
    kassert!(d.report().failures == 1 && d.report().metrics.is_ok(), "the metrics still work");
    allow(&r, &d)?;
    let m = Mem::new(&r.gpu)?;
    kassert!(run_one(&r.gpu, &m, &Cx::new(&r.gpu), RingType::Gfx, &write_ib(RingType::Gfx, DATA, 7))? == 1, "no job after a refused limit");
    drop(m);
    r.finish_jobs()?;

    // PMFW runs no GFX DPM: no limit is sent.
    let r = rig(JobCfg::default())?;
    r.fake(|f| f.smu.hostile.push(SmuHostile::Value { id: msg::GET_ENABLED_SMU_FEATURES, value: 0 }));
    let (d, _) = r.start_dpm()?;
    kassert!(r.fake(|f| !f.smu.taken.iter().any(|t| t.1 == msg::SET_HARD_MIN_GFXCLK || t.1 == msg::SET_SOFT_MAX_GFXCLK)), "a limit without GFX DPM");
    kassert!(matches!(d.report().level, Level::Left(w) if w.contains("no GFX DPM")));
    r.finish_jobs()?;

    // PMFW silent on the table: bounded, no message after it, the job half goes on, the tool says so.
    let r = rig(JobCfg::default())?;
    r.fake(|f| f.smu.hostile.push(SmuHostile::Never { id: msg::TRANSFER_TABLE_SMU2DRAM }));
    let t0 = r.fake(|f| f.now);
    let room = r.gpu.heaps_table_room();
    let (d, _) = r.start_dpm()?;
    // PMFW may still write the page, and the same heap serves page tables.
    kassert!(r.gpu.heaps_table_room() < room, "the driver table's page went back while PMFW may write to it");
    let waited = r.fake(|f| f.now) - t0;
    kassert!(waited <= 2 * SmuTimeouts::RUNNING.answer_us + 1_000_000, "waited {waited} us");
    let at = mark(&r);
    let rep = d.report();
    kassert!(rep.dead && !d.enabled() && rep.requests == 1 && taken_since(&r, at).is_empty(), "dead {} requests {}", rep.dead, rep.requests);
    kassert!(crate::tools_gpu::text(&rep).contains("PMFW silent") && crate::tools_gpu::text(&rep).contains("metrics: FAILED"), "{}", crate::tools_gpu::text(&rep));
    kassert!(r.gpu.sensor(Sensor::GfxSclk).is_none() && !r.gpu.dpm_enabled(), "SENSOR");
    let m = Mem::new(&r.gpu)?;
    kassert!(run_one(&r.gpu, &m, &Cx::new(&r.gpu), RingType::Gfx, &write_ib(RingType::Gfx, DATA, 7))? == 1, "no job with PMFW silent");
    drop(m);
    r.fake(|f| f.smu.hostile.clear());
    d.stop();
    kassert!(r.gpu.heaps_table_room() < room, "the stop gave the page back");
    r.finish_jobs()?;

    // Allow refused: never asked again, the block stays up.
    let (r, d, _) = up()?;
    r.fake(|f| f.smu.hostile.push(SmuHostile::Answer { id: msg::ALLOW_GFXOFF, code: resp::FAILED }));
    pass(&r, dpm::GFX_OFF_DELAY_US);
    kassert!(d.tick() == Next::Idle && !d.flags().asleep.load(Ordering::Acquire));
    pass(&r, dpm::GFX_OFF_DELAY_US);
    let at = mark(&r);
    kassert!(d.tick() == Next::Idle && taken_since(&r, at).is_empty(), "asked again");
    kassert!(crate::tools_gpu::text(&d.report()).contains("gfxoff: off, refused"), "{}", crate::tools_gpu::text(&d.report()));
    r.finish_jobs()?;

    // The RLC never answers safe mode: no GFXOFF at all, clocks stand.
    let r = rig(JobCfg::default())?;
    r.fake(|f| f.gc.safe_mode_answers = false);
    let (d, _) = r.start_dpm()?;
    kassert!(matches!(d.report().level, Level::Auto(g) if g.max == hi));
    pass(&r, dpm::GFX_OFF_DELAY_US);
    let at = mark(&r);
    kassert!(d.tick() == Next::Idle && taken_since(&r, at).is_empty() && d.report().requests == 1, "Allow without the RLC");
    r.fake(|f| f.gc.safe_mode_answers = true);
    r.finish_jobs()?;

    // Disallow refused: the block counts as asleep, INFO's read answers nothing, the next step tries again.
    let (r, d, _) = up()?;
    allow(&r, &d)?;
    let reg = r.gpu.chip_profile_register().ok_or("no plain register")?;
    r.fake(|f| f.smu.hostile.push(SmuHostile::Answer { id: msg::DISALLOW_GFXOFF, code: resp::FAILED }));
    kassert!(r.gpu.read_register_for_test(&reg).is_none(), "a read of a sleeping block");
    kassert!(d.flags().asleep.load(Ordering::Acquire) && d.report().failures >= 1);
    r.fake(|f| f.smu.hostile.clear());
    kassert!(r.gpu.read_register_for_test(&reg).is_some_and(|x| x != u32::MAX), "after PMFW answered again");
    kassert!(!d.flags().asleep.load(Ordering::Acquire));
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_refused", Safe, refused);

/// The stop and the Quiesce hook: Disallow, then the RLC's part off, in Linux's order; the page back; pass-through after.
pub(crate) fn stop() -> ktest::TestResult {
    let (r, d, w) = up()?;
    allow(&r, &d)?;
    let room = r.gpu.heaps_table_room();
    let at = mark(&r);
    let writes = r.fake(|f| f.gc.pg_writes.len());
    d.stop();
    kassert!(taken_since(&r, at) == [(msg::DISALLOW_GFXOFF, 0)], "{:x?}", taken_since(&r, at));
    r.fake(|f| {
        let g = w.gc;
        let pg = f.gc.pg_writes.split_off(writes);
        kassert!(pg.len() == 3 && pg[0] == (g.rlc_safe_mode, 3) && pg[1].0 == g.rlc_pg_cntl && pg[1].1 & 1 == 0 && pg[2] == (g.rlc_safe_mode, 1), "{pg:x?}");
        kassert!(!f.gc.gfxoff_allowed, "GFXOFF stays allowed");
        Ok(())
    })?;
    kassert!(r.gpu.heaps_table_room() > room, "the driver table's page stayed");
    let f = d.flags();
    kassert!(!f.asleep.load(Ordering::Acquire) && !f.pg_on.load(Ordering::Acquire));
    kassert!(!d.enabled() && d.metrics().is_err(), "metrics after the stop");
    pass(&r, dpm::GFX_OFF_DELAY_US * 2);
    let at = mark(&r);
    kassert!(d.tick() == Next::Exit && taken_since(&r, at).is_empty(), "a message after the stop");
    let a = d.awake();
    kassert!(a.up && taken_since(&r, at).is_empty(), "awake after the stop");
    drop(a);
    d.stop();
    kassert!(taken_since(&r, at).is_empty(), "a second stop");
    r.finish_jobs()?;

    // The Quiesce hook, by the flags alone: first Disallow, then power gating off; nothing the second time.
    let (r, d, w) = up()?;
    allow(&r, &d)?;
    let at = mark(&r);
    let flags = d.flags();
    r.fake(|f| crate::dpm_live::quiesce(f, &w, &flags));
    kassert!(taken_since(&r, at) == [(msg::DISALLOW_GFXOFF, 0)], "{:x?}", taken_since(&r, at));
    kassert!(!flags.asleep.load(Ordering::Acquire) && !flags.pg_on.load(Ordering::Acquire));
    let at = mark(&r);
    let writes = r.fake(|f| f.gc.pg_writes.len());
    r.fake(|f| crate::dpm_live::quiesce(f, &w, &flags));
    kassert!(taken_since(&r, at).is_empty() && r.fake(|f| f.gc.pg_writes.len()) == writes, "the hook twice");
    // PMFW silent: bounded, GC stays unwritten.
    flags.asleep.store(true, Ordering::Release);
    flags.pg_on.store(true, Ordering::Release);
    r.fake(|f| {
        f.smu.hostile.push(SmuHostile::Never { id: msg::DISALLOW_GFXOFF });
        crate::dpm_live::quiesce(f, &w, &flags);
    });
    kassert!(flags.asleep.load(Ordering::Acquire) && r.fake(|f| f.gc.pg_writes.len()) == writes, "written with no answer");
    flags.asleep.store(false, Ordering::Release);
    flags.pg_on.store(false, Ordering::Release);
    r.fake(|f| f.smu.hostile.clear());
    drop(d);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_stop", Safe, stop);
