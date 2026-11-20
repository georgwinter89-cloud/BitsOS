//! Boot time from the kernel's own timestamps: the boot screen's step list (`/proc/bitsos/boot`) says when each
//! part of the start finished; the latest of them is the time to a ready desktop. A night does not reboot five
//! times: every start of bitsbench notes this boot's time in `boots.json`, and the night takes the last five boots
//! of the running generation. Under another Linux (the CachyOS comparison) `systemd-analyze` says the same: the
//! kernel's part plus the user space up to the graphical target.

use super::{Ctx, Fail, Values};
use mcpreg::Json;
use std::path::Path;

/// Boots kept in boots.json.
const KEEP: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Boot {
    /// When the boot began (UTC s, rounded to 10 s so one boot is noted once).
    pub at_s: i64,
    pub ms: u64,
    pub generation: Option<u64>,
}

fn parse(j: &Json) -> Vec<Boot> {
    j.get("boots")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| Some(Boot { at_s: b.get("at_s")?.as_i64()?, ms: b.get("ms")?.as_u64()?, generation: b.get("generation").and_then(Json::as_u64) }))
                .collect()
        })
        .unwrap_or_default()
}

fn to_json(v: &[Boot]) -> Json {
    let a = v.iter().map(|b| {
        Json::obj(vec![
            ("at_s", Json::Num(b.at_s as f64)),
            ("ms", Json::Num(b.ms as f64)),
            ("generation", b.generation.map_or(Json::Null, |g| Json::Num(g as f64))),
        ])
    });
    Json::obj(vec![("boots", Json::Arr(a.collect()))])
}

/// The list with `b` noted once (a boot seen again replaces itself), the newest KEEP.
pub fn with(mut v: Vec<Boot>, b: Boot) -> Vec<Boot> {
    v.retain(|x| (x.at_s - b.at_s).abs() > 30);
    v.push(b);
    v.sort_by_key(|x| x.at_s);
    let cut = v.len().saturating_sub(KEEP);
    v.drain(..cut);
    v
}

/// The values a night takes: the last `n` boots of `generation`.
pub fn last_of(v: &[Boot], generation: Option<u64>, n: usize) -> Vec<f64> {
    let mut x: Vec<f64> = v.iter().rev().filter(|b| b.generation == generation).take(n).map(|b| b.ms as f64).collect();
    x.reverse();
    x
}

/// Notes this boot (when its start is finished). Returns its time.
pub fn record(root: &Path) -> Option<u64> {
    let ms = crate::probe::boot_ms(&std::fs::read_to_string("/proc/bitsos/boot").ok()?)?;
    let at_s = (crate::time::now_s() - crate::probe::uptime_s()? as i64) / 10 * 10;
    let p = root.join("boots.json");
    let old = crate::store::read_json(&p).ok().flatten().map(|j| parse(&j)).unwrap_or_default();
    let v = with(old, Boot { at_s, ms, generation: crate::probe::generation() });
    crate::store::write_json(&p, &to_json(&v)).ok()?;
    Some(ms)
}

/// `systemd-analyze` (time): kernel part plus user space up to graphical.target, in ms.
pub fn systemd_ms(text: &str) -> Option<f64> {
    let secs = |w: &str| -> Option<f64> {
        let w = w.trim_end_matches('.');
        if let Some(m) = w.strip_suffix("ms") {
            return m.parse::<f64>().ok().map(|x| x / 1000.0);
        }
        // "1min 2.5s" comes as two words; the caller adds them.
        if let Some(m) = w.strip_suffix("min") {
            return m.parse::<f64>().ok().map(|x| x * 60.0);
        }
        w.strip_suffix('s')?.parse().ok()
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    let kernel = words.windows(2).find(|w| w[1] == "(kernel)").and_then(|w| secs(w[0]))?;
    let i = words.iter().position(|w| *w == "after")?;
    let mut user = 0.0;
    for w in &words[i + 1..] {
        match secs(w) {
            Some(s) => user += s,
            None => break,
        }
    }
    (user > 0.0).then_some((kernel + user) * 1000.0)
}

pub fn run(ctx: &Ctx) -> Values {
    ctx.check()?;
    if !ctx.cfg.bitsos {
        let out = std::process::Command::new("systemd-analyze").output().map_err(|e| Fail::Missing(format!("systemd-analyze fehlt ({e})")))?;
        let v = systemd_ms(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| Fail::Missing("systemd-analyze nennt keine Startzeit".into()))?;
        return Ok(vec![v]);
    }
    record(&ctx.cfg.root);
    let all = crate::store::read_json(&ctx.cfg.root.join("boots.json")).ok().flatten().map(|j| parse(&j)).unwrap_or_default();
    let v = last_of(&all, crate::probe::generation(), ctx.cfg.runs);
    if v.is_empty() {
        return Err(Fail::Missing("noch kein fertiger Start dieser Generation aufgezeichnet".into()));
    }
    Ok(v)
}
