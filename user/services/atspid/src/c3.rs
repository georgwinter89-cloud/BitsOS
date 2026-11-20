//! Contract C3 on /run/bitsos/atspi.sock: JSON lines `{"id","op","args"}`. `tree {pid, window?, title?, origin?,
//! size?, app?}` answers the C1 window (with `id` and `ok`), `act {pid, element, action, text?}` answers
//! `{"ok":true}`; a failure is `{"ok":false,"error":"<German sentence>"}`. `window`, `title`, `origin`, `size` and
//! `app` are optional additions: bitscomp's id of the window, which of the program's windows, where the program's
//! surface (its buffer, client-side shadows included: bitscomp's `window_origin`) starts on the desktop, that
//! surface's size in desktop pixels (rects are scaled when the program counts in other pixels), and the
//! program's name as the desktop knows it. EINGABE: `act` of `type` on a field without EditableText answers
//! `{"ok":true,"by":"desktop"}` once the field has the focus (the desktop writes the text); `focused {pid, element}`
//! answers `{"ok":true,"focused":bool}`, the desktop's check right before it pastes. EINGABE2: `focused` is true
//! only when the field is the one editable element of its window with the focus; `act` of `submit` answers like a
//! desktop `type` (bitscomp presses Enter); `select {pid, element}` selects all of a field's text and answers
//! `{"ok":true,"selected":bool}`; `tree` takes `settle {ms, calm}` and then answers `settled` and `loading`.

use crate::atspi::{self, Bus, state};
use crate::ids::{self, WinIds};
use crate::map::{self, Ctx, Origin};
use crate::read::{self, Reader};
use crate::registry::{self, Apps};
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use zruntime::SharedRuntime;

pub const MAX_LINE: usize = 64 * 1024;
/// A program that hangs must not hold the desktop's question: every call has 3 s, one answer at most this long.
const ANSWER: Duration = Duration::from_secs(10);
const SLOW: &str = "Das Programm antwortet nicht rechtzeitig";

#[derive(Default)]
pub struct State {
    /// Set once the accessibility bus runs; until then every question is answered "switched off".
    pub bus: OnceLock<Bus>,
    pub apps: Arc<Apps>,
    ids: Mutex<HashMap<String, WinIds>>,
}

const OFF: &str = "Durchblick liest fremde Programme gerade nicht (in den Einstellungen ausgeschaltet)";
const NO_TREE: &str = "Dieses Programm zeigt Durchblick keine Bedienelemente, nur sein Bild";

fn err(id: &Value, why: impl Into<String>) -> Value {
    json!({"id": id, "ok": false, "error": why.into()})
}

pub async fn handle(st: &State, line: &str) -> Value {
    let Ok(req) = serde_json::from_str::<Value>(line) else {
        return err(&Value::Null, "Die Anfrage ist kein JSON");
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let args = req.get("args").cloned().unwrap_or(Value::Null);
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    if op == "ping" {
        return json!({"id": id, "ok": true, "running": st.bus.get().is_some(), "programs": st.apps.count()});
    }
    let Some(bus) = st.bus.get() else { return err(&id, OFF) };
    let Some(pid) = args.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else {
        return err(&id, "Es fehlt, welches Programm (pid)");
    };
    let Ok(rt) = SharedRuntime::current() else { return err(&id, SLOW) };
    match op {
        "tree" => match rt.timeout(ANSWER, tree(st, bus, pid, &args)).await.unwrap_or(Err(SLOW.to_string())) {
            Ok(Value::Object(mut w)) => {
                w.insert("id".into(), id);
                w.insert("ok".into(), json!(true));
                Value::Object(w)
            }
            Ok(_) => err(&id, NO_TREE),
            Err(e) => err(&id, e),
        },
        // EINGABE: whether the field the desktop is about to paste into still has the focus.
        "focused" => match rt.timeout(ANSWER, focused(st, bus, pid, &args)).await.unwrap_or(Err(SLOW.to_string())) {
            Ok(f) => json!({"id": id, "ok": true, "focused": f}),
            Err(e) => err(&id, e),
        },
        // EINGABE2: all of the field's text selected (the desktop's text then replaces it), read back.
        "select" => match rt.timeout(ANSWER, select(st, bus, pid, &args)).await.unwrap_or(Err(SLOW.to_string())) {
            Ok(yes) => json!({"id": id, "ok": true, "selected": yes}),
            Err(e) => err(&id, e),
        },
        "act" => match rt.timeout(ANSWER, act(st, bus, pid, &args)).await.unwrap_or(Err(SLOW.to_string())) {
            Ok(crate::act::Acted::Done) => json!({"id": id, "ok": true}),
            // EINGABE: the field has the focus; bitscomp writes the text (C3 addition `by`).
            Ok(crate::act::Acted::ByDesktop) => json!({"id": id, "ok": true, "by": "desktop"}),
            Err(e) => err(&id, e),
        },
        other => err(&id, format!("Unbekannte Anfrage „{other}“")),
    }
}

/// The program's windows as (path, name, states).
async fn frames(r: &Reader<'_>, root: &str) -> Vec<(String, String, u64)> {
    let kids: Vec<String> = r.children(root).await.into_iter().filter(|(b, _)| b.is_empty() || b == r.app).map(|(_, p)| p).collect();
    join_all(kids.iter().map(|p| async move { (p.clone(), r.name(p).await, r.states(p).await) })).await
}

/// The program's window to show: the one titled `title`, else the one read last for this window id (`last`: its
/// title may just be changing, as a page loads), else the active one, else the first on screen. `exact_only` (a
/// process the asked one started): only a window titled exactly `title`, or the one read last.
async fn pick_frame(r: &Reader<'_>, root: &str, title: Option<&str>, exact_only: bool, last: Option<&str>) -> Option<String> {
    let looks = frames(r, root).await;
    let by = |f: &dyn Fn(&(String, String, u64)) -> bool| looks.iter().find(|l| f(l)).map(|l| l.0.clone());
    let exact = title.filter(|t| !t.is_empty()).and_then(|t| by(&|l| l.1 == t)).or_else(|| last.and_then(|p| by(&|l| l.0 == p)));
    if exact_only {
        return exact;
    }
    exact
        .or_else(|| title.and_then(|t| by(&|l| !t.is_empty() && l.1.contains(t))))
        .or_else(|| by(&|l| atspi::has(l.2, state::ACTIVE)))
        .or_else(|| by(&|l| atspi::has(l.2, state::SHOWING)))
        .or_else(|| looks.first().map(|l| l.0.clone()))
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// D (EINGABE2): what a read says about the page in the window. `busy`: its document says BUSY (loading);
/// `mismatch`: the window's title (its own object's name, else the desktop's from the program's toplevel) does not
/// name the document the tree holds, as on the Mini-PC when Brave's title already said "Google - Brave" and its tree
/// still held the new-tab page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub busy: bool,
    pub mismatch: bool,
}

/// Whether the window is as the program will show it: not loading, the title and the document agree, and (`calm`,
/// after a step) the list did not change between two reads (`same`).
pub fn settled(look: Look, calm: bool, same: bool) -> bool {
    !look.busy && !look.mismatch && (!calm || same)
}

/// Between two reads while a window settles.
const SETTLE_LOOK: Duration = Duration::from_millis(150);
/// The longest a caller may have a window settle.
const SETTLE_MAX: u64 = 5000;

fn is_doc(r: u32) -> bool {
    matches!(r, atspi::role::DOCUMENT_WEB | atspi::role::DOCUMENT_FRAME)
}

fn names(title: &str, doc: &str) -> bool {
    let d = doc.trim().to_lowercase();
    !d.is_empty() && title.to_lowercase().contains(&d)
}

/// The window's top documents (not those inside another).
fn top_docs<'n>(n: &'n read::Node, out: &mut Vec<&'n read::Node>) {
    if is_doc(n.role) {
        return out.push(n);
    }
    for c in &n.children {
        top_docs(c, out);
    }
}

/// Looks at the read window by `own` (the window's own name as read now) and `news` (the desktop's title from the
/// program's toplevel, when it is new since the last read: an old one may just not have caught up). Where several
/// top documents hang side by side and some are named by the title, the others are an old page on its way out and
/// are dropped.
pub fn look(node: &mut read::Node, own: Option<&str>, news: Option<&str>) -> Look {
    let clean = |t: Option<&str>| t.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    let (own, news) = (clean(own), clean(news));
    let title = own.clone().or(news.clone()).unwrap_or_default();
    let (named, busy, mismatch) = {
        let mut docs = Vec::new();
        top_docs(node, &mut docs);
        let by = |t: &str| docs.iter().filter(|d| !t.is_empty() && names(t, &d.name)).map(|d| d.path.clone()).collect::<Vec<_>>();
        let named = by(&title);
        let busy = docs.iter().any(|d| atspi::has(d.states, state::BUSY) && (named.is_empty() || named.contains(&d.path)));
        let has_names = docs.iter().any(|d| !d.name.trim().is_empty());
        let news_other = news.as_deref().filter(|n| Some(*n) != own.as_deref()).is_some_and(|n| by(n).is_empty());
        (named.clone(), busy, has_names && (named.is_empty() && !title.is_empty() || news_other))
    };
    if !named.is_empty() {
        fn prune(n: &mut read::Node, keep: &[String]) {
            n.children.retain(|c| !is_doc(c.role) || keep.contains(&c.path));
            for c in n.children.iter_mut().filter(|c| !is_doc(c.role)) {
                prune(c, keep);
            }
        }
        prune(node, &named);
    }
    Look { busy, mismatch }
}

/// C3 `tree`; with `settle {ms, calm}` read again (every 150 ms, up to `ms`) until `settled`; the answer then says
/// `settled` and `loading` (the page still loads or the tree still holds the old one at the deadline).
async fn tree(st: &State, bus: &Bus, pid: u32, args: &Value) -> Result<Value, String> {
    let settle = args.get("settle");
    let Some(ms) = settle.and_then(|s| s.get("ms")).and_then(Value::as_u64) else { return read_window(st, bus, pid, args).await.map(|(v, _)| v) };
    let calm = settle.and_then(|s| s.get("calm")).and_then(Value::as_bool).unwrap_or(false);
    let end = std::time::Instant::now() + Duration::from_millis(ms.min(SETTLE_MAX));
    let mut last: Option<Value> = None;
    loop {
        let (mut v, l) = read_window(st, bus, pid, args).await?;
        let same = last.as_ref().is_some_and(|p| p.get("elements") == v.get("elements") && p.get("title") == v.get("title"));
        let ok = settled(l, calm, same);
        if ok || std::time::Instant::now() >= end {
            if let Value::Object(m) = &mut v {
                m.insert("settled".into(), json!(ok));
                m.insert("loading".into(), json!(!ok && (l.busy || l.mismatch)));
            }
            return Ok(v);
        }
        last = Some(v);
        let Ok(rt) = SharedRuntime::current() else { return Err(SLOW.into()) };
        rt.sleep(SETTLE_LOOK).await;
    }
}

async fn read_window(st: &State, bus: &Bus, pid: u32, args: &Value) -> Result<(Value, Look), String> {
    let title = args.get("title").and_then(Value::as_str);
    let win_id = args.get("window").and_then(Value::as_str).map_or_else(|| format!("atspi-{pid}"), str::to_string);
    let last = {
        let all = st.ids.lock().unwrap_or_else(|e| e.into_inner());
        all.get(&win_id).filter(|t| t.pid == pid && !t.frame.is_empty()).map(|t| (t.app.clone(), t.frame.clone()))
    };
    let mut picked = None;
    for app in registry::find(bus, &st.apps, pid).await {
        let last = last.as_ref().filter(|(a, _)| *a == app.bus).map(|(_, f)| f.as_str());
        if let Some(f) = pick_frame(&Reader::new(bus, &app.bus), &app.root, title, !app.exact, last).await {
            picked = Some((app, f));
            break;
        }
    }
    let (app, frame) = picked.ok_or(NO_TREE)?;
    let r = Reader::new(bus, &app.bus);
    let mut node = r.node(frame, 0, false).await.ok_or(NO_TREE)?;
    // The window's own name as read now, and the desktop's title when it is news since the last read.
    let prev = {
        let all = st.ids.lock().unwrap_or_else(|e| e.into_inner());
        all.get(&win_id).map(|t| t.title.clone()).unwrap_or_default()
    };
    let news = title.filter(|t| *t != prev);
    let own = node.name.clone();
    let seen = look(&mut node, Some(&own), news);
    let win = args.get("window").and_then(Value::as_str).map_or_else(|| format!("atspi-{pid}"), str::to_string);
    let pair = |k: &str| {
        let a = args.get(k).and_then(Value::as_array)?;
        let at = |i: usize| a.get(i).and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok());
        Some([at(0)?, at(1)?])
    };
    let [ox, oy] = pair("origin").unwrap_or([0, 0]);
    let [_, _, w, h] = node.rect.unwrap_or_default();
    let origin = pair("size").map_or(Origin::at(ox, oy), |size| Origin::at(ox, oy).scaled([w, h], size));
    let name = match args.get("app").and_then(Value::as_str) {
        Some(a) => a.to_string(),
        None => Some(r.name(&app.root).await).filter(|n| !n.is_empty()).unwrap_or_else(|| comm(pid)),
    };
    let mut all = st.ids.lock().unwrap_or_else(|e| e.into_inner());
    let ids = all.entry(win.clone()).or_default();
    ids.for_app(&app.bus, pid);
    ids.frame = node.path.clone();
    ids.title = node.name.clone();
    let mut cx = Ctx::new(&win, origin, ids, atspi::has(node.states, state::VISIBLE));
    let elements: Vec<Value> = node.children.iter().flat_map(|c| map::elements(c, &mut cx, &node.path, node.has_iface("Selection"))).collect();
    let mut out = Map::new();
    out.insert("window".into(), json!(win));
    out.insert("app".into(), json!(name));
    out.insert("title".into(), json!(node.name));
    out.insert("pid".into(), json!(pid));
    out.insert("source".into(), json!("atspi"));
    out.insert("focused".into(), json!(atspi::has(node.states, state::ACTIVE)));
    out.insert("rect".into(), json!(origin.place([0, 0, w, h])));
    out.insert("elements".into(), Value::Array(elements));
    if r.truncated() {
        out.insert("truncated".into(), json!(true));
    }
    Ok((Value::Object(out), seen))
}

/// The program, the object and its parent an element id of window `pid` stands for.
struct Located {
    app: registry::App,
    path: String,
    parent: Option<String>,
    /// The window the element was read in.
    frame: String,
}

async fn locate(st: &State, bus: &Bus, pid: u32, args: &Value) -> Result<Located, String> {
    let element = args.get("element").and_then(Value::as_str).ok_or("Es fehlt, welches Element")?;
    let (win, n) = ids::split(element).ok_or("Diese Element-Nummer kennt Durchblick nicht")?;
    let apps = registry::find(bus, &st.apps, pid).await;
    let (app, path, parent, frame) = {
        let all = st.ids.lock().unwrap_or_else(|e| e.into_inner());
        // Only the program this window was read from, for the same process: an id never reaches another program.
        let ids = all.get(win).filter(|t| t.pid == pid).ok_or("Dieses Fenster hat Durchblick noch nicht gelesen")?;
        let app = apps.into_iter().find(|a| a.bus == ids.app).ok_or(NO_TREE)?;
        let path = ids.path(n).ok_or("Diese Element-Nummer kennt Durchblick nicht")?.to_string();
        let parent = ids.parent(&path).map(str::to_string);
        (app, path, parent, ids.frame.clone())
    };
    Ok(Located { app, path, parent, frame })
}

async fn act(st: &State, bus: &Bus, pid: u32, args: &Value) -> crate::act::Done {
    let action = args.get("action").and_then(Value::as_str).ok_or("Es fehlt, was getan werden soll")?;
    let text = args.get("text").and_then(Value::as_str);
    let l = locate(st, bus, pid, args).await?;
    crate::act::act(bus, &l.app.bus, &l.path, l.parent.as_deref(), &l.frame, action, text).await
}

/// EINGABE2: the field is still the one editable element of its window with the focus (`focus::proof`).
async fn focused(st: &State, bus: &Bus, pid: u32, args: &Value) -> Result<bool, String> {
    let l = locate(st, bus, pid, args).await?;
    Ok(crate::focus::proof(&Reader::new(bus, &l.app.bus), &l.frame, &l.path).await == crate::focus::Proof::Only)
}

async fn select(st: &State, bus: &Bus, pid: u32, args: &Value) -> Result<bool, String> {
    let l = locate(st, bus, pid, args).await?;
    crate::act::select_all(bus, &l.app.bus, &l.path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::Node;

    fn doc(path: &str, name: &str, busy: bool) -> Node {
        let mut states = 1 << state::SHOWING;
        if busy {
            states |= 1 << state::BUSY;
        }
        Node { path: path.into(), role: atspi::role::DOCUMENT_WEB, name: name.into(), states, ..Node::default() }
    }

    #[test]
    fn a_window_settles_when_title_and_page_agree_and_nothing_changes() {
        let mut w = Node { path: "/f".into(), role: atspi::role::FRAME, children: vec![doc("/old", "Neuer Tab", false)], ..Node::default() };
        let l = look(&mut w, Some("Google - Brave"), None);
        assert_eq!(l, Look { busy: false, mismatch: true }, "the Mini-PC's case: title new, page old");
        assert!(look(&mut w, Some("Neuer Tab - Brave"), Some("Google - Brave")).mismatch, "the window's name not yet new, the desktop's title is");
        assert!(!look(&mut w, Some("Neuer Tab - Brave"), None).mismatch, "an old desktop title is no news");
        assert!(!settled(l, false, true));
        let mut w = Node { path: "/f".into(), role: atspi::role::FRAME, children: vec![doc("/old", "Neuer Tab", false), doc("/new", "Google", false)], ..Node::default() };
        let l = look(&mut w, Some("Google - Brave"), None);
        assert_eq!((l, w.children.len(), w.children[0].path.as_str()), (Look::default(), 1, "/new"), "the old page beside the new one is dropped");
        let mut w = Node { path: "/f".into(), role: atspi::role::FRAME, children: vec![doc("/new", "Google", true)], ..Node::default() };
        assert!(look(&mut w, Some("Google - Brave"), None).busy, "still loading");
        let mut w = Node { path: "/f".into(), role: atspi::role::FRAME, children: vec![doc("/d", "", false)], ..Node::default() };
        assert_eq!(look(&mut w, Some("Irgendwas"), Some("Anders")), Look::default(), "a page without a name: the title proves nothing");
        assert!(settled(Look::default(), false, false), "a plain list needs no second read");
        assert!(!settled(Look::default(), true, false) && settled(Look::default(), true, true), "after a step: two equal reads");
    }
}
