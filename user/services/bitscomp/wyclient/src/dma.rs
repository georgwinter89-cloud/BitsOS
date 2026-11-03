//! WY-16: `--dmabuf`: the window's buffers are dma-bufs from `/dev/udmabuf` (memfd pages, the test source of
//! gx-scout-mesa.md §5 C2) handed over with zwp_linux_dmabuf_v1 (version 4: default feedback read and printed,
//! `create` answered by `created` or `failed`). The pictures are the wl_shm ones byte for byte, so a test compares
//! both screenshots. `--modifier HEX` sends that modifier instead of LINEAR (a renderer that cannot read it must
//! answer `failed`). `--syncobj`: explicit sync (wp_linux_drm_syncobj_v1): one acquire timeline, one release
//! timeline per buffer; a buffer is used again only once its release point signalled; `--acquire-delay MS` signals
//! the first acquire point MS ms after the commit; `--syncobj-break no-release|shm|conflict` commits a wrong
//! combination (the compositor must answer with that protocol error and stay up); `--syncobj-break flood` commits
//! 500 times with acquire points it never signals (the compositor must not spend an fd per waiting commit) and
//! `destroy-waiting` destroys the wl_buffer of a commit still waiting for its acquire point (signalled 1 s later).
//! `--dmabuf-bad stride|memfd|oversize` hands over a buffer a renderer must not read: rows shorter than the width
//! (answered `failed`), a plain memfd instead of a dma-buf (`failed`), rows past the end (out_of_bounds).
//! Lines: `dmabuf feedback main_device=MAJ:MIN formats=N first=CODE:MOD tranches=T tranche_formats=K`,
//! `dmabuf created N`, `dmabuf failed`, `syncobj acquire signalled point=P`; with `--animate` the frames line gets
//! ` sync_released=N`.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use wayland_client::protocol::wl_buffer;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback, zwp_linux_dmabuf_v1 as dmabuf,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1 as sync_mgr, wp_linux_drm_syncobj_surface_v1 as sync_surf,
    wp_linux_drm_syncobj_timeline_v1 as sync_tl,
};

use crate::drm_sync::Drm;
use crate::{State, picture};

const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const LINEAR: u64 = 0;
/// `--syncobj-break flood`: commits whose acquire point never signals.
const FLOOD: usize = 500;
/// struct udmabuf_create (linux-6.6/include/uapi/linux/udmabuf.h): memfd, flags, offset, size.
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}
/// _IOW('u', 0x42, struct udmabuf_create), checked with a C compiler against the header.
const UDMABUF_CREATE: u64 = 0x4018_7542;
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

#[derive(Default)]
pub struct Dma {
    pub global: Option<dmabuf::ZwpLinuxDmabufV1>,
    pub sync_mgr: Option<sync_mgr::WpLinuxDrmSyncobjManagerV1>,
    pub modifier: u64,
    pub syncobj: bool,
    pub acquire_delay: Option<Duration>,
    pub brk: Option<String>,
    pub bad: Option<String>,
    /// From the default feedback.
    main_device: Option<u64>,
    table: Vec<(u32, u64)>,
    tranches: usize,
    tranche_formats: usize,
    fb_done: bool,
    /// Buffers asked for and not answered yet, and the ones made, by index.
    waiting: usize,
    made: Vec<Option<wl_buffer::WlBuffer>>,
    /// Counts the requests: a buffer of an older one (a configure came before it was answered) is dropped.
    generation: u32,
    size: (i32, i32),
    /// Explicit sync: the node, the acquire timeline (handle, last point), per buffer (handle, last point).
    drm: Option<Arc<Drm>>,
    surface: Option<sync_surf::WpLinuxDrmSyncobjSurfaceV1>,
    acquire: Option<(u32, u64, sync_tl::WpLinuxDrmSyncobjTimelineV1)>,
    release: Vec<(u32, u64, sync_tl::WpLinuxDrmSyncobjTimelineV1)>,
    pub sync_released: u64,
    /// `--acquire-delay`: no acquire point signals before this (signalling a later point would signal the
    /// delayed one too, timelines are ordered).
    delay_until: Option<std::time::Instant>,
}

fn fail(what: String) -> ! {
    eprintln!("wyclient: {what}");
    std::process::exit(1);
}

/// A dma-buf of `len` bytes (page-rounded) over a sealed memfd, the content `bytes` written into the memfd; with
/// `plain` the memfd itself, which is no dma-buf (`--dmabuf-bad memfd`).
fn udmabuf(bytes: &[u8], plain: bool) -> std::io::Result<OwnedFd> {
    let len = bytes.len().div_ceil(4096) * 4096;
    // SAFETY: memfd_create with a NUL-terminated name; checked, then owned.
    let fd = unsafe { libc::memfd_create(c"wyclient-dma".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: just made, owned by nobody else.
    let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ftruncate, pwrite (from our slice, inside the new size) and fcntl on our own memfd.
    unsafe {
        if libc::ftruncate(memfd.as_raw_fd(), len as libc::off_t) < 0
            || libc::pwrite(memfd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) != bytes.len() as isize
            || libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    if plain {
        return Ok(memfd);
    }
    let dev = std::fs::OpenOptions::new().read(true).write(true).open("/dev/udmabuf")?;
    let mut arg = UdmabufCreate { memfd: memfd.as_raw_fd() as u32, flags: UDMABUF_FLAGS_CLOEXEC, offset: 0, size: len as u64 };
    // SAFETY: UDMABUF_CREATE encodes the 24-byte #[repr(C)] struct `arg` points to, alive for the call.
    let r = unsafe { libc::ioctl(dev.as_raw_fd(), UDMABUF_CREATE as _, &mut arg as *mut UdmabufCreate) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the ioctl answered a new fd that nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(r) })
}

impl State {
    /// After the registry: the default feedback (printed), and the syncobj objects when asked for.
    pub fn dma_setup(&mut self, conn: &Connection, queue: &mut wayland_client::EventQueue<State>, qh: &QueueHandle<State>) {
        let d = self.dma.as_ref().unwrap();
        let Some(g) = d.global.clone() else { fail("--dmabuf: the compositor has no zwp_linux_dmabuf_v1".into()) };
        if g.version() < 4 {
            fail(format!("--dmabuf: zwp_linux_dmabuf_v1 version {} (4 needed for the feedback)", g.version()));
        }
        g.get_default_feedback(qh, ());
        while !self.dma.as_ref().unwrap().fb_done {
            if let Err(e) = queue.blocking_dispatch(self) {
                fail(format!("feedback: {e}"));
            }
        }
        let d = self.dma.as_mut().unwrap();
        let dev = d.main_device.unwrap_or(0);
        let first = d.table.first().map_or("-".to_string(), |&(f, m)| {
            let n: String = f.to_le_bytes().iter().map(|&b| b as char).collect();
            if m == LINEAR { format!("{n}:LINEAR") } else { format!("{n}:{m:#x}") }
        });
        println!(
            "dmabuf feedback main_device={}:{} formats={} first={first} tranches={} tranche_formats={}",
            libc::major(dev),
            libc::minor(dev),
            d.table.len(),
            d.tranches,
            d.tranche_formats
        );
        if !d.syncobj {
            return;
        }
        let Some(mgr) = d.sync_mgr.clone() else { fail("--syncobj: no wp_linux_drm_syncobj_manager_v1".into()) };
        let Some(node) = crate::drm_sync::node_of(dev) else { fail(format!("--syncobj: no render node for {dev:#x}")) };
        let drm = Arc::new(Drm::open(&node).unwrap_or_else(|e| fail(format!("{node}: {e}"))));
        let tl = |drm: &Drm| {
            let h = drm.create().unwrap_or_else(|e| fail(format!("SYNCOBJ_CREATE: {e}")));
            let fd = drm.export(h).unwrap_or_else(|e| fail(format!("SYNCOBJ_HANDLE_TO_FD: {e}")));
            (h, 0u64, mgr.import_timeline(fd.as_fd(), qh, ()))
        };
        d.acquire = Some(tl(&drm));
        d.release = (0..2).map(|_| tl(&drm)).collect();
        d.drm = Some(drm);
        d.surface = Some(mgr.get_surface(self.surface.as_ref().unwrap(), qh, ()));
        let _ = conn.flush();
    }

    /// Asks for `n` buffers (the picture, the second one shifted as `--animate` does); they are attached once all
    /// arrived (`dma_ready`).
    pub fn dma_request(&mut self, n: usize, qh: &QueueHandle<State>) -> std::io::Result<()> {
        let (w, h) = self.size;
        let colour = self.colour;
        let d = self.dma.as_mut().unwrap();
        // The buffers made before stay in use until the new ones replace them (`dma_ready`).
        d.made = vec![None; n];
        d.waiting = n;
        d.generation += 1;
        d.size = (w, h);
        let g = d.global.clone().unwrap();
        for k in 0..n {
            let mut px = picture(w, h, colour);
            if k == 1 {
                px.rotate_right(16);
            }
            let mut bytes: Vec<u8> = px.iter().flat_map(|p| p.to_le_bytes()).collect();
            let mut stride = (w * 4) as u32;
            match d.bad.as_deref() {
                Some("stride") => stride = 4,
                Some("oversize") => bytes.truncate(4096),
                _ => {}
            }
            let fd = udmabuf(&bytes, d.bad.as_deref() == Some("memfd"))?;
            let p = g.create_params(qh, (d.generation, k as u8));
            p.add(fd.as_fd(), 0, 0, stride, (d.modifier >> 32) as u32, d.modifier as u32);
            p.create(w, h, XRGB8888, params::Flags::empty());
        }
        Ok(())
    }

    /// Before a commit of buffer `k`: the acquire and release points (and the `--syncobj-break` mistakes).
    pub fn dma_points(&mut self, k: usize) {
        let Some(d) = self.dma.as_mut() else { return };
        let (Some(s), Some(drm)) = (d.surface.clone(), d.drm.clone()) else { return };
        let brk = d.brk.clone().unwrap_or_default();
        let a = d.acquire.as_mut().unwrap();
        a.1 += 1;
        let (ah, ap) = (a.0, a.1);
        s.set_acquire_point(&a.2, (ap >> 32) as u32, ap as u32);
        let r = &mut d.release[k % 2];
        r.1 += 1;
        match brk.as_str() {
            "no-release" => {}
            "conflict" => s.set_release_point(&a.2, (ap >> 32) as u32, ap as u32),
            _ => s.set_release_point(&r.2, (r.1 >> 32) as u32, r.1 as u32),
        }
        if brk == "flood" {
            // Never signalled: the commit waits until the client goes.
            return;
        }
        if let (None, Some(t)) = (d.delay_until, d.acquire_delay) {
            d.delay_until = Some(std::time::Instant::now() + t);
        }
        match d.delay_until.map(|u| u.saturating_duration_since(std::time::Instant::now())).filter(|t| !t.is_zero()) {
            None => {
                if let Err(e) = drm.signal(ah, ap) {
                    fail(format!("SYNCOBJ_TIMELINE_SIGNAL: {e}"));
                }
            }
            Some(t) => {
                std::thread::spawn(move || {
                    std::thread::sleep(t);
                    match drm.signal(ah, ap) {
                        Ok(()) => println!("syncobj acquire signalled point={ap}"),
                        Err(e) => println!("syncobj acquire signal failed: {e}"),
                    }
                });
            }
        }
    }

    /// With explicit sync, whether buffer `k`'s last release point has signalled (counted once per point).
    pub fn dma_released(&mut self, k: usize) -> bool {
        let Some(d) = self.dma.as_mut() else { return false };
        let Some(drm) = d.drm.clone() else { return false };
        let r = &d.release[k % 2];
        match drm.signalled(r.0, r.1) {
            Ok(true) => {
                d.sync_released += 1;
                true
            }
            Ok(false) => false,
            Err(e) => fail(format!("SYNCOBJ_TIMELINE_WAIT: {e}")),
        }
    }

    fn dma_ready(&mut self, qh: &QueueHandle<State>) {
        let d = self.dma.as_mut().unwrap();
        let bufs: Vec<wl_buffer::WlBuffer> = d.made.iter().flatten().cloned().collect();
        let (w, h) = d.size;
        if let Some(a) = self.anim.as_mut() {
            let first = a.bufs.is_empty();
            for (b, _) in a.bufs.drain(..) {
                b.destroy();
            }
            a.bufs = bufs.into_iter().map(|b| (b, false)).collect();
            if first {
                self.frame(qh);
            }
            return;
        }
        let s = self.surface.clone().unwrap();
        self.dma_points(0);
        if self.dma.as_ref().unwrap().brk.as_deref() == Some("shm") {
            // A wl_shm buffer with points: unsupported_buffer.
            let _ = self.draw_shm(qh);
            return;
        }
        s.attach(Some(&bufs[0]), 0, 0);
        s.damage_buffer(0, 0, w, h);
        s.commit();
        match self.dma.as_ref().unwrap().brk.as_deref() {
            Some("flood") => {
                for i in 1..FLOOD {
                    self.dma_points(i);
                    s.attach(Some(&bufs[0]), 0, 0);
                    s.damage_buffer(0, 0, w, h);
                    s.commit();
                }
                println!("flood committed {FLOOD}");
            }
            Some("destroy-waiting") => {
                bufs[0].destroy();
                println!("destroyed the waiting buffer");
                return;
            }
            _ => {}
        }
        if let Some(old) = self.buffer.replace(bufs[0].clone()) {
            old.destroy();
        }
    }
}

impl Dispatch<dmabuf::ZwpLinuxDmabufV1, ()> for State {
    fn event(_: &mut Self, _: &dmabuf::ZwpLinuxDmabufV1, _: dmabuf::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // format/modifier: deprecated since version 4 (the feedback says it).
    }
}

impl Dispatch<feedback::ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(st: &mut Self, fb: &feedback::ZwpLinuxDmabufFeedbackV1, ev: feedback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let d = st.dma.as_mut().unwrap();
        match ev {
            feedback::Event::FormatTable { fd, size } => {
                let mut buf = vec![0u8; size as usize];
                // SAFETY: pread into our own buffer of `size` bytes from the table fd we were given.
                let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                buf.truncate(n.max(0) as usize);
                d.table = buf
                    .chunks_exact(16)
                    .map(|c| (u32::from_ne_bytes(c[0..4].try_into().unwrap()), u64::from_ne_bytes(c[8..16].try_into().unwrap())))
                    .collect();
            }
            feedback::Event::MainDevice { device } if device.len() == 8 => {
                d.main_device = Some(u64::from_ne_bytes(device[..8].try_into().unwrap()));
            }
            feedback::Event::TrancheFormats { indices } => d.tranche_formats += indices.len() / 2,
            feedback::Event::TrancheDone => d.tranches += 1,
            feedback::Event::Done => {
                d.fb_done = true;
                fb.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<params::ZwpLinuxBufferParamsV1, (u32, u8)> for State {
    fn event(st: &mut Self, p: &params::ZwpLinuxBufferParamsV1, ev: params::Event, data: &(u32, u8), _: &Connection, qh: &QueueHandle<Self>) {
        let (generation, k) = (data.0, &data.1);
        match ev {
            params::Event::Created { buffer } => {
                let d = st.dma.as_mut().unwrap();
                p.destroy();
                if generation != d.generation {
                    buffer.destroy();
                    return;
                }
                if let Some(slot) = d.made.get_mut(*k as usize) {
                    *slot = Some(buffer);
                }
                d.waiting = d.waiting.saturating_sub(1);
                println!("dmabuf created {k}");
                if d.waiting == 0 {
                    st.dma_ready(qh);
                }
            }
            params::Event::Failed => {
                println!("dmabuf failed");
                p.destroy();
                std::process::exit(3);
            }
            _ => {}
        }
    }

    event_created_child!(State, params::ZwpLinuxBufferParamsV1, [
        params::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, 0u8),
    ]);
}

wayland_client::delegate_noop!(State: ignore sync_mgr::WpLinuxDrmSyncobjManagerV1);
wayland_client::delegate_noop!(State: ignore sync_surf::WpLinuxDrmSyncobjSurfaceV1);
wayland_client::delegate_noop!(State: ignore sync_tl::WpLinuxDrmSyncobjTimelineV1);
