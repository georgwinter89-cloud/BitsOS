//! `bitsprof report DIR [--top N] [--json]`: the hottest functions of a run, the kernel's and each program's,
//! as shares of the busy time (every sample that did not find a CPU idle).

use crate::store::{KERNEL, Profile};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;

/// A JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A number from meta.json (`"key": value`), 0 when missing.
fn meta_num(meta: &str, key: &str) -> f64 {
    let pat = format!("\"{key}\":");
    meta.find(&pat).and_then(|i| meta[i + pat.len()..].trim_start().split([',', '\n', '}']).next()?.trim().parse().ok()).unwrap_or(0.0)
}

pub struct Func {
    pub name: String,
    pub module: String,
    pub samples: u64,
}

pub struct Program {
    pub name: String,
    pub pids: Vec<u32>,
    pub samples: u64,
    pub kernel_samples: u64,
    /// By the function each sample was in.
    pub top: Vec<Func>,
    /// By every function on the stack (callers included).
    pub total_top: Vec<Func>,
}

pub struct Summary {
    pub seconds: f64,
    pub cpus: u64,
    pub hz: u64,
    pub lost: u64,
    /// The profiler's own share of the CPU time (meta.json `cost_share`).
    pub cost: f64,
    pub busy: u64,
    pub idle: u64,
    pub kernel_samples: u64,
    pub kernel_top: Vec<Func>,
    pub programs: Vec<Program>,
}

fn top(counts: HashMap<u32, u64>, p: &Profile, n: usize) -> Vec<Func> {
    let mut v: Vec<(u32, u64)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(n);
    v.into_iter()
        .map(|(f, c)| {
            let (name, module) = p.funcs[f as usize];
            Func { name: p.strings[name as usize].clone(), module: p.strings[module as usize].clone(), samples: c }
        })
        .collect()
}

pub fn summarize(p: &Profile, meta: &str, n: usize) -> Summary {
    let is_kernel = |f: u32| p.strings[p.funcs[f as usize].1 as usize] == KERNEL;
    let mut kernel: HashMap<u32, u64> = HashMap::new();
    let mut progs: HashMap<String, (Vec<u32>, u64, u64, HashMap<u32, u64>, HashMap<u32, u64>)> = HashMap::new();
    let (mut busy, mut idle, mut kernel_samples) = (0, 0, 0);
    for w in &p.windows {
        idle += w.idle;
        for &(pi, si, c) in &w.counts {
            busy += c;
            let stack = &p.stacks[si as usize];
            let (name, pid) = p.progs[pi as usize];
            let e = progs.entry(p.strings[name as usize].clone()).or_default();
            if !e.0.contains(&pid) {
                e.0.push(pid);
            }
            e.1 += c;
            if let Some(&leaf) = stack.first() {
                *e.3.entry(leaf).or_insert(0) += c;
                if is_kernel(leaf) {
                    *kernel.entry(leaf).or_insert(0) += c;
                    kernel_samples += c;
                    e.2 += c;
                }
            }
            let mut seen: Vec<u32> = Vec::new();
            for &f in stack {
                if !seen.contains(&f) {
                    seen.push(f);
                    *e.4.entry(f).or_insert(0) += c;
                }
            }
        }
    }
    let mut programs: Vec<Program> = progs
        .into_iter()
        .map(|(name, (mut pids, samples, ks, own, all))| {
            pids.sort_unstable();
            Program { name, pids, samples, kernel_samples: ks, top: top(own, p, n), total_top: top(all, p, n) }
        })
        .collect();
    programs.sort_by(|a, b| b.samples.cmp(&a.samples).then(a.name.cmp(&b.name)));
    programs.truncate(n);
    Summary {
        seconds: meta_num(meta, "seconds"),
        cpus: meta_num(meta, "cpus") as u64,
        hz: meta_num(meta, "hz") as u64,
        lost: meta_num(meta, "lost_records") as u64,
        cost: meta_num(meta, "cost_share"),
        busy,
        idle,
        kernel_samples,
        kernel_top: top(kernel, p, n),
        programs,
    }
}

fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 / whole as f64 }
}

/// "12,3 %" (German decimal comma).
fn pct(x: f64) -> String {
    format!("{:.1} %", x * 100.0).replace('.', ",")
}

pub fn text(s: &Summary) -> String {
    let mut o = String::new();
    let all = s.busy + s.idle;
    let _ = writeln!(o, "Messung: {:.0} Sekunden, {} Prozessoren, {} Proben pro Sekunde und Prozessor", s.seconds, s.cpus, s.hz);
    let _ = writeln!(o, "Proben: {all}, davon belegt {} ({}), Leerlauf {}, verloren: {}", s.busy, pct(share(s.busy, all)), s.idle, s.lost);
    let _ = writeln!(o, "Kosten der Messung: {} der Rechenzeit (etwas zu niedrig: der Wechsel in die Messung ist nicht mitgezählt)", format!("{:.2} %", s.cost * 100.0).replace('.', ","));
    let _ = writeln!(o, "\nKern: {} der belegten Zeit", pct(share(s.kernel_samples, s.busy)));
    for f in &s.kernel_top {
        let _ = writeln!(o, "  {:>7}  {}", pct(share(f.samples, s.busy)), f.name);
    }
    let _ = writeln!(o, "\nProgramme:");
    for p in &s.programs {
        let _ = writeln!(o, "  {} ({} der belegten Zeit, davon im Kern {})", p.name, pct(share(p.samples, s.busy)), pct(share(p.kernel_samples, p.samples)));
        for f in &p.top {
            let place = if f.module == KERNEL { "Kern".to_string() } else { f.module.clone() };
            let _ = writeln!(o, "    {:>7}  {}  [{place}]", pct(share(f.samples, s.busy)), f.name);
        }
    }
    o
}

fn funcs_json(v: &[Func], busy: u64) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|f| format!("{{\"function\": {}, \"module\": {}, \"samples\": {}, \"share\": {:.4}}}", json_str(&f.name), json_str(&f.module), f.samples, share(f.samples, busy)))
        .collect();
    format!("[{}]", items.join(", "))
}

pub fn json(s: &Summary) -> String {
    let progs: Vec<String> = s
        .programs
        .iter()
        .map(|p| {
            format!(
                "{{\"name\": {}, \"pids\": {:?}, \"samples\": {}, \"share\": {:.4}, \"kernel_samples\": {}, \"top\": {}, \"total_top\": {}}}",
                json_str(&p.name),
                p.pids,
                p.samples,
                share(p.samples, s.busy),
                p.kernel_samples,
                funcs_json(&p.top, s.busy),
                funcs_json(&p.total_top, s.busy)
            )
        })
        .collect();
    format!(
        "{{\"seconds\": {:.1}, \"cpus\": {}, \"hz\": {}, \"samples\": {}, \"busy_samples\": {}, \"idle_samples\": {}, \"lost_records\": {}, \"cost_share\": {:.4}, \"kernel\": {{\"samples\": {}, \"share\": {:.4}, \"top\": {}}}, \"programs\": [{}]}}\n",
        s.seconds,
        s.cpus,
        s.hz,
        s.busy + s.idle,
        s.busy,
        s.idle,
        s.lost,
        s.cost,
        s.kernel_samples,
        share(s.kernel_samples, s.busy),
        funcs_json(&s.kernel_top, s.busy),
        progs.join(", ")
    )
}

pub fn report(dir: &Path, n: usize, as_json: bool) -> Result<String, String> {
    let bin = std::fs::read(dir.join("profile.bin")).map_err(|e| format!("{}/profile.bin fehlt ({e}).", dir.display()))?;
    let meta = std::fs::read_to_string(dir.join("meta.json")).unwrap_or_default();
    let p = Profile::decode(&bin)?;
    let s = summarize(&p, &meta, n);
    Ok(if as_json { json(&s) } else { text(&s) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Builder, Window};

    #[test]
    fn hottest_first() {
        let mut b = Builder::default();
        let hot = b.func("hot_loop", "/bin/busy");
        let main = b.func("main", "/bin/busy");
        let sched = b.func("sched::run::schedule", KERNEL);
        let busy = b.prog("busy", 7);
        let other = b.prog("other", 8);
        let s1 = b.stack(&[hot, main]);
        let s2 = b.stack(&[sched, main]);
        b.p.windows.push(Window { start_ms: 0, len_ms: 1000, idle: 50, counts: vec![(busy, s1, 80), (busy, s2, 10), (other, s2, 10)] });
        let s = summarize(&b.p, "{\"seconds\": 1.0, \"cpus\": 2, \"hz\": 1000, \"lost_records\": 3}", 5);
        assert_eq!((s.busy, s.idle, s.kernel_samples, s.lost, s.cpus), (100, 50, 20, 3, 2));
        assert_eq!(s.programs[0].name, "busy");
        assert_eq!(s.programs[0].top[0].name, "hot_loop");
        assert_eq!(s.programs[0].total_top[0].name, "main");
        assert_eq!(s.kernel_top[0].samples, 20);
        let t = text(&s);
        assert!(t.contains("80,0 %  hot_loop"), "{t}");
        let j = json(&s);
        assert!(j.contains("\"function\": \"hot_loop\", \"module\": \"/bin/busy\", \"samples\": 80, \"share\": 0.8000"), "{j}");
        assert_eq!(json_str("a\"b\\\n"), "\"a\\\"b\\\\\\u000a\"");
    }
}
