//! WY-19 against a real GLES context (the host's Mesa, the gate's "bitscomp gpu tests"): every shader compiles and
//! draws what it should, at the right place and the right way up. Pixel probes, not reference pictures: QEMU and the
//! scenarios compare whole screens (host/tests/wy/89-effects.sh, 46-v2-effects.sh).

use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind as K;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::Offscreen;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Scale, Transform};

use crate::fx::trans::{Image, Look};
use crate::fx::{Fx, GPU_WAIT_CUT_MS, Key, Kind, seal};
use crate::render::{CompElement, CompRenderer};
use crate::render_gl::GlesPicture;

type E = CompElement<GlesRenderer>;

const W: i32 = 200;
const H: i32 = 200;

struct Gl {
    r: GlesRenderer,
    target: GlesPicture,
    tracker: OutputDamageTracker,
    fx: Fx,
}

impl Gl {
    fn new() -> Gl {
        let mut r = GlesRenderer::create().expect("a GLES context from the host's Mesa");
        super::ready(&mut r).expect("every shader compiles");
        let target = r.target(W, H).expect("a target");
        Gl { r, target, tracker: OutputDamageTracker::new((W, H), 1.0, Transform::Normal), fx: Fx::default() }
    }

    /// Topmost first, like a frame: the stamps, the work before the frame, the frame.
    fn draw(&mut self, mut v: Vec<E>) {
        seal(&mut self.fx, &mut v, Scale::from(1.0));
        self.r.fx_prepare(&mut self.target, &v, Scale::from(1.0));
        self.r.draw(&mut self.target, &mut self.tracker, &v, [0.0, 0.0, 0.0, 1.0].into()).expect("the frame draws");
    }

    fn at(&self, x: i32, y: i32) -> [i32; 3] {
        let (px, stride) = GlesRenderer::pixels(&self.target);
        let p = px[y as usize * stride + x as usize];
        [(p >> 16 & 255) as i32, (p >> 8 & 255) as i32, (p & 255) as i32]
    }

    fn fx(&mut self, key: Key, rect: [i32; 4], kind: Kind) -> E {
        self.fx.element(key, rect, kind).into()
    }

    /// A picture made of `f(x, y)` = 0xAARRGGBB, W x H.
    fn picture(&mut self, buf: &MemoryRenderBuffer) -> E {
        MemoryRenderBufferRenderElement::from_buffer(&mut self.r, (0.0, 0.0), buf, None, None, None, K::Unspecified).expect("the picture").into()
    }
}

fn buffer(f: impl Fn(i32, i32) -> u32) -> MemoryRenderBuffer {
    let mut bytes = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            bytes.extend_from_slice(&f(x, y).to_le_bytes());
        }
    }
    MemoryRenderBuffer::from_slice(&bytes, Fourcc::Argb8888, (W, H), 1, Transform::Normal, None)
}

fn solid(buf: &SolidColorBuffer) -> E {
    SolidColorRenderElement::from_buffer(buf, (0, 0), 1.0, 1.0, K::Unspecified).into()
}

fn near(a: [i32; 3], b: [i32; 3], tol: i32) -> bool {
    (0..3).all(|i| (a[i] - b[i]).abs() <= tol)
}

fn lum(p: [i32; 3]) -> i32 {
    p[0] + p[1] + p[2]
}

const RED: u32 = 0xffc80000;
const BLUE: u32 = 0xff0000c8;

/// One context for all, in a process of its own: with `LIBGL_ALWAYS_SOFTWARE` it is llvmpipe whatever GPU the host
/// has (the host's GPU is its user's), and no thread of the other tests sees the environment change.
#[test]
fn every_effect_draws_what_it_should() {
    if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
        let me = std::env::current_exe().expect("the test binary");
        let out = std::process::Command::new(me)
            .args(["--exact", "fx_gl::tests::every_effect_draws_what_it_should", "--nocapture"])
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .output()
            .expect("the test binary runs");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("llvmpipe"), "it must run on llvmpipe: {text}");
        return;
    }
    let mut gl = Gl::new();
    shadow_lies_below_and_outside_the_box(&mut gl);
    glass_blurs_keeps_its_corners_and_stays_upright(&mut gl);
    glass_blurs_once_until_what_is_under_it_changes(&mut gl);
    glass_saturates(&mut gl);
    halo_blurs_the_element_above_it(&mut gl);
    beam_and_scan(&mut gl);
    radar_sweeps_clockwise_behind_its_angle(&mut gl);
    ring_turns_with_its_phase(&mut gl);
    dot_and_flow(&mut gl);
    transitions_show_the_last_frame_the_right_way_up(&mut gl);
    reveal_comes_out_of_its_blur(&mut gl);
    a_failure_stays_with_its_element_and_waits_ever_longer(&mut gl);
    a_glass_that_cannot_blur_keeps_the_picture_it_has(&mut gl);
    a_failed_snapshot_leaves_the_new_scene(&mut gl);
    what_no_element_uses_goes_back(&mut gl);
    a_late_frame_comes_back_at_once_and_the_next_one_shows_it(&mut gl);
}

fn fresh(gl: &mut Gl) {
    gl.tracker = OutputDamageTracker::new((W, H), 1.0, Transform::Normal);
}

fn shadow_lies_below_and_outside_the_box(gl: &mut Gl) {
    fresh(gl);
    let white = SolidColorBuffer::new((W, H), [1.0, 1.0, 1.0, 1.0]);
    // A box 60..140, the shadow 12 px lower, sigma 8.
    let kind = Kind::Shadow { bx: [40.0, 40.0, 80.0, 80.0], radius: 8.0, sigma: 8.0, color: [0.0, 0.0, 0.0, 0.8], hole: [40.0, 28.0, 80.0, 80.0], hole_radius: 8.0 };
    let s = gl.fx(("shadow", 0), [20, 32, 160, 160], kind);
    gl.draw(vec![s, solid(&white)]);
    assert!(near(gl.at(100, 100), [255, 255, 255], 2), "nothing inside the panel's own box: {:?}", gl.at(100, 100));
    let below = lum(gl.at(100, 146));
    let above = lum(gl.at(100, 52));
    assert!(below < 450, "dark under the box: {below}");
    assert!(above > below + 200, "the shadow is lower than the box, below it and not above it ({above} / {below})");
    assert!(near(gl.at(100, 185), [255, 255, 255], 8), "and it fades out");
}

fn glass_blurs_keeps_its_corners_and_stays_upright(gl: &mut Gl) {
    fresh(gl);
    // Stripes every 8 px in the top half, blue in the bottom half.
    let stripes = buffer(|x, y| if y >= 100 { BLUE } else if (x / 4) % 2 == 0 { 0xffffffff } else { 0xff000000 });
    let bg = gl.picture(&stripes);
    let glass = gl.fx(("glass", 1), [40, 40, 120, 120], Kind::Glass { radius: 20.0, sigma: 8.0, saturate: 1.0 });
    gl.draw(vec![glass, bg]);
    let (a, b) = (lum(gl.at(100, 60)), lum(gl.at(104, 60)));
    assert!((a - b).abs() < 90, "the stripes are gone under the glass: {a} / {b}");
    assert!(lum(gl.at(10, 60)) > 700 || lum(gl.at(10, 60)) < 30, "and stay beside it");
    assert!((lum(gl.at(14, 60)) - lum(gl.at(10, 60))).abs() > 600, "beside it they are sharp");
    let corner = gl.at(41, 41);
    assert!(lum(corner) > 700 || lum(corner) < 30, "the rounded corner leaves the sharp ground: {corner:?}");
    let low = gl.at(100, 150);
    assert!(low[2] > 180 && low[0] < 30, "the bottom half is still blue at the bottom: upright ({low:?})");
    let high = gl.at(100, 50);
    assert!(high[0] > 60 && (high[0] - high[2]).abs() < 40, "and the top half grey at the top: {high:?}");
}

fn glass_blurs_once_until_what_is_under_it_changes(gl: &mut Gl) {
    fresh(gl);
    let count = |gl: &Gl| -> u64 {
        let line = super::report(&gl.r);
        line.split_whitespace().find_map(|w| w.strip_prefix("blurs=")).and_then(|v| v.parse().ok()).expect("blurs= in the report")
    };
    let mut under = SolidColorBuffer::new((W, H), [1.0, 0.0, 0.0, 1.0]);
    let g = gl.fx(("glass", 2), [40, 40, 120, 120], Kind::Glass { radius: 0.0, sigma: 8.0, saturate: 1.0 });
    gl.draw(vec![g, solid(&under)]);
    assert_eq!(gl.at(100, 100)[0], 255);
    let n = count(gl);
    for _ in 0..3 {
        let g = gl.fx(("glass", 2), [40, 40, 120, 120], Kind::Glass { radius: 0.0, sigma: 8.0, saturate: 1.0 });
        gl.draw(vec![g, solid(&under)]);
    }
    assert_eq!(count(gl), n, "a still picture is not blurred again");
    under.set_color([0.0, 0.0, 1.0, 1.0]);
    let g = gl.fx(("glass", 2), [40, 40, 120, 120], Kind::Glass { radius: 0.0, sigma: 8.0, saturate: 1.0 });
    gl.draw(vec![g, solid(&under)]);
    assert_eq!(count(gl), n + 1, "a change under it is");
    assert!(gl.at(100, 100)[2] > 240 && gl.at(100, 100)[0] < 10, "and shows at once: {:?}", gl.at(100, 100));
}

fn glass_saturates(gl: &mut Gl) {
    fresh(gl);
    let under = SolidColorBuffer::new((W, H), [0.6, 0.4, 0.4, 1.0]);
    let g = gl.fx(("glass", 3), [40, 40, 120, 120], Kind::Glass { radius: 0.0, sigma: 4.0, saturate: 2.0 });
    gl.draw(vec![g, solid(&under)]);
    let (inside, out) = (gl.at(100, 100), gl.at(10, 10));
    assert!(inside[0] - inside[1] > out[0] - out[1] + 20, "saturate(2) spreads the channels: {inside:?} against {out:?}");
}

fn halo_blurs_the_element_above_it(gl: &mut Gl) {
    fresh(gl);
    let ground = SolidColorBuffer::new((W, H), [0.0, 0.0, 0.0, 1.0]);
    let mark = SolidColorBuffer::new((40, 40), [0.0, 1.0, 0.0, 1.0]);
    let sprite: E = SolidColorRenderElement::from_buffer(&mark, (80, 80), 1.0, 1.0, K::Unspecified).into();
    let h = gl.fx(("halo", 0), [50, 50, 100, 100], Kind::Halo { sigma: 8.0, gain: 1.0 });
    gl.draw(vec![sprite, h, solid(&ground)]);
    assert_eq!(gl.at(100, 100)[1], 255, "the sprite itself stays sharp over its halo");
    assert!(gl.at(76, 100)[1] > 40, "a glow just outside it: {:?}", gl.at(76, 100));
    assert!(gl.at(55, 100)[1] < gl.at(76, 100)[1], "fading away");
}

fn beam_and_scan(gl: &mut Gl) {
    fresh(gl);
    let ground = SolidColorBuffer::new((W, H), [0.0, 0.0, 0.0, 1.0]);
    let b = gl.fx(("beam", 0), [0, 50, W, 100], Kind::Beam { color: [1.0, 1.0, 1.0], peak: 0.5, mid: 0.1 });
    gl.draw(vec![b, solid(&ground)]);
    assert!(gl.at(100, 52)[0] < 10, "the band starts at nothing: {:?}", gl.at(100, 52));
    assert!(gl.at(100, 145)[0] > 90, "and ends strong at its lower edge: {:?}", gl.at(100, 145));
    assert_eq!(gl.at(100, 155)[0], 0, "under it nothing");
    fresh(gl);
    let s = gl.fx(("scan", 0), [0, 0, W, H], Kind::Scan { period: 4.0, color: [0.5, 0.5, 0.5, 0.5] });
    gl.draw(vec![s, solid(&ground)]);
    let rows: Vec<i32> = (0..8).map(|y| gl.at(5, y)[0]).collect();
    assert_eq!(rows.iter().filter(|v| **v > 100).count(), 2, "one row in four: {rows:?}");
    assert!(rows[0] > 100 && rows[4] > 100, "from the top: {rows:?}");
}

fn radar_sweeps_clockwise_behind_its_angle(gl: &mut Gl) {
    fresh(gl);
    let ground = SolidColorBuffer::new((W, H), [0.0, 0.0, 0.0, 1.0]);
    let k = Kind::Radar { angle: 0.0, accent: [0.0, 1.0, 0.0], ground: [0.0, 0.0, 0.0, 0.6] };
    let rd = gl.fx(("radar", 0), [0, 0, W, H], k);
    gl.draw(vec![rd, solid(&ground)]);
    let (cx, top) = (100, 30);
    // Straight up is 0 deg: the strongest part lies just before it, counter-clockwise, nothing just after it.
    let left = gl.at(cx - 12, top)[1];
    let right = gl.at(cx + 12, top)[1];
    assert!(left > right + 40, "bright behind the angle, dark ahead of it: {left} / {right}");
    assert_eq!(gl.at(2, 2), [0, 0, 0], "a disc: nothing in the corner");
    fresh(gl);
    let k = Kind::Radar { angle: std::f32::consts::FRAC_PI_2, accent: [0.0, 1.0, 0.0], ground: [0.0, 0.0, 0.0, 0.6] };
    let rd = gl.fx(("radar", 1), [0, 0, W, H], k);
    gl.draw(vec![rd, solid(&ground)]);
    let above = gl.at(170, 100 - 12)[1];
    let below = gl.at(170, 100 + 12)[1];
    assert!(above > below + 40, "turned a quarter clockwise it points right, y going down: {above} / {below}");
}

fn ring_turns_with_its_phase(gl: &mut Gl) {
    let ground = SolidColorBuffer::new((W, H), [0.0, 0.0, 0.0, 1.0]);
    // One dash of half a turn from straight up, clockwise.
    let turn = std::f32::consts::TAU * 60.0;
    let ring = |phase: f32| Kind::Ring { r: 60.0, width: 6.0, on: turn / 2.0, off: turn / 2.0, phase, color: [0.0, 1.0, 0.0, 1.0], glow: 0.0 };
    fresh(gl);
    let e = gl.fx(("ring", 0), [0, 0, W, H], ring(0.0));
    gl.draw(vec![e, solid(&ground)]);
    assert_eq!(gl.at(160, 100)[1], 255, "the dash runs clockwise through 3 o'clock");
    assert_eq!(gl.at(40, 100)[1], 0, "and not through 9 o'clock");
    fresh(gl);
    let e = gl.fx(("ring", 1), [0, 0, W, H], ring(0.5));
    gl.draw(vec![e, solid(&ground)]);
    assert_eq!(gl.at(160, 100)[1], 0);
    assert_eq!(gl.at(40, 100)[1], 255, "half a turn later it is on the other side");
}

fn dot_and_flow(gl: &mut Gl) {
    fresh(gl);
    let ground = SolidColorBuffer::new((W, H), [0.0, 0.0, 0.0, 1.0]);
    let d = gl.fx(("dot", 0), [50, 50, 100, 100], Kind::Dot { r: 20.0, stroke: 0.0, color: [1.0, 0.0, 0.0, 1.0], glow: 0.0 });
    gl.draw(vec![d, solid(&ground)]);
    assert_eq!(gl.at(100, 100)[0], 255);
    assert_eq!(gl.at(125, 100)[0], 0);
    fresh(gl);
    let d = gl.fx(("dot", 1), [50, 50, 100, 100], Kind::Dot { r: 20.0, stroke: 4.0, color: [1.0, 0.0, 0.0, 1.0], glow: 0.0 });
    gl.draw(vec![d, solid(&ground)]);
    assert_eq!(gl.at(100, 100)[0], 0, "a line leaves the middle free");
    assert_eq!(gl.at(120, 100)[0], 255);
    fresh(gl);
    let flow = |offset: f32| Kind::Flow { a: [0.0, 100.0], b: [200.0, 100.0], width: 4.0, on: 10.0, off: 10.0, offset, color: [0.0, 1.0, 0.0, 1.0], glow: 1.0 };
    let f = gl.fx(("flow", 0), [0, 0, W, H], flow(0.0));
    gl.draw(vec![f, solid(&ground)]);
    assert_eq!(gl.at(5, 100)[1], 255, "a dash");
    assert!(gl.at(15, 100)[1] < 40, "a gap");
    fresh(gl);
    let f = gl.fx(("flow", 1), [0, 0, W, H], flow(10.0));
    gl.draw(vec![f, solid(&ground)]);
    assert!(gl.at(5, 100)[1] < 40, "ten px of flow later a gap is there");
    assert_eq!(gl.at(15, 100)[1], 255);
}

fn transitions_show_the_last_frame_the_right_way_up(gl: &mut Gl) {
    let old = buffer(|_, y| if y < 100 { RED } else { 0xff00c800 });
    let new = SolidColorBuffer::new((W, H), [0.0, 0.0, 1.0, 1.0]);
    let snap = |gl: &mut Gl, serial: u64, rect: [i32; 4], look: Look| gl.fx(("transition", 0), rect, Kind::Snapshot { serial, look });
    let one = |gl: &mut Gl, serial: u64, rect: [i32; 4], look: Look| {
        // The frame before: the old picture.
        fresh(gl);
        let e = gl.picture(&old);
        gl.draw(vec![e]);
        let s = snap(gl, serial, rect, look);
        gl.draw(vec![s, solid(&new)]);
    };
    let full = [0, 0, W, H];
    one(gl, 1, full, Look::Wipe { cx: 100.0, cy: 100.0, r: 40.0 });
    assert!(gl.at(100, 100)[2] > 240, "the new scene in the hole: {:?}", gl.at(100, 100));
    assert!(gl.at(5, 5)[0] > 190 && gl.at(5, 195)[1] > 190, "the old picture around it, upright: {:?} {:?}", gl.at(5, 5), gl.at(5, 195));
    one(gl, 2, full, Look::Fade { alpha: 0.5 });
    let mid = gl.at(5, 5);
    assert!(near(mid, [100, 0, 127], 12), "half of each: {mid:?}");
    one(gl, 3, full, Look::Glitch { alpha: 0.6, blur: 0.0, dx: 0.0, skew: 0.0, hue: 0.0 });
    let mid = gl.at(5, 5);
    assert!(near(mid, [80, 0, 153], 14), "the new scene at 60 % over the old: {mid:?}");
    one(gl, 4, full, Look::Glitch { alpha: 1.0, blur: 0.0, dx: 40.0, skew: 0.0, hue: 0.0 });
    assert!(gl.at(20, 100)[0] > 150 || gl.at(20, 100)[1] > 150, "shifted right: the old picture left of it: {:?}", gl.at(20, 100));
    assert!(gl.at(100, 100)[2] > 240, "the new scene in its place: {:?}", gl.at(100, 100));
    one(gl, 5, full, Look::Carousel { off: 0.5, ground: [0.1, 0.1, 0.1, 1.0], persp: 800.0, radius: 8.0 });
    let (l, r) = (gl.at(30, 100), gl.at(170, 100));
    assert!(l[0] > 100 || l[1] > 100, "the old card leaves to the left: {l:?}");
    assert!(r[2] > 100, "the new one comes from the right: {r:?}");
    assert!(near(gl.at(2, 2), [25, 25, 25], 4), "on the ground: {:?}", gl.at(2, 2));
    one(gl, 6, [50, 50, 50, 50], Look::Morph { src: [0.0, 0.0, 100.0, 100.0], radius: 0.0, alpha: 1.0 });
    assert!(gl.at(75, 75)[0] > 190, "the top left of the old picture flies here: {:?}", gl.at(75, 75));
    assert!(gl.at(150, 150)[2] > 240, "the new scene around it: {:?}", gl.at(150, 150));
    fresh(gl);
    let s = snap(gl, 6, full, Look::Fade { alpha: 1.0 });
    gl.draw(vec![s, solid(&new)]);
    assert!(gl.at(5, 5)[0] > 190, "the same serial keeps its snapshot: {:?}", gl.at(5, 5));
}

fn reveal_comes_out_of_its_blur(gl: &mut Gl) {
    let ground = SolidColorBuffer::new((W, H), [0.0, 0.0, 0.0, 1.0]);
    let bytes: Vec<u8> = (0..40 * 40).flat_map(|i: i32| {
        let on = (i / 40) >= 10 && (i / 40) < 30 && (i % 40) >= 10 && (i % 40) < 30;
        (if on { 0xffffffffu32 } else { 0 }).to_le_bytes()
    }).collect();
    let image = std::rc::Rc::new(Image { serial: 1, w: 40, h: 40, bytes });
    fresh(gl);
    let e = gl.fx(("title", 0), [80, 80, 40, 40], Kind::Reveal { image: image.clone(), sigma: 0.0, alpha: 1.0 });
    gl.draw(vec![e, solid(&ground)]);
    assert!(gl.at(100, 100)[0] > 240, "plain at the end: {:?}", gl.at(100, 100));
    assert!(gl.at(82, 82)[0] < 10, "and sharp");
    fresh(gl);
    let e = gl.fx(("title", 0), [80, 80, 40, 40], Kind::Reveal { image, sigma: 8.0, alpha: 1.0 });
    gl.draw(vec![e, solid(&ground)]);
    let (mid, edge) = (gl.at(100, 100)[0], gl.at(86, 100)[0]);
    assert!(mid < 240 && edge > 15, "blurred at the start: {mid} / {edge}");
}

/// A number from `ctl fx`'s renderer part.
fn seen(gl: &Gl, key: &str) -> u64 {
    let line = super::report(&gl.r);
    let want = format!("{key}=");
    line.split_whitespace().find_map(|w| w.strip_prefix(want.as_str())).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("{key}= in {line}"))
}

/// A frame at the time `at`, with `last` as the frame on the screen.
fn frame_at(gl: &mut Gl, mut v: Vec<E>, last: impl FnMut(&mut GlesRenderer, &mut GlesTexture) -> Result<(), String>, at: Instant) {
    seal(&mut gl.fx, &mut v, Scale::from(1.0));
    super::prepare::prepare_at(&mut gl.r, &v, Scale::from(1.0), (W, H), last, at);
    gl.r.draw(&mut gl.target, &mut gl.tracker, &v, [0.0, 0.0, 0.0, 1.0].into()).expect("the frame draws");
}

fn glass(gl: &mut Gl, key: Key, rect: [i32; 4]) -> E {
    gl.fx(key, rect, Kind::Glass { radius: 0.0, sigma: 8.0, saturate: 1.0 })
}

/// A picture too short for its size: the renderer refuses it, on every driver.
fn broken_reveal(gl: &mut Gl) -> E {
    let image = Rc::new(Image { serial: 900, w: 40, h: 40, bytes: vec![0; 4] });
    gl.fx(("broken", 0), [80, 80, 40, 40], Kind::Reveal { image, sigma: 0.0, alpha: 1.0 })
}

fn a_failure_stays_with_its_element_and_waits_ever_longer(gl: &mut Gl) {
    fresh(gl);
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let last = |_: &mut GlesRenderer, _: &mut GlesTexture| Ok(());
    let under = SolidColorBuffer::new((W, H), [1.0, 0.0, 0.0, 1.0]);
    let gone = glass(gl, ("gone", 0), [40, 40, 120, 120]);
    frame_at(gl, vec![gone, solid(&under)], last, t0);
    let (fails, blurs) = (seen(gl, "fails"), seen(gl, "blurs"));

    // The broken one lies lowest, so its work comes first: the glass above it still gets its own.
    let (g, b) = (glass(gl, ("good", 0), [40, 40, 120, 120]), broken_reveal(gl));
    frame_at(gl, vec![g, b, solid(&under)], last, t0);
    assert_eq!(seen(gl, "fails"), fails + 1, "the failure is counted");
    assert_eq!(seen(gl, "retrying"), 1);
    assert_eq!(seen(gl, "blurs"), blurs + 1, "the glass above the failure blurs");
    assert_eq!(seen(gl, "kept"), 1, "and what no element uses any more still goes: {}", super::report(&gl.r));
    assert_eq!(gl.at(100, 100)[0], 255, "the glass shows");

    for (at, want, why) in [(50, 1, "not before its time"), (150, 2, "100 ms after the first"), (200, 2, "now it waits 200 ms"), (360, 3, "and then 400")] {
        let (g, b) = (glass(gl, ("good", 0), [40, 40, 120, 120]), broken_reveal(gl));
        frame_at(gl, vec![g, b, solid(&under)], last, t0 + ms(at));
        assert_eq!(seen(gl, "fails"), fails + want, "{why} ({at} ms)");
    }
    assert_eq!(seen(gl, "blurs"), blurs + 1, "nothing changed under the glass all this time");

    // The element is back in order: it tries at its time, and then it is over.
    let good = Rc::new(Image { serial: 901, w: 40, h: 40, bytes: vec![255; 40 * 40 * 4] });
    let fixed = gl.fx(("broken", 0), [80, 80, 40, 40], Kind::Reveal { image: good, sigma: 0.0, alpha: 1.0 });
    frame_at(gl, vec![fixed, solid(&under)], last, t0 + ms(5000));
    assert_eq!(seen(gl, "retrying"), 0, "it works again");
    assert_eq!(seen(gl, "fails"), fails + 3);
    assert_eq!(gl.at(100, 100), [255, 255, 255], "and shows");
}

fn a_glass_that_cannot_blur_keeps_the_picture_it_has(gl: &mut Gl) {
    fresh(gl);
    let t0 = Instant::now();
    let last = |_: &mut GlesRenderer, _: &mut GlesTexture| Ok(());
    let under = SolidColorBuffer::new((W, H), [0.0, 0.0, 1.0, 1.0]);
    let g = glass(gl, ("keep", 0), [0, 0, W, H]);
    frame_at(gl, vec![g, solid(&under)], last, t0);
    let (fails, kept) = (seen(gl, "fails"), seen(gl, "kept"));
    assert!(gl.at(100, 100)[2] > 240);
    // Past every texture size the driver has.
    let g = glass(gl, ("keep", 0), [0, 0, 1 << 20, 1 << 20]);
    frame_at(gl, vec![g, solid(&under)], last, t0);
    assert_eq!(seen(gl, "fails"), fails + 1, "{}", super::report(&gl.r));
    assert_eq!(seen(gl, "kept"), kept, "its blur stays");
    assert!(gl.at(100, 100)[2] > 240, "and is still drawn: {:?}", gl.at(100, 100));
}

fn a_failed_snapshot_leaves_the_new_scene(gl: &mut Gl) {
    fresh(gl);
    let t0 = Instant::now();
    let new = SolidColorBuffer::new((W, H), [0.0, 0.0, 1.0, 1.0]);
    let fails = seen(gl, "fails");
    let s = gl.fx(("transition", 0), [0, 0, W, H], Kind::Snapshot { serial: 950, look: Look::Fade { alpha: 1.0 } });
    let g = glass(gl, ("over", 0), [10, 10, 20, 20]);
    let blurs = seen(gl, "blurs");
    frame_at(gl, vec![g, s, solid(&new)], |_, _| Err("no frame on the screen yet".into()), t0);
    assert_eq!(seen(gl, "fails"), fails + 1);
    assert_eq!(seen(gl, "blurs"), blurs + 1, "the glass above still blurs");
    assert!(gl.at(100, 100)[2] > 240, "no old picture, so the new scene: {:?}", gl.at(100, 100));
}

fn what_no_element_uses_goes_back(gl: &mut Gl) {
    fresh(gl);
    let t0 = Instant::now();
    let last = |r: &mut GlesRenderer, t: &mut GlesTexture| Offscreen::<GlesTexture>::create_buffer(r, Fourcc::Abgr8888, (W, H).into()).map(|n| *t = n).map_err(|e| e.to_string());
    let under = SolidColorBuffer::new((W, H), [0.0, 1.0, 0.0, 1.0]);
    // Ends what the test before left waiting.
    gl.draw(vec![solid(&under)]);
    let image = Rc::new(Image { serial: 960, w: 40, h: 40, bytes: vec![255; 40 * 40 * 4] });
    let all = |gl: &mut Gl, under: &SolidColorBuffer| {
        let t = gl.fx(("title", 0), [80, 80, 40, 40], Kind::Reveal { image: image.clone(), sigma: 4.0, alpha: 1.0 });
        let s = gl.fx(("transition", 0), [0, 0, W, H], Kind::Snapshot { serial: 961, look: Look::Glitch { alpha: 0.6, blur: 0.0, dx: 0.0, skew: 0.0, hue: 0.0 } });
        let g = glass(gl, ("still", 0), [40, 40, 120, 120]);
        vec![t, s, g, solid(under)]
    };
    let v = all(gl, &under);
    frame_at(gl, v, last, t0);
    assert_eq!(seen(gl, "retrying"), 0, "{}", super::report(&gl.r));
    assert_eq!(seen(gl, "textures"), 6, "glass + reveal + scratch + old + new + title: {}", super::report(&gl.r));
    let g = glass(gl, ("still", 0), [40, 40, 120, 120]);
    frame_at(gl, vec![g, solid(&under)], last, t0);
    assert_eq!(seen(gl, "textures"), 2, "glass + scratch, the rest ended with the transition: {}", super::report(&gl.r));
    gl.draw(vec![solid(&under)]);
    assert_eq!(seen(gl, "textures"), 0, "no effect in the frame, no texture: {}", super::report(&gl.r));
    assert_eq!(seen(gl, "kept"), 0);
    let g = glass(gl, ("still", 0), [40, 40, 120, 120]);
    gl.draw(vec![g, solid(&under)]);
    assert_eq!(gl.at(100, 100)[1], 255, "and the level draws again from nothing");
}

/// `GPU_WAIT`'s bound (B4) without judging a time: cut to nothing, the frame comes back while its GPU work still runs,
/// even with a long blur before it, and is not read back; the point it waits on covers the blur too (once it is reached
/// no work of length is left); the next frame shows it, with no change of its own (main.rs `gpu_late` asks for that one).
fn a_late_frame_comes_back_at_once_and_the_next_one_shows_it(gl: &mut Gl) {
    fresh(gl);
    let start = SolidColorBuffer::new((W, H), [1.0, 1.0, 1.0, 1.0]);
    gl.draw(vec![solid(&start)]);
    assert_eq!(gl.at(100, 100), [255, 255, 255]);
    GPU_WAIT_CUT_MS.store(0, Ordering::Relaxed);
    let mut caught = None;
    // A slower picture each round until the frame is caught at it (a fast rasteriser may finish a small one first).
    for (round, size) in [2048, 4096, 8192, 8192, 8192].into_iter().enumerate() {
        let color = [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0], [1.0, 1.0, 0.0, 1.0], [0.0, 1.0, 1.0, 1.0]][round];
        let under = SolidColorBuffer::new((size, size), color);
        let shown = gl.at(100, 100);
        let g = glass(gl, ("late", round as u64), [0, 0, size, size]);
        let mut v = vec![g, solid(&under)];
        seal(&mut gl.fx, &mut v, Scale::from(1.0));
        gl.r.fx_prepare(&mut gl.target, &v, Scale::from(1.0));
        let (damage, _) = gl.r.draw(&mut gl.target, &mut gl.tracker, &v, [0.0, 0.0, 0.0, 1.0].into()).expect("the frame draws");
        if !GlesRenderer::gpu_idle(&mut gl.target) {
            assert!(damage.is_empty(), "a late frame is not shown");
            assert_eq!(gl.at(100, 100), shown, "nor read back");
            caught = Some((v, under, color));
            break;
        }
        assert_eq!(gl.at(100, 100)[..2], [(color[0] * 255.0) as i32, (color[1] * 255.0) as i32], "a frame in time is shown");
    }
    let (v, _under, color) = caught.expect("even the largest blur was over before the frame came back: this rasteriser proves nothing");
    let t0 = Instant::now();
    while !GlesRenderer::gpu_idle(&mut gl.target) {
        assert!(t0.elapsed() < Duration::from_secs(60), "the GPU work never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
    let waited = t0.elapsed();
    GPU_WAIT_CUT_MS.store(u64::MAX, Ordering::Relaxed);
    let t1 = Instant::now();
    let (damage, _) = gl.r.draw(&mut gl.target, &mut gl.tracker, &v, [0.0, 0.0, 0.0, 1.0].into()).expect("the next frame draws");
    let left = t1.elapsed();
    assert!(left <= waited.max(Duration::from_millis(50)), "the point reached, this much was still running: {left:?} after a wait of {waited:?}");
    assert!(!damage.is_empty(), "the next frame shows it");
    assert!(near(gl.at(100, 100), color.map(|c| (c * 255.0) as i32)[..3].try_into().expect("three"), 2), "with nothing changed: {:?}", gl.at(100, 100));
}
