//! `ctl fx`: what the effect level does, and the hooks the screendump tests and the desk's measuring round drive.
//!
//! fx                         one line: level, chosen level, renderer, clocks, what the renderer did so far
//! fx software on|off         draw the gpu level on a software rasteriser too (tests, measuring; default off)
//! fx freeze SECONDS|off      the ambient clocks stand at this time, no frame is asked for them
//! fx wait MS|off             cuts the renderer's wait for a frame's GPU work (a test's late frames; never longer)
//! fx play wipe|fade|glitch|carousel [K]   a transition; with K (0..1) it stands at that progress
//! fx play morph X Y W H X Y W H [K]       the old picture's first rectangle flies to the second (physical px)
//! fx turn ...                like `play`, but the running transition goes on as this one and keeps its old picture
//! fx title TEXT [K]          the title's blur reveal in the middle of the screen
//! fx hold K|off              the running transition or title stands at this progress
//! fx stop                    ends a transition or title
//! fx demo pulse|ping|flow X Y W H | fx demo clear   the primitives no screen uses yet
//!
//! Words come from the control socket: `parse` refuses what would not be a number on the clocks or a rectangle on the
//! screen, before any of it reaches the arithmetic and the shaders.

use std::rc::Rc;
use std::sync::atomic::Ordering;

use super::trans::{Image, Play, Title};
use super::{Demo, GPU_WAIT_CUT_MS, plan};
use crate::canvas::Canvas;
use crate::layout::Rect;
use crate::render::Composer;
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::Motion;

/// The latest time `fx freeze` takes: past it the clocks' f32 arithmetic has no seconds left.
const FREEZE_MOST: f64 = 1e6;

/// A command of `ctl fx`, checked.
#[derive(Debug, PartialEq)]
enum Cmd {
    Report,
    Software(bool),
    Freeze(Option<f64>),
    Wait(Option<u64>),
    Play { what: Play, hold: Option<f32>, keep: bool },
    Hold(Option<f32>),
    Stop,
    Title { text: String, hold: Option<f32> },
    Demo(Demo),
    DemoClear,
}

/// A progress: a finite number, 0..1 at most (NaN and the infinities are no progress).
fn progress(word: &str) -> Option<f32> {
    word.parse::<f32>().ok().filter(|k| k.is_finite()).map(|k| k.clamp(0.0, 1.0))
}

/// Four words as a rectangle with a size, all of it on a screen `sw` x `sh`.
fn rect(words: &[&str], (sw, sh): (i32, i32)) -> Option<[i32; 4]> {
    let v: Vec<i32> = words.iter().map(|w| w.parse().ok()).collect::<Option<_>>()?;
    let &[x, y, w, h] = v.as_slice() else { return None };
    let fits = x >= 0 && y >= 0 && w >= 1 && h >= 1 && x as i64 + w as i64 <= sw as i64 && y as i64 + h as i64 <= sh as i64;
    fits.then_some([x, y, w, h])
}

fn parse(words: &[&str], screen: (i32, i32)) -> Result<Cmd, String> {
    match words.get(1).copied() {
        None => Ok(Cmd::Report),
        Some("software") => match words.get(2).copied() {
            Some("on") => Ok(Cmd::Software(true)),
            Some("off") => Ok(Cmd::Software(false)),
            _ => Err("fx software on|off".into()),
        },
        Some("freeze") => match words.get(2).copied() {
            Some("off") => Ok(Cmd::Freeze(None)),
            Some(v) => {
                let t = v.parse::<f64>().ok().filter(|t| (0.0..=FREEZE_MOST).contains(t));
                Ok(Cmd::Freeze(Some(t.ok_or("fx freeze SECONDS|off")?)))
            }
            None => Err("fx freeze SECONDS|off".into()),
        },
        Some("wait") => match words.get(2).copied() {
            Some("off") => Ok(Cmd::Wait(None)),
            Some(v) => Ok(Cmd::Wait(Some(v.parse().map_err(|_| "fx wait MS|off")?))),
            None => Err("fx wait MS|off".into()),
        },
        Some("play") => play(words, screen, false),
        Some("turn") => play(words, screen, true),
        Some("hold") => match words.get(2).copied() {
            Some("off") => Ok(Cmd::Hold(None)),
            Some(v) => Ok(Cmd::Hold(Some(progress(v).ok_or("fx hold K|off")?))),
            None => Err("fx hold K|off".into()),
        },
        Some("stop") => Ok(Cmd::Stop),
        Some("title") => {
            let all = words.get(2..).unwrap_or_default();
            // A last word that is a progress is one, after at least one word of text.
            let (text, hold) = match all.split_last().and_then(|(k, text)| Some((text, progress(k)?))) {
                Some((text, k)) if !text.is_empty() => (text.join(" "), Some(k)),
                _ => (all.join(" "), None),
            };
            if text.is_empty() {
                return Err("fx title TEXT [K]".into());
            }
            Ok(Cmd::Title { text, hold })
        }
        Some("demo") => {
            let usage = "fx demo pulse|ping|flow X Y W H | fx demo clear";
            let what = words.get(2).copied().ok_or(usage)?;
            if what == "clear" {
                return Ok(Cmd::DemoClear);
            }
            let r = rect(words.get(3..7).ok_or(usage)?, screen).ok_or(usage)?;
            Ok(Cmd::Demo(match what {
                "pulse" => Demo::Pulse(r),
                "ping" => Demo::Ping(r),
                "flow" => Demo::Flow(r),
                _ => return Err(usage.into()),
            }))
        }
        Some(o) => Err(format!("fx {o}: see the list at the top of fx/ctl.rs")),
    }
}

fn play(words: &[&str], screen: (i32, i32), keep: bool) -> Result<Cmd, String> {
    let morph = "fx play morph X Y W H X Y W H [K], both on the screen";
    let (what, rest) = match words.get(2).copied() {
        Some("wipe") => (Play::Wipe, &words[3..]),
        Some("fade") => (Play::Fade, &words[3..]),
        Some("glitch") => (Play::Glitch, &words[3..]),
        Some("carousel") => (Play::Carousel, &words[3..]),
        Some("morph") => {
            let from = rect(words.get(3..7).ok_or(morph)?, screen).ok_or(morph)?;
            let to = rect(words.get(7..11).ok_or(morph)?, screen).ok_or(morph)?;
            (Play::Morph { from, to }, &words[11..])
        }
        _ => return Err("fx play wipe|fade|glitch|carousel|morph ...".into()),
    };
    let hold = match rest.first() {
        Some(k) => Some(progress(k).ok_or("K is a number 0..1")?),
        None => None,
    };
    Ok(Cmd::Play { what, hold, keep })
}

pub fn execute(app: &mut App, comp: &dyn Composer, words: &[&str]) -> Result<String, String> {
    let screen = crate::chrome_render::wallpaper::output_px(app);
    let cmd = parse(words, screen)?;
    match cmd {
        Cmd::Report => return Ok(report(app, comp)),
        Cmd::Software(on) => app.fx.on_software = on,
        Cmd::Freeze(t) => app.fx.frozen = t,
        Cmd::Wait(ms) => GPU_WAIT_CUT_MS.store(ms.unwrap_or(u64::MAX), Ordering::Relaxed),
        Cmd::Play { what, hold, keep } => return start(app, what, hold, keep),
        Cmd::Hold(k) => {
            if let Some(p) = &mut app.fx.playing {
                p.hold = k;
            }
            if let Some(p) = &mut app.fx.title {
                p.hold = k;
            }
        }
        Cmd::Stop => {
            app.fx.playing = None;
            app.fx.title = None;
        }
        Cmd::Title { text, hold } => title(app, &text, hold, screen)?,
        Cmd::Demo(d) => {
            // One of each: they share their element's key.
            app.fx.demo.retain(|o| std::mem::discriminant(o) != std::mem::discriminant(&d));
            app.fx.demo.push(d);
        }
        Cmd::DemoClear => app.fx.demo.clear(),
    }
    app.needs_frame = true;
    Ok("ok".to_string())
}

fn start(app: &mut App, what: Play, hold: Option<f32>, keep: bool) -> Result<String, String> {
    if !app.theme.effects_gpu {
        return Err("the gpu level is not drawn".into());
    }
    if app.theme.motion == Motion::Off {
        return Err("theme.motion off plays no transitions".into());
    }
    if !keep || app.fx.playing.is_none() {
        app.fx_play(what);
    }
    if let Some(p) = &mut app.fx.playing {
        if keep {
            p.what = what;
        }
        p.hold = hold;
    }
    app.needs_frame = true;
    Ok(format!("playing {}", app.fx.playing.map_or("none", |p| p.what.word())))
}

fn title(app: &mut App, text: &str, hold: Option<f32>, (sw, _): (i32, i32)) -> Result<(), String> {
    if !app.theme.effects_gpu {
        return Err("the gpu level is not drawn".into());
    }
    let t = app.theme.clone();
    let font = t.bfont(Family::Display, 80.0, Weight::SemiBold).spaced(-0.01);
    let w = font.width(text) + 8;
    if w > sw {
        return Err(format!("a title {w} px wide does not fit the screen's {sw} px"));
    }
    let h = (120.0 * t.board) as i32;
    let mut c = Canvas::new(w, h);
    c.text_mid(&font, Rect { x: 0, y: 0, w, h }, text, t.text);
    let serial = app.fx.next_serial();
    let image = Rc::new(Image { serial, w, h, bytes: c.bytes() });
    app.fx.title = Some(Title { image, since: crate::pacer::now(), hold });
    Ok(())
}

fn report(app: &App, comp: &dyn Composer) -> String {
    let t = &app.theme;
    let trans = match app.fx.playing {
        Some(p) => match p.hold {
            Some(k) => format!("{}@{k}", p.what.word()),
            None => p.what.word().to_string(),
        },
        None => "none".into(),
    };
    format!(
        "fx level={} chosen={} renderer={} on_software={} motion={} moving={} elements={} transition={trans} frozen={} why={:?} {}",
        if plan::drawn(t) { "gpu" } else { "cheap" },
        t.effects.word(),
        comp.renderer(),
        u8::from(app.fx.on_software),
        match t.motion {
            Motion::Medium => "medium",
            Motion::Calm => "calm",
            Motion::Off => "off",
        },
        // A step that came due waits as `needs_frame` for its grid point (a frame longer than a refresh).
        u8::from(app.fx.due().is_some() || app.needs_frame),
        app.fx.elements,
        app.fx.frozen.map_or("off".to_string(), |v| v.to_string()),
        app.fx.why_cheap.as_deref().unwrap_or(""),
        comp.fx_report(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: (i32, i32) = (2560, 1440);

    fn cmd(line: &str) -> Result<Cmd, String> {
        let words: Vec<&str> = line.split_whitespace().collect();
        parse(&words, SCREEN)
    }

    #[test]
    fn what_the_tests_and_the_measuring_round_send_is_understood() {
        assert_eq!(cmd("fx"), Ok(Cmd::Report));
        assert_eq!(cmd("fx software on"), Ok(Cmd::Software(true)));
        assert_eq!(cmd("fx freeze 4.5"), Ok(Cmd::Freeze(Some(4.5))));
        assert_eq!(cmd("fx freeze 0"), Ok(Cmd::Freeze(Some(0.0))));
        assert_eq!(cmd("fx freeze off"), Ok(Cmd::Freeze(None)));
        assert_eq!(cmd("fx wait 0"), Ok(Cmd::Wait(Some(0))));
        assert_eq!(cmd("fx wait off"), Ok(Cmd::Wait(None)));
        assert_eq!(cmd("fx hold 0.35"), Ok(Cmd::Hold(Some(0.35))));
        assert_eq!(cmd("fx hold 7"), Ok(Cmd::Hold(Some(1.0))));
        assert_eq!(cmd("fx play wipe"), Ok(Cmd::Play { what: Play::Wipe, hold: None, keep: false }));
        assert_eq!(cmd("fx turn glitch 0.3"), Ok(Cmd::Play { what: Play::Glitch, hold: Some(0.3), keep: true }));
        assert_eq!(
            cmd("fx turn morph 0 0 1280 720 400 300 640 360 0.6"),
            Ok(Cmd::Play { what: Play::Morph { from: [0, 0, 1280, 720], to: [400, 300, 640, 360] }, hold: Some(0.6), keep: true })
        );
        assert_eq!(cmd("fx title BitsOS"), Ok(Cmd::Title { text: "BitsOS".into(), hold: None }));
        assert_eq!(cmd("fx title Bits OS 0.15"), Ok(Cmd::Title { text: "Bits OS".into(), hold: Some(0.15) }));
        assert_eq!(cmd("fx demo ping 600 500 600 40"), Ok(Cmd::Demo(Demo::Ping([600, 500, 600, 40]))));
        assert_eq!(cmd("fx demo clear"), Ok(Cmd::DemoClear));
        assert_eq!(cmd("fx stop"), Ok(Cmd::Stop));
    }

    #[test]
    fn no_word_from_the_socket_gets_past_the_parser_unchecked() {
        for line in [
            "fx nonsense",
            "fx software",
            "fx software maybe",
            "fx freeze",
            "fx freeze NaN",
            "fx freeze inf",
            "fx freeze -inf",
            "fx freeze -1",
            "fx freeze 1e308",
            "fx freeze soon",
            "fx wait",
            "fx wait -1",
            "fx wait NaN",
            "fx wait 1.5",
            "fx hold",
            "fx hold NaN",
            "fx hold inf",
            "fx hold -inf",
            "fx hold 1e999",
            "fx hold x",
            "fx play",
            "fx turn",
            "fx play nothing",
            "fx play wipe NaN",
            "fx play wipe inf",
            "fx play fade x",
            "fx play morph",
            "fx play morph 1 2 3",
            "fx play morph 0 0 10 10 0 0 10",
            "fx play morph a b c d e f g h",
            "fx play morph NaN 0 10 10 0 0 10 10",
            "fx play morph 0 0 0 0 0 0 10 10",
            "fx play morph 0 0 10 10 0 0 0 0",
            "fx play morph -5 0 10 10 0 0 10 10",
            "fx play morph 0 0 -10 10 0 0 10 10",
            "fx play morph 0 0 10 10 2147483647 2147483647 2147483647 2147483647",
            "fx play morph -2147483648 -2147483648 2147483647 2147483647 0 0 10 10",
            "fx play morph 0 0 2147483647 10 0 0 10 10",
            "fx play morph 0 0 99999999999 10 0 0 10 10",
            "fx play morph 0 0 2561 10 0 0 10 10",
            "fx play morph 0 0 10 10 0 0 10 10 NaN",
            "fx title",
            "fx demo",
            "fx demo pulse",
            "fx demo pulse 1 2 3",
            "fx demo pulse a b c d",
            "fx demo pulse 0 0 0 0",
            "fx demo ping 0 0 -1 -1",
            "fx demo flow 2147483647 2147483647 2147483647 2147483647",
            "fx demo flow -2147483648 0 10 10",
            "fx demo nothing 0 0 10 10",
        ] {
            let got = cmd(line);
            assert!(got.is_err(), "{line:?} should be refused, got {got:?}");
        }
    }

    #[test]
    fn a_word_that_is_no_progress_stays_part_of_the_title() {
        assert_eq!(cmd("fx title Infinite inf"), Ok(Cmd::Title { text: "Infinite inf".into(), hold: None }));
        assert_eq!(cmd("fx title NaN"), Ok(Cmd::Title { text: "NaN".into(), hold: None }));
        assert_eq!(cmd("fx title 0.5"), Ok(Cmd::Title { text: "0.5".into(), hold: None }));
    }

    #[test]
    fn every_rectangle_the_parser_lets_through_stays_in_i32_arithmetic() {
        let (sw, sh) = SCREEN;
        for r in [[0, 0, 1, 1], [0, 0, sw, sh], [sw - 1, sh - 1, 1, 1]] {
            let words: Vec<String> = r.iter().map(|v| v.to_string()).collect();
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            assert_eq!(rect(&words, SCREEN), Some(r));
        }
        assert_eq!(rect(&["0", "0", "1"], SCREEN), None);
        assert_eq!(rect(&["0", "0", "1", "1", "1"], SCREEN), None);
    }
}
