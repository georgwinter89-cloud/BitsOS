//! Claude's ring (plan/spec/desktop.md §3, Details.dc.html "CLAUDE-RING · ZUSTÄNDE"): a dashed ring, a faint
//! outer ring and a core, per state. DS-2: the busy states move (thinking turns fast, working slowly, listening
//! breathes); idle and waiting stand still, so an idle desktop draws it once. WY-19b: the shader draws the dash with its
//! stroke glow and turns it (`fx::plan::ring`); the sprite here is the outer ring and the core.

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

/// WY-19b: the dash along the arc in the mock-up's 48 px box, for the ring the shader turns.
pub fn dash(r: Ring) -> (f32, f32) {
    let (on, off, _) = spec(r);
    (on, off)
}

/// WY-19b: whether the sprite itself moves in steps of `anim::RING_STEP`: the shader turns the dash, only listening's
/// core is left.
pub fn sprite_moves(r: Ring) -> bool {
    moves(r) && r == Ring::Listening
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

/// Whether the state moves (a busy ring); the others are drawn once.
pub fn moves(r: Ring) -> bool {
    matches!(r, Ring::Thinking | Ring::Working | Ring::Listening)
}

/// Steps of `anim::RING_STEP` for one breath of listening's core.
const BREATH: u32 = 12;

/// Draws the ring's sprite centred at (cx, cy), `size` pixels across (the mock-up's box), at busy step `step` (0 =
/// still): the faint outer ring and the core, listening's breathing. The dash and its stroke glow are the shader's
/// (`fx::plan::ring`, WY-19b).
pub fn draw(c: &mut Canvas, cx: f32, cy: f32, size: f32, r: Ring, step: u32, t: &Theme) {
    let k = size / 48.0;
    let colour = if r == Ring::Waiting { t.warm } else { t.claude };
    let (_, _, mut core) = spec(r);
    if r == Ring::Listening {
        core += 1.5 * (std::f32::consts::TAU * (step % BREATH) as f32 / BREATH as f32).sin();
    }
    c.ring(cx, cy, 22.0 * k, k.max(1.0), alpha(colour, 0.3));
    c.disc(cx, cy, core * k, alpha(colour, 0.8));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shader_has_the_dash_and_only_listening_steps_its_sprite() {
        let t = crate::theme::test_theme();
        let mut c = Canvas::new(60, 60);
        draw(&mut c, 30.0, 30.0, 48.0, Ring::Idle, 0, t);
        // On the dash's radius straight up the sprite has no ink: only the outer ring and the core.
        assert_eq!(c.pixel(30, 13) >> 24, 0, "no dash in the sprite");
        assert!(c.pixel(30, 30) >> 24 > 150, "the core stays");
        assert!(!sprite_moves(Ring::Thinking) && sprite_moves(Ring::Listening) && !sprite_moves(Ring::Idle));
        assert!(moves(Ring::Thinking) && moves(Ring::Working) && !moves(Ring::Waiting));
    }

    #[test]
    fn every_state_draws_ink_and_waiting_is_warm() {
        let t = crate::theme::test_theme();
        for r in Ring::ALL {
            let mut c = Canvas::new(60, 60);
            draw(&mut c, 30.0, 30.0, 48.0, r, 0, t);
            assert!(c.pixel(30, 30) >> 24 > 150, "{r:?} has its core");
            assert!(!hint(r).is_empty() && !r.word().is_empty());
        }
        let mut c = Canvas::new(60, 60);
        draw(&mut c, 30.0, 30.0, 48.0, Ring::Waiting, 0, t);
        let p = c.pixel(30, 30);
        assert!((p >> 16) & 255 > (p & 255), "warm: more red than blue");
    }

    fn picture(r: Ring, step: u32) -> Vec<u32> {
        let t = crate::theme::test_theme();
        let mut c = Canvas::new(60, 60);
        draw(&mut c, 30.0, 30.0, 48.0, r, step, t);
        (0..60 * 60).map(|i| c.pixel(i % 60, i / 60)).collect()
    }

    #[test]
    fn only_listening_breathes_in_the_sprite() {
        for r in Ring::ALL {
            let changes = (1..40).filter(|&s| picture(r, s) != picture(r, s - 1)).count();
            if sprite_moves(r) {
                assert!(changes >= 6, "{r:?} breathes ({changes} of 39 steps change)");
            } else {
                assert_eq!(changes, 0, "{r:?} stands still in the sprite");
            }
        }
        assert_eq!(picture(Ring::Listening, 0), picture(Ring::Listening, BREATH), "one breath comes back");
    }
}
