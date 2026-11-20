//! The requests of the service protocol v2 (plan/spec/assistent-eins K1) from a caller the gate admitted:
//! `status`, `chats`, `new`, `ask`, `answer`, `desk_result`, `cancel`, `history`, `rename`, `remove`. A request
//! of before K1 (no `chat`) means the sidebar's one chat of before, so a view of that time keeps working until
//! it is replaced.

use super::chats::title_of;
use super::events::{Level, q};
use super::out::Out;
use super::state::NotBegun;
use super::worker::{self, Job, Seen};
use super::{Ctx, history, proto, status};
use crate::conv::Image;
use crate::json::{self, Json};
use crate::prompt::Origin;
use crate::provider::program::pictures;
use crate::term::wrap;
use crate::tools::desk::DeskAnswer;
use crate::tools::permit::By;
use std::sync::Arc;

/// The most bytes of a window's element list and of the extras a question carries.
const CONTEXT_MAX: usize = 256 * 1024;

fn picture_of(j: &Json) -> Result<Option<Image>, String> {
    let Some(i) = j.get("image") else { return Ok(None) };
    let media_type = i.str("media_type").unwrap_or("");
    if !matches!(media_type, "image/png" | "image/jpeg") {
        return Err("Das Bild muss PNG oder JPEG sein.".into());
    }
    let data = i.str("data").unwrap_or("");
    if data.is_empty() || data.len() > pictures::PICTURE_MAX || !data.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')) {
        return Err("Das Bild ist kein gültiges Base64 in der erlaubten Größe.".into());
    }
    Ok(Some(Image { media_type: media_type.into(), base64: data.into() }))
}

/// K1 `context`: `{window:{id, app, title}, list?, image?, extras?}` as the words before the question.
pub fn seen(context: Option<&Json>) -> Result<Seen, String> {
    let Some(c) = context.filter(|c| matches!(c, Json::Obj(f) if !f.is_empty())) else { return Ok(Seen::default()) };
    let w = c.get("window");
    let field = |k: &str| w.and_then(|w| w.str(k)).map(|v| wrap::inline(v.trim(), 200)).unwrap_or_default();
    let mut text = String::new();
    if w.is_some() {
        text.push_str(&format!("The window in front: program {}, title \u{201e}{}\u{201c}, window id {}.", field("app"), field("title"), field("id")));
    }
    for (key, head) in [("list", "Its elements (one per line):"), ("extras", "More about it:")] {
        if let Some(t) = c.str(key).filter(|t| !t.trim().is_empty()) {
            if t.len() > CONTEXT_MAX {
                return Err(format!("Was zum Fenster mitgeschickt wurde ({key}), ist länger als {} KiB.", CONTEXT_MAX / 1024));
            }
            text.push_str(&format!("\n{head}\n{}", t.trim_end()));
        }
    }
    Ok(Seen { text: text.trim().to_string(), image: picture_of(c)? })
}

/// The chat a request names, or the sidebar's chat of before K1 (made when there is none yet).
fn chat_of(ctx: &Ctx, j: &Json) -> Result<String, Json> {
    if let Some(c) = j.get("chat") {
        let id = c.as_str().unwrap_or("");
        return match ctx.state.meta(id) {
            Some(_) => Ok(id.to_string()),
            None => Err(proto::err("no_chat", "Diesen Chat gibt es nicht (mehr).").with("chat", id)),
        };
    }
    if let Some(d) = ctx.state.default_chat() {
        return Ok(d);
    }
    let m = ctx.store.create(Origin::Sidebar, None, None).map_err(|e| proto::err("store", format!("Der Chat ließ sich nicht anlegen ({e}).")))?;
    let id = m.id.clone();
    ctx.state.add(m);
    ctx.state.set_default(&id);
    Ok(id)
}

pub fn request(ctx: &Arc<Ctx>, n: u64, out: &Arc<Out>, line: &str) -> Option<Json> {
    let j = match json::parse(line) {
        Ok(j @ Json::Obj(_)) => j,
        Ok(_) => return Some(proto::err("bad_request", "Eine Anfrage ist ein JSON-Objekt in einer Zeile.")),
        Err(_) => return Some(proto::err("bad_request", "Die Zeile ist kein gültiges JSON.")),
    };
    let Some(op) = j.str("op") else {
        return Some(proto::err("bad_request", "Der Anfrage fehlt „op“."));
    };
    let ev = &ctx.parts.events;
    let id = j.get("id").and_then(Json::as_i64);
    let chat = |j: &Json| chat_of(ctx, j);
    Some(match op {
        "status" => status::reply(ctx),
        "chats" => {
            let mut r = proto::ok(op).with("chats", ctx.state.list().iter().map(super::chats::Meta::json).collect::<Vec<Json>>());
            if ctx.skipped > 0 {
                r = r.with("notice", format!("{} Chat-Dateien ließen sich nicht lesen und fehlen in der Liste.", ctx.skipped));
            }
            r
        }
        "new" => {
            let origin = match j.str("origin") {
                None => Origin::Sidebar,
                Some(o) => match Origin::parse(o) {
                    Some(o) => o,
                    None => return Some(proto::err("bad_request", "„origin“ ist sidebar, popup oder console.").with("op", op)),
                },
            };
            match ctx.store.create(origin, j.str("app"), j.str("title")) {
                Ok(m) => {
                    let c = m.id.clone();
                    ctx.state.add(m);
                    // A view of before K1 says `new` for a fresh conversation in its one chat: the answer at work
                    // in the old one belongs to that one and stops, as it did then.
                    if j.get("origin").is_none()
                        && let Some(old) = ctx.state.default_chat()
                    {
                        ctx.state.cancel(Some(&old), None);
                    }
                    if j.get("origin").is_none() || origin == Origin::Sidebar {
                        ctx.state.set_default(&c);
                    }
                    ev.emit(Level::Info, "new", &format!("client={n} chat={c} origin={}", origin.name()));
                    proto::ok(op).with("chat", c)
                }
                Err(e) => proto::err("store", format!("Der Chat ließ sich nicht anlegen ({e}).")).with("op", op),
            }
        }
        "history" => match (j.get("chat").is_some(), chat(&j)) {
            (_, Err(e)) => e.with("op", op),
            (named, Ok(c)) => match ctx.store.load(&c) {
                Ok(l) if named => proto::ok(op).with("chat", c).with("turns", history::entries(&l)),
                Ok(l) => proto::ok(op).with("chat", c).with("turns", history::plain(&l)),
                Err(e) => proto::err("store", format!("Der Chat ließ sich nicht lesen ({e}).")).with("op", op).with("chat", c),
            },
        },
        "rename" => match (chat(&j), j.str("title").map(title_of).filter(|t| !t.is_empty())) {
            (Err(e), _) => e.with("op", op),
            (Ok(_), None) => proto::err("bad_request", "Ein neuer Name braucht „title“.").with("op", op),
            (Ok(c), Some(t)) => match ctx.store.add(&c, &super::chats::Record::Title(t.clone())) {
                Ok(()) => {
                    ctx.state.update(&c, |m| {
                        m.title = t.clone();
                        m.named = true;
                    });
                    proto::ok(op).with("chat", c).with("title", t)
                }
                Err(e) => proto::err("store", format!("Der Name ließ sich nicht speichern ({e}).")).with("op", op),
            },
        },
        "remove" => match j.get("chat").and_then(Json::as_str) {
            None => proto::err("bad_request", "Zum Entfernen fehlt „chat“.").with("op", op),
            Some(c) if ctx.state.meta(c).is_none() => proto::err("no_chat", "Diesen Chat gibt es nicht (mehr).").with("op", op).with("chat", c),
            Some(c) => {
                // A question at work there is stopped first: what it still writes then finds no file.
                let _ = ctx.state.cancel(Some(c), None);
                match ctx.store.remove(c) {
                    Ok(said) => {
                        ctx.state.remove(c);
                        ev.emit(Level::Info, "removed", &format!("client={n} chat={c} where={}", q(&said)));
                        proto::ok(op).with("chat", c)
                    }
                    Err(e) => proto::err("store", format!("Der Chat ließ sich nicht in den Papierkorb legen ({e}); er bleibt.")).with("op", op).with("chat", c),
                }
            }
        },
        "cancel" => {
            let named = j.get("chat").and_then(Json::as_str);
            if named.is_none() && id.is_none() {
                return Some(proto::err("bad_request", "Zum Abbrechen fehlt der Chat („chat“) oder die Nummer der Frage („id“).").with("op", op));
            }
            match ctx.state.cancel(named, id) {
                Some((c, i)) => proto::ok(op).with("chat", c).with("id", i),
                None => proto::err("not_running", "Diese Frage läuft nicht (mehr).").with("op", op).with("id", id),
            }
        }
        // Only a number and a plain true or false are an answer: nothing else is ever read as a yes.
        "answer" => match (j.get("ask_id").and_then(Json::as_i64).and_then(|m| u64::try_from(m).ok()), j.get("yes")) {
            (Some(m), Some(Json::Bool(yes))) if ctx.state.answer(m, *yes) => proto::ok(op).with("ask_id", m),
            (Some(m), Some(Json::Bool(_))) => proto::err("no_question", "Diese Rückfrage wartet nicht (mehr) auf eine Antwort.").with("op", op).with("ask_id", m),
            _ => proto::err("bad_request", "Eine Antwort braucht „ask_id“ (Zahl) und „yes“ (true oder false).").with("op", op),
        },
        "desk_result" => {
            let Some(call) = j.get("call_id").and_then(Json::as_i64).and_then(|c| u64::try_from(c).ok()) else {
                return Some(proto::err("bad_request", "Ein Ergebnis braucht „call_id“ (Zahl).").with("op", op));
            };
            let image = match picture_of(&j) {
                Ok(i) => i,
                Err(why) => return Some(proto::err("bad_request", why).with("op", op)),
            };
            let a = DeskAnswer { content: j.str("content").unwrap_or("").to_string(), is_error: j.get("is_error").and_then(Json::as_bool) == Some(true), image };
            if ctx.state.desk_result(call, a) { proto::ok(op).with("call_id", call) } else { proto::err("no_call", "Dieser Aufruf wartet nicht (mehr) auf ein Ergebnis.").with("op", op).with("call_id", call) }
        }
        "ask" => {
            let text = j.str("text").map(str::trim).unwrap_or("");
            let Some(id) = id.filter(|_| !text.is_empty()) else {
                return Some(proto::err("bad_request", "Eine Frage braucht „id“ (Zahl) und „text“.").with("op", op));
            };
            let Some(by) = By::parse(j.str("by")) else {
                return Some(proto::err("bad_request", "„by“ ist hand oder remote.").with("op", op).with("id", id));
            };
            let seen = match seen(j.get("context")) {
                Ok(s) => s,
                Err(why) => return Some(proto::err("bad_request", why).with("op", op).with("id", id)),
            };
            let c = match chat(&j) {
                Ok(c) => c,
                Err(e) => return Some(e.with("op", op).with("id", id)),
            };
            let b = match ctx.state.begin(&c, id, n) {
                Ok(b) => b,
                Err(NotBegun::Busy) => return Some(proto::err("busy", "Der Assistent arbeitet in diesem Chat noch an der letzten Frage.").with("op", op).with("id", id).with("chat", c)),
                Err(NotBegun::Full) => return Some(proto::err("busy", "Der Assistent arbeitet schon an drei Fragen. Warte, bis eine fertig ist.").with("op", op).with("id", id).with("chat", c)),
                Err(NotBegun::NoChat) => return Some(proto::err("no_chat", "Diesen Chat gibt es nicht (mehr).").with("op", op).with("id", id)),
            };
            let job = Job { chat: c.clone(), id, text: text.to_string(), seen, by, out: out.clone(), cancel: b.cancel, conv: b.conv };
            if worker::spawn(ctx, job) {
                return None;
            }
            ev.emit(Level::Error, "worker.thread", &format!("chat={c} id={id}"));
            proto::err("busy", "Für die Frage ließ sich gerade keine Arbeit beginnen. Stell sie gleich noch einmal.").with("op", op).with("id", id).with("chat", c)
        }
        other => proto::err("unknown_op", format!("Die Anfrage „{}“ gibt es nicht.", wrap::inline(other, 40))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_becomes_the_words_before_the_question() {
        let c = json::parse(r#"{"window":{"id":"w3","app":"brave","title":"Suchseite"},"list":"button \"Suchen\" #w3/1a\n","extras":"Georg tippte eben."}"#).unwrap();
        let s = seen(Some(&c)).unwrap();
        assert_eq!(s.text, "The window in front: program brave, title \u{201e}Suchseite\u{201c}, window id w3.\nIts elements (one per line):\nbutton \"Suchen\" #w3/1a\nMore about it:\nGeorg tippte eben.");
        assert!(s.image.is_none() && seen(None).unwrap().text.is_empty() && seen(Some(&Json::obj())).unwrap().text.is_empty());
        let bad = json::parse(r#"{"window":{"id":"w1"},"image":{"media_type":"image/gif","data":"R0lG"}}"#).unwrap();
        assert!(seen(Some(&bad)).is_err());
        let pic = json::parse(r#"{"window":{"id":"w1"},"image":{"media_type":"image/png","data":"iVBORw0KGgo="}}"#).unwrap();
        assert_eq!(seen(Some(&pic)).unwrap().image.map(|i| i.media_type), Some("image/png".into()));
        let long = Json::obj().with("list", "x".repeat(CONTEXT_MAX + 1));
        assert!(seen(Some(&long)).is_err());
    }
}
