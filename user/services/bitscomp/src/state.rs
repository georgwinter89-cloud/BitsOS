//! Compositor state: the protocol globals of V2-PLAN §8a.2, the seat, the output and the workspace.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::keyboard::{Keycode, XkbConfig};
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::utils::{Clock, Logical, Monotonic, Point, Transform};
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::viewporter::ViewporterState;

use crate::grab::Grab;
use crate::layout::Workspace;
use crate::menu::{Items, Menu};
use crate::stats::Stats;
use crate::theme::Theme;
use crate::zones::Part;

/// Key repeat as bitscomp tells clients through wl_keyboard.repeat_info; clients repeat themselves from it
/// (Wayland's model), and the kernel's soft repeat (evdev value 2) is ignored so nothing repeats twice. They become the
/// settings keys `input.repeat_delay_ms` / `input.repeat_rate` with settingsd (WY-5 or later). bitscomp's own
/// Ctrl+Alt shortcuts do not repeat: every window action has its mouse form (desktop.md), none needs holding.
pub struct KeyRepeat {
    pub delay_ms: i32,
    pub rate: i32,
}

pub const KEY_REPEAT: KeyRepeat = KeyRepeat { delay_ms: 400, rate: 30 };

pub struct App {
    pub dh: DisplayHandle,
    pub socket_name: String,
    pub start: Instant,
    /// The current theme (WY-5: rebuilt from the settings, `apply`); `theme_gen` counts its changes.
    pub theme: std::rc::Rc<Theme>,
    pub theme_gen: u64,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)] // owns the global for the whole run
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // owns the global for the whole run
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<App>,
    pub data_device_state: DataDeviceState,
    #[allow(dead_code)] // owns the global for the whole run
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)] // owns the global for the whole run
    pub fractional_scale_state: FractionalScaleManagerState,
    #[allow(dead_code)] // owns the global for the whole run
    pub presentation_state: PresentationState,
    pub popups: PopupManager,

    pub seat: Seat<App>,
    pub output: Output,
    pub space: Space<Window>,
    pub ws: Workspace<Window>,
    /// Decoration pieces keep their buffers between frames so the damage tracker sees them unchanged.
    pub deco: HashMap<(ObjectId, u8), SolidColorBuffer>,
    /// Header images, redrawn only when their look (tabs, hover, focus, size) changes.
    pub deco_img: HashMap<ObjectId, (crate::header::Look, MemoryRenderBuffer)>,
    pub menu_img: Option<((Items, Option<usize>, u64), MemoryRenderBuffer)>,
    /// The rounded bottom corners of tiles, per window and side.
    pub corner_img: HashMap<(ObjectId, bool), (crate::deco::CornerKey, crate::layout::Rect, MemoryRenderBuffer)>,
    pub preview_buf: SolidColorBuffer,
    pub pointer: Point<f64, Logical>,
    pub grab: Option<Grab>,
    pub menu: Option<Menu<Window>>,
    /// The header button or tab under the pointer (drawn highlighted).
    pub hover: Option<(Window, Part)>,
    /// WY-5c: the last press on a floating window's header (window, `pacer` time, place), for double clicks.
    pub last_click: Option<(Window, std::time::Duration, (f64, f64))>,
    /// Buttons whose press bitscomp kept for itself; their release is kept too.
    pub swallowed: HashSet<u32>,
    /// The pointer shape bitscomp wants over its own frames (resize edges, gaps); `None` over a client.
    pub cursor_deco: Option<smithay::input::pointer::CursorIcon>,
    /// Keys whose press the compositor kept for itself; their release is kept too.
    pub intercepted: HashSet<Keycode>,
    pub cursor_named: Option<smithay::input::pointer::CursorIcon>,
    /// WY-1a, the pointer bitscomp draws itself (`cursor`): on when the back end has no host cursor.
    pub draw_pointer: bool,
    pub cursor_hidden: bool,
    pub cursor_surface: Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    /// WY-11: the software pointer's buffer (the image, the scale's bits, its padded size).
    pub pointer_img: Option<(std::rc::Rc<crate::cursor_theme::CursorImage>, u64, (i32, i32), MemoryRenderBuffer)>,
    /// WY-11: theme images, the hardware cursor and its counters (`hwcursor`).
    pub cursor: crate::hwcursor::Pointer,
    pub _cursor_shape: smithay::wayland::cursor_shape::CursorShapeManagerState,
    /// WY-7: programs started and not yet shown (launch feedback), and the "starting" mark at its pixel size.
    pub launches: crate::launch::Launches,
    pub mark_img: Option<(i32, MemoryRenderBuffer)>,
    /// The frame being drawn (CLOCK_MONOTONIC, `pacer`) and the output's period.
    pub frame_time: std::time::Duration,
    pub refresh: std::time::Duration,
    pub layout_dirty: bool,
    pub needs_frame: bool,
    pub quit: bool,
    pub stats: Stats,
    pub spawn_env: Vec<(String, String)>,
    /// Started programs with their start time (for the first-frame event's startup time). Each one leads its
    /// own process group, so the end of bitscomp reaches what it started in turn (end_children in main.rs).
    pub children: Vec<(std::process::Child, std::time::Instant)>,
    /// The argv each started program got (WY-5 session: restarted with exactly that, never with what
    /// /proc/PID/cmdline shows of another client).
    pub spawned: std::collections::HashMap<u32, Vec<String>>,
    pub ctl_queue: Vec<(String, std::os::unix::net::UnixStream)>,
    /// A size asked for over the control socket; the back end applies it.
    pub requested_size: Option<(i32, i32)>,
    /// B4 (WY-4): ping state per client with a window (`hung`), and programs being ended (`ending`).
    pub answer: HashMap<smithay::reexports::wayland_server::backend::ClientId, crate::hung::Tracker>,
    /// The xdg_wm_base each client's open ping went out on (`answer`).
    pub pinged: HashMap<smithay::reexports::wayland_server::backend::ClientId, smithay::wayland::shell::xdg::ShellClient>,
    pub endings: Vec<crate::ending::Ending>,
    /// The grey veil over hung windows and the box with "antwortet nicht" / "Beenden" (by hover and pixel size).
    pub veil: HashMap<ObjectId, SolidColorBuffer>,
    pub hung_img: Vec<((Option<Part>, u64, i32, i32, u64), MemoryRenderBuffer)>,
    /// "Warten" on a hung window's veil: window id -> until when the veil stays hidden.
    pub veil_wait: HashMap<u64, std::time::Duration>,
    pub veil_next: Option<std::time::Duration>,
    /// WY-5: workspaces, chrome state, the settings and what follows from them.
    pub desk: crate::desk::Desk,
    pub images: crate::chrome_render::Images,
    pub settings: crate::settings::Values,
    /// Every settings key with its German text (address suggestions).
    pub setting_keys: Vec<(String, String)>,
    /// WY-9: the settings page's rows, built from settingsd's schema (the built-in rows until it answers).
    pub page_model: crate::page::Model,
    /// settingsd answered (the values are its, not only the defaults).
    pub settings_live: bool,
    /// WY-5b: the settings pages (by the pid of their window's program) and the writer of their changes.
    pub pages: crate::page_app::Pages,
    pub settings_writer: Option<crate::settings_write::Writer>,
    /// WY-5b: foot's colours from the theme.
    pub foot: crate::foot::FootState,
    pub status: crate::status::Status,
    pub session: crate::session::Session,
    pub anim: crate::anim::Anim,
    /// Programs opened from the address bar or the start page: pid, the window whose stack gets the new tab
    /// (`None`: a new tile), argv.
    pub pending_open: Vec<(u32, Option<Window>, Vec<String>)>,
    pub restoring: Vec<crate::place::Restore>,
    pub anchors: crate::place::Anchors,
    pub next_group: u64,
    /// The Interactive class grant on V2 (`grant`); `None` where the kernel does not offer it.
    pub grant: Option<crate::grant::Grant>,
}

impl App {
    pub fn new(dh: DisplayHandle, socket_name: String, size: (i32, i32)) -> Self {
        // presentation_time's clock: CLOCK_MONOTONIC, the clock `pacer` reads for every frame time.
        let clock = Clock::<Monotonic>::new();
        let compositor_state = CompositorState::new::<App>(&dh);
        let xdg_shell_state = XdgShellState::new::<App>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<App>(&dh);
        let shm_state = ShmState::new::<App>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<App>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<App>(&dh);
        let viewporter_state = ViewporterState::new::<App>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<App>(&dh);
        let presentation_state = PresentationState::new::<App>(&dh, clock.id() as u32);
        // WY-11: wp_cursor_shape_v1, so browsers name their pointer shapes and bitscomp draws them from its theme.
        let cursor_shape_state = smithay::wayland::cursor_shape::CursorShapeManagerState::new::<App>(&dh);

        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        // German keyboard (V2-PLAN §8a.2).
        let xkb = XkbConfig { layout: "de", ..Default::default() };
        seat.add_keyboard(xkb, KEY_REPEAT.delay_ms, KEY_REPEAT.rate).expect("German xkb keymap (xkeyboard-config installed?)");
        seat.add_pointer();

        let output = Output::new(
            "BITS-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "BitsOS".into(),
                model: "bitscomp".into(),
            },
        );
        let _global = output.create_global::<App>(&dh);
        let mut app = Self {
            dh,
            socket_name,
            start: Instant::now(),
            theme: std::rc::Rc::new(Theme::default()),
            theme_gen: 0,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            viewporter_state,
            fractional_scale_state,
            presentation_state,
            popups: PopupManager::default(),
            seat,
            output: output.clone(),
            space: Space::default(),
            ws: Workspace::default(),
            deco: HashMap::new(),
            deco_img: HashMap::new(),
            menu_img: None,
            corner_img: HashMap::new(),
            preview_buf: SolidColorBuffer::default(),
            pointer: (0.0, 0.0).into(),
            grab: None,
            menu: None,
            hover: None,
            last_click: None,
            swallowed: HashSet::new(),
            cursor_deco: None,
            intercepted: HashSet::new(),
            cursor_named: None,
            draw_pointer: false,
            cursor_hidden: false,
            cursor_surface: None,
            pointer_img: None,
            cursor: crate::hwcursor::Pointer::new(crate::cursor_theme::THEME, Theme::default().cursor_size),
            _cursor_shape: cursor_shape_state,
            launches: Default::default(),
            mark_img: None,
            frame_time: std::time::Duration::ZERO,
            refresh: crate::pacer::DEFAULT_PERIOD,
            layout_dirty: true,
            needs_frame: true,
            quit: false,
            stats: Stats::default(),
            spawn_env: Vec::new(),
            children: Vec::new(),
            spawned: Default::default(),
            ctl_queue: Vec::new(),
            requested_size: None,
            answer: HashMap::new(),
            pinged: HashMap::new(),
            endings: Vec::new(),
            veil: HashMap::new(),
            hung_img: Vec::new(),
            veil_wait: HashMap::new(),
            veil_next: None,
            desk: Default::default(),
            images: Default::default(),
            settings: Default::default(),
            setting_keys: Vec::new(),
            page_model: crate::page_schema::model(&crate::page_schema::builtin()),
            settings_live: false,
            pages: Default::default(),
            settings_writer: None,
            foot: Default::default(),
            status: Default::default(),
            session: Default::default(),
            anim: Default::default(),
            pending_open: Vec::new(),
            restoring: Vec::new(),
            anchors: Default::default(),
            next_group: 0,
            grant: None,
        };
        app.space.map_output(&output, (0, 0));
        app.set_mode(size);
        app
    }

    /// The output's physical size follows the window (nested) or the framebuffer (fbdev); the scale stays.
    pub fn set_mode(&mut self, size: (i32, i32)) {
        let mode = Mode { size: size.into(), refresh: 60_000 };
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(self.theme.scale)),
            Some((0, 0).into()),
        );
        self.output.set_preferred(mode);
        self.layout_dirty = true;
        self.needs_frame = true;
    }

    pub fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn find_window(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<Window> {
        self.all_windows().into_iter().find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
    }

    /// Every window, also those of the parked workspaces.
    pub fn all_windows(&self) -> Vec<Window> {
        let mut v: Vec<Window> = self.ws.tiles.iter().flat_map(|t| t.stack.iter().cloned()).collect();
        v.extend(self.ws.floating.iter().map(|f| f.win.clone()));
        v.extend(self.parked_windows());
        v
    }
}
