//! The chrome's place in the frame (WY-5): one cached image per bar (`chrome_draw`), the start page, the
//! address suggestions and the notice; each is redrawn only when its look changes. The start animation only
//! changes the images' alpha and offset (`anim`). DS-2: the wallpaper under everything (`wallpaper`) and Claude's
//! ring as small sprites of their own, so a busy ring (10 steps a second at most) redraws only itself. DS-3: the
//! Bühne preset (`buehne` geometry, `scene` images) replaces the bars when theme.layout.preset is buehne. DS-5: the boot
//! scene (`boot`) lies over everything at the start until the desktop is ready.

pub mod boot;
mod boot_art;
mod boot_draw;
mod boot_model;
mod boot_ring;
#[cfg(test)]
mod boot_tests;
pub mod buehne;
pub mod scene;
pub mod wallpaper;

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use crate::chrome::{self, Chrome, Part};
use crate::chrome_draw::{self as draw, FavLook, ListLook, RingLook, STATUS_GLOW, SideLook, StatusLook, SuggestLook, TopLook};
use crate::deco::image_a;
use crate::desk::Spot;
use crate::fx::plan as fx;
use crate::header::px_rect;
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::start_draw::{self, StartLook};
use crate::state::App;

#[derive(Default)]
pub struct Images {
    top: Option<(TopLook, MemoryRenderBuffer)>,
    favs: Option<(FavLook, MemoryRenderBuffer)>,
    list: Option<(ListLook, MemoryRenderBuffer)>,
    side: Option<(SideLook, MemoryRenderBuffer)>,
    status: Option<(StatusLook, MemoryRenderBuffer)>,
    start: Option<(StartLook, MemoryRenderBuffer)>,
    start_blocks: Option<(StartLook, MemoryRenderBuffer)>,
    start_hover: Option<(StartLook, MemoryRenderBuffer)>,
    suggest: Option<(SuggestLook, MemoryRenderBuffer)>,
    notice: Option<((String, Rect, u64), MemoryRenderBuffer)>,
    top_ring: Option<(RingLook, MemoryRenderBuffer)>,
    side_ring: Option<(RingLook, MemoryRenderBuffer)>,
    /// Read by `wallpaper::elements` (render.rs draws it under every layer).
    pub(crate) wallpaper: Option<(wallpaper::Key, MemoryRenderBuffer)>,
    pub(crate) buehne: scene::Images,
    /// DS-5: the boot scene over the desktop at the start (`boot`).
    pub(crate) boot: boot::Boot,
}

/// The cached image for `look`, drawn again when the look differs.
fn cached<'a, L: PartialEq + Clone>(
    slot: &'a mut Option<(L, MemoryRenderBuffer)>,
    look: &L,
    draw: impl FnOnce(&L) -> crate::canvas::Canvas,
) -> &'a MemoryRenderBuffer {
    if slot.as_ref().is_none_or(|(l, _)| l != look) {
        *slot = Some((look.clone(), draw(look).into_buffer()));
    }
    &slot.as_ref().unwrap().1
}

impl App {
    /// The output in logical pixels.
    pub fn area_full(&self) -> Rect {
        let g = self.space.output_geometry(&self.output).unwrap_or_default();
        Rect { x: g.loc.x, y: g.loc.y, w: g.size.w, h: g.size.h }
    }

    pub fn chrome(&self) -> Chrome {
        if let Some(b) = self.buehne() {
            return b.chrome();
        }
        let inputs = chrome::Inputs {
            size: (self.area_full().w, self.area_full().h),
            favourites: self.desk.favourites.clone(),
            workspaces: self.ws_count(),
            minimized: self.ws.minimized.len(),
            reveal: self.desk.reveal.shown,
        };
        chrome::layout(&self.theme, &inputs)
    }

    /// Something the chrome shows changed: a frame (the looks decide what is redrawn).
    pub fn chrome_dirty(&mut self) {
        self.needs_frame = true;
        self.buehne_work_sync();
    }

    /// The field being edited: the top bar's, or the tile's own bar.
    pub fn edit_field(&self) -> Rect {
        match self.desk.edit.as_ref().and_then(|e| e.tile) {
            Some(t) => self
                .ws
                .tile_frames(self.area(), self.theme.gap)
                .get(t)
                .map(|f| crate::header::tile_address_rect(*f, &self.theme))
                .unwrap_or_else(|| self.chrome().address),
            None => self.chrome().address,
        }
    }

    pub fn start_page(&self) -> crate::start::Page {
        let blocks = start_draw::blocks(&self.status.blocks, self.status.cpus).len();
        crate::start::layout(self.chrome().work, self.desk.favourites.len(), self.session.recent.len().max(1), blocks)
    }

    fn start_look(&self, theme: u64) -> StartLook {
        let secs = if self.status.fixed { crate::status::FIXED_SECS } else { crate::status::epoch_secs() };
        let now = crate::status::tm_of(secs, self.status.fixed);
        let (date, greet) = crate::status::date_and_greeting(&now);
        let restore = self.session.saved.as_ref().filter(|_| self.session.offered).map(|s| {
            let (tiles, tabs) = s.counts();
            let name = s.spaces.get(s.active).map_or("", |sp| sp.name.as_str());
            let when = crate::status::when_text(&crate::status::tm_of(s.when, self.status.fixed), &now);
            format!("{name} · {} · {when}", crate::desk::summary(tiles, tabs).replace(" · ", ", "))
        });
        let favs = self.desk.favourites.iter().map(|a| (crate::suggest::short_name(a), a.clone())).collect();
        let recent = self
            .session
            .recent
            .iter()
            .map(|r| {
                let when = crate::status::when_text(&crate::status::tm_of(r.when, self.status.fixed), &now);
                (crate::session::kind(&r.addr).to_string(), crate::suggest::short_name(&r.addr), when)
            })
            .collect();
        let hover = match self.desk.hover {
            Some(Spot::Start(p)) => Some(p),
            _ => None,
        };
        StartLook {
            work: self.chrome().work,
            page: self.start_page(),
            theme,
            date,
            greet: greet.to_string(),
            restore,
            favs,
            recent,
            blocks: start_draw::blocks(&self.status.blocks, self.status.cpus),
            hover,
        }
    }
}

/// Places a cached chrome image at `r` moved by `dy`, faded by `alpha`.
fn put<R: CompRenderer>(renderer: &mut R, buf: &MemoryRenderBuffer, r: Rect, dy: f32, s: f64, alpha: f32, out: &mut Vec<CompElement<R>>) {
    let moved = Rect { y: r.y + dy.round() as i32, ..r };
    let p = px_rect(r, s);
    out.extend(image_a(renderer, buf, moved, (p.w, p.h), s, alpha));
}

/// The notice and the address suggestions, topmost first: they lie over everything, floating windows too (WY-5c).
pub fn over<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let t = app.theme.clone();
    let s = t.scale;
    let now = crate::pacer::now();
    app.anim.first_frame(now);
    let (alpha, _) = app.anim.chrome(now, &t);
    let tg = app.theme_gen;
    let ch = app.chrome();
    let mut out = Vec::new();
    let put = |renderer: &mut R, buf: &MemoryRenderBuffer, r: Rect, dy: f32, out: &mut Vec<CompElement<R>>| {
        put(renderer, buf, r, dy, s, alpha, out)
    };
    if let Some((text, _)) = app.desk.notice.clone() {
        let a = ch.address;
        let f = draw::font(&t, 13.0, crate::text::Weight::Regular);
        let w = (chrome::logical_width(&f, &text, s) + 24).min(a.w);
        let r = Rect { x: a.x, y: a.y + a.h + 8, w, h: 30 };
        let buf = cached(&mut app.images.notice, &(text.clone(), r, tg), |(tx, r, _)| draw::notice(tx, *r, &t));
        put(renderer, buf, r, 0.0, &mut out);
    }
    if let Some(e) = app.desk.edit.clone() {
        let items = app.suggestions();
        if !items.is_empty() {
            let (frame, rows) = crate::suggest::layout(&items, app.edit_field(), app.area_full().h - 8);
            let hv = match app.desk.hover {
                Some(Spot::Suggest(i)) => Some(i),
                _ => None,
            };
            let look = SuggestLook { frame, rows, items, pick: e.pick, hover: hv, theme: tg };
            let buf = cached(&mut app.images.suggest, &look, |l| draw::suggest(l, &t));
            put(renderer, buf, frame, 0.0, &mut out);
        }
    }
    out
}

/// The bars and the start page, topmost first. They lie below floating windows (WY-5c) and above the tiles; the
/// tiles never reach under a docked bar, a revealed one lies over them.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    if let Some(b) = app.buehne() {
        // The classic bars' images are not drawn: let them go (a switch back draws them again).
        let im = &mut app.images;
        (im.top, im.favs, im.list, im.side, im.status, im.side_ring) = (None, None, None, None, None, None);
        return scene::elements(app, renderer, &b);
    }
    let t = app.theme.clone();
    let s = t.scale;
    let now = crate::pacer::now();
    let (alpha, glide) = app.anim.chrome(now, &t);
    let tg = app.theme_gen;
    let ch = app.chrome();
    let hover = match app.desk.hover {
        Some(Spot::Chrome(p)) => Some(p),
        _ => None,
    };
    let mut out = Vec::new();
    let put = |renderer: &mut R, buf: &MemoryRenderBuffer, r: Rect, dy: f32, out: &mut Vec<CompElement<R>>| {
        put(renderer, buf, r, dy, s, alpha, out)
    };
    let edit_top = app.desk.edit.as_ref().filter(|e| e.tile.is_none());
    let look = TopLook {
        chrome: ch.clone(),
        theme: tg,
        text: match edit_top {
            Some(e) => e.text.clone(),
            None if t.layout.address_top => app.shown_address().unwrap_or_default(),
            None => String::new(),
        },
        cursor: edit_top.map(|e| e.before().to_string()),
        hover,
        context: None,
        library: app.lib.open,
    };
    // The busy ring's step: the same for both rings; asks for the next step's frame only while it moves.
    let ring = app.desk.ring;
    let step = app.status.ring.shown(crate::ring::sprite_moves(ring, &t), now, &t);
    let rl = RingLook { rect: ch.ring, ring, step, size: draw::TOP_RING, theme: tg };
    out.extend(fx::ring(app, "ring-top", ch.ring, draw::TOP_RING, ring, -glide).map(CompElement::from));
    let rbuf = cached(&mut app.images.top_ring, &rl, |l| draw::ring_sprite(l, &t));
    put(renderer, rbuf, ch.ring, -glide, &mut out);
    let buf = cached(&mut app.images.top, &look, |l| draw::top(l, &t));
    put(renderer, buf, ch.top, -glide, &mut out);
    out.extend(fx::bar(app, "bar-top", ch.top).map(CompElement::from));
    if let Some(bar) = ch.favs {
        let fh = match hover {
            Some(Part::Fav(i)) => Some(i),
            _ => None,
        };
        let look = FavLook { chrome: ch.clone(), theme: tg, items: app.desk.favourites.clone(), hover: fh };
        let buf = cached(&mut app.images.favs, &look, |l| draw::favs(l, &t));
        put(renderer, buf, bar, -glide, &mut out);
    }
    if let Some(r) = ch.status {
        let look = StatusLook { rect: r, theme: tg, blocks: app.status.blocks.clone(), clock: app.status.clock.clone(), grant: app.install_marker() };
        let buf = cached(&mut app.images.status, &look, |l| draw::status(l, &t));
        put(renderer, buf, Rect { y: r.y - STATUS_GLOW, h: r.h + STATUS_GLOW, ..r }, glide, &mut out);
    }
    // A revealed bar lies over the tiles: it comes before (above) the start page and the tiles.
    if let Some(r) = ch.side {
        let rr = draw::side_ring_rect(r);
        out.extend(fx::ring(app, "ring-side", rr, draw::SIDE_RING, ring, 0.0).map(CompElement::from));
        let rl = RingLook { rect: rr, ring, step, size: draw::SIDE_RING, theme: tg };
        let rbuf = cached(&mut app.images.side_ring, &rl, |l| draw::ring_sprite(l, &t));
        put(renderer, rbuf, rr, 0.0, &mut out);
        let look = SideLook { rect: r, theme: tg, name: app.desk.claude_name.clone(), ring, solid: !ch.side_docked };
        let buf = cached(&mut app.images.side, &look, |l| draw::side(l, &t));
        put(renderer, buf, r, 0.0, &mut out);
        out.extend(fx::bar(app, "bar-side", r).map(CompElement::from));
    }
    if !ch.list_shown {
        app.images.list = None;
    }
    let entries = (0..app.ws_count())
        .map(|i| {
            let (tiles, tabs) = app.ws_counts(i);
            (app.desk.names[i].clone(), crate::desk::summary(tiles, tabs))
        })
        .collect();
    if ch.list_shown {
        let minimized = app.ws.minimized.iter().map(|m| crate::minimize::label(&m.win)).collect();
        let look = ListLook { chrome: ch.clone(), theme: tg, entries, minimized, active: app.desk.active, hover };
        let buf = cached(&mut app.images.list, &look, |l| draw::list(l, &t));
        put(renderer, buf, ch.list, 0.0, &mut out);
        out.extend(fx::bar(app, "bar-list", ch.list).map(CompElement::from));
    }
    if app.start_shown() {
        start_page(app, renderer, ch.work, alpha, &mut out);
    }
    out
}

/// The start page's images, topmost first: the part under the pointer, the blocks, the page.
fn start_page<R: CompRenderer>(app: &mut App, renderer: &mut R, work: Rect, alpha: f32, out: &mut Vec<CompElement<R>>) {
    let t = app.theme.clone();
    let look = app.start_look(app.theme_gen);
    if let Some(r) = start_draw::hover_rect(&look) {
        let buf = cached(&mut app.images.start_hover, &start_draw::hover_look(&look), |l| start_draw::draw_hover(l, &t, r));
        put(renderer, buf, r, 0.0, t.scale, alpha, out);
    }
    if let Some(r) = start_draw::blocks_rect(&look) {
        let buf = cached(&mut app.images.start_blocks, &start_draw::blocks_look(&look), |l| start_draw::draw_blocks(l, &t, r));
        put(renderer, buf, r, 0.0, t.scale, alpha, out);
    }
    let buf = cached(&mut app.images.start, &start_draw::base_look(&look), |l| start_draw::draw(l, &t));
    put(renderer, buf, work, 0.0, t.scale, alpha, out);
}
