//! In-kernel tests `rb.crashdisk.*` (CRASHDISK). Safe: the slot format and the extent check as the kernel builds
//! them. QemuOnly, with host/tests/rb/45-crashdisk.sh: `forced` panics on purpose in a first QEMU (CONFIG
//! `rb_crashdisk=panic`), and in a second QEMU on the same NVMe image (RAM gone) checks the record read back from
//! the ESP; `rb_crashdisk=hang` checks the record of fch-wdt's `rb.crashdisk.hang`; `rb_crashdisk=off` (no
//! `nvme_boot=`) checks that nothing is armed and panics, the scenario then checks the ESP did not change.

use super::extents::{self, FileFacts};
use super::format::{self, Head, Seen};
use super::show::{self, Source};
use super::State;
use crate::boot;
use alloc::format;
use alloc::string::String;
use ktest::{TestResult, kassert, ktest};

fn format_round_trip() -> TestResult {
    let h = Head { kind: format::KIND_RECORD, slot: 2, seq: 7, flags: format::flags::HANG, boot_id: [9; 16], time_ns: 5_000_000_000, wall_s: 1_763_052_540, cpu: 3, ..Head::default() };
    let body = b"panic on cpu 3 at 5.000000 s: watchdog: test\nlog:\n";
    let mut slot = format::encode(&h, body).ok_or("encode")?;
    let (got, b) = format::decode(&slot).map_err(|e| format!("{e:?}"))?;
    kassert!(got.seq == 7 && got.slot == 2 && got.body_len as usize == body.len() && b == body, "{got:?}");
    kassert!(format::seen(&slot, 2) == Seen::Record { seq: 7 } && format::seen(&slot, 1) == Seen::Damaged);
    let last = slot.len() - 1;
    slot[last] ^= 1;
    kassert!(format::decode(&slot) == Err(format::SlotError::BadChecksum), "a torn body is found");
    kassert!(format::seen(&[0u8; 512], 0) == Seen::Unprepared);
    let seen = [Seen::Empty { tag: 1 }, Seen::Record { seq: 4 }, Seen::Record { seq: 9 }, Seen::Damaged];
    kassert!(format::next_slot(&seen) == (3, 10));
    kassert!(format::next_slot(&[Seen::Record { seq: 3 }, Seen::Empty { tag: 0 }]) == (1, 4));
    kassert!(format::next_slot(&[Seen::Unprepared; 4]) == (0, 1));
    Ok(())
}
ktest!("rb.crashdisk.format", Safe, format_round_trip);

fn extent_check() -> TestResult {
    let runs = [(1000u64, 1024u64), (3000, 1024)];
    let f = FileFacts { runs: &runs, size: 1 << 20, bps: 512, data_start: 900, vol_total: 10_000, part: (559_104, 20_000), vol_offset: 0, foreign: 0 };
    let ext = extents::resolve(&f, 512, (559_104, 20_000), 1 << 20).map_err(String::from)?;
    kassert!(ext.runs() == [(560_104, 1024), (562_104, 1024)]);
    let mut out = [super::Run::default(); 8];
    // Slot 1 of 256 KiB: the second half of the first run.
    let n = ext.map(256 << 10, 1024, 0, &mut out).ok_or("map")?;
    kassert!(n == 1 && out[0].lba == 560_104 + 512 && out[0].blocks == 2);
    // Across the runs.
    let n = ext.map((512 << 10) - 1024, 2048, 0, &mut out).ok_or("map across")?;
    kassert!(n == 2 && out[0].lba == 560_104 + 1022 && out[1].lba == 562_104 && out[1].off == 1024, "{:?}", &out[..n]);
    kassert!(ext.holds(560_104, 1024) && !ext.holds(560_104 + 1023, 2) && !ext.holds(559_104, 1));
    kassert!(ext.map(1 << 20, 512, 0, &mut out).is_none(), "past the file");
    let bad = |f: &FileFacts<'_>, allowed| extents::resolve(f, 512, allowed, 1 << 20).err();
    kassert!(bad(&f, (559_104, 20_001)).is_some(), "another partition than the device's");
    kassert!(bad(&FileFacts { bps: 4096, ..f.clone() }, (559_104, 20_000)).is_some(), "sector size");
    kassert!(bad(&FileFacts { runs: &[(800, 2048)], ..f.clone() }, (559_104, 20_000)).is_some(), "before the data area");
    kassert!(bad(&FileFacts { runs: &[(9000, 2048)], ..f.clone() }, (559_104, 20_000)).is_some(), "past the volume");
    kassert!(bad(&FileFacts { runs: &[(1000, 1024), (1500, 1024)], ..f.clone() }, (559_104, 20_000)).is_some(), "overlap");
    kassert!(bad(&FileFacts { runs: &[(1000, 1000)], ..f.clone() }, (559_104, 20_000)).is_some(), "too short");
    kassert!(bad(&FileFacts { vol_total: 30_000, ..f.clone() }, (559_104, 20_000)).is_some(), "volume larger than its partition");
    kassert!(bad(&FileFacts { foreign: 2, ..f.clone() }, (559_104, 20_000)).is_some(), "a cross-linked chain");
    Ok(())
}
ktest!("rb.crashdisk.extents", Safe, extent_check);

const SEC: u64 = 1_000_000_000;

/// Waits (bounded) until the boot thread decided.
pub fn wait_decided(secs: u64) -> State {
    let end = time::now_ns() + secs * SEC;
    loop {
        let s = super::state();
        if s != State::Waiting || time::now_ns() > end {
            return s;
        }
        sched::sleep_ns(200_000_000);
    }
}

/// The record a forced panic left, if this boot is the one after it.
fn fresh(sign: &str) -> Option<show::Rec> {
    show::newest().filter(|r| !r.reported_before && r.text.contains(sign))
}

fn check_disk_record(r: &show::Rec, hang: bool) -> TestResult {
    kassert!(matches!(r.source, Source::Disk { .. }), "the record came from {} (a cold start has no RAM record)", r.source.name());
    kassert!(super::super::marker().is_some_and(|m| m.0 == super::super::record::MarkerFound::Absent), "the RAM marker survived: not a cold start");
    let t = &r.text;
    kassert!(t.starts_with("panic on cpu "), "the first line is not the panic: {}", r.first_line());
    if !hang {
        kassert!(t.contains("rb_crash_level_three") && t.contains("rb_crash_level_one"), "no backtrace of the forced panic");
    } else {
        kassert!(r.head.flags & format::flags::HANG != 0 && t.contains("watchdog: "), "not marked as a hang: {}", r.first_line());
    }
    for want in [" regs: rip ", "memory: mem.free ", "\nlog:\n"] {
        kassert!(t.contains(want), "the record has no {want:?}");
    }
    kassert!(r.head.flags & format::flags::CUT == 0, "the disk record was cut ({} bytes)", t.len());
    kassert!(r.head.wall_s > 1_700_000_000, "no wall time: {}", r.head.wall_s);
    let proc = vfs::root_ns().read_file("/proc/bitsos/crash", 1 << 20).map_err(|e| format!("/proc/bitsos/crash: errno {e}"))?;
    let proc = String::from_utf8_lossy(&proc);
    let head = proc.lines().next().unwrap_or("");
    kassert!(head.starts_with("new=1 source=disk ") && head.ends_with(" decided=1"), "{head}");
    kassert!(proc.contains("notice=BitsOS ") && proc.contains("Der Bericht ist gesichert."), "no desktop line");
    // crash_info as MCP shows it: the newest, the list, one record by index.
    let info = super::super::tool::info_text(&mcp::Json::Null);
    kassert!(info.contains("disk copy: armed on ") && !info.contains("now RAM only") && info.contains("source: [0] disk slot"), "{info}");
    let list = super::super::tool::info_text(&mcp::Json::parse(r#"{"list":true}"#).map_err(|_| String::from("json"))?);
    kassert!(list.contains("[0] disk slot"), "{list}");
    let one = super::super::tool::info_text(&mcp::Json::parse(r#"{"index":0}"#).map_err(|_| String::from("json"))?);
    kassert!(one.contains(r.first_line()), "{one}");
    // The next panic goes to the slot after this one, with the next number.
    let Source::Disk { slot } = r.source else { return Err("source".into()) };
    match wait_decided(120) {
        State::Armed { slot: s, seq, .. } => kassert!(s == (slot + 1) % format::SLOTS && seq == r.head.seq + 1, "armed at slot {s} seq {seq}"),
        other => return Err(format!("not armed again: {other:?}")),
    }
    klog::event!(Info, "crash", "ktest.disk_record", slot = slot, seq = r.head.seq, bytes = t.len(), wall_s = r.head.wall_s);
    Ok(())
}

fn forced() -> TestResult {
    let mode = boot::config("rb_crashdisk").unwrap_or_default();
    // The disk's records are known once the boot thread decided (it reads them before arming).
    let decided = wait_decided(150);
    match mode.as_str() {
        "panic" => {
            if let Some(r) = fresh("rb.crash forced kind=panic") {
                return check_disk_record(&r, false);
            }
            if !matches!(decided, State::Armed { .. }) {
                return Err(format!("the disk copy is not armed: {decided:?}"));
            }
            klog::event!(Warn, "crash", "ktest.forcing", kind = 0; "a deliberate kernel panic follows (disk copy armed)");
            super::super::ktests::force(0);
            Err("the forced panic returned".into())
        }
        // Boot 1 of the hang: fch-wdt's rb.crashdisk.hang hangs a CPU; boot 2 checks its record here.
        "hang" => match fresh("watchdog: ") {
            Some(r) => check_disk_record(&r, true),
            None => Ok(()),
        },
        "off" => {
            match decided {
                State::Off(why) => kassert!(why.contains("nvme_boot="), "off for another reason: {why}"),
                other => return Err(format!("armed although nvme_boot= is missing: {other:?}")),
            }
            let info = super::super::tool::info_text(&mcp::Json::Null);
            kassert!(info.contains("disk copy: off") && info.contains("RAM only"), "{info}");
            klog::event!(Warn, "crash", "ktest.forcing", kind = 0; "a deliberate kernel panic follows (disk copy off)");
            super::super::ktests::force(0);
            Err("the forced panic returned".into())
        }
        _ => ktest::fail("CONFIG rb_crashdisk=panic|hang|off not set"),
    }
}
ktest!("rb.crashdisk.forced", Run("rb/45-crashdisk"), forced);
