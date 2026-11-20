//! WY-15: the GLES renderer. Smithay's `GlesRenderer` on an EGL display of Mesa's surfaceless platform (no window
//! system, no KMS), radeonsi on the render node.
//!
//! WY-17: every frame goes straight into fbdev's windows (`adopt_scanout`, `backend::scanout`): the windows are
//! dma-bufs imported as render targets, the frame is drawn into the hidden one and flipped, and nothing is read back
//! but on request (`sync_pixels`: `ctl shot`, Durchblick's picture, the bench).
//!
//! The effect shaders are compiled when the renderer is made (`create`); the tree's environment is `mesa::set_env`.

use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Format, Fourcc, Modifier};
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture, ffi};
use smithay::backend::renderer::{Bind, Blit, Color32F, ExportMem, TextureFilter};
use smithay::utils::{Physical, Rectangle};

use crate::backend::scanout::{Aim, Redraw, Windows};
use crate::readback::{self, Order};
use crate::render::{CompElement, CompRenderer};

/// The GLES renderer's picture: the scanout windows it draws into, and the read-back copy of the frame on the screen
/// that `pixels` hands out (filled by `sync_pixels` only).
pub struct GlesPicture {
    px: Vec<u32>,
    w: i32,
    h: i32,
    /// Which byte order GL reads in: BGRA first; RGBA (every GLES has it) once BGRA was refused.
    order: Order,
    /// WY-16: the sync point of a frame whose GPU work outlasted `GPU_WAIT`; client buffers stay held until it is
    /// reached (`gpu_idle`).
    busy: Option<smithay::backend::renderer::sync::SyncPoint>,
    /// WY-17: the scanout windows as render targets (None until `adopt_scanout`).
    direct: Option<Direct>,
}

/// WY-17: the windows and the frame under way.
struct Direct {
    bufs: Vec<Dmabuf>,
    aim: Aim,
    /// The window of the last frame that was drawn: what the screen shows once it flipped (`sync_pixels`).
    drawn: Option<usize>,
    redraw: Redraw,
}

/// WY-16: how long a frame waits for its GPU work on the main loop (B4: never unbounded; a client dma-buf whose
/// implicit fence never signals must not stop input and every other client). A time, not a count.
const GPU_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Waits at most `limit` for `sync` (an EGL fence with a timeout; a point without a fence is reached).
fn wait_bounded(sync: &smithay::backend::renderer::sync::SyncPoint, limit: std::time::Duration) -> bool {
    use smithay::backend::egl::fence::EGLFence;
    match sync.get::<EGLFence>() {
        Some(f) => f.client_wait(Some(limit), true).unwrap_or(false),
        None => sync.is_reached(),
    }
}

impl CompRenderer for GlesRenderer {
    type Tex = GlesTexture;
    type Target = GlesPicture;
    const NAME: &'static str = "gles";

    fn create() -> Result<Self, String> {
        // Smithay's EGL loader panics when libEGL.so.1 does not load (egl/ffi.rs:148): never call it then.
        if crate::mesa::egl_probe() != "loadable" {
            return Err("gles: libEGL.so.1 does not load".into());
        }
        // SAFETY: every EGL display in bitscomp comes from this call (Smithay's condition for `new`: no foreign
        // code calls eglGetPlatformDisplay/eglTerminate), and the surfaceless platform needs no native handle
        // (EGL_DEFAULT_DISPLAY). On a mode change `follow_mode` makes the new compositor while the old one lives:
        // EGL hands out the same display then, and Smithay shares one handle for it (egl/display.rs:236-246, its
        // DISPLAYS set) whose eglTerminate runs only when the last renderer drops, so dropping the old one leaves
        // the new context working whatever Mesa does on a second eglInitialize.
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.map_err(|e| format!("gles: EGL display: {e}"))?;
        // WY-21: a robust context, so that a lost GPU is reported (`reset_status`) instead of Mesa ending the process at
        // the next submission it cannot make (amdgpu winsys: a non-robust context aborts).
        let (context, robust) = match robust_context(&display) {
            Ok(c) => (c, true),
            Err(e) => {
                eprintln!("bitscomp: renderer gles: no robust context ({e}); a GPU reset ends the process");
                (EGLContext::new(&display).map_err(|e| format!("gles: EGL context: {e}"))?, false)
            }
        };
        ROBUST.store(robust, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: the context is new and handed over whole; nothing else makes it current on this thread.
        let mut r = unsafe { GlesRenderer::new(context) }.map_err(|e| format!("gles: renderer: {e}"))?;
        let gl = gl_strings(&mut r);
        eprintln!("bitscomp: renderer gles: {gl}{}", if robust { "; robust context" } else { "" });
        crate::fx_gl::ready(&mut r).map_err(|e| format!("gles: the effect shaders: {e}"))?;
        Ok(r)
    }

    fn reset_status(&mut self) -> Option<String> {
        if !ROBUST.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let status = self.with_context(|gl| if gl.GetGraphicsResetStatus.is_loaded() { unsafe { gl.GetGraphicsResetStatus() } } else { ffi::NO_ERROR }).ok()?;
        let what = match status {
            ffi::NO_ERROR => return None,
            GUILTY_CONTEXT_RESET => "a reset this context caused",
            INNOCENT_CONTEXT_RESET => "a reset another program caused",
            _ => "a reset of unknown cause",
        };
        Some(format!("the GL context reports {what} ({status:#x})"))
    }

    fn target(&mut self, w: i32, h: i32) -> Result<Self::Target, String> {
        let (w, h) = (w.max(1), h.max(1));
        Ok(GlesPicture { px: vec![0; w as usize * h as usize], w, h, order: Order::Bgra, busy: None, direct: None })
    }

    fn draw(
        &mut self,
        target: &mut Self::Target,
        tracker: &mut OutputDamageTracker,
        elements: &[CompElement<Self>],
        clear: Color32F,
    ) -> Result<(Vec<Rectangle<i32, Physical>>, RenderElementStates), String> {
        let GlesPicture { busy, direct, .. } = target;
        let d = direct.as_mut().ok_or("no scanout windows to draw into")?;
        draw_direct(self, d, busy, tracker, elements, clear)
    }

    fn target_size(target: &Self::Target) -> (i32, i32) {
        (target.w, target.h)
    }

    fn pixels(target: &Self::Target) -> (&[u32], usize) {
        (&target.px, target.w as usize)
    }

    fn cleanup(&mut self) {
        let _ = smithay::backend::renderer::Renderer::cleanup_texture_cache(self);
    }

    fn gpu_idle(target: &mut Self::Target) -> bool {
        // Later GL work finishes after earlier work: a reached later point covers the late frame too.
        if target.busy.as_ref().is_some_and(|s| s.is_reached()) {
            target.busy = None;
        }
        target.busy.is_none()
    }

    fn adopt_scanout(&mut self, target: &mut Self::Target, windows: Windows) -> Result<(), String> {
        if windows.size != (target.w, target.h) {
            return Err(format!("the windows are {}x{}, the picture {}x{}", windows.size.0, windows.size.1, target.w, target.h));
        }
        let code = Fourcc::try_from(windows.fourcc).map_err(|_| format!("format {:#x}", windows.fourcc))?;
        let format = Format { code, modifier: Modifier::Linear };
        if !Bind::<Dmabuf>::supported_formats(self).is_some_and(|f| f.contains(&format)) {
            return Err(format!("EGL renders into no {} buffer", crate::dmabuf::fourcc_name(format)));
        }
        let mut bufs = Vec::with_capacity(windows.list.len());
        for w in windows.list {
            let mut b = Dmabuf::builder((target.w, target.h), code, Modifier::Linear, DmabufFlags::empty());
            b.add_plane(w.fd, 0, w.offset, w.stride);
            bufs.push(b.build().ok_or("a dma-buf without planes")?);
        }
        // The import and the framebuffer are checked now, not at the first frame.
        for b in &mut bufs {
            self.bind(b).map_err(|e| format!("EGL refuses the window: {e}"))?;
        }
        target.direct = Some(Direct { bufs, aim: Aim { window: 1, age: 0 }, drawn: None, redraw: Redraw::default() });
        Ok(())
    }

    fn aim(target: &mut Self::Target, aim: Aim) -> bool {
        match &mut target.direct {
            Some(d) => {
                d.aim = aim;
                true
            }
            None => false,
        }
    }

    fn sync_pixels(&mut self, target: &mut Self::Target) {
        let GlesPicture { px, w, h, order, direct, .. } = target;
        let Some(Direct { bufs, drawn: Some(on_screen), .. }) = direct else { return };
        let rect = (0, 0, *w, *h);
        let read = (|| {
            let fb = self.bind(&mut bufs[*on_screen]).map_err(|e| format!("bind: {e}"))?;
            let region = Rectangle::from_size((*w, *h).into());
            let mapping = readback::read_with_fallback(
                order,
                |e| matches!(e, GlesError::UnsupportedPixelFormat(_)),
                |o| self.copy_framebuffer(&fb, region, fourcc(o)),
            )
            .map_err(|e| format!("read: {e}"))?;
            let bytes = self.map_texture(&mapping).map_err(|e| format!("map: {e}"))?;
            readback::copy_rows(px, *w as usize, rect, bytes, *order);
            Ok::<_, String>(())
        })();
        if let Err(e) = read {
            eprintln!("bitscomp: scanout: reading the frame on the screen failed: {e}");
        }
    }

    fn fx_prepare(&mut self, target: &mut Self::Target, elements: &[CompElement<Self>], scale: smithay::utils::Scale<f64>) {
        let size = (target.w, target.h);
        crate::fx_gl::prepare(self, elements, scale, size, |r, tex| last_picture(r, target, tex));
    }

    fn fx_draw(
        frame: &mut GlesFrame<'_, '_>,
        fx: &crate::fx::FxElement,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        crate::fx_gl::draw(frame, fx, dst, damage)
    }

    fn fx_report(&self) -> String {
        format!("{} late={}", crate::fx_gl::report(self), LATE_FRAMES.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// WY-19c: the last frame on the screen into `tex` (a transition's old picture): the scanout window shown last.
fn last_picture(r: &mut GlesRenderer, target: &mut GlesPicture, tex: &mut GlesTexture) -> Result<(), String> {
    let all: Rectangle<i32, Physical> = Rectangle::from_size((target.w, target.h).into());
    let src = match &mut target.direct {
        Some(Direct { bufs, drawn: Some(i), .. }) => r.bind(&mut bufs[*i]),
        _ => return Err("no frame on the screen yet".into()),
    }
    .map_err(|e| format!("bind the last frame: {e}"))?;
    let mut to = r.bind(tex).map_err(|e| format!("bind the snapshot: {e}"))?;
    r.blit(&src, &mut to, all, all, TextureFilter::Nearest).map_err(|e| format!("blit the last frame: {e}"))
}

/// WY-19: how many frames outlasted their wait (`ctl fx`).
static LATE_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// WY-19: `GPU_WAIT`, or the shorter wait `ctl fx wait` asks for; it can only shorten it, so B4's bound stays.
fn gpu_wait() -> std::time::Duration {
    GPU_WAIT.min(std::time::Duration::from_millis(crate::fx::GPU_WAIT_CUT_MS.load(std::sync::atomic::Ordering::Relaxed)))
}

/// WY-17: one frame straight into the hidden window. Damage as ever, at the age the window has (Smithay keeps the
/// damage of the frames before). When this returns damage the frame is finished in memory and may be flipped: bounded
/// by `GPU_WAIT` (B4). Past it no flip (the empty damage), the window stays hidden, and both windows are redrawn: the
/// tracker has counted a frame that reached neither whole.
fn draw_direct(
    r: &mut GlesRenderer,
    d: &mut Direct,
    busy: &mut Option<smithay::backend::renderer::sync::SyncPoint>,
    tracker: &mut OutputDamageTracker,
    elements: &[CompElement<GlesRenderer>],
    clear: Color32F,
) -> Result<(Vec<Rectangle<i32, Physical>>, RenderElementStates), String> {
    let Aim { window, age } = d.aim;
    let age = d.redraw.start(age);
    let mut fb = r.bind(&mut d.bufs[window]).map_err(|e| format!("gles bind: {e}"))?;
    let result = tracker.render_output(r, &mut fb, age, elements, clear).map_err(|e| format!("gles render: {e:?}"))?;
    let damage = result.damage.cloned().unwrap_or_default();
    if !wait_bounded(&result.sync, gpu_wait()) {
        LATE_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if busy.is_none() {
            eprintln!("bitscomp: render gles: the frame's GPU work outlasted {} ms; no flip, both windows are drawn again", GPU_WAIT.as_millis());
        }
        *busy = Some(result.sync);
        return Ok((Vec::new(), result.states));
    }
    d.redraw.finished();
    if busy.take().is_some() {
        eprintln!("bitscomp: render gles: the GPU finished a late frame");
    }
    if !damage.is_empty() {
        d.drawn = Some(window);
    }
    Ok((damage, result.states))
}

/// WY-21: whether the context was made robust (`robust_context`); only then does GL report a reset.
static ROBUST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// glGetGraphicsResetStatus's answers (GLES 3.2, KHR_robustness).
const GUILTY_CONTEXT_RESET: u32 = 0x8253;
const INNOCENT_CONTEXT_RESET: u32 = 0x8254;

/// WY-21: Smithay's own contexts are not robust: this one is made by hand, LOSE_CONTEXT_ON_RESET
/// (EGL_EXT_create_context_robustness, which radeonsi offers), on a config of the surfaceless display
/// (Smithay's raw constructor needs one; nothing draws into a surface of it). The display stays alive with the context:
/// Smithay's raw display does not end it, and the handle of `display` is never dropped.
fn robust_context(display: &EGLDisplay) -> Result<EGLContext, String> {
    use smithay::backend::egl::ffi::egl;
    if !display.extensions().iter().any(|e| e == "EGL_EXT_create_context_robustness") {
        return Err("the display has no EGL_EXT_create_context_robustness".into());
    }
    let dpy = **display.get_display_handle();
    let want = [egl::RENDERABLE_TYPE as i32, egl::OPENGL_ES2_BIT as i32, egl::SURFACE_TYPE as i32, 0, egl::NONE as i32];
    let mut config: egl::types::EGLConfig = std::ptr::null();
    let mut n = 0;
    // SAFETY: `dpy` is the initialised display of `display`; the attribute list ends with NONE; one config is written.
    let chose = unsafe { egl::ChooseConfig(dpy, want.as_ptr(), &mut config, 1, &mut n) };
    if chose != egl::TRUE || n < 1 || config.is_null() {
        // SAFETY: no argument; reads this thread's last EGL error.
        return Err(format!("no ES 2 config ({:#x})", unsafe { egl::GetError() }));
    }
    let attrs = [
        egl::CONTEXT_CLIENT_VERSION as i32,
        2,
        egl::CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_EXT as i32,
        egl::LOSE_CONTEXT_ON_RESET_EXT as i32,
        egl::NONE as i32,
    ];
    // SAFETY: display and config as above, no shared context, the attribute list ends with NONE.
    let ctx = unsafe { egl::CreateContext(dpy, config, egl::NO_CONTEXT, attrs.as_ptr()) };
    if ctx.is_null() {
        // SAFETY: as above.
        return Err(format!("eglCreateContext {:#x}", unsafe { egl::GetError() }));
    }
    // SAFETY: the three handles are this process's EGL objects from Smithay's own libEGL, valid as long as the display
    // lives, which the guard below and the leaked display handle make for the whole process.
    let context = unsafe { EGLContext::from_raw(dpy, config, ctx) }.map_err(|e| format!("gles: EGL context: {e}"))?;
    context.user_data().insert_if_missing(|| DestroyContext { dpy, ctx });
    std::mem::forget(display.get_display_handle());
    Ok(context)
}

/// Smithay leaves a raw context alive when it drops it: the last clone of the context destroys it here (a mode
/// change makes a new compositor). A lost GPU's compositor is never dropped (`main.rs`), so nothing reaches the driver then.
struct DestroyContext {
    dpy: smithay::backend::egl::ffi::egl::types::EGLDisplay,
    ctx: smithay::backend::egl::ffi::egl::types::EGLContext,
}

impl Drop for DestroyContext {
    fn drop(&mut self) {
        // SAFETY: the context was made on this display, Smithay unbound it when the renderer dropped, and this runs once.
        unsafe { smithay::backend::egl::ffi::egl::DestroyContext(self.dpy, self.ctx) };
    }
}

fn fourcc(order: Order) -> Fourcc {
    match order {
        Order::Bgra => Fourcc::Argb8888,
        Order::Rgba => Fourcc::Abgr8888,
    }
}

/// GL_RENDERER and GL_VERSION, for the log: says which driver draws.
fn gl_strings(r: &mut GlesRenderer) -> String {
    let get = |gl: &ffi::Gles2, name| {
        // SAFETY: the context is current inside `with_context`; GetString returns a static NUL-terminated string
        // or null, which is checked; the text is copied before the closure returns.
        unsafe {
            let p = gl.GetString(name);
            if p.is_null() { "?".to_string() } else { std::ffi::CStr::from_ptr(p.cast()).to_string_lossy().into_owned() }
        }
    };
    r.with_context(|gl| format!("GL_RENDERER={} GL_VERSION={}", get(gl, ffi::RENDERER), get(gl, ffi::VERSION)))
        .unwrap_or_else(|e| format!("GL strings: {e}"))
}
