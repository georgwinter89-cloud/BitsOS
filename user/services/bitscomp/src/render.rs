//! Software composition with pixman into a shadow image; back ends copy only the damaged parts out
//! (a host window now, `/dev/fb0` in WY-1, where reading back from the framebuffer would be slow).

use std::time::{Duration, Instant};

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{
    AsRenderElements, RenderElementStates, default_primary_scanout_output_compare, render_elements,
};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{Bind, Color32F, Renderer};
use smithay::desktop::utils::{
    OutputPresentationFeedback, surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
    update_surface_primary_scanout_output,
};
use smithay::reexports::pixman::{FormatCode, Image};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{Monotonic, Physical, Rectangle, Scale, Time};
use smithay::wayland::presentation::Refresh;

use smithay::desktop::Window;

use crate::deco;
use crate::layout::Placement;
use crate::state::App;

render_elements! {
    pub CompElement<=PixmanRenderer>;
    Surface=WaylandSurfaceRenderElement<PixmanRenderer>,
    Solid=SolidColorRenderElement,
    Memory=MemoryRenderBufferRenderElement<PixmanRenderer>,
}

pub struct Compositor {
    pub pixman: PixmanRenderer,
    pub shadow: Image<'static, 'static>,
    damage: OutputDamageTracker,
    seq: u64,
}

impl Compositor {
    pub fn new(app: &App) -> Self {
        let size = app.output.current_mode().unwrap().size;
        Self {
            pixman: PixmanRenderer::new().expect("pixman renderer"),
            shadow: Image::new(FormatCode::X8R8G8B8, size.w as usize, size.h as usize, true).expect("shadow image"),
            damage: OutputDamageTracker::from_output(&app.output),
            seq: 0,
        }
    }

    pub fn size(&self) -> (i32, i32) {
        (self.shadow.width() as i32, self.shadow.height() as i32)
    }

    /// Renders one frame; returns the damaged rectangles (empty when nothing changed on screen).
    pub fn render(&mut self, app: &mut App) -> Vec<Rectangle<i32, Physical>> {
        let mode = app.output.current_mode().unwrap().size;
        if (mode.w, mode.h) != self.size() {
            *self = Self::new(app);
        }
        let t0 = Instant::now();
        let elements = build_elements(app, &mut self.pixman);
        let clear: Color32F = app.theme.bg.into();
        let mut target = self.pixman.bind(&mut self.shadow).expect("bind shadow");
        let result = self.damage.render_output(&mut self.pixman, &mut target, 1, &elements, clear).expect("render");
        let damage = result.damage.cloned().unwrap_or_default();
        let states = result.states;
        drop(target);
        app.stats.render += t0.elapsed();
        app.stats.frames += 1;
        if damage.is_empty() {
            app.stats.empty_frames += 1;
        }
        app.stats.damaged_px += damage.iter().map(|r| (r.size.w * r.size.h) as u64).sum::<u64>();
        self.after_frame(app, &states);
        damage
    }

    /// Frame callbacks and presentation feedback. Hidden stack pages get a callback once a second only,
    /// so a browser behind another page does not keep drawing at full rate. B4 (WY-4): the frame never waits for
    /// a client: a callback says "this frame was drawn", with whatever the client had committed by then; a
    /// client that has not drawn keeps its last buffer on screen. A hung client gets none (they would only pile up).
    fn after_frame(&mut self, app: &mut App, states: &RenderElementStates) {
        let output = app.output.clone();
        // The one clock (`pacer`): callbacks carry the frame's time, the frame is on screen one period later.
        let now: Time<Monotonic> = app.frame_time.into();
        let shown: Time<Monotonic> = (app.frame_time + app.refresh).into();
        let mut feedback = OutputPresentationFeedback::new(&output);
        for w in app.all_windows() {
            if app.is_hung(&w) {
                continue;
            }
            w.with_surfaces(|surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    &output,
                    data,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
            w.send_frame(&output, now, Some(Duration::from_secs(1)), surface_primary_scanout_output);
            w.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, |surface, _| {
                surface_presentation_feedback_flags_from_states(surface, states)
            });
        }
        self.seq += 1;
        feedback.presented(shown, Refresh::Fixed(app.refresh), self.seq, wp_presentation_feedback::Kind::empty());
        let _ = self.pixman.cleanup_texture_cache();
    }
}

fn build_elements(app: &mut App, pixman: &mut PixmanRenderer) -> Vec<CompElement> {
    // The damage tracker wants the topmost element first; `paint::order` says which (WY-5c: floats over the bars).
    use crate::paint::Layer;
    let placements = app.placements();
    let mut out = Vec::new();
    for layer in crate::paint::order(&placements) {
        match layer {
            Layer::Cursor => out.extend(crate::cursor::elements(app, pixman)),
            Layer::Overlays => out.extend(deco::overlays(app, pixman)),
            Layer::Over => out.extend(crate::chrome_render::over(app, pixman)),
            // The frame above the window: its bottom corners round off the client's corners.
            Layer::Frame(i) => out.extend(deco::frame(app, pixman, &placements[i])),
            Layer::Veil(i) => out.extend(crate::veil::elements(app, pixman, &placements[i])),
            Layer::Content(i) => window_content(app, pixman, &placements[i], &mut out),
            Layer::Chrome => out.extend(crate::chrome_render::elements(app, pixman)),
        }
    }
    out
}

/// A window's own picture (under its veil and frame).
fn window_content(app: &mut App, pixman: &mut PixmanRenderer, p: &Placement<Window>, out: &mut Vec<CompElement>) {
    let scale = Scale::from(app.theme.scale);
    let origin = app.window_origin(&p.win, p.frame).to_physical_precise_round(scale);
    // WY-5b: a settings page's content is bitscomp's own image (its client only holds the window).
    if let Some(pid) = app.page_pid(&p.win) {
        let c = app.content_rect(p.frame);
        out.extend(app.page_elements(pixman, pid, c));
        return;
    }
    out.extend(p.win.render_elements::<CompElement>(pixman, origin, scale, 1.0));
}
