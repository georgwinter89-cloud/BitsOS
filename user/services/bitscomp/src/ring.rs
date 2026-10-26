//! Claude's ring (plan/spec/desktop.md §3, Details.dc.html "CLAUDE-RING · ZUSTÄNDE"): a dashed ring, a faint
//! outer ring and a core, per state. Static: an idle desktop draws it once and then costs nothing; CL-3 will
//! animate the busy states (the dash phase is a parameter for that).

use crate::canvas::Canvas;
use crate::desk::Ring;
use crate::theme::{Theme, alpha};

/// (dash on, dash off, core radius) in the mock-up's 48 px box (ring radius 17, outer 22).
fn spec(r: Ring) -> (f32, f32, f32) {
    match r {
        Ring::Idle => (70.0, 37.0, 4.0),
        Ring::Listening => (106.0, 1.0, 7.0),
        Ring::Thinking => (20.0, 10.0, 4.0),
        Ring::Working => (50.0, 8.0, 6.0),
        Ring::Waiting => (106.0, 1.0, 5.0),
    }
}

/// The German line under the state (the mock-up's hints).
pub fn hint(r: Ring) -> &'static str {
    match r {
        Ring::Idle => "ruhig",
        Ring::Listening => "pulsiert mit der Stimme",
        Ring::Thinking => "dreht schnell",
        Ring::Working => "steuert ein Programm",
        Ring::Waiting => "leuchtet warm",
    }
}

/// Draws the ring centred at (cx, cy), `size` pixels across (the mock-up's box), dash moved by `phase` pixels.
pub fn draw(c: &mut Canvas, cx: f32, cy: f32, size: f32, r: Ring, phase: f32, t: &Theme) {
    let k = size / 48.0;
    let colour = if r == Ring::Waiting { t.warm } else { t.accent };
    let (on, off, core) = spec(r);
    // SVG dashes start at 3 o'clock; `arc` starts at the top: a quarter turn back.
    let start = 17.0 * k * std::f32::consts::FRAC_PI_2 + phase;
    c.arc(cx, cy, 17.0 * k, 3.0 * k, Some((on * k, off * k, start)), colour);
    c.ring(cx, cy, 22.0 * k, k.max(1.0), alpha(colour, 0.3));
    c.disc(cx, cy, core * k, alpha(colour, 0.8));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_draws_ink_and_waiting_is_warm() {
        let t = crate::theme::test_theme();
        for r in Ring::ALL {
            let mut c = Canvas::new(60, 60);
            draw(&mut c, 30.0, 30.0, 48.0, r, 0.0, t);
            assert!(c.pixel(30, 30) >> 24 > 150, "{r:?} has its core");
            assert!(!hint(r).is_empty() && !r.word().is_empty());
        }
        let mut c = Canvas::new(60, 60);
        draw(&mut c, 30.0, 30.0, 48.0, Ring::Waiting, 0.0, t);
        let p = c.pixel(30, 30);
        assert!((p >> 16) & 255 > (p & 255), "warm: more red than blue");
    }
}
