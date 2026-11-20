//! GM-6a: the sofa layer's elements (DS rule "new look files"; plan/spec/sofa-mode.md §1, §2): paint layer `Sofa`,
//! above the workspaces, below the System layer. On a sofa screen it is opaque and alone: the cached dot-grid
//! ground (bg #121a16, dots every 24 board px, DS-2's grid without the glows), the header and the footer
//! (sofa_header_draw.rs); the bodies of Start, Bibliothek and Medien (GM-6b: sofa_start_draw.rs,
//! sofa_library_draw.rs), Einstellungen (GM-6c) and the search with its keyboard (GM-6d: sofa_search_draw.rs, the whole screen). Over a program in front it is the quick menu's
//! frame: the screen dimmed (bg at 50 %, the program's last frame stays visible) and the panel at the right. Each
//! image is drawn again only when its look changes, so an idle sofa screen costs no drawing (the clock's minute
//! changes the header alone). Always the dark tokens; no blur, no animation.

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::utils::{Physical, Point, Scale};

use crate::canvas::Canvas;
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::sofa::{Frame, Show};
use crate::sofa_header_draw::{self as hd, FooterLook, HeaderLook};
use crate::sofa_layout::{self as lay, Text};
use crate::state::App;
use crate::text::{Font, Typeface, Weight};
use crate::theme::{DARK, Rgba, alpha, hex};

/// The dark tokens the sofa mode draws with (theme/tokens.rs `DARK`, also when the desktop is light).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ink {
    pub bg: Rgba,
    pub panel: Rgba,
    pub line: Rgba,
    pub lifted_ring: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub line_strong: Rgba,
    pub accent: Rgba,
    pub accent_soft: Rgba,
    pub on_accent: Rgba,
}

impl Ink {
    pub fn dark() -> Ink {
        let h = |s: &str| hex(s).unwrap_or([0.0, 0.0, 0.0, 1.0]);
        Ink {
            bg: h(DARK.bg),
            panel: h(DARK.panel),
            line: h(DARK.line),
            lifted_ring: h(DARK.lifted_ring),
            text: h(DARK.text),
            muted: h(DARK.muted),
            line_strong: h(DARK.line_strong),
            accent: h(DARK.accent),
            accent_soft: h(DARK.accent_soft),
            on_accent: h(DARK.on_accent),
        }
    }
}

/// A font at board px `px` times k (theme.font.size does not apply: the sofa is a TV layout).
pub fn font(tf: Typeface, px: f32, w: Weight, k: f32) -> Font {
    Font::of(tf, px, w, k as f64)
}

/// A text's width in board px (sofa_layout's measure). Bricolage 700 is not in the font folder yet (GM-6f): 600.
pub fn measure(s: &str, t: Text) -> i32 {
    let (tf, px, w) = match t {
        Text::Word => (Typeface::Bricolage, 28.0, Weight::SemiBold),
        Text::Pill => (Typeface::Figtree, 20.0, Weight::SemiBold),
        Text::Status => (Typeface::JetBrainsMono, 16.0, Weight::Regular),
        Text::Clock => (Typeface::JetBrainsMono, 24.0, Weight::Medium),
        Text::Menu => (Typeface::Figtree, 18.0, Weight::SemiBold),
        Text::Hint => (Typeface::Figtree, 17.0, Weight::Regular),
        Text::Sign => (Typeface::Figtree, 14.0, Weight::Bold),
        Text::Section => (Typeface::Bricolage, 34.0, Weight::SemiBold),
        Text::Button => (Typeface::Figtree, 20.0, Weight::SemiBold),
        Text::HeroButton => (Typeface::Figtree, 22.0, Weight::Bold),
        Text::Chip => (Typeface::Figtree, 17.0, Weight::SemiBold),
    };
    Font::of(tf, px, w, 1.0).width(s)
}

#[derive(Clone, Debug, PartialEq)]
struct GroundLook {
    out: (i32, i32),
    frame: Frame,
}

#[derive(Default)]
pub struct Images {
    ground: Option<(GroundLook, MemoryRenderBuffer)>,
    header: Option<(HeaderLook, MemoryRenderBuffer)>,
    footer: Option<(FooterLook, MemoryRenderBuffer)>,
    quick: Option<(crate::sofa_quick_draw::QuickLook, MemoryRenderBuffer)>,
    dim: Option<SolidColorBuffer>,
    /// GM-6b: the shown screen's body (Start, Bibliothek, Medien).
    body: Option<(BodyLook, MemoryRenderBuffer)>,
}

/// What a screen's body shows (sofa_start_draw.rs, sofa_library_draw.rs).
#[derive(Clone, Debug, PartialEq)]
pub enum BodyLook {
    Start(crate::sofa_start_draw::StartLook),
    Library(crate::sofa_library_draw::LibraryLook),
    /// GM-6c: Einstellungen.
    Settings(crate::sofa_settings_draw::SettingsLook),
    /// GM-6d: the search and its keyboard (the whole screen: no header, no footer).
    Search(crate::sofa_search_draw::SearchLook),
}

fn draw_body(l: &BodyLook, o: Rect, ink: &Ink) -> Canvas {
    match l {
        BodyLook::Start(s) => crate::sofa_start_draw::draw_start(s, o, ink),
        BodyLook::Library(b) => crate::sofa_library_draw::draw_library(b, o, ink),
        BodyLook::Settings(b) => crate::sofa_settings_draw::draw_settings(b, o, ink),
        BodyLook::Search(b) => crate::sofa_search_draw::draw_search(b, o, ink),
    }
}

/// The ground: bg with the dot grid (#2b3731, 1 px, 1.3 px soft edge, every 24 board px from the frame's corner).
fn draw_ground(l: &GroundLook, ink: &Ink) -> Canvas {
    let (w, h) = l.out;
    let mut c = Canvas::new(w, h);
    c.clear(ink.bg);
    let k = l.frame.k as f32;
    let p = 24.0 * k;
    let (x0, y0) = (l.frame.x as f32 + 12.0 * k, l.frame.y as f32 + 12.0 * k);
    let (sx, sy) = (x0.rem_euclid(p), y0.rem_euclid(p));
    let mut y = sy;
    while y < h as f32 {
        let mut x = sx;
        while x < w as f32 {
            c.disc(x, y, 1.15 * k, ink.line);
            x += p;
        }
        y += p;
    }
    c
}

/// The logical rect covering physical `r` at output scale `s`, and the physical rect it really covers.
fn place(r: Rect, s: f64) -> (Rect, Rect) {
    let x0 = (r.x as f64 / s).floor() as i32;
    let y0 = (r.y as f64 / s).floor() as i32;
    let x1 = ((r.x + r.w) as f64 / s).ceil() as i32;
    let y1 = ((r.y + r.h) as f64 / s).ceil() as i32;
    let l = Rect { x: x0, y: y0, w: (x1 - x0).max(1), h: (y1 - y0).max(1) };
    (l, crate::header::px_rect(l, s))
}

fn solid<R: CompRenderer>(buf: &mut SolidColorBuffer, r: Rect, s: f64, c: Rgba) -> CompElement<R> {
    buf.update((r.w, r.h), c);
    let p = crate::header::px_rect(r, s);
    let loc: Point<i32, Physical> = Point::from((p.x, p.y));
    SolidColorRenderElement::from_buffer(buf, loc, Scale::from(s), 1.0, Kind::Unspecified).into()
}

/// The sofa layer's elements, topmost first; none while the mode is off or a program is in front.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let mut out = Vec::new();
    let show = app.sofa.show();
    if show == Show::None {
        app.sofa.images = Images::default();
        return out;
    }
    let s = app.theme.scale;
    let ink = Ink::dark();
    let f = app.sofa_frame();
    let (bw, bh) = f.board();
    let outp = crate::chrome_render::wallpaper::output_px(app);
    let (_, clock) = app.sofa_status();
    let mut push = |out: &mut Vec<CompElement<R>>, buf: &MemoryRenderBuffer, l: Rect, p: Rect| {
        out.extend(crate::deco::image(renderer, buf, l, (p.w, p.h), s));
    };
    if app.sofa.nav.quick {
        let panel = f.r(Rect { x: bw - lay::QUICK_W, y: 0, w: lay::QUICK_W, h: bh });
        let panel = Rect { w: outp.0 - panel.x, y: 0, h: outp.1, ..panel };
        let (l, p) = place(panel, s);
        let look = app.sofa_quick_look(&clock);
        let im = &mut app.sofa.images;
        if im.quick.as_ref().is_none_or(|(q, _)| *q != look) {
            let c = crate::sofa_quick_draw::draw(&look, p, &ink);
            im.quick = Some((look, c.into_buffer()));
        }
        push(&mut out, &app.sofa.images.quick.as_ref().unwrap().1, l, p);
        let dim = app.sofa.images.dim.get_or_insert_with(|| SolidColorBuffer::new((1, 1), ink.bg));
        let all = place(Rect { x: 0, y: 0, w: outp.0, h: outp.1 }, s).0;
        out.push(solid(dim, all, s, alpha(ink.bg, 0.5)));
    } else {
        app.sofa.images.quick = None;
    }
    if show == Show::Over {
        return out;
    }
    if let Some(header) = app.sofa_header() {
        let hr = f.r(Rect { x: 0, y: 0, w: bw, h: lay::HEADER_H });
        let (l, p) = place(Rect { x: 0, y: 0, w: outp.0, h: hr.y + hr.h }, s);
        let look = HeaderLook { frame: f, bw, header, clock: clock.clone(), active: app.sofa.nav.screen.pill(), focus: app.sofa.focus.clone() };
        let im = &mut app.sofa.images;
        if im.header.as_ref().is_none_or(|(h, _)| *h != look) {
            im.header = Some((look.clone(), hd::draw_header(&look, p, &ink).into_buffer()));
        }
        push(&mut out, &app.sofa.images.header.as_ref().unwrap().1, l, p);
        let fr = f.r(Rect { x: 0, y: bh - lay::FOOTER_H, w: bw, h: lay::FOOTER_H });
        let (l, p) = place(Rect { x: 0, y: fr.y, w: outp.0, h: outp.1 - fr.y }, s);
        let look = FooterLook { frame: f, bw, bh, hints: app.sofa_hints(), hover: app.sofa.hover.clone() };
        let im = &mut app.sofa.images;
        if im.footer.as_ref().is_none_or(|(h, _)| *h != look) {
            im.footer = Some((look.clone(), hd::draw_footer(&look, p, &ink).into_buffer()));
        }
        push(&mut out, &app.sofa.images.footer.as_ref().unwrap().1, l, p);
    }
    match app.sofa_body_look() {
        Some(look) => {
            let br = if matches!(look, BodyLook::Search(_)) {
                f.r(Rect { x: 0, y: 0, w: bw, h: bh })
            } else {
                f.r(Rect { x: 0, y: lay::HEADER_H, w: bw, h: bh - lay::HEADER_H - lay::FOOTER_H })
            };
            let (l, p) = place(br, s);
            let im = &mut app.sofa.images;
            if im.body.as_ref().is_none_or(|(b, _)| *b != look) {
                im.body = Some((look.clone(), draw_body(&look, p, &ink).into_buffer()));
            }
            push(&mut out, &app.sofa.images.body.as_ref().unwrap().1, l, p);
        }
        None => app.sofa.images.body = None,
    }
    let look = GroundLook { out: outp, frame: f };
    let im = &mut app.sofa.images;
    if im.ground.as_ref().is_none_or(|(g, _)| *g != look) {
        im.ground = Some((look.clone(), draw_ground(&look, &ink).into_buffer()));
    }
    let (l, p) = place(Rect { x: 0, y: 0, w: outp.0, h: outp.1 }, s);
    push(&mut out, &app.sofa.images.ground.as_ref().unwrap().1, l, p);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sofa_focus::Screen;

    #[test]
    fn every_text_of_the_header_and_footer_has_its_glyphs() {
        let k = 4.0 / 3.0;
        let mut texts: Vec<(Font, &str)> = vec![
            (font(Typeface::Bricolage, 28.0, Weight::SemiBold, k), "BitsOS"),
            (font(Typeface::Bricolage, 40.0, Weight::SemiBold, k), "Schnellmenü"),
            (font(Typeface::Figtree, 18.0, Weight::SemiBold, k), "Menü"),
            (font(Typeface::Figtree, 17.0, Weight::Regular, k), hd::QUICK_HINT),
            (font(Typeface::JetBrainsMono, 16.0, Weight::Regular, k), "Kabel WLAN Kein Netz Ton 30 %"),
            (font(Typeface::JetBrainsMono, 16.0, Weight::Regular, k), crate::sofa_app::VOLUME_UNKNOWN),
            (font(Typeface::JetBrainsMono, 24.0, Weight::Medium, k), "20:14"),
        ];
        for s in Screen::PILLS {
            texts.push((font(Typeface::Figtree, 20.0, Weight::SemiBold, k), s.title()));
        }
        for s in [Screen::Start, Screen::Bibliothek, Screen::Einstellungen] {
            for h in lay::hints(s) {
                let sign = if h.round { font(Typeface::Figtree, 17.0, Weight::Bold, k) } else { font(Typeface::Figtree, 14.0, Weight::Bold, k) };
                texts.push((sign, h.sign));
                texts.push((font(Typeface::Figtree, 17.0, Weight::Regular, k), h.word));
                texts.push((font(Typeface::Figtree, 17.0, Weight::Regular, k), h.ways));
            }
        }
        // GM-6b: the texts of Start, Bibliothek and Medien in their fonts.
        let body = "Zuletzt gespielt heute um 19:40 · 2 Std. 15 Min. Gestern Vor 2 Tagen Letzte Woche Noch nie gespielt wird geladen \
            Proton Linux über Proton 1,2 GB 238 GB auf /data installiert Titel Listen Alben kommt bald nicht installiert Webseite \
            Browser „Spiel“ ließ sich nicht starten: steam fehlt oder ist nicht ausführbar. Größe A–Z Zuletzt gespielt Das Spiel wird noch geladen. Brave ist nicht installiert.";
        let mut owned: Vec<(Font, String)> = vec![
            (crate::sofa_tile_draw::label(k), "WEITERSPIELEN · WINDOWS-SPIEL ÜBER PROTON LINUX-SPIEL SAMMLUNGEN SORTIERT NACH AUSGEWÄHLT YOUTUBE · WEITERSCHAUEN MUSIK · ZULETZT GEHÖRT".into()),
            (font(Typeface::JetBrainsMono, 15.0, Weight::SemiBold, k), "WEITERSPIELEN · WINDOWS-SPIEL ÜBER PROTON BIBLIOTHEK".into()),
            (font(Typeface::Bricolage, 76.0, Weight::SemiBold, k), "Dungeon Keeper 2 KKND2 Bibliothek".into()),
            (font(Typeface::Figtree, 20.0, Weight::Medium, k), body.into()),
            (font(Typeface::Figtree, 18.0, Weight::SemiBold, k), body.into()),
            (font(Typeface::Figtree, 22.0, Weight::Bold, k), format!("Spielen Details {}", lay::EMPTY_HERO)),
            (font(Typeface::Bricolage, 34.0, Weight::SemiBold, k), crate::sofa_start_draw::RECENT_TITLE.into()),
            (font(Typeface::Figtree, 19.0, Weight::Regular, k), crate::sofa_library_draw::EMPTY.into()),
            (font(Typeface::Figtree, 20.0, Weight::SemiBold, k), format!("{} {}", crate::sofa_library_draw::IN_STEAM, crate::sofa_tile_draw::STEAM_OPEN)),
            (font(Typeface::JetBrainsMono, 16.0, Weight::Regular, k), "0123456789".into()),
        ];
        use crate::sofa_start_draw as sd;
        for t in [sd::YT_TITLE, sd::MUSIC_NONE, sd::MUSIC_LATER, sd::ALL] {
            owned.push((font(Typeface::Bricolage, 32.0, Weight::SemiBold, k), t.into()));
            owned.push((font(Typeface::Figtree, 18.0, Weight::Regular, k), t.into()));
        }
        for c in crate::sofa_model::Coll::LIBRARY {
            owned.push((font(Typeface::Bricolage, 48.0, Weight::SemiBold, k), c.title().into()));
            owned.push((font(Typeface::Figtree, 21.0, Weight::SemiBold, k), c.title().into()));
        }
        for (f, s) in texts.into_iter().map(|(f, s)| (f, s.to_string())).chain(owned) {
            assert!(s.chars().all(|c| c == ' ' || f.draws(c)), "{s}");
        }
    }

    #[test]
    fn the_start_and_library_bodies_draw_at_2560x1440() {
        use crate::sofa_layout::StartShape;
        use crate::sofa_model::{self as model, Coll, Input};
        let ink = Ink::dark();
        let f = Frame::of(2560, 1440, 100);
        let (bw, bh) = f.board();
        let ix = crate::sofa_model::tests::index("draw");
        let play = crate::playtime::Book::default();
        let tm = |s: i64| crate::status::tm_of(s, true);
        let i = Input { index: &ix, play: &play, favourites: &[], music: None, windows: false, sort: "zuletzt", now: crate::sofa_model::tests::NOW, tm: &tm };
        let hero = model::hero(&i);
        let recent = model::recent(&i, &hero, lay::RECENT_MAX);
        let items = lay::start(bw, StartShape { music: false, recent: recent.len(), game: true }, &measure);
        let youtube = model::tiles(Coll::YouTube, &i).remove(0);
        let look = crate::sofa_start_draw::StartLook { frame: f, bw, bh, hero: hero.clone(), recent, youtube, music: None, items, focus: Some("hero".into()), note: None, motion: true, passend: Vec::new() };
        let o = f.r(Rect { x: 0, y: lay::HEADER_H, w: bw, h: bh - lay::HEADER_H - lay::FOOTER_H });
        let c = crate::sofa_start_draw::draw_start(&look, o, &ink);
        let h = f.r(lay::HERO);
        let colour = crate::sofa_tile_draw::kontext()[hero.colour];
        assert_eq!(c.pixel(h.x + 30, h.y - o.y + h.h / 2), crate::canvas::pack(colour), "the hero in its kontext colour");
        assert_eq!(c.pixel(h.x - 7, h.y - o.y + h.h / 2), crate::canvas::pack(ink.accent), "the focus ring 4 px [board] outside");
        let tiles = model::tiles(Coll::Spiele, &i);
        let items = lay::library(bw, bh, 6, tiles.len(), 0, &measure);
        let look = crate::sofa_library_draw::LibraryLook {
            frame: f, bw, bh, colls: Coll::LIBRARY.iter().map(|c| (c.title(), model::count(*c, &i))).collect(), active: 0, sort: "zuletzt".into(),
            title: "Spiele".into(), info: model::info(Coll::Spiele, &tiles, &i), sel: tiles.first().cloned(), facts: String::new(), tiles, first: 0,
            items: items.clone(), focus: Some("tile:0".into()), note: None, motion: false,
        };
        let c = crate::sofa_library_draw::draw_library(&look, o, &ink);
        let side = f.r(items[0].rect);
        assert_eq!(c.pixel(side.x + 4, side.y - o.y + side.h / 2), crate::canvas::pack(ink.accent_soft), "the active collection");
        let t0 = f.r(items.iter().find(|i| i.id == "tile:0").unwrap().rect);
        let first = &look.tiles[0];
        let want = first.colour.map_or(ink.panel, |n| crate::sofa_tile_draw::kontext()[n]);
        assert_eq!(c.pixel(t0.x + t0.w / 2, t0.y - o.y + 20), crate::canvas::pack(want), "the first tile's colour");
    }

    #[test]
    fn the_header_footer_and_ground_draw_at_2560x1440() {
        let ink = Ink::dark();
        let f = Frame::of(2560, 1440, 100);
        let (bw, bh) = f.board();
        let header = lay::header(bw, Screen::Start, &["Kabel".into()], "20:14", &measure);
        let look = HeaderLook { frame: f, bw, header, clock: "20:14".into(), active: Screen::Start.pill(), focus: Some("pill:bibliothek".into()) };
        let o = Rect { x: 0, y: 0, w: 2560, h: 128 };
        let c = hd::draw_header(&look, o, &ink);
        let pill = f.r(look.header.items[0].rect);
        let mid = c.pixel(pill.x + 4, pill.y + pill.h / 2);
        assert_eq!(mid, crate::canvas::pack(ink.accent_soft), "the active pill is accent_soft");
        let foot = FooterLook { frame: f, bw, bh, hints: lay::footer(bw, bh, Screen::Start, &measure), hover: None };
        let fo = Rect { x: 0, y: 1344, w: 2560, h: 96 };
        let c = hd::draw_footer(&foot, fo, &ink);
        let a = f.r(foot.hints[0].sign);
        assert_eq!(c.pixel(a.x + a.w / 2, a.y - fo.y + 3), crate::canvas::pack(ink.accent), "the A sign is filled");
        let g = draw_ground(&GroundLook { out: (2560, 1440), frame: f }, &ink);
        assert_eq!(g.pixel(16, 16), crate::canvas::pack(ink.line), "a dot at 12 board px");
        assert_eq!(g.pixel(2, 2), crate::canvas::pack(ink.bg));
    }
}
