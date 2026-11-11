//! WY-19: one effect into the frame under way (also inside `prepare`, for what lies under a glass).

use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesPixelProgram, GlesTexture, Uniform};
use smithay::backend::renderer::{Color32F, Texture};
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};

use super::{Shared, whole};
use crate::fx::trans::{self, Look};
use crate::fx::{FxElement, Kind};

/// Draws one effect into the frame under way.
pub fn draw(frame: &mut GlesFrame<'_, '_>, f: &FxElement, dst: Rectangle<i32, Physical>, damage: &[Rectangle<i32, Physical>]) -> Result<(), GlesError> {
    let Some(shared) = frame.egl_context().user_data().get::<Shared>() else { return Ok(()) };
    let size: Size<i32, Buffer> = (f.rect.size.w, f.rect.size.h).into();
    let src = Rectangle::from_size((size.w as f64, size.h as f64).into());
    // Only clones go on from here: no borrow is held over a draw.
    let g = shared.0.borrow();
    let p = &g.prog;
    let pixel = |prog: GlesPixelProgram, frame: &mut GlesFrame<'_, '_>, u: &[Uniform<'_>]| frame.render_pixel_shader_to(&prog, src, dst, size, Some(damage), 1.0, u);
    match &f.kind {
        Kind::Shadow { bx, radius, sigma, color, hole, hole_radius } => {
            let prog = p.shadow.clone();
            drop(g);
            let u = [
                Uniform::new("bx", *bx),
                Uniform::new("hole", *hole),
                Uniform::new("radius", *radius),
                Uniform::new("hole_radius", *hole_radius),
                Uniform::new("sigma", *sigma),
                Uniform::new("color", *color),
            ];
            pixel(prog, frame, &u)
        }
        Kind::Beam { color, peak, mid } => {
            let prog = p.beam.clone();
            drop(g);
            pixel(prog, frame, &[Uniform::new("color", *color), Uniform::new("mid", *mid), Uniform::new("peak", *peak)])
        }
        Kind::Scan { period, color } => {
            let prog = p.scan.clone();
            drop(g);
            pixel(prog, frame, &[Uniform::new("period", *period), Uniform::new("color", *color)])
        }
        Kind::Radar { angle, accent, ground } => {
            let prog = p.radar.clone();
            drop(g);
            pixel(prog, frame, &[Uniform::new("angle", *angle), Uniform::new("accent", *accent), Uniform::new("ground", *ground)])
        }
        Kind::Ring { r, width, on, off, phase, color, glow } => {
            let prog = p.ring.clone();
            drop(g);
            let u = [
                Uniform::new("r", *r),
                Uniform::new("width", *width),
                Uniform::new("on", *on),
                Uniform::new("off", *off),
                Uniform::new("phase", *phase),
                Uniform::new("glow", *glow),
                Uniform::new("color", *color),
            ];
            pixel(prog, frame, &u)
        }
        Kind::Dot { r, stroke, color, glow } => {
            let prog = p.dot.clone();
            drop(g);
            pixel(prog, frame, &[Uniform::new("r", *r), Uniform::new("stroke", *stroke), Uniform::new("glow", *glow), Uniform::new("color", *color)])
        }
        Kind::Flow { a, b, width, on, off, offset, color, glow } => {
            let prog = p.flow.clone();
            drop(g);
            let u = [
                Uniform::new("a", *a),
                Uniform::new("b", *b),
                Uniform::new("width", *width),
                Uniform::new("on", *on),
                Uniform::new("off", *off),
                Uniform::new("offset", *offset),
                Uniform::new("glow", *glow),
                Uniform::new("color", *color),
            ];
            pixel(prog, frame, &u)
        }
        Kind::Glass { radius, saturate, .. } => {
            let Some(tex) = g.blurred.get(&f.key).map(|b| b.tex.clone()) else { return Ok(()) };
            let prog = p.glass.clone();
            drop(g);
            let u = [
                Uniform::new("uv", [0.0f32, 0.0, 1.0, 1.0]),
                Uniform::new("size_px", (f.rect.size.w as f32, f.rect.size.h as f32)),
                Uniform::new("radius", *radius),
                Uniform::new("saturate", *saturate),
            ];
            frame.render_texture_from_to(&tex, whole(&tex), dst, damage, &[], Transform::Normal, 1.0, Some(&prog), &u)
        }
        Kind::Halo { gain, .. } => {
            let Some(tex) = g.blurred.get(&f.key).map(|b| b.tex.clone()) else { return Ok(()) };
            drop(g);
            frame.render_texture_from_to(&tex, whole(&tex), dst, damage, &[], Transform::Normal, *gain, None, &[])
        }
        Kind::Reveal { alpha, .. } => {
            let Some(tex) = g.blurred.get(&f.key).map(|b| b.tex.clone()) else { return Ok(()) };
            drop(g);
            frame.render_texture_from_to(&tex, whole(&tex), dst, damage, &[], Transform::Normal, *alpha, None, &[])
        }
        Kind::Snapshot { look, serial } => {
            // Only this transition's own old picture: its snapshot may have failed (`prepare`).
            let Some(old) = g.snapshot.as_ref().filter(|(have, _)| have == serial).map(|(_, t)| t.clone()) else { return Ok(()) };
            let new = g.live.clone();
            let (wipe, glitch, card, glass) = (p.wipe.clone(), p.glitch.clone(), p.card.clone(), p.glass.clone());
            drop(g);
            let full = (f.rect.size.w as f32, f.rect.size.h as f32);
            match *look {
                Look::Wipe { cx, cy, r } => {
                    let u = [Uniform::new("centre", (cx, cy)), Uniform::new("r", r), Uniform::new("size_px", full)];
                    frame.render_texture_from_to(&old, whole(&old), dst, damage, &[], Transform::Normal, 1.0, Some(&wipe), &u)
                }
                Look::Fade { alpha } => frame.render_texture_from_to(&old, whole(&old), dst, damage, &[], Transform::Normal, alpha, None, &[]),
                Look::Glitch { alpha, blur, dx, skew, hue } => {
                    frame.render_texture_from_to(&old, whole(&old), dst, damage, &[], Transform::Normal, 1.0, None, &[])?;
                    let Some(new) = new else { return Ok(()) };
                    let u = [
                        Uniform::new("size_px", full),
                        Uniform::new("blur", blur),
                        Uniform::new("dx", dx),
                        Uniform::new("skew", skew),
                        Uniform::new("hue", hue),
                    ];
                    frame.render_texture_from_to(&new, whole(&new), dst, damage, &[], Transform::Normal, alpha, Some(&glitch), &u)
                }
                Look::Carousel { off, ground, persp, radius } => {
                    frame.draw_solid(dst, damage, Color32F::from(ground))?;
                    let card_of = |frame: &mut GlesFrame<'_, '_>, tex: &GlesTexture, o: f32| {
                        let (share, scale, turn) = trans::card(o);
                        let u = [
                            Uniform::new("size_px", full),
                            Uniform::new("turn", turn),
                            Uniform::new("persp", persp),
                            Uniform::new("scale", scale),
                            Uniform::new("shift", share * full.0),
                            Uniform::new("radius", radius),
                        ];
                        frame.render_texture_from_to(tex, whole(tex), dst, damage, &[], Transform::Normal, 1.0, Some(&card), &u)
                    };
                    card_of(frame, &old, -off)?;
                    match new {
                        Some(new) => card_of(frame, &new, 1.0 - off),
                        None => Ok(()),
                    }
                }
                Look::Morph { src, radius, alpha } => {
                    let ts = old.size();
                    let from = Rectangle::new((src[0] as f64, src[1] as f64).into(), (src[2] as f64, src[3] as f64).into());
                    let uv = [src[0] / ts.w as f32, src[1] / ts.h as f32, src[2] / ts.w as f32, src[3] / ts.h as f32];
                    let u = [
                        Uniform::new("uv", uv),
                        Uniform::new("size_px", (f.rect.size.w as f32, f.rect.size.h as f32)),
                        Uniform::new("radius", radius),
                        Uniform::new("saturate", 1.0f32),
                    ];
                    frame.render_texture_from_to(&old, from, dst, damage, &[], Transform::Normal, alpha, Some(&glass), &u)
                }
            }
        }
    }
}
