//! The focus model on the layouts of every screen of sofa-mode.md §2 (board numbers, sofa_layout.rs).

use super::*;
use crate::sofa_layout::{self as lay, StartShape, Text};

/// A rough width for the tests (the shell measures with the real fonts).
fn rough(s: &str, t: Text) -> i32 {
    let px = match t {
        Text::Word => 28.0,
        Text::Pill | Text::Button => 20.0,
        Text::HeroButton => 22.0,
        Text::Status | Text::Hint | Text::Chip => 17.0,
        Text::Clock => 24.0,
        Text::Menu => 18.0,
        Text::Sign => 14.0,
        Text::Section => 34.0,
    };
    (s.chars().count() as f32 * px * 0.55).round() as i32
}

const BW: i32 = 1920;
const BH: i32 = 1080;

fn screen(s: Screen, body: Vec<Item>) -> Vec<Item> {
    let mut v = if s == Screen::Suche { Vec::new() } else { lay::header(BW, s, &["Kabel".into()], "20:14", &rough).items };
    v.extend(body);
    v
}

/// From element `from`, the moves `dirs` in turn; the id it ends on (None: a move found nothing).
fn walk(items: &[Item], from: &str, dirs: &[Dir]) -> Option<String> {
    let mut i = index(items, from).unwrap_or_else(|| panic!("no {from}"));
    for d in dirs {
        i = step(items, i, *d)?;
    }
    Some(items[i].id.clone())
}

fn go(items: &[Item], from: &str, d: Dir) -> Option<String> {
    walk(items, from, &[d])
}

use Dir::{Down, Left, Right, Up};

#[test]
fn the_header_walks_left_to_right_and_down_into_the_screen() {
    let items = screen(Screen::Start, lay::start(BW, StartShape { music: true, recent: 4, game: true }, &rough));
    let order = ["pill:start", "pill:bibliothek", "pill:medien", "pill:einstellungen", "search", "menu"];
    for w in order.windows(2) {
        assert_eq!(go(&items, w[0], Right).as_deref(), Some(w[1]), "{} -> right", w[0]);
        assert_eq!(go(&items, w[1], Left).as_deref(), Some(w[0]), "{} -> left", w[1]);
    }
    assert_eq!(go(&items, "menu", Right), None, "no wrap at the end");
    assert_eq!(go(&items, "pill:start", Left), None);
    assert_eq!(go(&items, "pill:start", Up), None);
    assert_eq!(go(&items, "pill:start", Down).as_deref(), Some("hero"));
    assert_eq!(go(&items, "menu", Down).as_deref(), Some("card:youtube"));
}

#[test]
fn start_moves_by_the_boards_rows() {
    let items = screen(Screen::Start, lay::start(BW, StartShape { music: true, recent: 4, game: true }, &rough));
    // GM-6b: Details lies on the hero: Right from the hero, Left back; its other moves go as the hero's.
    assert_eq!(go(&items, "hero", Right).as_deref(), Some("hero:details"));
    assert_eq!(go(&items, "hero:details", Right).as_deref(), Some("card:youtube"));
    assert_eq!(go(&items, "hero:details", Left).as_deref(), Some("hero"));
    assert!(go(&items, "hero:details", Up).unwrap().starts_with("pill:"));
    assert_eq!(go(&items, "hero:details", Down).as_deref(), Some("alle"));
    assert_eq!(go(&items, "card:youtube", Down).as_deref(), Some("card:musik"));
    assert_eq!(go(&items, "card:musik", Left).as_deref(), Some("hero"));
    assert_eq!(go(&items, "card:youtube", Left).as_deref(), Some("hero"));
    assert_eq!(go(&items, "hero", Left), None, "no left column on Start, no wrap");
    assert!(go(&items, "hero", Up).unwrap().starts_with("pill:"), "content -> header pills up");
    assert_eq!(go(&items, "tile:0", Up).as_deref(), Some("alle"));
    assert_eq!(go(&items, "alle", Up).as_deref(), Some("hero"));
    assert_eq!(walk(&items, "tile:0", &[Right, Right, Right, Right]).as_deref(), Some("steam"));
    assert_eq!(go(&items, "steam", Right), None);
    assert_eq!(go(&items, "tile:0", Down), None, "the page's end");
    assert_eq!(go(&items, "card:musik", Down).as_deref(), Some("steam"));
    // Without a music player the music card is not there (skipped by the focus).
    let items = screen(Screen::Start, lay::start(BW, StartShape { music: false, recent: 0, game: false }, &rough));
    assert!(index(&items, "card:musik").is_none());
    assert_eq!(go(&items, "card:youtube", Down).as_deref(), Some("alle"), "down finds the next row whatever its x");
    assert_eq!(go(&items, "hero", Down).as_deref(), Some("alle"));
}

#[test]
fn the_library_goes_between_its_column_and_its_grid() {
    let colls = 6;
    let items = screen(Screen::Bibliothek, lay::library(BW, BH, colls, 12, 0, &rough));
    assert!(index(&items, "search").is_none(), "no search button on the library (board)");
    assert_eq!(go(&items, "coll:0", Right).as_deref(), Some("tile:0"), "the column -> content right");
    let left = go(&items, "tile:0", Left).unwrap();
    assert!(left.starts_with("coll:"), "content -> left column: {left}");
    assert_eq!(walk(&items, "tile:0", &[Right, Right, Right, Right]).as_deref(), Some("tile:4"));
    assert_eq!(go(&items, "tile:4", Right), None, "no jump to the buttons far below, no wrap");
    // Two rows of tiles fit at 1920x1080 (sofa_layout::lib_geom); below them the bar.
    assert_eq!(go(&items, "tile:2", Down).as_deref(), Some("tile:7"));
    assert_eq!(go(&items, "tile:7", Down).as_deref(), Some("play"));
    assert!(index(&items, "tile:10").is_none(), "the third row scrolls");
    assert_eq!(go(&items, "play", Right).as_deref(), Some("steam-open"));
    assert_eq!(walk(&items, "coll:0", &[Down, Down, Down, Down, Down]).as_deref(), Some("coll:5"));
    assert_eq!(go(&items, "coll:5", Down).as_deref(), Some("sort"));
    assert!(go(&items, "coll:0", Up).unwrap().starts_with("pill:"));
    assert!(go(&items, "tile:0", Up).unwrap().starts_with("pill:"));
}

#[test]
fn settings_go_from_the_sections_to_the_rows_and_back() {
    for (i, (_, rows)) in lay::SECTIONS.iter().enumerate() {
        let items = screen(Screen::Einstellungen, lay::settings(BW, *rows));
        let sec = format!("section:{i}");
        if *rows == 0 {
            assert_eq!(go(&items, &sec, Right), None, "Konten: text only");
            continue;
        }
        assert!(go(&items, &sec, Right).unwrap().starts_with("row:"), "{sec} -> its rows");
        let mut at = "row:0".to_string();
        for j in 1..*rows {
            at = go(&items, &at, Down).unwrap();
            assert_eq!(at, format!("row:{j}"));
        }
        assert_eq!(go(&items, &at, Down), None);
        assert!(go(&items, "row:0", Left).unwrap().starts_with("section:"));
    }
}

#[test]
fn the_quick_menu_keeps_the_focus_in_its_panel() {
    let items = lay::quick(BW, BH, true, &["Fernseher (HDMI)", "Kopfhörer (USB)"], &rough);
    assert_eq!(go(&items, "q:resume", Right).as_deref(), Some("q:end"));
    assert_eq!(go(&items, "q:resume", Down).as_deref(), Some("q:volume"));
    // Down from the slider: the output chip nearest by centres.
    assert_eq!(go(&items, "q:volume", Down).as_deref(), Some("q:out:1"));
    assert_eq!(go(&items, "q:out:1", Left).as_deref(), Some("q:out:0"));
    assert_eq!(go(&items, "q:out:0", Right).as_deref(), Some("q:out:1"));
    assert_eq!(go(&items, "q:out:0", Down).as_deref(), Some("q:wlan"));
    assert_eq!(walk(&items, "q:wlan", &[Right, Down]).as_deref(), Some("q:power"));
    assert_eq!(go(&items, "q:screen", Down).as_deref(), Some("q:desktop"));
    assert_eq!(go(&items, "q:desktop", Down).as_deref(), Some("q:reboot"));
    assert_eq!(go(&items, "q:reboot", Right).as_deref(), Some("q:off"));
    for (id, d) in [("q:resume", Left), ("q:resume", Up), ("q:off", Down), ("q:off", Right)] {
        assert_eq!(go(&items, id, d), None, "{id} {d:?}: the panel's edge");
    }
    // Without a running program the Läuft-gerade card is left out.
    let items = lay::quick(BW, BH, false, &["Fernseher (HDMI)"], &rough);
    assert!(index(&items, "q:resume").is_none() && index(&items, "q:end").is_none());
    assert_eq!(go(&items, "q:volume", Down).as_deref(), Some("q:out:0"));
}

#[test]
fn the_keyboard_wraps_per_row_and_keeps_the_column() {
    let items = screen(Screen::Suche, lay::search(BW, BH, true, 0, 0, lay::Level::Letters, "Suchen"));
    assert!(index(&items, "pill:start").is_none(), "no header on the search screen");
    assert_eq!(go(&items, "key:ß", Right).as_deref(), Some("key:⌫ Löschen"));
    assert_eq!(go(&items, "key:⌫ Löschen", Right).as_deref(), Some("key:1"), "wraps in its row");
    assert_eq!(go(&items, "key:q", Left).as_deref(), Some("key:ü"));
    assert_eq!(go(&items, "key:Suchen", Right).as_deref(), Some("key:&123"));
    assert_eq!(go(&items, "key:1", Down).as_deref(), Some("key:q"));
    assert_eq!(go(&items, "key:z", Down).as_deref(), Some("key:h"));
    assert_eq!(go(&items, "key:m", Down).as_deref(), Some("key:Leerzeichen"));
    assert_eq!(go(&items, "key:⌫ Löschen", Down).as_deref(), Some("key:ü"), "the nearest key below by x");
    assert_eq!(go(&items, "key:Leerzeichen", Down), None);
    assert_eq!(go(&items, "key:1", Up).as_deref(), Some("hit:0"), "up to the hits");
    assert_eq!(go(&items, "hit:0", Right).as_deref(), Some("hit:1"));
    assert_eq!(go(&items, "hit:0", Up), None);
}

#[test]
fn the_mouse_hover_finds_the_element_under_it() {
    let items = screen(Screen::Start, lay::start(BW, StartShape { music: true, recent: 2, game: true }, &rough));
    let hero = &items[index(&items, "hero").unwrap()];
    let (x, y) = (hero.rect.x as f64 + 10.0, hero.rect.y as f64 + 10.0);
    assert_eq!(at(&items, x, y).map(|i| items[i].id.as_str()), Some("hero"));
    let det = &items[index(&items, "hero:details").unwrap()];
    let (x, y) = (det.rect.x as f64 + 5.0, det.rect.y as f64 + 5.0);
    assert_eq!(at(&items, x, y).map(|i| items[i].id.as_str()), Some("hero:details"), "the button on the tile, not the tile");
    assert_eq!(at(&items, 5.0, 5.0), None, "the mark is no button");
}

#[test]
fn b_goes_back_one_level() {
    let mut n = Nav::default();
    assert_eq!(n.back(Some(Area::Content)), Back::Stay, "B never leaves the sofa mode");
    n.quick = true;
    assert_eq!(n.back(None), Back::Quick);
    assert!(!n.quick);
    n.go(Screen::Bibliothek);
    assert_eq!(n.back(Some(Area::Content)), Back::Screen(Screen::Start));
    n.go(Screen::Einstellungen);
    assert_eq!(n.back(Some(Area::Content)), Back::Column, "a row: back to its section");
    assert_eq!(n.back(Some(Area::Side)), Back::Screen(Screen::Start));
    n.go(Screen::Medien);
    n.go(Screen::Suche);
    assert_eq!(n.back(Some(Area::Keys)), Back::Screen(Screen::Medien), "search -> the screen it came from");
    // LB/RB on Start: the next pill, no wrap.
    let n = Nav::default();
    assert_eq!(n.pill_step(1), Some(Screen::Bibliothek));
    assert_eq!(n.pill_step(-1), None);
    let n = Nav { screen: Screen::Einstellungen, ..Nav::default() };
    assert_eq!(n.pill_step(1), None);
}

/// Every element of every screen of §2 is reached by D-pad moves from the screen's first element, and every id is
/// there once (no element the focus can never land on, none drawn twice).
#[test]
fn every_element_of_every_screen_is_reachable_and_there_once() {
    let mut screens: Vec<(&str, Vec<Item>)> = vec![
        ("Start", screen(Screen::Start, lay::start(BW, StartShape { music: true, recent: 4, game: true }, &rough))),
        ("Start without music", screen(Screen::Start, lay::start(BW, StartShape { music: false, recent: 0, game: false }, &rough))),
        ("Bibliothek", screen(Screen::Bibliothek, lay::library(BW, BH, 6, 12, 0, &rough))),
        ("Medien", screen(Screen::Medien, lay::library(BW, BH, 3, 5, 0, &rough))),
        ("Schnellmenü", lay::quick(BW, BH, true, &["Fernseher (HDMI)", "Kopfhörer (USB)"], &rough)),
        ("Schnellmenü without a program", lay::quick(BW, BH, false, &["Fernseher (HDMI)"], &rough)),
        ("Suche", screen(Screen::Suche, lay::search(BW, BH, true, 0, 0, lay::Level::Letters, "Suchen"))),
    ];
    for (_, rows) in lay::SECTIONS.iter() {
        screens.push(("Einstellungen", screen(Screen::Einstellungen, lay::settings(BW, *rows))));
    }
    for (name, items) in &screens {
        assert!(!items.is_empty(), "{name}: no elements");
        let mut ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "{name}: an id twice");
        let mut seen = vec![false; items.len()];
        let mut todo = vec![0];
        seen[0] = true;
        while let Some(i) = todo.pop() {
            for d in [Up, Down, Left, Right] {
                if let Some(j) = step(items, i, d)
                    && !seen[j]
                {
                    seen[j] = true;
                    todo.push(j);
                }
            }
        }
        let lost: Vec<&str> = items.iter().zip(&seen).filter(|(_, s)| !**s).map(|(i, _)| i.id.as_str()).collect();
        assert!(lost.is_empty(), "{name}: never reached from {}: {lost:?}", items[0].id);
    }
}
