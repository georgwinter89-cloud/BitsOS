//! Pointer commands of the control socket, so tests drive every mouse way like a hand would, plus `hot`:
//! where the clickable things are (physical output pixels, the unit pointer commands take): window lines, then
//! one line per drawn tab (rect, its close button, window id, label, glyph boxes) and per "+N" chip, then the
//! chrome, start page and suggestion lines of WY-5 (`chrome ...`, `fav`, `ws`, `start ...`, `suggest`).
//!
//! pointer X Y | press [left|right|middle] [X Y] | release [BTN] [X Y] | click [X Y] [BTN] | wheel N
//! drag X1 Y1 X2 Y2 [BTN] (press, eight moves, release) | hot

use crate::input::InputEvent;
use crate::layout::{Place, Rect};
use crate::menu::item_rect;
use crate::pointer::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use crate::state::App;
use smithay::desktop::Window;

use crate::{header, tabs, zones};

fn button_code(w: Option<&&str>) -> Result<u32, String> {
    match w.copied() {
        None | Some("left") => Ok(BTN_LEFT),
        Some("right") => Ok(BTN_RIGHT),
        Some("middle") => Ok(BTN_MIDDLE),
        Some(o) => Err(format!("unknown button '{o}'")),
    }
}

pub fn execute(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    let num = |i: usize| -> Result<f64, String> {
        words.get(i).ok_or("missing number")?.parse::<f64>().map_err(|e| e.to_string())
    };
    let mv = |app: &mut App, x: f64, y: f64| app.handle_input(InputEvent::Motion { x, y });
    let btn = |app: &mut App, code: u32, pressed: bool| app.handle_input(InputEvent::Button { code, pressed });
    let r = (|| -> Result<String, String> {
        match words[0] {
            "pointer" => mv(app, num(1)?, num(2)?),
            "press" | "release" => {
                let code = button_code(words.get(1))?;
                if words.len() >= 4 {
                    mv(app, num(2)?, num(3)?);
                }
                btn(app, code, words[0] == "press");
            }
            "click" => {
                let code = button_code(words.get(3))?;
                if words.len() >= 3 {
                    mv(app, num(1)?, num(2)?);
                }
                btn(app, code, true);
                btn(app, code, false);
            }
            "drag" => {
                let code = button_code(words.get(5))?;
                let (x1, y1, x2, y2) = (num(1)?, num(2)?, num(3)?, num(4)?);
                mv(app, x1, y1);
                btn(app, code, true);
                for i in 1..=8 {
                    let f = i as f64 / 8.0;
                    mv(app, x1 + (x2 - x1) * f, y1 + (y2 - y1) * f);
                }
                btn(app, code, false);
            }
            "wheel" => app.handle_input(InputEvent::Wheel { steps: num(1)? }),
            "hot" => return Ok(hot(app)),
            _ => return Err(String::new()),
        }
        Ok("ok".into())
    })();
    match r {
        Err(e) if e.is_empty() => None,
        r => Some(r),
    }
}

/// One line per drawn tab of a header and one for its "+N" chip.
fn tab_hot(app: &App, win: &Window, place: Place, strip: &tabs::Strip) -> String {
    let t = &app.theme;
    let s = t.scale;
    let mid = |r: Rect| {
        format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round())
    };
    let rect = |r: Rect| {
        let p = header::px_rect(r, s);
        format!("{},{},{},{}", p.x, p.y, p.w, p.h)
    };
    let font = header::tab_font(t);
    let (n, shown) = zones::pages(place);
    let labels = header::labels(app, win, place);
    let owner = match place {
        Place::Tiled { tile, .. } => format!("tile={tile}"),
        Place::Floating { index } => format!("float={index}"),
    };
    let mut out = String::new();
    for (i, &r) in strip.tabs.iter().enumerate() {
        let page = strip.first + i;
        let Some(w) = app.tab_window(win, place, page) else { continue };
        let (tx0, tx1) = tabs::text_room(r, t);
        let room = header::px_rect(Rect { x: tx0, w: tx1 - tx0, ..r }, s);
        let shows = font.fit(&labels[page], room.w.max(0));
        let glyphs: Vec<String> = font.boxes(&shows).iter().map(|(x, w)| format!("{x}:{w}")).collect();
        let pr = header::px_rect(r, s);
        let x = tabs::close_rect(r, t);
        out.push_str(&format!(
            "tab {owner} page={page}/{n} shown={} id={} at={} close={} rect={} closerect={} text={},{} cell=0x{} \
             glyphs={} shows={shows:?} title={:?}\n",
            page == shown,
            crate::ctl::win_id(&w),
            mid(r),
            mid(x),
            rect(r),
            rect(x),
            room.x,
            pr.y + (pr.h - font.height()) / 2,
            font.height(),
            glyphs.join(","),
            labels[page],
        ));
    }
    if let Some((r, count)) = strip.more {
        out.push_str(&format!("more {owner} count={count} at={} rect={}\n", mid(r), rect(r)));
    }
    out
}

/// The WY-5 lines: chrome parts, workspaces, favourites, the start page, the suggestions.
fn chrome_hot(app: &App, pt: &dyn Fn(i32, i32) -> String) -> String {
    let mid = |r: Rect| pt(r.x + r.w / 2, r.y + r.h / 2);
    let c = app.chrome();
    let mut out = format!(
        "chrome address at={} back={} forward={} ring={} work={},{},{},{} sidebar={} editing={} list={} wsfold={}{}\n",
        mid(c.address),
        mid(c.back),
        mid(c.forward),
        mid(c.ring),
        c.work.x,
        c.work.y,
        c.work.w,
        c.work.h,
        c.side.is_some(),
        app.desk.edit.is_some(),
        c.list_shown,
        mid(c.ws_fold),
        c.ws_button.map_or(String::new(), |b| format!(" wsbutton={}", mid(b)))
    );
    for (i, r) in c.fav_items.iter().enumerate() {
        out.push_str(&format!("fav {i} at={} addr={}\n", mid(*r), app.desk.favourites[i]));
    }
    for (i, r) in c.ws_items.iter().take(if c.list_shown { c.ws_shown() } else { 0 }).enumerate() {
        out.push_str(&format!("ws {i} at={} active={} name={:?}\n", mid(*r), i == app.desk.active, app.desk.names[i]));
    }
    out.push_str(&format!("wsnew at={}\nhome at={}\n", mid(c.ws_new), mid(c.home)));
    if app.start_shown() {
        let p = app.start_page();
        let can = app.session.offered && app.session.saved.is_some();
        out.push_str(&format!("start restore at={} enabled={can}\nstart new at={}\n", mid(p.restore), mid(p.new)));
        for (i, r) in p.favs.iter().enumerate().take(app.desk.favourites.len()) {
            out.push_str(&format!("start fav {i} at={}\n", mid(*r)));
        }
        for (i, r) in p.recents.iter().enumerate().take(app.session.recent.len()) {
            out.push_str(&format!("start recent {i} at={} addr={}\n", mid(*r), app.session.recent[i].addr));
        }
    }
    let rows = app.suggestions();
    let (_, placed) = crate::suggest::layout(&rows, app.edit_field(), app.area_full().h - 8);
    for (i, ((_, r), row)) in placed.iter().zip(&rows).enumerate() {
        out.push_str(&format!("suggest {i} at={} group={} addr={}\n", mid(*r), row.group.replace(' ', "_"), row.addr));
    }
    out
}

fn hot(app: &App) -> String {
    let t = &app.theme;
    let s = t.scale;
    let pt = |x: i32, y: i32| format!("{},{}", (x as f64 * s).round(), (y as f64 * s).round());
    let mid = |r: Rect| pt(r.x + r.w / 2, r.y + r.h / 2);
    let mut out = String::new();
    let mut tab_lines = String::new();
    for p in app.placements().into_iter().filter(|p| p.visible) {
        let f = p.frame;
        let (_, title) = crate::ctl::names(&p.win);
        let place = match p.place {
            Place::Tiled { tile, index, depth, .. } => format!("tile={tile} page={index}/{depth}"),
            Place::Floating { index } => format!("float={index}"),
        };
        let (n, shown) = zones::pages(p.place);
        let floating = matches!(p.place, Place::Floating { .. });
        let strip = tabs::strip(zones::tab_frame(f, floating, t), n, shown, t);
        let own = strip.tab(shown).unwrap_or(Rect { h: t.header_height, ..f });
        out.push_str(&format!(
            "win {place} header={} float={} close={} content={}",
            pt(own.x + (own.w / 4).min(40), f.y + t.header_height / 2),
            mid(zones::float_button(f, t)),
            mid(zones::close_button(f, t)),
            mid(app.content_rect(f)),
        ));
        // The client's part in screen pixels (x,y,w,h), for clicks at a terminal's first row.
        let b = header::px_rect(app.content_rect(f), s);
        out.push_str(&format!(" body={},{},{},{}", b.x, b.y, b.w, b.h));
        if let Place::Floating { index } = p.place {
            out.push_str(&format!(" corner={}", pt(f.x + f.w + 2, f.y + f.h + 2)));
            // WY-5c: the maximise button, and whether the window is maximised.
            let max = app.ws.floating[index].restore.is_some();
            out.push_str(&format!(" max={} maximised={max}", mid(zones::max_button(f, t))));
        }
        if crate::veil::shown(app, &p.win) {
            out.push_str(&format!(" kill={}", mid(crate::veil::kill_button(app.content_rect(f), t))));
            out.push_str(&format!(" wait={}", mid(crate::veil::wait_button(app.content_rect(f), t))));
        }
        if t.tile_bar > 0 {
            out.push_str(&format!(" tileaddr={}", mid(crate::header::tile_address_rect(f, t))));
        }
        out.push_str(&format!(" id={} title={title:?}\n", crate::ctl::win_id(&p.win)));
        // Tabs come after every window line, so a title pattern finds the window line first.
        tab_lines.push_str(&tab_hot(app, &p.win, p.place, &strip));
    }
    out.push_str(&tab_lines);
    let frames = app.ws.tile_frames(app.area(), t.gap);
    for (i, w) in frames.windows(2).enumerate() {
        out.push_str(&format!("gap={i} at={}\n", pt((w[0].x + w[0].w + w[1].x) / 2, w[0].y + w[0].h / 2)));
    }
    let a = app.area();
    out.push_str(&format!("empty at={}\n", pt(a.x + a.w / 2, a.y + t.gap / 2)));
    if let Some(m) = &app.menu {
        let (cx, cy) = (m.centre.0 as i32, m.centre.1 as i32);
        out.push_str(&format!("menu centre={}\n", pt(cx, cy)));
        for (d, it) in m.items.iter().enumerate() {
            if let Some(it) = it {
                let r = item_rect(d, t);
                let at = mid(Rect { x: cx + r.x, y: cy + r.y, ..r });
                out.push_str(&format!("item={d} at={at} enabled={} label={:?}\n", it.enabled, it.label));
            }
        }
    }
    out.push_str(&chrome_hot(app, &pt));
    out.push_str(&app.page_hot());
    out.push_str("end");
    out
}
