//! WY-20: the protocols of mouse look, as SDL2 and Xwayland use them. `--relative` asks for zwp_relative_pointer_v1 and
//! prints `relative DX DY RAWX RAWY` (`relative: no manager` when there is none). `--lock persistent|oneshot` and
//! `--confine persistent|oneshot` make that constraint on the window's surface once the pointer object exists (`no
//! constraints` without the global) and print `locked`, `unlocked`, `confined`, `unconfined`. `--region X,Y,W,H` limits
//! it to a rectangle; `--hint X,Y`, sent once locked, says where the pointer goes when the lock ends. F7 destroys the constraint
//! (`constraint destroyed`), as a game does when it goes back to its menu.

use wayland_client::protocol::{wl_pointer, wl_region};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{self, ZwpLockedPointerV1};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{self, ZwpRelativePointerV1};

use super::State;

#[derive(Default)]
pub struct Lock {
    pub relative: bool,
    pub lock: Option<Lifetime>,
    pub confine: Option<Lifetime>,
    pub region: Option<[i32; 4]>,
    pub hint: Option<(f64, f64)>,
    pub relative_mgr: Option<ZwpRelativePointerManagerV1>,
    pub constraints: Option<ZwpPointerConstraintsV1>,
    pub pointer: Option<wl_pointer::WlPointer>,
    asked: bool,
    locked: Option<ZwpLockedPointerV1>,
    confined: Option<ZwpConfinedPointerV1>,
}

pub fn lifetime(v: &str) -> Option<Lifetime> {
    match v {
        "persistent" => Some(Lifetime::Persistent),
        "oneshot" => Some(Lifetime::Oneshot),
        _ => None,
    }
}

/// `A,B,..` as numbers, `n` of them.
pub fn numbers<const N: usize>(v: &str) -> Option<[i32; N]> {
    let list: Vec<i32> = v.split(',').map(|w| w.parse()).collect::<Result<_, _>>().ok()?;
    list.try_into().ok()
}

/// Makes what was asked for, once the pointer and the surface are there (they come in either order).
pub fn ask(st: &mut State, qh: &QueueHandle<State>) {
    let (Some(pointer), Some(surface)) = (st.lock.pointer.clone(), st.surface.clone()) else { return };
    if st.lock.asked {
        return;
    }
    st.lock.asked = true;
    if st.lock.relative {
        match &st.lock.relative_mgr {
            Some(m) => {
                m.get_relative_pointer(&pointer, qh, ());
            }
            None => println!("relative: no manager"),
        }
    }
    if st.lock.lock.is_none() && st.lock.confine.is_none() {
        return;
    }
    let (Some(c), Some(comp)) = (st.lock.constraints.clone(), st.compositor.clone()) else {
        println!("no constraints");
        return;
    };
    let region = st.lock.region.map(|[x, y, w, h]| {
        let r = comp.create_region(qh, ());
        r.add(x, y, w, h);
        r
    });
    if let Some(l) = st.lock.lock {
        st.lock.locked = Some(c.lock_pointer(&surface, &pointer, region.as_ref(), l, qh, ()));
    }
    if let Some(l) = st.lock.confine {
        st.lock.confined = Some(c.confine_pointer(&surface, &pointer, region.as_ref(), l, qh, ()));
    }
}

/// F7: the client ends what it holds.
pub fn destroy(st: &mut State) {
    if let Some(l) = st.lock.locked.take() {
        l.destroy();
    }
    if let Some(c) = st.lock.confined.take() {
        c.destroy();
    }
    println!("constraint destroyed");
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpRelativePointerV1,
        ev: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, dx_unaccel, dy_unaccel, .. } = ev {
            println!("relative {dx:.2} {dy:.2} {dx_unaccel:.2} {dy_unaccel:.2}");
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        st: &mut Self,
        l: &ZwpLockedPointerV1,
        ev: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwp_locked_pointer_v1::Event::Locked => {
                println!("locked");
                // The hint is double-buffered and means something only under a lock.
                if let (Some((x, y)), Some(s)) = (st.lock.hint, &st.surface) {
                    l.set_cursor_position_hint(x, y);
                    s.commit();
                }
            }
            zwp_locked_pointer_v1::Event::Unlocked => println!("unlocked"),
            _ => {}
        }
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpConfinedPointerV1,
        ev: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwp_confined_pointer_v1::Event::Confined => println!("confined"),
            zwp_confined_pointer_v1::Event::Unconfined => println!("unconfined"),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ZwpRelativePointerManagerV1);
delegate_noop!(State: ignore ZwpPointerConstraintsV1);
delegate_noop!(State: ignore wl_region::WlRegion);
