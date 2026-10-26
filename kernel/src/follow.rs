//! GX-4 change request 1: the screen's software side follows a display mode switch (amdgpu's `modeset`): the
//! live GopFb's visible size, the console's RAM frame, fbdev's geometry and the VT grid, in that order. `check`
//! asks before anything is written: the console has the screen (no program holds it) and fb0 can take the size.

use alloc::string::String;

fn check(w: u32, h: u32) -> Result<(), String> {
    if let Some((owner, pid)) = display::scanout_holder() {
        return Err(alloc::format!("the screen is held by {owner} (pid {pid}): switch the mode from the console"));
    }
    gopfb::fbdev::can_resize(w, h)
}

fn resize(w: u32, h: u32) -> Result<(), String> {
    gopfb::resize_live(w, h)?;
    display::resize(w, h).map_err(String::from)?;
    let fb = gopfb::fbdev::resize(w, h);
    // The VT follows the frame either way: the console must never draw outside it.
    console::vt::refit();
    fb
}

static FOLLOW: amdgpu::modeset::Follower = amdgpu::modeset::Follower { check, resize };

/// After `console::init`: amdgpu's mode switch may run from then on.
pub fn install() {
    if !amdgpu::modeset::set_follower(&FOLLOW) {
        klog::event!(Warn, "kernel", "follow.refused"; "a mode follower is installed already");
    }
}
