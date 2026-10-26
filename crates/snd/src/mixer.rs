//! The mixer core: what the `snd/mix` thread does on each wake (`Core::step`), free of the kernel so host tests
//! drive it with a fake back end and a fake clock.
//!
//! Each step: take the client list (states snapshotted under its lock, so `start_together` is atomic), follow
//! target changes and plug events, open the output on demand (the null output by the clock when nothing can
//! be opened), read the play position, top the output up to `lead` frames ahead of it, feed retiring outputs
//! silence until their real frames are surely out, finish drains, publish counters.
//!
//! Mixing: per client its frames at 48 kHz (resampled if needed), times its Q15 gain (`(s * g + 2^14) >> 15`, rounded, a
//! change ramps over 128 frames), summed in i64, times the master gain, saturated to 16 bit. A running client
//! with an empty ring gets silence; each starvation counts once (`underruns`), the silent frames too.
//!
//! Switching (the mixer holds every frame it mixed in `hist` until it is surely played): make before break
//! when the new output opens while the old one plays (another controller: jack <-> HDMI): the old output plays
//! what it has, then silence for `drain_margin` frames (QEMU's hda-codec buffers 2048 frames past LPIB, the AMD
//! FIFO 32), then closes; the new one starts with the next frame. When the new output is `Busy` (one converter
//! for speaker and headphone): break before make, the old one stops, the new one replays from the old one's
//! play position (`hist`); only what sat in the controller's FIFO is lost.

use crate::backend::{Backend, Endpoint, NullOut, OutError, Stream, Target, WakeFn};
use crate::client::{Client, Playback, State, UNITY};
use crate::resample::{Resampler, clamp16, OUT_RATE};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};
use devmodel::audio::PcmFormat;

/// A gain change moves by UNITY/RAMP per frame: 0 dB to mute in 128 frames (2.7 ms), no zipper clicks.
const RAMP: i64 = 128;
const RAMP_STEP: i64 = UNITY as i64 / RAMP;

/// Frame counts of the output side. `latency_ms` is the documented knob (CONFIG `snd_latency_ms`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Hardware period (wake-up granularity), a multiple of 32 frames (hda's 128-byte rule).
    pub period: u32,
    /// Frames kept queued ahead of the play position (four periods) when the clients have them: the most
    /// output latency the mixer adds.
    pub lead: u32,
    /// Frames the output never drops below (two periods): only to keep this many does the mixer take more
    /// than a running client has, which is that client's underrun (a dmix client's XRUN comes when the
    /// hardware passes its appl_ptr, not a whole lead before it).
    pub floor: u32,
    /// Hardware ring asked for: lead plus two periods of room.
    pub ring: u32,
    /// An output without running clients closes after this long.
    pub idle_ns: u64,
    /// Silence a retiring output plays after its last real frame before it closes.
    pub drain_margin: u32,
    /// Retry a real output this often while on the null output.
    pub retry_ns: u64,
}

impl Config {
    pub fn latency_ms(ms: u32) -> Config {
        let want = ms.clamp(5, 500) * (OUT_RATE / 1000);
        let period = (want / 4).div_ceil(32) * 32;
        let lead = 4 * period;
        Config { period, lead, floor: 2 * period, ring: lead + 2 * period, idle_ns: 2_000_000_000, drain_margin: 4800, retry_ns: 1_000_000_000 }
    }
    pub fn period_ns(&self) -> u64 {
        self.period as u64 * 1_000_000_000 / OUT_RATE as u64
    }
}

/// What the mixer thread shares with clients and callers.
pub struct Shared {
    pub(crate) clients: SpinLock<Vec<Arc<Client>>>,
    target: SpinLock<Target>,
    target_gen: AtomicU64,
    master: [AtomicU32; 2],
    kicked: AtomicBool,
    pub(crate) wake: WaitQueue,
    pub(crate) stop: AtomicBool,
    pub(crate) release: AtomicBool,
    pub(crate) next_id: AtomicU64,
    /// Clock time (ns) at which the output queue the last step measured runs dry (0: nothing queued).
    /// One word, so a reader never pairs one step's count with another step's time; `queued_now`
    /// takes off what played since, as Linux's delay reads the DMA position at every call
    /// (pcm_lib.c snd_pcm_update_hw_ptr0).
    out_empty_at: AtomicU64,
    /// Host tests: the simulated clock `queued_now` reads (0: the real one).
    #[cfg(test)]
    pub(crate) test_now: AtomicU64,
    played: AtomicU64,
    mixed: AtomicU64,
    hw_underruns: AtomicU64,
    switches: AtomicU64,
    opens: AtomicU64,
    open_failures: AtomicU64,
    streams: AtomicUsize,
    real: AtomicBool,
    endpoint: SpinLock<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The output playing now (None: none open or the null output).
    pub endpoint: Option<String>,
    /// Outputs open (the active one plus retiring ones).
    pub streams: usize,
    pub clients: usize,
    pub mixed: u64,
    pub played: u64,
    pub hw_underruns: u64,
    pub switches: u64,
    pub opens: u64,
    pub open_failures: u64,
}

/// A client as the `snd` tool shows it: its frames go to the one output in [`Status::endpoint`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: u64,
    pub name: String,
    pub fmt: PcmFormat,
    pub state: State,
    /// Q15 per channel (`client::UNITY` = 0 dB).
    pub volume: [u32; 2],
    /// Client frames the mixer took.
    pub consumed: u64,
    pub underruns: u64,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            clients: SpinLock::named("snd.mix.clients", Vec::new()),
            target: SpinLock::named("snd.mix.target", Target::Auto),
            target_gen: AtomicU64::new(0),
            master: [AtomicU32::new(UNITY), AtomicU32::new(UNITY)],
            kicked: AtomicBool::new(false),
            wake: WaitQueue::new(),
            stop: AtomicBool::new(false),
            release: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            out_empty_at: AtomicU64::new(0),
            #[cfg(test)]
            test_now: AtomicU64::new(0),
            played: AtomicU64::new(0),
            mixed: AtomicU64::new(0),
            hw_underruns: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            opens: AtomicU64::new(0),
            open_failures: AtomicU64::new(0),
            streams: AtomicUsize::new(0),
            real: AtomicBool::new(false),
            endpoint: SpinLock::named("snd.mix.endpoint", None),
        }
    }

    fn now(&self) -> u64 {
        #[cfg(test)]
        if let t @ 1.. = self.test_now.load(Ordering::Acquire) {
            return t;
        }
        time::now_ns()
    }

    /// Output frames queued ahead of the speaker now: the last step's count less what the output played
    /// since at its rate (never below 0).
    pub(crate) fn queued_now(&self) -> u64 {
        let left = self.out_empty_at.load(Ordering::Acquire).saturating_sub(self.now());
        left.saturating_mul(OUT_RATE as u64) / 1_000_000_000
    }

    /// Records `q` output frames queued at clock `now`. Rounds the time up so that `queued_now` at the
    /// same instant gives back exactly `q` (OUT_RATE < 1e9).
    pub(crate) fn set_queued(&self, now: u64, q: u64) {
        let at = if q == 0 { 0 } else { now.saturating_add(q.saturating_mul(1_000_000_000).div_ceil(OUT_RATE as u64)) };
        self.out_empty_at.store(at, Ordering::Release);
    }

    /// Wakes the mixer thread (callable from any thread, also the hda service thread).
    pub fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        self.wake.wake_one();
    }
    pub(crate) fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }

    pub fn set_target(&self, t: Target) {
        *self.target.lock() = t;
        self.target_gen.fetch_add(1, Ordering::AcqRel);
        self.kick();
    }
    pub fn target(&self) -> Target {
        self.target.lock().clone()
    }
    pub fn set_master(&self, g: [u32; 2]) {
        for (a, v) in self.master.iter().zip(g) {
            a.store(v.min(UNITY), Ordering::Release);
        }
    }
    pub fn master(&self) -> [u32; 2] {
        [self.master[0].load(Ordering::Acquire), self.master[1].load(Ordering::Acquire)]
    }
    pub fn status(&self) -> Status {
        let ld = |a: &AtomicU64| a.load(Ordering::Acquire);
        Status {
            endpoint: self.endpoint.lock().clone(),
            streams: self.streams.load(Ordering::Acquire),
            clients: self.clients.lock().len(),
            mixed: ld(&self.mixed),
            played: ld(&self.played),
            hw_underruns: ld(&self.hw_underruns),
            switches: ld(&self.switches),
            opens: ld(&self.opens),
            open_failures: ld(&self.open_failures),
        }
    }
    /// One row per client (SN-10's `snd` tool). Clones the list under the lock, reads the rest after it.
    pub fn client_infos(&self) -> Vec<ClientInfo> {
        let list: Vec<Arc<Client>> = self.clients.lock().clone();
        let ld = |a: &AtomicU64| a.load(Ordering::Acquire);
        list.iter()
            .map(|c| ClientInfo {
                id: c.id,
                name: c.name.clone(),
                fmt: c.fmt,
                state: c.state(),
                volume: [c.gain[0].load(Ordering::Acquire), c.gain[1].load(Ordering::Acquire)],
                consumed: ld(&c.consumed),
                underruns: ld(&c.underruns),
            })
            .collect()
    }
    /// Whether a real (not null) output is open.
    pub fn real_output(&self) -> bool {
        self.real.load(Ordering::Acquire)
    }
}

impl Default for Shared {
    fn default() -> Self {
        Shared::new()
    }
}

/// Starts all `list` in one step of the mixer (their first frames are mixed into the same output frame).
pub fn start_together(sh: &Shared, list: &[&Playback]) {
    {
        let _g = sh.clients.lock();
        for p in list {
            let c = p.client();
            if !c.move_state(State::Prepared, State::Running) {
                c.move_state(State::Paused, State::Running);
            }
        }
    }
    sh.kick();
}

struct Voice {
    c: Arc<Client>,
    rs: Option<Resampler>,
    gain: [i64; 2],
    snap: State,
    starved: bool,
    /// Zero inputs fed since the ring ran dry while draining.
    zeros: usize,
    /// Global frame after the client's last one (draining done once played).
    end: Option<u64>,
    reset_seen: u64,
}

impl Voice {
    fn new(c: Arc<Client>) -> Voice {
        let g = [c.gain[0].load(Ordering::Acquire) as i64, c.gain[1].load(Ordering::Acquire) as i64];
        // Made at open; a voice is made once per client, so it is there.
        let rs = c.rs.lock().take();
        let reset_seen = c.reset_gen.load(Ordering::Acquire);
        Voice { c, rs, gain: g, snap: State::Prepared, starved: false, zeros: 0, end: None, reset_seen }
    }
    fn taps(&self) -> usize {
        self.rs.as_ref().map_or(0, |r| r.taps())
    }
}

struct Out {
    s: Box<dyn Stream>,
    /// None: the null output.
    ep: Option<Endpoint>,
    /// Global frame of stream frame 0, modulo 2^64 (after an output underrun the stream's frame count runs
    /// ahead of the global one, so this may "go negative": always wrapping arithmetic).
    base: u64,
    written: u64,
    /// Mixed but not yet taken by the stream (the next global frames).
    carry: Vec<[i16; 2]>,
    started: bool,
}

impl Out {
    fn name(&self) -> String {
        self.ep.as_ref().map_or_else(|| "null".into(), |e| e.name())
    }
}

struct Retiring {
    o: Out,
    until: u64,
}

pub struct Core<B: Backend> {
    b: B,
    sh: Arc<Shared>,
    cfg: Config,
    wake: WakeFn,
    voices: Vec<Voice>,
    active: Option<Out>,
    retiring: Vec<Retiring>,
    mixed: u64,
    hist: Vec<[i16; 2]>,
    master: [i64; 2],
    target_gen: u64,
    idle_since: Option<u64>,
    retry_at: u64,
    fail_streak: u64,
    hw_underruns: u64,
    published: Option<String>,
    now: u64,
    acc: Vec<[i64; 2]>,
    tmp: Vec<[i32; 2]>,
    inbuf: Vec<[i16; 2]>,
    out: Vec<[i16; 2]>,
}

/// Half an LSB of Q15: products round to nearest instead of toward minus infinity (no DC bias per stage).
const ROUND: i64 = 1 << 14;

fn zeroed(len: usize) -> Option<Vec<[i16; 2]>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, [0; 2]);
    Some(v)
}

fn pow2(n: u64) -> bool {
    n.is_power_of_two()
}

impl<B: Backend> Core<B> {
    pub fn new(b: B, sh: Arc<Shared>, cfg: Config) -> Core<B> {
        let w = Arc::downgrade(&sh);
        let wake: WakeFn = Arc::new(move || {
            if let Some(s) = w.upgrade() {
                s.kick()
            }
        });
        let m = sh.master();
        Core {
            b,
            sh,
            cfg,
            wake,
            voices: Vec::new(),
            active: None,
            retiring: Vec::new(),
            mixed: 0,
            hist: zeroed(2 * (cfg.ring + cfg.lead) as usize).or_else(|| zeroed(1)).unwrap_or_default(),
            master: [m[0] as i64, m[1] as i64],
            target_gen: 0,
            idle_since: None,
            retry_at: 0,
            fail_streak: 0,
            hw_underruns: 0,
            published: None,
            now: 0,
            acc: Vec::new(),
            tmp: Vec::new(),
            inbuf: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn config(&self) -> Config {
        self.cfg
    }
    pub fn backend(&mut self) -> &mut B {
        &mut self.b
    }

    /// One round; returns how long the thread may sleep (it is also woken by kicks).
    pub fn step(&mut self, now: u64) -> u64 {
        self.now = now;
        self.sync_voices();
        let busy = self.voices.iter().any(|v| matches!(v.snap, State::Running | State::Draining));
        let tgen = self.sh.target_gen.load(Ordering::Acquire);
        let jack = self.b.take_jack_events();
        if tgen != self.target_gen || jack {
            self.target_gen = tgen;
            self.reroute(jack);
        }
        if self.active.is_none() {
            // Nothing plays: a master change needs no ramp.
            let m = self.sh.master();
            self.master = [m[0] as i64, m[1] as i64];
        }
        if self.active.is_none() && busy && now >= self.retry_at {
            self.open_active();
        }
        if self.active.as_ref().is_some_and(|a| a.ep.is_none()) && now >= self.retry_at {
            self.promote_null();
        }
        if busy {
            self.idle_since = None;
            self.sh.release.store(false, Ordering::Release);
        } else if self.active.is_some() {
            let since = *self.idle_since.get_or_insert(now);
            if self.sh.release.load(Ordering::Acquire) || now.saturating_sub(since) >= self.cfg.idle_ns {
                self.retire_active("idle");
            }
        }
        self.fill_active();
        self.service_retiring();
        self.finish_drains();
        self.publish();
        if self.active.is_some() || !self.retiring.is_empty() {
            (self.cfg.period_ns() / 2).max(500_000)
        } else {
            1_000_000_000
        }
    }

    /// Closes every output at once (thread stop).
    pub fn shutdown(&mut self) {
        if let Some(mut a) = self.active.take() {
            a.s.stop();
            a.s.close();
        }
        for mut r in core::mem::take(&mut self.retiring) {
            r.o.s.stop();
            r.o.s.close();
        }
        self.publish();
    }

    fn sync_voices(&mut self) {
        let mut fresh: Vec<Arc<Client>> = Vec::new();
        {
            let mut g = self.sh.clients.lock();
            for c in g.iter() {
                if !self.voices.iter().any(|v| v.c.id == c.id) {
                    fresh.push(c.clone());
                }
            }
            for v in self.voices.iter_mut() {
                v.snap = v.c.state();
            }
            g.retain(|c| c.state() != State::Closed);
        }
        for c in fresh {
            if self.voices.try_reserve(1).is_err() {
                // Not mixed this round; the client is still listed and is picked up at the next.
                klog::event!(Warn, "snd", "mix.voice_nomem", id = c.id; "retrying next round");
                break;
            }
            let mut v = Voice::new(c);
            v.snap = v.c.state();
            self.voices.push(v);
        }
        self.voices.retain(|v| v.snap != State::Closed);
        for v in self.voices.iter_mut() {
            if v.snap == State::Prepared {
                // Not mixed yet: take the volume as set, no ramp from what it was at open.
                v.gain = [v.c.gain[0].load(Ordering::Acquire) as i64, v.c.gain[1].load(Ordering::Acquire) as i64];
            }
            let r = v.c.reset_gen.load(Ordering::Acquire);
            if r != v.reset_seen {
                v.reset_seen = r;
                if let Some(r) = v.rs.as_mut() {
                    r.reset();
                }
                v.starved = false;
                v.zeros = 0;
                v.end = None;
            }
        }
    }

    fn reroute(&mut self, jack: bool) {
        let t = self.sh.target();
        let want = self.b.resolve(&t);
        let Some(a) = self.active.as_ref() else { return };
        match want {
            Some(ep) if a.ep.as_ref() == Some(&ep) => {}
            Some(ep) => self.switch_to(ep, if jack { "plug" } else { "target" }),
            None => klog::event!(Warn, "snd", "mix.target_unavailable", target = alloc::format!("{t:?}"), keep = a.name();
                "nothing playable there; staying on the current output"),
        }
    }

    fn log_open_failure(&mut self, what: &str, e: OutError) {
        self.fail_streak += 1;
        self.sh.open_failures.fetch_add(1, Ordering::AcqRel);
        if pow2(self.fail_streak) {
            klog::event!(Warn, "snd", "mix.open_failed", endpoint = what, error = alloc::format!("{e:?}"), count = self.fail_streak;
                "playing into the null output, retrying");
        }
    }

    fn new_out(&self, s: Box<dyn Stream>, ep: Option<Endpoint>, base: u64, carry: Vec<[i16; 2]>) -> Out {
        Out { s, ep, base, written: 0, carry, started: false }
    }

    fn open_real(&mut self, ep: &Endpoint) -> Result<Box<dyn Stream>, OutError> {
        let r = self.b.open(ep, self.cfg.ring, self.cfg.period, self.wake.clone(), self.now);
        match &r {
            Ok(s) => {
                self.fail_streak = 0;
                self.sh.opens.fetch_add(1, Ordering::AcqRel);
                let ring = s.ring_frames();
                let want = 2 * (ring + self.cfg.lead) as usize;
                if self.hist.len() < want {
                    self.resize_hist(want);
                }
                klog::event!(Info, "snd", "mix.open", endpoint = ep.name().as_str(), ring = ring, period = self.cfg.period, lead = self.cfg.lead);
            }
            Err(e) => self.log_open_failure(ep.name().as_str(), *e),
        }
        r
    }

    fn resize_hist(&mut self, len: usize) {
        let Some(fresh) = zeroed(len) else {
            // The old one stays: a break-before-make switch then replays less (it is clamped to it).
            klog::event!(Warn, "snd", "mix.hist_nomem", have = self.hist.len() as u64, want = len as u64);
            return;
        };
        let old = core::mem::replace(&mut self.hist, fresh);
        let keep = (old.len() as u64).min(self.mixed);
        for g in self.mixed - keep..self.mixed {
            self.hist[(g % len as u64) as usize] = old[(g % old.len() as u64) as usize];
        }
    }

    fn open_active(&mut self) {
        let t = self.sh.target();
        let s = match self.b.resolve(&t) {
            Some(ep) => match self.open_real(&ep) {
                Ok(s) => Some((s, ep)),
                Err(_) => None,
            },
            None => {
                self.log_open_failure("none", OutError::NoSuch);
                None
            }
        };
        match s {
            Some((s, ep)) => self.active = Some(self.new_out(s, Some(ep), self.mixed, Vec::new())),
            None => {
                self.retry_at = self.now + self.cfg.retry_ns;
                if self.fail_streak == 1 {
                    klog::event!(Info, "snd", "mix.null_output", ring = self.cfg.ring);
                }
                self.active = Some(self.new_out(NullOut::boxed(self.cfg.ring, self.now), None, self.mixed, Vec::new()));
            }
        }
    }

    /// On the null output: try the real one again.
    fn promote_null(&mut self) {
        self.retry_at = self.now + self.cfg.retry_ns;
        let t = self.sh.target();
        if let Some(ep) = self.b.resolve(&t) {
            self.switch_to(ep, "retry");
        }
    }

    fn switch_to(&mut self, ep: Endpoint, why: &str) {
        let Some(from) = self.active.as_ref().map(|a| a.name()) else { return };
        self.cut_retiring(&ep.dev);
        match self.open_real(&ep) {
            Ok(s) => {
                let Some(mut old) = self.active.take() else { return };
                let carry = core::mem::take(&mut old.carry);
                let n = carry.len();
                let new = self.new_out(s, Some(ep), self.mixed - n as u64, carry);
                if old.ep.is_some() {
                    let until = old.written + self.cfg.drain_margin as u64;
                    self.retiring.push(Retiring { o: old, until });
                } else {
                    old.s.close();
                }
                klog::event!(Info, "snd", "mix.switch", from = from.as_str(), to = new.name().as_str(), mode = "make_before_break", why = why, carried = n as u64);
                self.switched(new);
            }
            Err(OutError::Busy) => {
                let Some(mut old) = self.active.take() else { return };
                old.s.stop();
                let pos = old.s.position(self.now).unwrap_or(old.written).min(old.written);
                let mut g = old.base.wrapping_add(pos);
                let hl = self.hist.len() as u64;
                if self.mixed - g > hl {
                    g = self.mixed - hl;
                }
                let name = old.name();
                old.s.close();
                match self.open_real(&ep) {
                    Ok(s) => {
                        let carry: Vec<[i16; 2]> = (g..self.mixed).map(|i| self.hist[(i % hl) as usize]).collect();
                        let new = self.new_out(s, Some(ep), g, carry);
                        klog::event!(Info, "snd", "mix.switch", from = name.as_str(), to = new.name().as_str(), mode = "break_before_make", why = why,
                            replayed = self.mixed - g);
                        self.switched(new);
                    }
                    Err(e) => {
                        klog::event!(Warn, "snd", "mix.switch_failed", from = name.as_str(), to = ep.name().as_str(), error = alloc::format!("{e:?}"),
                            mode = "break_before_make"; "old output closed; reopening by the target");
                        self.retry_at = self.now;
                    }
                }
            }
            Err(e) => klog::event!(Warn, "snd", "mix.switch_failed", from = from.as_str(), to = ep.name().as_str(), error = alloc::format!("{e:?}");
                "staying on the current output"),
        }
    }

    /// The new output becomes active and is published before the switch counts, so a reader that sees the
    /// count also sees the endpoint.
    fn switched(&mut self, new: Out) {
        self.active = Some(new);
        self.publish();
        self.sh.switches.fetch_add(1, Ordering::AcqRel);
    }

    /// A retiring output on `dev` may hold the converter the new one needs: it closes now (what it had not
    /// played yet of its real frames is lost; only a switch back within lead + margin has any).
    fn cut_retiring(&mut self, dev: &str) {
        let now = self.now;
        let margin = self.cfg.drain_margin as u64;
        let mut keep = Vec::new();
        for mut r in core::mem::take(&mut self.retiring) {
            if r.o.ep.as_ref().is_some_and(|e| e.dev == dev) {
                let pos = r.o.s.position(now).unwrap_or(r.until);
                r.o.s.stop();
                klog::event!(Info, "snd", "mix.retire_cut", endpoint = r.o.name().as_str(), unplayed = r.until.saturating_sub(margin).saturating_sub(pos));
                r.o.s.close();
            } else {
                keep.push(r);
            }
        }
        self.retiring = keep;
    }

    fn retire_active(&mut self, why: &str) {
        let Some(mut a) = self.active.take() else { return };
        klog::event!(Info, "snd", "mix.idle_close", endpoint = a.name().as_str(), why = why, written = a.written);
        if a.ep.is_some() && a.started {
            let until = a.written + self.cfg.drain_margin as u64;
            self.retiring.push(Retiring { o: a, until });
        } else {
            a.s.stop();
            a.s.close();
        }
    }

    fn lose_active(&mut self, e: OutError) {
        if let Some(a) = self.active.take() {
            klog::event!(Warn, "snd", "mix.output_lost", endpoint = a.name().as_str(), error = alloc::format!("{e:?}"), written = a.written;
                "closing it; reopening by the target");
            a.s.close();
        }
        self.retry_at = self.now;
    }

    /// Output frames every running client can give now (None: no running client): the mixer tops the output
    /// up only that far, so a writer that keeps `snd_pcm_delay` above the lead is never starved by the mixer
    /// running ahead of it. Starved (silence-fed) clients do not hold the others back.
    fn supply(&self) -> Option<u64> {
        self.voices
            .iter()
            .filter(|v| v.snap == State::Running && !v.starved && v.end.is_none())
            .map(|v| {
                let len = v.c.ring.lock().len();
                v.rs.as_ref().map_or(len, |r| r.outputs_for(len)) as u64
            })
            .min()
    }

    fn fill_active(&mut self) {
        let now = self.now;
        let supply = self.supply();
        let floor = self.cfg.floor as u64;
        let Some(a) = self.active.as_mut() else { return };
        let pos = match a.s.position(now) {
            Ok(p) => p,
            Err(e) => return self.lose_active(e),
        };
        if pos > a.written {
            if a.started {
                self.hw_underruns += 1;
                self.sh.hw_underruns.fetch_add(1, Ordering::AcqRel);
                if pow2(self.hw_underruns) {
                    klog::event!(Warn, "snd", "mix.hw_underrun", endpoint = a.name().as_str(), count = self.hw_underruns, late = pos - a.written);
                }
            }
            a.written = pos;
            a.base = self.mixed.wrapping_sub(a.carry.len() as u64).wrapping_sub(pos);
        }
        self.sh.played.store(a.base.wrapping_add(pos), Ordering::Release);
        let ring = a.s.ring_frames() as u64;
        let lead = (self.cfg.lead as u64).min(ring);
        if !a.carry.is_empty() {
            match a.s.write(&a.carry) {
                Ok(k) => {
                    a.written += k as u64;
                    a.carry.drain(..k);
                }
                Err(e) => return self.lose_active(e),
            }
        }
        let queued = a.written - pos;
        let n = if a.carry.is_empty() {
            let want = lead.saturating_sub(queued);
            let must = floor.min(lead).saturating_sub(queued);
            want.min(supply.unwrap_or(want)).max(must) as usize
        } else {
            0
        };
        if n > 0 {
            self.mix(n);
            let Some(a) = self.active.as_mut() else { return };
            match a.s.write(&self.out[..n]) {
                Ok(k) => {
                    a.written += k as u64;
                    a.carry.extend_from_slice(&self.out[k..n]);
                }
                Err(e) => return self.lose_active(e),
            }
        }
        let Some(a) = self.active.as_mut() else { return };
        self.sh.set_queued(now, a.written - pos + a.carry.len() as u64);
        if !a.started && a.written > 0 {
            match a.s.start() {
                Ok(()) => a.started = true,
                Err(e) => self.lose_active(e),
            }
        }
    }

    fn service_retiring(&mut self) {
        let now = self.now;
        let lead = self.cfg.lead as u64;
        let margin = self.cfg.drain_margin as u64;
        let mut zeros: Vec<[i16; 2]> = Vec::new();
        self.retiring.retain_mut(|r| {
            let pos = match r.o.s.position(now) {
                Ok(p) => p,
                Err(_) => {
                    let o = core::mem::replace(&mut r.o.s, NullOut::boxed(0, now));
                    o.close();
                    return false;
                }
            };
            if pos >= r.until {
                r.o.s.stop();
                let s = core::mem::replace(&mut r.o.s, NullOut::boxed(0, now));
                klog::event!(Info, "snd", "mix.retired", endpoint = r.o.name().as_str(), played = pos, real = r.until.saturating_sub(margin));
                s.close();
                return false;
            }
            if pos > r.o.written {
                r.o.written = pos;
            }
            let want = lead.min(r.o.s.ring_frames() as u64).saturating_sub(r.o.written - pos) as usize;
            if want > 0 {
                zeros.resize(want, [0; 2]);
                if let Ok(k) = r.o.s.write(&zeros[..want]) {
                    r.o.written += k as u64;
                }
            }
            true
        });
    }

    fn finish_drains(&mut self) {
        let played = self.sh.played.load(Ordering::Acquire);
        let has_out = self.active.is_some();
        for v in self.voices.iter_mut() {
            if let Some(end) = v.end
                && has_out
                && played >= end
            {
                v.end = None;
                if v.c.move_state(State::Draining, State::Drained) {
                    klog::event!(Debug, "snd", "client.drained", id = v.c.id, consumed = v.c.consumed.load(Ordering::Acquire));
                }
                v.c.done.wake_all();
                v.c.kick_poll();
            }
        }
    }

    fn publish(&mut self) {
        let name = self.active.as_ref().and_then(|a| a.ep.as_ref().map(|e| e.name()));
        if name != self.published {
            self.published = name.clone();
            *self.sh.endpoint.lock() = name;
        }
        self.sh.real.store(self.active.as_ref().is_some_and(|a| a.ep.is_some()), Ordering::Release);
        self.sh.streams.store(self.active.is_some() as usize + self.retiring.len(), Ordering::Release);
        self.sh.mixed.store(self.mixed, Ordering::Release);
        if self.active.is_none() {
            self.sh.set_queued(0, 0);
        }
    }

    /// Mixes the next `n` global frames into `self.out[..n]` (and `hist`).
    fn mix(&mut self, n: usize) {
        self.acc.clear();
        self.acc.resize(n, [0; 2]);
        self.tmp.resize(n, [0; 2]);
        let after = self.mixed + n as u64;
        for v in self.voices.iter_mut() {
            if !matches!(v.snap, State::Running | State::Draining) || v.end.is_some() {
                continue;
            }
            let need = v.rs.as_ref().map_or(n, |r| r.inputs_for(n));
            self.inbuf.clear();
            let got = v.c.ring.lock().pop_into(&mut self.inbuf, need);
            if got > 0 {
                v.c.consumed.fetch_add(got as u64, Ordering::AcqRel);
                // Unconditional: wake_all's own SeqCst fence orders the pop before its waiter check (a bare
                // waiter_count() here could miss a writer that queued itself as the ring looked full).
                v.c.space.wake_all();
                v.c.kick_poll();
            }
            if got < need {
                if v.snap == State::Running && v.c.stop_on_underrun.load(Ordering::Acquire) && v.c.move_state(State::Running, State::Xrun) {
                    // ALSA XRUN: the client stops here; what it had is played, then silence (not mixed again).
                    v.snap = State::Xrun;
                    v.c.silence.fetch_add((need - got) as u64, Ordering::AcqRel);
                    let u = v.c.underruns.fetch_add(1, Ordering::AcqRel) + 1;
                    klog::event!(Warn, "snd", "client.xrun", id = v.c.id, name = v.c.name.as_str(), count = u; "stopped until prepared");
                    v.c.space.wake_all();
                    v.c.kick_poll();
                    v.c.done.wake_all();
                } else if v.snap == State::Running {
                    v.c.silence.fetch_add((need - got) as u64, Ordering::AcqRel);
                    if !v.starved {
                        v.starved = true;
                        let u = v.c.underruns.fetch_add(1, Ordering::AcqRel) + 1;
                        if pow2(u) {
                            klog::event!(Warn, "snd", "client.underrun", id = v.c.id, name = v.c.name.as_str(), count = u; "silence until it writes again");
                        }
                    }
                } else {
                    v.zeros += need - got;
                }
                self.inbuf.resize(need, [0; 2]);
            } else if v.snap == State::Running {
                v.starved = false;
            }
            match v.rs.as_mut() {
                Some(r) => r.run(&self.inbuf, &mut self.tmp[..n]),
                None => {
                    for (t, s) in self.tmp[..n].iter_mut().zip(&self.inbuf) {
                        *t = [s[0] as i32, s[1] as i32];
                    }
                }
            }
            let target = [v.c.gain[0].load(Ordering::Acquire) as i64, v.c.gain[1].load(Ordering::Acquire) as i64];
            for ch in 0..2 {
                let step = RAMP_STEP;
                for (a, t) in self.acc.iter_mut().zip(&self.tmp[..n]) {
                    if v.gain[ch] != target[ch] {
                        v.gain[ch] = if v.gain[ch] < target[ch] { (v.gain[ch] + step).min(target[ch]) } else { (v.gain[ch] - step).max(target[ch]) };
                    }
                    a[ch] += (t[ch] as i64 * v.gain[ch] + ROUND) >> 15;
                }
            }
            if v.snap == State::Draining && v.zeros >= v.taps().max(1) {
                v.end = Some(after);
            }
        }
        let target = self.sh.master();
        let target = [target[0] as i64, target[1] as i64];
        self.out.resize(n.max(self.out.len()), [0; 2]);
        let hl = self.hist.len() as u64;
        for ch in 0..2 {
            let step = RAMP_STEP;
            for (i, a) in self.acc.iter().enumerate() {
                if self.master[ch] != target[ch] {
                    let m = self.master[ch];
                    self.master[ch] = if m < target[ch] { (m + step).min(target[ch]) } else { (m - step).max(target[ch]) };
                }
                self.out[i][ch] = clamp16((a[ch] * self.master[ch] + ROUND) >> 15) as i16;
            }
        }
        if hl > 0 {
            for i in 0..n {
                self.hist[((self.mixed + i as u64) % hl) as usize] = self.out[i];
            }
        }
        self.mixed = after;
    }

    /// The endpoint of the active output (tests).
    pub fn active_endpoint(&self) -> Option<Option<Endpoint>> {
        self.active.as_ref().map(|a| a.ep.clone())
    }
    pub fn retiring(&self) -> usize {
        self.retiring.len()
    }
    pub fn mixed(&self) -> u64 {
        self.mixed
    }
}
