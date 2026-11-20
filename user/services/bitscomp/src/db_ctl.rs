//! DB3, EINS-B: the popup on the control socket (scripts, the desk's checks): `durchblick open|close|state`,
//! `durchblick press <button>`, `durchblick ask <text>` (a question; empty: the default one). Everything here is
//! injected input: its question is marked as asked from outside (`by: remote`, every step asks) and a button that
//! says yes refuses it (only Georg's own hand).

use crate::db_layout::Button;
use crate::state::App;

fn px(app: &App, r: crate::layout::Rect) -> [i32; 4] {
    app.ui_px(r)
}

pub fn state(app: &App) -> serde_json::Value {
    let d = &app.db;
    let g = app.db_geo();
    let mut v = serde_json::json!({
        "open": d.open,
        "stage": d.stage.word(),
        "chat": d.chat,
        "window": d.target,
        "app": d.snapshot.as_ref().map(|w| w.app.clone()),
        "sees": app.db_sees(),
        "hidden": d.hidden,
        "source": d.snapshot.as_ref().map(|w| format!("{:?}", w.source).to_lowercase()),
        "pointed": d.pointed,
        "question": d.question,
        "remote": d.remote,
        "context_sent": d.context_sent,
        "picture": d.shot.is_some(),
        "text": d.shown,
        // What the popup's body says now (the answer, a step, its warning).
        "body": app.db_texts(),
        "tools": d.tools.iter().map(|(n, l, s)| serde_json::json!({"name": n, "line": l, "state": s.word()})).collect::<Vec<_>>(),
        "asking": d.asking.as_ref().map(|a| serde_json::json!({"ask_id": a.ask_id, "name": a.name, "text": a.text})),
        "ids": d.ids,
        "frames": app.db_frames(),
        "panel": g.as_ref().map(|g| px(app, g.panel)),
        "field_rect": g.as_ref().map(|g| px(app, g.field)),
        "buttons": g.as_ref().map(|g| g.buttons.iter().map(|(b, r)| serde_json::json!({"name": b.word(), "rect": px(app, *r)})).collect::<Vec<_>>()).unwrap_or_default(),
        "focus": d.focus.map(Button::word),
        "field": d.field.text,
        "note": app.db_note_shown(),
        "level": app.assist_level().word(),
        "cloud": app.assist_cloud_line(false),
        "act_note": app.act.note,
        "bar": app.db_bar().map(|(text, r)| serde_json::json!({"text": text, "rect": px(app, r)})),
        "message": match &d.stage { crate::db_app::Stage::Setup(m) | crate::db_app::Stage::Failed(m) => Some(m.clone()), _ => None },
    });
    v["task"] = task(app);
    v
}

/// The acting task and its step (`db_act_app`).
fn task(app: &App) -> serde_json::Value {
    serde_json::json!(app.act.task.as_ref().map(|t| serde_json::json!({
            "chat": app.act.chat,
            "n": t.n,
            "by_georg": t.by_georg,
            "steps": t.steps,
            "done": t.done,
            "stopped": t.stopped,
            "auto": app.db_auto(),
            "step": app.act.pending.as_ref().map(|p| serde_json::json!({
                "element": p.step.element,
                "action": crate::ui_act::words(p.step.action),
                "said": p.said,
                "final": matches!(p.risk, crate::db_act::Risk::Final(_)),
                "why": match p.risk { crate::db_act::Risk::Final(w) => w, crate::db_act::Risk::Normal => "" },
                "phase": format!("{:?}", p.phase).to_lowercase(),
            })),
        })))
}

/// A press from the socket is the socket's input, not Georg's: `input_origin` still holds his last real key or
/// mouse move (he just pressed Win+F1), so without this any program could say yes in his place.
fn as_ctl(app: &mut App, f: impl FnOnce(&mut App)) {
    let before = std::mem::replace(&mut app.input_origin, app.ctl_origin);
    f(app);
    app.input_origin = before;
}

pub fn ctl(app: &mut App, words: &[&str], line: &str) -> Option<Result<String, String>> {
    if words.first() != Some(&"durchblick") {
        return None;
    }
    Some(match words.get(1).copied() {
        Some("open") => {
            app.db_open("ctl");
            Ok("ok".into())
        }
        Some("close") => {
            app.db_close("ctl");
            Ok("ok".into())
        }
        Some("state") | None => Ok(state(app).to_string()),
        Some("press") => match words.get(2).and_then(|w| Button::of_word(w)) {
            Some(b) if app.db.open => {
                as_ctl(app, |app| app.db_press(b));
                Ok(state(app).to_string())
            }
            Some(_) => Err("Durchblick ist nicht offen.".into()),
            None => Err(format!("durchblick press {}", Button::ALL.map(Button::word).join("|"))),
        },
        Some("ask") => {
            let text = line.trim_start().splitn(3, char::is_whitespace).nth(2).unwrap_or("").trim().to_string();
            if !app.db.open || !app.db.has_field() {
                return Some(Err(format!("Durchblick wartet gerade nicht auf eine Frage ({}).", app.db.stage.word())));
            }
            let origin = app.ctl_origin;
            app.db_submit(&text, origin);
            Ok("ok".into())
        }
        Some(other) => Err(format!("durchblick open|close|state|press|ask, not {other}")),
    })
}
