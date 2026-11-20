//! The boot side of the disk copy (CRASHDISK): a thread waits for /boot, reads the slots the boots before left
//! (also when /boot is read-only), and only when /boot on the ESP `nvme_boot=` names is writable makes the file,
//! prepares its empty slots, resolves and checks its blocks, reads them back through both paths and arms the
//! panic writer. Then it samples the memory counters for the record every few seconds.

use super::extents::{self, FileFacts};
use super::format::{self, Head, Seen};
use super::show::{self, Rec, Source};
use super::{Armed, State, ARMED, PROVIDER, set_state};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const MOUNT_WAIT_MS: u64 = 180_000;
const MEM_PERIOD_NS: u64 = 5_000_000_000;
const AREA_BYTES: u64 = (format::SLOTS * format::SLOT_BYTES) as u64;

pub(super) fn start() {
    if sched::spawn("crash.disk", sched::Class::Normal, run).is_err() {
        set_state(State::Failed(String::from("its thread could not start")));
        klog::event!(Warn, "crash", "disk.no_thread"; "the crash record stays in RAM only");
    }
}

fn run() {
    super::sample_memory();
    let st = setup();
    match &st {
        State::Armed { disk, slot, seq, runs, first_lba } => {
            klog::event!(Info, "crash", "disk.armed", disk = disk.as_str(), slot = *slot, seq = *seq, runs = *runs, first_lba = *first_lba;
                "a panic writes its record to \\BITSOS2\\CRASH\\SLOTS.BIN on the boot ESP")
        }
        State::Off(why) => klog::event!(Info, "crash", "disk.off", why = why.as_str(); "the crash record is kept in RAM only"),
        State::Failed(why) => klog::event!(Warn, "crash", "disk.failed", why = why.as_str(); "the crash record is kept in RAM only"),
        State::Waiting => {}
    }
    set_state(st);
    show::report_new();
    let mut said = false;
    loop {
        sched::sleep_ns(MEM_PERIOD_NS);
        super::sample_memory();
        // The writer refuses once the boot window closed or changed (crash_info says so too): once in the log.
        if !said && let Some(Err(why)) = super::armed().map(|a| a.dev.ready()) {
            said = true;
            klog::event!(Warn, "crash", "disk.disarmed", why = why; "the boot partition can no longer be written: the next crash record stays in RAM only");
        }
    }
}

fn boot_id() -> [u8; 16] {
    super::super::my_boot_id()
}

fn area<R>(f: impl FnOnce(&fs_fat::crash_area::Area<'_>) -> Result<R, i64>) -> Result<R, String> {
    fs_fat::stick::with_crash_area(f).map_err(|e| format!("no boot volume (errno {e})")).and_then(|r| r.map_err(|e| format!("errno {e}")))
}

/// The boot right before this one, as far as this boot knows without a write: its id from the RAM marker (warm
/// reset only) and this boot's start on the wall clock.
fn before_me() -> (Option<[u8; 16]>, Option<i64>) {
    use super::super::record::MarkerFound;
    let prev = match super::super::marker() {
        Some((MarkerFound::Kept, Some(m))) => Some(m.boot_id),
        _ => None,
    };
    let start = time::wall::realtime_ns().map(|n| (n / 1_000_000_000) as i64 - (time::now_ns() / 1_000_000_000) as i64);
    (prev, start)
}

/// Reads every slot that holds a record; marks the unreported ones reported when `mark` (/boot writable), else
/// decides "new" without a write (`show::new_unmarked`).
fn read_records(bs: usize, mark: bool) -> Vec<Rec> {
    let (prev, start) = before_me();
    let mut out = Vec::new();
    let mut buf = vec![0u8; format::SLOT_BYTES];
    for i in 0..format::SLOTS {
        let off = (i * format::SLOT_BYTES) as u64;
        match area(|a| a.read_at(off, &mut buf)) {
            Ok(n) if n == buf.len() => {}
            _ => continue,
        }
        if !matches!(format::seen(&buf, i), Seen::Record { .. }) {
            continue;
        }
        let (h, body) = match format::decode(&buf) {
            Ok(x) => x,
            Err(e) => {
                klog::event!(Warn, "crash", "disk.slot_damaged", slot = i, error = format!("{e:?}"); "a torn or damaged record (a panic during its write?)");
                continue;
            }
        };
        let flagged = h.flags & super::super::record::flags::REPORTED != 0;
        let reported = flagged || (!mark && !show::new_unmarked(&h.boot_id, h.wall_s, prev.as_ref(), start));
        if !flagged && mark {
            // Only the header's flag changes: the first block again, sealed over the same body.
            let marked = Head { flags: h.flags | super::super::record::flags::REPORTED, ..h };
            let mut f = marked.sum_start();
            f.update(body);
            let mut first = buf[..bs.max(format::HEADER)].to_vec();
            first[..format::HEADER].copy_from_slice(&marked.sealed(f.finish()));
            if let Err(e) = area(|a| a.write_at(off, &first)) {
                klog::event!(Warn, "crash", "disk.mark_failed", slot = i, error = e.as_str());
            }
        }
        let text = String::from_utf8_lossy(body).into_owned();
        let head = Head { flags: h.flags & !super::super::record::flags::REPORTED, ..h };
        out.push(Rec { source: Source::Disk { slot: i }, head, text, reported_before: reported });
    }
    out
}

fn setup() -> State {
    if fs_fat::stick::wait_mounted(MOUNT_WAIT_MS).is_none() {
        return State::Off(String::from("no boot volume came within 180 s"));
    }
    if !fs_fat::stick::on_nvme() {
        return State::Off(String::from("the boot volume is the USB stick, not an ESP on the NVMe"));
    }
    let rw = fs_fat::stick::last_reason();
    let bs = fs_fat::stick::volume().map_or(512, |v| v.geo.bps);
    show::set_disk(read_records(bs, rw.is_none()));
    if let Some(why) = rw {
        return State::Off(format!("/boot on the NVMe is read-only ({why}): nvme_boot= does not let BitsOS write the boot partition"));
    }
    match arm() {
        Ok(s) => s,
        Err(why) => State::Failed(why),
    }
}

fn arm() -> Result<State, String> {
    let p = PROVIDER.get().ok_or("no NVMe panic writer in this kernel")?;
    let whole = fs_fat::stick::whole_disk().ok_or("the boot volume's disk is gone")?;
    let dev = p(whole.name(), format::SLOT_BYTES).map_err(|e| format!("panic writer: {e}"))?;
    let bs = dev.block_size();
    if bs > format::SLOT_BYTES || !format::SLOT_BYTES.is_multiple_of(bs) || bs < format::HEADER {
        return Err(format!("block size {bs} does not suit the slots"));
    }
    area(|a| a.ensure(AREA_BYTES)).map_err(|e| format!("making \\BITSOS2\\CRASH\\SLOTS.BIN: {e}"))?;
    let me = boot_id();
    let mut seen = Vec::new();
    let mut block = vec![0u8; bs];
    for i in 0..format::SLOTS {
        let off = (i * format::SLOT_BYTES) as u64;
        area(|a| a.read_at(off, &mut block)).map_err(|e| format!("reading slot {i}: {e}"))?;
        let mut s = format::seen(&block, i);
        if matches!(s, Seen::Unprepared | Seen::Damaged) {
            let tag = format::prep_tag(&me, i);
            let h = Head { kind: format::KIND_EMPTY, slot: i as u16, boot_id: me, tag, ..Head::default() };
            let mut b = format::encode(&h, &[]).ok_or("empty slot")?;
            b.resize(bs, 0);
            area(|a| a.write_at(off, &b)).map_err(|e| format!("preparing slot {i}: {e}"))?;
            s = Seen::Empty { tag };
        }
        seen.push(s);
    }
    let layout = area(|a| a.layout()).map_err(|e| format!("the area's place: {e}"))?;
    // The block layer's view of the ESP (its boot window), checked against the controller's (`dev.allowed`).
    let part = whole.boot_window().ok_or("the block layer has no open boot window on the disk")?;
    let facts = FileFacts {
        runs: &layout.runs,
        size: layout.size,
        bps: layout.bps,
        data_start: layout.data_start,
        vol_total: layout.total,
        part,
        vol_offset: fs_fat::stick::start_lba(),
        foreign: layout.foreign,
    };
    let ext = extents::resolve(&facts, bs, dev.allowed(), AREA_BYTES).map_err(String::from)?;
    // Read back through both paths: the file system's view of each slot's first block and the driver's view of
    // the LBA the panic would write must be the same prepared bytes.
    let mut raw = vec![0u8; bs];
    for (i, s) in seen.iter().enumerate() {
        let off = (i * format::SLOT_BYTES) as u64;
        let lba = ext.lba_of(off).ok_or("a slot lies past the runs")?;
        area(|a| a.read_at(off, &mut block)).map_err(|e| format!("reading slot {i}: {e}"))?;
        dev.read(lba, &mut raw).map_err(|e| format!("reading LBA {lba}: {e}"))?;
        if block != raw || format::seen(&raw, i) != *s {
            return Err(format!("slot {i}: LBA {lba} does not hold what the file holds: the area's place is not sure"));
        }
    }
    let (slot, seq) = format::next_slot(&seen);
    let tag = match seen[slot] {
        Seen::Empty { tag } => tag,
        _ => format::prep_tag(&me, slot),
    };
    let wall_ns = time::wall::realtime_ns();
    let st = State::Armed { disk: String::from(dev.name()), slot, seq, runs: ext.runs().len(), first_lba: ext.runs()[0].0 };
    ARMED.get_or_init(|| Armed { dev, ext, slot, seq, tag, wall_s: wall_ns.map_or(0, |n| (n / 1_000_000_000) as i64), up_ns: time::now_ns() });
    Ok(st)
}
