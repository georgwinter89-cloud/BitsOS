//! IN-8: which late frames warn, and the per-program summary windows (pure state, the tests' own clock).

use crate::latency::program::{COMM_LEN, Programs, Reader, WINDOW_NS, fine_parts};
use crate::latency::{self, BUDGET_NS, PATHS, Path, STALE_NS, Sample, Verdict, classify};
use alloc::vec::Vec;

const MS: u64 = 1_000_000;

fn reader(pid: u32, name: &[u8]) -> Reader {
    let mut comm = [0u8; COMM_LEN];
    comm[..name.len()].copy_from_slice(name);
    Reader { pid, comm }
}

#[test]
fn only_bitsos_paths_warn() {
    for p in PATHS {
        assert_eq!(classify(p, BUDGET_NS), Verdict::InBudget, "{} at the budget", p.name());
        assert_eq!(classify(p, 0), Verdict::InBudget);
        assert_eq!(classify(p, STALE_NS + 1), Verdict::Stale, "{} stale", p.name());
        let late = classify(p, BUDGET_NS + 1);
        match p {
            Path::Console | Path::Pointer | Path::PointerOverProgram => assert_eq!(late, Verdict::Late, "{}", p.name()),
            Path::Program => assert_eq!(late, Verdict::ProgramLate),
        }
        assert_eq!(p.owned(), p != Path::Program);
    }
    // The journal of 24./25.10.: 223 and 316 ms in a video are a program's drawing, a 47 ms echo is BitsOS's.
    assert_eq!(classify(Path::Program, 316 * MS), Verdict::ProgramLate);
    assert_eq!(classify(Path::Console, 47 * MS), Verdict::Late);
    assert_eq!(classify(Path::Program, STALE_NS), Verdict::ProgramLate, "1 s is still counted");
}

#[test]
fn fine_buckets_hold_the_median_within_a_sixteenth() {
    for us in (0..20_000u64).chain((20_000..=1_000_000).step_by(997)) {
        let (i, mid) = fine_parts(us);
        assert!(i < crate::latency::program::FINE, "{us} µs -> bucket {i}");
        assert!(mid.abs_diff(us) * 16 <= us.max(16), "{us} µs -> mid {mid}");
    }
    let mut last = 0;
    for us in 0..=1_000_000u64 {
        let i = fine_parts(us).0;
        assert!(i >= last, "buckets grow with the time");
        last = i;
    }
}

#[test]
fn a_window_sums_up_one_program() {
    let mut p = Programs::new();
    let brave = reader(41, b"brave");
    let other = reader(42, b"mpv");
    let t0 = 5 * WINDOW_NS;
    // A video: most frames about 24 ms, some fast, one at 316 ms.
    let mut times: Vec<u64> = Vec::new();
    for i in 0..30 {
        times.push(24 * MS + i * 10_000);
    }
    times.extend([3 * MS, 5 * MS, 316 * MS, 2 * STALE_NS]);
    let mut grown = 0;
    for (i, &t) in times.iter().enumerate() {
        let now = t0 + i as u64 * 16 * MS;
        while !p.add(brave, t, now) {
            let more = Vec::with_capacity(p.room_wanted());
            drop(p.grow(more));
            grown += 1;
        }
    }
    while !p.add(other, 2 * MS, t0) {
        drop(p.grow(Vec::with_capacity(p.room_wanted())));
        grown += 1;
    }
    assert!(grown >= 1, "the first window needed room");
    assert_eq!(p.windows().len(), 2);
    let w = p.windows().iter().find(|w| w.who == brave).unwrap();
    assert_eq!(w.who.name(), "brave");
    assert_eq!(w.start_ns, t0, "the window opens at the program's first frame");
    assert_eq!(w.frames, 33, "the stale frame stays out");
    assert_eq!(w.late, 31, "30 at about 24 ms and the 316 ms one");
    assert_eq!(w.worst_ns, 316 * MS);
    let m = w.median_ns();
    assert!(m.abs_diff(24 * MS) * 16 <= 24 * MS, "median {m} ns, want about 24 ms");
    // Not due before a minute; then each window closes once.
    assert!(p.take_due(t0 + WINDOW_NS - 1).is_none());
    let mut closed: Vec<u32> = Vec::new();
    while let Some(w) = p.take_due(t0 + WINDOW_NS) {
        closed.push(w.who.pid);
    }
    closed.sort();
    assert_eq!(closed, [41, 42]);
    assert!(p.windows().is_empty());
    // The next frame opens a new window.
    assert!(p.add(brave, 30 * MS, t0 + WINDOW_NS) || { drop(p.grow(Vec::with_capacity(p.room_wanted()))); p.add(brave, 30 * MS, t0 + WINDOW_NS) });
    let w = &p.windows()[0];
    assert_eq!((w.frames, w.late, w.start_ns), (1, 1, t0 + WINDOW_NS));
    assert!(p.take_due(t0 + 2 * WINDOW_NS - 1).is_none(), "at most one summary a minute");
    drop(p.clear());
    assert!(p.windows().is_empty());
}

#[test]
fn a_program_is_its_pid_and_comm() {
    let mut p = Programs::new();
    drop(p.grow(Vec::with_capacity(4)));
    assert!(p.add(reader(9, b"a"), 30 * MS, 0));
    assert!(p.add(reader(9, b"b"), 30 * MS, 0), "a reused pid with another comm is another program");
    assert_eq!(p.windows().len(), 2);
    assert!(p.grow(Vec::with_capacity(1)).capacity() == 1, "a smaller vector is handed back untouched");
    let mut odd = [0xffu8; COMM_LEN];
    odd[..2].copy_from_slice(b"ok");
    assert_eq!(Reader { pid: 1, comm: odd }.name(), "ok", "bytes that are no text are left out");
    assert_eq!(Reader::UNKNOWN.name(), "");
    let empty = Programs::new();
    assert!(empty.windows().is_empty());
}

/// A frame of `path` that showed its input `ms` after it arrived (and `us` µs more).
fn frame(path: Path, ms: u64, us: u64) -> Sample {
    Sample { path, arrival_ns: 1_000, picked_ns: 2_000, start_ns: 1_000 + ms * MS + us * 1000, done_ns: 2_000 + ms * MS + us * 1000 }
}

#[test]
fn record_warns_owned_paths_and_sums_up_programs() {
    let _serial = crate::tests_in6::latency_serial();
    latency::reset();
    let brave = reader(51, b"brave");
    let quiet = reader(52, b"clock");
    // Late program frames count in the path's numbers but leave the warning's count alone (the host clock is 0).
    for i in 0..5 {
        latency::record_as(frame(Path::Program, 24, i), brave);
    }
    latency::record_as(frame(Path::Program, 5, 0), quiet);
    let (paths, _) = latency::report();
    let p = &paths[Path::Program as usize].1;
    assert_eq!((p.frames, p.over_budget), (6, 5), "the measurement is kept");
    assert_eq!(latency::late_owned(), 0, "a program's drawing is no warning");
    // BitsOS's own late frame warns (the 1, 2, 4, ... count).
    latency::record(frame(Path::Console, 47, 0));
    assert_eq!(latency::late_owned(), 1);
    latency::record(frame(Path::PointerOverProgram, 3, 0));
    assert_eq!(latency::late_owned(), 1, "in budget: no count");
    let (open, summaries) = latency::programs();
    assert_eq!((open.len(), summaries), (2, 0), "windows stay open within the minute");
    latency::flush_programs(WINDOW_NS - 1);
    assert_eq!(latency::programs().0.len(), 2, "not due before a minute");
    // A minute on: brave's window had late frames (one summary), the quiet program's had none (no summary).
    latency::flush_programs(WINDOW_NS);
    let (open, summaries) = latency::programs();
    assert!(open.is_empty(), "both windows closed");
    assert_eq!(summaries, 1, "one summary per closed window with late frames");
    // Again a minute of frames all within the budget: nothing to sum up.
    for i in 0..3 {
        latency::record_as(frame(Path::Program, 4, i), brave);
    }
    latency::flush_programs(2 * WINDOW_NS);
    let (open, summaries) = latency::programs();
    assert!(open.is_empty());
    assert_eq!(summaries, 1, "a window without late frames logs nothing");
    latency::reset();
    assert_eq!((latency::late_owned(), latency::programs().1), (0, 0), "reset starts the counts again");
}

#[test]
fn a_read_names_its_reader_once_a_frame() {
    let _serial = crate::tests_in6::latency_serial();
    latency::reset();
    let mut asked = 0;
    latency::program_read(0, 5, || { asked += 1; reader(1, b"none") });
    assert_eq!(asked, 0, "an event without a time opens nothing");
    latency::program_read(1_000, 2_000, || { asked += 1; reader(61, b"first") });
    latency::program_read(1_500, 2_500, || { asked += 1; reader(62, b"second") });
    assert_eq!(asked, 1, "the reader is asked only when its read opens the slot");
    latency::program_frame(40_000_000, 41_000_000);
    latency::program_frame(50_000_000, 51_000_000);
    let (open, _) = latency::programs();
    assert_eq!(open.len(), 1, "one frame, charged to the first reader; a frame without input records nothing");
    assert_eq!((open[0].who.pid, open[0].who.name(), open[0].frames), (61, "first", 1));
    let (paths, recent) = latency::report();
    assert_eq!(paths[Path::Program as usize].1.frames, 1);
    let s = recent.last().unwrap();
    assert_eq!((s.arrival_ns, s.picked_ns, s.start_ns), (1_000, 2_000, 40_000_000), "arrival and pick stay together");
    latency::reset();
}
