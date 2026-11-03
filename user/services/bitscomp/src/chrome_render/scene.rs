//! DS-3: the Bühne preset in the frame. `App::buehne` is the layout (None with the classic preset), `elements` the
//! cached images above the tiles (header, headline, right column, strip, bottom row, Claude's ring, the start page
//! of an empty workspace), `stage_element` the stage under the tiles (pushed with the wallpaper by render.rs).
//! `buehne_spot` / `buehne_act` serve the parts WY's `chrome::Part` has no name for (spots.rs asks them first).

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use super::buehne::{self, Buehne, Part};
use super::{cached, put};
use crate::chrome_draw::buehne::{self as bd, ColumnLook, HeaderLook, HeadlineLook, Hover};
use crate::chrome_draw::strip::{self, BottomLook, StripLook};
use crate::chrome_draw::{self as draw, RingLook};
use crate::desk::Spot;
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::theme::Preset;

/// The Bühne's cached images.
#[derive(Default)]
pub struct Images {
    header: Option<(HeaderLook, MemoryRenderBuffer)>,
    headline: Option<(HeadlineLook, MemoryRenderBuffer)>,
    column: Option<(ColumnLook, MemoryRenderBuffer)>,
    strip: Option<(StripLook, MemoryRenderBuffer)>,
    bottom: Option<(BottomLook, MemoryRenderBuffer)>,
    pub(crate) stage: Option<((Rect, u64, (i32, i32)), MemoryRenderBuffer)>,
    /// The tiles' area of the last frame: when it moves (a headline typed, the column folded) the tiles are laid
    /// out again.
    work: Option<Rect>,
}

impl App {
    /// The Bühne's layout, or None when theme.layout.preset is classic.
    pub fn buehne(&self) -> Option<Buehne> {
        if self.theme.layout.preset != Preset::Buehne {
            return None;
        }
        let full = self.area_full();
        let inputs = buehne::Inputs {
            size: (full.w, full.h),
            names: self.desk.names.clone(),
            active: self.desk.active,
            min_labels: self.ws.minimized.iter().map(|m| crate::minimize::label(&m.win)).collect(),
            absicht: self.absicht().is_some(),
        };
        Some(buehne::layout(&self.theme, &inputs))
    }

    /// The shown workspace's Absicht headline (`desktop.absicht.<slug>`), if the user typed one.
    pub fn absicht(&self) -> Option<String> {
        let name = self.desk.names.get(self.desk.active)?;
        let s = self.settings.text(&buehne::absicht_key(name, &self.desk.names))?.trim();
        (!s.is_empty()).then(|| s.chars().take(200).collect())
    }

    /// The tiles' area moved (a headline typed or removed, the column folded, the preset switched): the tiles are laid
    /// out again before the next frame (`chrome_dirty` asks this, so a settings change never draws one frame of tiles
    /// against the old area).
    pub(crate) fn buehne_work_sync(&mut self) {
        let work = self.buehne().map(|b| b.work);
        if self.images.buehne.work != work {
            self.images.buehne.work = work;
            self.layout_dirty = true;
        }
    }

    /// Programs that hang with a window on the shown workspace (tiles, floating, minimised; each program once).
    fn hung_here(&self) -> usize {
        let mut ids = Vec::new();
        for w in self.ws.all_windows() {
            if let Some(c) = self.client_of(w).filter(|_| self.is_hung(w)) {
                let id = c.id();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids.len()
    }

    /// The Bühne part under (x, y) (spots.rs asks this before the chrome).
    pub fn buehne_spot(&self, x: f64, y: f64) -> Option<Part> {
        self.buehne()?.hit(x, y)
    }

    /// A click on a Bühne part: the sun switches theme.mode (kept in settingsd like a fold, local without it);
    /// the bell, the Feld bar, the strip and "+N" have nothing to open yet (later items), so they only say so.
    pub fn buehne_act(&mut self, p: Part) {
        match p {
            Part::Sun => {
                let mode = if self.theme.dark { "light" } else { "dark" };
                self.settings.set("theme.mode", mode);
                self.apply_settings("theme.mode");
                let req = crate::settings_write::Req::set("theme.mode", mcpreg::Json::Str(mode.into()));
                let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(crate::desk::DESK_PID, req));
                eprintln!("bitscomp: theme.mode = {mode} by the sun ({})", if sent { "kept in settingsd" } else { "local only" });
            }
            other => eprintln!("bitscomp: {other:?} has nothing to open yet"),
        }
    }

    /// CR DS-3-4: registers the Absicht key of every workspace settingsd does not know yet (owner bitscomp), so
    /// the headline can be typed on the settings page and over MCP. Called when settingsd lists its keys (also
    /// after a registration: settingsd announces it and the list comes again), after a new workspace and after a
    /// session restore. A key is sent once per settingsd connection; a refused one stays local (`ctl set`).
    pub fn absicht_register(&mut self) {
        if !self.settings_live {
            return;
        }
        let Some(w) = self.settings_writer.as_ref() else { return };
        let names = self.desk.names.clone();
        let keys: Vec<(String, String)> = names
            .iter()
            .map(|n| (super::buehne::absicht_key(n, &names), n.clone()))
            .filter(|(k, _)| !self.setting_keys.iter().any(|(s, _)| s == k) && !self.absicht_sent.contains(k))
            .collect();
        if keys.is_empty() {
            return;
        }
        let list: Vec<&str> = keys.iter().map(|(k, _)| k.as_str()).collect();
        let sent = w.send(crate::desk::DESK_PID, crate::settings_write::Req::Register { keys: keys.clone() });
        eprintln!("bitscomp: absicht keys {} ({})", list.join(","), if sent { "registering in settingsd" } else { "writer gone" });
        if sent {
            self.absicht_sent.extend(keys.into_iter().map(|(k, _)| k));
        }
    }

    fn buehne_hover(&self) -> Option<Hover> {
        match self.desk.hover {
            Some(Spot::Chrome(p)) => Some(Hover::Chrome(p)),
            Some(Spot::Buehne(p)) => Some(Hover::Buehne(p)),
            _ => None,
        }
    }

    fn slug(&self) -> String {
        self.desk.names.get(self.desk.active).map_or_else(|| "raum".into(), |n| buehne::slug(n))
    }
}

/// The Bühne's images, topmost first, all outside the tiles' area (they never cover a tile).
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R, b: &Buehne) -> Vec<CompElement<R>> {
    let t = app.theme.clone();
    let s = t.scale;
    let now = crate::pacer::now();
    let (alpha, glide) = app.anim.chrome(now, &t);
    let tg = app.theme_gen;
    // Only a safety net: `chrome_dirty` already caught the change before this frame's layout (`buehne_work_sync`).
    if app.images.buehne.work != Some(b.work) {
        app.images.buehne.work = Some(b.work);
        app.layout_dirty = true;
    }
    let hover = app.buehne_hover();
    let slug = app.slug();
    let mut out = Vec::new();
    let put = |renderer: &mut R, buf: &MemoryRenderBuffer, r: Rect, dy: f32, out: &mut Vec<CompElement<R>>| {
        put(renderer, buf, r, dy, s, alpha, out)
    };
    // Claude's ring in the header: its own sprite over the header image.
    let ring = app.desk.ring;
    let step = app.status.ring.shown(crate::ring::moves(ring), now, &t);
    let size = t.bl(40.0) as f32 * 48.0 / 44.0 * 0.9;
    let rl = RingLook { rect: b.ring, ring, step, size, theme: tg };
    let rbuf = cached(&mut app.images.top_ring, &rl, |l| draw::ring_sprite(l, &t));
    put(renderer, rbuf, b.ring, -glide, &mut out);
    let edit = app.desk.edit.as_ref().filter(|e| e.tile.is_none()).map(|e| (e.text.clone(), e.before().to_string()));
    let look = HeaderLook {
        geo: b.clone(),
        theme: tg,
        chips: app.desk.names.iter().map(|n| (n.clone(), buehne::colour_of(n))).collect(),
        active: app.desk.active,
        slug: slug.clone(),
        edit,
        blocks: app.status.blocks.clone(),
        clock: app.status.clock.clone(),
        hover,
        notices: 0,
    };
    let buf = cached(&mut app.images.buehne.header, &look, |l| bd::header(l, &t));
    put(renderer, buf, b.header, -glide, &mut out);
    if let (Some(r), Some(text)) = (b.headline, app.absicht()) {
        let look = HeadlineLook { rect: r, theme: tg, slug: slug.clone(), text };
        let buf = cached(&mut app.images.buehne.headline, &look, |l| bd::headline(l, &t));
        put(renderer, buf, r, 0.0, &mut out);
    }
    if let Some((map, next)) = b.column {
        let look = ColumnLook { map, next, theme: tg, workspaces: app.ws_count() };
        let area = bd::padded(Rect { h: next.y + next.h - map.y, ..map }, &t, b.stage.x + b.stage.w);
        let buf = cached(&mut app.images.buehne.column, &look, |l| bd::column(l, &t, area));
        put(renderer, buf, area, 0.0, &mut out);
    }
    if let Some(r) = b.strip {
        let look = StripLook {
            rect: r,
            theme: tg,
            lanes: app.desk.names.iter().map(|n| (n.clone(), buehne::colour_of(n))).collect(),
            active: app.desk.active,
            now: strip::hour_minute(&app.status.clock),
        };
        // The strip's image stops short of the stage (it lies over the stage's lower margin, not its tiles).
        let area = Rect { y: r.y - t.bl(20.0), h: r.h + t.bl(44.0), ..bd::padded(r, &t, 0) };
        let buf = cached(&mut app.images.buehne.strip, &look, |l| strip::strip(l, &t, area));
        put(renderer, buf, area, glide, &mut out);
    }
    if let Some(row) = b.bottom {
        let (tiles, _) = app.ws_counts(app.desk.active);
        let hung = app.hung_here();
        let look = BottomLook {
            row,
            hint: b.hint,
            feld: b.feld,
            home: b.home,
            min_items: b.min_items.clone(),
            theme: tg,
            text: buehne::hint_text(tiles, hung),
            grant: app.install_marker(),
            minimized: app.ws.minimized.iter().map(|m| crate::minimize::label(&m.win)).collect(),
            home_lit: app.start_shown(),
            hover,
        };
        let full = app.area_full();
        let top = row.y - t.bl(16.0);
        let area = Rect { x: 0, y: top, w: full.w, h: full.h - top };
        let buf = cached(&mut app.images.buehne.bottom, &look, |l| strip::bottom(l, &t, area));
        put(renderer, buf, area, glide, &mut out);
    }
    if app.start_shown() {
        let look = app.start_look(tg);
        let buf = cached(&mut app.images.start, &look, |l| crate::start_draw::draw(l, &t));
        put(renderer, buf, b.work, 0.0, &mut out);
    }
    out
}

/// The stage as an element under the tiles (`wallpaper::elements` puts it above the wallpaper).
pub fn stage_element<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Option<CompElement<R>> {
    let b = app.buehne()?;
    let t = app.theme.clone();
    let full = app.area_full();
    let m = t.bl(72.0);
    let st = b.stage;
    let x0 = (st.x - m).max(full.x);
    let y0 = (st.y - m).max(full.y);
    let area = Rect { x: x0, y: y0, w: (st.x + st.w + m).min(full.x + full.w) - x0, h: (st.y + st.h + m).min(full.y + full.h) - y0 };
    let key = (st, app.theme_gen, (full.w, full.h));
    let buf = cached(&mut app.images.buehne.stage, &key, |(r, _, _)| strip::stage(*r, &t, area));
    let p = crate::header::px_rect(area, t.scale);
    crate::deco::image(renderer, buf, area, (p.w, p.h), t.scale)
}
