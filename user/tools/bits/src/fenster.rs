//! `bits fenster liste|vor|schliessen|groesse`: bitscomp's `list`, `close` and the new `bits-vor`/`bits-groesse`.
//! A window is named as in the desktop's list (C1): `w3`; the bare number works too.

use serde_json::{Value, json};

use crate::frage;
use crate::out::{Done, Fail, R};
use crate::sock;

pub fn run(args: &[String]) -> R {
    match args.first().map(String::as_str) {
        Some("liste") | None => liste(),
        Some("vor") => one(args, "bits-vor", "vorn"),
        Some("schliessen") => one(args, "close", "gebeten zu schließen"),
        Some("groesse") => groesse(&args[1..]),
        Some(v) => Err(Fail::usage(format!("„fenster {v}“ gibt es nicht; möglich: liste, vor, schliessen, groesse."))),
    }
}

/// `w3` or `3` -> 3.
pub fn number(s: &str) -> R<u64> {
    s.strip_prefix('w').unwrap_or(s).parse::<u64>().map_err(|_| Fail::usage(format!("„{s}“ ist kein Fenster (so: w3).")))
}

fn one(args: &[String], verb: &str, done: &str) -> R {
    let id = number(args.get(1).ok_or_else(|| Fail::usage("Welches Fenster? (so: w3)"))?)?;
    frage::changing(&format!("{verb} {id}"))?;
    Ok(Done::new(format!("Fenster w{id}: {done}.")).with("window", format!("w{id}")))
}

fn groesse(a: &[String]) -> R {
    let id = number(a.first().ok_or_else(|| Fail::usage("fenster groesse <fenster> <breite> <höhe> | voll"))?)?;
    let line = match (a.get(1).map(String::as_str), a.get(2)) {
        (Some("voll"), None) => format!("bits-groesse {id} voll"),
        (Some(w), Some(h)) => {
            let (w, h) = (w.parse::<u32>(), h.parse::<u32>());
            let (Ok(w), Ok(h)) = (w, h) else { return Err(Fail::usage("Breite und Höhe sind ganze Zahlen (Bildpunkte).")) };
            format!("bits-groesse {id} {w} {h}")
        }
        _ => return Err(Fail::usage("fenster groesse <fenster> <breite> <höhe> | voll")),
    };
    let answer = frage::changing(&line)?;
    let rect = parse_rect(&answer);
    Ok(Done::new(format!("Fenster w{id}: {answer}")).with("window", format!("w{id}")).with("rect", rect))
}

/// `rect=x,y,w,h` in an answer.
fn parse_rect(a: &str) -> Value {
    let r = a.split_whitespace().find_map(|w| w.strip_prefix("rect=")).map(|r| r.split(',').filter_map(|n| n.parse::<i64>().ok()).collect::<Vec<_>>());
    match r {
        Some(v) if v.len() == 4 => json!(v),
        _ => Value::Null,
    }
}

fn liste() -> R {
    let raw = sock::ctl("list")?;
    let windows: Vec<Value> = raw.lines().filter(|l| *l != "end").filter_map(window).collect();
    let text = windows
        .iter()
        .map(|w| {
            let mark = if w["focused"] == true { "*" } else { " " };
            format!("{mark} {:<5} {:<20} {}", w["id"].as_str().unwrap_or(""), w["app"].as_str().unwrap_or(""), w["title"].as_str().unwrap_or(""))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Done::new(if text.is_empty() { "Keine Fenster.".into() } else { text }).with("windows", windows))
}

/// One line of bitscomp's `list` (ctl.rs `list`, minimize.rs `minimized_lines`) as BITS.md's window object.
pub fn window(line: &str) -> Option<Value> {
    let words = words(line);
    let get = |k: &str| words.iter().find_map(|(key, v)| (key == k).then_some(v.as_str()));
    let id: u64 = get("id")?.parse().ok()?;
    let flag = |k: &str| get(k) == Some("true");
    let place = if get("minimized").is_some() {
        "minimized"
    } else if get("float").is_some() {
        "float"
    } else {
        "tile"
    };
    let rect = match (get("frame"), words.iter().position(|(k, _)| k == "frame")) {
        (Some(xy), Some(i)) => {
            let size = words.get(i + 1).map(|(k, _)| k.clone()).unwrap_or_default();
            let mut n: Vec<i64> = xy.split(',').chain(size.split('x')).filter_map(|s| s.parse().ok()).collect();
            n.truncate(4);
            if n.len() == 4 { json!(n) } else { Value::Null }
        }
        _ => Value::Null,
    };
    Some(json!({
        "id": format!("w{id}"),
        "app": get("app_id").unwrap_or(""),
        "title": get("title").unwrap_or(""),
        "pid": get("pid").and_then(|p| p.parse::<i64>().ok()).unwrap_or(0),
        "focused": flag("focused"),
        "visible": flag("visible"),
        "place": place,
        "fullscreen": words.iter().any(|(k, v)| k == "fullscreen" && v.is_empty()),
        "max": words.iter().any(|(k, v)| k == "max" && v.is_empty()),
        "hung": flag("hung"),
        "workspace": get("ws").and_then(|w| w.parse::<i64>().ok()).unwrap_or(0),
        "rect": rect,
    }))
}

/// `key=value` words; a value in quotes is Rust's debug form of a string (the title).
fn words(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut it = line.chars().peekable();
    loop {
        while it.peek() == Some(&' ') {
            it.next();
        }
        if it.peek().is_none() {
            return out;
        }
        let mut key = String::new();
        while let Some(&c) = it.peek() {
            if c == ' ' || c == '=' {
                break;
            }
            key.push(c);
            it.next();
        }
        let mut val = String::new();
        if it.peek() == Some(&'=') {
            it.next();
            if it.peek() == Some(&'"') {
                it.next();
                val = unquote(&mut it);
            } else {
                while let Some(&c) = it.peek() {
                    if c == ' ' {
                        break;
                    }
                    val.push(c);
                    it.next();
                }
            }
        }
        out.push((key, val));
    }
}

fn unquote(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(c) = it.next() {
        match c {
            '"' => break,
            '\\' => match it.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('r') => s.push('\r'),
                Some('0') => s.push('\0'),
                Some('u') => {
                    let hex: String = it.by_ref().skip(1).take_while(|c| *c != '}').collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        s.push(ch);
                    }
                }
                Some(o) => s.push(o),
                None => break,
            },
            o => s.push(o),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_line_becomes_a_window() {
        let w = window(r#"tile=0 page=1/1 visible=true focused=true frame=10,20 800x600 id=3 app_id=foot title="Fuß \"a\" \u{1f600}" pid=12 hung=false ws=1"#).unwrap();
        assert_eq!(w["id"], "w3");
        assert_eq!(w["title"], "Fuß \"a\" 😀");
        assert_eq!(w["rect"], json!([10, 20, 800, 600]));
        assert_eq!(w["place"], "tile");
        let m = window(r#"minimized=0 visible=false focused=false id=4 app_id=x title="" pid=1 hung=false ws=2"#).unwrap();
        assert_eq!((m["place"].as_str(), m["rect"].is_null(), m["workspace"].as_i64()), (Some("minimized"), true, Some(2)));
        let f = window(r#"float=1 max fullscreen visible=true focused=false frame=0,0 10x10 id=5 app_id=y title="t" pid=2 hung=true ws=1"#).unwrap();
        assert_eq!((f["place"].as_str(), f["max"].as_bool(), f["fullscreen"].as_bool(), f["hung"].as_bool()), (Some("float"), Some(true), Some(true), Some(true)));
    }
}
