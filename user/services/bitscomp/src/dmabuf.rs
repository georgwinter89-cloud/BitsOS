//! WY-16: client dma-bufs. zwp_linux_dmabuf_v1 version 4 with a default feedback whose main device is the render node
//! bitscomp draws on (226:128, what Mesa's WSI compares with its own node's rdev: a client on the same GPU shares its
//! VRAM images), and whose one tranche lists the renderer's formats, LINEAR first (`table`). GLES imports them through
//! an EGL image. Every import is checked by the renderer before the client gets its wl_buffer
//! (`Composer::dmabuf_turn`), so a buffer the renderer cannot read (a bad stride, a format EGL refuses) is `failed`,
//! never a crash at draw time.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Format, Fourcc, Modifier};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::dmabuf::{
    DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};

use crate::state::App;

/// The render node clients render on; its char device is the feedback's main device.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";
/// 226:128, the render node's number on Linux and on V2 (DR-2), answered when no node is there (only the host
/// tests: bitscomp itself ends without a node, `mesa::gpu_or_why`).
pub const RENDER_RDEV: (u32, u32) = (226, 128);

pub struct Dma {
    pub state: DmabufState,
    pub global: Option<DmabufGlobal>,
    /// WY-21: globals of renderers that drew before (`renew`, version 3 only).
    pub retired: Vec<DmabufGlobal>,
    /// The global has a default feedback (version 4), which `renew` changes in place.
    pub feedback: bool,
    /// Imports waiting for the renderer's check (the main loop's next `dmabuf_turn`).
    pub pending: Vec<(Dmabuf, ImportNotifier)>,
    /// A wl_buffer was destroyed: the renderer's cache drops its mapping or EGL image at the next turn, not only
    /// at the next frame (the client's udmabuf pages stay pinned while it is mapped, DR-5).
    pub cleanup_due: bool,
    /// Buffers (and release points) replaced by a commit, given back after the frame (`release`).
    pub hold: crate::release::Hold,
    pub sync: Option<crate::syncobj::Syncobj>,
    /// The last frame's GPU work had not finished within the renderer's wait (`CompRenderer::gpu_idle`): held
    /// buffers stay held until it has.
    pub gpu_busy: bool,
}

impl Dma {
    pub fn new() -> Dma {
        Dma {
            state: DmabufState::new(),
            global: None,
            retired: Vec::new(),
            feedback: false,
            pending: Vec::new(),
            cleanup_due: false,
            hold: Default::default(),
            sync: None,
            gpu_busy: false,
        }
    }
}

/// The feedback's format table: LINEAR first (every client can use it, and Mesa's WSI takes the first
/// usable entry it can render), then explicit modifiers, then the implicit one (`Modifier::Invalid`); within each
/// group XRGB8888 and ARGB8888 first, then by fourcc and modifier, so the table is the same every start.
pub fn table(formats: impl IntoIterator<Item = Format>) -> Vec<Format> {
    let mut v: Vec<Format> = formats.into_iter().collect();
    let group = |m: Modifier| match m {
        Modifier::Linear => 0,
        Modifier::Invalid => 2,
        _ => 1,
    };
    let rank = |c: Fourcc| match c {
        Fourcc::Xrgb8888 => 0,
        Fourcc::Argb8888 => 1,
        _ => 2,
    };
    v.sort_by_key(|f| (group(f.modifier), rank(f.code), f.code as u32, u64::from(f.modifier)));
    v.dedup();
    v
}

/// The render node's number and where it was found: the render node when it exists as a char device, else the
/// first renderD* (sorted), else 226:128 without a node.
pub fn render_device() -> (u64, Option<String>) {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let is_char = |p: &str| std::fs::metadata(p).ok().filter(|m| m.file_type().is_char_device()).map(|m| m.rdev());
    if let Some(rdev) = is_char(RENDER_NODE) {
        return (rdev, Some(RENDER_NODE.into()));
    }
    let mut names: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    for n in names.iter().filter(|n| n.starts_with("renderD")) {
        let p = format!("/dev/dri/{n}");
        if let Some(rdev) = is_char(&p) {
            return (rdev, Some(p));
        }
    }
    (libc::makedev(RENDER_RDEV.0, RENDER_RDEV.1), None)
}

pub fn fourcc_name(f: Format) -> String {
    let code = (f.code as u32).to_le_bytes();
    let name: String = code.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect();
    let name = name.trim_end();
    let m = match f.modifier {
        Modifier::Linear => "LINEAR".to_string(),
        Modifier::Invalid => "IMPLICIT".to_string(),
        m => format!("{:#x}", u64::from(m)),
    };
    format!("{name}:{m}")
}

/// DMA_BUF_IOCTL_SYNC, _IOW('b', 0, struct dma_buf_sync { u64 flags })
/// (linux-7.3.1/include/uapi/linux/dma-buf.h:172).
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
/// A flag outside DMA_BUF_SYNC_VALID_FLAGS_MASK: a dma-buf answers EINVAL before touching anything
/// (linux-7.3.1/drivers/dma-buf/dma-buf.c:555-556, V2's crates/drm dma_buf.rs the same), any other file ENOTTY.
const SYNC_NO_SUCH_FLAG: u64 = 1 << 63;

/// One plane as the client described it, and the size of its file.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub offset: u32,
    pub stride: u32,
    pub size: u64,
}

/// The layout rules of `probe`, without files (unit-tested). Every plane starts inside its file. A LINEAR plane 0 has
/// rows of at least width x bytes per pixel, and all `height` rows fit the file: a shorter stride would let the GPU
/// read past the buffer (Linux's framebuffer checks refuse it with EINVAL as well,
/// linux-7.3.1/drivers/gpu/drm/drm_gem_framebuffer_helper.c:186-196). The rest is checked by the driver at import.
pub fn check_layout(planes: &[Plane], w: u32, h: u32, f: Format) -> Result<(), String> {
    use smithay::backend::allocator::format::get_bpp;
    if w == 0 || h == 0 || planes.is_empty() {
        return Err(format!("{w}x{h} with {} planes", planes.len()));
    }
    for (i, p) in planes.iter().enumerate() {
        if u64::from(p.offset) > p.size {
            return Err(format!("plane {i}: offset {} past the end ({} bytes)", p.offset, p.size));
        }
    }
    let (Some(bpp), true) = (get_bpp(f.code), f.modifier == Modifier::Linear) else { return Ok(()) };
    let p = planes[0];
    let row = u64::from(w).checked_mul(bpp as u64).map(|b| b.div_ceil(8)).ok_or("row size overflows")?;
    if u64::from(p.stride) < row {
        return Err(format!("plane 0: stride {} below a row of {w} pixels ({row} bytes)", p.stride));
    }
    let end = u64::from(p.stride).checked_mul(u64::from(h)).and_then(|n| n.checked_add(u64::from(p.offset)));
    match end {
        Some(end) if end <= p.size => Ok(()),
        Some(end) => Err(format!("plane 0: needs {end} bytes, the dma-buf has {}", p.size)),
        None => Err("plane 0: size overflows".into()),
    }
}

/// Checks a client's dma-buf before any renderer touches it: every plane fd is a dma-buf (a plain memfd or file
/// could be truncated under the mapping, a SIGBUS of the compositor; a dma-buf keeps its size), then the layout
/// (`check_layout`).
pub fn probe(buf: &Dmabuf) -> Result<(), String> {
    use smithay::backend::allocator::Buffer as _;
    use std::os::fd::AsRawFd;
    let mut planes = Vec::new();
    for (i, ((fd, off), stride)) in buf.handles().zip(buf.offsets()).zip(buf.strides()).enumerate() {
        let mut flags: u64 = SYNC_NO_SUCH_FLAG;
        // SAFETY: DMA_BUF_IOCTL_SYNC encodes an 8-byte argument; `flags` is a live u64 for the call. The invalid flag
        // makes a dma-buf answer EINVAL without any cache work; on another file the number means nothing (ENOTTY).
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC as _, &mut flags as *mut u64) };
        let e = std::io::Error::last_os_error();
        if r == 0 || e.raw_os_error() != Some(libc::EINVAL) {
            return Err(format!("plane {i}: not a dma-buf ({})", if r == 0 { "the sync ioctl took a bad flag".into() } else { e.to_string() }));
        }
        // SAFETY: lseek on a dma-buf fd the Dmabuf owns for the call; only the file position changes (reset).
        let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        // SAFETY: as above.
        unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
        if size < 0 {
            return Err(format!("plane {i}: no size ({})", std::io::Error::last_os_error()));
        }
        planes.push(Plane { offset: off, stride, size: size as u64 });
    }
    check_layout(&planes, buf.width(), buf.height(), buf.format())
}

/// Makes the dma-buf global (version 4, default feedback) for the renderer's formats, and the syncobj global
/// when the render node has timelines and eventfds. Called once, after the first compositor exists.
pub fn init(app: &mut App, comp: &dyn crate::render::Composer, handle: smithay::reexports::calloop::LoopHandle<'static, App>) {
    advertise(app, comp);
    // The node stays for the syncobj global: explicit sync does not depend on which device renders.
    let node = render_device().1;
    app.dma.sync = crate::syncobj::Syncobj::start(&app.dh.clone(), node.as_deref(), handle);
}

/// WY-21: a new renderer took over after a GPU reset (`main.rs` `leave_gpu`). The global stays; its default feedback and
/// every surface's feedback say what the new renderer takes (main device, format table), so a client already bound hears it
/// and allocates its next buffers to fit (Mesa's EGL and Vulkan WSI follow a feedback's `done`). A buffer made before
/// it heard can still be refused by the new renderer: a `create_immed` that fails ends that client, as linux-dmabuf
/// says. Without feedback (a version 3 global) the old global is hidden and a new one made.
pub fn renew(app: &mut App, comp: &dyn crate::render::Composer) {
    advertise_as(app, comp, true);
}

fn advertise(app: &mut App, comp: &dyn crate::render::Composer) {
    advertise_as(app, comp, false);
}

/// The new feedback to every surface that asked for one (Smithay's own default reaches only the default feedbacks).
fn tell_surfaces(app: &App, feedback: &smithay::wayland::dmabuf::DmabufFeedback) -> usize {
    use smithay::wayland::dmabuf::SurfaceDmabufFeedbackState;
    let mut n = 0;
    for w in app.all_windows().into_iter().chain(app.x_unmanaged()) {
        w.with_surfaces(|_, data| {
            if let Some(f) = SurfaceDmabufFeedbackState::from_states(data) {
                f.set_feedback(feedback);
                n += 1;
            }
        });
    }
    n
}

fn advertise_as(app: &mut App, comp: &dyn crate::render::Composer, renew: bool) {
    let (formats, renderer) = (table(comp.dmabuf_formats()), comp.renderer());
    if formats.is_empty() {
        eprintln!("bitscomp: dmabuf: renderer {renderer} imports no dma-buf format: no zwp_linux_dmabuf_v1 (clients use wl_shm)");
        return;
    }
    let (dev, node) = render_device();
    let shown = node.as_deref().unwrap_or("no node");
    let dh: DisplayHandle = app.dh.clone();
    let names: Vec<String> = formats.iter().take(8).map(|f| fourcc_name(*f)).collect();
    let more = formats.len().saturating_sub(names.len());
    let built = DmabufFeedbackBuilder::new(dev, formats.iter().copied()).build();
    if renew && let (Ok(feedback), Some(g), true) = (&built, app.dma.global, app.dma.feedback) {
        app.dma.state.set_default_feedback(&g, feedback);
        let n = tell_surfaces(app, feedback);
        eprintln!(
            "bitscomp: dmabuf: the feedback now for renderer {renderer}, main device {}:{} ({shown}), {} formats (default and {n} surfaces)",
            libc::major(dev),
            libc::minor(dev),
            formats.len()
        );
        return;
    }
    if renew && let Some(g) = app.dma.global.take() {
        app.dma.state.disable_global::<App>(&dh, &g);
        app.dma.retired.push(g);
    }
    app.dma.feedback = built.is_ok();
    let global = match built {
        Ok(feedback) => app.dma.state.create_global_with_default_feedback::<App>(&dh, &feedback),
        Err(e) => {
            // The table is a sealed memfd; without it clients still get the v3 format list (Mesa needs >= 3).
            eprintln!("bitscomp: dmabuf: no feedback format table ({e}); zwp_linux_dmabuf_v1 version 3 only");
            app.dma.state.create_global::<App>(&dh, formats.clone())
        }
    };
    app.dma.global = Some(global);
    eprintln!(
        "bitscomp: dmabuf: zwp_linux_dmabuf_v1 for renderer {renderer}, main device {}:{} ({shown}), {} formats: {}{}",
        libc::major(dev),
        libc::minor(dev),
        formats.len(),
        names.join(" "),
        if more > 0 { format!(" and {more} more") } else { String::new() }
    );
}

impl App {
    /// After the turn's frame: the held buffers and release points go back (not while the renderer's GPU work
    /// is still running); finished acquire waits leave the loop.
    pub fn give_back(&mut self) {
        if !self.dma.gpu_busy {
            let (buffers, points) = self.dma.hold.give_back();
            self.stats.held_buffers += buffers as u64;
            self.stats.release_points += points as u64;
        }
        self.stats.held_now = self.dma.hold.len() as u64;
        if let Some(s) = self.dma.sync.as_mut() {
            self.stats.acquire_pending = s.prune() as u64;
        }
    }
}

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dma.state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        // Checked here, at once: a create_immed buffer that fails ends its client before any commit of it is read.
        if let Err(e) = probe(&dmabuf) {
            self.stats.dmabuf_failed += 1;
            let n = self.stats.dmabuf_failed;
            if n <= 8 || n % 100 == 0 {
                eprintln!("bitscomp: dmabuf: refused before import: {e} (refusal {n})");
            }
            return notifier.failed();
        }
        // The renderer's own check follows in this loop turn (it is not reachable from here); the client waits.
        self.dma.pending.push((dmabuf, notifier));
    }
}

smithay::delegate_dmabuf!(App);

#[cfg(test)]
mod tests {
    use super::*;

    fn f(code: Fourcc, modifier: Modifier) -> Format {
        Format { code, modifier }
    }

    #[test]
    fn linear_comes_first_then_modifiers_then_implicit() {
        let t = table([
            f(Fourcc::Abgr8888, Modifier::Invalid),
            f(Fourcc::Argb8888, Modifier::from(0x0200_0000_0000_0001u64)),
            f(Fourcc::Abgr8888, Modifier::Linear),
            f(Fourcc::Argb8888, Modifier::Linear),
            f(Fourcc::Xrgb8888, Modifier::Linear),
            f(Fourcc::Xrgb8888, Modifier::Linear),
        ]);
        let names: Vec<String> = t.iter().map(|x| fourcc_name(*x)).collect();
        assert_eq!(names, ["XR24:LINEAR", "AR24:LINEAR", "AB24:LINEAR", "AR24:0x200000000000001", "AB24:IMPLICIT"]);
    }

    #[test]
    fn layouts_that_would_read_past_the_buffer_are_refused() {
        let xr = f(Fourcc::Xrgb8888, Modifier::Linear);
        let one = |offset, stride, size| [Plane { offset, stride, size }];
        // 64x4 XRGB8888: rows of 256 bytes.
        assert!(check_layout(&one(0, 256, 1024), 64, 4, xr).is_ok());
        assert!(check_layout(&one(0, 252, 4096), 64, 4, xr).is_err(), "stride a pixel short of a row");
        assert!(check_layout(&one(0, 4, 16), 4096, 4, xr).is_err(), "the tiny-stride attack");
        assert!(check_layout(&one(0, 256, 1023), 64, 4, xr).is_err(), "one byte short");
        assert!(check_layout(&one(4096, 256, 4096), 64, 4, xr).is_err(), "rows past the end after the offset");
        assert!(check_layout(&one(8192, 256, 4096), 64, 4, xr).is_err(), "offset past the end");
        assert!(check_layout(&one(0, u32::MAX, u64::MAX), u32::MAX / 4, u32::MAX, xr).is_ok(), "no overflow");
        assert!(check_layout(&one(0, u32::MAX, 1 << 40), u32::MAX / 4, u32::MAX, xr).is_err());
        assert!(check_layout(&one(0, 256, 1024), 0, 4, xr).is_err(), "no width");
        assert!(check_layout(&[], 64, 4, xr).is_err(), "no plane");
    }

    #[test]
    fn tiled_and_many_plane_buffers_are_the_drivers_to_check() {
        let one = [Plane { offset: 0, stride: 256, size: 1 << 20 }];
        assert!(check_layout(&one, 64, 4, f(Fourcc::Xrgb8888, Modifier::from(0x0200_0000_0000_0001u64))).is_ok());
        assert!(check_layout(&one, 64, 4, f(Fourcc::Xrgb8888, Modifier::Invalid)).is_ok());
        let two = [one[0], Plane { offset: 1024, stride: 256, size: 1 << 20 }];
        assert!(check_layout(&two, 64, 4, f(Fourcc::Nv12, Modifier::Linear)).is_ok());
        assert!(check_layout(&[Plane { offset: 4096, stride: 256, size: 1024 }], 64, 4, f(Fourcc::Nv12, Modifier::Linear)).is_err(), "a plane past its file");
    }

    #[test]
    fn the_sync_ioctl_number_is_linux_one() {
        // _IOW('b', 0, struct dma_buf_sync), 8 bytes (linux-7.3.1/include/uapi/linux/dma-buf.h:172).
        assert_eq!(DMA_BUF_IOCTL_SYNC, (1 << 30) | (8 << 16) | ((b'b' as u64) << 8));
    }

    #[test]
    fn the_render_device_is_a_render_node_number() {
        let (dev, node) = render_device();
        assert_eq!(libc::major(dev), 226);
        if node.is_none() {
            assert_eq!(libc::minor(dev), 128);
        } else {
            assert!(libc::minor(dev) >= 128);
        }
    }
}
