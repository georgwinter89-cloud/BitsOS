//! WY-19b: the design's ambient clocks as pure functions of the time (seconds since the effects began): where the
//! beam is, how far the radar and the rings have turned, how a dot breathes, pulses or pings, how far dashes flowed.
//! The values are the keyframes of plan/spec/desktop-v2/project/Main.dc.html. Nothing here draws or wakes anything.

use crate::desk::Ring;

/// The part of the period `period` that `t` is into: 0..1.
pub fn frac(t: f64, period: f64) -> f32 {
    (t.rem_euclid(period) / period) as f32
}

/// CSS cubic-bezier(x1, y1, x2, y2) at `x` (Newton on x(s), then y(s)).
pub fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    let curve = |a: f32, b: f32, s: f32| {
        let r = 1.0 - s;
        3.0 * r * r * s * a + 3.0 * r * s * s * b + s * s * s
    };
    let slope = |a: f32, b: f32, s: f32| {
        let r = 1.0 - s;
        3.0 * r * r * a + 6.0 * r * s * (b - a) + 3.0 * s * s * (1.0 - b)
    };
    let mut s = x;
    for _ in 0..8 {
        let d = slope(x1, x2, s);
        if d.abs() < 1e-4 {
            break;
        }
        s = (s - (curve(x1, x2, s) - x) / d).clamp(0.0, 1.0);
    }
    curve(y1, y2, s)
}

/// CSS `ease-in-out`.
pub fn ease_in_out(x: f32) -> f32 {
    bezier(0.42, 0.0, 0.58, 1.0, x)
}

/// CSS `ease-out`.
pub fn ease_out(x: f32) -> f32 {
    bezier(0.0, 0.0, 0.58, 1.0, x)
}

/// The design's entry curve cubic-bezier(.2,.8,.2,1).
pub fn rise(x: f32) -> f32 {
    bezier(0.2, 0.8, 0.2, 1.0, x)
}

/// A keyframe pair 0%,100% = `a`, 50% = `b` under ease-in-out (pulse, breathe).
fn there_and_back(t: f64, period: f64, a: f32, b: f32) -> f32 {
    let p = frac(t, period) * 2.0;
    let k = if p <= 1.0 { ease_in_out(p) } else { ease_in_out(2.0 - p) };
    a + (b - a) * k
}

/// `.beam`: the top of the 140 board px band (physical px), crossing an output `out_h` px high in 9 s
/// (translateY(-160px) to translateY(1120px) on the 1080 px board). `k` is board px to physical px.
pub fn beam_top(t: f64, out_h: f32, k: f32) -> f32 {
    let from = -160.0 * k;
    let to = out_h + 40.0 * k;
    from + (to - from) * frac(t, 9.0)
}

/// `.radar`: spin 6 s; radians clockwise.
pub fn radar_angle(t: f64) -> f32 {
    frac(t, 6.0) * std::f32::consts::TAU
}

/// Seconds per turn of Claude's ring per state; None stands still. The busy states as `ring.rs` steps them
/// (thinking "dreht schnell"); idle turns at the design's `.spin` (60 s, GPU level only, desktop.md §0 answer 7).
pub fn ring_turn(r: Ring) -> Option<f64> {
    match r {
        Ring::Idle => Some(60.0),
        Ring::Thinking => Some(1.2),
        Ring::Working => Some(3.0),
        Ring::Listening => Some(4.0),
        Ring::Waiting => None,
    }
}

/// `.breathe`: (scale, opacity), 3.4 s.
pub fn breathe(t: f64) -> (f32, f32) {
    (there_and_back(t, 3.4, 1.0, 1.12), there_and_back(t, 3.4, 0.95, 0.55))
}

/// `.pulse`: opacity, 2.4 s.
pub fn pulse(t: f64) -> f32 {
    there_and_back(t, 2.4, 1.0, 0.3)
}

/// `.ping`: (scale, opacity), 2.2 s ease-out.
pub fn ping(t: f64) -> (f32, f32) {
    let k = ease_out(frac(t, 2.2));
    (0.6 + (2.4 - 0.6) * k, 0.9 * (1.0 - k))
}

/// `.flow`: stroke-dashoffset, board px along the path (dasharray 4 8, -24 px in 1.4 s).
pub fn flow_offset(t: f64) -> f32 {
    frac(t, 1.4) * 24.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bezier_hits_its_ends_and_matches_known_points() {
        for f in [ease_in_out as fn(f32) -> f32, ease_out, rise] {
            assert!(f(0.0).abs() < 1e-3);
            assert!((f(1.0) - 1.0).abs() < 1e-3);
        }
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-3, "ease-in-out is symmetric");
        assert!(rise(0.5) > 0.9, "the entry curve is nearly there halfway");
    }

    #[test]
    fn the_beam_crosses_the_whole_output_in_nine_seconds() {
        let k = 4.0 / 3.0;
        assert!((beam_top(0.0, 1440.0, k) + 160.0 * k).abs() < 0.01, "starts above the screen");
        assert!(beam_top(4.5, 1440.0, k) > 400.0);
        assert!((beam_top(8.9999, 1440.0, k) - (1440.0 + 40.0 * k)).abs() < 1.0, "ends below it");
        assert!((beam_top(9.0, 1440.0, k) + 160.0 * k).abs() < 0.01, "and comes round");
    }

    #[test]
    fn dots_follow_the_keyframes() {
        assert!((pulse(0.0) - 1.0).abs() < 1e-3 && (pulse(1.2) - 0.3).abs() < 1e-3);
        let (s, a) = breathe(1.7);
        assert!((s - 1.12).abs() < 1e-3 && (a - 0.55).abs() < 1e-3);
        let (s, a) = ping(0.0);
        assert!((s - 0.6).abs() < 1e-3 && (a - 0.9).abs() < 1e-3);
        assert!(ping(2.19).1 < 0.05, "a ping has faded out at its end");
    }

    #[test]
    fn a_waiting_ring_stands_still_the_others_turn() {
        assert_eq!(ring_turn(Ring::Waiting), None);
        for r in [Ring::Idle, Ring::Thinking, Ring::Working, Ring::Listening] {
            assert!(ring_turn(r).is_some());
        }
        assert!(ring_turn(Ring::Thinking) < ring_turn(Ring::Working));
    }
}
