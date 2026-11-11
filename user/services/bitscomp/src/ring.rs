//! Claude's ring (plan/spec/desktop.md §3, Details.dc.html "CLAUDE-RING · ZUSTÄNDE"): a dashed ring, a faint
//! outer ring and a core, per state. DS-2: the busy states move in steps of `anim::RING_STEP` (thinking turns
//! fast, working slowly, listening breathes); idle and waiting stand still, so an idle desktop draws it once. The
//! design's blurred stroke glow is baked: the dash is drawn wide and faint, then narrower, then sharp.

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

/// WY-19b: the dash along the arc in the mock-up's 48 px box, for the ring the gpu level turns.
pub fn dash(r: Ring) -> (f32, f32) {
    let (on, off, _) = spec(r);
    (on, off)
}

/// WY-19b: whether the sprite itself moves: at the gpu level the shader turns the dash, only listening's core is left.
pub fn sprite_moves(r: Ring, t: &Theme) -> bool {
    moves(r) && (!t.effects_gpu || r == Ring::Listening)
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

/// Steps of `anim::RING_STEP` for one turn of the dash (thinking "dreht schnell"), and for one breath (listening).
fn steps_per_turn(r: Ring) -> u32 {
    match r {
        Ring::Thinking => 12,
        Ring::Listening => 40,
        _ => 30,
    }
}
const BREATH: u32 = 12;

/// Draws the ring centred at (cx, cy), `size` pixels across (the mock-up's box), at busy step `step` (0 = still).
pub fn draw(c: &mut Canvas, cx: f32, cy: f32, size: f32, r: Ring, step: u32, t: &Theme) {
    let k = size / 48.0;
    let colour = if r == Ring::Waiting { t.warm } else { t.claude };
    let (on, off, mut core) = spec(r);
    let step = if moves(r) { step } else { 0 };
    let turn = std::f32::consts::TAU * 17.0 * k;
    let phase = (step % steps_per_turn(r)) as f32 * turn / steps_per_turn(r) as f32;
    if r == Ring::Listening {
        core += 1.5 * (std::f32::consts::TAU * (step % BREATH) as f32 / BREATH as f32).sin();
    }
    // SVG dashes start at 3 o'clock; `arc` starts at the top: a quarter turn back.
    let dash = Some((on * k, off * k, 17.0 * k * std::f32::consts::FRAC_PI_2 + phase));
    // The baked stroke glow (feGaussianBlur in the design), at theme.glow's strength.
    let g = t.glow[3];
    if !t.effects_gpu {
        c.arc(cx, cy, 17.0 * k, 11.0 * k, dash, alpha(colour, 0.6 * g));
        c.arc(cx, cy, 17.0 * k, 7.0 * k, dash, alpha(colour, 0.9 * g));
        c.arc(cx, cy, 17.0 * k, 3.0 * k, dash, colour);
    }
    c.ring(cx, cy, 22.0 * k, k.max(1.0), alpha(colour, 0.3));
    c.disc(cx, cy, core * k, alpha(colour, 0.8));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_the_gpu_level_the_shader_has_the_dash_and_only_listening_steps_its_sprite() {
        let cheap = crate::theme::test_theme();
        let gpu = Theme { effects_gpu: true, ..cheap.clone() };
        let mut c = Canvas::new(60, 60);
        draw(&mut c, 30.0, 30.0, 48.0, Ring::Idle, 0, &gpu);
        // On the dash's radius straight up the cheap sprite has ink; the gpu one has only the outer ring and the core.
        assert_eq!(c.pixel(30, 13) >> 24, 0, "no dash in the sprite");
        assert!(c.pixel(30, 30) >> 24 > 150, "the core stays");
        assert!(!sprite_moves(Ring::Thinking, &gpu) && sprite_moves(Ring::Listening, &gpu));
        assert!(sprite_moves(Ring::Thinking, cheap) && !sprite_moves(Ring::Idle, cheap));
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
    fn busy_states_move_and_idle_and_waiting_stand_still() {
        for r in Ring::ALL {
            let changes = (1..40).filter(|&s| picture(r, s) != picture(r, s - 1)).count();
            if moves(r) {
                assert!(changes >= 30, "{r:?} moves ({changes} of 39 steps change)");
            } else {
                assert_eq!(changes, 0, "{r:?} stands still");
            }
        }
        // One turn of thinking comes back to the first picture.
        assert_eq!(picture(Ring::Thinking, 0), picture(Ring::Thinking, steps_per_turn(Ring::Thinking)));
    }

    #[test]
    fn the_stroke_glow_lies_outside_the_dash() {
        let t = crate::theme::test_theme();
        let mut c = Canvas::new(60, 60);
        draw(&mut c, 30.0, 30.0, 48.0, Ring::Working, 0, t);
        // Radius 17 is the dash, 21 the faint glow just inside the outer ring (22): some ink, less than the dash.
        let dash = (0..360).map(|d| (d as f32).to_radians()).map(|a| c.pixel(30 + (17.0 * a.sin()) as i32, 30 - (17.0 * a.cos()) as i32) >> 24).max().unwrap();
        let glow = (0..360).map(|d| (d as f32).to_radians()).map(|a| c.pixel(30 + (20.0 * a.sin()) as i32, 30 - (20.0 * a.cos()) as i32) >> 24).max().unwrap();
        assert!(dash > 200 && glow > 5 && glow < dash, "dash {dash}, glow {glow}");
    }
}
