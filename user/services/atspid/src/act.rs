//! `act`: doing what a C1 action names through AT-SPI's Action, EditableText, Selection, Value and Component.
//! Only an action the element offers in its C1 list is done: Durchblick asks Georg by the action it requests, so a
//! harmless-sounding one (toggle, choose) must never end up as a press on some other kind of element.
//! Errors are German sentences for Georg's screen.

use crate::atspi::{self, Bus, role};
use crate::map;
use crate::read::Reader;
use zbus::Variant;

/// The action names programs use for a plain press, in the order they are tried.
const PRESS: &[&str] = &["click", "press", "activate", "jump", "open", "select", "showmenu"];
const TOGGLE: &[&str] = &["toggle", "check", "uncheck", "click", "press", "activate"];
/// Roles whose option `choose` may press when its parent cannot select it (a menu's or a combo popup's item).
const OPTION: &[u32] = &[role::LIST_ITEM, role::TREE_ITEM, role::TABLE_CELL, role::MENU_ITEM, role::CHECK_MENU_ITEM, role::RADIO_MENU_ITEM];

/// How an action was done: by the program itself, or (EINGABE) the field has the focus now and the desktop
/// writes the text (the program offers no EditableText, as Chromium's fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acted {
    Done,
    ByDesktop,
}

pub type Done = Result<Acted, String>;

fn no<T>(what: &str) -> Result<T, String> {
    Err(what.to_string())
}

/// How long the field may take to be the one with the keyboard after GrabFocus (Chromium moves it asynchronously).
const FOCUS_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
/// After this long with another field still saying FOCUSED, GrabFocus once more (what moved Brave's keyboard out
/// of its address bar into the page on the Mini-PC: a second GrabFocus, as the explicit `focus` step did).
const REGRAB: std::time::Duration = std::time::Duration::from_millis(500);
/// Between two looks at the window.
const LOOK: std::time::Duration = std::time::Duration::from_millis(50);

fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The program's action named first in `prefer`; `first_else`: else its first action (a plain press, whatever
/// the program calls it).
async fn do_action(bus: &Bus, app: &str, path: &str, prefer: &[&str], first_else: bool) -> Done {
    let list: Vec<(String, String, String)> = bus.call(app, path, atspi::ACTION, "GetActions", &()).await.map_err(|_| "Dieses Element lässt sich nicht bedienen".to_string())?;
    if list.is_empty() {
        return no("Dieses Element lässt sich nicht bedienen");
    }
    let named = prefer.iter().find_map(|p| list.iter().position(|(n, _, _)| same(n, p)));
    let Some(i) = named.or(first_else.then_some(0)) else { return no("Das Programm bietet dafür keine passende Bedienung an") };
    let ok: bool = bus.call(app, path, atspi::ACTION, "DoAction", &(i as i32,)).await.map_err(|e| format!("Das Programm hat nicht reagiert ({e})"))?;
    if ok { Ok(Acted::Done) } else { no("Das Programm hat die Bedienung abgelehnt") }
}

async fn select_in(bus: &Bus, app: &str, parent: &str, index: i32) -> bool {
    bus.call::<_, bool>(app, parent, atspi::SELECTION, "SelectChild", &(index,)).await.unwrap_or(false)
}

/// Selects `path` in its parent (an option of a list or a menu). `press_else`: an option that its parent cannot
/// select is pressed instead (how a menu's or a combo popup's items are chosen).
async fn choose_item(bus: &Bus, app: &str, path: &str, parent: Option<&str>, press_else: bool) -> Done {
    let index: i32 = bus.call(app, path, atspi::ACCESSIBLE, "GetIndexInParent", &()).await.unwrap_or(-1);
    if let Some(p) = parent.filter(|_| index >= 0) {
        if select_in(bus, app, p, index).await {
            return Ok(Acted::Done);
        }
    }
    if press_else { do_action(bus, app, path, PRESS, false).await } else { no("Das Programm hat die Auswahl abgelehnt") }
}

/// The option named `want` below `path` (a choice's menu, a list), read even when it is not on screen.
async fn find_option(bus: &Bus, app: &str, path: &str, want: &str) -> Option<(String, String)> {
    let r = Reader::new(bus, app);
    let tree = r.node(path.to_string(), 0, true).await?;
    fn walk(n: &crate::read::Node, parent: &str, want: &str) -> Option<(String, String)> {
        if !n.name.is_empty() && same(&n.name, want) && OPTION.contains(&n.role) {
            return Some((n.path.clone(), parent.to_string()));
        }
        n.children.iter().find_map(|c| walk(c, &n.path, want))
    }
    tree.children.iter().find_map(|c| walk(c, path, want))
}

async fn selects(bus: &Bus, app: &str, path: Option<&str>) -> bool {
    let Some(p) = path else { return false };
    let ifaces: Vec<String> = bus.call(app, p, atspi::ACCESSIBLE, "GetInterfaces", &()).await.unwrap_or_default();
    ifaces.iter().any(|i| i == atspi::SELECTION)
}

/// Does `action` on the object `path` of program `app`. `parent`: its parent as last read; `frame`: its window.
pub async fn act(bus: &Bus, app: &str, path: &str, parent: Option<&str>, frame: &str, action: &str, text: Option<&str>) -> Done {
    let r = Reader::new(bus, app);
    let (n, parent_selects) = futures_util::join!(r.one(path), selects(bus, app, parent));
    let Some(n) = n else { return no("Dieses Element gibt es nicht mehr") };
    let offered = map::c1_role(&n).map(|c1| map::actions(&n, c1, parent_selects)).unwrap_or_default();
    // Scrolling an element into view presses nothing; every other action must be one the element offers.
    if action != "scroll" && !offered.contains(&action) {
        return Err(match offered.is_empty() {
            true => format!("„{action}“ geht hier nicht: dieses Element lässt sich nicht bedienen"),
            false => format!("„{action}“ geht bei diesem Element nicht (es kann: {})", offered.join(", ")),
        });
    }
    let can = |i: &str| n.ifaces.iter().any(|x| x == i);
    let role = n.role;
    match action {
        "press" => do_action(bus, app, path, PRESS, true).await,
        "toggle" => do_action(bus, app, path, TOGGLE, false).await,
        "type" => {
            let Some(t) = text else { return no("Zum Schreiben fehlt der Text") };
            if can(atspi::EDITABLE_TEXT) {
                let ok: bool = bus.call(app, path, atspi::EDITABLE_TEXT, "SetTextContents", &(t,)).await.map_err(|e| format!("Das Programm hat nicht reagiert ({e})"))?;
                return if ok { Ok(Acted::Done) } else { no("Das Programm hat den Text nicht angenommen") };
            }
            if !matches!(map::c1_role(&n), Some("field" | "choice")) || (role == atspi::role::SPIN_BUTTON && can(atspi::VALUE)) {
                let Ok(v) = t.trim().replace(',', ".").parse::<f64>() else { return no("Hier geht nur eine Zahl") };
                bus.call::<_, ()>(app, path, atspi::PROPERTIES, "Set", &(atspi::VALUE, "CurrentValue", Variant::from(v)))
                    .await
                    .map_err(|e| format!("Das Programm hat den Wert nicht angenommen ({e})"))?;
                return Ok(Acted::Done);
            }
            focus_for_desktop(bus, app, path, frame).await
        }
        // EINGABE2: Enter in exactly this field; the desktop presses it once the field has the keyboard.
        "submit" => focus_for_desktop(bus, app, path, frame).await,
        "choose" => match text.filter(|t| !t.trim().is_empty()) {
            None if OPTION.contains(&role) => choose_item(bus, app, path, parent, false).await,
            None => no("Zum Auswählen fehlt, was gewählt werden soll"),
            Some(want) => match find_option(bus, app, path, want).await {
                Some((opt, opt_parent)) => choose_item(bus, app, &opt, Some(&opt_parent), true).await,
                None => Err(format!("„{want}“ gibt es hier nicht zur Auswahl")),
            },
        },
        "focus" => {
            let ok: bool = bus.call(app, path, atspi::COMPONENT, "GrabFocus", &()).await.unwrap_or(false);
            if ok { Ok(Acted::Done) } else { no("Dieses Element kann nicht ausgewählt werden") }
        }
        "scroll" if !can(atspi::COMPONENT) => no("Dieses Element kann nicht ausgewählt werden"),
        "scroll" => {
            let ok: bool = bus.call(app, path, atspi::COMPONENT, "ScrollTo", &(atspi::SCROLL_ANYWHERE,)).await.unwrap_or(false);
            if ok { Ok(Acted::Done) } else { no("Das Programm kann nicht dorthin blättern") }
        }
        other => Err(format!("„{other}“ kann Durchblick hier nicht tun")),
    }
}

/// EINGABE2: the field gets the focus and is then the ONE editable element of its window that says so (`focus`);
/// GrabFocus is sent even to a field that says FOCUSED already (inside a web page that is only the page's focus).
/// One that does not get there is left alone, so the text never lands elsewhere.
async fn focus_for_desktop(bus: &Bus, app: &str, path: &str, frame: &str) -> Done {
    use crate::focus::Proof;
    let grab = || async { bus.call::<_, bool>(app, path, atspi::COMPONENT, "GrabFocus", &()).await.unwrap_or(false) };
    if !grab().await {
        return no("Das Feld lässt sich nicht auswählen; es wurde nichts geschrieben");
    }
    let start = std::time::Instant::now();
    let mut again = true;
    loop {
        let Ok(rt) = zruntime::SharedRuntime::current() else { return no("Das Feld hat die Eingabe nicht bekommen; es wurde nichts geschrieben") };
        rt.sleep(LOOK).await;
        let p = crate::focus::proof(&Reader::new(bus, app), frame, path).await;
        if p == Proof::Only {
            return Ok(Acted::ByDesktop);
        }
        if again && start.elapsed() >= REGRAB {
            again = false;
            grab().await;
        }
        if start.elapsed() >= FOCUS_WAIT {
            return Err(match p {
                Proof::Other(name) if !name.trim().is_empty() => format!("Die Tastatur ist noch in „{}“, nicht in diesem Feld; es wurde nichts geschrieben", name.trim()),
                Proof::Other(_) => "Die Tastatur ist noch in einem anderen Feld; es wurde nichts geschrieben".into(),
                Proof::Unknown => "Durchblick konnte nicht sicher sehen, welches Feld die Tastatur hat; es wurde nichts geschrieben".into(),
                _ => "Das Feld hat die Eingabe nicht bekommen; es wurde nichts geschrieben".into(),
            });
        }
    }
}

/// EINGABE2: all of the field's text selected through AT-SPI Text, and the selection read back (so the desktop's
/// text or paste replaces it). True also for an empty field (nothing to replace).
pub async fn select_all(bus: &Bus, app: &str, path: &str) -> Result<bool, String> {
    let n: i32 = bus.prop(app, path, atspi::TEXT, "CharacterCount").await.map_err(|_| "Das Feld zeigt seinen Text nicht".to_string())?;
    if n == 0 {
        return Ok(true);
    }
    let have: i32 = bus.call(app, path, atspi::TEXT, "GetNSelections", &()).await.unwrap_or(0);
    let set: bool = if have > 0 {
        bus.call(app, path, atspi::TEXT, "SetSelection", &(0i32, 0i32, n)).await.unwrap_or(false)
    } else {
        bus.call(app, path, atspi::TEXT, "AddSelection", &(0i32, n)).await.unwrap_or(false)
    };
    if !set {
        return Ok(false);
    }
    let got: (i32, i32) = bus.call(app, path, atspi::TEXT, "GetSelection", &(0i32,)).await.unwrap_or((0, 0));
    Ok(got == (0, n) || got == (n, 0))
}
