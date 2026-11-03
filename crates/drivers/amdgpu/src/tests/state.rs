//! GX-7b: the state classifier over the Mini-PC's captured registers (tests/data/minipc-780m-state.txt: cold =
//! the Linux trace's first reads, after Linux = read back after the load), the bring-up owner's write log over
//! the PSP fake, the carve-out plan for any MEMSIZE, and the BO manager's reserved ranges for 512 MiB to 16 GB.

use crate::attach;
use crate::bo::{self, BoManager, Reserved};
use crate::gpucore::bringup::{Bringup, Gate, Outcome};
use crate::gpucore::classify::{self, GcPower, Start};
use crate::gpucore::disc::{self, Discovery};
use crate::gpucore::psp::{Carve, Hw, Psp, Timeouts};
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource};
use crate::gpucore::vram::{self, CarvePlan, Look};
use crate::ipmod::psp13::Mailbox13;
use crate::layout as l;
use super::fake::{BYTES, gop, mini_pc, mini_pc_native, pci, reader};
use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec;

fn blob() -> Option<Discovery> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/gx/fixtures/minipc-780m-discovery.bin");
    let Ok(buf) = std::fs::read(&p) else {
        std::eprintln!("skip: {} missing", p.display());
        return None;
    };
    Some(disc::parse(&buf).unwrap())
}

type Img = BTreeMap<u32, u32>;

/// (cold, after Linux) register images, BAR5 byte -> value, and each name's byte, from the fixture.
fn images() -> (Img, Img, BTreeMap<&'static str, u32>) {
    let text = include_str!("../../tests/data/minipc-780m-state.txt");
    let (mut cold, mut after, mut names) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let f: Vec<&'static str> = line.split_whitespace().collect();
        let byte = hex(f[1]) * 4;
        names.insert(f[0], byte);
        if f[2] != "-" {
            cold.insert(byte, hex(f[2]));
        }
        if f[3] != "-" {
            after.insert(byte, hex(f[3]));
        }
    }
    (cold, after, names)
}

fn run(d: &Discovery, img: &BTreeMap<u32, u32>) -> (classify::State, Vec<u32>) {
    let mut seen = Vec::new();
    let st = classify::classify(d, &mut |b| {
        seen.push(b);
        img.get(&b).copied()
    });
    (st, seen)
}

#[test]
fn fixture_addresses_are_the_classifiers() {
    let Some(d) = blob() else { return };
    let (_, _, names) = images();
    let addrs: BTreeMap<_, _> = classify::addresses(&d).into_iter().collect();
    for (n, b) in &names {
        assert_eq!(addrs.get(n).copied().flatten(), Some(*b), "{n}");
    }
    // Every address the classifier reads lies in BAR5 (512 KiB) and outside the index/data pairs.
    for (n, b) in &addrs {
        let b = b.unwrap_or_else(|| panic!("{n}: no address in the real table"));
        assert!(b < 0x8_0000 && crate::state::wide_readable(b), "{n} @{b:#x}");
    }
}

#[test]
fn cold_start_from_the_trace() {
    let Some(d) = blob() else { return };
    let (cold, _, names) = images();
    let (st, seen) = run(&d, &cold);
    assert_eq!(st.start, Start::Cold, "{:?}", st.why);
    // SMUIO's GFXOFF status is not in the capture: GC's state unknown, so no GC core register was read (the IMU
    // flags are read always and only recorded, GX-7f; Linux never read them, so the capture has no value).
    assert_eq!(st.gc, GcPower::Unknown);
    let gc: Vec<u32> = classify::addresses(&d).into_iter().filter(|(n, _)| n.starts_with("RLC_") || n.starts_with("CP_") || n.starts_with("GRBM") || n.starts_with("GC")).filter_map(|(_, b)| b).collect();
    assert_eq!(gc.len(), 12, "GX-7h: GFXHUB's VMID 0 table base too");
    assert!(seen.iter().all(|b| !gc.contains(b)), "a GC register was read while GC's power is unknown");
    let f = |n: &str| st.findings.iter().find(|f| f.name == n).unwrap().clone();
    assert_eq!(f("MMMC_VM_FB_OFFSET").meaning, "carve-out at CPU 0x840000000 (Linux's APU aperture base)");
    assert_eq!(f("RCC_CONFIG_MEMSIZE").meaning, "512 MiB of graphics memory (the carve-out's size)");
    assert_eq!(f("MMVM_CONTEXT0_CNTL").meaning, "VMID 0 (GART) off");
    assert_eq!(f("GFX_IMU_MSG_FLAGS").value, None);
    assert_eq!(f("GFX_IMU_MSG_FLAGS").meaning, "not readable");
    // Every finding carries its meaning and its address.
    assert!(st.findings.iter().all(|f| !f.meaning.is_empty() && f.byte.is_some()));
    assert_eq!(names.len(), 29);
}

#[test]
fn after_linux_from_the_read_back() {
    let Some(d) = blob() else { return };
    let (_, after, _) = images();
    let (st, _) = run(&d, &after);
    assert_eq!(st.start, Start::AfterLinux);
    assert_eq!(st.why.len(), 3, "IH, doorbells and GART all say Linux: {:?}", st.why);
    let m = |n: &str| st.findings.iter().find(|f| f.name == n).unwrap().meaning.clone();
    assert_eq!(m("MMMC_VM_SYSTEM_APERTURE_LOW_ADDR"), "system aperture from MC 0x8000000000");
    assert_eq!(m("MMMC_VM_SYSTEM_APERTURE_HIGH_ADDR"), "system aperture to MC 0x801fffffff");
    assert_eq!(m("MMMC_VM_AGP_TOP"), "AGP aperture top 0: AGP off (as Linux programs it)");
    assert_eq!(m("MP1_C2PMSG_66"), "last message to PMFW: 0x6 PowerDownVcn");
    assert_eq!(m("MP0_C2PMSG_67"), "KM ring write pointer 432 dwords (27 frames): a driver used a ring");
    assert_eq!(m("IH_DOORBELL_RPTR"), "IH read pointer by doorbell on (offset 0x2f0)");
}

/// The other starts, from the cold image with the registers that tell them apart changed.
#[test]
fn bitsos_crash_and_dead_psp() {
    let Some(d) = blob() else { return };
    let (cold, _, names) = images();
    let at = |n: &str| names.get(n).copied().unwrap_or_else(|| classify::addresses(&d).into_iter().find(|(x, _)| *x == n).unwrap().1.unwrap());
    let with = |ch: &[(&str, u32)]| {
        let mut m = cold.clone();
        for (n, v) in ch {
            m.insert(at(n), *v);
        }
        m
    };
    // A clean BitsOS stop: GC powered (SMUIO 2, IMU flags 2), RLC booted, ring destroyed (write pointer 0).
    let on = [("SMUIO_GFX_MISC_CNTL", 2 << 1), ("GFX_IMU_MSG_FLAGS", 2 << 1), ("RLC_RLCS_BOOTLOAD_STATUS", 0x8000_0000)];
    let (st, seen) = run(&d, &with(&on));
    assert_eq!((st.start, st.gc), (Start::AfterBitsOs, GcPower::On), "{:?}", st.why);
    assert!(seen.contains(&at("CP_STAT")), "GC read once powered");
    // GX-7f: the IMU flags decide nothing (the 28.10. values, and 0): SMUIO 2 reads GC either way.
    for flags in [0x8000_0001, 0x8000_000d, 0x8000_001f, 0] {
        let (st, seen) = run(&d, &with(&[("SMUIO_GFX_MISC_CNTL", 2 << 1), ("GFX_IMU_MSG_FLAGS", flags), ("RLC_RLCS_BOOTLOAD_STATUS", 0xc000_001f)]));
        assert_eq!((st.start, st.gc), (Start::AfterBitsOs, GcPower::On), "{flags:#x}: {:?}", st.why);
        assert!(seen.contains(&at("CP_STAT")), "{flags:#x}");
        assert!(st.findings.iter().find(|f| f.name == "GFX_IMU_MSG_FLAGS").is_some_and(|f| f.value == Some(flags) && f.meaning.starts_with("IMU message flags (recorded only")));
    }
    // GFXOFF (SMUIO 0): the IMU flags are read (recorded), no GC core register.
    let (st, seen) = run(&d, &with(&[("SMUIO_GFX_MISC_CNTL", 0)]));
    assert_eq!((st.start, st.gc), (Start::Cold, GcPower::Off { status: 0 }));
    assert!(seen.contains(&at("GFX_IMU_MSG_FLAGS")) && !seen.contains(&at("CP_STAT")));
    // A ring left behind, a message without answer, GC on without RLC: each a crash.
    for ch in [&[("MP0_C2PMSG_67", 0x40)][..], &[("MP1_C2PMSG_66", 0x19), ("MP1_C2PMSG_90", 0)], &[on[0], on[1], ("RLC_RLCS_BOOTLOAD_STATUS", 0)]] {
        let (st, _) = run(&d, &with(ch));
        assert_eq!(st.start, Start::AfterCrash, "{ch:?}: {:?}", st.why);
    }
    // No message and no answer: PMFW as it comes out of reset, not a crash.
    let (st, _) = run(&d, &with(&[("MP1_C2PMSG_66", 0), ("MP1_C2PMSG_90", 0)]));
    assert_eq!(st.start, Start::Cold, "{:?}", st.why);
    let (st, _) = run(&d, &with(&[("MP0_C2PMSG_81", 0)]));
    assert_eq!(st.start, Start::PspDown);
    // Nothing readable: unknown, and every finding still says why.
    let (st, _) = run(&d, &BTreeMap::new());
    assert_eq!(st.start, Start::Unknown);
    assert!(st.findings.iter().all(|f| f.value.is_none() && !f.meaning.is_empty()));
}

/// The owner over the PSP fake: every write passes with its read-back, the log equals the fake's own record,
/// a shut gate writes nothing and logs each refusal, the kept part is bounded while the counts are not.
#[test]
fn bringup_owner_logs_every_write() {
    let mut owner = Bringup::new(FakeGpu::new(Hostile::None), Gate::Allow, 8);
    let opts = crate::gpucore::ucode::ListOpts::PSP13_0_4;
    let (fw, _) = crate::gpucore::fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).unwrap();
    let list = fw.load_list(&opts).unwrap();
    let (mc, size, phys, a) = { let f = owner.hw(); (f.fb_mc, f.fb_size, f.fb_phys, f.a) };
    let mut bo = BoManager::new(size, Vec::new());
    let mut p = Psp::new(&mut bo, Mailbox13 { addrs: a }, Carve { mc_base: mc, phys_base: phys }, Timeouts::LINUX).unwrap();
    p.start(&mut owner, &mut bo, &fw, &list, opts.autoload).unwrap();
    let fake_writes = owner.hw().writes.clone();
    let t = owner.log().totals();
    assert_eq!(t.writes, fake_writes.len() as u64);
    assert_eq!((t.refused, t.failed, t.mismatched), (0, 0, 0));
    assert_eq!(t.registers, 5, "ring address lo/hi/size, control, write pointer");
    let kept: Vec<_> = owner.log().recent().collect();
    assert_eq!(kept.len(), 8);
    let tail = &fake_writes[fake_writes.len() - 8..];
    for (e, &(b, v)) in kept.iter().zip(tail) {
        assert_eq!((e.byte, e.value, e.outcome), (b, v, Outcome::Ok));
        assert!(e.read_back.is_some());
    }
    assert_eq!(owner.log().count(a.c2pmsg_67), fake_writes.iter().filter(|w| w.0 == a.c2pmsg_67).count() as u64);
    assert_eq!(owner.log().since(t.writes - 2).count(), 2);
    // A checked register whose bits do not stick: a mismatch, still written.
    owner.expect_read_back(0x1234, 0xFF);
    let e = owner.write(0x1234, 0x5A);
    assert!(matches!(e.outcome, Outcome::Ok | Outcome::Mismatch));

    // Gate shut: nothing reaches the fake, every attempt is in the log.
    let mut shut = Bringup::new(FakeGpu::new(Hostile::None), Gate::Refuse("test"), 4);
    assert!(!shut.reg_write(0x5_8000, 1));
    assert!(!shut.mem_write(pspfake::FB_MC, &[1, 2, 3, 4]));
    assert!(shut.hw().writes.is_empty());
    assert_eq!(shut.log().totals().refused, 1);
    assert_eq!(shut.log().totals().mem_refused, 1, "a refused memory write is counted");
    assert_eq!(owner.log().totals().mem_refused, 0);
    assert_eq!(shut.log().recent().next().unwrap().outcome, Outcome::Refused);
}

/// The PSP bring-up does not care how big the carve-out is (512 MiB as on 27.10., 8 GB, 16 GB).
#[test]
fn psp_runs_with_any_carve_out_size() {
    for size in [512u64 << 20, 8 << 30, 16 << 30] {
        let mut fake = FakeGpu::new(Hostile::None);
        fake.fb_size = size;
        let r = pspfake::run(&mut fake, &mut MapSource(pspfake::synthetic_780m())).unwrap_or_else(|e| panic!("{size:#x}: {e:?}"));
        let tmr = r.psp.tmr.unwrap();
        assert!(tmr.mc >= pspfake::FB_MC && tmr.mc + tmr.size <= pspfake::FB_MC + size);
    }
}

#[test]
fn carve_plan_for_any_memsize() {
    // 27.10.: MMMC_VM_FB_OFFSET 0x840, MEMSIZE 512: exactly the 0x85fff0000 gx_probe read 0xff at.
    let p = CarvePlan::new(0x840, 512).unwrap();
    assert_eq!((p.base, p.size), (0x8_4000_0000, 512 << 20));
    let at: Vec<(&str, u64, usize)> = p.samples.iter().map(|s| (s.what, p.phys(s), s.len)).collect();
    assert_eq!(at, [(vram::START, 0x8_4000_0000, 4096), (vram::BELOW, 0x8_5FFE_F000, 4096), (vram::DISCOVERY, 0x8_5FFF_0000, 8192)]);
    // 8 GB and 16 GB at the same offset; only bits 23:0 count.
    assert_eq!(CarvePlan::new(0xFF00_0840, 8192).unwrap().phys(&CarvePlan::new(0x840, 8192).unwrap().samples[2]), 0x8_4000_0000 + (8 << 30) - (64 << 10));
    assert_eq!(CarvePlan::new(0x840, 16384).unwrap().size, 16 << 30);
    assert_eq!(CarvePlan::new(0, 512), Err(vram::VramError::NoCarveOut));
    assert_eq!(CarvePlan::new(0x840, 0), Err(vram::VramError::Range));
    // The conclusions.
    assert_eq!(vram::look(&[0xFF; 16]), Look::AllOnes);
    assert_eq!(vram::look(&[0; 16]), Look::AllZero);
    assert_eq!(vram::look(&[0, 1, 0, 2]), Look::Data { nonzero: 2 });
    let data = Some(Look::Data { nonzero: 9 });
    assert!(vram::conclude(data, data, Some(Look::AllOnes), false).contains("closes the top of the carve-out"));
    assert!(vram::conclude(Some(Look::AllOnes), Some(Look::AllOnes), Some(Look::AllOnes), false).starts_with("every sample reads all ones"));
    assert!(vram::conclude(data, Some(Look::AllOnes), Some(Look::AllOnes), false).contains("smaller than MEMSIZE"));
    assert!(vram::conclude(data, data, data, true).starts_with("the CPU reads"));
    assert!(vram::conclude(data, data, None, false).contains("not read"));
}

fn manager(img: &crate::tests::fake::Image) -> (attach::Attach, BoManager) {
    let a = attach::gates(&pci(), Some(gop()), &reader(img)).unwrap();
    let m = BoManager::new(bo::carve_size(&a), bo::reserved_from(&a));
    (a, m)
}

/// The real 512 MiB of 22.10./27.10. (MEMSIZE 0x200, FB 0x8000-0x801F), 8 GB and 16 GB (FB top moved with it),
/// and a BIOS whose FB range is wider than MEMSIZE: the manager ends at MEMSIZE, the discovery reserve with it.
#[test]
fn reserved_ranges_follow_memsize() {
    let (a, m) = manager(&mini_pc_native());
    assert_eq!((a.memsize_mb, a.fb_first, a.fb_last), (512, 0x80_0000_0000, 0x80_1FFF_FFFF));
    assert_eq!(m.size(), 512 << 20);
    assert_eq!(m.reserved()[0], Reserved { name: String::from("gop"), range: 0..a.bytes });
    let d = m.reserved().iter().find(|r| r.name == "discovery").unwrap().range.clone();
    assert_eq!(d, (512 << 20) - (64 << 10)..512 << 20);
    assert!(m.reserved().iter().all(|r| r.range.end <= 512 << 20));
    for gb in [8u32, 16] {
        let mut img = mini_pc();
        img.insert(l::MEMSIZE, gb << 10);
        img.insert(l::FB_TOP, 0x8000 + (gb << 6) - 1);
        let (_, m) = manager(&img);
        assert_eq!(m.size(), u64::from(gb) << 30);
        let d = m.reserved().iter().find(|r| r.name == "discovery").unwrap().range.clone();
        assert_eq!(d, (u64::from(gb) << 30) - (64 << 10)..u64::from(gb) << 30);
    }
    // FB range 8 GB, MEMSIZE 512 MiB: the BIOS's range is not graphics memory past MEMSIZE.
    let mut img = mini_pc();
    img.insert(l::MEMSIZE, 512);
    let (a, m) = manager(&img);
    assert_eq!(a.fb_last - a.fb_first + 1, 8 << 30);
    assert_eq!(m.size(), 512 << 20);
    assert!(m.reserved().iter().all(|r| r.range.end <= 512 << 20), "{:?}", m.reserved());
    assert_eq!(m.reserved().iter().find(|r| r.name == "discovery").unwrap().range, (512 << 20) - (64 << 10)..512 << 20);
    let _ = BYTES;
}

/// The GC addresses of the real table (IMU flags, RLC, CP, GRBM, GFXHUB).
fn gc_addrs(d: &Discovery) -> Vec<u32> {
    classify::addresses(d)
        .into_iter()
        .filter(|(n, _)| n.starts_with("GFX_") || n.starts_with("RLC_") || n.starts_with("CP_") || n.starts_with("GRBM") || n.starts_with("GC"))
        .filter_map(|(_, b)| b)
        .collect()
}

/// GC powered at the check but gated before the re-check: the GC values are thrown away, GC's power unknown.
#[test]
fn gc_gating_during_the_reads_discards_gc() {
    let Some(d) = blob() else { return };
    let (cold, _, names) = images();
    let smuio = classify::addresses(&d).into_iter().find(|(n, _)| *n == "SMUIO_GFX_MISC_CNTL").unwrap().1.unwrap();
    let imu = classify::addresses(&d).into_iter().find(|(n, _)| *n == "GFX_IMU_MSG_FLAGS").unwrap().1.unwrap();
    let rlc = classify::addresses(&d).into_iter().find(|(n, _)| *n == "RLC_RLCS_BOOTLOAD_STATUS").unwrap().1.unwrap();
    let mut img = cold.clone();
    img.insert(imu, 2 << 1);
    img.insert(rlc, 0);
    let mut smuio_reads = 0;
    let st = classify::classify(&d, &mut |b| {
        if b == smuio {
            smuio_reads += 1;
            return Some(if smuio_reads == 1 { 2 << 1 } else { 0 });
        }
        img.get(&b).copied()
    });
    assert_eq!(smuio_reads, 2, "SMUIO read again after the GC reads");
    assert_eq!(st.gc, GcPower::Unknown);
    // RLC 0 would have said "crash": thrown away, the start is cold.
    assert_eq!(st.start, Start::Cold, "{:?}", st.why);
    for f in st.findings.iter().filter(|f| gc_addrs(&d).contains(&f.byte.unwrap()) && f.name != "GFX_IMU_MSG_FLAGS") {
        assert_eq!(f.value, None, "{}", f.name);
        assert!(f.meaning.starts_with("not used: GC left the powered state"), "{}: {}", f.name, f.meaning);
    }
    let _ = names;
}

/// A dead device (all ones), a bus that reads 0 everywhere, and random values: no panic, no GC core register read
/// unless SMUIO said "on" (GX-7f: the IMU flags are read always and decide nothing), a defined verdict.
#[test]
fn hostile_register_values() {
    let Some(d) = blob() else { return };
    let smuio = classify::addresses(&d).into_iter().find(|(n, _)| *n == "SMUIO_GFX_MISC_CNTL").unwrap().1.unwrap();
    let imu = classify::addresses(&d).into_iter().find(|(n, _)| *n == "GFX_IMU_MSG_FLAGS").unwrap().1.unwrap();
    let gc: Vec<u32> = gc_addrs(&d).into_iter().filter(|b| *b != imu).collect();
    let mut seen = Vec::new();
    let st = classify::classify(&d, &mut |b| {
        seen.push(b);
        Some(u32::MAX)
    });
    assert_eq!(st.start, Start::Unknown, "{:?}", st.why);
    assert!(st.why[0].contains("all ones"), "{:?}", st.why);
    assert_eq!(st.gc, GcPower::Off { status: 3 });
    assert!(seen.iter().all(|b| !gc.contains(b)), "GC read on a dead device");
    let mut seen = Vec::new();
    let st = classify::classify(&d, &mut |b| {
        seen.push(b);
        Some(0)
    });
    assert_eq!((st.start, st.gc), (Start::PspDown, GcPower::Off { status: 0 }));
    assert!(seen.iter().all(|b| !gc.contains(b)));
    // xorshift: reproducible pseudo-random registers.
    let mut x: u32 = 0x9E37_79B9;
    for _ in 0..2000 {
        let mut seen = Vec::new();
        let st = classify::classify(&d, &mut |b| {
            seen.push(b);
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xF != 0).then_some(x)
        });
        assert!(st.findings.iter().all(|f| !f.meaning.is_empty()));
        assert!(!st.why.is_empty());
        let first_gc = seen.iter().position(|b| gc.contains(b));
        if let Some(i) = first_gc {
            // The first GC core access comes after an SMUIO read and the IMU flags.
            assert!(seen[..i].contains(&smuio) && seen[..i].contains(&imu));
        }
        if st.gc == GcPower::On {
            assert!(st.value("SMUIO_GFX_MISC_CNTL").is_some_and(crate::gpucore::gfxup::smuio_gc_on));
        } else {
            assert!(st.findings.iter().filter(|f| gc.contains(&f.byte.unwrap()) && f.name != "GFX_IMU_MSG_FLAGS").all(|f| f.value.is_none()));
        }
    }
}

/// The carve-out offset's cross-check compares MMHUB with DCN, never DCN with itself.
#[test]
fn carve_offset_cross_check() {
    use crate::carve::offset_line;
    assert_eq!(offset_line("MMMC_VM_FB_OFFSET", 0x840, Some(0x840)).unwrap(), "carve-out offset: MMMC_VM_FB_OFFSET 0x840, DCN_VM_FB_OFFSET 0x840: agree");
    assert!(offset_line("MMMC_VM_FB_OFFSET", 0x840, Some(0x66)).unwrap().ends_with("DIFFER (MMHUB is Linux's source)"));
    assert!(offset_line("MMMC_VM_FB_OFFSET", 0xFF00_0840, Some(0x840)).unwrap().ends_with("agree"), "only bits 23:0 count");
    assert_eq!(offset_line("MMMC_VM_FB_OFFSET", 0x840, None), None);
    let l = offset_line("DCN_VM_FB_OFFSET", 0x840, Some(0x840)).unwrap();
    assert!(l.contains("needs a discovery table") && !l.contains("agree"), "{l}");
}

/// The probe's sections 2 to 5 over fakes: whatever the carve-out gave, every other section is in the answer.
#[test]
fn probe_reports_every_section() {
    use crate::carve::Carve;
    use crate::probe::{Found, Via, assemble, bringup_line};
    let Some(d) = blob() else { return };
    let bin = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/gx/fixtures/minipc-780m-discovery.bin")).unwrap();
    let head = String::from("gpu: 0000:c4:00.0 1002:1900 rev 0xc1, BAR0 0x7c00000000 (262144 KiB)\nmemsize: 512 MiB (RCC_CONFIG_MEMSIZE)\n");
    let iommu = [String::from("iommu 00:00.2 (IVHD 0x40) at 0xfd200000: control 0x0, IommuEn 0 (translation off), EFR 0x0")];
    let table = crate::ip::known_blocks(0x1900);
    let (cold, _, _) = images();
    let mut calls = 0;
    let mut state = |dd: &Discovery| {
        calls += 1;
        let st = classify::classify(dd, &mut |b| cold.get(&b).copied());
        let mut v = crate::state::lines(&st);
        v.push(bringup_line(&Default::default()));
        v
    };
    let verdict = String::from("carve-out verdict: the CPU reads all ones at the discovery area");
    // 1. The carve-out failed, no copy: discovery and state say why, the rest is there.
    let c = Carve { lines: std::vec![String::from("carve-out sample discovery at 0x85fff0000: map 0x85fff0000: OverlapsRam"), verdict.clone()], discovery: None };
    let (a, keep) = assemble(head.clone(), c, None, &table, false, &mut state, &iommu);
    assert!(keep.is_none());
    for p in ["gpu: ", "memsize: ", "carve-out sample", "carve-out verdict", "discovery: not available", "state: not classified", "iommu "] {
        assert!(a.lines().any(|l| l.starts_with(p)), "no {p} line in:\n{a}");
    }
    // 2. The carve-out bytes are all 0xff and no copy: the REFUSED detail of those bytes is kept.
    let c = Carve { lines: std::vec![verdict.clone()], discovery: Some((0x8_5FFF_0000, std::vec![0xFF; 8192])) };
    let (a, keep) = assemble(head.clone(), c, None, &table, false, &mut state, &iommu);
    assert!(keep.is_none(), "garbage is not kept");
    let l = a.lines().find(|l| l.starts_with("discovery: ")).unwrap();
    assert!(l.starts_with("discovery: REFUSED (") && l.contains("read via carve-out at phys 0x85fff0000") && l.ends_with("8192 bytes"), "{l}");
    assert!(a.lines().any(|l| l.starts_with("state: not classified")) && a.lines().any(|l| l.starts_with("iommu ")));
    // 3. Garbage at the carve-out but a copy kept this boot: classified with the copy, which is not kept again.
    let c = Carve { lines: std::vec![verdict.clone()], discovery: Some((0x8_5FFF_0000, std::vec![0xFF; 8192])) };
    let copy = Found { via: Via::Indirect { pos: 0x1FFF_0000 }, bytes: bin.clone() };
    let (a, keep) = assemble(head.clone(), c, Some(copy), &table, false, &mut state, &iommu);
    assert!(keep.is_none());
    assert!(a.contains("discovery source: the indirect copy of this boot"));
    assert!(a.lines().any(|l| l.starts_with("discovery: ok via")), "{a}");
    assert!(a.lines().any(|l| l == "state: cold (firmware state)"), "{a}");
    assert!(a.lines().any(|l| l.starts_with("bringup: 0 writes (0 refused, 0 failed, 0 read back wrong, 0 memory writes refused)")));
    assert!(a.lines().any(|l| l.starts_with("iommu ")));
    // 3b. GX-7f: no copy this boot and none from the carve-out: the probe fetches it itself (under the run lock).
    use crate::probe::probe_copy;
    let ind = || Found { via: Via::Indirect { pos: 0x1FFF_0000 }, bytes: bin.clone() };
    let (got, l) = probe_copy(None, false, || Some((Ok(ind()), 34)));
    assert!(got.is_some() && l.as_deref() == Some("discovery fetch: through MM_INDEX/MM_DATA as gx_bringup does (34 index writes, index restored; no other register written)"), "{l:?}");
    let c = Carve { lines: std::vec![verdict.clone()], discovery: Some((0x8_5FFF_0000, std::vec![0xFF; 8192])) };
    let (a, _) = assemble(head.clone(), c, got, &table, false, &mut state, &iommu);
    assert!(!a.contains("discovery: REFUSED") && a.lines().any(|l| l == "state: cold (firmware state)"), "{a}");
    let (got, l) = probe_copy(None, false, || None);
    assert!(got.is_none() && l.is_some_and(|l| l.starts_with("discovery fetch: skipped (a gx_bringup run holds the PSP")));
    let (got, l) = probe_copy(None, false, || Some((Err(String::from("indirect read: Busy")), 2)));
    assert!(got.is_none() && l.as_deref() == Some("discovery fetch: indirect read failed: indirect read: Busy"));
    // A copy this boot or a carve-out that parsed: no fetch at all.
    let (got, l) = probe_copy(Some(ind()), false, || panic!("fetched with a copy"));
    assert!(got.is_some() && l.is_none());
    let (got, l) = probe_copy(None, true, || panic!("fetched with the carve-out's binary"));
    assert!(got.is_none() && l.is_none());
    // 4. The carve-out gave the binary: used and kept for the boot.
    let c = Carve { lines: std::vec![verdict], discovery: Some((0x8_5FFF_0000, bin)) };
    let (a, keep) = assemble(head, c, None, &table, false, &mut state, &iommu);
    assert!(matches!(keep, Some(Found { via: Via::CarveOut { phys: 0x8_5FFF_0000 }, .. })));
    assert!(a.lines().any(|l| l.starts_with("discovery: ok via carve-out at phys 0x85fff0000")), "{a}");
    assert_eq!(calls, 3);
    let _ = d;
}
