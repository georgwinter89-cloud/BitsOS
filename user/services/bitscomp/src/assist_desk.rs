//! EINS-B (PLAN K3): the desktop tools of every chat, run by bitscomp for `assist serve` (`desk{call_id, name,
//! args}` in, `desk_result{call_id, content, is_error, image?}` out): `fenster` lists the windows, `sehen` gives
//! one window's elements (and with `bild` its picture), `foto` a picture of a window or the screen, `handeln` one
//! step (`db_act_app`, the gate). A window of a never-see program is named in the list only by its program and
//! nothing else of it goes out; the screen's picture is refused while one is on it. Secrets never leave
//! (`db_question`: the list never holds a secret value).

use mcpreg::Json;
use serde_json::Value;

use crate::assist_level as lv;
use crate::db_question as q;
use crate::layout::Rect;
use crate::state::App;
use crate::ui_reg::Full;

pub const NO_WINDOW: &str = "Ein solches Fenster gibt es nicht; „fenster“ zeigt, welche es gibt.";
pub const SCREEN_HIDDEN: &str = "Auf dem Bildschirm ist ein Fenster, das der Assistent nicht sieht; ein Bild des ganzen Bildschirms gibt es darum nicht. Ein einzelnes Fenster geht.";

/// The text of `fenster`: one line per window, the one in front marked.
pub fn window_lines(list: &[uilist::Window], front: &str, hidden: &dyn Fn(&str) -> bool) -> String {
    let mut out = String::new();
    for w in list {
        let mark = if w.window == front { " (vorne)" } else { "" };
        if hidden(&w.app) {
            out.push_str(&format!("- {} · {} ({}){mark}\n", w.window, w.app, lv::NOT_SEEN));
        } else {
            out.push_str(&format!("- {} · {} · „{}“{mark}\n", w.window, w.app, w.title));
        }
    }
    if out.is_empty() { "Kein Fenster offen.".into() } else { out }
}

/// A window picture is a cut of the screen: what lies over the window is in it too.
pub const COVERED: &str = "Über dem Fenster liegt eines, das der Assistent nicht sieht; ein Bild gibt es darum nicht.";

fn overlaps(a: [i32; 4], b: [i32; 4]) -> bool {
    a[2] > 0 && a[3] > 0 && b[2] > 0 && b[3] > 0 && a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

impl App {
    /// Whether a window of a never-see program other than `own` lies in `rect` on the screen (its pixels would
    /// be in a cut of `rect`); a minimised one has no place on the screen.
    pub(crate) fn never_seen_in(&self, rect: [i32; 4], own: &str) -> bool {
        let v = &self.settings;
        if lv::never_list(v).is_empty() {
            return false;
        }
        self.ui_windows(&Full::One(String::new())).iter().any(|w| w.window != own && ![crate::ui_reg::DESKTOP, crate::ui_reg::SOFA].contains(&w.window.as_str()) && lv::never_sees(v, &w.app) && overlaps(w.rect, rect))
    }
}

fn to_value(j: &Json) -> Value {
    serde_json::from_str(&j.to_string()).unwrap_or(Value::Null)
}

impl App {
    /// A desktop tool's call of chat `chat`.
    pub(crate) fn desk_call(&mut self, chat: &str, j: &Json) {
        // The engine numbers its calls (EINS-A); a view's fake may name them: both come back as they came.
        let call = j.get("call_id").and_then(|c| c.as_str().map(str::to_string).or_else(|| c.as_u64().map(|n| n.to_string()))).unwrap_or_default();
        let name = j.get("name").and_then(Json::as_str).unwrap_or("");
        let args = j.get("args").map(to_value).unwrap_or(Value::Null);
        crate::db_app::journal(&format!("bitscomp assist.desk chat={chat} call={call} name={name}"));
        match name {
            "fenster" => {
                let list = self.ui_windows(&Full::Focused);
                let front = self.ui_focused_id();
                let v = &self.settings;
                let text = window_lines(&list, &front, &|app| lv::never_sees(v, app));
                self.desk_result(&call, Ok(text), None);
            }
            "sehen" => self.desk_see(&call, &args),
            "foto" => self.desk_photo(&call, &args),
            crate::db_act_app::ACT_TOOL => self.db_act_call(chat, &call, &args),
            other => self.desk_result(&call, Err(format!("Das Werkzeug „{other}“ gibt es auf dem Schreibtisch nicht.")), None),
        }
    }

    /// The window a tool names (`fenster`), else the one in front.
    fn desk_window(&self, args: &Value) -> Result<uilist::Window, String> {
        let id = args.get("fenster").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map_or_else(|| self.ui_focused_id(), str::to_string);
        let w = self.ui_windows(&Full::One(id.clone())).into_iter().find(|w| w.window == id).ok_or_else(|| NO_WINDOW.to_string())?;
        if lv::never_sees(&self.settings, &w.app) {
            return Err(format!("{}. Es wurde nichts gelesen.", lv::NOT_SEEN));
        }
        Ok(w)
    }

    fn desk_see(&mut self, call: &str, args: &Value) {
        let w = match self.desk_window(args) {
            Ok(w) => w,
            Err(e) => return self.desk_result(call, Err(e), None),
        };
        let picture = args.get("bild").and_then(Value::as_bool).unwrap_or(false);
        // A foreign program is read again from its provider first (D, EINGABE2: never from a stale cache).
        let (c, id) = (call.to_string(), w.window.clone());
        if self.ui_fetch_then(&Full::One(id.clone()), move || crate::ui_fetch::Then::Desk(c, id, picture)) {
            return;
        }
        self.desk_see_ready(call, &w.window, picture);
    }

    /// `sehen`'s window was read: its list (and its picture) go to the model.
    pub(crate) fn desk_see_ready(&mut self, call: &str, window: &str, picture: bool) {
        let w = match self.desk_window(&serde_json::json!({ "fenster": window })) {
            Ok(w) => w,
            Err(e) => return self.desk_result(call, Err(e), None),
        };
        let mut text = q::build(&q::Inputs { window: &w, pointed: None, access: None, picture }).list;
        if self.ui_fetch.loading(&w.window) {
            text = format!("{}\n{text}", crate::db_act_app::LOADING_LIST);
        }
        crate::db_app::journal(&format!("bitscomp assist.tool name=sehen window={} bytes={} picture={}", w.window, text.len(), if picture { "yes" } else { "no" }));
        if picture && self.never_seen_in(w.rect, &w.window) {
            return self.desk_result(call, Ok(format!("{text}\n{COVERED}")), None);
        }
        if picture {
            return self.desk_shot(call, self.ui_rect(w.rect), text);
        }
        self.desk_result(call, Ok(text), None);
    }

    fn desk_photo(&mut self, call: &str, args: &Value) {
        if args.get("fenster").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()) {
            return match self.desk_window(args) {
                Ok(w) if self.never_seen_in(w.rect, &w.window) => self.desk_result(call, Err(COVERED.into()), None),
                Ok(w) => {
                    let text = format!("Ein Bild des Fensters {} von „{}“ liegt bei.", w.window, w.app);
                    self.desk_shot(call, self.ui_rect(w.rect), text)
                }
                Err(e) => self.desk_result(call, Err(e), None),
            };
        }
        let v = &self.settings;
        if self.ui_windows(&Full::Focused).iter().any(|w| lv::never_sees(v, &w.app)) {
            return self.desk_result(call, Err(SCREEN_HIDDEN.into()), None);
        }
        let s = self.theme.scale;
        let full = self.area_full();
        let r = Rect { x: (full.x as f64 * s) as i32, y: (full.y as f64 * s) as i32, w: (full.w as f64 * s) as i32, h: (full.h as f64 * s) as i32 };
        self.desk_shot(call, r, "Ein Bild des ganzen Bildschirms liegt bei.".into());
    }

    fn ui_rect(&self, r: [i32; 4]) -> Rect {
        Rect { x: r[0], y: r[1], w: r[2], h: r[3] }
    }

    /// A picture of `rect` (physical px) from the next frame, for call `call` (`db_shot::take`).
    fn desk_shot(&mut self, call: &str, rect: Rect, text: String) {
        if rect.w <= 0 || rect.h <= 0 {
            return self.desk_result(call, Ok(format!("{text} (Das Fenster hat gerade keine Fläche auf dem Bildschirm; es gibt kein Bild.)")), None);
        }
        self.assist_route.shots.push(crate::assist_route::Shot { call: call.into(), rect, text, asked: std::time::Instant::now(), taken: false });
        self.needs_frame = true;
    }

    /// The picture for call `call` was made (None: none could be).
    pub(crate) fn desk_shot_done(&mut self, call: &str, shot: Option<crate::db_shot::Shot>) {
        let text = self.assist_route.shots.iter().position(|s| s.call == call).map(|i| self.assist_route.shots.remove(i).text).unwrap_or_default();
        match shot {
            Some(s) => {
                crate::db_app::journal(&format!("bitscomp assist.tool name=foto call={call} bytes={}", s.png.len()));
                self.desk_result(call, Ok(text), Some(crate::db_shot::base64(&s.png)));
            }
            None => self.desk_result(call, Err("Es ließ sich gerade kein Bild machen.".into()), None),
        }
    }

    /// K1 `desk_result`; a picture goes as `image` beside the text (eins-b.md, change request 2).
    pub(crate) fn desk_result(&mut self, call: &str, r: Result<String, String>, image: Option<String>) {
        let (content, error) = match r {
            Ok(t) => (t, false),
            Err(t) => (t, true),
        };
        let id = call.parse::<u64>().map_or_else(|_| Json::str(call), |n| Json::Num(n as f64));
        let mut v = vec![("op", Json::str("desk_result")), ("call_id", id), ("content", Json::Str(content))];
        if error {
            v.push(("is_error", Json::Bool(true)));
        }
        if let Some(data) = image {
            v.push(("image", Json::obj(vec![("media_type", Json::str("image/png")), ("data", Json::Str(data))])));
        }
        self.assist_out(vec![Json::obj(v).to_string()], crate::assist_route::NewFor::Sidebar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_list_names_a_hidden_program_only() {
        let w = |id: &str, app: &str, title: &str| uilist::Window { window: id.into(), app: app.into(), title: title.into(), pid: 1, source: uilist::Source::Client, focused: false, rect: [0, 0, 10, 10], elements: Vec::new() };
        let list = [w("w1", "brave", "Bank – Konto 1234"), w("w2", "keepassxc", "Tresor.kdbx")];
        let text = window_lines(&list, "w1", &|a| a == "keepassxc");
        assert!(text.contains("- w1 · brave · „Bank – Konto 1234“ (vorne)"), "{text}");
        assert!(text.contains("- w2 · keepassxc (Dieses Programm sieht der Assistent nicht)") && !text.contains("Tresor"), "{text}");
        assert_eq!(window_lines(&[], "", &|_| false), "Kein Fenster offen.");
    }

    /// INT-EINS1 (gm/62): the engine numbers its calls; the result carries the same number back.
    #[test]
    fn a_numbered_call_gets_its_number_back() {
        let (_d, mut a, mut f) = crate::assist_fake::app();
        a.assist_msg(crate::assist_client::Msg::Line(Json::parse(r#"{"ev":"desk","chat":"c1","id":1,"call_id":7,"name":"fenster","args":{},"level":"ask","by":"hand"}"#).unwrap()));
        let r = f.got(&mut a, "desk_result");
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].get("call_id").and_then(Json::as_u64), Some(7), "{:?}", r[0]);
    }

    #[test]
    fn a_window_over_another_is_in_its_cut() {
        let a = [100, 100, 400, 300];
        assert!(overlaps(a, [450, 350, 100, 100]), "a corner over it");
        assert!(overlaps(a, [0, 0, 2000, 1000]), "all over it");
        assert!(!overlaps(a, [500, 100, 100, 100]), "beside it, edge to edge");
        assert!(!overlaps(a, [100, 400, 400, 10]), "under it, edge to edge");
        assert!(!overlaps(a, [0, 0, 0, 0]), "minimised: no place on the screen");
    }

    #[test]
    fn no_never_see_window_nothing_is_covered() {
        let d = smithay::reexports::wayland_server::Display::<App>::new().unwrap();
        let mut a = App::new(d.handle(), "desk-test".into(), (2000, 1125));
        assert!(!a.never_seen_in([0, 0, 2000, 1125], "w1"));
        // The desktop's own layer is the screen, not a window over another.
        a.settings.set(lv::NEVER_SEE, "bitscomp");
        assert!(!a.never_seen_in([0, 0, 2000, 1125], "w1"));
    }
}
