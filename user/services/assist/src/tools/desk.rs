//! The desktop tools (plan/spec/assistent-eins K3): `fenster`, `sehen`, `handeln`, `foto`. The service does not
//! run them itself: it hands each call to the view that shows the chat (`desk{call_id,name,args,level,by}`) and
//! waits for its `desk_result`. bitscomp stays the gate for `handeln` (element proof, `limits()`, `classify()`,
//! the level, the bar while it acts, Esc); the engine passes the level along and never asks about a step.

use super::permit::{ACT_TOOL, Level};
use super::{Class, Grant, ToolAnswer, ToolSource, ToolSpec};
use crate::conv::Image;
use crate::json::{self, Json};
use std::cell::RefCell;

pub const WINDOWS: &str = "fenster";
pub const LOOK: &str = "sehen";
pub const ACT: &str = ACT_TOOL;
pub const PHOTO: &str = "foto";

/// What the view answered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeskAnswer {
    pub content: String,
    pub is_error: bool,
    pub image: Option<Image>,
}

/// Where the calls go: the view of the chat (serve's connection to it).
pub trait Desk {
    /// Runs one call at the view and waits for its result. `stop` turns true when Georg stopped the question.
    fn run(&self, name: &str, args: &Json, level: Level, stop: &dyn Fn() -> bool) -> DeskAnswer;
}

fn spec(name: &str, class: Class, description: &str, schema: &str) -> ToolSpec {
    ToolSpec { name: name.into(), description: description.into(), schema: json::parse(schema).unwrap_or_else(|_| Json::obj()), class }
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(WINDOWS, Class::Read, "Lists the windows on the screen: id, program, title, and which one is in front.", r#"{"type":"object","properties":{},"additionalProperties":false}"#),
        spec(
            LOOK,
            Class::Read,
            "A window's elements as a list (one line each: role, name, value, state, place, and its id after #). picture: true also brings a picture of the window.",
            r#"{"type":"object","properties":{"window":{"type":"string","description":"a window id from fenster, like w3; default: the window in front"},"picture":{"type":"boolean"}},"additionalProperties":false}"#,
        ),
        spec(
            ACT,
            Class::Change,
            "Does ONE step on an element of a window's list: press, type (replaces the field's whole content), choose, toggle, scroll, focus, abschicken (Enter in that field). BitsOS shows the step, decides under the level (it may ask Georg or refuse) and answers with the window as it is afterwards.",
            r#"{"type":"object","properties":{"element":{"type":"string","description":"an element id of the list without the hash, like w3/1a2b"},"aktion":{"type":"string","enum":["press","type","choose","toggle","scroll","focus","abschicken","submit"]},"text":{"type":"string"},"name":{"type":"string","description":"the element's name as the list shows it"},"unumkehrbar":{"type":"boolean","description":"true when the step buys, pays, sends, publishes, deletes, submits a form, accepts terms or changes an account"},"warum":{"type":"string","description":"one short German sentence"}},"required":["element","aktion","name","warum"],"additionalProperties":false}"#,
        ),
        spec(PHOTO, Class::Read, "A picture of the whole screen, or of one window.", r#"{"type":"object","properties":{"window":{"type":"string"}},"additionalProperties":false}"#),
    ]
}

/// What a call does, in Georg's words. None: not a desktop tool.
pub fn describe(name: &str, args: &Json) -> Option<String> {
    let t = |k: &str| crate::term::wrap::inline(args.str(k).unwrap_or("").trim(), 200);
    Some(match name {
        WINDOWS => "Die Fenster auflisten".into(),
        LOOK if args.get("picture").and_then(Json::as_bool) == Some(true) => format!("Ein Fenster ansehen, mit Bild: {}", if t("window").is_empty() { "das vorne".into() } else { t("window") }),
        LOOK => format!("Ein Fenster ansehen: {}", if t("window").is_empty() { "das vorne".into() } else { t("window") }),
        ACT => format!("Im Fenster: {} „{}“ ({}){}", t("aktion"), t("name"), t("element"), args.str("text").map(|_| format!(": „{}“", t("text"))).unwrap_or_default()),
        PHOTO if t("window").is_empty() => "Ein Foto vom Bildschirm".into(),
        PHOTO => format!("Ein Foto vom Fenster {}", t("window")),
        _ => return None,
    })
}

pub struct DeskTools<'d> {
    desk: &'d dyn Desk,
    pictures: RefCell<Vec<Image>>,
}

impl<'d> DeskTools<'d> {
    pub fn new(desk: &'d dyn Desk) -> DeskTools<'d> {
        DeskTools { desk, pictures: RefCell::new(Vec::new()) }
    }
    pub fn has(name: &str) -> bool {
        [WINDOWS, LOOK, ACT, PHOTO].contains(&name)
    }
}

impl ToolSource for DeskTools<'_> {
    fn list(&self) -> Vec<ToolSpec> {
        specs()
    }
    fn call(&self, name: &str, args: &Json, grant: Grant) -> ToolAnswer {
        self.call_with(name, args, grant, &|| false)
    }
    fn call_with(&self, name: &str, args: &Json, grant: Grant, stop: &dyn Fn() -> bool) -> ToolAnswer {
        let a = self.desk.run(name, args, grant.level, stop);
        if let Some(i) = a.image {
            self.pictures.borrow_mut().push(i);
        }
        if a.is_error { Err(a.content) } else { Ok(a.content) }
    }
    fn take_pictures(&self) -> Vec<Image> {
        std::mem::take(&mut *self.pictures.borrow_mut())
    }
}
