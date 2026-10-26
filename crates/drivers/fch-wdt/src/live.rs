//! Who is alive (V2-PLAN §4 "Watchdog", spec/core.md §10.1). Pure: the kicker feeds samples and its clock in,
//! the host tests feed made-up ones.
//!
//! A CPU is **alive** when its heartbeat (SC-4: every switch, idle return, preemption point, reschedule IPI and
//! tick) moved within the window (CONFIG `watchdog_alive=`, default 10 s), or when it sits in the idle loop with
//! interrupts on (its idle-entry record) and has not ignored a probe for a whole window. The idle record alone
//! is only the CPU's own claim (`sched::last_idle_entry`): a CPU that went idle and then hung inside an interrupt
//! handler still claims it. So a CPU whose heartbeat stood still for `probe_after` gets a probe (a reschedule
//! IPI, whose handler bumps the heartbeat); a CPU that answers moves, one that does not answer for a window is
//! stalled even if it claims to be idle. A tickless CPU halted in idle answers at once, so idle CPUs never count
//! as stalled; a CPU spinning with interrupts off cannot answer and does. The CPU that runs the check is alive.
//! Kick only when every online CPU is alive (`all_alive`).

/// One CPU as the scheduler shows it right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Joined the scheduler (offline CPUs are not judged).
    pub online: bool,
    pub heartbeat: u64,
    /// In its idle loop now (`IdleEntry::idle_now`).
    pub idle_now: bool,
    /// Its last idle entry had interrupts on.
    pub idle_irq_on: bool,
}

/// What the check remembers per CPU between rounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Track {
    pub seen: bool,
    pub last_heartbeat: u64,
    /// When the heartbeat last moved (or the CPU was first seen).
    pub moved_at: u64,
    /// When the oldest probe not answered yet went out (0: none).
    pub probe_since: u64,
    /// When the last probe went out.
    pub last_probe: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    Offline,
    /// The CPU that runs the check.
    Checking,
    /// The heartbeat moved within the window.
    Moved,
    /// In the idle loop with interrupts on, no probe ignored for a window.
    Idle,
    /// Neither: no heartbeat for longer than the window (`idle`: it claims to be idle but ignored the probes).
    Stalled { idle: bool },
}

impl Live {
    pub fn alive(self) -> bool {
        !matches!(self, Live::Stalled { .. })
    }
    pub fn name(self) -> &'static str {
        match self {
            Live::Offline => "offline",
            Live::Checking => "checking",
            Live::Moved => "moved",
            Live::Idle => "idle",
            Live::Stalled { idle: false } => "stalled",
            Live::Stalled { idle: true } => "stalled-idle",
        }
    }
    pub fn code(self) -> u8 {
        match self {
            Live::Offline => 0,
            Live::Checking => 1,
            Live::Moved => 2,
            Live::Idle => 3,
            Live::Stalled { idle: false } => 4,
            Live::Stalled { idle: true } => 5,
        }
    }
    pub fn from_code(c: u8) -> Live {
        match c {
            1 => Live::Checking,
            2 => Live::Moved,
            3 => Live::Idle,
            4 => Live::Stalled { idle: false },
            5 => Live::Stalled { idle: true },
            _ => Live::Offline,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// A heartbeat older than this is not proof of life.
    pub window_ns: u64,
    /// A heartbeat this old gets a probe (and again every `probe_after`).
    pub probe_after_ns: u64,
}

impl Policy {
    pub fn new(window_ns: u64) -> Policy {
        let window_ns = window_ns.max(1);
        Policy { window_ns, probe_after_ns: (window_ns / 4).clamp(1, 2_000_000_000) }
    }
}

/// The judgement of one CPU in one round: the new track, the verdict, and whether to probe it now.
/// `probed` is the caller's answer to "could the probe be sent" and is applied by `note_probe`.
pub fn judge(t: Track, s: Sample, checking: bool, now: u64, p: Policy) -> (Track, Live, bool) {
    if !s.online {
        return (Track::default(), Live::Offline, false);
    }
    let mut t = t;
    if !t.seen || s.heartbeat != t.last_heartbeat || checking {
        t = Track { seen: true, last_heartbeat: s.heartbeat, moved_at: now, probe_since: 0, last_probe: t.last_probe };
    }
    if checking {
        return (t, Live::Checking, false);
    }
    let age = now.saturating_sub(t.moved_at);
    let want_probe = age >= p.probe_after_ns && now.saturating_sub(t.last_probe) >= p.probe_after_ns;
    let ignored = t.probe_since != 0 && now.saturating_sub(t.probe_since) > p.window_ns;
    let live = if age <= p.window_ns {
        Live::Moved
    } else if s.idle_now && s.idle_irq_on && !ignored {
        Live::Idle
    } else {
        Live::Stalled { idle: s.idle_now && s.idle_irq_on }
    };
    (t, live, want_probe)
}

/// Records a probe that went out at `now` (`sent` false: it could not be sent, nothing changes).
pub fn note_probe(t: Track, sent: bool, now: u64) -> Track {
    if !sent {
        return t;
    }
    let now = now.max(1);
    Track { last_probe: now, probe_since: if t.probe_since == 0 { now } else { t.probe_since }, ..t }
}

/// The kick decision of one round.
pub fn all_alive(lives: impl IntoIterator<Item = Live>) -> bool {
    lives.into_iter().all(Live::alive)
}
