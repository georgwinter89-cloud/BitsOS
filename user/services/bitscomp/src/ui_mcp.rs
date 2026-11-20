//! DB1: bitscomp's MCP tools `ui_list` (read) and `ui_act` (change) over the desktop list. Both answer only for a
//! call the kernel's gate runs for a client with a live full-access grant (MC-10, Georg's release at the keyboard,
//! Strg+Alt+V): the gate marks such a call (`mcpreg::Call::full_access`, `_meta.access`). Every other call (the
//! token-only kernel server, a narrow TLS client, a local program over the tools socket) gets a German refusal and
//! nothing is read. Acting is the same `ui-act` as the control socket's (injected input: confirm dialogs come,
//! dialogs that count only real input refuse it), except that the button answering a question is never pressed.

use mcpreg::{Json, Reply, Safety, Tool};

use crate::mcp::Ask;
use crate::state::App;
use crate::ui_reg::Full;

pub const REFUSAL: &str = "Die Oberfläche als Liste gibt es über MCP nur, solange Georg an der Tastatur den Vollzugriff erlaubt hat (Strg+Alt+V am BitsOS-Rechner). Gerade ist er nicht erlaubt; es wurde nichts gelesen und nichts getan.";

pub fn list_tool() -> Result<Tool, mcpreg::Error> {
    Tool::new(
        "ui_list",
        "The desktop as a list (Durchblick): windows with their elements (role, name, value, state, rect in screen \
         pixels, actions), as JSON. Without `window`: every window, elements only for the focused one; with \
         `window` (an id from the list, or \"focus\"): that window with all its elements. Secret fields never carry \
         a value. Answers only while Georg has granted full access at the keyboard (MC-10).",
        r#"{"type":"object","properties":{"window":{"type":"string","minLength":1,"maxLength":64,"description":"window id (desktop, sofa, w12) or focus"}},"additionalProperties":false}"#,
        Safety::Read,
    )
}

pub fn act_tool() -> Result<Tool, mcpreg::Error> {
    Tool::new(
        "ui_act",
        "Does an action on one element of ui_list as a real click or key would (press, type, choose, toggle, scroll, \
         focus, submit: Enter in a foreign program's field, nothing else); a change that asks first still asks, and the button that answers such a question (answers: true) \
         is refused: Georg answers it himself. Answers ok or a German reason. Only while Georg has granted \
         full access at the keyboard (MC-10).",
        r#"{"type":"object","properties":{"element":{"type":"string","minLength":3,"maxLength":128,"description":"element id from ui_list"},"action":{"type":"string","enum":["press","type","choose","toggle","scroll","focus","submit"]},"text":{"type":"string","maxLength":2000,"description":"what to type (replaces the field's text), or wheel steps for scroll"}},"required":["element","action"],"additionalProperties":false}"#,
        Safety::Change,
    )
}

/// The refusal for a DB1 tool called without the grant's mark; None for other tools and granted calls.
pub fn refused(tool: &str, full_access: bool) -> Option<Reply> {
    (matches!(tool, "ui_list" | "ui_act") && !full_access).then(|| Reply::error(REFUSAL))
}

pub fn ask_of(name: &str, args: &Json) -> Result<Ask, Reply> {
    let text = |k: &str| args.get(k).and_then(Json::as_str).map(str::to_string);
    match name {
        "ui_list" => match args.get("window") {
            None => Ok(Ask::UiList(None)),
            Some(w) => match w.as_str().filter(|w| !w.is_empty() && w.len() <= 64 && !w.contains(char::is_whitespace)) {
                Some(w) => Ok(Ask::UiList(Some(w.to_string()))),
                None => Err(Reply::bad_argument("window must be a window id from ui_list (desktop, sofa, w12) or focus")),
            },
        },
        _ => {
            let element = text("element").map(|e| uilist::clean_id(&e).to_string()).filter(|e| uilist::window_of(e).is_some() && !e.contains(char::is_whitespace));
            let action = text("action").filter(|a| uilist::Action::of_word(a).is_some());
            match (element, action) {
                (Some(element), Some(action)) => Ok(Ask::UiAct { element, action, text: text("text") }),
                (None, _) => Err(Reply::bad_argument("element must be an element id from ui_list (like desktop/1a2b)")),
                (_, None) => Err(Reply::bad_argument("action must be one of press, type, choose, toggle, scroll, focus, submit")),
            }
        }
    }
}

impl App {
    pub(crate) fn ui_mcp_list(&mut self, window: Option<String>) -> Reply {
        let full = match window.as_deref() {
            None => Full::Focused,
            Some("focus") => Full::One(self.ui_focused_id()),
            Some(w) => Full::One(w.to_string()),
        };
        let mut list = self.ui_windows(&full);
        if let Full::One(w) = &full {
            list.retain(|x| &x.window == w);
            if list.is_empty() {
                return Reply::bad_argument(format!("Ein Fenster {w} gibt es nicht (ui_list ohne window zeigt alle)."));
            }
        }
        match serde_json::to_string(&list) {
            Ok(t) => Reply::text(t),
            Err(e) => Reply::error(e.to_string()),
        }
    }

    pub(crate) fn ui_mcp_act(&mut self, element: &str, action: &str, text: Option<&str>) -> Reply {
        let before = self.ctl_origin;
        self.ctl_origin = crate::input::Origin::Injected;
        let r = self.ui_act_by(element, action, text, crate::ui_act::By::Mcp);
        self.ctl_origin = before;
        match r {
            Ok(t) => Reply::text(t),
            Err(e) => Reply::error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_granted_call_gets_the_list() {
        assert!(matches!(refused("ui_list", false), Some(Reply::Error(t)) if t.contains("Strg+Alt+V")));
        assert!(refused("ui_act", false).is_some());
        assert!(refused("ui_list", true).is_none() && refused("ui_act", true).is_none());
        assert!(refused("windows", false).is_none(), "other tools keep their rules");
    }

    #[test]
    fn arguments_are_checked() {
        let a = |s: &str| Json::parse(s).unwrap();
        assert!(matches!(ask_of("ui_list", &a("{}")), Ok(Ask::UiList(None))));
        assert!(matches!(ask_of("ui_list", &a(r#"{"window":"w3"}"#)), Ok(Ask::UiList(Some(w))) if w == "w3"));
        for bad in [r#"{"window":""}"#, r#"{"window":7}"#, r#"{"window":"a b"}"#] {
            assert!(matches!(ask_of("ui_list", &a(bad)), Err(Reply::BadArgument(_))), "{bad}");
        }
        assert!(matches!(ask_of("ui_act", &a(r#"{"element":"desktop/1a","action":"press"}"#)), Ok(Ask::UiAct { text: None, .. })));
        assert!(matches!(ask_of("ui_act", &a(r#"{"element":"w2/9","action":"type","text":"hallo welt"}"#)), Ok(Ask::UiAct { text: Some(t), .. }) if t == "hallo welt"));
        for bad in [r#"{"element":"nope","action":"press"}"#, r#"{"element":"desktop/1","action":"klick"}"#, r#"{"action":"press"}"#] {
            assert!(matches!(ask_of("ui_act", &a(bad)), Err(Reply::BadArgument(_))), "{bad}");
        }
    }
}
