//! The job half's queue of submissions grows with its lock dropped (`hw::jobs::Shared::queue`): the window between the
//! unlock and the relock. `Jobs::submit_with`'s closure runs in that window and is the other thread here: the job thread
//! taking the list, another submitter filling it, a stop. Fake engines only, as `ktests_hw_jobs`.
//!
//! How many entries a list has room for and by how much it grows is `ksync::Spare`'s business: no test here counts on
//! it. Each fills the list until a window opens (`fill`) and judges what the queue does then.

use crate::hw::frame::RingType;
use crate::hw::jobs::{JobCfg, Jobs};
use crate::ktests_hw_jobs::{DATA, Ends, Mem, Cx, give_way, job, kassert_end, rig, sub, write_ib};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use drm::fence::{self, Fence};
use drm::gpu::GpuDevice;
use drm::job::Submission;
use ktest::kassert;
use linux::ab::errno_more::ECANCELED;
use linux::errno::ENODEV;
use vfs::Errno;

const SDMA: RingType = RingType::Sdma;
/// `fill` gives up here: a list with room for this many from the start has no window a test could reach.
const FILL: u32 = 4096;

/// What `fill` found.
struct Met {
    /// Queued when the window opened, in the order pushed.
    before: Vec<Ends>,
    /// The submission that had to wait for its list, and what its submit answered.
    waited: Ends,
    got: Result<(), Errno>,
    /// How often the lock was dropped for it: more than once means the list made first was not enough any more.
    windows: u32,
}

/// Submits `make(1)`, `make(2)` .. until one finds the list full with others queued in it, and runs `window` there,
/// once, with the ends of those queued. The caller keeps the thread from taking the list meanwhile (`no_round`).
fn fill(jobs: &Jobs, make: &dyn Fn(u32) -> (Submission, Ends), window: &mut dyn FnMut(&[Ends])) -> Result<Met, String> {
    let mut before = Vec::new();
    for v in 1..=FILL {
        let (s, waited) = make(v);
        let mut windows = 0u32;
        let got = jobs.submit_with(s, &mut || {
            if !before.is_empty() {
                windows += 1;
                if windows == 1 {
                    window(&before);
                }
            }
        });
        if windows > 0 {
            return Ok(Met { before, waited, got, windows });
        }
        got.map_err(|x| format!("submit {v}: errno {x}"))?;
        before.push(waited);
    }
    Err(format!("{FILL} submissions, and the list never had to grow with one queued"))
}

/// 600 submissions with no round between them: the list grows to many pages, never under its lock (the detector's
/// `heap.atomic_large` would name `Shared::queue`), and every job runs, in the order submitted.
pub(crate) fn grows() -> ktest::TestResult {
    const N: u32 = 600;
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let e = cx.entity(SDMA);
    let mut ends = Vec::new();
    let mut grown = 0u32;
    {
        let _held = jobs.no_round();
        for k in 0..N {
            let (j, en) = job(e.clone(), vec![m.ib(&write_ib(SDMA, DATA, k + 1), 0)]);
            jobs.submit_with(sub(g, &m, vec![j], Vec::new(), None), &mut || grown += 1).map_err(|x| format!("submit {k}: errno {x}"))?;
            ends.push(en);
        }
    }
    kassert!(N as usize * core::mem::size_of::<Submission>() >= 2 * 4096, "the list stayed below two pages");
    // It began empty, so it grew, each time with the lock dropped (the closure ran); not once for every push.
    kassert!(grown >= 1 && grown < N, "the list was made {grown} times for {N} submissions");
    g.kick();
    for (k, en) in ends.iter().enumerate() {
        kassert_end(g, en, SDMA)?;
        kassert!(fence::status(&en.finished) == 1, "job {k}: status {}", fence::status(&en.finished));
    }
    kassert!(m.gtt_u32(0) == N, "the last write reads {}: a job was lost or ran out of order", m.gtt_u32(0));
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_queue_grows", Safe, grows);

/// The window: the thread takes the list (the larger one made for the old list goes into the fresh, empty one), and
/// another submitter fills it past what was made (it is made again). Nothing is lost, the order is the order of the pushes.
pub(crate) fn window() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let e = cx.entity(SDMA);
    let one = |v: u32| {
        let (j, en) = job(e.clone(), vec![m.ib(&write_ib(SDMA, DATA, v), 0)]);
        (sub(g, &m, vec![j], Vec::new(), None), en)
    };
    let errno = |x| format!("submit: errno {x}");

    // The thread's round in the window: it takes the list and runs what was queued.
    let mut held = Some(jobs.no_round());
    let mut taken = Ok(());
    let met = fill(&jobs, &one, &mut |before| {
        drop(held.take());
        g.kick();
        taken = before.iter().try_for_each(|en| kassert_end(g, en, SDMA));
    })?;
    taken?;
    met.got.map_err(errno)?;
    let queued = met.before.len() as u32;
    kassert!(m.gtt_u32(0) == queued, "{queued} were queued before the window, the last write reads {}", m.gtt_u32(0));
    g.kick();
    kassert_end(g, &met.waited, SDMA)?;
    let st = fence::status(&met.waited.finished);
    kassert!(st == 1 && m.gtt_u32(0) == queued + 1, "the job pushed after the thread's take: status {st}, reads {}", m.gtt_u32(0));

    // Another submitter in the window: the list grows under it, and it goes on until the list is full again. The list
    // made for the waiting push is as large as the one the others got, so it is too small now and is made again.
    let mut ends = Vec::new();
    let mut pushed: Result<(), String> = Ok(());
    let met = {
        let _held = jobs.no_round();
        fill(&jobs, &one, &mut |_| {
            for k in 0..FILL {
                let (s, en) = one(FILL + 1 + k);
                if let Err(x) = jobs.submit(s) {
                    pushed = Err(errno(x));
                    break;
                }
                ends.push(en);
                if jobs.queue_room() == 0 {
                    break;
                }
            }
        })?
    };
    pushed?;
    met.got.map_err(errno)?;
    kassert!(met.windows >= 2, "the list was made {} time(s) for the waiting push, with {} others pushed before it", met.windows, ends.len());
    g.kick();
    for en in met.before.iter().chain(&ends).chain([&met.waited]) {
        kassert_end(g, en, SDMA)?;
        kassert!(fence::status(&en.finished) == 1, "status {}", fence::status(&en.finished));
    }
    let last = met.before.len() as u32 + 1;
    kassert!(m.gtt_u32(0) == last, "the last write reads {}, not {last}: the push that waited for the list must come last", m.gtt_u32(0));
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_queue_window", Safe, window);

/// The window: the device is stopped. The submission is refused and ends, as one that came after the stop: pushed after
/// the thread's last drain it would never end.
pub(crate) fn closing() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let never = Fence::new(fence::context_alloc(1), 1, "test", "never");
    let one = |v: u32| {
        let (j, en) = job(cx.entity(SDMA), vec![m.ib(&write_ib(SDMA, DATA, v), 0)]);
        (sub(g, &m, vec![j], vec![never.clone()], None), en)
    };
    let mut held = Some(jobs.no_round());
    let met = fill(&jobs, &one, &mut |_| {
        drop(held.take());
        jobs.leave();
        give_way(g, 1_000_000);
    })?;
    let st = fence::status(&met.waited.finished);
    kassert!(met.got == Err(ENODEV) && st == -(ENODEV as i32), "a submission met by the stop: {:?}, status {st}", met.got);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.is_empty(), "nothing ran, but {} are held", held.len());
    for en in &met.before {
        kassert!(fence::status(&en.finished) == -(ECANCELED as i32), "a job queued before the stop: status {}", fence::status(&en.finished));
    }
    kassert!(m.gtt_u32(0) == 0, "a job ran: the memory reads {}", m.gtt_u32(0));
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.job_queue_closing", Safe, closing);
