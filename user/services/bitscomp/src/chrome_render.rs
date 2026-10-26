//! The chrome's place in the frame (WY-5): one cached image per bar (`chrome_draw`), the start page, the
//! address suggestions and the notice; each is redrawn only when its look changes. The start animation only
//! changes the images' alpha and offset (`anim`).

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::pixman::PixmanRenderer;

use crate::chrome::{self, Chrome, Part};
use crate::chrome_draw::{self as draw, FavLook, ListLook, STATUS_GLOW, SideLook, StatusLook, SuggestLook, TopLook};
use crate::deco::image_a;
use crate::desk::Spot;
use crate::header::px_rect;
use crate::layout::Rect;
use crate::render::CompElement;
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
    suggest: Option<(SuggestLook, MemoryRenderBuffer)>,
    notice: Option<((String, Rect, u64), MemoryRenderBuffer)>,
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
        let inputs = chrome::Inputs {
            size: (self.area_full().w, self.area_full().h),
            favourites: self.desk.favourites.clone(),
            workspaces: self.ws_count(),
            reveal: self.desk.reveal.shown,
        };
        chrome::layout(&self.theme, &inputs)
    }

    /// Something the chrome shows changed: a frame (the looks decide what is redrawn).
    pub fn chrome_dirty(&mut self) {
        self.needs_frame = true;
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
fn put(pixman: &mut PixmanRenderer, buf: &MemoryRenderBuffer, r: Rect, dy: f32, s: f64, alpha: f32, out: &mut Vec<CompElement>) {
    let moved = Rect { y: r.y + dy.round() as i32, ..r };
    let p = px_rect(r, s);
    out.extend(image_a(pixman, buf, moved, (p.w, p.h), s, alpha));
}

/// The notice and the address suggestions, topmost first: they lie over everything, floating windows too (WY-5c).
pub fn over(app: &mut App, pixman: &mut PixmanRenderer) -> Vec<CompElement> {
    let t = app.theme.clone();
    let s = t.scale;
    let now = crate::pacer::now();
    app.anim.first_frame(now);
    let (alpha, _) = app.anim.chrome(now, &t);
    let tg = app.theme_gen;
    let ch = app.chrome();
    let mut out = Vec::new();
    let put = |pixman: &mut PixmanRenderer, buf: &MemoryRenderBuffer, r: Rect, dy: f32, out: &mut Vec<CompElement>| {
        put(pixman, buf, r, dy, s, alpha, out)
    };
    if let Some((text, _)) = app.desk.notice.clone() {
        let a = ch.address;
        let f = draw::font(&t, 13.0, crate::text::Weight::Regular);
        let w = (chrome::logical_width(&f, &text, s) + 24).min(a.w);
        let r = Rect { x: a.x, y: a.y + a.h + 8, w, h: 30 };
        let buf = cached(&mut app.images.notice, &(text.clone(), r, tg), |(tx, r, _)| draw::notice(tx, *r, &t));
        put(pixman, buf, r, 0.0, &mut out);
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
            put(pixman, buf, frame, 0.0, &mut out);
        }
    }
    out
}

/// The bars and the start page, topmost first. They lie below floating windows (WY-5c) and above the tiles; the
/// tiles never reach under a docked bar, a revealed one lies over them.
pub fn elements(app: &mut App, pixman: &mut PixmanRenderer) -> Vec<CompElement> {
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
    let put = |pixman: &mut PixmanRenderer, buf: &MemoryRenderBuffer, r: Rect, dy: f32, out: &mut Vec<CompElement>| {
        put(pixman, buf, r, dy, s, alpha, out)
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
        ring: app.desk.ring,
        context: None,
    };
    let buf = cached(&mut app.images.top, &look, |l| draw::top(l, &t));
    put(pixman, buf, ch.top, -glide, &mut out);
    if let Some(bar) = ch.favs {
        let fh = match hover {
            Some(Part::Fav(i)) => Some(i),
            _ => None,
        };
        let look = FavLook { chrome: ch.clone(), theme: tg, items: app.desk.favourites.clone(), hover: fh };
        let buf = cached(&mut app.images.favs, &look, |l| draw::favs(l, &t));
        put(pixman, buf, bar, -glide, &mut out);
    }
    if let Some(r) = ch.status {
        let look = StatusLook { rect: r, theme: tg, blocks: app.status.blocks.clone(), clock: app.status.clock.clone() };
        let buf = cached(&mut app.images.status, &look, |l| draw::status(l, &t));
        put(pixman, buf, Rect { y: r.y - STATUS_GLOW, h: r.h + STATUS_GLOW, ..r }, glide, &mut out);
    }
    // A revealed bar lies over the tiles: it comes before (above) the start page and the tiles.
    if let Some(r) = ch.side {
        let look = SideLook { rect: r, theme: tg, name: app.desk.claude_name.clone(), ring: app.desk.ring };
        let buf = cached(&mut app.images.side, &look, |l| draw::side(l, &t));
        put(pixman, buf, r, 0.0, &mut out);
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
        let look = ListLook { chrome: ch.clone(), theme: tg, entries, active: app.desk.active, hover };
        let buf = cached(&mut app.images.list, &look, |l| draw::list(l, &t));
        put(pixman, buf, ch.list, 0.0, &mut out);
    }
    if app.start_shown() {
        let look = app.start_look(tg);
        let buf = cached(&mut app.images.start, &look, |l| start_draw::draw(l, &t));
        put(pixman, buf, ch.work, 0.0, &mut out);
    }
    out
}
