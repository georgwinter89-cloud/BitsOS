//! Composition on the GPU: GLES draws every frame straight into the screen's scanout windows (`render_gl`,
//! `backend::scanout`). The element building is written against `CompRenderer`, whose one implementation is
//! Smithay's `GlesRenderer`; screenshots, Durchblick's picture and the bench read the frame on the screen back on
//! request only (`Picture::pixels` after `Composer::sync_pixels`).

use std::time::{Duration, Instant};

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{
    AsRenderElements, RenderElementStates, default_primary_scanout_output_compare, render_elements,
};
use smithay::backend::allocator::{Buffer as _, Format};
use smithay::backend::renderer::{Color32F, ImportAll, ImportDma, ImportMem, Renderer, RendererSuper, Texture};
use smithay::desktop::utils::{
    OutputPresentationFeedback, surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
    update_surface_primary_scanout_output,
};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{Monotonic, Physical, Rectangle, Scale, Time};
use smithay::wayland::presentation::Refresh;

use crate::backend::scanout::{Aim, Windows};
use crate::win::Window;

use crate::deco;
use crate::layout::Placement;
use crate::state::App;

/// What bitscomp needs of a renderer: Smithay's import traits for client buffers and its own images, the scanout
/// windows as its targets, the effects' shaders, and a read of the frame on the screen (XRGB8888, fb0's format).
pub trait CompRenderer: Renderer<TextureId = Self::Tex> + ImportAll + ImportDma + ImportMem + Sized + 'static {
    type Tex: Texture + Clone + Send + 'static;
    /// The picture one frame is drawn into.
    type Target;
    /// For `ctl stats` and the start line.
    const NAME: &'static str;
    /// The renderer with its effect shaders compiled (a shader that does not compile is an error).
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
    /// The rows `sync_pixels` read last, `stride` u32 apart (stride >= width).
    fn pixels(target: &Self::Target) -> (&[u32], usize);
    /// After each frame: textures no element uses any more.
    fn cleanup(&mut self);
    /// WY-21: why the GPU behind this renderer is gone, once it says so (a robust GL context's reset).
    fn reset_status(&mut self) -> Option<String>;
    /// WY-16: the last frame's GPU work is over (client buffers may go back); asked without waiting.
    fn gpu_idle(target: &mut Self::Target) -> bool;
    /// WY-17: takes fbdev's two windows as the render targets (`backend::scanout`): every frame goes straight there.
    /// Why not, and nothing changed.
    fn adopt_scanout(&mut self, target: &mut Self::Target, windows: Windows) -> Result<(), String>;
    /// WY-17, before a frame: the window it goes to. False: this picture has no windows (yet, or any more).
    fn aim(target: &mut Self::Target, aim: Aim) -> bool;
    /// WY-17: the frame on the screen into `pixels` (`ctl shot`, Durchblick's picture, the bench).
    fn sync_pixels(&mut self, target: &mut Self::Target);
    /// WY-19, before a frame's draw: what its effects need beforehand (blurs, snapshots), in frame order.
    fn fx_prepare(&mut self, target: &mut Self::Target, elements: &[CompElement<Self>], scale: Scale<f64>);
    /// WY-19: one effect into the frame under way.
    fn fx_draw(
        frame: &mut <Self as RendererSuper>::Frame<'_, '_>,
        fx: &crate::fx::FxElement,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), <Self as RendererSuper>::Error>;
    /// WY-19: for `ctl fx`.
    fn fx_report(&self) -> String;
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

/// The frame on the screen as screenshots, Durchblick and the bench read it (after `Composer::sync_pixels`).
pub trait Picture {
    fn size(&self) -> (i32, i32);
    /// XRGB8888 rows `stride` u32 apart.
    fn pixels(&self) -> (&[u32], usize);
    fn renderer(&self) -> &'static str;
}

pub struct Compositor<R: CompRenderer> {
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
}

/// WY-15: the compositor whichever renderer draws it; the main loop holds one of these (`make`).
pub trait Composer: Picture {
    /// Renders one frame; returns the damaged rectangles (empty when nothing changed on screen).
    fn render(&mut self, app: &mut App) -> Vec<Rectangle<i32, Physical>>;
    /// WY-16: the dma-buf formats this renderer imports (the EGL display's).
    fn dmabuf_formats(&self) -> Vec<Format>;
    /// WY-16, each loop turn: answers the new dma-bufs (the renderer imports each one now, so a buffer it cannot
    /// read is `failed` instead of an empty window later) and drops the imports of destroyed buffers.
    fn dmabuf_turn(&mut self, app: &mut App);
    /// WY-16: the last frame's GPU work is over (`CompRenderer::gpu_idle`).
    fn gpu_idle(&mut self) -> bool;
    /// WY-17: `CompRenderer::adopt_scanout`.
    fn adopt_scanout(&mut self, windows: Windows) -> Result<(), String>;
    /// WY-17, before `render`: follows the output's mode first, since a new picture has no windows; then
    /// `CompRenderer::aim`.
    fn aim(&mut self, app: &App, aim: Aim) -> bool;
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

    fn gpu_idle(&mut self) -> bool {
        R::gpu_idle(&mut self.target)
    }

    fn adopt_scanout(&mut self, windows: Windows) -> Result<(), String> {
        self.renderer.adopt_scanout(&mut self.target, windows)
    }

    fn aim(&mut self, app: &App, aim: Aim) -> bool {
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

/// WY-15: the GLES compositor. EGL that cannot start, imports no dma-buf (EGL_EXT_image_dma_buf_import, which radeonsi
/// has) or compiles no effect shader is an error: bitscomp ends with its line and the console keeps the screen.
pub fn make(app: &App) -> Result<Box<dyn Composer>, String> {
    let c = Compositor::<smithay::backend::renderer::gles::GlesRenderer>::new(app)?;
    if c.renderer.dmabuf_formats().iter().next().is_none() {
        return Err("renderer gles: EGL imports no dma-buf (EGL_EXT_image_dma_buf_import)".into());
    }
    Ok(Box::new(c))
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
    let placements = app.drawn_placements();
    let mut out = Vec::new();
    let sofa = app.sofa.show();
    let (scale, mut below_cursor) = (Scale::from(app.output.current_scale().fractional_scale()), 0);
    for layer in crate::paint::order_with(&placements, sofa) {
        match layer {
            Layer::Cursor => {
                out.extend(crate::cursor::elements(app, renderer));
                below_cursor = out.len();
            }
            Layer::System => {
                out.extend(crate::install_draw::elements(app, renderer));
                // DB3: Durchblick's panel and frames over everything but the installer's question.
                out.extend(crate::db_draw::elements(app, renderer));
            }
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
            // FELD, REWIND: the Feld and the rewind view share the library's layer (only one shows at a time).
            Layer::Library => {
                out.extend(crate::rewind_draw::elements(app, renderer));
                out.extend(crate::feld_draw::elements(app, renderer));
                out.extend(crate::library_draw::elements(app, renderer));
            }
            Layer::Sofa => out.extend(crate::sofa_draw::elements(app, renderer)),
            // The frame above the window: its bottom corners round off the client's corners.
            Layer::Frame(i) => {
                // DB3: whether anything but its own frame lies over the window Durchblick takes a picture of.
                crate::db_shot::cover(app, &placements[i].win, &out[below_cursor..], scale);
                out.extend(deco::frame(app, renderer, &placements[i]));
            }
            Layer::Veil(i) => {
                crate::db_shot::cover(app, &placements[i].win, &out[below_cursor..], scale);
                out.extend(crate::veil::elements(app, renderer, &placements[i]));
            }
            Layer::Content(i) => {
                window_content(app, renderer, &placements[i], &mut out);
                out.extend(crate::fx::plan::window(app, &placements[i]).into_iter().map(CompElement::from));
            }
            Layer::Chrome => out.extend(crate::chrome_render::elements(app, renderer)),
        }
    }
    crate::db_shot::frame_done(app);
    let covered = placements.iter().any(|p| p.fullscreen && p.visible);
    // GM-6a: a sofa screen is opaque: its own ground replaces the wallpaper.
    if sofa != crate::sofa::Show::Shell {
        out.extend(crate::fx::plan::ground(app, covered).into_iter().map(CompElement::from));
        // DS-2: the wallpaper is the bottom of every frame (none under a fullscreen window).
        out.extend(crate::chrome_render::wallpaper::elements(app, renderer, &placements));
    }
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
