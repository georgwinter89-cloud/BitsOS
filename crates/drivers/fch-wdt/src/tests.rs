use crate::config;
use crate::dog::{CpuSource, Dog};
use crate::live::{self, Live, Policy, Sample, Track};
use crate::regs::{self, ChipError, Fake, Regs};
use crate::system::State;
use crate::tool::hang_refusal;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const SEC: u64 = 1_000_000_000;

// ---------------------------------------------------------------------------------------------------------
// Chip sequence

#[test]
fn enable_sets_decode_and_seconds() {
    let f = Fake::new(false, false, None);
    f.pm_write8(3, 0x0F);
    assert_eq!(regs::enable(&f), Ok(()));
    assert_eq!(f.pm(0) & regs::PM0_DECODE, regs::PM0_DECODE);
    assert_eq!(f.pm(3), 0x03, "disable bits cleared, seconds set");
}

#[test]
fn enable_reports_refused_decode_and_disabled() {
    let f = Fake::new(false, false, None);
    f.refuse_decode();
    assert_eq!(regs::enable(&f), Err(ChipError::NoDecode));
    let f = Fake::new(false, true, None);
    assert_eq!(regs::enable(&f), Err(ChipError::Disabled));
    assert_eq!(regs::arm(&f, 30), Err(ChipError::Disabled));
    assert!(!f.running());
}

#[test]
fn fired_is_read_and_cleared() {
    let f = Fake::new(true, false, None);
    let st = regs::read_and_clear_fired(&f);
    assert!(st.fired && !st.was_running);
    assert_eq!(f.ctl() & regs::FIRED, 0, "cleared");
    assert!(!regs::read_and_clear_fired(&f).fired, "a second look sees it cleared");
    let f = Fake::new(false, false, None);
    let w = f.writes();
    assert!(!regs::read_and_clear_fired(&f).fired);
    assert_eq!(f.writes(), w, "nothing written when FIRED is clear");
}

#[test]
fn arm_kick_stop() {
    let f = Fake::new(true, false, None);
    regs::arm(&f, 30).unwrap();
    assert!(f.running());
    assert_eq!(f.count(), 30);
    assert_eq!(f.ctl() & (regs::POWEROFF | regs::FIRED), 0, "reset action, FIRED cleared");
    assert_eq!(f.kicks(), 1, "armed = kicked once");
    regs::kick(&f);
    assert_eq!(f.kicks(), 2);
    regs::set_count_and_kick(&f, 240);
    assert_eq!((f.count(), f.kicks()), (240, 3));
    regs::stop(&f);
    assert!(!f.running());
    regs::kick(&f);
    assert_eq!(f.kicks(), 3, "a kick of a stopped chip is none");
    regs::arm(&f, 0x1_0000).unwrap();
    assert_eq!(f.count(), regs::MAX_SECONDS, "16-bit count");
}

static CLOCK: AtomicU64 = AtomicU64::new(1);
fn clock() -> u64 {
    CLOCK.load(Ordering::SeqCst)
}

#[test]
fn fake_expires_without_kicks() {
    CLOCK.store(1, Ordering::SeqCst);
    let f = Fake::new(false, false, Some(clock));
    regs::arm(&f, 30).unwrap();
    CLOCK.store(1 + 29 * SEC, Ordering::SeqCst);
    assert!(!f.would_fire(clock()));
    regs::kick(&f);
    CLOCK.store(1 + 58 * SEC, Ordering::SeqCst);
    assert!(!f.would_fire(clock()));
    CLOCK.store(1 + 60 * SEC, Ordering::SeqCst);
    assert!(f.would_fire(clock()));
}

// ---------------------------------------------------------------------------------------------------------
// Liveness

fn pol() -> Policy {
    Policy::new(10 * SEC)
}

fn s(hb: u64, idle: bool, irq: bool) -> Sample {
    Sample { online: true, heartbeat: hb, idle_now: idle, idle_irq_on: irq }
}

#[test]
fn moving_heartbeat_is_alive() {
    let mut t = Track::default();
    for i in 0..100u64 {
        let (n, l, _) = live::judge(t, s(i, false, false), false, 1 + i * SEC, pol());
        assert_eq!(l, Live::Moved);
        t = n;
    }
}

#[test]
fn idle_with_interrupts_on_never_stalls_when_it_answers() {
    // A tickless idle CPU: the heartbeat stands still until a probe moves it.
    let mut t = Track::default();
    let mut hb = 7;
    for i in 0..600u64 {
        let now = 1 + i * SEC;
        let (n, l, probe) = live::judge(t, s(hb, true, true), false, now, pol());
        assert!(l.alive(), "second {i}: {l:?}");
        t = n;
        if probe {
            t = live::note_probe(t, true, now);
            hb += 1; // its reschedule IPI bumps the heartbeat by the next round
        }
    }
}

#[test]
fn idle_without_probe_vector_counts_as_alive() {
    let mut t = Track::default();
    for i in 0..600u64 {
        let now = 1 + i * SEC;
        let (n, l, probe) = live::judge(t, s(3, true, true), false, now, pol());
        t = if probe { live::note_probe(n, false, now) } else { n };
        assert!(l.alive());
        if i > 10 {
            assert_eq!(l, Live::Idle);
        }
    }
}

#[test]
fn spinning_with_interrupts_off_stalls() {
    let mut t = Track::default();
    let mut first = None;
    for i in 0..30u64 {
        let now = 1 + i * SEC;
        let (n, l, probe) = live::judge(t, s(5, false, false), false, now, pol());
        t = if probe { live::note_probe(n, true, now) } else { n };
        if !l.alive() && first.is_none() {
            first = Some(i);
            assert_eq!(l, Live::Stalled { idle: false });
        }
    }
    assert_eq!(first, Some(11), "stalled once the heartbeat is older than the 10 s window");
}

#[test]
fn idle_claim_without_answer_stalls() {
    // Went idle, then hung inside an interrupt handler: it still claims idle but never answers.
    let mut t = Track::default();
    let mut verdicts = Vec::new();
    for i in 0..40u64 {
        let now = 1 + i * SEC;
        let (n, l, probe) = live::judge(t, s(9, true, true), false, now, pol());
        t = if probe { live::note_probe(n, true, now) } else { n };
        verdicts.push(l);
    }
    assert!(verdicts[..12].iter().all(|l| l.alive()));
    assert_eq!(*verdicts.last().unwrap(), Live::Stalled { idle: true });
}

#[test]
fn offline_and_checking_cpus() {
    let (_, l, p) = live::judge(Track::default(), Sample::default(), false, 5, pol());
    assert_eq!((l, p), (Live::Offline, false));
    let (_, l, p) = live::judge(Track { seen: true, moved_at: 1, ..Track::default() }, s(1, false, false), true, 100 * SEC, pol());
    assert_eq!((l, p), (Live::Checking, false));
    assert!(live::all_alive([Live::Offline, Live::Checking, Live::Idle, Live::Moved]));
    assert!(!live::all_alive([Live::Moved, Live::Stalled { idle: true }]));
    for c in 0..6 {
        assert_eq!(Live::from_code(c).code(), c);
    }
}

// ---------------------------------------------------------------------------------------------------------
// The dog with a fake CPU table

#[derive(Default)]
struct Table {
    cpus: Mutex<Vec<Sample>>,
    probes: Mutex<Vec<usize>>,
    /// Probes move the heartbeat of these CPUs.
    answer: Mutex<Vec<bool>>,
}

struct FakeCpus(Arc<Table>);

impl CpuSource for FakeCpus {
    fn count(&self) -> usize {
        self.0.cpus.lock().unwrap().len()
    }
    fn this_cpu(&self) -> usize {
        0
    }
    fn sample(&self, cpu: usize) -> Sample {
        self.0.cpus.lock().unwrap()[cpu]
    }
    fn probe(&self, cpu: usize) -> bool {
        self.0.probes.lock().unwrap().push(cpu);
        if self.0.answer.lock().unwrap()[cpu] {
            self.0.cpus.lock().unwrap()[cpu].heartbeat += 1;
        }
        true
    }
}

static DOG_CLOCK: AtomicU64 = AtomicU64::new(1);
fn dog_clock() -> u64 {
    DOG_CLOCK.load(Ordering::SeqCst)
}

#[test]
fn dog_kicks_only_when_all_alive() {
    DOG_CLOCK.store(1, Ordering::SeqCst);
    let t = Arc::new(Table::default());
    *t.cpus.lock().unwrap() = vec![s(1, false, true), s(1, true, true), s(1, true, true), s(1, false, false)];
    *t.answer.lock().unwrap() = vec![true, true, true, true];
    let fake = Arc::new(Fake::new(true, false, Some(dog_clock)));
    let d = Dog::new(Box::new(fake.clone()), Box::new(FakeCpus(t.clone())), dog_clock, pol(), 30);
    // Not armed: checks, no kicks.
    d.check();
    assert_eq!(fake.kicks(), 0);
    d.arm().unwrap();
    assert_eq!(fake.ctl() & regs::FIRED, 0);
    let k0 = fake.kicks();
    // 10 minutes: CPU 3 busy with interrupts on (answers probes), 1 and 2 idle and halted.
    for i in 1..=600u64 {
        DOG_CLOCK.store(1 + i * SEC, Ordering::SeqCst);
        let r = d.check();
        assert!(r.kicked && r.stalled == 0, "second {i}: {r:?}");
        assert!(!fake.would_fire(dog_clock()));
    }
    assert_eq!(fake.kicks(), k0 + 600);
    assert!(!d.take_news());
    // CPU 3 now spins with interrupts off: probes go unanswered.
    t.answer.lock().unwrap()[3] = false;
    let mut stalled_at = None;
    for i in 601..=700u64 {
        DOG_CLOCK.store(1 + i * SEC, Ordering::SeqCst);
        let r = d.check();
        if r.stalled > 0 && stalled_at.is_none() {
            stalled_at = Some(i);
            assert!(!r.kicked);
            assert!(d.take_news());
            assert_eq!(d.view(3).unwrap().live, Live::Stalled { idle: false });
            assert!(d.view(1).unwrap().live.alive() && d.view(2).unwrap().live.alive(), "idle CPUs stay alive");
        }
        if fake.would_fire(dog_clock()) {
            break;
        }
    }
    let at = stalled_at.expect("stall seen");
    assert!(at <= 612, "stalled within window + probe time: {at}");
    assert!(fake.would_fire(dog_clock()), "no kicks: the chip resets");
    assert!(d.withheld() > 0);
    // It recovers: kicks resume.
    t.answer.lock().unwrap()[3] = true;
    t.cpus.lock().unwrap()[3].heartbeat += 1;
    let r = d.check();
    assert!(r.kicked && d.take_news());
    // Grace before a reboot: armed, count 240, no more kicks.
    assert!(d.grace(240), "a running chip takes the grace count");
    let k = fake.kicks();
    d.check();
    assert_eq!((fake.count(), fake.kicks(), fake.running()), (240, k, true));
    d.stop();
    assert!(!fake.running());
    // RB-13 fixer: a stopped chip gets no count, and grace says so (the panic record must not claim a safety net).
    assert!(!d.grace(120), "grace on a stopped chip wrote nothing");
    assert!(!fake.running() && fake.count() != 120);
}

/// A block whose control read is slow: widens the window between a kick's read and its write.
struct Slow(Arc<Fake>);

impl Regs for Slow {
    fn pm_read8(&self, off: usize) -> u8 {
        self.0.pm_read8(off)
    }
    fn pm_write8(&self, off: usize, v: u8) {
        self.0.pm_write8(off, v)
    }
    fn ctl_read(&self) -> u32 {
        let v = self.0.ctl_read();
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
        v
    }
    fn ctl_write(&self, v: u32) {
        self.0.ctl_write(v)
    }
    fn count_read(&self) -> u32 {
        self.0.count_read()
    }
    fn count_write(&self, v: u32) {
        self.0.count_write(v)
    }
}

#[test]
fn stop_and_grace_win_over_a_running_kick() {
    // The checker's finding: a kick reads the control register with RUN set, the stop writes RUN clear, the
    // late kick writes RUN back. Stop and grace now take the kicker's flag, so after they return no kick lands.
    let t = Arc::new(Table::default());
    *t.cpus.lock().unwrap() = vec![s(1, false, true)];
    *t.answer.lock().unwrap() = vec![true];
    for round in 0..200 {
        let fake = Arc::new(Fake::new(false, false, None));
        let d = Arc::new(Dog::new(Box::new(Slow(fake.clone())), Box::new(FakeCpus(t.clone())), dog_clock, pol(), 30));
        d.arm().unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let kicker = {
            let (d, done) = (d.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    d.check();
                }
            })
        };
        while d.checks() < 2 {
            std::thread::yield_now();
        }
        if round % 2 == 0 {
            d.stop();
            assert!(!fake.running(), "round {round}: stopped");
        } else {
            d.grace(240);
            assert_eq!(fake.count(), 240, "round {round}: grace count kept");
        }
        let k = fake.kicks();
        for _ in 0..50 {
            std::thread::yield_now();
        }
        done.store(true, Ordering::Release);
        kicker.join().unwrap();
        if round % 2 == 0 {
            assert!(!fake.running(), "round {round}: no late kick set RUN again");
        } else {
            assert_eq!((fake.count(), fake.kicks()), (240, k), "round {round}: no kicks after grace");
        }
    }
}

/// A block where nothing answers.
struct Absent;

impl Regs for Absent {
    fn pm_read8(&self, _: usize) -> u8 {
        0xFF
    }
    fn pm_write8(&self, _: usize, _: u8) {}
    fn ctl_read(&self) -> u32 {
        u32::MAX
    }
    fn ctl_write(&self, _: u32) {
        panic!("nothing may be written where nothing answers");
    }
    fn count_read(&self) -> u32 {
        u32::MAX
    }
    fn count_write(&self, _: u32) {}
}

#[test]
fn fired_is_read_even_when_the_chip_refuses() {
    // V1 logs FIRED before it looks at DISABLED: a watchdog reset followed by a boot with the chip disabled.
    let f = Fake::new(true, true, None);
    assert_eq!(regs::enable(&f), Err(ChipError::Disabled));
    let st = regs::read_and_clear_fired(&f);
    assert!(st.fired);
    assert_eq!(f.ctl() & regs::FIRED, 0, "cleared");
    // All ones is no chip, not FIRED.
    assert_eq!(regs::enable(&Absent), Err(ChipError::NoChip));
    let st = regs::read_and_clear_fired(&Absent);
    assert!(!st.fired && !st.was_running);
}

// ---------------------------------------------------------------------------------------------------------
// CONFIG

#[test]
fn config_defaults_and_bad_values() {
    let (c, p) = config::parse(None, None, None, None, None, false);
    assert!(c.on && !c.on_from_config && p.is_none());
    assert_eq!((c.timeout_s, c.alive_s, c.grace_s, c.reload_grace), (30, 10, 240, false));
    let (c, _) = config::parse(None, None, None, None, None, true);
    assert!(!c.on, "off under a hypervisor by default");
    let (c, p) = config::parse(Some("on"), Some("60"), Some("20"), Some("300"), Some("grace"), true);
    assert!(c.on && c.on_from_config && p.is_none());
    assert_eq!((c.timeout_s, c.alive_s, c.grace_s, c.reload_grace), (60, 20, 300, true));
    let (c, p) = config::parse(Some("OFF"), None, None, None, None, false);
    assert!(!c.on && p.is_none());
    let (c, p) = config::parse(Some("maybe"), Some("2"), None, None, Some("x"), false);
    assert!(c.on && c.timeout_s == 30);
    assert_eq!(p, Some("watchdog"));
    let (c, p) = config::parse(None, Some("8"), Some("10"), None, None, false);
    assert_eq!((c.timeout_s, c.alive_s), (8, 5), "window shrunk to fit the timeout");
    assert_eq!(p, Some("watchdog_alive"));
    let (c, p) = config::parse(None, Some("45s"), None, Some("1"), None, false);
    assert_eq!((c.timeout_s, c.grace_s), (45, 240));
    assert_eq!(p, Some("watchdog_grace"));
}

#[test]
fn hang_only_when_armed() {
    assert!(hang_refusal(State::Armed).is_none());
    for st in [State::NotStarted, State::Starting, State::Absent, State::Off, State::Unusable, State::Stopped, State::Grace] {
        assert!(hang_refusal(st).is_some_and(|t| t.contains(st.name())), "{st:?}");
    }
}

/// RB-14: the two sync-flood bits name a flood; the reset that was asked for (0xCF9 6) does not, nor all ones.
#[test]
fn sync_flood_bits() {
    use crate::regs::{is_sync_flood, s5_reset_reasons};
    assert!(is_sync_flood(1 << 27));
    assert!(is_sync_flood(1 << 31 | 1 << 19));
    assert!(!is_sync_flood(1 << 19));
    assert!(!is_sync_flood(0));
    assert!(!is_sync_flood(u32::MAX), "a window that reads all ones says nothing");
    assert_eq!(s5_reset_reasons(1 << 27), "sync_flood_error");
    assert_eq!(s5_reset_reasons(1 << 19), "cf9_6");
}

// ---------------------------------------------------------------------------------------------------------
// RB-16: breadcrumbs in the count register

#[test]
fn a_crumb_is_the_timeout_while_the_chip_runs_and_not_kept_when_it_is_stopped() {
    use crate::crumb::{self, Previous};
    use klog::crumb::Stage;
    let t = Arc::new(Table::default());
    *t.cpus.lock().unwrap() = vec![s(1, true, true)];
    let fake = Arc::new(Fake::new(false, false, Some(dog_clock)));
    let d = Dog::new(Box::new(fake.clone()), Box::new(FakeCpus(t.clone())), dog_clock, pol(), 30);
    d.arm().unwrap();
    let base = crumb::base(30, 240);
    let code = crumb::code(crumb::id(Stage::Device, "wlan0"), base).unwrap();
    let k = fake.kicks();
    assert!(d.crumb(code));
    assert_eq!((fake.count(), fake.kicks()), (code, k + 1), "counted from now");
    // A shutdown that did not end the kernel: the normal timeout back.
    d.restore_timeout();
    assert_eq!(fake.count(), 30);
    // The grace (the watchdog's own Quiesce), then the reset's crumb: still running, counted from now.
    assert!(d.grace(240));
    let reset = crumb::code(Stage::Reset as u16, base).unwrap();
    d.crumb(reset);
    assert_eq!(fake.count(), reset);
    d.restore_timeout();
    assert_eq!(fake.count(), reset, "not armed any more (grace): nothing restored");
    // RB-17: stopped, the chip only takes the value for its next kick and reads what it stopped with (the
    // Mini-PC after a Reload before RB-17: the watchdog's own crumb, not the jump's).
    d.stop();
    let k = fake.kicks();
    let jump = crumb::code(Stage::ReloadJump as u16, base).unwrap();
    assert!(!d.crumb(jump), "not kept: reported as such");
    assert_eq!((fake.count(), fake.reload_value(), fake.kicks()), (reset, jump, k));
    let st = regs::read_and_clear_fired(&*fake);
    assert_eq!(Previous::read(st.count, base).id, Some(Stage::Reset as u16));
}

/// The crumbs of a Reload's way out, in kexec's order, on a dog that `quiesce` (the watchdog's Reload
/// Quiesce) either stops at once (RB-16) or holds until the jump (RB-17). Returns the count the next start reads.
fn reload_way_out(fake: &Arc<Fake>, hold: bool) -> u32 {
    use crate::crumb;
    use klog::crumb::Stage;
    let t = Arc::new(Table::default());
    *t.cpus.lock().unwrap() = vec![s(1, true, true)];
    let d = Dog::new(Box::new(fake.clone()), Box::new(FakeCpus(t.clone())), dog_clock, pol(), 30);
    d.arm().unwrap();
    let base = crumb::base(30, 240);
    let leave = |stage: Stage, what: &str| d.crumb(crumb::code(crumb::id(stage, what), base).unwrap());
    leave(Stage::ReloadStopped, "reload");
    leave(Stage::Quiesce, "reload");
    for dev in ["nvme0", "xhci0", "mirror", "igc0", "fch-wdt"] {
        leave(Stage::Device, dev);
        if dev == "fch-wdt" {
            if hold {
                assert!(d.hold());
            } else {
                d.stop();
            }
        }
    }
    leave(Stage::Quiesced, "");
    leave(Stage::ReloadSweep, "reload");
    leave(Stage::ReloadJump, "reload");
    if hold {
        d.stop();
    }
    regs::read_and_clear_fired(&**fake).count
}

#[test]
fn a_clean_reload_reads_as_reached_its_end_at_the_next_start() {
    use crate::crumb::{self, Previous};
    use klog::crumb::Stage;
    let base = crumb::base(30, 240);
    // RB-16's order on the chip's count register: the next start read the watchdog's own crumb (Mini-PC 28.10.).
    let fake = Arc::new(Fake::new(false, false, Some(dog_clock)));
    let p = Previous::read(reload_way_out(&fake, false), base);
    assert_eq!(p.id, Some(crumb::id(Stage::Device, "fch-wdt")));
    assert!(p.incomplete() && p.text().contains("stopped there"), "{}", p.text());
    // RB-17: held through the last crumbs, stopped after the jump's: a clean Reload is never incomplete.
    let fake = Arc::new(Fake::new(false, false, Some(dog_clock)));
    let count = reload_way_out(&fake, true);
    assert!(!fake.running(), "stopped before the jump");
    for drift in 0..3 {
        // The next start reads the stopped counter (a second or two of drift at most before the stop).
        let p = Previous::read_with(count - drift, base, false);
        assert_eq!(p.id, Some(Stage::ReloadJump as u16), "drift {drift}");
        assert!(!p.incomplete() && p.text().contains("reload_jump") && p.text().contains("reached its end"), "{}", p.text());
    }
}

/// Its own clock: `dog_kicks_only_when_all_alive` moves `DOG_CLOCK` while the tests run in parallel.
static HOLD_CLOCK: AtomicU64 = AtomicU64::new(1);
fn hold_clock() -> u64 {
    HOLD_CLOCK.load(Ordering::SeqCst)
}

/// RB-17 fixer: the paths around the held Reload besides the clean one. A hang between the Quiesce and the jump
/// (held, never stopped) is reset after the last crumb's count and gets no kick; a jump that failed after the
/// stop is armed again without kicks (`after_failed_jump`'s call) and resets after the timeout; with
/// `watchdog_reload=grace` the crumbs land counted from now and the next start reads them as "about", never
/// incomplete.
#[test]
fn a_held_reload_resets_a_hang_rearms_after_a_failed_jump_and_grace_stays_about() {
    use crate::crumb::{self, Previous};
    use klog::crumb::Stage;
    let base = crumb::base(30, 240);
    let sweep = crumb::code(Stage::ReloadSweep as u16, base).unwrap();
    let jump = crumb::code(Stage::ReloadJump as u16, base).unwrap();
    let t = Arc::new(Table::default());
    *t.cpus.lock().unwrap() = vec![s(1, true, true)];
    let fake = Arc::new(Fake::new(false, false, Some(hold_clock)));
    let d = Dog::new(Box::new(fake.clone()), Box::new(FakeCpus(t.clone())), hold_clock, pol(), 30);
    d.arm().unwrap();
    // The hang: held, the sweep's crumb counts from now, the jump never comes.
    assert!(d.hold(), "a running chip is held");
    let t0 = hold_clock();
    assert!(d.crumb(sweep), "held: the crumb is kept");
    let k = fake.kicks();
    HOLD_CLOCK.store(t0 + 60 * SEC, Ordering::SeqCst);
    d.check();
    assert_eq!((fake.kicks(), fake.running(), fake.count()), (k, true, sweep), "held: no kicks, still counting");
    assert!(!fake.would_fire(t0 + (sweep as u64 - 1) * SEC), "not before the crumb's count");
    assert!(fake.would_fire(t0 + (sweep as u64 + 1) * SEC), "the hang is reset after {sweep} s");
    assert!((5 * 60..=12 * 60).contains(&sweep), "minutes, not seconds: {sweep}");
    // The jump failed after the hook's stop: armed again without kicks, reset after the normal timeout.
    d.crumb(jump);
    d.stop();
    assert!(!fake.running());
    let t1 = hold_clock();
    d.arm_unkicked().unwrap();
    assert!(fake.running() && fake.count() == 30 && !d.armed(), "re-armed, not kicked by the dog");
    let k = fake.kicks();
    HOLD_CLOCK.store(t1 + 20 * SEC, Ordering::SeqCst);
    d.check();
    assert_eq!(fake.kicks(), k, "no kicks after the failed jump");
    assert!(!fake.would_fire(t1 + 29 * SEC) && fake.would_fire(t1 + 31 * SEC));
    // A failed jump without the stop (no hook ran): the same re-arm on a held chip.
    let fake = Arc::new(Fake::new(false, false, Some(hold_clock)));
    let d = Dog::new(Box::new(fake.clone()), Box::new(FakeCpus(t.clone())), hold_clock, pol(), 30);
    d.arm().unwrap();
    assert!(d.hold());
    d.arm_unkicked().unwrap();
    assert!(fake.running() && fake.count() == 30);
    // watchdog_reload=grace: the grace instead of the hold, the crumbs after it land, the counter runs into the
    // next start (read with `running`): "about", never "stopped there".
    let fake = Arc::new(Fake::new(false, false, Some(hold_clock)));
    let d = Dog::new(Box::new(fake.clone()), Box::new(FakeCpus(t.clone())), hold_clock, pol(), 30);
    d.arm().unwrap();
    assert!(d.grace(240));
    let dev = crumb::code(crumb::id(Stage::Device, "fch-wdt"), base).unwrap();
    for code in [dev, sweep, jump] {
        assert!(d.crumb(code), "running under the grace: kept");
    }
    assert!(fake.running() && fake.count() == jump);
    for count in [jump, jump - 3, dev] {
        let p = Previous::read_with(count, base, true);
        assert!(!p.incomplete() && p.text().contains("about "), "{}", p.text());
    }
}
