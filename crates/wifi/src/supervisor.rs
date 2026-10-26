//! When to join (again): the supervisor of one WLAN interface, independent of what programs do
//! (spec/wlan.md §3: V1's rejoin rhythm hung on the prompt, the key wait and games, because V1
//! had one thread; V2 runs it in the driver's own kernel thread). Without I/O: the thread asks
//! [`Supervisor::tick`] whether to join now and sleeps until [`Supervisor::next_wake`] (B4: never
//! an unbounded wait).
//!
//! The rhythm (spec/wlan.md §3, V1 K6 `nicwlanwatch.asm` reworked): after a link is lost the
//! first join comes 2 s later, then six more 10 s apart, then one every 60 s, as long as the
//! interface is on. Switched on, the first join comes at once.

use crate::events::{EventLog, LinkLoss, WifiEvent};

/// The delays of the rhythm
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rhythm {
    /// before the first join after a loss
    pub first_ms: u64,
    /// between the next `short_tries` joins
    pub short_ms: u64,
    pub short_tries: u32,
    /// from then on
    pub long_ms: u64,
}

impl Default for Rhythm {
    fn default() -> Rhythm {
        Rhythm {
            first_ms: 2_000,
            short_ms: 10_000,
            short_tries: 6,
            long_ms: 60_000,
        }
    }
}

impl Rhythm {
    /// The wait before join number `attempt` (1 = the first after the loss)
    pub fn delay(&self, attempt: u32) -> u64 {
        match attempt {
            0 | 1 => self.first_ms,
            n if n <= 1 + self.short_tries => self.short_ms,
            _ => self.long_ms,
        }
    }
}

/// Where the interface is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvState {
    /// switched off: no joins
    Off,
    /// a join runs now
    Joining,
    /// linked
    Up,
    /// waiting until `until` (ms) for the next join
    Waiting { until: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supervisor {
    state: SvState,
    rhythm: Rhythm,
    /// joins since the link was lost (or the interface was switched on)
    attempt: u32,
    why: LinkLoss,
    events: EventLog<WifiEvent>,
}

impl Default for Supervisor {
    fn default() -> Supervisor {
        Supervisor::new(Rhythm::default())
    }
}

impl Supervisor {
    pub fn new(rhythm: Rhythm) -> Supervisor {
        Supervisor {
            state: SvState::Off,
            rhythm,
            attempt: 0,
            why: LinkLoss::JoinFailed,
            events: EventLog::new(),
        }
    }

    pub fn state(&self) -> SvState {
        self.state
    }
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
    pub fn take_events(&mut self) -> alloc::vec::Vec<WifiEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[WifiEvent] {
        self.events.as_slice()
    }

    /// Switched on (`wlan` on, or `Wlan "on"`): the first join at once
    pub fn on(&mut self, now: u64) {
        if self.state == SvState::Off {
            self.attempt = 0;
            self.state = SvState::Waiting { until: now };
        }
    }

    /// Switched off: no more joins (the driver ends a link itself)
    pub fn off(&mut self) {
        if self.state != SvState::Off {
            self.events.push(WifiEvent::Off);
        }
        self.state = SvState::Off;
        self.attempt = 0;
    }

    /// Whether to start a join now (then the state is `Joining` until `joined`/`join_failed`)
    pub fn tick(&mut self, now: u64) -> bool {
        match self.state {
            SvState::Waiting { until } if now >= until => {
                self.attempt = self.attempt.saturating_add(1);
                self.state = SvState::Joining;
                self.events.push(WifiEvent::RejoinStart {
                    attempt: self.attempt,
                });
                true
            }
            _ => false,
        }
    }

    /// When the thread must look again (`None`: nothing pending, sleep until an event)
    pub fn next_wake(&self) -> Option<u64> {
        match self.state {
            SvState::Waiting { until } => Some(until),
            _ => None,
        }
    }

    /// The join went through
    pub fn joined(&mut self) {
        if self.state == SvState::Off {
            return;
        }
        self.state = SvState::Up;
        self.attempt = 0;
        self.events.push(WifiEvent::LinkUp);
    }

    /// The join failed: the next one by the rhythm
    pub fn join_failed(&mut self, now: u64) {
        if self.state == SvState::Off {
            return;
        }
        self.schedule(now, LinkLoss::JoinFailed);
    }

    /// The link is gone (the AP ended it, no beacons, the chip failed): join again by the rhythm,
    /// counted from the first join after this loss
    pub fn link_lost(&mut self, now: u64, why: LinkLoss) {
        if self.state == SvState::Off {
            return;
        }
        self.attempt = 0;
        self.schedule(now, why);
    }

    fn schedule(&mut self, now: u64, why: LinkLoss) {
        let next = self.attempt.saturating_add(1);
        let in_ms = self.rhythm.delay(next);
        self.why = why;
        self.state = SvState::Waiting {
            until: now.saturating_add(in_ms),
        };
        self.events.push(WifiEvent::RejoinScheduled {
            attempt: next,
            in_ms,
            why,
        });
    }
}
