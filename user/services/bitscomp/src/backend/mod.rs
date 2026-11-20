//! The back end: V2's `/dev/fb0` (WY-1a, `fbdev`) for the picture, `/dev/input/event*` read directly for input
//! (`evdev`, no udev or libinput), the console handed over by KD_GRAPHICS (`vt`). The renderer draws straight into
//! fb0's scanout windows (`scanout`).

pub mod evdev;
pub mod evdev_map;
#[cfg(test)]
mod evdev_tests;
pub mod fbcursor;
pub mod fbdev;
mod flip;
pub mod hotplug;
pub mod scanout;
pub mod vt;
