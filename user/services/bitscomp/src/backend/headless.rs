//! No window: the frame goes into a memory buffer the size of a framebuffer mapping.

use smithay::utils::{Physical, Rectangle};

use super::{Backend, copy_rects};
use crate::render::Compositor;
use crate::state::App;

pub struct Headless {
    size: (i32, i32),
    fb: Vec<u32>,
    /// WY-11 `--pointer none|soft|fake`: no pointer (the reference pictures), the software pointer (fbdev's
    /// ENODEV path), or a fake hardware cursor that records its calls.
    pointer: String,
}

impl Headless {
    pub fn new(size: (i32, i32), pointer: &str) -> Self {
        Self { size, fb: vec![0; (size.0 * size.1) as usize], pointer: pointer.to_string() }
    }
}

impl Backend for Headless {
    fn size(&self) -> (i32, i32) {
        self.size
    }

    fn dispatch(&mut self, app: &mut App) {
        if let Some(size) = app.requested_size.take() {
            self.size = size;
            self.fb = vec![0; (size.0 * size.1) as usize];
            app.set_mode(size);
        }
    }

    fn draws_pointer(&self) -> bool {
        self.pointer != "none"
    }

    fn hw_cursor(&mut self) -> Option<Box<dyn crate::hwcursor::HwCursor>> {
        (self.pointer == "fake").then(|| Box::new(crate::hwcursor::Fake::default()) as Box<dyn crate::hwcursor::HwCursor>)
    }

    fn present(&mut self, comp: &Compositor, damage: &[Rectangle<i32, Physical>]) {
        if comp.size() == self.size {
            copy_rects(comp, &mut self.fb, self.size.0 as usize, damage);
        }
    }
}
