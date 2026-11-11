//! Composition into an offscreen picture; back ends copy only the damaged parts out (a host window, `/dev/fb0` on
//! V2, where reading back from the framebuffer would be slow).
//!
//! WY-14 (gx-scout-mesa.md step C0): the compositor is generic over its renderer (`CompRenderer`). Pixman is the
//! default and today the only one; WY-15's GLES renderer implements the same trait, with a readback buffer behind
//! `CompRenderer::pixels`, so back ends, screenshots and the bench see only the renderer-neutral `Picture`.

use std::time::{Duration, Instant};

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{
    AsRenderElements, RenderElementStates, default_primary_scanout_output_compare, render_elements,
};
use smithay::backend::renderer::pixman::{PixmanRenderer, PixmanTexture};
use smithay::backend::allocator::{Buffer as _, Format};
use smithay::backend::renderer::{Bind, Color32F, ImportAll, ImportDma, ImportMem, Renderer, RendererSuper, Texture};
use smithay::desktop::utils::{
    OutputPresentationFeedback, surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
    update_surface_primary_scanout_output,
};
use smithay::reexports::pixman::{FormatCode, Image};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{Monotonic, Physical, Rectangle, Scale, Time};
use smithay::wayland::presentation::Refresh;

use crate::backend::scanout::{Aim, Windows};
use crate::win::Window;

use crate::deco;
use crate::layout::Placement;
use crate::state::App;

/// The renderer every build draws with unless GLES is chosen (WY-15: only the gpu build has GLES).
pub type DefaultRenderer = PixmanRenderer;

/// What bitscomp needs of a renderer: Smithay's import traits for client buffers and its own images, plus an
/// offscreen target whose pixels the back ends read (XRGB8888, the format of `/dev/fb0` and the host window).
pub trait CompRenderer: Renderer<TextureId = Self::Tex> + ImportAll + ImportDma + ImportMem + Sized + 'static {
    type Tex: Texture + Clone + Send + 'static;
    /// The picture one frame is drawn into.
    type Target;
    /// For `ctl stats` and the start line.
    const NAME: &'static str;
    fn create() -> Result<Self, String>;
    fn target(&mut self, w: i32, h: i32) -> Result<Self::Target, String>;
    /// Draws `elements` (topmost first) into `target`; the damaged rectangles (empty when nothing changed) and
    /// the element states for presentation feedback.
    fn draw(
        &mut self,
        target: &mut Self::Target,
        tracker: &mut OutputDamageTracker,
        elements: &[CompElement<Self>],
        clear: Color32F,
    ) -> Result<(Vec<Rectangle<i32, Physical>>, RenderElementStates), String>;
    fn target_size(target: &Self::Target) -> (i32, i32);
    /// The target's rows, `stride` u32 apart (stride >= width); not drawn into while borrowed.
    fn pixels(target: &Self::Target) -> (&[u32], usize);
    /// After each frame: textures no element uses any more.
    fn cleanup(&mut self);
    /// WY-21: why the GPU behind this renderer is gone, once it says so (a robust GL context's reset).
    fn reset_status(&mut self) -> Option<String> {
        None
    }
    /// WY-16: the renderer reads client dma-bufs through a CPU mapping that must be checked first (`probe`).
    fn maps_dmabufs(&self) -> bool {
        false
    }
    /// WY-17b: this renderer reads client buffers by CPU, whichever way it maps them: it has no GPU a client could
    /// share VRAM with, so linux-dmabuf names no main device (`dmabuf::advertised`). Pixman; GLES says what it runs on.
    fn reads_by_cpu(&self) -> bool {
        true
    }
    /// WY-16: the last frame's GPU work is over (client buffers may go back); asked without waiting.
    fn gpu_idle(_target: &mut Self::Target) -> bool {
        true
    }
    /// WY-17: takes fbdev's two windows as render targets (`backend::scanout`): frames go straight there, `pixels`
    /// is only what `sync_pixels` read last. Why not, and nothing changed.
    fn adopt_scanout(&mut self, _target: &mut Self::Target, _windows: Windows) -> Result<(), String> {
        Err(format!("renderer {} has no render targets but its own picture", Self::NAME))
    }
    /// WY-17, before a frame: where it goes. None ends the direct path (the next frame draws and reads back whole).
    /// False: `aim` named a window but this picture has none.
    fn aim(_target: &mut Self::Target, aim: Option<Aim>) -> bool {
        aim.is_none()
    }
    /// WY-17: the frame on the screen into `pixels` (`ctl shot`); nothing to do for a picture that always has it.
    fn sync_pixels(&mut self, _target: &mut Self::Target) {}
    /// WY-17: for `ctl stats`.
    fn draws_direct(_target: &Self::Target) -> bool {
        false
    }
    /// WY-19: whether this renderer draws the gpu effect level (`on_software`: also as a software rasteriser); why not.
    fn fx_capable(&mut self, _on_software: bool) -> Result<(), String> {
        Err(format!("renderer {} has no shaders", Self::NAME))
    }
    /// WY-19, before a frame's draw: what its effects need beforehand (blurs, snapshots), in frame order.
    fn fx_prepare(&mut self, _target: &mut Self::Target, _elements: &[CompElement<Self>], _scale: Scale<f64>) {}
    /// WY-19: one effect into the frame under way. A renderer without shaders is never handed one (`fx_capable`).
    fn fx_draw(
        _frame: &mut <Self as RendererSuper>::Frame<'_, '_>,
        _fx: &crate::fx::FxElement,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), <Self as RendererSuper>::Error> {
        Ok(())
    }
    /// WY-19: for `ctl fx`.
    fn fx_report(&self) -> String {
        "no shaders".to_string()
    }
}

impl CompRenderer for PixmanRenderer {
    type Tex = PixmanTexture;
    type Target = Image<'static, 'static>;
    const NAME: &'static str = "pixman";

    fn create() -> Result<Self, String> {
        PixmanRenderer::new().map_err(|e| format!("pixman renderer: {e:?}"))
    }

    fn target(&mut self, w: i32, h: i32) -> Result<Self::Target, String> {
        Image::new(FormatCode::X8R8G8B8, w.max(1) as usize, h.max(1) as usize, true)
            .map_err(|_| format!("pixman: no {w}x{h} shadow image"))
    }

    fn draw(
        &mut self,
        target: &mut Self::Target,
        tracker: &mut OutputDamageTracker,
        elements: &[CompElement<Self>],
        clear: Color32F,
    ) -> Result<(Vec<Rectangle<i32, Physical>>, RenderElementStates), String> {
        let mut fb = self.bind(target).map_err(|e| format!("pixman bind: {e:?}"))?;
        let result = tracker.render_output(self, &mut fb, 1, elements, clear).map_err(|e| format!("pixman render: {e:?}"))?;
        Ok((result.damage.cloned().unwrap_or_default(), result.states))
    }

    fn target_size(target: &Self::Target) -> (i32, i32) {
        (target.width() as i32, target.height() as i32)
    }

    fn pixels(target: &Self::Target) -> (&[u32], usize) {
        let stride = target.stride() / 4;
        // SAFETY: the image is ours and alive for the borrow, its buffer is `stride * height` u32 long (pixman's
        // bits image, made by `target` above), and nothing renders into it while the shared borrow lasts.
        let px = unsafe { std::slice::from_raw_parts(target.data(), stride * target.height()) };
        (px, stride)
    }

    fn cleanup(&mut self) {
        let _ = self.cleanup_texture_cache();
    }
}

render_elements! {
    pub CompElement<R> where R: CompRenderer;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
    Memory=MemoryRenderBufferRenderElement<R>,
    Fx=crate::fx::FxElement,
}

impl<R: CompRenderer> CompElement<R> {
    pub fn is_beam(&self) -> bool {
        matches!(self, CompElement::Fx(f) if matches!(f.kind, crate::fx::Kind::Beam { .. }))
    }
}

/// The finished picture, renderer-neutral: what the back ends copy out, screenshots and the bench read.
pub trait Picture {
    fn size(&self) -> (i32, i32);
    /// XRGB8888 rows `stride` u32 apart.
    fn pixels(&self) -> (&[u32], usize);
    fn renderer(&self) -> &'static str;
    /// WY-17: the frames go straight into the scanout windows (`pixels` holds only a read on request).
    fn draws_direct(&self) -> bool {
        false
    }
}

pub struct Compositor<R: CompRenderer = DefaultRenderer> {
    pub renderer: R,
    target: R::Target,
    damage: OutputDamageTracker,
    seq: u64,
    /// A mode a new compositor could not be made for yet: tried again with a backoff, said once per mode.
    retry: Option<Retry>,
    /// Since when frames fail to draw (said once; cleared, and said, by the next frame that draws).
    draw_failed: Option<u64>,
}

/// WY-14 fixer: a failed rebuild for a new mode is retried (1 s, doubling to 30 s), not latched until the next mode.
struct Retry {
    mode: (i32, i32),
    next: Instant,
    delay: Duration,
    tries: u32,
}

const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

impl<R: CompRenderer> Picture for Compositor<R> {
    fn size(&self) -> (i32, i32) {
        R::target_size(&self.target)
    }
    fn pixels(&self) -> (&[u32], usize) {
        R::pixels(&self.target)
    }
    fn renderer(&self) -> &'static str {
        R::NAME
    }
    fn draws_direct(&self) -> bool {
        R::draws_direct(&self.target)
    }
}

/// WY-15: the compositor whichever renderer draws it; the main loop holds one of these (`make`).
pub trait Composer: Picture {
    /// Renders one frame; returns the damaged rectangles (empty when nothing changed on screen).
    fn render(&mut self, app: &mut App) -> Vec<Rectangle<i32, Physical>>;
    /// WY-16: the dma-buf formats this renderer imports (Pixman: LINEAR by mmap; GLES: the EGL display's).
    fn dmabuf_formats(&self) -> Vec<Format>;
    /// WY-16: client dma-bufs must map for reading before this renderer sees them (`dmabuf::probe`).
    fn maps_dmabufs(&self) -> bool;
    /// WY-17b: `CompRenderer::reads_by_cpu`.
    fn reads_by_cpu(&self) -> bool;
    /// WY-16, each loop turn: answers the new dma-bufs (the renderer imports each one now, so a buffer it cannot
    /// read is `failed` instead of an empty window later) and drops the imports of destroyed buffers.
    fn dmabuf_turn(&mut self, app: &mut App);
    /// WY-16: the last frame's GPU work is over (`CompRenderer::gpu_idle`).
    fn gpu_idle(&mut self) -> bool;
    /// WY-17: `CompRenderer::adopt_scanout`.
    fn adopt_scanout(&mut self, windows: Windows) -> Result<(), String>;
    /// WY-17, before `render`: follows the output's mode first, since a new picture has no windows; then
    /// `CompRenderer::aim`.
    fn aim(&mut self, app: &App, aim: Option<Aim>) -> bool;
    /// WY-17: `CompRenderer::sync_pixels`.
    fn sync_pixels(&mut self);
    /// WY-19: `CompRenderer::fx_report`.
    fn fx_report(&self) -> String;
    /// WY-21: `CompRenderer::reset_status`.
    fn gpu_reset(&mut self) -> Option<String>;
}

impl<R: CompRenderer> Composer for Compositor<R> {
    fn render(&mut self, app: &mut App) -> Vec<Rectangle<i32, Physical>> {
        Compositor::render(self, app)
    }

    fn dmabuf_formats(&self) -> Vec<Format> {
        self.renderer.dmabuf_formats().iter().copied().collect()
    }

    fn maps_dmabufs(&self) -> bool {
        self.renderer.maps_dmabufs()
    }

    fn reads_by_cpu(&self) -> bool {
        self.renderer.reads_by_cpu()
    }

    fn gpu_idle(&mut self) -> bool {
        R::gpu_idle(&mut self.target)
    }

    fn adopt_scanout(&mut self, windows: Windows) -> Result<(), String> {
        self.renderer.adopt_scanout(&mut self.target, windows)
    }

    fn aim(&mut self, app: &App, aim: Option<Aim>) -> bool {
        if let Some(mode) = app.output.current_mode().map(|m| (m.size.w, m.size.h)) {
            self.follow_mode(app, mode);
        }
        R::aim(&mut self.target, aim)
    }

    fn sync_pixels(&mut self) {
        self.renderer.sync_pixels(&mut self.target);
    }

    fn fx_report(&self) -> String {
        self.renderer.fx_report()
    }

    fn gpu_reset(&mut self) -> Option<String> {
        self.renderer.reset_status()
    }
    fn dmabuf_turn(&mut self, app: &mut App) {
        for (buf, notifier) in std::mem::take(&mut app.dma.pending) {
            match self.renderer.import_dmabuf(&buf, None).map_err(|e| format!("{e:?}")) {
                Ok(_) => {
                    app.stats.dmabuf_imported += 1;
                    let _ = notifier.successful::<App>();
                }
                Err(e) => {
                    app.stats.dmabuf_failed += 1;
                    // Said for the first refusals and then every 100th (a client may retry in a loop).
                    let n = app.stats.dmabuf_failed;
                    if n <= 8 || n % 100 == 0 {
                        let (w, h) = (buf.width(), buf.height());
                        let what = crate::dmabuf::fourcc_name(buf.format());
                        eprintln!("bitscomp: dmabuf: {} refuses a {w}x{h} {what} buffer: {e} (refusal {n})", R::NAME);
                    }
                    notifier.failed();
                }
            }
        }
        if std::mem::take(&mut app.dma.cleanup_due) {
            self.renderer.cleanup();
        }
    }
}

/// WY-15: the compositor for the chosen renderer. GLES (gpu build only) that cannot start falls back to Pixman in
/// the same process, said once: the picture must come up whatever EGL does. A failed Pixman one is an error.
pub fn make(app: &App, renderer: crate::flavour::Renderer) -> Result<Box<dyn Composer>, String> {
    #[cfg(feature = "gpu")]
    if renderer == crate::flavour::Renderer::Gles {
        match Compositor::<smithay::backend::renderer::gles::GlesRenderer>::new(app) {
            // WY-16: a Mesa whose EGL imports no dma-buf would leave clients an empty table; Pixman reads them by
            // mmap, so it draws instead (radeonsi and llvmpipe both have EGL_EXT_image_dma_buf_import).
            Ok(c) if c.renderer.dmabuf_formats().iter().next().is_none() => {
                eprintln!("bitscomp: renderer gles: EGL imports no dma-buf (EGL_EXT_image_dma_buf_import); drawing with pixman");
            }
            Ok(c) => return Ok(Box::new(c)),
            Err(e) => eprintln!("bitscomp: renderer gles: {e}; drawing with pixman"),
        }
    }
    #[cfg(not(feature = "gpu"))]
    let _ = renderer;
    Ok(Box::new(Compositor::<DefaultRenderer>::new(app)?))
}

impl<R: CompRenderer> Compositor<R> {
    pub fn new(app: &App) -> Result<Self, String> {
        let size = app.output.current_mode().map(|m| m.size).ok_or("the output has no mode")?;
        let mut renderer = R::create()?;
        let target = renderer.target(size.w, size.h)?;
        Ok(Self { renderer, target, damage: OutputDamageTracker::from_output(&app.output), seq: 0, retry: None, draw_failed: None })
    }

    /// Renders one frame; returns the damaged rectangles (empty when nothing changed on screen).
    pub fn render(&mut self, app: &mut App) -> Vec<Rectangle<i32, Physical>> {
        if let Some(mode) = app.output.current_mode().map(|m| (m.size.w, m.size.h)) {
            self.follow_mode(app, mode);
        }
        let t0 = Instant::now();
        // WY-19: the gpu level follows the renderer's shaders, live (a theme change just draws the new images).
        if app.theme.effects == crate::theme::Effects::Gpu || app.theme.effects_gpu {
            let can = self.renderer.fx_capable(app.fx.on_software);
            app.fx_follow(can);
        }
        let elements = build_elements(app, &mut self.renderer);
        app.stats.build += t0.elapsed();
        // Also without an effect in the frame: that is what hands the textures of the last one back.
        let scale = Scale::from(app.output.current_scale().fractional_scale());
        let t1 = Instant::now();
        self.renderer.fx_prepare(&mut self.target, &elements, scale);
        app.stats.prepare += t1.elapsed();
        let clear: Color32F = app.theme.bg.into();
        let (damage, states) = match self.renderer.draw(&mut self.target, &mut self.damage, &elements, clear) {
            Ok(r) => r,
            Err(e) => {
                let n = self.draw_failed.get_or_insert(0);
                if *n == 0 {
                    eprintln!("bitscomp: render: {e} (frame skipped; said once until a frame draws again)");
                }
                *n += 1;
                // WY-16: the replaced buffers go back before the callbacks (a client reuses them on `done`).
                app.dma.gpu_busy = !R::gpu_idle(&mut self.target);
                app.give_back();
                // Clients still get their frame callback (they would wait for it); the frame was not shown.
                self.after_frame(app, &RenderElementStates::default(), false);
                return Vec::new();
            }
        };
        if let Some(n) = self.draw_failed.take() {
            eprintln!("bitscomp: render: drawing again after {n} skipped frames");
        }
        app.stats.render += t0.elapsed();
        app.stats.frames += 1;
        if damage.is_empty() {
            app.stats.empty_frames += 1;
        }
        app.stats.damaged_px += damage.iter().map(|r| (r.size.w * r.size.h) as u64).sum::<u64>();
        // WY-16: the draw is over, so the buffers this turn's commits replaced go back now, before the frame
        // callbacks: a client that reuses a buffer on `done` finds its release already there. Not while the GPU
        // still runs the frame (GLES waited a bounded time, render_gl.rs): then at the end of a later turn.
        app.dma.gpu_busy = !R::gpu_idle(&mut self.target);
        app.give_back();
        self.after_frame(app, &states, true);
        damage
    }

    /// A new mode: a whole new compositor (renderer, picture, damage history), as before WY-14. When that fails the
    /// old picture keeps its size and the damage tracker is pinned to it (it would otherwise follow the output's new
    /// mode, and its rectangles would leave the picture); the rebuild is tried again with a backoff.
    fn follow_mode(&mut self, app: &App, mode: (i32, i32)) {
        if mode == self.size() {
            if self.retry.take().is_some() {
                // Back at the picture's size: follow the output again (one full redraw).
                self.damage = OutputDamageTracker::from_output(&app.output);
            }
            return;
        }
        let now = Instant::now();
        if let Some(r) = &self.retry {
            if r.mode == mode && now < r.next {
                return;
            }
        }
        match Self::new(app) {
            Ok(c) => {
                if let Some(r) = &self.retry {
                    if r.mode == mode {
                        eprintln!("bitscomp: render: compositor for {}x{} after {} failed tries", mode.0, mode.1, r.tries);
                    }
                }
                *self = c;
            }
            Err(e) => {
                let r = match self.retry.take() {
                    Some(r) if r.mode == mode => {
                        let delay = (r.delay * 2).min(RETRY_MAX);
                        Retry { mode, next: now + delay, delay, tries: r.tries + 1 }
                    }
                    _ => {
                        eprintln!(
                            "bitscomp: render: no compositor for {}x{} ({e}); the picture keeps its size, tried again every {}-{} s",
                            mode.0,
                            mode.1,
                            RETRY_FIRST.as_secs(),
                            RETRY_MAX.as_secs()
                        );
                        Retry { mode, next: now + RETRY_FIRST, delay: RETRY_FIRST, tries: 1 }
                    }
                };
                if r.tries == 1 {
                    let (w, h) = self.size();
                    self.damage = OutputDamageTracker::new(
                        (w, h),
                        app.output.current_scale().fractional_scale(),
                        app.output.current_transform(),
                    );
                }
                self.retry = Some(r);
            }
        }
    }

    /// Frame callbacks and presentation feedback. Hidden stack pages get a callback once a second only,
    /// so a browser behind another page does not keep drawing at full rate. B4 (WY-4): the frame never waits for
    /// a client: a callback says "this frame was drawn", with whatever the client had committed by then; a
    /// client that has not drawn keeps its last buffer on screen. A hung client gets none (they would only pile up).
    /// `shown`: false for a frame that failed to draw: callbacks still go out (with the scanout outputs of the last
    /// frame that drew), the presentation feedback is discarded.
    fn after_frame(&mut self, app: &mut App, states: &RenderElementStates, shown: bool) {
        let output = app.output.clone();
        // The one clock (`pacer`): callbacks carry the frame's time, the frame is on screen one period later.
        let now: Time<Monotonic> = app.frame_time.into();
        let on_screen: Time<Monotonic> = (app.frame_time + app.refresh).into();
        let mut feedback = OutputPresentationFeedback::new(&output);
        for w in app.all_windows().into_iter().chain(app.x_unmanaged()) {
            if app.is_hung(&w) {
                continue;
            }
            if shown {
                w.with_surfaces(|surface, data| {
                    update_surface_primary_scanout_output(
                        surface,
                        &output,
                        data,
                        states,
                        default_primary_scanout_output_compare,
                    );
                });
            }
            w.send_frame(&output, now, Some(Duration::from_secs(1)), surface_primary_scanout_output);
            w.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, |surface, _| {
                surface_presentation_feedback_flags_from_states(surface, states)
            });
        }
        if shown {
            self.seq += 1;
            feedback.presented(on_screen, Refresh::Fixed(app.refresh), self.seq, wp_presentation_feedback::Kind::empty());
        } else {
            feedback.discarded();
        }
        self.renderer.cleanup();
    }
}

fn build_elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    // The damage tracker wants the topmost element first; `paint::order` says which (WY-5c: floats over the bars).
    use crate::paint::Layer;
    let placements = app.placements();
    let mut out = Vec::new();
    for layer in crate::paint::order(&placements) {
        match layer {
            Layer::Cursor => out.extend(crate::cursor::elements(app, renderer)),
            Layer::System => out.extend(crate::install_draw::elements(app, renderer)),
            Layer::Overlays => {
                // WY-3: X11 menus and tooltips under bitscomp's own overlays, over every window and bar.
                // WY-19: the transition over all of them (the boot scene decides first).
                let overlays = deco::overlays(app, renderer);
                let covered = placements.iter().any(|p| p.fullscreen && p.visible);
                out.extend(crate::fx::plan::top(app, covered).into_iter().map(CompElement::from));
                x_unmanaged(app, renderer, &mut out);
                out.extend(overlays);
            }
            Layer::Over => out.extend(crate::chrome_render::over(app, renderer)),
            Layer::Library => out.extend(crate::library_draw::elements(app, renderer)),
            // The frame above the window: its bottom corners round off the client's corners.
            Layer::Frame(i) => out.extend(deco::frame(app, renderer, &placements[i])),
            Layer::Veil(i) => out.extend(crate::veil::elements(app, renderer, &placements[i])),
            Layer::Content(i) => {
                window_content(app, renderer, &placements[i], &mut out);
                out.extend(crate::fx::plan::window(app, &placements[i]).into_iter().map(CompElement::from));
            }
            Layer::Chrome => out.extend(crate::chrome_render::elements(app, renderer)),
        }
    }
    let covered = placements.iter().any(|p| p.fullscreen && p.visible);
    out.extend(crate::fx::plan::ground(app, covered).into_iter().map(CompElement::from));
    // DS-2: the wallpaper is the bottom of every frame (none under a fullscreen window).
    out.extend(crate::chrome_render::wallpaper::elements(app, renderer, &placements));
    let scale = Scale::from(app.output.current_scale().fractional_scale());
    crate::fx::seal(&mut app.fx, &mut out, scale);
    out
}

/// WY-3: the override-redirect X11 windows at their X positions, the last mapped on top.
fn x_unmanaged<R: CompRenderer>(app: &App, renderer: &mut R, out: &mut Vec<CompElement<R>>) {
    let scale = Scale::from(app.theme.scale);
    for w in app.x_unmanaged().iter().rev() {
        let at = w.x11().map(|x| x.geo.get().loc).unwrap_or_default().to_physical_precise_round(scale);
        out.extend(w.render_elements::<CompElement<R>>(renderer, at, scale, 1.0));
    }
}

/// A window's own picture (under its veil and frame).
fn window_content<R: CompRenderer>(app: &mut App, renderer: &mut R, p: &Placement<Window>, out: &mut Vec<CompElement<R>>) {
    let scale = Scale::from(app.theme.scale);
    let origin = app.window_origin(&p.win, p.frame).to_physical_precise_round(scale);
    // WY-5b: a settings page's content is bitscomp's own image (its client only holds the window).
    if let Some(pid) = app.page_pid(&p.win) {
        let c = app.content_rect(p.frame);
        out.extend(app.page_elements(renderer, pid, c));
        return;
    }
    out.extend(p.win.render_elements::<CompElement<R>>(renderer, origin, scale, 1.0));
}
