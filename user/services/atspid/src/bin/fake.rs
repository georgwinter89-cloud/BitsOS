//! atspid-fake: a small program on the accessibility bus with a known tree, for the tests (host and QEMU; real
//! programs are tested only on the Mini-PC). It announces itself like atk-bridge (Embed, GetRegisteredEvents) and
//! answers Accessible, Component, Action, Text, EditableText, Selection and Value. What was done shows in the
//! name of the "Status" label, so a test sees it through the tree alone. `--browser DIR` (EINGABE) is a tree like
//! Chromium's instead: its fields offer no EditableText, their text is read from DIR/text.<n> (written by the
//! Wayland client the fake runs beside), and the focused field's number goes to DIR/focus.
#![forbid(unsafe_code)]

use atspid::atspi::{self, role, state};
use atspid::registry::Ref;
use std::sync::{Arc, Mutex};
use zbus::{Connection, OwnedObjectPath, Type, interface};

#[derive(Default, Clone)]
struct N {
    role: u32,
    name: String,
    states: Vec<u32>,
    kids: Vec<usize>,
    parent: usize,
    rect: (i32, i32, i32, i32),
    actions: Vec<&'static str>,
    text: Option<String>,
    value: Option<f64>,
    selection: bool,
    editable: bool,
    /// `--browser`: the text lives in this file (the Wayland client beside the fake writes what it was given).
    file: Option<std::path::PathBuf>,
    /// Object attributes (Chromium's `xml-roles`, `autocomplete`).
    attrs: Vec<(&'static str, &'static str)>,
    /// `--browser`: part of the browser's own bar (its address bar), not of the page.
    ui: bool,
}

type M = Arc<Mutex<Vec<N>>>;

fn path(i: usize) -> String {
    if i == 0 { atspi::ROOT_PATH.to_string() } else { format!("/org/a11y/atspi/accessible/{i}") }
}

fn obj(i: usize) -> OwnedObjectPath {
    OwnedObjectPath::try_from(path(i)).expect("a valid path")
}

const SHOWN: &[u32] = &[state::SHOWING, 30, state::ENABLED, state::SENSITIVE];
/// The "Status" label's place in the tree (17 in the form, 6 in `--browser`).
static STATUS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(17);
/// `--browser DIR`: where the focused field's number goes (the Wayland client reads it).
static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// `--browser`: the field with the keyboard (as Chromium's browser bar or page) and the page's focused element.
static KB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(3);
static PAGE_FOCUS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// `--browser`: the new-tab page, the Google page that replaces it after a press on its tile, and how long the
/// tree keeps the old page after the title already changed (as Brave did on the Mini-PC).
const NEW_TAB: usize = 7;
const GOOGLE: usize = 9;
const TILE: usize = 8;
const SWAP_AFTER: std::time::Duration = std::time::Duration::from_millis(900);

/// EINGABE/EINGABE2: a browser like Chromium: fields with Text but without EditableText (the desktop writes into
/// them), the address bar in the browser's own bar, a new-tab page (Google's box as an editable combobox, a long
/// multi-line comment, a "Google" tile) and a Google page the tile leads to (its box, "Google Suche" and a hidden
/// copy of that button in a closed suggestion list). Focus as in Chromium: while the bar has the keyboard, the first
/// GrabFocus on a page field only moves the page's focus (both say FOCUSED), the second moves the keyboard.
fn browser(dir: &std::path::Path) -> Vec<N> {
    let shown = |extra: &[u32]| {
        let mut v = SHOWN.to_vec();
        v.extend_from_slice(extra);
        v
    };
    let mk = |role, name: &str, states, parent, rect| N { role, name: name.into(), states, parent, rect, ..N::default() };
    let field = [state::EDITABLE, state::FOCUSABLE];
    let mut v = vec![
        mk(role::APPLICATION, "Brave", vec![], 0, (0, 0, 0, 0)),
        mk(role::FRAME, "Neuer Tab - Brave", shown(&[state::ACTIVE]), 0, (0, 0, 400, 300)),
        mk(39, "", shown(&[]), 1, (0, 0, 400, 300)),
        mk(role::ENTRY, "Adress- und Suchleiste", shown(&[field[0], field[1], state::SINGLE_LINE, state::FOCUSED]), 2, (10, 10, 380, 24)),
        // Google's box is a <textarea role=combobox>: Chromium says MULTI_LINE.
        mk(role::COMBO_BOX, "Suche", shown(&[field[0], field[1], state::MULTI_LINE]), NEW_TAB, (10, 60, 300, 24)),
        mk(role::ENTRY, "Kommentar", shown(&[field[0], field[1], state::MULTI_LINE]), NEW_TAB, (10, 100, 380, 120)),
        mk(role::LABEL, "Status: bereit", shown(&[]), 2, (10, 270, 300, 20)),
        mk(role::DOCUMENT_WEB, "Neuer Tab", shown(&[]), 2, (0, 40, 400, 220)),
        mk(role::PUSH_BUTTON, "Google", shown(&[state::FOCUSABLE]), NEW_TAB, (320, 60, 70, 24)),
        mk(role::DOCUMENT_WEB, "Google", shown(&[]), 2, (0, 40, 400, 220)),
        mk(role::COMBO_BOX, "Suche", shown(&[field[0], field[1], state::MULTI_LINE]), GOOGLE, (40, 120, 320, 30)),
        mk(role::PUSH_BUTTON, "Google Suche", shown(&[state::FOCUSABLE]), GOOGLE, (40, 170, 120, 28)),
        mk(39, "", shown(&[]), GOOGLE, (40, 150, 320, 0)),
        // The suggestion list's copy: on the bus and SHOWING, but not VISIBLE and without a size.
        mk(role::PUSH_BUTTON, "Google Suche", vec![state::SHOWING, state::ENABLED, state::SENSITIVE, state::FOCUSABLE], 12, (0, 0, 0, 0)),
    ];
    for i in 1..v.len() {
        let p = v[i].parent;
        if i != p && i != GOOGLE {
            v[p].kids.push(i);
        }
    }
    let start = [(3, "https://example.org/".to_string()), (4, String::new()), (5, "Ein langer Kommentar. ".repeat(14)), (10, String::new())];
    for (i, t) in start {
        let f = dir.join(format!("text.{i}"));
        std::fs::write(&f, t).expect("the field's file");
        let (x, y, w, h) = v[i].rect;
        std::fs::write(dir.join(format!("rect.{i}")), format!("{x} {y} {w} {h}")).expect("the field's place");
        v[i].file = Some(f);
        v[i].text = Some(String::new());
    }
    for i in [4, 10] {
        v[i].actions = vec!["click"];
        v[i].attrs = vec![("xml-roles", "combobox"), ("autocomplete", "both")];
    }
    for i in [TILE, 11, 13] {
        v[i].actions = vec!["click"];
    }
    v[3].ui = true;
    v[6].text = Some(v[6].name.clone());
    STATUS.store(6, std::sync::atomic::Ordering::Relaxed);
    let _ = std::fs::write(dir.join("focus"), "3");
    v
}

/// `--browser`: the tile was pressed. The window's title (the Wayland client's, DIR/title) says "Google" at once;
/// the tree keeps the new-tab page (and its window's old name) a moment longer, then holds the Google page instead.
fn navigate(m: &M) {
    if let Some(d) = DIR.get() {
        let _ = std::fs::write(d.join("title"), "Google - Brave");
    }
    let m = m.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SWAP_AFTER);
        let mut g = lock(&m);
        g[1].name = "Google - Brave".into();
        for k in g[2].kids.iter_mut() {
            if *k == NEW_TAB {
                *k = GOOGLE;
            }
        }
        // The old page's focus goes with it; a keyboard that was in it is back in the address bar.
        use std::sync::atomic::Ordering::Relaxed;
        let page = PAGE_FOCUS.swap(0, Relaxed);
        if page != 0 {
            g[page].states.retain(|s| *s != state::FOCUSED);
        }
        if !g[KB.load(Relaxed)].ui {
            KB.store(3, Relaxed);
            g[3].states.push(state::FOCUSED);
            if let Some(d) = DIR.get() {
                let _ = std::fs::write(d.join("focus"), "3");
            }
        }
        status(&mut g, "Seite Google".into());
        drop(g);
    });
}

/// `--browser`: Chromium's GrabFocus (see `browser`). Returns the field that has the keyboard now.
fn browser_focus(m: &mut [N], i: usize) -> usize {
    use std::sync::atomic::Ordering::Relaxed;
    let (kb, page) = (KB.load(Relaxed), PAGE_FOCUS.load(Relaxed));
    let unfocus = |m: &mut [N], bar: bool| m.iter_mut().filter(|n| n.ui == bar).for_each(|n| n.states.retain(|s| *s != state::FOCUSED));
    if m[i].ui {
        // The bar takes the keyboard; the page keeps its focused element (Chromium still says FOCUSED there).
        unfocus(m, true);
        m[i].states.push(state::FOCUSED);
        KB.store(i, Relaxed);
        return i;
    }
    unfocus(m, false);
    m[i].states.push(state::FOCUSED);
    PAGE_FOCUS.store(i, Relaxed);
    if m[kb].ui && page != i {
        return kb;
    }
    unfocus(m, true);
    KB.store(i, Relaxed);
    i
}

/// The known tree (the test in tests/bridge.rs and the ktest check it).
fn model() -> Vec<N> {
    let s = |extra: &[u32], shown: bool| {
        let mut v: Vec<u32> = if shown { SHOWN.to_vec() } else { vec![state::ENABLED, state::SENSITIVE] };
        v.extend_from_slice(extra);
        v
    };
    let mk = |role, name: &str, states, parent, rect| N { role, name: name.into(), states, parent, rect, ..N::default() };
    let mut v = vec![
        mk(role::APPLICATION, "Fake", vec![], 0, (0, 0, 0, 0)),
        mk(role::FRAME, "Formular", s(&[state::ACTIVE], true), 0, (0, 0, 800, 600)),
        mk(39, "", s(&[], true), 1, (0, 0, 800, 600)),
        mk(role::LABEL, "Name", s(&[], true), 2, (10, 10, 80, 20)),
        mk(role::ENTRY, "Name", s(&[state::EDITABLE, state::FOCUSABLE, state::FOCUSED], true), 2, (100, 10, 200, 20)),
        mk(role::PASSWORD_TEXT, "Passwort", s(&[state::EDITABLE, state::FOCUSABLE], true), 2, (100, 40, 200, 20)),
        mk(role::CHECK_BOX, "Merken", s(&[state::FOCUSABLE], true), 2, (100, 70, 120, 20)),
        mk(role::COMBO_BOX, "Farbe", s(&[state::FOCUSABLE], true), 2, (100, 100, 120, 20)),
        mk(role::POPUP_MENU, "", s(&[], false), 7, (100, 120, 120, 40)),
        mk(role::MENU_ITEM, "Rot", s(&[state::SELECTED], false), 8, (100, 120, 120, 20)),
        mk(role::MENU_ITEM, "Grün", s(&[], false), 8, (100, 140, 120, 20)),
        mk(role::PUSH_BUTTON, "Senden", s(&[state::FOCUSABLE], true), 2, (100, 130, 80, 24)),
        mk(role::PUSH_BUTTON, "Versteckt", s(&[], false), 2, (0, 0, 10, 10)),
        mk(role::LIST, "Zahlen", s(&[], true), 2, (100, 160, 120, 40)),
        mk(role::LIST_ITEM, "Eins", s(&[state::SELECTED], true), 13, (100, 160, 120, 20)),
        mk(role::LIST_ITEM, "Zwei", s(&[], true), 13, (100, 180, 120, 20)),
        mk(role::SLIDER, "Lautstärke", s(&[state::FOCUSABLE], true), 2, (100, 210, 200, 20)),
        mk(role::LABEL, "Status: bereit", s(&[], true), 2, (10, 240, 300, 20)),
        mk(role::PUSH_BUTTON, "Gesperrt", vec![state::SHOWING, 30], 2, (200, 130, 80, 24)),
    ];
    for i in 1..v.len() {
        let p = v[i].parent;
        if i != p {
            v[p].kids.push(i);
        }
    }
    v[4].text = Some("Georg".into());
    v[5].text = Some("geheim123".into());
    v[3].text = Some("Name".into());
    v[17].text = Some("Status: bereit".into());
    for i in [4, 5] {
        v[i].editable = true;
    }
    for i in [6, 11, 18] {
        v[i].actions = vec!["click"];
    }
    v[9].actions = vec!["click"];
    v[10].actions = vec!["click"];
    v[16].value = Some(30.0);
    for i in [8, 13] {
        v[i].selection = true;
    }
    v
}

fn status(m: &mut [N], what: String) {
    let at = STATUS.load(std::sync::atomic::Ordering::Relaxed);
    m[at].name = format!("Status: {what}");
    m[at].text = Some(m[at].name.clone());
}

fn toggle(st: &mut Vec<u32>, s: u32) {
    if let Some(i) = st.iter().position(|x| *x == s) {
        st.remove(i);
    } else {
        st.push(s);
    }
}

fn select(m: &mut [N], container: usize, index: usize) -> bool {
    let Some(&k) = m[container].kids.get(index) else { return false };
    for c in m[container].kids.clone() {
        m[c].states.retain(|s| *s != state::SELECTED);
    }
    m[k].states.push(state::SELECTED);
    status(m, format!("gewählt {}", m[k].name));
    true
}

fn lock(m: &M) -> std::sync::MutexGuard<'_, Vec<N>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Acc(M, usize);

#[interface(name = "org.a11y.atspi.Accessible")]
impl Acc {
    fn get_children(&self, #[zbus(connection)] c: &Connection) -> Vec<Ref> {
        let me = c.unique_name().map(|n| n.to_string()).unwrap_or_default();
        lock(&self.0)[self.1].kids.iter().map(|k| Ref(me.clone(), obj(*k))).collect()
    }
    fn get_role(&self) -> u32 {
        lock(&self.0)[self.1].role
    }
    fn get_state(&self) -> Vec<u32> {
        let mask = lock(&self.0)[self.1].states.iter().fold(0u64, |m, s| m | 1 << s);
        vec![mask as u32, (mask >> 32) as u32]
    }
    fn get_interfaces(&self) -> Vec<String> {
        let n = lock(&self.0)[self.1].clone();
        let mut v = vec![atspi::ACCESSIBLE.to_string()];
        if self.1 != 0 {
            v.push(atspi::COMPONENT.into());
        }
        if !n.actions.is_empty() {
            v.push(atspi::ACTION.into());
        }
        if n.text.is_some() {
            v.push(atspi::TEXT.into());
        }
        if n.editable {
            v.push(atspi::EDITABLE_TEXT.into());
        }
        if n.selection {
            v.push(atspi::SELECTION.into());
        }
        if n.value.is_some() {
            v.push(atspi::VALUE.into());
        }
        v
    }
    fn get_index_in_parent(&self) -> i32 {
        let m = lock(&self.0);
        m[m[self.1].parent].kids.iter().position(|k| *k == self.1).map_or(-1, |i| i as i32)
    }
    fn get_attributes(&self) -> std::collections::HashMap<String, String> {
        lock(&self.0)[self.1].attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    #[zbus(property)]
    fn name(&self) -> String {
        lock(&self.0)[self.1].name.clone()
    }
    #[zbus(property)]
    fn child_count(&self) -> i32 {
        lock(&self.0)[self.1].kids.len() as i32
    }
}

#[derive(serde::Serialize, Type)]
struct Extents(i32, i32, i32, i32);

struct Comp(M, usize);

#[interface(name = "org.a11y.atspi.Component")]
impl Comp {
    fn get_extents(&self, _coord: u32) -> Extents {
        let r = lock(&self.0)[self.1].rect;
        Extents(r.0, r.1, r.2, r.3)
    }
    fn grab_focus(&self) -> bool {
        let mut m = lock(&self.0);
        if let Some(d) = DIR.get() {
            let kb = browser_focus(&mut m, self.1);
            let _ = std::fs::write(d.join("focus"), kb.to_string());
        } else {
            for n in m.iter_mut() {
                n.states.retain(|s| *s != state::FOCUSED);
            }
            m[self.1].states.push(state::FOCUSED);
        }
        let what = format!("Fokus {}", m[self.1].name);
        status(&mut m, what);
        true
    }
    fn scroll_to(&self, _kind: u32) -> bool {
        true
    }
}

struct Act(M, usize);

#[interface(name = "org.a11y.atspi.Action")]
impl Act {
    fn get_actions(&self) -> Vec<(String, String, String)> {
        lock(&self.0)[self.1].actions.iter().map(|a| (a.to_string(), String::new(), String::new())).collect()
    }
    fn do_action(&self, index: i32) -> bool {
        let mut m = lock(&self.0);
        let i = self.1;
        if index != 0 || m[i].actions.is_empty() || !m[i].states.contains(&state::SENSITIVE) {
            return false;
        }
        match m[i].role {
            role::CHECK_BOX => {
                toggle(&mut m[i].states, state::CHECKED);
                let on = m[i].states.contains(&state::CHECKED);
                status(&mut m, format!("Merken {}", if on { "an" } else { "aus" }));
            }
            role::MENU_ITEM => {
                let p = m[i].parent;
                let k = m[p].kids.iter().position(|k| *k == i).unwrap_or(0);
                return select(&mut m, p, k);
            }
            _ => {
                let what = format!("gedrückt {} mit {}", m[i].name, m[4].text.clone().unwrap_or_default());
                status(&mut m, what);
                if DIR.get().is_some() && i == TILE {
                    drop(m);
                    navigate(&self.0);
                }
            }
        }
        true
    }
    #[zbus(property)]
    fn n_actions(&self) -> i32 {
        lock(&self.0)[self.1].actions.len() as i32
    }
}

struct Txt(M, usize);

#[interface(name = "org.a11y.atspi.Text")]
impl Txt {
    fn get_text(&self, start: i32, end: i32) -> String {
        let n = lock(&self.0)[self.1].clone();
        let t = n.file.and_then(|f| std::fs::read_to_string(f).ok()).or(n.text).unwrap_or_default();
        let end = if end < 0 { t.chars().count() } else { end as usize };
        t.chars().skip(start.max(0) as usize).take(end.saturating_sub(start.max(0) as usize)).collect()
    }
    #[zbus(property)]
    fn character_count(&self) -> i32 {
        self.get_text(0, -1).chars().count() as i32
    }
    // `--browser`: a selection lives in DIR/sel.<n> as "start end" in characters (the Wayland client replaces it).
    fn get_n_selections(&self) -> i32 {
        i32::from(self.sel_file().is_some_and(|f| f.exists()))
    }
    fn get_selection(&self, _n: i32) -> (i32, i32) {
        let t = self.sel_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
        let mut w = t.split_whitespace().filter_map(|x| x.parse().ok());
        (w.next().unwrap_or(0), w.next().unwrap_or(0))
    }
    fn add_selection(&self, start: i32, end: i32) -> bool {
        self.sel_file().is_some_and(|f| std::fs::write(f, format!("{start} {end}")).is_ok())
    }
    fn set_selection(&self, _n: i32, start: i32, end: i32) -> bool {
        self.add_selection(start, end)
    }
}

impl Txt {
    fn sel_file(&self) -> Option<std::path::PathBuf> {
        DIR.get().filter(|_| lock(&self.0)[self.1].file.is_some()).map(|d| d.join(format!("sel.{}", self.1)))
    }
}

struct Edit(M, usize);

#[interface(name = "org.a11y.atspi.EditableText")]
impl Edit {
    fn set_text_contents(&self, text: String) -> bool {
        let mut m = lock(&self.0);
        m[self.1].text = Some(text);
        let what = format!("geschrieben in {}", m[self.1].name);
        status(&mut m, what);
        true
    }
}

struct Sel(M, usize);

#[interface(name = "org.a11y.atspi.Selection")]
impl Sel {
    fn select_child(&self, index: i32) -> bool {
        select(&mut lock(&self.0), self.1, index.max(0) as usize)
    }
    #[zbus(property)]
    fn n_selected_children(&self) -> i32 {
        let m = lock(&self.0);
        m[self.1].kids.iter().filter(|k| m[**k].states.contains(&state::SELECTED)).count() as i32
    }
}

struct Val(M, usize);

#[interface(name = "org.a11y.atspi.Value")]
impl Val {
    #[zbus(property)]
    fn current_value(&self) -> f64 {
        lock(&self.0)[self.1].value.unwrap_or_default()
    }
    #[zbus(property)]
    fn set_current_value(&self, v: f64) {
        let mut m = lock(&self.0);
        m[self.1].value = Some(v);
        status(&mut m, format!("Wert {v}"));
    }
}

/// `--child`: a process of its own below the fake, with no tree (it must never answer for the fake, nor the fake
/// for it). It ends when the fake does.
fn idle_child() -> ! {
    let parent = rustix::process::getppid();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if rustix::process::getppid() != parent {
            std::process::exit(0);
        }
    }
}

async fn run(addr: String, child: Option<u32>) -> zbus::Result<()> {
    let m: M = Arc::new(Mutex::new(DIR.get().map_or_else(model, |d| browser(d))));
    let n = lock(&m).len();
    let mut b = zbus::connection::Builder::address(addr.as_str());
    for i in 0..n {
        let x = lock(&m)[i].clone();
        b = b.serve_at(path(i), Acc(m.clone(), i));
        if i != 0 {
            b = b.serve_at(path(i), Comp(m.clone(), i));
        }
        if !x.actions.is_empty() {
            b = b.serve_at(path(i), Act(m.clone(), i));
        }
        if x.text.is_some() {
            b = b.serve_at(path(i), Txt(m.clone(), i));
        }
        if x.editable {
            b = b.serve_at(path(i), Edit(m.clone(), i));
        }
        if x.selection {
            b = b.serve_at(path(i), Sel(m.clone(), i));
        }
        if x.value.is_some() {
            b = b.serve_at(path(i), Val(m.clone(), i));
        }
    }
    let c = b.build().await?;
    let me = c.unique_name().map(|n| n.to_string()).unwrap_or_default();
    let reply = c.call_method(Some(atspi::REGISTRY_NAME), atspi::ROOT_PATH, Some(atspi::SOCKET), "Embed", &(Ref(me, obj(0)),)).await?;
    let (desktop,): (Ref,) = reply.body().deserialize()?;
    let events = c.call_method(Some(atspi::REGISTRY_NAME), atspi::REGISTRY_PATH, Some(atspi::REGISTRY), "GetRegisteredEvents", &()).await?;
    let ev: Vec<(String, String)> = events.body().deserialize()?;
    let child = child.map(|c| format!(", child {c}")).unwrap_or_default();
    println!("atspid-fake: embedded under {} {} ({} event listeners), pid {}{child}", desktop.0, desktop.1.as_str(), ev.len(), std::process::id());
    std::future::pending::<()>().await;
    Ok(())
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    if a.iter().any(|x| x == "--idle") {
        idle_child();
    }
    let child = a.iter().any(|x| x == "--child").then(|| {
        let me = std::env::current_exe().expect("the fake's own path");
        std::process::Command::new(me).arg("--idle").spawn().expect("the child").id()
    });
    if let Some(i) = a.iter().position(|x| x == "--browser").filter(|i| i + 1 < a.len()) {
        let _ = DIR.set(a.remove(i + 1).into());
    }
    let addr = match a.iter().position(|x| x == "--bus") {
        Some(i) if i + 1 < a.len() => format!("unix:path={}", a.remove(i + 1)),
        _ => std::env::var("AT_SPI_BUS_ADDRESS").unwrap_or_else(|_| format!("unix:path={}", atspid::serve::BUS)),
    };
    if let Err(e) = zruntime::block_on(run(addr, child)) {
        eprintln!("atspid-fake: {e}");
        std::process::exit(1);
    }
}
