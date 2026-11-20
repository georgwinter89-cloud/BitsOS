//! GPY-PASTE: a provider's key pasted into its field on the page "KI": Georg's own Ctrl+V fills the field (shown
//! and reported as hidden), his Enter stores it; an injected Ctrl+V ends the edit and stores nothing.

use smithay::input::keyboard::{Keysym, ModifiersState};

use super::tests::{PID, SECRET, enter, msg, new_app, open_key, sent, with_lokal1};
use super::*;
use crate::input::Origin;
use crate::settings_write::request_line;

fn ctrl_v(app: &mut App, origin: Origin) {
    app.input_origin = origin;
    app.page_key(PID, Keysym::v, &ModifiersState { ctrl: true, ..Default::default() });
}

#[test]
fn a_key_pasted_by_georg_is_stored_an_injected_paste_ends_the_edit() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    app.sys_clip_set(format!("{SECRET}\n"));
    // Injected: the key's edit ends, nothing is pasted or sent.
    open_key(&mut app);
    ctrl_v(&mut app, Origin::Injected);
    assert!(app.pages.by_pid[&PID].edit.is_none() && sent(&rx).is_empty());
    assert_eq!(msg(&app), (REAL_INPUT.into(), true));
    // Georg's own: the field holds the key, nothing outside shows it.
    open_key(&mut app);
    ctrl_v(&mut app, Origin::Physical);
    assert!(app.ki_secret_edit(PID));
    let rep = app.page_report();
    assert!(rep.contains("edit=field=ai.provider.lokal1.key:hidden") && !rep.contains("geheim"), "{rep}");
    enter(&mut app, Origin::Physical);
    let reqs = sent(&rx);
    assert_eq!(reqs.len(), 1);
    let j = Json::parse(&request_line(&reqs[0])).unwrap();
    assert_eq!(j.get("key").and_then(Json::as_str), Some("ai.provider.lokal1.key"));
    assert_eq!(j.get("value").and_then(Json::as_str), Some(SECRET), "the first line, without its newline");
    assert!(crate::clip_read::LOG.with(|l| l.borrow().iter().all(|x| !x.contains("geheim"))));
}
