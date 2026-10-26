//! Frames into the host window through our own wl_shm buffers on winit's connection. Never waits for the
//! host (B4): a buffer still held by the host is skipped, a third one is made when needed, and when all
//! are busy the damage waits for the next release.

use std::fs::File;

use memmap2::MmapMut;
use smithay::reexports::rustix::fs::{MemfdFlags, memfd_create};
use smithay::utils::{Physical, Rectangle};
use wayland_client::backend::{Backend as WlBackend, ObjectId};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use super::copy_rects;
use crate::render::Compositor;

const MAX_BUFFERS: usize = 3;

#[derive(Default)]
struct Shared {
    shm: Option<wl_shm::WlShm>,
    busy: Vec<bool>,
}

struct Buf {
    buffer: wl_buffer::WlBuffer,
    map: MmapMut,
    /// Damage this buffer has not seen yet.
    stale: Vec<Rectangle<i32, Physical>>,
}

pub struct HostShm {
    conn: Connection,
    queue: EventQueue<Shared>,
    shared: Shared,
    surface: wl_surface::WlSurface,
    bufs: Vec<Buf>,
    size: (i32, i32),
    dirty: bool,
}

impl HostShm {
    /// # Safety
    /// `display` and `surface` must be the live wl_display and wl_surface of the host window.
    pub unsafe fn new(display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) -> Result<Self, String> {
        let backend = unsafe { WlBackend::from_foreign_display(display as *mut _) };
        let conn = Connection::from_backend(backend);
        let mut queue = conn.new_event_queue::<Shared>();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut shared = Shared::default();
        queue.roundtrip(&mut shared).map_err(|e| e.to_string())?;
        if shared.shm.is_none() {
            return Err("host has no wl_shm".into());
        }
        let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface as *mut _) }
            .map_err(|e| e.to_string())?;
        let surface = wl_surface::WlSurface::from_id(&conn, id).map_err(|e| e.to_string())?;
        Ok(Self { conn, queue, shared, surface, bufs: Vec::new(), size: (0, 0), dirty: false })
    }

    fn new_buffer(&mut self) -> Result<Buf, String> {
        let (w, h) = self.size;
        let len = (w * h * 4) as u64;
        let fd = memfd_create("bitscomp-host", MemfdFlags::CLOEXEC).map_err(|e| e.to_string())?;
        let file = File::from(fd);
        file.set_len(len).map_err(|e| e.to_string())?;
        // SAFETY: the memfd is ours; the host only reads it.
        let map = unsafe { MmapMut::map_mut(&file) }.map_err(|e| e.to_string())?;
        let qh = self.queue.handle();
        let pool = self.shared.shm.as_ref().unwrap().create_pool(std::os::fd::AsFd::as_fd(&file), len as i32, &qh, ());
        let index = self.bufs.len();
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, &qh, index);
        pool.destroy();
        self.shared.busy.push(false);
        Ok(Buf { buffer, map, stale: vec![Rectangle::from_size((w, h).into())] })
    }

    pub fn present(&mut self, comp: &Compositor, damage: &[Rectangle<i32, Physical>]) {
        if comp.size() != self.size {
            for b in self.bufs.drain(..) {
                b.buffer.destroy();
            }
            self.shared.busy.clear();
            self.size = comp.size();
        }
        for b in &mut self.bufs {
            b.stale.extend_from_slice(damage);
            if b.stale.len() > 64 {
                b.stale = vec![Rectangle::from_size(self.size.into())];
            }
        }
        self.dirty |= !damage.is_empty();
        self.flush(comp);
    }

    pub fn flush(&mut self, comp: &Compositor) {
        let _ = self.queue.dispatch_pending(&mut self.shared);
        if !self.dirty || comp.size() != self.size {
            return;
        }
        let free = (0..self.bufs.len()).find(|&i| !self.shared.busy[i]);
        let i = match free {
            Some(i) => i,
            None if self.bufs.len() < MAX_BUFFERS => match self.new_buffer() {
                Ok(b) => {
                    self.bufs.push(b);
                    self.bufs.len() - 1
                }
                Err(e) => {
                    eprintln!("bitscomp: host buffer: {e}");
                    return;
                }
            },
            None => return,
        };
        let (w, h) = self.size;
        let b = &mut self.bufs[i];
        // SAFETY: the mapping is w*h u32 long and 4-byte aligned (page aligned).
        let dst = unsafe { std::slice::from_raw_parts_mut(b.map.as_mut_ptr() as *mut u32, (w * h) as usize) };
        copy_rects(comp, dst, w as usize, &b.stale);
        for r in b.stale.drain(..) {
            self.surface.damage_buffer(r.loc.x, r.loc.y, r.size.w, r.size.h);
        }
        self.surface.attach(Some(&b.buffer), 0, 0);
        self.surface.commit();
        self.shared.busy[i] = true;
        self.dirty = false;
        let _ = self.conn.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Shared {
    fn event(
        s: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = ev
            && interface == "wl_shm"
            && s.shm.is_none()
        {
            s.shm = Some(reg.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Shared {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Shared {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for Shared {
    fn event(
        s: &mut Self,
        _: &wl_buffer::WlBuffer,
        ev: wl_buffer::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = ev
            && let Some(b) = s.busy.get_mut(*i)
        {
            *b = false;
        }
    }
}
