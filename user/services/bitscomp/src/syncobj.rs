//! WY-16: explicit sync, wp_linux_drm_syncobj_manager_v1 (staging, version 1) with DR-3's sync objects. Written
//! here instead of Smithay's `drm_syncobj` module, which needs its `backend_drm` feature and with it the crates
//! `drm`/`drm-ffi` that plan/spec/rust-ecosystem.md does not name (no download); the ioctls are `drm_sync.rs`.
//!
//! A client imports timelines (syncobj fds) and sets, with each buffer commit, an acquire point bitscomp waits for
//! before the commit is applied (a Smithay blocker per commit, cleared through DRM_IOCTL_SYNCOBJ_EVENTFD's eventfd
//! armed for the surface's oldest waiting point only, `acquire::Queue`, so the loop never blocks, B4, and a client
//! never costs more than one fd per surface) and a release point bitscomp signals once the renderer is done with that buffer (`Release`,
//! given back by `release::Hold` after the frame). Protocol errors as the XML says (unsupported_buffer for
//! non-dma-buf buffers, no_buffer, no_acquire_point, no_release_point, conflicting_points, surface_exists,
//! invalid_timeline, no_surface).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay::reexports::wayland_protocols::wp::linux_drm_syncobj::v1::server::{
    wp_linux_drm_syncobj_manager_v1 as manager, wp_linux_drm_syncobj_surface_v1 as surf,
    wp_linux_drm_syncobj_timeline_v1 as timeline,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak};
use smithay::wayland::compositor::{
    self, BufferAssignment, Cacheable, HookId, SurfaceAttributes, SurfaceData, with_states,
};

use crate::ClientState;
use crate::drm_sync::Drm;
use crate::state::App;

pub struct Syncobj {
    drm: Arc<Drm>,
    handle: LoopHandle<'static, App>,
    /// Surfaces with commits waiting for their acquire point; an entry leaves when its queue is empty or its
    /// surface dies (`prune`, each turn), so a client that never signals leaks nothing past its surface.
    waits: HashMap<ObjectId, Waits>,
}

/// One surface's waiting commits and the one source armed for the oldest: an eventfd, or a retry timer after the
/// kernel refused one (no fd left, a timeline it cannot read): such a commit stays blocked, never applied unsynced.
struct Waits {
    surface: Weak<WlSurface>,
    queue: crate::acquire::Queue<Point>,
    armed: Option<RegistrationToken>,
    retry: Duration,
}

/// The first retry after a refused eventfd or wait, doubled up to `RETRY_MAX` (a time, not a count).
const RETRY_MIN: Duration = Duration::from_millis(10);
const RETRY_MAX: Duration = Duration::from_secs(1);

impl Syncobj {
    /// The global, when the render node opens and has timelines and syncobj eventfds; otherwise none (clients
    /// keep implicit sync), said once.
    pub fn start(dh: &DisplayHandle, node: Option<&str>, handle: LoopHandle<'static, App>) -> Option<Syncobj> {
        let Some(node) = node else {
            eprintln!("bitscomp: syncobj: no render node: no wp_linux_drm_syncobj_manager_v1 (implicit sync only)");
            return None;
        };
        let drm = match Drm::open(node).map_err(|e| e.to_string()).and_then(|d| d.has_timelines_and_eventfd().map(|_| d)) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("bitscomp: syncobj: {node}: {e}: no wp_linux_drm_syncobj_manager_v1 (implicit sync only)");
                return None;
            }
        };
        dh.create_global::<App, manager::WpLinuxDrmSyncobjManagerV1, _>(1, ());
        eprintln!("bitscomp: syncobj: wp_linux_drm_syncobj_manager_v1 on {node}");
        Some(Syncobj { drm: Arc::new(drm), handle, waits: HashMap::new() })
    }

    /// Each turn: surfaces that are gone (their armed source leaves the loop) or wait for nothing; returns how
    /// many commits still wait (`ctl stats` acquire_pending).
    pub fn prune(&mut self) -> usize {
        let handle = self.handle.clone();
        self.waits.retain(|_, w| {
            if w.surface.upgrade().is_err() {
                if let Some(t) = w.armed.take() {
                    handle.remove(t);
                }
                return false;
            }
            !w.queue.is_empty() || w.armed.is_some()
        });
        self.waits.values().map(|w| w.queue.len()).sum()
    }
}

/// The surface's oldest waiting commits that signalled are released (their client's queue is looked at again in an
/// idle callback, never from inside a commit hook), then one source is armed for the new head: its eventfd, or a
/// retry timer when the kernel refuses the eventfd or the wait (said once per surface until it works again).
fn arm(sync: &mut Syncobj, id: &ObjectId, dh: &DisplayHandle) {
    let handle = sync.handle.clone();
    let Some(w) = sync.waits.get_mut(id) else { return };
    if w.armed.is_some() {
        return;
    }
    let advanced = w.queue.advance(|p| p.tl.drm.signalled(p.tl.handle, p.value));
    if let Ok(true) = advanced {
        let (weak, dh) = (w.surface.clone(), dh.clone());
        handle.insert_idle(move |app: &mut App| {
            if let Ok(s) = weak.upgrade()
                && let Some(client) = s.client()
                && let Some(data) = client.get_data::<ClientState>()
            {
                data.compositor_state.blocker_cleared(app, &dh);
            }
        });
    }
    let armed = match advanced {
        Err(e) => Err(format!("acquire point not readable: {e}")),
        Ok(_) => {
            let Some(head) = w.queue.head() else { return };
            let (cb_id, cb_dh) = (id.clone(), dh.clone());
            head.tl.drm.eventfd(head.tl.handle, head.value).map_err(|e| format!("no eventfd for acquire point {}: {e}", head.value)).and_then(|efd| {
                handle
                    .insert_source(Generic::new(efd, Interest::READ, Mode::Level), move |_, _, app: &mut App| {
                        rearm(app, &cb_id, &cb_dh);
                        Ok(PostAction::Remove)
                    })
                    .map_err(|e| format!("acquire wait not registered: {e}"))
            })
        }
    };
    match armed {
        Ok(token) => {
            if w.retry > RETRY_MIN {
                eprintln!("bitscomp: syncobj: acquire wait armed again after retries");
            }
            w.armed = Some(token);
            w.retry = RETRY_MIN;
        }
        Err(e) => {
            if w.retry == RETRY_MIN {
                eprintln!("bitscomp: syncobj: {e}; the commit stays blocked, retried in {} ms and later", RETRY_MIN.as_millis());
            }
            let (cb_id, cb_dh) = (id.clone(), dh.clone());
            let t = handle.insert_source(Timer::from_duration(w.retry), move |_, _, app: &mut App| {
                rearm(app, &cb_id, &cb_dh);
                TimeoutAction::Drop
            });
            w.retry = (w.retry * 2).min(RETRY_MAX);
            match t {
                Ok(token) => w.armed = Some(token),
                // Not even a timer: the next commit of the surface or its death looks at it again.
                Err(e) => eprintln!("bitscomp: syncobj: no retry timer either: {e}"),
            }
        }
    }
}

/// The armed source fired (it leaves the loop by itself): advance and arm the next one.
fn rearm(app: &mut App, id: &ObjectId, dh: &DisplayHandle) {
    let Some(sync) = app.dma.sync.as_mut() else { return };
    if let Some(w) = sync.waits.get_mut(id) {
        w.armed = None;
    }
    arm(sync, id, dh);
}

/// An imported timeline: the handle on bitscomp's own render node file, destroyed with the last point using it.
struct Timeline {
    drm: Arc<Drm>,
    handle: u32,
}

impl Drop for Timeline {
    fn drop(&mut self) {
        self.drm.destroy(self.handle);
    }
}

/// A point on a timeline.
#[derive(Clone)]
pub struct Point {
    tl: Arc<Timeline>,
    value: u64,
}

/// A release point: signalled when it drops (given back after the frame, replaced unused in the cache, or with
/// its surface), so a client never waits forever for a buffer bitscomp holds no more.
pub struct Release(Point);

impl Release {
    /// Release points signal in this order (timeline, then value): a lower point signalled after a higher one on
    /// one timeline would be an out-of-order point (linux-6.6/drivers/gpu/drm/drm_syncobj.c:327-343), whose waiter
    /// may never see it.
    pub fn order_key(&self) -> (usize, u64) {
        (Arc::as_ptr(&self.0.tl) as usize, self.0.value)
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        if let Err(e) = self.0.tl.drm.signal(self.0.tl.handle, self.0.value) {
            eprintln!("bitscomp: syncobj: release point {} not signalled: {e}", self.0.value);
        }
    }
}

/// The double-buffered points of a surface (applied with its commit, also for synchronised subsurfaces).
#[derive(Default)]
pub struct Cached {
    acquire: Option<Point>,
    release: Option<Release>,
}

impl Cacheable for Cached {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        Cached { acquire: self.acquire.take(), release: self.release.take() }
    }
    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        // Points come only with a buffer (pre_commit); a newer buffer replaces an older one that was never applied,
        // whose release point then signals at once (dropped here): bitscomp never read that buffer.
        if self.release.is_some() || self.acquire.is_some() {
            into.acquire = self.acquire;
            into.release = self.release;
        }
    }
}

/// The surface's live wp_linux_drm_syncobj_surface_v1 (one at a time) and the release point of the buffer it
/// shows now.
#[derive(Default)]
struct Slot {
    object: Option<surf::WpLinuxDrmSyncobjSurfaceV1>,
    current: Option<Release>,
}

pub struct SurfaceObj {
    surface: Weak<WlSurface>,
    hook: HookId,
}

fn slot<T>(states: &SurfaceData, f: impl FnOnce(&mut Slot) -> T) -> T {
    let m = states.data_map.get_or_insert_threadsafe(|| Mutex::new(Slot::default()));
    let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut g)
}

/// While a commit is applied (`release::Hold::keep_tree`, before Smithay takes the buffer): the release point of
/// the buffer being replaced or removed, to give back after the frame; the new buffer's point becomes current.
pub fn on_apply(states: &SurfaceData) -> Option<Release> {
    let new = {
        let mut c = states.cached_state.get::<Cached>();
        let cur = c.current();
        cur.acquire = None;
        cur.release.take()
    };
    slot(states, |s| std::mem::replace(&mut s.current, new))
}

fn post(obj: &surf::WpLinuxDrmSyncobjSurfaceV1, e: surf::Error, what: &str) {
    obj.post_error(e, what.to_string());
}

/// What a commit's pending state asks for (the protocol's rules, unit-tested): `buffer` is None without a new
/// buffer, else whether it is a linux-dmabuf one; the points as (timeline, value) with the timeline's identity.
#[derive(Debug, PartialEq)]
enum Verdict {
    Apply,
    Wait,
    Error(surf::Error, &'static str),
}

fn verdict(buffer: Option<bool>, acquire: Option<(usize, u64)>, release: Option<(usize, u64)>) -> Verdict {
    match (buffer, acquire, release) {
        (None, None, None) => Verdict::Apply,
        (None, _, _) => Verdict::Error(surf::Error::NoBuffer, "points set without a buffer"),
        (Some(false), _, _) => Verdict::Error(surf::Error::UnsupportedBuffer, "explicit sync needs a linux-dmabuf buffer"),
        (Some(true), None, _) => Verdict::Error(surf::Error::NoAcquirePoint, "no acquire point"),
        (Some(true), _, None) => Verdict::Error(surf::Error::NoReleasePoint, "no release point"),
        (Some(true), Some((ta, a)), Some((tr, r))) if ta == tr && a >= r => {
            Verdict::Error(surf::Error::ConflictingPoints, "acquire point not below release point on one timeline")
        }
        (Some(true), Some(_), Some(_)) => Verdict::Wait,
    }
}

/// Checks a commit of a surface with a syncobj object (the protocol's rules on the pending state) and holds it
/// until its acquire point has signalled.
fn pre_commit(app: &mut App, dh: &DisplayHandle, surface: &WlSurface) {
    let checked = with_states(surface, |states| {
        let obj = slot(states, |s| s.object.clone())?;
        let new_buffer = match &states.cached_state.get::<SurfaceAttributes>().pending().buffer {
            Some(BufferAssignment::NewBuffer(b)) => Some(smithay::wayland::dmabuf::get_dmabuf(b).is_ok()),
            _ => None,
        };
        let mut c = states.cached_state.get::<Cached>();
        let p = c.pending();
        let key = |p: &Point| (Arc::as_ptr(&p.tl) as usize, p.value);
        match verdict(new_buffer, p.acquire.as_ref().map(key), p.release.as_ref().map(|r| key(&r.0))) {
            Verdict::Apply => None,
            Verdict::Error(e, what) => {
                post(&obj, e, what);
                None
            }
            Verdict::Wait => p.acquire.clone(),
        }
    });
    let Some(acquire) = checked else { return };
    let Some(sync) = app.dma.sync.as_mut() else { return };
    let id = surface.id();
    let w = sync.waits.entry(id.clone()).or_insert_with(|| Waits {
        surface: surface.downgrade(),
        queue: Default::default(),
        armed: None,
        retry: RETRY_MIN,
    });
    // Nothing older waits and the point is already there: applied now, no blocker at all.
    if w.queue.is_empty() && matches!(acquire.tl.drm.signalled(acquire.tl.handle, acquire.value), Ok(true)) {
        return;
    }
    let flag = w.queue.push(acquire);
    compositor::add_blocker(surface, Wait(flag));
    app.stats.acquire_waits += 1;
    arm(sync, &id, dh);
}

struct Wait(Arc<AtomicBool>);

impl compositor::Blocker for Wait {
    fn state(&self) -> compositor::BlockerState {
        if self.0.load(Ordering::Acquire) { compositor::BlockerState::Released } else { compositor::BlockerState::Pending }
    }
}

impl GlobalDispatch<manager::WpLinuxDrmSyncobjManagerV1, ()> for App {
    fn bind(
        _: &mut App,
        _: &DisplayHandle,
        _: &Client,
        res: New<manager::WpLinuxDrmSyncobjManagerV1>,
        _: &(),
        init: &mut DataInit<'_, App>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<manager::WpLinuxDrmSyncobjManagerV1, ()> for App {
    fn request(
        app: &mut App,
        _: &Client,
        res: &manager::WpLinuxDrmSyncobjManagerV1,
        req: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, App>,
    ) {
        match req {
            manager::Request::GetSurface { id, surface } => {
                if with_states(&surface, |st| slot(st, |s| s.object.is_some())) {
                    res.post_error(manager::Error::SurfaceExists, "the surface has a syncobj object already");
                    return;
                }
                let hook = compositor::add_pre_commit_hook::<App, _>(&surface, pre_commit);
                let obj = init.init(id, SurfaceObj { surface: surface.downgrade(), hook });
                with_states(&surface, |st| slot(st, |s| s.object = Some(obj)));
            }
            manager::Request::ImportTimeline { id, fd } => {
                let Some(sync) = app.dma.sync.as_ref() else {
                    res.post_error(manager::Error::InvalidTimeline, "no syncobj device");
                    return;
                };
                match sync.drm.import(std::os::fd::AsFd::as_fd(&fd)) {
                    Ok(handle) => {
                        init.init(id, Arc::new(Timeline { drm: sync.drm.clone(), handle }));
                    }
                    Err(e) => res.post_error(manager::Error::InvalidTimeline, format!("not a syncobj: {e}")),
                }
            }
            manager::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<timeline::WpLinuxDrmSyncobjTimelineV1, Arc<Timeline>> for App {
    fn request(
        _: &mut App,
        _: &Client,
        _: &timeline::WpLinuxDrmSyncobjTimelineV1,
        _: timeline::Request,
        _: &Arc<Timeline>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, App>,
    ) {
        // Destroy only; points already set keep the timeline alive (their Arc).
    }
}

impl Dispatch<surf::WpLinuxDrmSyncobjSurfaceV1, SurfaceObj> for App {
    fn request(
        _: &mut App,
        _: &Client,
        res: &surf::WpLinuxDrmSyncobjSurfaceV1,
        req: surf::Request,
        data: &SurfaceObj,
        _: &DisplayHandle,
        _: &mut DataInit<'_, App>,
    ) {
        let surface = data.surface.upgrade().ok();
        let point = |tl: &timeline::WpLinuxDrmSyncobjTimelineV1, hi: u32, lo: u32| {
            tl.data::<Arc<Timeline>>().map(|t| Point { tl: t.clone(), value: (u64::from(hi) << 32) | u64::from(lo) })
        };
        match req {
            surf::Request::Destroy => {
                if let Some(s) = surface {
                    compositor::remove_pre_commit_hook(&s, data.hook.clone());
                    with_states(&s, |st| {
                        slot(st, |sl| sl.object = None);
                        // Points set since the last commit are discarded (their release point signals).
                        let mut c = st.cached_state.get::<Cached>();
                        *c.pending() = Cached::default();
                    });
                }
            }
            surf::Request::SetAcquirePoint { timeline, point_hi, point_lo } => {
                let Some(s) = surface else {
                    return res.post_error(surf::Error::NoSurface, "the surface is gone");
                };
                let p = point(&timeline, point_hi, point_lo);
                with_states(&s, |st| st.cached_state.get::<Cached>().pending().acquire = p);
            }
            surf::Request::SetReleasePoint { timeline, point_hi, point_lo } => {
                let Some(s) = surface else {
                    return res.post_error(surf::Error::NoSurface, "the surface is gone");
                };
                let p = point(&timeline, point_hi, point_lo).map(Release);
                with_states(&s, |st| st.cached_state.get::<Cached>().pending().release = p);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Verdict, surf, verdict};

    #[test]
    fn the_protocol_rules_of_a_commit() {
        let e = |x| match verdict_of(x) {
            Verdict::Error(e, _) => Some(e),
            _ => None,
        };
        fn verdict_of(x: (Option<bool>, Option<(usize, u64)>, Option<(usize, u64)>)) -> Verdict {
            verdict(x.0, x.1, x.2)
        }
        assert_eq!(verdict(None, None, None), Verdict::Apply);
        assert_eq!(e((None, Some((1, 1)), None)), Some(surf::Error::NoBuffer));
        assert_eq!(e((None, None, Some((1, 1)))), Some(surf::Error::NoBuffer));
        assert_eq!(e((Some(false), Some((1, 1)), Some((2, 1)))), Some(surf::Error::UnsupportedBuffer));
        assert_eq!(e((Some(false), None, None)), Some(surf::Error::UnsupportedBuffer));
        assert_eq!(e((Some(true), None, Some((2, 1)))), Some(surf::Error::NoAcquirePoint));
        assert_eq!(e((Some(true), Some((1, 1)), None)), Some(surf::Error::NoReleasePoint));
        assert_eq!(e((Some(true), Some((1, 5)), Some((1, 5)))), Some(surf::Error::ConflictingPoints));
        assert_eq!(e((Some(true), Some((1, 6)), Some((1, 5)))), Some(surf::Error::ConflictingPoints));
        assert_eq!(verdict(Some(true), Some((1, 4)), Some((1, 5))), Verdict::Wait, "release above acquire on one timeline");
        assert_eq!(verdict(Some(true), Some((1, 9)), Some((2, 1))), Verdict::Wait, "two timelines");
    }
}
