//! SYS-A: the program's own history of the system-wide series, kept only while it is open: one point a second for
//! the last minute, the mean of 10 s for the last hour, the mean of 5 min for the last 24 hours. A gap (the
//! program was closed, a source missing) stays a gap (`None`), it is never filled.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Range {
    #[default]
    Minute,
    Hour,
    Day,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Minute, Range::Hour, Range::Day];
    /// (points, seconds per point).
    pub fn shape(self) -> (usize, u64) {
        match self {
            Range::Minute => (60, 1),
            Range::Hour => (360, 10),
            Range::Day => (288, 300),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Range::Minute => "1 Min",
            Range::Hour => "1 Std",
            Range::Day => "24 Std",
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Range::Minute => "1min",
            Range::Hour => "1h",
            Range::Day => "24h",
        }
    }
    pub fn of_word(w: &str) -> Option<Range> {
        Range::ALL.into_iter().find(|r| r.word() == w)
    }
    /// The axis labels under a chart (oldest first, "jetzt" last).
    pub fn axis(self) -> [&'static str; 5] {
        match self {
            Range::Minute => ["vor 60 s", "vor 45 s", "vor 30 s", "vor 15 s", "jetzt"],
            Range::Hour => ["vor 60 Min", "vor 45 Min", "vor 30 Min", "vor 15 Min", "jetzt"],
            Range::Day => ["vor 24 Std", "vor 18 Std", "vor 12 Std", "vor 6 Std", "jetzt"],
        }
    }
}

/// One level: fixed points, the newest last, and the bucket being filled.
#[derive(Clone, Debug, PartialEq)]
struct Level {
    points: Vec<Option<f64>>,
    step: u64,
    /// The bucket (time / step) being filled, its sum and count.
    bucket: Option<u64>,
    sum: f64,
    n: u32,
}

impl Level {
    fn new(r: Range) -> Level {
        let (n, step) = r.shape();
        Level { points: vec![None; n], step, bucket: None, sum: 0.0, n: 0 }
    }

    /// Closes the bucket being filled and moves on to `b`, leaving gaps for skipped buckets.
    fn roll(&mut self, b: u64) {
        let Some(cur) = self.bucket else {
            self.bucket = Some(b);
            return;
        };
        if b <= cur {
            return;
        }
        let mean = (self.n > 0).then(|| self.sum / self.n as f64);
        self.push(mean);
        let gaps = (b - cur - 1).min(self.points.len() as u64);
        for _ in 0..gaps {
            self.push(None);
        }
        self.bucket = Some(b);
        self.sum = 0.0;
        self.n = 0;
    }

    fn push(&mut self, v: Option<f64>) {
        self.points.remove(0);
        self.points.push(v);
    }

    fn add(&mut self, t: u64, v: Option<f64>) {
        self.roll(t / self.step);
        if let Some(v) = v.filter(|v| v.is_finite()) {
            self.sum += v;
            self.n += 1;
        }
    }

    /// The points with the bucket being filled as the newest.
    fn shown(&self) -> Vec<Option<f64>> {
        let mut v = self.points[1..].to_vec();
        v.push((self.n > 0).then(|| self.sum / self.n as f64));
        v
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    levels: [Level; 3],
}

impl Series {
    fn new() -> Series {
        Series { levels: Range::ALL.map(Level::new) }
    }
}

/// Every series by name; `t` is seconds on a monotonic clock.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    series: BTreeMap<String, Series>,
    /// When the first point came (the program opened): the 24 h view says it starts there.
    pub since: Option<u64>,
}

impl History {
    pub fn add(&mut self, t: u64, name: &str, v: Option<f64>) {
        self.since.get_or_insert(t);
        let s = self.series.entry(name.to_string()).or_insert_with(Series::new);
        for l in &mut s.levels {
            l.add(t, v);
        }
    }

    /// The points of `name` over `r`, oldest first (all gaps for an unknown series).
    pub fn points(&self, name: &str, r: Range) -> Vec<Option<f64>> {
        let i = Range::ALL.iter().position(|x| *x == r).unwrap_or(0);
        self.series.get(name).map_or_else(|| vec![None; r.shape().0], |s| s.levels[i].shown())
    }

    /// The newest value of `name` (this second's).
    #[cfg(test)]
    pub fn last(&self, name: &str) -> Option<f64> {
        self.points(name, Range::Minute).last().copied().flatten()
    }

    /// (mean, peak, index of the peak) of the known points.
    pub fn summary(points: &[Option<f64>]) -> Option<(f64, f64, usize)> {
        let known: Vec<(usize, f64)> = points.iter().enumerate().filter_map(|(i, v)| v.map(|v| (i, v))).collect();
        let first = known.first()?;
        let mean = known.iter().map(|(_, v)| v).sum::<f64>() / known.len() as f64;
        let (pi, peak) = known.iter().fold(*first, |a, b| if b.1 > a.1 { *b } else { a });
        Some((mean, peak, pi))
    }

    /// Replaces the 24 h points of `name` with the journal's (bitsjournal knows the time before the program
    /// opened); the bucket being filled stays the program's own.
    pub fn set_day(&mut self, name: &str, pts: &[Option<f64>]) {
        let s = self.series.entry(name.to_string()).or_insert_with(Series::new);
        let l = &mut s.levels[2];
        let n = l.points.len();
        let take = pts.len().min(n);
        for (i, v) in pts[pts.len() - take..].iter().enumerate() {
            l.points[n - take + i] = *v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_a_point_and_means_for_the_longer_views() {
        let mut h = History::default();
        for t in 0..25u64 {
            h.add(1000 + t, "cpu", Some(t as f64));
        }
        let m = h.points("cpu", Range::Minute);
        assert_eq!(m.len(), 60);
        assert_eq!(m[59], Some(24.0));
        assert_eq!(m[58], Some(23.0));
        assert_eq!(m[34], None, "before the program opened: a gap");
        let hr = h.points("cpu", Range::Hour);
        assert_eq!(hr.len(), 360);
        // Buckets of 10 s: 1000..1009 -> 0..9 mean 4.5, 1010..1019 -> 14.5, the open one 1020..1024 -> 22.
        assert_eq!(&hr[357..], &[Some(4.5), Some(14.5), Some(22.0)]);
        assert_eq!(h.last("cpu"), Some(24.0));
        assert_eq!(h.since, Some(1000));
    }

    #[test]
    fn gaps_stay_gaps() {
        let mut h = History::default();
        h.add(10, "net", Some(1.0));
        h.add(11, "net", None);
        h.add(15, "net", Some(3.0));
        let m = h.points("net", Range::Minute);
        assert_eq!(&m[54..], &[Some(1.0), None, None, None, None, Some(3.0)]);
        assert_eq!(h.points("nothing", Range::Day), vec![None; 288]);
        // A jump longer than the view clears it.
        h.add(500, "net", Some(9.0));
        let m = h.points("net", Range::Minute);
        assert_eq!(m.iter().filter(|v| v.is_some()).count(), 1);
    }

    #[test]
    fn summary_and_the_journal_day() {
        assert_eq!(History::summary(&[None, Some(2.0), Some(8.0), Some(5.0)]), Some((5.0, 8.0, 2)));
        assert_eq!(History::summary(&[None, None]), None);
        let mut h = History::default();
        h.add(0, "cpu", Some(1.0));
        h.set_day("cpu", &[Some(7.0), None, Some(9.0)]);
        let d = h.points("cpu", Range::Day);
        assert_eq!(&d[284..], &[Some(7.0), None, Some(9.0), Some(1.0)]);
    }
}
