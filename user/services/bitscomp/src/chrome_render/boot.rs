//! DS-5: bitscomp's half of the boot screen (plan/spec/desktop.md §0 answer 4). When the kernel handed its boot
//! screen to this bitscomp (`/proc/bitsos/boot`: `screen handed`, the desktop step from this start), the first
//! frame is the same picture (`boot_draw`), lying over the whole desktop, which is built under it as usual. The
//! scene goes on with the kernel's list (read again every `POLL` while it is up, a wake through `Status::due`, no
//! frame when nothing changed) and bitscomp's own steps in the desktop row: its start, the shell and status
//! services, the first program's window. Once the desktop is ready the scene fades out in `FADE` (cheap level;
//! motion off: at once); it never waits for itself, and `CAP` ends a wait for a window that does not come. A key
//! skips the fade: the desktop shows as soon as its own part (settings, status) is ready. Anything odd (no file,
//! another format, a small screen, a desktop step of an earlier start) means no scene, straight to the desktop.

use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform};

use super::boot_art::{Frame, Rect};
use super::boot_draw::{Art, Scene, State, View};
use super::boot_model::{Own, Steps, own};
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::theme::Motion;

pub const PATH: &str = "/proc/bitsos/boot";
/// Another file instead of /proc/bitsos/boot (host tests: a headless bitscomp shows the scene from a fixture).
pub const PATH_ENV: &str = "BITSCOMP_BOOT_FILE";
/// The list is read again this often while the scene is up (DS-4: "e.g. 10 Hz").
pub const POLL: Duration = Duration::from_millis(100);
/// The cheap level's transition (at most 400 ms, Georg's answer 7; the circle wipe is the GPU level's).
pub const FADE: Duration = Duration::from_millis(300);
/// The longest the scene waits for the first program's window (scout's WY-23: hard cap 1.8 s).
pub const CAP: Duration = Duration::from_millis(1800);
/// How long the desktop waits for settingsd's first answer (so it opens in the user's theme, not the default).
pub const SETTINGS_WAIT: Duration = Duration::from_millis(1000);
/// How long before this bitscomp's start the kernel may have marked the desktop step running (spawn, exec, load).
const LAUNCH_MS: u64 = 10_000;
/// How much earlier than our start (`App::new`) a step marked done may be and still be ours: the take of the
/// screen (fbdev/KD_GRAPHICS) comes before `App::new` (input devices, the grant and dropping root lie between,
/// 130-250 ms in QEMU), and the kernel marks the step done at its next poll (console-cmds POLL_NS, 100 ms) after
/// the take (INT-W27: at 100 ms ds/61-boot-qemu lost the scene on most runs). A restart is still refused: it
/// comes at least a second after the earlier handover.
const HANDOVER_SLACK_MS: u64 = 500;

#[derive(Default)]
pub struct Boot {
    phase: Phase,
}

#[derive(Default)]
enum Phase {
    #[default]
    Unknown,
    Up(Box<Up>),
    Gone,
}

struct Up {
    path: String,
    art: Art,
    scene: Scene,
    buf: MemoryRenderBuffer,
    kernel: Steps,
    view: View,
    own: Own,
    own_ms: u64,
    program: String,
    since: Duration,
    next_poll: Duration,
    keys: u64,
    key: bool,
    fade: Option<Duration>,
    read_failed: bool,
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Whether the file says this bitscomp took the kernel's boot screen; Err: why not (for the log). `start_ms` is
/// this process's start on the kernel's clock: a desktop step marked done before it is an earlier bitscomp's
/// handover (a restart must not replay the scene over a desktop that was already shown).
pub fn eligible(s: &Steps, start_ms: u64) -> Result<(), String> {
    if s.screen != "handed" && s.screen != "shown" {
        return Err(format!("the kernel's boot screen is {}", if s.screen.is_empty() { "unknown" } else { &s.screen }));
    }
    let ours = |d: &super::boot_model::Step| match d.state {
        State::Running => d.t_ms + LAUNCH_MS >= start_ms,
        State::Done => d.t_ms + HANDOVER_SLACK_MS >= start_ms,
        _ => false,
    };
    match s.get("desktop") {
        Some(d) if ours(d) => Ok(()),
        Some(d) => Err(format!("the desktop step ({:?} at {} ms) is not this start's", d.state, d.t_ms)),
        None => Err(String::from("no desktop step")),
    }
}

// `shown` and `App::boot_key` are called from input.rs (a key ends the scene at once and reaches no program) and from
// the pointer code (no pointer during the scene).
impl Boot {
    /// The scene is on screen.
    pub fn shown(&self) -> bool {
        matches!(self.phase, Phase::Up(_))
    }
}

impl App {
    /// A key while the scene is up (input.rs keeps it from the programs and calls this at once).
    pub fn boot_key(&mut self) {
        if let Phase::Up(up) = &mut self.images.boot.phase {
            up.key = true;
            self.needs_frame = true;
        }
    }
}

impl App {
    /// Called first by every frame (`deco::overlays`, the topmost layer bitscomp draws itself): decides once,
    /// follows the steps, ends the scene; the scene's element while it is up.
    pub(crate) fn boot_frame<R: CompRenderer>(&mut self, renderer: &mut R) -> Option<CompElement<R>> {
        let now = crate::pacer::now();
        if matches!(self.images.boot.phase, Phase::Unknown) {
            self.images.boot.phase = match self.boot_begin(now) {
                Ok(up) => Phase::Up(Box::new(up)),
                Err(why) => {
                    if !why.is_empty() {
                        eprintln!("bitscomp: boot scene: none ({why})");
                    }
                    Phase::Gone
                }
            };
        }
        if !matches!(self.images.boot.phase, Phase::Up(_)) {
            return None;
        }
        let alpha = self.boot_tick(now)?;
        let Phase::Up(up) = &self.images.boot.phase else { return None };
        let full = self.area_full();
        let o = crate::header::px_rect(full, self.theme.scale);
        let src: Rectangle<f64, Logical> = Rectangle::from_size(Size::from((up.scene.w as f64, up.scene.h as f64)));
        let at: Point<f64, Physical> = Point::from((o.x as f64, o.y as f64));
        let size: Size<i32, Logical> = (full.w, full.h).into();
        let a = (alpha < 1.0).then_some(alpha.max(0.0));
        MemoryRenderBufferRenderElement::from_buffer(renderer, at, &up.buf, a, Some(src), Some(size), Kind::Unspecified)
            .ok()
            .map(CompElement::from)
    }

    fn boot_begin(&mut self, now: Duration) -> Result<Up, String> {
        let env = std::env::var(PATH_ENV).ok().filter(|p| !p.is_empty());
        if env.is_none() && self.headless {
            return Err(String::new());
        }
        let path = env.unwrap_or_else(|| PATH.to_string());
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        let kernel = Steps::parse(&text).ok_or_else(|| format!("{path}: not the format bitsos-boot 1"))?;
        eligible(&kernel, ms(now).saturating_sub(ms(self.start.elapsed())))?;
        let (w, h) = super::wallpaper::output_px(self);
        let scene = Scene::new(w, h).ok_or_else(|| format!("the screen is {w}x{h}, the scene needs 2560x1440"))?;
        let t0 = Instant::now();
        let art = Art::new();
        let t_art = t0.elapsed();
        let program = self.boot_program();
        let own = Own::Services;
        let own_ms = ms(now);
        let view = kernel.with_desktop(Some(&own.step(&program, own_ms))).view(&art);
        let px = scene.picture(&view, &art);
        let opaque: Rectangle<i32, Buffer> = Rectangle::from_size(Size::from((w, h)));
        let buf = MemoryRenderBuffer::from_slice(&bytes(&px), Fourcc::Argb8888, (w, h), 1, Transform::Normal, Some(vec![opaque]));
        eprintln!(
            "bitscomp: boot scene: shown ({path}: seq {}, the kernel drew {}, screen {}); pieces {} ms, picture {} ms, {} ms after bitscomp's start",
            kernel.seq,
            kernel.drawn,
            kernel.screen,
            t_art.as_millis(),
            (t0.elapsed() - t_art).as_millis(),
            self.start.elapsed().as_millis()
        );
        self.anim.hold = true;
        self.status.boot_poll = Some(now + POLL);
        Ok(Up {
            path,
            art,
            scene,
            buf,
            kernel,
            view,
            own,
            own_ms,
            program,
            since: now,
            next_poll: now + POLL,
            keys: self.stats.key_presses,
            key: false,
            fade: None,
            read_failed: false,
        })
    }

    /// The first program bitscomp started (the window the desktop waits for), "" when none.
    fn boot_program(&self) -> String {
        let argv = self.children.iter().find_map(|(c, _)| self.spawned.get(&c.id()));
        argv.and_then(|a| a.first()).map(|p| p.rsplit('/').next().unwrap_or(p).to_string()).unwrap_or_default()
    }

    /// One frame of the scene: its alpha, None once the desktop shows alone.
    fn boot_tick(&mut self, now: Duration) -> Option<f32> {
        let motion_off = self.theme.motion == Motion::Off;
        let settings = self.settings_live || self.settings_writer.is_none();
        let status = !self.status.clock.is_empty();
        let window_expected = !self.children.is_empty();
        let window = !window_expected || self.space.elements().next().is_some();
        let keys = self.stats.key_presses;
        let start = self.start;
        let program = self.boot_program();
        let Phase::Up(up) = &mut self.images.boot.phase else { return None };
        if key_seen(keys, up.keys) {
            up.key = true;
        }
        if let Some(f0) = up.fade {
            if up.key || now >= f0 + FADE {
                return self.boot_end(now, "faded");
            }
            let k = (now.saturating_sub(f0).as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
            return Some(1.0 - k * k * (3.0 - 2.0 * k));
        }
        if now >= up.next_poll {
            up.next_poll = now + POLL;
            match std::fs::read_to_string(&up.path).ok().as_deref().and_then(Steps::parse) {
                Some(s) => up.kernel = s,
                None if !up.read_failed => {
                    up.read_failed = true;
                    eprintln!("bitscomp: boot scene: {} unreadable now, the list keeps its last state", up.path);
                }
                None => {}
            }
        }
        let waited = now.saturating_sub(up.since);
        let next = own(settings || waited >= SETTINGS_WAIT, status, window);
        let capped = waited >= CAP;
        if up.program.is_empty() {
            up.program = program;
        }
        if next != up.own {
            eprintln!("bitscomp: boot scene: {} after {} ms", next.detail(&up.program), ms(waited));
            up.own = next;
            up.own_ms = ms(now);
        }
        let reason = ends(up.key, next, capped);
        let end = reason.is_some();
        let reason = reason.unwrap_or("");
        if end {
            up.own = Own::Ready;
            up.own_ms = ms(now);
        }
        let steps = up.kernel.with_desktop(Some(&up.own.step(&up.program, up.own_ms)));
        let view = steps.view(&up.art);
        if view != up.view {
            redraw(up, &view);
        }
        if !end {
            self.status.boot_poll = Some(up.next_poll);
            return Some(1.0);
        }
        eprintln!(
            "bitscomp: boot scene: desktop ready {} ms after bitscomp's start ({reason}; scene {} ms, settings {}, window {})",
            start.elapsed().as_millis(),
            ms(waited),
            if self.settings_live { "live" } else if self.settings_writer.is_none() { "not connected" } else { "built-in" },
            if window_expected { if window { "shown" } else { "not yet" } } else { "none started" }
        );
        self.status.boot_poll = None;
        self.anim.hold = false;
        self.anim.settle(now);
        if !fades(reason, motion_off) {
            return self.boot_end(now, reason);
        }
        // WY-19c: the gpu level's way into the desktop: the scene is the old picture of the circle wipe.
        if self.theme.effects_gpu {
            self.fx_play(crate::fx::plan::WIPE);
            return self.boot_end(now, "wipe");
        }
        up.fade = Some(now);
        self.anim.boot_fade = Some(now + FADE);
        Some(1.0)
    }

    /// WY-19b: the boot scene is up: nothing moves over it, it shows the kernel's picture to the pixel.
    pub(crate) fn boot_shown(&self) -> bool {
        matches!(self.images.boot.phase, Phase::Up(_))
    }

    fn boot_end(&mut self, now: Duration, how: &str) -> Option<f32> {
        if let Phase::Up(up) = &self.images.boot.phase {
            eprintln!(
                "bitscomp: boot scene: desktop shown {} ms after bitscomp's start ({how}; transition {} ms)",
                self.start.elapsed().as_millis(),
                up.fade.map_or(0, |f| ms(now.saturating_sub(f)))
            );
        }
        self.images.boot.phase = Phase::Gone;
        self.anim.hold = false;
        self.anim.boot_fade = None;
        self.status.boot_poll = None;
        None
    }
}

/// Whether the scene ends now and why. A key ends it as soon as bitscomp's own part (settings, status) is ready
/// or at the cap, never earlier: the desktop behind it would open half themed; the key stays noted until then.
pub fn ends(key: bool, next: Own, capped: bool) -> Option<&'static str> {
    let shell_ready = next != Own::Services;
    match () {
        _ if key && (shell_ready || capped) => Some("key"),
        _ if next == Own::Ready => Some("ready"),
        _ if capped => Some(if shell_ready { "cap (no window yet)" } else { "cap (settings or status missing)" }),
        _ => None,
    }
}

/// Whether the end fades (FADE) or cuts: a key or motion off shows the desktop at once (transition 0).
pub fn fades(reason: &str, motion_off: bool) -> bool {
    !motion_off && reason != "key"
}

/// A key while the scene is up (the evdev count grew since the last frame).
pub fn key_seen(key_presses: u64, seen: u64) -> bool {
    key_presses > seen
}

/// Draws the parts of the picture that changed into the scene's buffer (only those rectangles are damaged).
fn redraw(up: &mut Up, view: &View) {
    let rects: Vec<Rect> = up.scene.changes(&up.view, view).into_iter().map(|r| r.cut(up.scene.full())).filter(|r| !r.is_empty()).collect();
    let (scene, art, w) = (&up.scene, &up.art, up.scene.w);
    let mut ctx = up.buf.render();
    let _ = ctx.draw(|mem: &mut [u8]| {
        let mut damage = Vec::new();
        for r in &rects {
            let mut f = Frame::new(*r);
            scene.draw(&mut f, view, art);
            for row in 0..r.h {
                let src = &f.px[(row * r.w) as usize..((row + 1) * r.w) as usize];
                let at = (((r.y + row) * w + r.x) * 4) as usize;
                if let Some(dst) = mem.get_mut(at..at + src.len() * 4) {
                    dst.copy_from_slice(&bytes(src));
                }
            }
            damage.push(Rectangle::<i32, Buffer>::new((r.x, r.y).into(), (r.w, r.h).into()));
        }
        Ok::<_, ()>(damage)
    });
    drop(ctx);
    up.view = view.clone();
}

/// XRGB pixels as opaque ARGB8888 bytes.
fn bytes(px: &[u32]) -> Vec<u8> {
    px.iter().flat_map(|p| (p | 0xFF00_0000).to_le_bytes()).collect()
}
