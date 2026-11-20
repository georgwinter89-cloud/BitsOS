//! Median and spread of a benchmark's runs (5 per night).

/// The middle value (the mean of the two middle ones for an even count); None without values.
pub fn median(v: &[f64]) -> Option<f64> {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return None;
    }
    s.sort_by(f64::total_cmp);
    let n = s.len();
    Some(if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 })
}

/// Highest minus lowest run: how far the runs of one night lie apart.
pub fn spread(v: &[f64]) -> f64 {
    let f = v.iter().copied().filter(|x| x.is_finite());
    let (lo, hi) = f.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(x), b.max(x)));
    if hi >= lo { hi - lo } else { 0.0 }
}

/// Rounded to three digits after the point (the JSON stays readable; no benchmark is that exact).
pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_spread() {
        assert_eq!(median(&[5.0, 1.0, 3.0, 2.0, 4.0]), Some(3.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[f64::NAN, 7.0]), Some(7.0));
        assert_eq!(median(&[]), None);
        assert_eq!(spread(&[5.0, 1.0, 3.0]), 4.0);
        assert_eq!(spread(&[]), 0.0);
        assert_eq!(round3(1.23456), 1.235);
    }
}
