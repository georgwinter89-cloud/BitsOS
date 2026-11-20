//! UPDATE: the update page's ops (bitsupdate's `flow`), on the program service's socket: it is root already,
//! started by init, and its peer rule (root and the compositor holding fb0's grant may change, everyone else
//! reads: `peer`) is the one the page needs, so no new service and no change to init (decision, notes).
//!
//! | op | fields | answer | who |
//! |---|---|---|---|
//! | `update_status` | – | `installed` (version, date, generations, pin, back, pending), `token` (`***` or none), `offer` | all |
//! | `update_check` | – | `offer` (version, date, notes, `comes`: what it brings) or `current`, or the German `message` | change |
//! | `update_do` | – | `progress` lines (each part's download and write), then `installed` (the slot) and `message` | change |
//! | `update_back`, `update_forward` | – | the generation the next start takes | change |
//! | `update_token` | `token` or `clear: true` | `token` (`***` or none) | change |
//! | `update_confirm` | `generation` | VB-1: Georg's "Ja" to "Version N läuft. Bestätigen?": `confirmed` or `writing`, and the `message` | change |
//!
//! `update_status` carries VB-1's `question` (version, generation, back) while the kernel asks, and `asks: false`
//! once it never will in this boot (the desktop's notice stops looking then).
//!
//! The token is never in an answer, an event or a log line. One update at a time (`RUN`).

use bitsupdate::flow::{self, Ctx, Fail, Installed, Offer};
use bitsupdate::confirm::{self as ask, Ask, Done};
use bitsupdate::{source, text};
use settingsd::json::Json;
use std::sync::Mutex;

/// The newest offer `update_check` found (what `update_do` installs; checked again before).
static OFFER: Mutex<Option<Offer>> = Mutex::new(None);
static RUN: Mutex<()> = Mutex::new(());

pub const OPS: &[&str] = &["update_status", "update_check", "update_do", "update_back", "update_forward", "update_token", "update_confirm"];
/// The ops of this list that change something or reach the network.
pub fn changing(op: &str) -> bool {
    OPS.contains(&op) && op != "update_status"
}

fn fail(f: &Fail) -> Json {
    let code = match f {
        Fail::NoToken | Fail::TokenWrong | Fail::TokenNoAccess => "token",
        Fail::Network(_) | Fail::Tls(_) | Fail::Clock | Fail::Server(_) => "network",
        Fail::Check(_) => "check",
        Fail::NoSpace | Fail::NoRoom { .. } | Fail::NoRam { .. } => "space",
        Fail::Taken(_) => "taken",
        _ => "failed",
    };
    // The page shows `message`; the technical detail goes to the journal (and the answer, for a desk's look).
    if let Some(d) = f.detail() {
        crate::event("pkg.update.failed", &[("error", code), ("detail", d)]);
    }
    let a = Json::obj().with("ok", false).with("error", code).with("message", f.text());
    match f.detail() {
        Some(d) => a.with("detail", d),
        None => a,
    }
}

fn installed(i: &Installed) -> Json {
    let gens: Vec<Json> = i.generations.iter().map(|(n, m)| Json::obj().with("n", *n as u64).with("markers", m.as_str())).collect();
    Json::obj()
        .with("version", i.running as u64)
        .with("date", i.date.as_str())
        .with("text", text::installed(i))
        .with("generation", i.generation as u64)
        .with("generations", Json::Arr(gens))
        .with("pin", i.pin.map_or(Json::Null, |p| Json::from(p as u64)))
        .with("back", i.back.map_or(Json::Null, |p| Json::from(p as u64)))
        .with("pending", i.pending)
        .with("writable", i.writable)
}

/// VB-1: the question for the desktop's notice (`question`, or null) and whether it may still come (`asks`).
fn question(i: &Installed) -> (Json, bool) {
    match ask::decide(i) {
        Ask::Now(q) => (
            Json::obj()
                .with("version", q.version as u64)
                .with("generation", q.generation as u64)
                .with("back", q.back.map_or(Json::Null, |b| Json::from(b as u64)))
                .with("text", ask::question_text(&q)),
            true,
        ),
        Ask::Later(_) => (Json::Null, true),
        Ask::Never(_) => (Json::Null, false),
    }
}

/// The generation the desktop was last told to ask about: the kernel's log names each question once (the desk
/// reads that a screen asked; the card asks every few seconds).
static ASKED: Mutex<u32> = Mutex::new(0);

fn note_question(q: &Json) {
    let Some(g) = q.get("generation").and_then(Json::as_i64) else { return };
    let Ok(mut last) = ASKED.lock() else { return };
    if *last == g as u32 {
        return;
    }
    *last = g as u32;
    // bitspkg's events reach the kernel log: the desk sees there that a screen asked.
    crate::event("pkg.update.question", &[("generation", &g.to_string())]);
}

fn confirm_op(j: &Json) -> Json {
    let Some(g) = j.get("generation").and_then(Json::as_i64).filter(|&g| g > 0 && g <= u32::MAX as i64) else {
        return Json::obj().with("ok", false).with("error", "bad_request").with("message", "generation fehlt");
    };
    crate::event("pkg.update.confirm", &[("generation", &g.to_string())]);
    match ask::yes(g as u32, std::time::Duration::from_secs(120)) {
        Ok(Done::Confirmed { by }) => Json::obj().with("ok", true).with("confirmed", true).with("by", by.as_str()).with("message", ask::confirmed(g as u32)),
        Ok(Done::Writing) => Json::obj().with("ok", true).with("confirmed", false).with("message", ask::still_writing(g as u32)),
        Err(d) => {
            crate::event("pkg.update.failed", &[("error", "confirm"), ("detail", &d)]);
            Json::obj().with("ok", false).with("error", "confirm").with("message", ask::not_confirmed()).with("detail", d.as_str())
        }
    }
}

fn offer(o: &Offer) -> Json {
    let j = Json::obj().with("version", o.release.version as u64).with("date", text::german_date(&o.manifest.date)).with("notes", o.notes.as_str());
    // UPD-4: what comes (Kern, Schreibtisch, Programme with their megabytes), Georg's line.
    match &o.plan {
        Some(p) => j.with("comes", text::comes(p).as_str()),
        None => j,
    }
}

fn token_state() -> Json {
    if source::read_token(&source::token_dir()).is_some() { Json::from("***") } else { Json::Null }
}

pub fn handle(op: &str, j: &Json, out: &mut dyn FnMut(&Json) -> bool) -> Json {
    match op {
        "update_status" => match flow::status() {
            Ok(i) => {
                let o = OFFER.lock().ok().and_then(|g| g.clone()).filter(|o| o.release.version > i.newest.max(i.running));
                let (q, asks) = question(&i);
                note_question(&q);
                Json::obj()
                    .with("ok", true)
                    .with("installed", installed(&i))
                    .with("token", token_state())
                    .with("offer", o.as_ref().map_or(Json::Null, offer))
                    .with("question", q)
                    .with("asks", asks)
            }
            Err(f) => fail(&f),
        },
        "update_check" => {
            let i = match flow::status() {
                Ok(i) => i,
                Err(f) => return fail(&f),
            };
            crate::event("pkg.update.check", &[("running", &i.running.to_string()), ("newest", &i.newest.to_string())]);
            match flow::check(&Ctx::standard(), &i) {
                Ok(None) => {
                    if let Ok(mut g) = OFFER.lock() {
                        *g = None;
                    }
                    Json::obj().with("ok", true).with("current", true).with("message", text::CURRENT)
                }
                Ok(Some(o)) => {
                    let a = Json::obj().with("ok", true).with("offer", offer(&o));
                    if let Ok(mut g) = OFFER.lock() {
                        *g = Some(o);
                    }
                    a
                }
                Err(f) => fail(&f),
            }
        }
        "update_do" => {
            let Ok(_run) = RUN.try_lock() else { return Json::obj().with("ok", false).with("error", "busy").with("message", "Es läuft schon ein Aktualisieren.") };
            let cx = Ctx::standard();
            let i = match flow::status() {
                Ok(i) => i,
                Err(f) => return fail(&f),
            };
            let o = match OFFER.lock().ok().and_then(|g| g.clone()) {
                Some(o) => o,
                None => match flow::check(&cx, &i) {
                    Ok(Some(o)) => o,
                    Ok(None) => return Json::obj().with("ok", true).with("current", true).with("message", text::CURRENT),
                    Err(f) => return fail(&f),
                },
            };
            crate::event("pkg.update.do", &[("version", &o.release.version.to_string())]);
            let mut say = |s| {
                out(&Json::obj().with("progress", text::step(s)));
            };
            let b = match flow::download(&cx, &i, &o, &mut say) {
                Ok(b) => b,
                Err(f) => {
                    crate::event("pkg.update.refused", &[("version", &o.release.version.to_string()), ("why", &f.text())]);
                    return fail(&f);
                }
            };
            match flow::install(&b, &mut say) {
                Ok(d) => {
                    crate::event("pkg.update.installed", &[("version", &d.slot.to_string())]);
                    if let Ok(mut g) = OFFER.lock() {
                        *g = None;
                    }
                    Json::obj().with("ok", true).with("installed", d.slot as u64).with("message", text::done(d.slot))
                }
                Err(f) => fail(&f),
            }
        }
        "update_confirm" => confirm_op(j),
        "update_back" | "update_forward" => match flow::pin(op == "update_back") {
            Ok(n) => Json::obj().with("ok", true).with("next", n.as_str()),
            Err(f) => fail(&f),
        },
        "update_token" => {
            let d = source::token_dir();
            let r = if j.get("clear").and_then(Json::as_bool) == Some(true) {
                source::clear_token(&d)
            } else {
                match j.str("token") {
                    Some(t) => source::store_token(&d, t),
                    None => Err(String::from("token fehlt")),
                }
            };
            crate::event("pkg.update.token", &[("stored", if r.is_ok() { "yes" } else { "no" })]);
            match r {
                Ok(()) => Json::obj().with("ok", true).with("token", token_state()),
                Err(t) => Json::obj().with("ok", false).with("error", "bad_request").with("message", t),
            }
        }
        _ => Json::obj().with("ok", false).with("error", "bad_request").with("message", "unbekannte op"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_ops_change() {
        assert!(!changing("update_status"));
        for op in ["update_check", "update_do", "update_back", "update_forward", "update_token", "update_confirm"] {
            assert!(changing(op), "{op}");
        }
        assert!(!changing("do"));
        let f = fail(&Fail::TokenWrong);
        assert_eq!(f.str("error"), Some("token"));
        assert!(!f.to_text().contains("test_") && f.str("message").unwrap().contains("Schlüssel"));
    }
}
