use super::*;
use crate::library::Index;

/// Terminal, Einstellungen and the given (id, name, StartupWMClass) programs, A-Z.
fn index(more: &[(&str, &str, &str)]) -> Index {
    let base = Index::builtin();
    let mut ix = base.clone();
    let t = base.entries.iter().find(|e| e.id == "terminal").unwrap();
    for (id, name, wm) in more {
        let mut e = t.clone();
        e.id = id.to_string();
        e.name = name.to_string();
        e.address = format!("programm/{id}");
        e.wm_class = wm.to_string();
        e.exec = vec!["/bin/sh".into(), format!("/stick/{id}")];
        ix.entries.push(e);
    }
    ix.entries.sort_by(|a, b| crate::library::order(&a.name, &b.name));
    ix
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn rows_show_favourites_recent_by_program_and_all() {
    let ix = index(&[("writer", "Writer", "libreoffice-writer"), ("brave", "Brave", "brave-browser")]);
    let favs = s(&["programm/terminal", "einstellungen/aussehen", "dokumente/x"]);
    // Two windows of Writer (one by its window word), the settings twice, an unknown address, Brave.
    let recent = s(&["programm/writer", "programm/libreoffice-writer", "einstellungen/favoriten", "einstellungen", "web/x", "programm/brave-browser"]);
    let r = rows(&ix, &favs, &recent, "");
    assert_eq!(r.iter().map(|r| r.title).collect::<Vec<_>>(), [FAVOURITES, RECENT, ALL]);
    assert_eq!(r[0].ids, s(&["terminal", "einstellungen"]), "addresses of no program are left out");
    assert_eq!(r[1].ids, s(&["writer", "einstellungen", "brave"]), "counted by program, not by address");
    assert_eq!(r[2].ids, s(&["brave", "einstellungen", "terminal", "writer"]), "A-Z");
    // Up to eight recent programs.
    let many: Vec<(String, String)> = (0..12).map(|i| (format!("p{i:02}"), format!("P{i:02}"))).collect();
    let refs: Vec<(&str, &str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str(), "")).collect();
    let ix2 = index(&refs);
    let rec: Vec<String> = many.iter().map(|(a, _)| format!("programm/{a}")).collect();
    assert_eq!(rows(&ix2, &[], &rec, "")[0].ids.len(), RECENT_MAX);
    // No favourites: the row is not there.
    assert_eq!(rows(&ix, &[], &[], "")[0].title, ALL);
}

#[test]
fn a_search_is_one_row_best_first() {
    let ix = index(&[("writer", "Writer", ""), ("brave", "Brave", "")]);
    let r = rows(&ix, &s(&["programm/terminal"]), &[], "wri");
    assert_eq!(r, [Row { title: HITS, ids: s(&["writer"]) }]);
    assert!(rows(&ix, &[], &[], "zzzz").is_empty(), "nothing found: no row");
}

#[test]
fn favourites_edit_keeps_order_and_has_no_duplicates() {
    let ix = index(&[("writer", "Writer", "libreoffice-writer"), ("brave", "Brave", "")]);
    let favs = s(&["programm/terminal", "einstellungen/aussehen"]);
    let p = pin(&ix, &favs, "brave");
    assert_eq!(p, s(&["programm/terminal", "einstellungen/aussehen", "programm/brave"]));
    assert_eq!(pin(&ix, &p, "brave"), p, "pinned twice: once");
    assert_eq!(pin(&ix, &p, "terminal"), p, "Terminal is pinned already");
    assert!(pinned(&ix, &p, "brave") && !pinned(&ix, &p, "writer"));
    assert_eq!(pin(&ix, &p, "nothere"), p, "an unknown id changes nothing");
    let u = unpin(&ix, &p, "terminal");
    assert_eq!(u, s(&["einstellungen/aussehen", "programm/brave"]), "the others keep their order");
    // Every address that names the program goes: by its window word too.
    let two = s(&["programm/writer", "programm/brave", "programm/libreoffice-writer"]);
    assert_eq!(unpin(&ix, &two, "writer"), s(&["programm/brave"]));
    assert_eq!(unpin(&ix, &u, "writer"), u, "not pinned: unchanged");
}

#[test]
fn arrows_move_in_the_grid_and_cross_rows() {
    use Move::*;
    // Rows of 3, 10 and 4 tiles, 4 columns.
    let lens = [3, 10, 4];
    let c = 4;
    assert_eq!(step(&lens, c, (0, 0), Right), (0, 1));
    assert_eq!(step(&lens, c, (0, 2), Right), (1, 0), "past a row's end: the next row");
    assert_eq!(step(&lens, c, (1, 0), Left), (0, 2), "before a row's start: the previous row's last");
    assert_eq!(step(&lens, c, (0, 0), Left), (0, 0), "the very first stays");
    assert_eq!(step(&lens, c, (2, 3), Right), (2, 3), "the very last stays");
    assert_eq!(step(&lens, c, (1, 1), Down), (1, 5));
    assert_eq!(step(&lens, c, (1, 7), Down), (1, 9), "a shorter last line: its last tile");
    assert_eq!(step(&lens, c, (1, 9), Down), (2, 1), "below the last line: the next row, same column");
    assert_eq!(step(&lens, c, (0, 2), Down), (1, 2));
    assert_eq!(step(&lens, c, (2, 3), Down), (2, 3));
    assert_eq!(step(&lens, c, (1, 5), Up), (1, 1));
    assert_eq!(step(&lens, c, (2, 2), Up), (1, 9), "above: the previous row's last line, clamped");
    assert_eq!(step(&lens, c, (2, 0), Up), (1, 8));
    assert_eq!(step(&lens, c, (1, 3), Up), (0, 2), "into a short row: clamped to its end");
    assert_eq!(step(&lens, c, (0, 1), Up), (0, 1));
    // Tab and Shift+Tab: the next or previous row's first tile, wrapping.
    assert_eq!(step(&lens, c, (0, 2), NextRow), (1, 0));
    assert_eq!(step(&lens, c, (2, 3), NextRow), (0, 0));
    assert_eq!(step(&lens, c, (0, 2), PrevRow), (2, 0));
    assert_eq!(step(&lens, c, (1, 4), PrevRow), (0, 0));
    // A focus from before an index swap (past the end) is clamped first; no rows, no panic.
    assert_eq!(step(&lens, c, (9, 99), Left), (2, 2));
    assert_eq!(step(&[], c, (0, 0), Down), (0, 0));
    assert_eq!(step(&[3, 0], 0, (1, 0), Down), (0, 0));
}

#[test]
fn the_panel_fits_every_screen_and_scrolling_keeps_the_focus_seen() {
    let t = Theme::default();
    for full in [Rect { x: 0, y: 0, w: 2048, h: 1152 }, Rect { x: 0, y: 0, w: 1024, h: 640 }, Rect { x: 0, y: 0, w: 100, h: 80 }] {
        let g = geo(full, 40, &t);
        assert!(g.panel.x >= full.x && g.panel.x + g.panel.w <= full.x + full.w, "{full:?}");
        assert!(g.panel.y >= 40 && g.panel.y + g.panel.h <= full.y + full.h, "{full:?}");
        assert!(g.cols >= 1 && g.body.w > 0 && g.body.h > 0);
        let (placed, content) = place(&g, &[3, 30]);
        let last = *placed[1].1.last().unwrap();
        assert!(last.x + last.w <= g.body.x + g.body.w || g.cols == 1, "{full:?}");
        assert!(content >= last.y + last.h);
        let s = scroll_to(0, last, None, g.body.h, content);
        assert!(last.y - s >= 0 && last.y + last.h - s <= g.body.h.max(last.h), "{full:?}");
        assert_eq!(scroll_to(s, placed[0].1[0], Some(placed[0].0), g.body.h, content), 0, "back to the top with its heading");
    }
    assert_eq!(wheel(0, 3.0, 10, 100, 1000), 30);
    assert_eq!(wheel(0, -3.0, 10, 100, 1000), 0);
    assert_eq!(wheel(890, 3.0, 10, 100, 1000), 900);
    assert_eq!(wheel(0, 3.0, 10, 100, 50), 0, "content shorter than the body: no scrolling");
}

#[test]
fn focus_survives_a_rescan_that_removes_it_and_empty_lists() {
    let row = |t, ids: &[&str]| Row { title: t, ids: s(ids) };
    let rows = vec![row(FAVOURITES, &["terminal"]), row(ALL, &["brave", "terminal", "writer"])];
    let at = |r: usize, id: &str| locate(&rows, Some(&(r, id.to_string())));
    assert_eq!(at(1, "writer"), Some((1, 2)));
    assert_eq!(at(0, "writer"), Some((1, 2)), "not in its row any more: found in another");
    assert_eq!(at(1, "gone"), Some((0, 0)), "removed by a rescan: the first tile");
    assert_eq!(locate(&rows, None), Some((0, 0)));
    assert_eq!(locate(&[], Some(&(0, "brave".into()))), None, "an empty index or search shows nothing to focus");
    assert_eq!(locate(&[], None), None);
    // An empty index gives no rows; a search that finds nothing too.
    let mut empty = Index::builtin();
    empty.entries.clear();
    assert!(rows_of(&empty, "").is_empty() && rows_of(&index(&[]), "zzzzqx").is_empty());
    for m in [Move::Left, Move::Right, Move::Up, Move::Down, Move::NextRow, Move::PrevRow] {
        assert_eq!(step(&[], 4, (3, 3), m), (0, 0), "{m:?} on nothing");
        assert_eq!(step(&[1], 4, (0, 0), m).0, 0, "{m:?} on one tile stays on it");
    }
}

fn rows_of(ix: &Index, q: &str) -> Vec<Row> {
    rows(ix, &[], &[], q)
}

#[test]
fn pin_into_the_default_favourites_keeps_them() {
    let ix = index(&[("brave", "Brave", "brave-browser")]);
    let default: Vec<String> = crate::apply::DEFAULT_FAVOURITES.iter().map(|x| x.to_string()).collect();
    let pinned = pin(&ix, &default, "brave");
    assert_eq!(pinned, s(&["programm/terminal", "einstellungen/aussehen", "programm/brave"]));
    assert_eq!(pin(&ix, &pinned, "brave"), pinned, "no duplicate");
    assert_eq!(pin(&ix, &default, "einstellungen"), default, "Einstellungen is there already (as einstellungen/aussehen)");
    assert_eq!(unpin(&ix, &pinned, "terminal"), s(&["einstellungen/aussehen", "programm/brave"]));
}
