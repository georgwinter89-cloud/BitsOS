//! WY-15 (gx-scout-mesa.md step C1): the GLES renderer of the gpu build. Smithay's `GlesRenderer` on an EGL display
//! of Mesa's surfaceless platform (no window system, no KMS): the frame is drawn into a GL texture, the damaged
//! rectangles are read back (`ExportMem::copy_framebuffer`) into a CPU-side XRGB8888 buffer, and the back ends copy
//! that to `/dev/fb0` exactly as Pixman's shadow image (DP-6 damage copy). So the picture path stays fbdev.
//!
//! Today `gles` always means Mesa's software rasteriser (llvmpipe, `LIBGL_ALWAYS_SOFTWARE`): in QEMU there is no
//! GPU, and on the Mini-PC the 780M takes no commands yet (roadmap wave 25 is its first one); radeonsi comes with a
//! later item. glvnd and Mesa find the tree's files through environment variables set for bitscomp only
//! (`set_env`); the programs bitscomp starts never get them (`flavour::GLES_ENV`, removed in `App::spawn`).

use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTexture, ffi};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Offscreen};
use smithay::utils::{Physical, Rectangle};

use crate::readback::{self, Order};
use crate::render::{CompElement, CompRenderer};

/// The GLES renderer's picture: the GL texture it draws into, kept across frames (the damage tracker draws only
/// what changed, age 1), and its read-back copy that `pixels` hands out.
pub struct GlesPicture {
    tex: GlesTexture,
    px: Vec<u32>,
    w: i32,
    h: i32,
    /// Which byte order GL reads in: BGRA first; RGBA (every GLES has it) once BGRA was refused.
    order: Order,
    /// The next frame reads the whole picture back, not only its damage: true at first and after a frame whose
    /// readback failed (the texture then holds more than `px`, and the damage tracker would not draw it again).
    full: readback::Full,
    /// WY-16: the sync point of a frame whose GPU work outlasted `GPU_WAIT`; client buffers stay held until it is
    /// reached (`gpu_idle`).
    busy: Option<smithay::backend::renderer::sync::SyncPoint>,
}

/// WY-16: how long a frame waits for its GPU work on the main loop (B4: never unbounded; a client dma-buf whose
/// implicit fence never signals must not stop input and every other client). A time, not a count; several frames
/// of a slow llvmpipe in QEMU.
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
        if crate::flavour::egl_probe() != "loadable" {
            return Err("gles: libEGL.so.1 does not load".into());
        }
        // SAFETY: every EGL display in bitscomp comes from this call (Smithay's condition for `new`: no foreign
        // code calls eglGetPlatformDisplay/eglTerminate), and the surfaceless platform needs no native handle
        // (EGL_DEFAULT_DISPLAY). On a mode change `follow_mode` makes the new compositor while the old one lives:
        // EGL hands out the same display then, and Smithay shares one handle for it (egl/display.rs:236-246, its
        // DISPLAYS set) whose eglTerminate runs only when the last renderer drops, so dropping the old one leaves
        // the new context working whatever Mesa does on a second eglInitialize (wy/98 changes the mode seven times).
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.map_err(|e| format!("gles: EGL display: {e}"))?;
        let context = EGLContext::new(&display).map_err(|e| format!("gles: EGL context: {e}"))?;
        // SAFETY: the context is new and handed over whole; nothing else makes it current on this thread.
        let mut r = unsafe { GlesRenderer::new(context) }.map_err(|e| format!("gles: renderer: {e}"))?;
        let gl = gl_strings(&mut r);
        let soft = ["llvmpipe", "softpipe", "swrast"].iter().any(|n| gl.contains(n));
        SOFTWARE.store(soft, std::sync::atomic::Ordering::Relaxed);
        eprintln!("bitscomp: renderer gles: {gl}; {} threads in bitscomp", threads());
        Ok(r)
    }

    fn target(&mut self, w: i32, h: i32) -> Result<Self::Target, String> {
        let (w, h) = (w.max(1), h.max(1));
        // RGBA8, the colour format every GLES can render into; read back as BGRA (`draw`).
        let tex = Offscreen::<GlesTexture>::create_buffer(self, Fourcc::Abgr8888, (w, h).into())
            .map_err(|e| format!("gles: no {w}x{h} texture: {e}"))?;
        Ok(GlesPicture { tex, px: vec![0; w as usize * h as usize], w, h, order: Order::Bgra, full: readback::Full::new(), busy: None })
    }

    fn draw(
        &mut self,
        target: &mut Self::Target,
        tracker: &mut OutputDamageTracker,
        elements: &[CompElement<Self>],
        clear: Color32F,
    ) -> Result<(Vec<Rectangle<i32, Physical>>, RenderElementStates), String> {
        let GlesPicture { tex, px, w, h, order, full, busy } = target;
        let mut fb = self.bind(tex).map_err(|e| format!("gles bind: {e}"))?;
        let was_full = full.begin();
        let result = tracker.render_output(self, &mut fb, 1, elements, clear).map_err(|e| format!("gles render: {e:?}"))?;
        // WY-16: the GPU's work on client buffers is over before the frame ends, so `release` may give them back
        // after it. Bounded (B4): past `GPU_WAIT` the frame is not read back (that would wait for the GPU too), the
        // next one reads the whole picture (`full` stays set) and the held buffers wait for this point (`gpu_idle`).
        if !wait_bounded(&result.sync, GPU_WAIT) {
            if busy.is_none() {
                eprintln!("bitscomp: render gles: the frame's GPU work outlasted {} ms; buffers stay held, the frame is read back later", GPU_WAIT.as_millis());
            }
            *busy = Some(result.sync);
            return Ok((Vec::new(), result.states));
        }
        if busy.take().is_some() {
            eprintln!("bitscomp: render gles: the GPU finished a late frame");
        }
        let rects: Vec<_> = result.damage.into_iter().flatten().map(|r| (r.loc.x, r.loc.y, r.size.w, r.size.h)).collect();
        let states = result.states;
        let plan = readback::plan(&rects, was_full, *w, *h);
        // The readback is part of the frame: `render_avg_ms` counts it (the cost GLES adds over Pixman on fb0).
        for &rect in &plan {
            // Picture coordinates as they are: the texture's row 0 is the picture's top row (readback.rs).
            let region = Rectangle::new((rect.0, rect.1).into(), (rect.2, rect.3).into());
            let mapping = readback::read_with_fallback(
                order,
                |e| matches!(e, GlesError::UnsupportedPixelFormat(_)),
                |o| self.copy_framebuffer(&fb, region, fourcc(o)),
            )
            .map_err(|e| format!("gles readback: {e}"))?;
            let bytes = self.map_texture(&mapping).map_err(|e| format!("gles readback map: {e}"))?;
            readback::copy_rows(px, *w as usize, rect, bytes, *order);
        }
        full.done();
        let damage = plan.iter().map(|r| Rectangle::new((r.0, r.1).into(), (r.2, r.3).into())).collect();
        Ok((damage, states))
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

    fn maps_dmabufs(&self) -> bool {
        SOFTWARE.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn gpu_idle(target: &mut Self::Target) -> bool {
        // Later GL work finishes after earlier work: a reached later point covers the late frame too.
        if target.busy.as_ref().is_some_and(|s| s.is_reached()) {
            target.busy = None;
        }
        target.busy.is_none()
    }
}

/// WY-16: GL_RENDERER names a software rasteriser (llvmpipe today, always): its EGL dma-buf import is an mmap.
static SOFTWARE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn fourcc(order: Order) -> Fourcc {
    match order {
        Order::Bgra => Fourcc::Argb8888,
        Order::Rgba => Fourcc::Abgr8888,
    }
}

/// GL_RENDERER and GL_VERSION, for the log: says which driver draws (llvmpipe).
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

/// How many threads bitscomp has (llvmpipe starts its own rasteriser threads).
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0)
}

/// The variables glvnd and Mesa need to find the tree's files instead of /usr's: the EGL vendor file, the DRI and
/// GBM drivers, the driver defaults; llvmpipe forced; no shader cache written (no home to write it to on V2, and
/// it must never land on the stick). Called by the gpu build before any thread exists, only when GLES is chosen.
pub fn set_env(tree: &str) {
    let tree = tree.trim_end_matches('/');
    let vars = env_values(tree);
    for (k, v) in &vars {
        // SAFETY: called from `main` before `run` starts any thread (the hand-over check was a process, not a
        // thread), so nothing reads the environment concurrently.
        unsafe { std::env::set_var(k, v) };
    }
}

/// The values `set_env` sets, every name of `flavour::GLES_ENV`.
pub fn env_values(tree: &str) -> Vec<(&'static str, String)> {
    vec![
        ("__EGL_VENDOR_LIBRARY_FILENAMES", format!("{tree}/share/glvnd/egl_vendor.d/50_mesa.json")),
        ("LIBGL_DRIVERS_PATH", format!("{tree}/lib/dri")),
        ("GBM_BACKENDS_PATH", format!("{tree}/lib/gbm")),
        ("DRIRC_CONFIGDIR", format!("{tree}/share/drirc.d")),
        ("LIBGL_ALWAYS_SOFTWARE", "1".into()),
        ("MESA_SHADER_CACHE_DISABLE", "true".into()),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn env_names_are_the_ones_children_lose() {
        let vals = super::env_values("/t");
        let names: Vec<&str> = vals.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, crate::flavour::GLES_ENV);
        assert_eq!(vals[0].1, "/t/share/glvnd/egl_vendor.d/50_mesa.json");
    }
}
