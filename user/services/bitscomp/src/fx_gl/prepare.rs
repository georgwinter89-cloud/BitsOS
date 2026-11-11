//! WY-19: the work before a frame: the blurs whose stamp changed, a new transition's snapshot, the new scene as a
//! texture, the reveal's blur. Bottom element first; no RefCell borrow is held over a draw.

use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform};
use smithay::backend::renderer::{Bind, Color32F, Frame, ImportMem, Renderer, Texture};
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Size, Transform};

use super::{Blurred, Retry, Shared, err, new_tex, whole};
use crate::fx::{FxElement, Key, Kind, trans};
use crate::render::CompElement;

/// How long an effect whose work failed waits before it tries again, doubled per failure up to `RETRY_MOST`.
const RETRY_FIRST: Duration = Duration::from_millis(100);
const RETRY_MOST: Duration = Duration::from_secs(5);

/// Halves a rectangle's corners (the half-resolution pictures), 1 for none.
fn div_rect(r: Rectangle<i32, Physical>, by: i32) -> Rectangle<i32, Physical> {
    if by == 1 {
        return r;
    }
    let lo = |v: i32| v.div_euclid(by);
    let hi = |v: i32| (v + by - 1).div_euclid(by);
    let (x0, y0, x1, y1) = (lo(r.loc.x), lo(r.loc.y), hi(r.loc.x + r.size.w), hi(r.loc.y + r.size.h));
    Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())
}

/// Draws `elements` (topmost first) into `tex`: the part `rect` of the frame, at 1/`by` of its size.
fn draw_scene(
    r: &mut GlesRenderer,
    tex: &mut GlesTexture,
    rect: Rectangle<i32, Physical>,
    elements: &[CompElement<GlesRenderer>],
    scale: Scale<f64>,
    by: i32,
) -> Result<(), String> {
    let s = tex.size();
    let out: Size<i32, Physical> = (s.w, s.h).into();
    let mut fb = r.bind(tex).map_err(err)?;
    let mut frame = r.render(&mut fb, out, Transform::Normal).map_err(err)?;
    frame.clear(Color32F::from([0.0, 0.0, 0.0, 0.0]), &[Rectangle::from_size(out)]).map_err(err)?;
    for e in elements.iter().rev().filter(|e| !e.is_beam()) {
        let g = e.geometry(scale);
        let Some(part) = g.intersection(rect) else { continue };
        let mut at = g;
        at.loc -= rect.loc;
        let dst = div_rect(at, by);
        let mut seen = part;
        seen.loc -= g.loc;
        let inside = Rectangle::from_size(dst.size);
        let Some(dmg) = div_rect(seen, by).intersection(inside) else { continue };
        e.draw(&mut frame, e.src(), dst, &[dmg], &[]).map_err(err)?;
    }
    // No wait: the GL work runs in order and the frame's own bounded wait comes after all of it.
    let _ = frame.finish().map_err(err)?;
    Ok(())
}

/// One pass of the blur from `from` into `to` (same size).
fn pass(r: &mut GlesRenderer, prog: &GlesTexProgram, from: &GlesTexture, to: &mut GlesTexture, step: (f32, f32)) -> Result<(), String> {
    let s = to.size();
    let out: Size<i32, Physical> = (s.w, s.h).into();
    let all = Rectangle::from_size(out);
    let mut fb = r.bind(to).map_err(err)?;
    let mut frame = r.render(&mut fb, out, Transform::Normal).map_err(err)?;
    frame.clear(Color32F::from([0.0, 0.0, 0.0, 0.0]), &[all]).map_err(err)?;
    frame
        .render_texture_from_to(from, whole(from), all, &[all], &[], Transform::Normal, 1.0, Some(prog), &[Uniform::new("step_uv", step)])
        .map_err(err)?;
    // No wait: the GL work runs in order and the frame's own bounded wait comes after all of it.
    let _ = frame.finish().map_err(err)?;
    Ok(())
}

/// `tex` under a Gaussian of `sigma` of its own pixels: two passes through the scratch texture.
fn blur(r: &mut GlesRenderer, prog: &GlesTexProgram, tex: &mut GlesTexture, sigma: f32, scratch: &mut Option<GlesTexture>) -> Result<(), String> {
    let s = tex.size();
    if scratch.as_ref().is_none_or(|t| t.size() != s) {
        *scratch = Some(new_tex(r, s.w, s.h)?);
    }
    let Some(b) = scratch.as_mut() else { return Ok(()) };
    // A step is a quarter sigma (BLUR's 25 taps).
    let st = sigma / 4.0;
    pass(r, prog, tex, b, (st / s.w as f32, 0.0))?;
    pass(r, prog, b, tex, (0.0, st / s.h as f32))
}

/// The blur of `below`'s part `rect`, fresh for the stamp `under`, kept under `key`. While it fails, the blur it has stays
/// (a stale picture under a glass beats none) and so does the scratch texture.
fn blurred(
    r: &mut GlesRenderer,
    f: &FxElement,
    below: &[CompElement<GlesRenderer>],
    sigma: f32,
    scale: Scale<f64>,
) -> Result<(), String> {
    let hs = half_size(f);
    let (kept, prog, mut scratch) = {
        let Some(s) = r.egl_context().user_data().get::<Shared>() else { return Ok(()) };
        let g = s.0.borrow();
        let have = g.blurred.get(&f.key);
        if have.is_some_and(|b| b.under == f.under && b.tex.size() == hs) {
            return Ok(());
        }
        (have.map(|b| b.tex.clone()).filter(|t| t.size() == hs), g.prog.blur.clone(), g.scratch.clone())
    };
    let mut tex = match kept {
        Some(t) => t,
        None => new_tex(r, hs.w, hs.h)?,
    };
    let done = draw_scene(r, &mut tex, f.rect, below, scale, 2).and_then(|()| blur(r, &prog, &mut tex, sigma / 2.0, &mut scratch));
    if let Some(s) = r.egl_context().user_data().get::<Shared>() {
        let mut g = s.0.borrow_mut();
        g.scratch = scratch;
        if done.is_ok() {
            g.blurs += 1;
            g.blurred.insert(f.key, Blurred { under: f.under, tex });
        }
    }
    done
}

fn half_size(f: &FxElement) -> Size<i32, Buffer> {
    ((f.rect.size.w + 1) / 2, (f.rect.size.h + 1) / 2).into()
}

/// The element is waiting out its last failure.
fn waiting(r: &GlesRenderer, key: Key, now: Instant) -> bool {
    r.egl_context().user_data().get::<Shared>().is_some_and(|s| s.0.borrow().retry.get(&key).is_some_and(|t| now < t.not_before))
}

/// Notes how the element's work went: a failure is said once per element and waits ever longer before the next try, so
/// a lasting error costs neither a line nor the GPU work per frame; `ctl fx` counts them (`fails=`, `retrying=`).
fn settle(r: &GlesRenderer, key: Key, done: Result<(), String>, now: Instant) {
    let Some(s) = r.egl_context().user_data().get::<Shared>() else { return };
    let mut g = s.0.borrow_mut();
    match done {
        Ok(()) => {
            if g.retry.remove(&key).is_some() {
                eprintln!("bitscomp: effects: {key:?} works again");
            }
        }
        Err(e) => {
            g.fails += 1;
            match g.retry.get_mut(&key) {
                Some(t) => {
                    t.wait = (t.wait * 2).min(RETRY_MOST);
                    t.not_before = now + t.wait;
                }
                None => {
                    eprintln!("bitscomp: effects: {key:?}: {e} (that effect stays out or stale until it works; said once)");
                    g.retry.insert(key, Retry { wait: RETRY_FIRST, not_before: now + RETRY_FIRST });
                }
            }
        }
    }
}

/// The work before a frame, bottom element first (a glass over a glass sees the lower one finished): blurs whose stamp
/// changed, a new transition's snapshot (`last` copies the last frame on the screen into a texture), the new scene
/// for the transitions that want it, the reveal's blur. A failure stays with its element: the others still get theirs.
/// What no element of this frame uses goes back: every full-size picture ends with its transition, and a frame without
/// effects (the cheap level, a fullscreen window with motion off) holds no texture at all.
pub fn prepare(
    r: &mut GlesRenderer,
    elements: &[CompElement<GlesRenderer>],
    scale: Scale<f64>,
    output: (i32, i32),
    last: impl FnMut(&mut GlesRenderer, &mut GlesTexture) -> Result<(), String>,
) {
    prepare_at(r, elements, scale, output, last, Instant::now());
}

pub(super) fn prepare_at(
    r: &mut GlesRenderer,
    elements: &[CompElement<GlesRenderer>],
    scale: Scale<f64>,
    output: (i32, i32),
    mut last: impl FnMut(&mut GlesRenderer, &mut GlesTexture) -> Result<(), String>,
    now: Instant,
) {
    let mut seen = Vec::new();
    let (mut blurs, mut old, mut new, mut word) = (false, false, false, false);
    for i in (0..elements.len()).rev() {
        let CompElement::Fx(f) = &elements[i] else { continue };
        seen.push(f.key);
        match &f.kind {
            Kind::Glass { .. } | Kind::Halo { .. } => blurs = true,
            Kind::Snapshot { look, .. } => {
                old = true;
                new |= look.wants_live();
            }
            Kind::Reveal { .. } => {
                blurs = true;
                word = true;
            }
            _ => {}
        }
        if waiting(r, f.key, now) {
            continue;
        }
        let done = match &f.kind {
            Kind::Glass { sigma, .. } => blurred(r, f, &elements[i + 1..], *sigma, scale),
            Kind::Halo { sigma, .. } if i > 0 => blurred(r, f, &elements[i - 1..i], *sigma, scale),
            Kind::Snapshot { serial, look } => snapshot(r, *serial, output, &mut last).and_then(|()| {
                if look.wants_live() { live(r, f, &elements[i + 1..], scale) } else { Ok(()) }
            }),
            Kind::Reveal { image, sigma, .. } => reveal(r, f, image, *sigma),
            _ => Ok(()),
        };
        settle(r, f.key, done, now);
    }
    if let Some(s) = r.egl_context().user_data().get::<Shared>() {
        let mut g = s.0.borrow_mut();
        g.blurred.retain(|k, _| seen.contains(k));
        g.retry.retain(|k, _| seen.contains(k));
        if !blurs {
            g.scratch = None;
        }
        if !old {
            g.snapshot = None;
        }
        if !new {
            g.live = None;
        }
        if !word {
            g.title = None;
        }
    }
}

fn snapshot(r: &mut GlesRenderer, serial: u64, (w, h): (i32, i32), last: &mut impl FnMut(&mut GlesRenderer, &mut GlesTexture) -> Result<(), String>) -> Result<(), String> {
    let taken = {
        let Some(s) = r.egl_context().user_data().get::<Shared>() else { return Ok(()) };
        let mut g = s.0.borrow_mut();
        if g.snapshot.as_ref().is_some_and(|(have, _)| *have == serial) {
            return Ok(());
        }
        g.snapshot.take()
    };
    let mut tex = match taken {
        Some((_, t)) if t.size() == Size::from((w, h)) => t,
        _ => new_tex(r, w, h)?,
    };
    last(r, &mut tex)?;
    if let Some(s) = r.egl_context().user_data().get::<Shared>() {
        let mut g = s.0.borrow_mut();
        g.snapshot = Some((serial, tex));
        g.snapshots += 1;
    }
    Ok(())
}

fn live(r: &mut GlesRenderer, f: &FxElement, below: &[CompElement<GlesRenderer>], scale: Scale<f64>) -> Result<(), String> {
    let (w, h) = (f.rect.size.w, f.rect.size.h);
    let taken = {
        let Some(s) = r.egl_context().user_data().get::<Shared>() else { return Ok(()) };
        s.0.borrow_mut().live.take()
    };
    let mut tex = match taken {
        Some(t) if t.size() == Size::from((w, h)) => t,
        _ => new_tex(r, w, h)?,
    };
    draw_scene(r, &mut tex, f.rect, below, scale, 1)?;
    if let Some(s) = r.egl_context().user_data().get::<Shared>() {
        let mut g = s.0.borrow_mut();
        g.live = Some(tex);
        g.lives += 1;
    }
    Ok(())
}

fn reveal(r: &mut GlesRenderer, f: &FxElement, image: &trans::Image, sigma: f32) -> Result<(), String> {
    let hs: Size<i32, Buffer> = ((image.w + 1) / 2, (image.h + 1) / 2).into();
    let (src, prog, mut scratch, kept) = {
        let Some(s) = r.egl_context().user_data().get::<Shared>() else { return Ok(()) };
        let g = s.0.borrow();
        let have = g.title.as_ref().filter(|(have, _)| *have == image.serial).map(|(_, t)| t.clone());
        (have, g.prog.blur.clone(), g.scratch.clone(), g.blurred.get(&f.key).map(|b| b.tex.clone()).filter(|t| t.size() == hs))
    };
    let src = match src {
        Some(t) => t,
        None => {
            let t = r.import_memory(&image.bytes, Fourcc::Argb8888, (image.w, image.h).into(), false).map_err(err)?;
            if let Some(s) = r.egl_context().user_data().get::<Shared>() {
                s.0.borrow_mut().title = Some((image.serial, t.clone()));
            }
            t
        }
    };
    let mut tex = match kept {
        Some(t) => t,
        None => new_tex(r, hs.w, hs.h)?,
    };
    let done = copy(r, &src, &mut tex).and_then(|()| if sigma > 0.5 { blur(r, &prog, &mut tex, sigma / 2.0, &mut scratch) } else { Ok(()) });
    if let Some(s) = r.egl_context().user_data().get::<Shared>() {
        let mut g = s.0.borrow_mut();
        g.scratch = scratch;
        if done.is_ok() {
            g.reveals += 1;
            g.blurred.insert(f.key, Blurred { under: 0, tex });
        }
    }
    done
}

/// `from` scaled into all of `to`.
fn copy(r: &mut GlesRenderer, from: &GlesTexture, to: &mut GlesTexture) -> Result<(), String> {
    let s = to.size();
    let out: Size<i32, Physical> = (s.w, s.h).into();
    let all = Rectangle::from_size(out);
    let mut fb = r.bind(to).map_err(err)?;
    let mut frame = r.render(&mut fb, out, Transform::Normal).map_err(err)?;
    frame.clear(Color32F::from([0.0, 0.0, 0.0, 0.0]), &[all]).map_err(err)?;
    frame.render_texture_from_to(from, whole(from), all, &[all], &[], Transform::Normal, 1.0, None, &[]).map_err(err)?;
    let _ = frame.finish().map_err(err)?;
    Ok(())
}
