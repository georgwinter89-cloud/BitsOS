//! `bits ui liste|tun|warten`: the desktop as a list (contract C1 through bitscomp's `ui`/`ui-act`) and waiting
//! for something to appear once the program has settled (EINGABE2's rule, seen from outside: a read only counts
//! when the next read a moment later still shows it at the same place).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::fenster;
use crate::frage;
use crate::out::{Done, Fail, R};
use crate::sock;

/// The C1 actions (one spelling: uilist's words).
pub const ACTIONS: &[&str] = &["press", "type", "choose", "toggle", "scroll", "focus", "submit"];

pub fn run(args: &[String]) -> R {
    match args.first().map(String::as_str) {
        Some("liste") | None => liste(args.get(1).map(String::as_str)),
        Some("tun") => tun(&args[1..]),
        Some("warten") => warten(&args[1..]),
        Some(v) => Err(Fail::usage(format!("„ui {v}“ gibt es nicht; möglich: liste, tun, warten."))),
    }
}

/// A window argument in C1's spelling: `w3`, `3`, `focus`, `desktop`, `sofa`.
pub fn c1_window(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_digit()) { format!("w{s}") } else { s.to_string() }
}

fn read(window: Option<&str>) -> R<Value> {
    let line = match window {
        Some(w) => format!("ui {}", c1_window(w)),
        None => "ui".into(),
    };
    let raw = sock::ctl(&line)?;
    serde_json::from_str(&raw).map_err(|e| Fail::failed(format!("Die Liste der Oberfläche ist unverständlich: {e}")))
}

fn liste(window: Option<&str>) -> R {
    if !crate::out::json_mode() {
        let line = window.map_or_else(|| "ui text".to_string(), |w| format!("ui text {}", c1_window(w)));
        return Ok(Done::new(sock::ctl(&line)?));
    }
    Ok(match read(window)? {
        v @ Value::Array(_) => Done::new("").with("windows", v),
        one => Done::new("").with("window", one),
    })
}

fn tun(all: &[String]) -> R {
    let undo = match (all.iter().any(|x| x == "--rueckgaengig"), all.iter().any(|x| x == "--endgueltig")) {
        (true, true) => return Err(Fail::usage("Entweder --rueckgaengig oder --endgueltig, nicht beides.")),
        (true, false) => Some("rueckgaengig"),
        (false, true) => Some("endgueltig"),
        (false, false) => None,
    };
    let a: Vec<String> = all.iter().filter(|x| !matches!(x.as_str(), "--rueckgaengig" | "--endgueltig")).cloned().collect();
    let (Some(id), Some(action)) = (a.first(), a.get(1)) else {
        return Err(Fail::usage(format!("ui tun <element> <aktion> [text]; Aktionen: {}", ACTIONS.join(", "))));
    };
    if !ACTIONS.contains(&action.as_str()) && action != "abschicken" {
        return Err(Fail::usage(format!("Die Aktion „{action}“ gibt es nicht; möglich: {}.", ACTIONS.join(", "))));
    }
    let text = a[2..].join(" ");
    if text.contains('\n') && !text.ends_with('\n') {
        return Err(Fail::usage("Ein Zeilenumbruch geht nur am Ende (dann ist er Enter)."));
    }
    let mut line = match undo {
        Some(w) => format!("bits-schritt {w} ui-act {id} {action}"),
        None => format!("ui-act {id} {action}"),
    };
    if !text.is_empty() {
        line.push(' ');
        line.push_str(&text.replace('\n', "\\n"));
    }
    let answer = frage::changing(&line)?;
    Ok(Done::new(answer.clone()).with("element", id.as_str()).with("action", action.as_str()).with("answer", answer))
}

#[derive(Debug, Default)]
struct Want {
    window: Option<String>,
    element: Option<String>,
    text: Option<String>,
    gone: bool,
    limit: Duration,
}

fn parse_want(a: &[String]) -> R<Want> {
    let mut w = Want { limit: Duration::from_secs(10), ..Want::default() };
    let mut it = a.iter();
    while let Some(o) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| Fail::usage(format!("{o} braucht einen Wert")));
        match o.as_str() {
            "--fenster" => w.window = Some(val()?),
            "--element" => w.element = Some(val()?),
            "--text" => w.text = Some(val()?),
            "--weg" => w.gone = true,
            "--bis-zu" => {
                let s = val()?;
                let secs = s.parse::<f64>().ok().filter(|s| (0.0..=3600.0).contains(s)).ok_or_else(|| Fail::usage(format!("--bis-zu {s}: Sekunden von 0 bis 3600")))?;
                w.limit = Duration::from_secs_f64(secs);
            }
            other => return Err(Fail::usage(format!("ui warten kennt „{other}“ nicht; möglich: --fenster, --element, --text, --weg, --bis-zu."))),
        }
    }
    if w.window.is_none() && w.element.is_none() && w.text.is_none() {
        return Err(Fail::usage("ui warten braucht --fenster, --element oder --text."));
    }
    Ok(w)
}

/// The window a `--fenster` word means: an id, else the first whose title or program contains it (any case).
fn find_window(windows: &[Value], word: &str) -> Option<Value> {
    let id = c1_window(word);
    let lower = word.to_lowercase();
    windows.iter().find(|w| w["id"] == id.as_str()).or_else(|| {
        windows.iter().find(|w| {
            let has = |k: &str| w[k].as_str().is_some_and(|s| s.to_lowercase().contains(&lower));
            has("title") || has("app")
        })
    }).cloned()
}

fn walk<'a>(els: &'a Value, f: &mut impl FnMut(&'a Value) -> bool) -> Option<&'a Value> {
    for e in els.as_array()? {
        if f(e) {
            return Some(e);
        }
        if let Some(hit) = walk(&e["children"], f) {
            return Some(hit);
        }
    }
    None
}

/// What one read shows: the window and the element that match, or None.
fn look(w: &Want) -> R<Option<(Value, Value)>> {
    let windows: Vec<Value> = sock::ctl("list")?.lines().filter_map(fenster::window).collect();
    let win = match &w.window {
        Some(word) if matches!(word.as_str(), "desktop" | "sofa" | "focus") => Some(json!({"id": word})),
        Some(word) => match find_window(&windows, word) {
            Some(x) => Some(x),
            None => return Ok(None),
        },
        None => None,
    };
    if w.element.is_none() && w.text.is_none() {
        return Ok(win.map(|x| (x, Value::Null)));
    }
    let c1 = match (&win, &w.element) {
        (Some(x), _) => x["id"].as_str().unwrap_or("focus").to_string(),
        (None, Some(e)) => uilist_window(e),
        (None, None) => "focus".into(),
    };
    let list = match read(Some(&c1)) {
        Ok(v) => v,
        Err(f) if f.code == "failed" => return Ok(None),
        Err(f) => return Err(f),
    };
    let lower = w.text.as_ref().map(|t| t.to_lowercase());
    let hit = walk(&list["elements"], &mut |e| {
        let by_id = w.element.as_ref().is_none_or(|id| e["id"] == id.as_str());
        let by_text = lower.as_ref().is_none_or(|t| ["name", "value"].iter().any(|k| e[*k].as_str().is_some_and(|s| s.to_lowercase().contains(t))));
        by_id && by_text
    });
    let summary = json!({"id": list["window"], "app": list["app"], "title": list["title"]});
    Ok(hit.map(|e| (summary, e.clone())))
}

/// `w3/1a2b` -> `w3`.
fn uilist_window(element: &str) -> String {
    element.split('/').next().unwrap_or("focus").trim_start_matches('#').to_string()
}

/// Two reads at least this far apart must agree before the wait is over.
const SETTLE: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_millis(200);

fn warten(a: &[String]) -> R {
    let w = parse_want(a)?;
    let start = Instant::now();
    let mut seen: Option<(Instant, Value)> = None;
    loop {
        let now = look(&w)?;
        let mark = now.as_ref().map(|(win, e)| json!([win["id"], e["id"], e["rect"]]));
        let met = if w.gone { now.is_none() } else { now.is_some() };
        if met {
            let key = mark.unwrap_or(Value::Null);
            match &seen {
                Some((at, k)) if *k == key && at.elapsed() >= SETTLE => {
                    let ms = start.elapsed().as_millis() as u64;
                    let (win, el) = now.unwrap_or((Value::Null, Value::Null));
                    let text = if w.gone { format!("Weg nach {ms} ms.") } else { format!("Da nach {ms} ms.") };
                    return Ok(Done::new(text).with("window", win).with("element", el).with("waited_ms", ms));
                }
                Some((_, k)) if *k == key => {}
                _ => seen = Some((Instant::now(), key)),
            }
        } else {
            seen = None;
        }
        if start.elapsed() >= w.limit {
            let what = w.text.as_deref().or(w.element.as_deref()).or(w.window.as_deref()).unwrap_or("");
            let secs = w.limit.as_secs_f64();
            return Err(Fail::timeout(if w.gone { format!("Nach {secs} s war „{what}“ noch da.") } else { format!("Nach {secs} s war „{what}“ nicht da.") }));
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_found_by_id_or_words() {
        let ws = vec![json!({"id": "w2", "app": "foot", "title": "Terminal"}), json!({"id": "w5", "app": "brave", "title": "Neuer Tab"})];
        assert_eq!(find_window(&ws, "5").unwrap()["id"], "w5");
        assert_eq!(find_window(&ws, "neuer").unwrap()["id"], "w5");
        assert_eq!(find_window(&ws, "FOOT").unwrap()["id"], "w2");
        assert!(find_window(&ws, "gimp").is_none());
    }

    #[test]
    fn elements_are_found_in_children() {
        let els = json!([{"id": "w1/a", "name": "Kopf", "children": [{"id": "w1/b", "name": "Senden", "value": null}]}]);
        assert_eq!(walk(&els, &mut |e| e["name"] == "Senden").unwrap()["id"], "w1/b");
        assert_eq!(uilist_window("w1/b"), "w1");
        assert_eq!(uilist_window("#w9/x"), "w9");
    }

    #[test]
    fn waiting_needs_something_to_wait_for() {
        assert!(parse_want(&[]).is_err());
        assert!(parse_want(&["--bis-zu".into(), "x".into(), "--text".into(), "a".into()]).is_err());
        let w = parse_want(&["--text".into(), "OK".into(), "--bis-zu".into(), "2.5".into()]).unwrap();
        assert_eq!((w.text.as_deref(), w.limit), (Some("OK"), Duration::from_millis(2500)));
    }
}
