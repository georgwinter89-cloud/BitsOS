//! REWIND stutter: a hover or a choice redraws only its part of the view, and the picture always equals the whole
//! view drawn at once.

use super::*;
use crate::rewind_draw::paint;

const FULL: u64 = 2560 * 1440;

/// One more step: the picture brought up to date; the pixels that step drew.
fn step(app: &mut App) -> u64 {
    let before = app.rewind.images.draw_px;
    assert!(paint::paint(app));
    let drawn = app.rewind.images.draw_px - before;
    assert!(paint::bytes(app).unwrap() == paint::whole(app), "the picture differs from the view drawn whole");
    drawn
}

fn hover(app: &mut App, h: Hot) {
    let (_, r) = *app.rewind_parts().iter().find(|(p, _)| *p == h).unwrap();
    let s = app.theme.scale;
    app.handle_input_as(InputEvent::Motion { x: (r.x as f64 + r.w as f64 / 2.0) * s, y: (r.y as f64 + r.h as f64 / 2.0) * s }, Physical);
}

#[test]
fn a_hover_or_a_choice_redraws_its_part_and_the_picture_stays_the_whole_view() {
    let (_d, mut app) = opened();
    assert_eq!(step(&mut app), FULL, "opened: drawn once, whole");
    assert_eq!(step(&mut app), 0, "nothing changed: nothing drawn");
    app.rewind_tab(Tab::Path);
    let l = sent(&mut app);
    let entries: Vec<String> = (0..12).map(|i| format!(r#"{{"name":"Ordner {i}","kind":"folder","open":true}}"#)).collect();
    answer(&mut app, l[0].0, &format!(r#"{{"ok":true,"entries":[{}]}}"#, entries.join(",")));
    assert_eq!(step(&mut app), FULL, "another way: its side and its button change, drawn whole once");
    let mut most = 0;
    for h in [Hot::Row(0), Hot::Row(1), Hot::Row(2), Hot::Back, Hot::Now, Hot::Chip(1), Hot::Way(Tab::All), Hot::Older, Hot::Row(3)] {
        hover(&mut app, h);
        most = most.max(step(&mut app));
    }
    assert!(most > 0 && most < FULL / 12, "a hover drew {most} pixels (the whole view is {FULL})");
    key(&mut app, KEY_DOWN, Physical);
    let picked = step(&mut app);
    assert!(picked > 0 && picked < FULL / 8, "choosing a row drew {picked} pixels");
    key(&mut app, KEY_LEFT, Physical);
    let l = sent(&mut app);
    let older = step(&mut app);
    assert!(older < FULL * 3 / 5, "another snapshot (its time, two chips, the list loading) drew {older} pixels");
    answer(&mut app, l[0].0, r#"{"ok":true,"entries":[{"name":"brief.txt","kind":"file","open":false,"bytes":1200}]}"#);
    step(&mut app);
    // The dialog comes over all of it; Tab in it moves the focus: two buttons drawn.
    key(&mut app, KEY_DOWN, Physical);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    answer(&mut app, l[0].0, r#"{"ok":true,"path":"/data/home/1000/brief.txt","then":{"kind":"file","bytes":1200},"now":{"kind":"file","bytes":1500}}"#);
    assert!(app.rewind.dialog.is_some());
    assert_eq!(step(&mut app), FULL);
    key(&mut app, KEY_TAB, Physical);
    let tab = step(&mut app);
    assert!(tab > 0 && tab < FULL / 12, "the dialog's focus drew {tab} pixels");
    key(&mut app, KEY_ESC, Physical);
    step(&mut app);
    let n = app.rewind.images.draws;
    assert!(app.rewind_state().contains(&format!("view draws={n} ")), "{}", app.rewind_state());
    app.rewind_close("test");
    assert!(!paint::paint(&mut app) && app.rewind.images.draws == 0, "closed: no picture kept");
}
