//! EINS-D: the control socket's part for `bits` (user/tools/bits). Every line passes here first (one registration
//! line in `ctl::run_queue`): a peer in the assistant's tree is judged (`ctl_bits_gate`), questions are numbered
//! and answered, and the verbs bitscomp lacked are run: `bits-vor`, `bits-groesse`, `bits-meldung`, `bits-foto`,
//! `bits-clip-get`, `bits-clip-set`, `bits-frage`, `bits-antwort`, `bits-mit`, `bits-fragen`. A peer outside the tree (Georg's
//! terminal, a test) gets every other line exactly as before.

use std::io::Write;
use std::os::unix::net::UnixStream;

use crate::ctl_bits_gate::{self as gate, Verdict};
use crate::input::Origin;
use crate::layout::{Place, Rect};
use crate::render::Composer;
use crate::state::App;
use crate::wm::Action;

pub fn reply(mut s: UnixStream, answer: &str) {
    let _ = s.write_all(format!("{answer}\n").as_bytes());
}

/// Whether the peer `pid` is in the assistant's tree. A peer that cannot be seen any more counts as inside
/// (`bitstree::of_peer`: the line is judged after it arrived). The service itself (`assist serve`, which judges its
/// own tools by the level) is not judged a second time: the tree's first process running `/bin/assist` (on the
/// host, where only the marker tags, the program alone).
fn in_tree(pid: Option<i32>) -> bool {
    let pid = pid.and_then(|p| u32::try_from(p).ok());
    let root = match bitstree::of_peer(pid) {
        bitstree::Tree::Out => return false,
        bitstree::Tree::In { root, .. } => root,
    };
    let Some(pid) = pid else { return true };
    if root.is_some_and(|r| r != pid) {
        return true;
    }
    let assist = crate::assist_serve::EXE;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok().and_then(|p| std::fs::canonicalize(p).ok());
    // Both must be known: an exe that cannot be read (the peer ended) is never the service.
    exe.is_none() || exe != std::fs::canonicalize(&assist).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_peer_that_cannot_be_seen_is_judged() {
        assert!(super::in_tree(None), "no peer credentials");
        assert!(super::in_tree(Some(0)) && super::in_tree(Some(-1)), "no pid in our namespace");
        assert!(!super::in_tree(Some(std::process::id() as i32)), "the test itself is outside");
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id() as i32;
        c.wait().unwrap();
        assert!(super::in_tree(Some(pid)), "a peer that wrote its line and ended");
    }
}

/// Ok(Some): run the line as before; Ok(None): answered here.
pub fn take(app: &mut App, comp: &mut dyn Composer, cmd: String, stream: UnixStream) -> Option<(String, UnixStream)> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let cred = crate::clients::peer_cred(&stream);
    let (pid, uid) = cred.unwrap_or((0, u32::MAX));
    let tree = in_tree(cred.map(|c| c.0));
    if tree {
        // EINS-B: what the tree never reads (a never-see program) and no local set that gives it more freedom.
        if let Some(r) = app.tree_private(&words) {
            crate::db_app::journal(&format!("bitscomp assist.tree private={} {}", words.first().unwrap_or(&"-"), if r.is_ok() { "filtered" } else { "refused" }));
            reply(stream, &r.unwrap_or_else(|e| format!("refused: {e}")));
            return None;
        }
        match gate::judge(app, &words, &cmd) {
            Verdict::Pass => {}
            Verdict::Refuse(m) => {
                crate::db_app::journal(&format!("bitscomp assist.tree refused={} level={}", words.first().unwrap_or(&"-"), gate::level(app)));
                reply(stream, &format!("refused: {m}"));
                return None;
            }
            Verdict::Ask(text) => {
                let n = gate::open(app, pid, Some(cmd.clone()), text, true);
                reply(stream, &format!("frage {n} {}", gate::WAIT.as_secs()));
                return None;
            }
        }
    }
    let line = match words.first().copied() {
        Some("bits-mit") => {
            let n: u64 = words.get(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            let inner = cmd.trim_start().splitn(3, char::is_whitespace).nth(2).unwrap_or("").trim().to_string();
            if let Err(why) = gate::take_yes(n, pid, &inner).and_then(|()| gate::recheck(app, &inner)) {
                reply(stream, &format!("refused: {why}"));
                return None;
            }
            inner
        }
        _ => cmd,
    };
    // Judged already (or from outside the tree, where the word changes nothing): the step itself runs.
    let line = if line.starts_with("bits-schritt ") { gate::step_line(&line).unwrap_or("").to_string() } else { line };
    let words: Vec<&str> = line.split_whitespace().collect();
    let r = match words.first().copied() {
        Some("bits-antwort") => answer_line(app, &words, Origin::Injected),
        // The open questions, for the desk and the tests (wave EINS-B draws them as cards).
        Some("bits-fragen") => {
            let open: Vec<String> = gate::pending().iter().map(|q| format!("{} {}", q.n, q.text)).collect();
            if open.is_empty() { "keine".into() } else { open.join("\n") }
        }
        Some("bits-frage") => match gate::frage_text(&words) {
            Some(text) => format!("frage {} {}", gate::open(app, pid, None, text, tree), gate::WAIT.as_secs()),
            None => "error: Diese Frage kennt die Oberfläche nicht.".into(),
        },
        Some("bits-vor") => done(vor(app, &words)),
        Some("bits-groesse") => done(groesse(app, &words)),
        Some("bits-meldung") => meldung(app, &line, tree),
        Some("bits-foto") => done(crate::ctl_bits_clip::foto(app, comp, &words, tree)),
        Some("bits-clip-set") => done(crate::ctl_bits_clip::set(app, &words, uid)),
        Some("bits-clip-get") => {
            crate::ctl_bits_clip::get(app, stream, tree);
            return None;
        }
        _ => return Some((line, stream)),
    };
    reply(stream, &r);
    None
}

fn done(r: Result<String, String>) -> String {
    r.unwrap_or_else(|e| format!("error: {e}"))
}

/// `bits-antwort <n> [ja|nein]`.
fn answer_line(app: &mut App, words: &[&str], origin: Origin) -> String {
    let Some(n) = words.get(1).and_then(|n| n.parse::<u64>().ok()) else { return "error: bits-antwort <n> [ja|nein]".into() };
    match words.get(2).copied() {
        None => gate::status(n).into(),
        Some(w @ ("ja" | "nein")) => match gate::answer(app, n, w == "ja", origin) {
            Ok(()) => gate::status(n).into(),
            Err(e) => format!("refused: {e}"),
        },
        Some(_) => "error: bits-antwort <n> [ja|nein]".into(),
    }
}

fn window_by(app: &App, id: &str) -> Result<crate::win::Window, String> {
    let n: u64 = id.trim_start_matches('w').parse().map_err(|_| format!("„{id}“ ist kein Fenster."))?;
    app.all_windows().into_iter().find(|w| crate::ctl::win_id(w) == n).ok_or(format!("Ein Fenster w{n} gibt es nicht."))
}

/// `bits-vor <id>`: to its workspace, out of the minimised ones, focused.
fn vor(app: &mut App, words: &[&str]) -> Result<String, String> {
    let w = window_by(app, words.get(1).ok_or("bits-vor <fenster>")?)?;
    if app.ws.find(&w).is_none() && !app.ws.minimized.iter().any(|m| m.win == w) {
        let i = (0..app.desk.parked.len()).find(|i| *i != app.desk.active && (app.desk.parked[*i].find(&w).is_some() || app.desk.parked[*i].minimized.iter().any(|m| m.win == w)));
        if let Some(i) = i {
            app.act(Action::Workspace(i));
        }
    }
    if app.ws.minimized.iter().any(|m| m.win == w) {
        app.unminimize(&w, "bits");
    }
    app.ws.focus_window(&w);
    app.layout_dirty = true;
    Ok("ok".into())
}

/// `bits-groesse <id> <w> <h>` or `voll`: a tiled window floats first; the size is clamped to the work area.
fn groesse(app: &mut App, words: &[&str]) -> Result<String, String> {
    let w = window_by(app, words.get(1).ok_or("bits-groesse <fenster> <breite> <höhe> | voll")?)?;
    if app.ws.find(&w).is_none() {
        return Err("Das Fenster liegt nicht auf dem gezeigten Arbeitsbereich; erst „bits fenster vor“.".into());
    }
    if words.get(2) == Some(&"voll") {
        app.act_on(Some(&w), Action::Maximise);
    } else {
        let num = |i: usize| words.get(i).and_then(|v| v.parse::<i32>().ok()).filter(|v| *v > 0).ok_or("Breite und Höhe sind ganze Zahlen über 0.");
        let (bw, bh) = (num(2)?, num(3)?);
        let area = app.area();
        if matches!(app.ws.find(&w), Some(Place::Tiled { .. })) {
            app.ws.toggle_float(&w, area);
        }
        let Some(Place::Floating { index }) = app.ws.find(&w) else { return Err("Das Fenster lässt sich nicht schwebend machen.".into()) };
        let r = app.ws.floating[index].rect;
        let (nw, nh) = (bw.min(area.w), bh.min(area.h));
        let x = r.x.clamp(area.x, area.x + area.w - nw);
        let y = r.y.clamp(area.y, area.y + area.h - nh);
        app.ws.floating[index].rect = Rect { x, y, w: nw, h: nh };
        app.ws.floating[index].restore = None;
    }
    app.ws.focus_window(&w);
    app.layout_dirty = true;
    let r = match app.ws.find(&w) {
        Some(Place::Floating { index }) => app.ws.floating[index].rect,
        _ => app.area(),
    };
    Ok(format!("rect={},{},{},{}", r.x, r.y, r.w, r.h))
}

/// The longest notice line.
const NOTICE_MAX: usize = 300;

fn meldung(app: &mut App, line: &str, tree: bool) -> String {
    let text: String = line.trim_start().strip_prefix("bits-meldung").unwrap_or("").trim().chars().take(NOTICE_MAX).collect();
    if text.is_empty() {
        return "error: Eine Meldung braucht Text.".into();
    }
    let shown = if tree { format!("Assistent: {text}") } else { text };
    app.notice(shown.clone());
    shown
}
