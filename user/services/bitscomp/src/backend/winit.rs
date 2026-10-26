//! Nested in the host's Wayland session: one winit window, the frame copied into wl_shm buffers (hostshm).
//! Smithay's own winit back end is GL-only; this one keeps the host path as CPU-bound as V2's fbdev path.

use std::os::fd::BorrowedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::utils::{Physical, Rectangle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use super::Backend;
use super::hostshm::HostShm;
use crate::input::InputEvent;
use crate::render::Compositor;
use crate::state::App;

struct Pump {
    window: Option<Rc<Window>>,
    events: Vec<WindowEvent>,
    size: (i32, i32),
}

impl ApplicationHandler for Pump {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("bitscomp (WY-0b) - Strg+Alt+Q beendet")
                .with_inner_size(PhysicalSize::new(self.size.0 as u32, self.size.1 as u32))
                .with_name("bitscomp", "bitscomp");
            self.window = Some(Rc::new(el.create_window(attrs).expect("host window")));
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.events.push(event);
    }
}

/// Field order is drop order: our buffers, then the window, then the host connection.
pub struct Winit {
    shm: HostShm,
    pump: Pump,
    event_loop: EventLoop<()>,
    cursor: Option<smithay::input::pointer::CursorIcon>,
}

impl Winit {
    pub fn new(size: (i32, i32)) -> Result<Self, String> {
        let event_loop = EventLoop::builder().with_wayland().build().map_err(|e| e.to_string())?;
        let mut pump = Pump { window: None, events: Vec::new(), size };
        let mut event_loop = event_loop;
        // The first pump creates the window.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pump.window.is_none() && Instant::now() < deadline {
            event_loop.pump_app_events(Some(Duration::from_millis(10)), &mut pump);
        }
        let window = pump.window.clone().ok_or("no host window")?;
        let RawDisplayHandle::Wayland(d) = window.display_handle().map_err(|e| e.to_string())?.as_raw() else {
            return Err("host is not Wayland".into());
        };
        let RawWindowHandle::Wayland(h) = window.window_handle().map_err(|e| e.to_string())?.as_raw() else {
            return Err("host window is not Wayland".into());
        };
        // SAFETY: both pointers belong to the window, which lives as long as this back end.
        let shm = unsafe { HostShm::new(d.display.as_ptr(), h.surface.as_ptr())? };
        let s = window.inner_size();
        pump.size = (s.width as i32, s.height as i32);
        let w = Self { event_loop, pump, shm, cursor: None };
        Ok(w)
    }
}

fn button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x116,
        MouseButton::Forward => 0x115,
        MouseButton::Other(n) => 0x110 + n as u32,
    }
}

impl Backend for Winit {
    fn size(&self) -> (i32, i32) {
        self.pump.size
    }

    fn dispatch(&mut self, app: &mut App) {
        self.event_loop.pump_app_events(Some(Duration::ZERO), &mut self.pump);
        let window = self.pump.window.clone().unwrap();
        if let Some((w, h)) = app.requested_size.take() {
            // On Wayland winit applies the size at once and sends no Resized event.
            if let Some(s) = window.request_inner_size(PhysicalSize::new(w as u32, h as u32)) {
                self.pump.size = (s.width as i32, s.height as i32);
                app.set_mode(self.pump.size);
            }
        }
        for ev in std::mem::take(&mut self.pump.events) {
            match ev {
                WindowEvent::CloseRequested => app.quit = true,
                WindowEvent::Resized(s) if s.width > 0 && s.height > 0 => {
                    self.pump.size = (s.width as i32, s.height as i32);
                    app.set_mode(self.pump.size);
                }
                WindowEvent::Focused(false) => app.release_all_keys(),
                WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                    let pressed = event.state == ElementState::Pressed;
                    // winit invents presses for keys held while focus arrives; only its releases matter.
                    if is_synthetic && pressed {
                        continue;
                    }
                    if let Some(code) = event.physical_key.to_scancode() {
                        app.handle_input(InputEvent::Key { code, pressed });
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    app.handle_input(InputEvent::Motion { x: position.x, y: position.y })
                }
                WindowEvent::MouseInput { state, button, .. } => app.handle_input(InputEvent::Button {
                    code: button_code(button),
                    pressed: state == ElementState::Pressed,
                }),
                WindowEvent::MouseWheel { delta, .. } => {
                    let steps = match delta {
                        MouseScrollDelta::LineDelta(_, y) => -y as f64,
                        MouseScrollDelta::PixelDelta(p) => -p.y / 15.0,
                    };
                    app.handle_input(InputEvent::Wheel { steps });
                }
                WindowEvent::RedrawRequested => app.needs_frame = true,
                _ => {}
            }
        }
        // bitscomp's own shape over its frames (resize edges, gaps), the client's over the client.
        let want = app.cursor_deco.or(app.cursor_named);
        if want != self.cursor {
            self.cursor = want;
            window.set_cursor(self.cursor.unwrap_or_default());
        }
    }

    fn present(&mut self, comp: &Compositor, damage: &[Rectangle<i32, Physical>]) {
        self.shm.present(comp, damage);
    }

    fn flush(&mut self, comp: &Compositor) {
        self.shm.flush(comp);
    }

    fn wake_fd(&self) -> Option<BorrowedFd<'static>> {
        let window = self.pump.window.as_ref()?;
        let RawDisplayHandle::Wayland(h) = window.display_handle().ok()?.as_raw() else { return None };
        let fd = unsafe {
            wayland_sys::ffi_dispatch!(
                wayland_sys::client::wayland_client_handle(),
                wl_display_get_fd,
                h.display.as_ptr() as *mut _
            )
        };
        // SAFETY: the host connection lives as long as the window, which lives until the process ends.
        Some(unsafe { BorrowedFd::borrow_raw(fd) })
    }
}
