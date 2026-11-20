//! GPY-PASTE: host tests of a paste into a page's field: the owners bitscomp can serve without a client (itself,
//! the X11 bridge's offer), the channel back from the reading thread, a field that ended meanwhile, Georg's own
//! Ctrl+V against an injected one, and the token that is never in a log line nor in a report.

use std::time::{Duration, Instant};

use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::selection::data_device::set_data_device_selection;

use super::*;
use crate::input::Origin;
use crate::ki_ask::REAL_INPUT;
use crate::page_app::PageState;
use crate::update_page::{ADDRESS, TOKEN_KEY};

const PID: u32 = 9;
const TOKEN: &str = "github_pat_11GEORG0815geheim";

fn ctrl() -> ModifiersState {
    ModifiersState { ctrl: true, ..Default::default() }
}

fn shift() -> ModifiersState {
    ModifiersState { shift: true, ..Default::default() }
}

/// An app with one page at `address`.
fn new_app(address: &str) -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "gpy-paste-test".into(), (2560, 1440));
    app.pages.by_pid.insert(PID, PageState::new(address));
    // bitspkgd answered (the token's row shows only then).
    app.upd.data.loaded = true;
    app.upd_refresh();
    LOG.with(|l| l.borrow_mut().clear());
    (d, app)
}

fn token_row(app: &App) -> usize {
    app.upd.built.model.rows.iter().position(|r| r.key == TOKEN_KEY).expect("the token's row")
}

/// Georg clicks into the token's field.
fn open_token(app: &mut App) -> Part {
    let part = Part::Field(token_row(app));
    (app.input_origin, app.desk.press_origin, app.pointer_origin) =
        (Origin::Physical, Origin::Physical, Origin::Physical);
    app.page_act(PID, part);
    assert!(app.pages.by_pid[&PID].edit.is_some(), "the token's field is open");
    part
}

fn key(app: &mut App, origin: Origin, sym: Keysym, mods: &ModifiersState) {
    app.input_origin = origin;
    app.page_key(PID, sym, mods);
}

fn field(app: &App) -> Option<String> {
    app.pages.by_pid[&PID].edit.as_ref().map(|(_, e)| e.text.clone())
}

fn msg(app: &App) -> Option<(String, bool)> {
    app.pages.by_pid[&PID].msg.clone()
}

use crate::clip_read::LOG;

fn logged() -> Vec<String> {
    LOG.with(|l| l.borrow().clone())
}

#[test]
fn bitscomps_own_copy_goes_into_the_token_field_masked_and_never_logged() {
    let (_d, mut app) = new_app(ADDRESS);
    assert_eq!(app.page_kind(PID), crate::page_kind::PageKind::Update);
    app.sys_clip_set(format!("  {TOKEN}  \nzweite Zeile\n"));
    open_token(&mut app);
    key(&mut app, Origin::Physical, Keysym::v, &ctrl());
    assert_eq!(field(&app).as_deref(), Some(TOKEN), "the first line, without the spaces around it");
    assert_eq!(app.pages.paste.open, None, "nothing on its way any more");
    // The field shows it masked as typed text (only "hidden" in the report, not even its length).
    let rep = app.page_report();
    assert!(rep.contains(&format!("edit=field={TOKEN_KEY}:hidden")) && !rep.contains("GEORG"), "{rep}");
    assert!(logged().iter().all(|l| !l.contains("GEORG")), "{:?}", logged());
    // Shift+Insert pastes too: the text goes in at the cursor.
    key(&mut app, Origin::Physical, Keysym::Home, &ModifiersState::default());
    key(&mut app, Origin::Physical, Keysym::Insert, &shift());
    assert_eq!(field(&app), Some(format!("{TOKEN}{TOKEN}")));
    // Esc: the field ends, nothing kept.
    key(&mut app, Origin::Physical, Keysym::Escape, &ModifiersState::default());
    assert_eq!(field(&app), None);
}

#[test]
fn an_injected_ctrl_v_is_refused_like_an_injected_key() {
    // The token's field: the edit ends, nothing is pasted or stored.
    let (_d, mut app) = new_app(ADDRESS);
    app.sys_clip_set(TOKEN.into());
    open_token(&mut app);
    key(&mut app, Origin::Physical, Keysym::a, &ModifiersState::default());
    key(&mut app, Origin::Injected, Keysym::v, &ctrl());
    assert_eq!(field(&app), None, "the injected Ctrl+V ended the token's edit");
    assert_eq!(msg(&app), Some((REAL_INPUT.into(), true)));
    assert_eq!(app.pages.paste.open, None);
    // A settings field: the injected paste is refused, what Georg typed stays.
    let (_d, mut app) = new_app("einstellungen");
    app.sys_clip_set(TOKEN.into());
    app.page_edit_begin(PID, Part::Search, "maus".into());
    key(&mut app, Origin::Injected, Keysym::v, &ctrl());
    key(&mut app, Origin::Injected, Keysym::Insert, &shift());
    assert_eq!(
        field(&app).as_deref(),
        Some("maus"),
        "an injected paste would read Georg's clipboard into a reported field"
    );
    assert_eq!(msg(&app), Some((REAL_INPUT.into(), true)));
    assert!(
        logged().iter().any(|l| l.contains("injected input refused")) && logged().iter().all(|l| !l.contains("GEORG"))
    );
    // His own Ctrl+V there works.
    key(&mut app, Origin::Physical, Keysym::v, &ctrl());
    assert_eq!(field(&app), Some(format!("maus{TOKEN}")));
}

#[test]
fn an_empty_clipboard_says_so() {
    let (_d, mut app) = new_app("einstellungen");
    app.page_edit_begin(PID, Part::Search, String::new());
    key(&mut app, Origin::Physical, Keysym::v, &ctrl());
    assert_eq!(field(&app).as_deref(), Some(""));
    assert_eq!(msg(&app), Some((EMPTY.into(), true)));
}

/// Runs the loop until the page says something (the reading thread's answer came back).
fn until_said(ev: &mut EventLoop<'static, App>, app: &mut App) {
    let t = Instant::now();
    while msg(app).is_none() && app.pages.paste.open.is_some() && t.elapsed() < Duration::from_secs(5) {
        ev.dispatch(Some(Duration::from_millis(20)), app).expect("dispatch");
    }
}

#[test]
fn an_x11_owner_is_asked_through_the_bridge_on_a_thread() {
    // A server-side selection bitscomp does not hold itself is an X11 program's (the bridge offers it). Without an
    // X display the bridge closes the pipe: the thread reads its end, the channel brings "no text" back.
    let (_d, mut app) = new_app("einstellungen");
    let mut ev = EventLoop::<'static, App>::try_new().expect("loop");
    app.page_paste_start(&ev.handle());
    set_data_device_selection(&app.dh, &app.seat, vec!["UTF8_STRING".into()], ());
    app.page_edit_begin(PID, Part::Search, "x".into());
    key(&mut app, Origin::Physical, Keysym::v, &ctrl());
    assert!(app.pages.paste.open.is_some(), "the paste is on its way, the loop did not wait");
    until_said(&mut ev, &mut app);
    assert_eq!(msg(&app), Some((EMPTY.into(), true)));
    assert_eq!(field(&app).as_deref(), Some("x"));
}

#[test]
fn the_text_back_from_the_thread_lands_only_in_its_own_open_field() {
    let (_d, mut app) = new_app(ADDRESS);
    let part = open_token(&mut app);
    // Its text lands at the cursor.
    app.pages.paste.open = Some((5, PID, part));
    app.page_pasted(Pasted { n: 5, text: Ok("abc".into()) });
    assert_eq!(field(&app).as_deref(), Some("abc"));
    // A late answer of an earlier paste is dropped.
    app.pages.paste.open = Some((7, PID, part));
    app.page_pasted(Pasted { n: 6, text: Ok("alt".into()) });
    assert_eq!(field(&app).as_deref(), Some("abc"));
    assert!(app.pages.paste.open.is_some(), "the newer one still waits");
    // The field ended meanwhile (Esc, an injected key, Enter): its paste goes unseen, also into a new field.
    app.input_origin = Origin::Physical;
    app.page_edit_end();
    assert_eq!(app.pages.paste.open, None);
    open_token(&mut app);
    app.page_pasted(Pasted { n: 7, text: Ok("spät".into()) });
    assert_eq!(field(&app).as_deref(), Some(""));
    // The owner gave nothing in time, or too much.
    app.pages.paste.open = Some((8, PID, part));
    app.page_pasted(Pasted { n: 8, text: Err(Why::Slow) });
    assert_eq!(msg(&app), Some((SLOW.into(), true)));
    app.pages.paste.open = Some((9, PID, part));
    app.page_pasted(Pasted { n: 9, text: Err(Why::TooLong) });
    assert_eq!(msg(&app), Some((TOO_LONG.into(), true)));
    // A line the field cannot hold is refused whole, never cut.
    app.pages.paste.open = Some((10, PID, part));
    app.page_pasted(Pasted { n: 10, text: Ok("a".repeat(FIELD_MAX + 1)) });
    assert_eq!((field(&app).as_deref(), msg(&app)), (Some(""), Some((TOO_LONG.into(), true))));
    // A second Ctrl+V while one is on its way does nothing.
    app.sys_clip_set(TOKEN.into());
    app.pages.paste.open = Some((11, PID, part));
    key(&mut app, Origin::Physical, Keysym::v, &ctrl());
    assert_eq!(field(&app).as_deref(), Some(""));
}
