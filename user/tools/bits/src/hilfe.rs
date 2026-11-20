//! `bits hilfe [thema]`: printed from BITS.md, the contract itself (one text for people, the assistant and tests).

use crate::out::{Done, Fail, R};

pub const MANUAL: &str = include_str!("../BITS.md");

/// `## <thema>` sections; the part before the first is the overview.
fn sections() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![(String::new(), String::new())];
    for line in MANUAL.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            let topic = h.split_whitespace().next().unwrap_or("").trim_matches('`').to_string();
            out.push((topic, String::new()));
        }
        if let Some(last) = out.last_mut() {
            last.1.push_str(line);
            last.1.push('\n');
        }
    }
    out
}

pub fn topics() -> Vec<String> {
    sections().into_iter().skip(1).map(|(t, _)| t).collect()
}

pub fn run(args: &[String]) -> R {
    let all = sections();
    match args.first() {
        None => {
            let text = format!("{}\nThemen: {}\nMehr: bits hilfe <thema>", all[0].1.trim_end(), topics().join(", "));
            Ok(Done::new(text.clone()).with("topic", "").with("text", text).with("topics", topics()))
        }
        Some(t) => {
            let (_, text) = all.iter().skip(1).find(|(x, _)| x == t).ok_or_else(|| Fail::usage(format!("Ein Thema „{t}“ gibt es nicht; es gibt: {}.", topics().join(", "))))?;
            Ok(Done::new(text.trim_end()).with("topic", t.as_str()).with("text", text.trim_end()))
        }
    }
}
