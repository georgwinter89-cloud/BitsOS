//! WY-19: the effect level on the GLES renderer (gpu build only): the shaders of `shaders.rs`, compiled once per EGL
//! context and kept in its user data, the work before a frame (`prepare`: blurs, snapshots, the new scene as a
//! texture) and each element's draw. Only Smithay's renderer calls, no raw GL: its GL state stays its own. Blurs run
//! at half resolution in two passes and only when what lies under a glass changed (`fx::seal`'s stamp), so a still
//! desktop costs one texture draw per damaged glass. All of it is GPU work inside the frame's bounded wait (`GPU_WAIT`).

mod draw;
mod prepare;
mod shaders;
#[cfg(test)]
mod tests;

pub use draw::draw;
pub use prepare::prepare;

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesError, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, UniformName, UniformType};
use smithay::backend::renderer::{Offscreen, Texture};
use smithay::utils::{Buffer, Rectangle, Size};

use crate::fx::Key;

struct Programs {
    shadow: GlesPixelProgram,
    beam: GlesPixelProgram,
    scan: GlesPixelProgram,
    radar: GlesPixelProgram,
    ring: GlesPixelProgram,
    dot: GlesPixelProgram,
    flow: GlesPixelProgram,
    blur: GlesTexProgram,
    glass: GlesTexProgram,
    wipe: GlesTexProgram,
    glitch: GlesTexProgram,
    card: GlesTexProgram,
}

/// A blur kept for its element while the stamp stays.
struct Blurred {
    under: u64,
    tex: GlesTexture,
}

/// An element whose work failed: when it may try again.
struct Retry {
    wait: Duration,
    not_before: Instant,
}

/// Everything the level holds per EGL context.
struct FxGl {
    prog: Programs,
    blurred: HashMap<Key, Blurred>,
    scratch: Option<GlesTexture>,
    /// The old picture of the transition `.0`.
    snapshot: Option<(u64, GlesTexture)>,
    /// The new scene while a transition needs it as a texture.
    live: Option<GlesTexture>,
    /// The picture of the reveal `.0`, unblurred.
    title: Option<(u64, GlesTexture)>,
    /// The elements whose work failed and did not work since.
    retry: HashMap<Key, Retry>,
    /// How often an element's work failed.
    fails: u64,
    blurs: u64,
    snapshots: u64,
    lives: u64,
    reveals: u64,
}

struct Shared(RefCell<FxGl>);
/// The shaders of this context did not compile: why (asked once, not every frame).
struct Failed(String);

fn names(list: &[(&'static str, UniformType)]) -> Vec<UniformName<'static>> {
    list.iter().map(|(n, t)| UniformName::new(*n, *t)).collect()
}

fn compile(r: &mut GlesRenderer) -> Result<Programs, String> {
    use UniformType::{_1f as F, _2f as V2, _3f as V3, _4f as V4};
    let mut pixel = |what: &str, src: &str, list: &[(&'static str, UniformType)]| {
        r.compile_custom_pixel_shader(src, &names(list)).map_err(|e| format!("shader {what}: {e}"))
    };
    let shadow = pixel(
        "shadow",
        shaders::SHADOW,
        &[("bx", V4), ("hole", V4), ("radius", F), ("hole_radius", F), ("sigma", F), ("color", V4)],
    )?;
    let beam = pixel("beam", shaders::BEAM, &[("color", V3), ("mid", F), ("peak", F)])?;
    let scan = pixel("scan", shaders::SCAN, &[("period", F), ("color", V4)])?;
    let radar = pixel("radar", shaders::RADAR, &[("angle", F), ("accent", V3), ("ground", V4)])?;
    let ring = pixel(
        "ring",
        shaders::RING,
        &[("r", F), ("width", F), ("on", F), ("off", F), ("phase", F), ("glow", F), ("color", V4)],
    )?;
    let dot = pixel("dot", shaders::DOT, &[("r", F), ("stroke", F), ("glow", F), ("color", V4)])?;
    let flow = pixel(
        "flow",
        shaders::FLOW,
        &[("a", V2), ("b", V2), ("width", F), ("on", F), ("off", F), ("offset", F), ("glow", F), ("color", V4)],
    )?;
    let mut tex = |what: &str, src: &str, list: &[(&'static str, UniformType)]| {
        r.compile_custom_texture_shader(src, &names(list)).map_err(|e| format!("shader {what}: {e}"))
    };
    let blur = tex("blur", shaders::BLUR, &[("step_uv", V2)])?;
    let glass = tex("glass", shaders::GLASS, &[("uv", V4), ("size_px", V2), ("radius", F), ("saturate", F)])?;
    let wipe = tex("wipe", shaders::WIPE, &[("centre", V2), ("r", F), ("size_px", V2)])?;
    let glitch = tex(
        "glitch",
        shaders::GLITCH,
        &[("size_px", V2), ("blur", F), ("dx", F), ("skew", F), ("hue", F)],
    )?;
    let card = tex(
        "card",
        shaders::CARD,
        &[("size_px", V2), ("turn", F), ("persp", F), ("scale", F), ("shift", F), ("radius", F)],
    )?;
    Ok(Programs { shadow, beam, scan, radar, ring, dot, flow, blur, glass, wipe, glitch, card })
}

/// The shaders of this context, compiled at the first call; why not, every later call without trying again.
pub fn ready(r: &mut GlesRenderer) -> Result<(), String> {
    let data = r.egl_context().user_data();
    if data.get::<Shared>().is_some() {
        return Ok(());
    }
    if let Some(f) = data.get::<Failed>() {
        return Err(f.0.clone());
    }
    let t0 = std::time::Instant::now();
    match compile(r) {
        Ok(prog) => {
            eprintln!("bitscomp: effects: shaders compiled in {} ms", t0.elapsed().as_millis());
            let gl = FxGl { prog, blurred: HashMap::new(), scratch: None, snapshot: None, live: None, title: None, retry: HashMap::new(), fails: 0, blurs: 0, snapshots: 0, lives: 0, reveals: 0 };
            r.egl_context().user_data().insert_if_missing(|| Shared(RefCell::new(gl)));
            Ok(())
        }
        Err(e) => {
            let why = e.clone();
            r.egl_context().user_data().insert_if_missing(|| Failed(why));
            Err(e)
        }
    }
}

/// For `ctl fx`: how much GPU work the level asked for so far.
pub fn report(r: &GlesRenderer) -> String {
    match r.egl_context().user_data().get::<Shared>() {
        Some(s) => {
            let g = s.0.borrow();
            let held = g.blurred.len() + [g.scratch.is_some(), g.snapshot.is_some(), g.live.is_some(), g.title.is_some()].iter().filter(|b| **b).count();
            format!(
                "blurs={} snapshots={} lives={} reveals={} kept={} textures={held} fails={} retrying={}",
                g.blurs,
                g.snapshots,
                g.lives,
                g.reveals,
                g.blurred.len(),
                g.fails,
                g.retry.len()
            )
        }
        None => "blurs=0 snapshots=0 lives=0 reveals=0 kept=0 textures=0 fails=0 retrying=0".to_string(),
    }
}

fn err(e: GlesError) -> String {
    e.to_string()
}

fn new_tex(r: &mut GlesRenderer, w: i32, h: i32) -> Result<GlesTexture, String> {
    Offscreen::<GlesTexture>::create_buffer(r, Fourcc::Abgr8888, (w.max(1), h.max(1)).into()).map_err(err)
}

fn whole(tex: &GlesTexture) -> Rectangle<f64, Buffer> {
    let s: Size<i32, Buffer> = tex.size();
    Rectangle::from_size((s.w as f64, s.h as f64).into())
}
