//! The series store: one value per series and sampling round. All series share the rounds' times. History is
//! bounded by a byte budget sized from RAM (R1), not by a count: the more series exist (programs come and
//! go), the fewer rounds are kept. The budget charges what the heap really holds: each series' name, map
//! entry and deque header, and per value 8 bytes plus as much again of the deques' growth room.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use core::fmt::Write;
use core::ops::Range;

/// A round in which a series had no value.
pub const NONE: u64 = u64::MAX;
/// A value (8 bytes) plus the room a `VecDeque` may hold beyond its length (it grows by doubling).
const VALUE_BYTES: usize = 16;
/// Per series besides its values: the name's heap block (series names are short, `token` caps the device
/// and program parts at 32), the `BTreeMap` entry (key `String` + `Series`) and node share.
pub const SERIES_BYTES: usize = 192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Hundredths of a percent: of the period (per CPU) or of one CPU (per program).
    Pct,
    Bytes,
    BytesPerS,
    /// Thousandths of events per second.
    PerS,
    /// Thousandths of a degree Celsius.
    Celsius,
    /// Milliwatts.
    Watt,
    /// A total (registered counters, error tallies) or a level (queue length).
    Count,
}

impl Unit {
    pub fn label(self) -> &'static str {
        match self {
            Unit::Pct => "%",
            Unit::Bytes => "B",
            Unit::BytesPerS => "B/s",
            Unit::PerS => "/s",
            Unit::Celsius => "C",
            Unit::Watt => "W",
            Unit::Count => "n",
        }
    }
    fn decimals(self) -> u32 {
        match self {
            Unit::Pct => 2,
            Unit::PerS | Unit::Celsius | Unit::Watt => 3,
            _ => 0,
        }
    }
    /// Totals are shown by their last value in a bucket, everything else by the mean.
    pub fn is_total(self) -> bool {
        self == Unit::Count
    }
    /// The stored value in the unit's label: `1250` Pct → `12.5`, `48125` Celsius → `48.125`.
    pub fn write(self, v: u64, out: &mut String) {
        let scale = 10u64.pow(self.decimals());
        let _ = write!(out, "{}", v / scale);
        let mut frac = v % scale;
        if frac != 0 {
            let mut digits = self.decimals() as usize;
            while frac % 10 == 0 {
                frac /= 10;
                digits -= 1;
            }
            let _ = write!(out, ".{frac:0digits$}");
        }
    }
}

pub struct Series {
    pub unit: Unit,
    /// Round of `values[0]`.
    first: u64,
    /// Last round with a value: a series without one left in the history is removed.
    last: u64,
    values: VecDeque<u64>,
}

impl Series {
    pub fn at(&self, round: u64) -> Option<u64> {
        let i = round.checked_sub(self.first)?;
        self.values.get(i as usize).copied().filter(|&v| v != NONE)
    }
    pub fn latest(&self) -> Option<u64> {
        self.at(self.last)
    }
}

pub struct Store {
    times: VecDeque<u64>,
    /// Round number of `times[0]` (rounds count up from the first one, never reused).
    base: u64,
    series: BTreeMap<String, Series>,
    budget: usize,
}

impl Store {
    pub fn new(budget_bytes: usize) -> Self {
        Store { times: VecDeque::new(), base: 0, series: BTreeMap::new(), budget: budget_bytes }
    }

    /// Starts a round at `now_ns` (never before the previous one) and returns its number.
    pub fn begin(&mut self, now_ns: u64) -> u64 {
        let t = self.times.back().map_or(now_ns, |&b| now_ns.max(b));
        self.times.push_back(t);
        self.end_round() - 1
    }

    /// Sets `name`'s value in the current round (a second put in one round replaces the first).
    pub fn put(&mut self, name: &str, unit: Unit, v: u64) {
        let Some(cur) = self.end_round().checked_sub(1).filter(|_| !self.times.is_empty()) else { return };
        let v = v.min(NONE - 1);
        match self.series.get_mut(name) {
            Some(s) => {
                s.unit = unit;
                let i = (cur - s.first) as usize;
                if i < s.values.len() {
                    s.values[i] = v;
                } else {
                    s.values.resize(i, NONE);
                    s.values.push_back(v);
                }
                s.last = cur;
            }
            None => {
                let mut values = VecDeque::new();
                values.push_back(v);
                self.series.insert(String::from(name), Series { unit, first: cur, last: cur, values });
            }
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.series.contains_key(name)
    }

    /// Rounds kept with today's number of series (at least 2, so a rate can always be read).
    pub fn keep(&self) -> usize {
        let n = self.series.len();
        (self.budget.saturating_sub(n * SERIES_BYTES) / (VALUE_BYTES * (n + 1))).max(2)
    }

    /// What the store charges against its budget now.
    pub fn charged(&self) -> usize {
        let n = self.series.len();
        n * SERIES_BYTES + VALUE_BYTES * (n + 1) * self.times.len()
    }

    /// Ends a round: drops the oldest rounds beyond the budget and series with no value left.
    pub fn end(&mut self) {
        let keep = self.keep();
        while self.times.len() > keep {
            self.times.pop_front();
            self.base += 1;
        }
        let base = self.base;
        self.series.retain(|_, s| {
            if s.first < base {
                let drop = ((base - s.first) as usize).min(s.values.len());
                s.values.drain(..drop);
                s.first = base;
            }
            s.last >= base
        });
    }

    fn end_round(&self) -> u64 {
        self.base + self.times.len() as u64
    }

    /// First kept round and one past the newest.
    pub fn rounds(&self) -> Range<u64> {
        self.base..self.end_round()
    }

    pub fn time(&self, round: u64) -> Option<u64> {
        self.times.get(round.checked_sub(self.base)? as usize).copied()
    }

    /// Rounds whose time lies in `from_ns..=to_ns`.
    pub fn range(&self, from_ns: u64, to_ns: u64) -> Range<u64> {
        let a = self.times.partition_point(|&t| t < from_ns) as u64;
        let b = self.times.partition_point(|&t| t <= to_ns) as u64;
        self.base + a..self.base + b.max(a)
    }

    pub fn series(&self) -> impl Iterator<Item = (&str, &Series)> {
        self.series.iter().map(|(k, s)| (k.as_str(), s))
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn units_render_their_decimals() {
        let r = |u: Unit, v| {
            let mut s = String::new();
            u.write(v, &mut s);
            s
        };
        assert_eq!((r(Unit::Pct, 1250), r(Unit::Pct, 10000), r(Unit::Pct, 3)), ("12.5".into(), "100".into(), "0.03".into()));
        assert_eq!((r(Unit::Celsius, 48125), r(Unit::Watt, 7004), r(Unit::Bytes, 4096)), ("48.125".into(), "7.004".into(), "4096".into()));
    }

    #[test]
    fn rounds_align_series_and_gaps_are_none() {
        let mut s = Store::new(1 << 20);
        s.begin(10);
        s.put("a", Unit::Count, 1);
        s.end();
        s.begin(20);
        s.put("b", Unit::Count, 5);
        s.end();
        s.begin(30);
        s.put("a", Unit::Count, 3);
        s.put("a", Unit::Count, 4);
        s.end();
        let a = s.series().find(|x| x.0 == "a").unwrap().1;
        assert_eq!((a.at(0), a.at(1), a.at(2), a.latest()), (Some(1), None, Some(4), Some(4)));
        assert_eq!((s.range(15, 30), s.range(0, 5), s.range(31, 99)), (1..3, 0..0, 3..3));
        assert_eq!(s.time(2), Some(30));
    }

    #[test]
    fn the_budget_bounds_history_and_old_series_go() {
        // 2 series (2 * 192) + 3 values of 16 per round: budget 864 → 10 rounds.
        let mut s = Store::new(864);
        for r in 0..50u64 {
            s.begin(r * 2);
            s.put("x", Unit::Pct, r);
            if r < 30 {
                s.put("gone", Unit::Pct, r);
            }
            s.end();
        }
        let kept: Vec<_> = s.series().map(|x| x.0).collect();
        assert_eq!(kept, ["x"], "a series without a value in the kept rounds is removed");
        // One series left since round 39: (864 - 192) / 32 → 21 rounds may be kept, 20 have come since.
        assert_eq!((s.rounds(), s.keep()), (30..50, 21));
        assert!(s.charged() <= 864);
        let x = s.series().next().unwrap().1;
        assert_eq!((x.at(29), x.at(30), x.at(49)), (None, Some(30), Some(49)));
    }

    #[test]
    fn many_short_lived_series_stay_near_the_budget() {
        // A program per round, each with its own series: the per-series cost shrinks history, and gone
        // programs leave, so the charge stays at the budget (2 rounds is the floor for rates).
        let budget = 64 * 1024;
        let mut s = Store::new(budget);
        for r in 0..2000u64 {
            s.begin(r);
            s.put("cpu.all.busy", Unit::Pct, 1);
            for p in 0..20 {
                s.put(&alloc::format!("proc.{}.prog.cpu", r * 20 + p), Unit::Pct, 1);
            }
            s.end();
            assert!(s.charged() <= budget, "round {r}: {} > {budget}", s.charged());
        }
        assert!(s.len() < 400, "{} series kept", s.len());
    }

    #[test]
    fn time_never_goes_back() {
        let mut s = Store::new(1024);
        s.begin(100);
        s.begin(50);
        assert_eq!((s.time(0), s.time(1)), (Some(100), Some(100)));
    }
}
