//! WY-16 (gx-scout-mesa.md step C2): client dma-bufs. zwp_linux_dmabuf_v1 version 4 with a default feedback
//! whose main device is the render node (226:128, what Mesa's WSI compares with its own node's rdev,
//! gx-scout-uapi.md §6) and whose one tranche lists the renderer's formats, LINEAR first (`table`). Pixman imports a
//! LINEAR buffer by mmap (DR-5's dma-buf mmap); GLES through an EGL image. Every import is checked by the renderer
//! before the client gets its wl_buffer (`Composer::dmabuf_turn`), so a buffer the renderer cannot read (a
//! non-linear modifier under Pixman, a bad stride) is `failed`, never a crash at draw time.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Format, Fourcc, Modifier};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::dmabuf::{
    DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};

use crate::state::App;

/// The render node clients render on; its char device is the feedback's main device.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";
/// 226:128, the render node's number on Linux and on V2 (DR-2), used when the node is missing (no GPU driver
/// yet, the Mini-PC with drm=auto): a client's own node is then the same number or there is none.
pub const RENDER_RDEV: (u32, u32) = (226, 128);

pub struct Dma {
    pub state: DmabufState,
    pub global: Option<DmabufGlobal>,
    /// Imports waiting for the renderer's check (the main loop's next `dmabuf_turn`).
    pub pending: Vec<(Dmabuf, ImportNotifier)>,
    /// A wl_buffer was destroyed: the renderer's cache drops its mapping or EGL image at the next turn, not only
    /// at the next frame (the client's udmabuf pages stay pinned while it is mapped, DR-5).
    pub cleanup_due: bool,
    /// Buffers (and release points) replaced by a commit, given back after the frame (`release`).
    pub hold: crate::release::Hold,
    pub sync: Option<crate::syncobj::Syncobj>,
    /// The renderer needs every plane mappable (`probe`).
    pub map_probe: bool,
    /// The last frame's GPU work had not finished within the renderer's wait (`CompRenderer::gpu_idle`): held
    /// buffers stay held until it has.
    pub gpu_busy: bool,
}

impl Dma {
    pub fn new() -> Dma {
        Dma {
            state: DmabufState::new(),
            global: None,
            pending: Vec::new(),
            cleanup_due: false,
            hold: Default::default(),
            sync: None,
            map_probe: false,
            gpu_busy: false,
        }
    }
}

/// The feedback's format table: LINEAR first (every client and Pixman can use it, and Mesa's WSI takes the first
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

/// The main device and the node it was found at: the render node when it exists as a char device, else the
/// first renderD* (sorted), else 226:128 without a node.
pub fn main_device() -> (u64, Option<String>) {
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

/// What a CPU reader (llvmpipe's mmap import) can take: one-plane formats of known pixel size, LINEAR or implicit.
pub fn cpu_readable(f: Format) -> bool {
    let one = Plane { offset: 0, stride: u32::MAX, size: u64::MAX };
    check_layout(&[one], 1, 1, f, true).is_ok()
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

/// DMA_BUF_IOCTL_SYNC, _IOW('b', 0, struct dma_buf_sync { u64 flags }) (linux-6.6/include/uapi/linux/dma-buf.h:171).
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
/// A flag outside DMA_BUF_SYNC_VALID_FLAGS_MASK: a dma-buf answers EINVAL before touching anything
/// (linux-6.6/drivers/dma-buf/dma-buf.c:455-456, V2's crates/drm dma_buf.rs the same), any other file ENOTTY.
const SYNC_NO_SUCH_FLAG: u64 = 1 << 63;

/// One plane as the client described it, and the size of its file.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub offset: u32,
    pub stride: u32,
    pub size: u64,
}

/// The layout rules of `probe`, without files (unit-tested). Every plane starts inside its file. A LINEAR plane 0
/// (and an implicit one a software renderer reads as linear, `cpu`) has rows of at least width x bytes per pixel,
/// and all `height` rows fit the file: a shorter stride would let Pixman or llvmpipe read past the mapping (a
/// SIGSEGV/SIGBUS of the compositor, Linux's framebuffer checks refuse it with EINVAL as well,
/// linux-6.6/drivers/gpu/drm/drm_gem_framebuffer_helper.c:173-190). A renderer reading by CPU (`cpu`) takes only
/// one-plane formats whose pixel size is known; a GPU import is checked further by its driver.
pub fn check_layout(planes: &[Plane], w: u32, h: u32, f: Format, cpu: bool) -> Result<(), String> {
    use smithay::backend::allocator::format::get_bpp;
    if w == 0 || h == 0 || planes.is_empty() {
        return Err(format!("{w}x{h} with {} planes", planes.len()));
    }
    for (i, p) in planes.iter().enumerate() {
        if u64::from(p.offset) > p.size {
            return Err(format!("plane {i}: offset {} past the end ({} bytes)", p.offset, p.size));
        }
    }
    let linear = f.modifier == Modifier::Linear || (cpu && f.modifier == Modifier::Invalid);
    let bpp = get_bpp(f.code);
    if cpu && (planes.len() != 1 || bpp.is_none() || !linear) {
        return Err(format!("{} with {} planes: not readable by the CPU", fourcc_name(f), planes.len()));
    }
    let (Some(bpp), true) = (bpp, linear) else { return Ok(()) };
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
/// (`check_layout`). With `map`, every plane must also map for reading: Mesa's llvmpipe imports by mmap and draws
/// from a null mapping when that fails (a SIGSEGV of bitscomp, seen with a RADV buffer Linux refuses to map,
/// NO_CPU_ACCESS), so a software GLES asks for it.
pub fn probe(buf: &Dmabuf, map: bool) -> Result<(), String> {
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
    check_layout(&planes, buf.width(), buf.height(), buf.format(), map)?;
    if map {
        for i in 0..planes.len() {
            buf.map_plane(i, smithay::backend::allocator::dmabuf::DmabufMappingMode::READ)
                .map_err(|e| format!("plane {i} does not map: {e:?}"))?;
        }
    }
    Ok(())
}

/// Makes the dma-buf global (version 4, default feedback) for the renderer's formats, and the syncobj global
/// when the render node has timelines and eventfds. Called once, after the first compositor exists.
pub fn init(app: &mut App, comp: &dyn crate::render::Composer, handle: smithay::reexports::calloop::LoopHandle<'static, App>) {
    app.dma.map_probe = comp.maps_dmabufs();
    // A renderer that reads by CPU gets only what `check_layout` lets through: no entry the client must fail on.
    let cpu = app.dma.map_probe;
    let all = comp.dmabuf_formats().into_iter().filter(|f| !cpu || cpu_readable(*f));
    let (formats, renderer) = (table(all), comp.renderer());
    if formats.is_empty() {
        eprintln!("bitscomp: dmabuf: renderer {renderer} imports no dma-buf format: no zwp_linux_dmabuf_v1 (clients use wl_shm)");
        return;
    }
    let (dev, node) = main_device();
    let dh: DisplayHandle = app.dh.clone();
    let names: Vec<String> = formats.iter().take(8).map(|f| fourcc_name(*f)).collect();
    let more = formats.len().saturating_sub(names.len());
    let global = match DmabufFeedbackBuilder::new(dev, formats.iter().copied()).build() {
        Ok(feedback) => app.dma.state.create_global_with_default_feedback::<App>(&dh, &feedback),
        Err(e) => {
            // The table is a sealed memfd; without it clients still get the v3 format list (Mesa needs >= 3).
            eprintln!("bitscomp: dmabuf: no feedback format table ({e}); zwp_linux_dmabuf_v1 version 3 only");
            app.dma.state.create_global::<App>(&dh, formats.clone())
        }
    };
    app.dma.global = Some(global);
    eprintln!(
        "bitscomp: dmabuf: zwp_linux_dmabuf_v1 for renderer {renderer}, main device {}:{} ({}), {} formats: {}{}",
        libc::major(dev),
        libc::minor(dev),
        node.as_deref().unwrap_or("no node"),
        formats.len(),
        names.join(" "),
        if more > 0 { format!(" and {more} more") } else { String::new() }
    );
    app.dma.sync = crate::syncobj::Syncobj::start(&dh, node.as_deref(), handle);
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
        if let Err(e) = probe(&dmabuf, self.dma.map_probe) {
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
    fn pixman_table_is_linear_only_and_starts_with_xrgb() {
        use smithay::backend::renderer::ImportDma;
        let r = smithay::backend::renderer::pixman::PixmanRenderer::new().unwrap();
        let t = table(r.dmabuf_formats());
        assert!(!t.is_empty());
        assert!(t.iter().all(|x| x.modifier == Modifier::Linear));
        assert_eq!(t[0].code, Fourcc::Xrgb8888);
        assert_eq!(t[1].code, Fourcc::Argb8888);
    }

    #[test]
    fn layouts_that_would_read_past_the_buffer_are_refused() {
        let xr = f(Fourcc::Xrgb8888, Modifier::Linear);
        let one = |offset, stride, size| [Plane { offset, stride, size }];
        // 64x4 XRGB8888: rows of 256 bytes.
        assert!(check_layout(&one(0, 256, 1024), 64, 4, xr, false).is_ok());
        assert!(check_layout(&one(0, 256, 1024), 64, 4, xr, true).is_ok());
        assert!(check_layout(&one(0, 252, 4096), 64, 4, xr, false).is_err(), "stride a pixel short of a row");
        assert!(check_layout(&one(0, 4, 16), 4096, 4, xr, false).is_err(), "the tiny-stride attack");
        assert!(check_layout(&one(0, 256, 1023), 64, 4, xr, false).is_err(), "one byte short");
        assert!(check_layout(&one(4096, 256, 4096), 64, 4, xr, false).is_err(), "rows past the end after the offset");
        assert!(check_layout(&one(8192, 256, 4096), 64, 4, xr, false).is_err(), "offset past the end");
        assert!(check_layout(&one(0, u32::MAX, u64::MAX), u32::MAX / 4, u32::MAX, xr, false).is_ok(), "no overflow");
        assert!(check_layout(&one(0, u32::MAX, 1 << 40), u32::MAX / 4, u32::MAX, xr, false).is_err());
        assert!(check_layout(&one(0, 256, 1024), 0, 4, xr, false).is_err(), "no width");
        assert!(check_layout(&[], 64, 4, xr, false).is_err(), "no plane");
    }

    #[test]
    fn a_cpu_reader_takes_only_linear_single_plane_formats_of_known_size() {
        let one = [Plane { offset: 0, stride: 256, size: 1 << 20 }];
        let tiled = f(Fourcc::Xrgb8888, Modifier::from(0x0200_0000_0000_0001u64));
        // A GPU import of a tiled buffer is its driver's to check; a CPU reader cannot read it.
        assert!(check_layout(&one, 64, 4, tiled, false).is_ok());
        assert!(check_layout(&one, 64, 4, tiled, true).is_err());
        // Implicit is linear to llvmpipe, so its rows are checked like LINEAR ones.
        let implicit = f(Fourcc::Xrgb8888, Modifier::Invalid);
        assert!(check_layout(&one, 64, 4, implicit, true).is_ok());
        assert!(check_layout(&[Plane { offset: 0, stride: 4, size: 1 << 20 }], 64, 4, implicit, true).is_err());
        let two = [one[0], Plane { offset: 1024, stride: 256, size: 1 << 20 }];
        assert!(check_layout(&two, 64, 4, f(Fourcc::Nv12, Modifier::Linear), true).is_err(), "two planes");
        assert!(check_layout(&two, 64, 4, f(Fourcc::Nv12, Modifier::Linear), false).is_ok());
        assert!(cpu_readable(f(Fourcc::Argb8888, Modifier::Linear)));
        assert!(!cpu_readable(f(Fourcc::Nv12, Modifier::Linear)));
        assert!(!cpu_readable(tiled));
    }

    #[test]
    fn the_sync_ioctl_number_is_linux_one() {
        // _IOW('b', 0, struct dma_buf_sync), 8 bytes (linux-6.6/include/uapi/linux/dma-buf.h:171).
        assert_eq!(DMA_BUF_IOCTL_SYNC, (1 << 30) | (8 << 16) | ((b'b' as u64) << 8));
    }

    #[test]
    fn main_device_is_a_render_node_number() {
        let (dev, node) = main_device();
        assert_eq!(libc::major(dev), 226);
        if node.is_none() {
            assert_eq!(libc::minor(dev), 128);
        } else {
            assert!(libc::minor(dev) >= 128);
        }
    }
}
