//! The hybrid score of DESIGN §7.1:
//! `S = w_v V + w_b B + w_c C + w_r R - w_x X`, weights of the present positive parts renormalised,
//! the gate `V >= v_min or B >= b_min` first, sorting on the unclamped S, the shown score clamped to [0,1].
//! `C` (co-use) needs the usage log of CX-3; in CX-1 it is never present, so the cold-start weights apply.

use crate::config::RankCfg;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Parts {
    /// Calibrated vector similarity in [0,1]; None when the embedder did not answer.
    pub v: Option<f32>,
    pub b: f32,
    /// Co-use; None when no anchor exists.
    pub c: Option<f32>,
    pub r: f32,
    pub x: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub v: f32,
    pub b: f32,
    pub c: f32,
    pub r: f32,
    pub x: f32,
}

/// Renormalise over the present positive parts (cold start and fallbacks); `w_x` never changes.
pub fn weights(cfg: &RankCfg, has_v: bool, has_c: bool) -> Weights {
    let wv = if has_v { cfg.w_v } else { 0.0 };
    let wc = if has_c { cfg.w_c } else { 0.0 };
    let sum = wv + cfg.w_b + wc + cfg.w_r;
    let n = |w: f32| if sum > 0.0 { w / sum } else { 0.0 };
    Weights { v: n(wv), b: n(cfg.w_b), c: n(wc), r: n(cfg.w_r), x: cfg.w_x }
}

pub fn recency(age_days: f32, t_half: f32) -> f32 {
    if t_half <= 0.0 {
        return 0.0;
    }
    (-std::f32::consts::LN_2 * age_days.max(0.0) / t_half).exp()
}

pub fn calibrated(cos: f32, floor: f32, ceil: f32) -> f32 {
    if ceil <= floor {
        return cos.clamp(0.0, 1.0);
    }
    ((cos - floor) / (ceil - floor)).clamp(0.0, 1.0)
}

/// Gate: learning can reorder but never create a result.
pub fn passes_gate(p: &Parts, cfg: &RankCfg) -> bool {
    p.v.is_some_and(|v| v >= cfg.v_min) || p.b >= cfg.b_min
}

pub fn score(p: &Parts, w: &Weights) -> f32 {
    w.v * p.v.unwrap_or(0.0) + w.b * p.b + w.c * p.c.unwrap_or(0.0) + w.r * p.r - w.x * p.x
}

pub fn shown(s: f32) -> f32 {
    s.clamp(0.0, 1.0)
}

/// Reciprocal rank fusion (kept as a switch for comparison, DESIGN §7.1): 1/(60+rank) per list.
pub fn rrf(rank_v: Option<usize>, rank_b: Option<usize>) -> f32 {
    let f = |r: Option<usize>| r.map(|r| 1.0 / (60.0 + r as f32 + 1.0)).unwrap_or(0.0);
    f(rank_v) + f(rank_b)
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub doc: u32,
    pub s: f32,
    pub matching: u32,
    pub r: f32,
}

/// Sort on unclamped S, ties by matching chunk count, then recency, then doc index (stable output).
pub fn sort(v: &mut [Scored]) {
    v.sort_by(|a, b| {
        b.s.total_cmp(&a.s).then(b.matching.cmp(&a.matching)).then(b.r.total_cmp(&a.r)).then(a.doc.cmp(&b.doc))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn weights_sum_to_one_in_every_case() {
        let cfg = RankCfg::default();
        for (hv, hc) in [(true, true), (true, false), (false, true), (false, false)] {
            let w = weights(&cfg, hv, hc);
            assert!(close(w.v + w.b + w.c + w.r, 1.0), "{hv} {hc}");
            assert_eq!(w.x, 0.60);
        }
        let cold = weights(&cfg, true, false);
        assert!(close(cold.v, 0.45 / 0.85) && close(cold.b, 0.30 / 0.85) && close(cold.r, 0.10 / 0.85));
        let partial = weights(&cfg, false, true);
        assert!(close(partial.b, 0.30 / 0.55) && close(partial.c, 0.15 / 0.55));
        let pc = weights(&cfg, false, false);
        assert!(close(pc.b, 0.75) && close(pc.r, 0.25));
    }

    #[test]
    fn gate_and_clamp() {
        let cfg = RankCfg::default();
        let recent_only = Parts { v: Some(0.1), b: 0.1, c: None, r: 1.0, x: 0.0 };
        assert!(!passes_gate(&recent_only, &cfg));
        assert!(passes_gate(&Parts { b: 0.2, ..recent_only }, &cfg));
        let w = weights(&cfg, true, false);
        let dismissed = Parts { v: Some(0.4), b: 0.0, c: None, r: 0.0, x: 1.0 };
        let s = score(&dismissed, &w);
        assert!(s < 0.0 && shown(s) == 0.0);
        let mut v = vec![
            Scored { doc: 1, s: -0.3, matching: 0, r: 0.0 },
            Scored { doc: 2, s: -0.1, matching: 0, r: 0.0 },
            Scored { doc: 3, s: 0.5, matching: 1, r: 0.0 },
            Scored { doc: 4, s: 0.5, matching: 3, r: 0.0 },
        ];
        sort(&mut v);
        assert_eq!(v.iter().map(|x| x.doc).collect::<Vec<_>>(), vec![4, 3, 2, 1], "unclamped order, ties by chunks");
    }

    #[test]
    fn recency_halves() {
        assert!(close(recency(180.0, 180.0), 0.5));
        assert!(close(recency(0.0, 180.0), 1.0));
        assert!(close(calibrated(0.8, 0.7, 0.9), 0.5));
    }
}
