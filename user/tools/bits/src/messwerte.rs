//! `bits messwerte [liste] | lesen <name>...`: the kernel's measurement round (`/proc/bitsos/metrics`, one `name
//! value unit` line per series, plan/spec/system-sources.md K1). A name ending in `*` takes every series it begins.
//! The kernel's tools socket is not asked: it admits /bin/assist only.

use serde_json::{Value, json};

use crate::out::{Done, Fail, R};

pub fn path() -> String {
    std::env::var("BITS_METRICS").unwrap_or_else(|_| "/proc/bitsos/metrics".into())
}

pub fn parse(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (name, value) = (w.next()?, w.next()?);
            let unit = w.next().unwrap_or("");
            let v = value.parse::<f64>().ok().filter(|f| f.is_finite()).map_or_else(|| json!(value), |f| json!(f));
            Some(json!({"name": name, "value": v, "unit": unit}))
        })
        .collect()
}

pub fn run(args: &[String]) -> R {
    let p = path();
    let text = std::fs::read_to_string(&p).map_err(|e| Fail::unreachable(format!("Die Messwerte des Kerns fehlen ({p}: {e}).")))?;
    let all = parse(&text);
    let names: Vec<&String> = match args.first().map(String::as_str) {
        Some("liste") | None => Vec::new(),
        Some("lesen") if args.len() > 1 => args[1..].iter().collect(),
        Some("lesen") => return Err(Fail::usage("messwerte lesen <name>... (ein * am Ende nimmt alle, die so beginnen)")),
        Some(v) => return Err(Fail::usage(format!("„messwerte {v}“ gibt es nicht; möglich: liste, lesen."))),
    };
    let wanted = |n: &str| names.is_empty() || names.iter().any(|w| match w.strip_suffix('*') { Some(p) => n.starts_with(p), None => n == w.as_str() });
    let values: Vec<Value> = all.into_iter().filter(|v| wanted(v["name"].as_str().unwrap_or(""))).collect();
    if values.is_empty() && !names.is_empty() {
        return Err(Fail::new("not_found", format!("Keinen Messwert mit dem Namen {} gibt es.", names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
    }
    let text = values.iter().map(|v| format!("{} {} {}", v["name"].as_str().unwrap_or(""), v["value"], v["unit"].as_str().unwrap_or("")).trim_end().to_string()).collect::<Vec<_>>().join("\n");
    Ok(Done::new(text).with("values", values))
}

#[cfg(test)]
mod tests {
    #[test]
    fn lines_become_values() {
        let v = super::parse("cpu.0.khz 3800000 kHz\ncrash.kernel.previous_start 0 n\nbroken\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["value"], 3_800_000.0);
        assert_eq!(v[1]["unit"], "n");
    }
}
