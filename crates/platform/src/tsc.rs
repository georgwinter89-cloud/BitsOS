//! TSC offsets from the bring-up handshake (pure; host-tested). Per round the BSP stamps `t0`, the AP answers
//! with its own TSC `t1`, the BSP stamps `t2`; the AP's offset is `t1 - (t0 + t2) / 2` (V1 `cpu/aptsc.asm`). The
//! median of the rounds is kept, so one round disturbed by an SMI does not count; the round with the shortest
//! round trip gives a second estimate with the smallest error bound (half that round trip).

use alloc::vec::Vec;

/// Above this (on hardware) the offset is a warning: the Mini-PC measured <= 38 ticks (spec §6.1, §11.9), so
/// a few hundred ticks already mean the TSCs are not in step.
pub const WARN_TICKS: u64 = 200;

/// The Mini-PC check (spec §6.1, §11.9: V1 measured <= 38 ticks there): offsets above this are marked in
/// `summary` and fail `cp.smp.tsc_offsets_hardware` on hardware (never judged under a hypervisor).
pub const CHECK_TICKS: u64 = 38;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TscOffset {
    /// Median offset of the AP's TSC against the BSP's (ticks; positive = the AP is ahead).
    pub median: i64,
    /// Offset of the round with the shortest round trip.
    pub best: i64,
    /// That round trip (ticks): `best` is exact to half of it.
    pub min_rtt: u64,
    pub rounds: usize,
}

impl TscOffset {
    pub fn magnitude(&self) -> u64 {
        self.median.unsigned_abs()
    }
}

fn clamp(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// The offset from the samples `(t0, t1, t2)`; `None` without a usable round (a round whose `t2` is before `t0`
/// is dropped: the BSP's TSC cannot run backwards between two reads on one CPU).
pub fn offset(samples: &[(u64, u64, u64)]) -> Option<TscOffset> {
    let rounds: Vec<(u64, i64)> = samples
        .iter()
        .filter(|(t0, _, t2)| t2 >= t0)
        .map(|&(t0, t1, t2)| {
            let rtt = t2 - t0;
            let mid = t0 as i128 + (rtt / 2) as i128;
            (rtt, clamp(t1 as i128 - mid))
        })
        .collect();
    if rounds.is_empty() {
        return None;
    }
    let mut offs: Vec<i64> = rounds.iter().map(|r| r.1).collect();
    offs.sort_unstable();
    let median = offs[offs.len() / 2];
    let (min_rtt, best) = rounds.iter().copied().min_by_key(|r| r.0).unwrap_or((0, median));
    Some(TscOffset { median, best, min_rtt, rounds: rounds.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_step() {
        // AP exactly in the middle: offset 0.
        let s = [(1000, 1050, 1100), (2000, 2040, 2080), (3000, 3100, 3200)];
        let o = offset(&s).unwrap();
        assert_eq!((o.median, o.best, o.min_rtt, o.rounds), (0, 0, 80, 3));
    }

    #[test]
    fn ahead_and_behind() {
        let ahead = [(0, 70, 100), (1000, 1068, 1100), (2000, 2071, 2100)];
        assert_eq!(offset(&ahead).unwrap().median, 20);
        let behind = [(1_000_000, 999_990, 1_000_100)];
        let o = offset(&behind).unwrap();
        assert_eq!(o.median, -60);
        assert_eq!(o.magnitude(), 60);
    }

    #[test]
    fn one_disturbed_round_does_not_count() {
        let s = [(0, 50, 100), (1000, 1050, 1100), (2000, 9000, 20000), (3000, 3050, 3100), (4000, 4050, 4100)];
        let o = offset(&s).unwrap();
        assert_eq!(o.median, 0);
        assert_eq!(o.rounds, 5);
    }

    #[test]
    fn nothing_usable() {
        assert_eq!(offset(&[]), None);
        assert_eq!(offset(&[(100, 50, 90)]), None, "t2 before t0 is dropped");
        // Extreme values neither overflow nor panic.
        let o = offset(&[(0, u64::MAX, 0)]).unwrap();
        assert_eq!(o.median, i64::MAX);
        let o = offset(&[(u64::MAX - 1, 0, u64::MAX)]).unwrap();
        assert_eq!(o.median, i64::MIN);
    }
}
