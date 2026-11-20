//! The cache of `xtask stable-list`: what the logs read so far said, so a log deleted since still counts.
use crate::history::{History, Tally};

const VERSION: &str = "stable-list cache v1";

impl History {
    /// Tab-separated lines; keys and test paths hold no tab.
    pub fn cache_text(&self) -> String {
        let mut s = format!("{VERSION}\nfiles\t{}\n", self.files);
        if let Some((a, b)) = self.span {
            s.push_str(&format!("span\t{a}\t{b}\n"));
        }
        for d in &self.logs {
            s.push_str(&format!("log\t{d:016x}\n"));
        }
        for (k, t) in &self.tests {
            s.push_str(&format!("test\t{k}\t{}\t{}\n", t.green, t.red));
        }
        for (t, r) in &self.homes {
            s.push_str(&format!("home\t{t}\t{r}\n"));
        }
        for (t, p) in &self.unplaced_red {
            s.push_str(&format!("unplaced\t{}\t{p}\n", t.as_deref().unwrap_or("-")));
        }
        s
    }

    pub fn parse_cache(text: &str) -> Result<History, String> {
        let mut lines = text.lines();
        if lines.next() != Some(VERSION) {
            return Err(format!("stable-list cache: not a `{VERSION}` file (delete it to start afresh)"));
        }
        let mut h = History::default();
        for l in lines {
            let f: Vec<&str> = l.split('\t').collect();
            let bad = || format!("stable-list cache: bad line {l:?}");
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok()).ok_or_else(bad);
            match f[0] {
                "files" => h.files = num(1)? as usize,
                "span" => h.span = Some((num(1)?, num(2)?)),
                "log" => {
                    h.logs.insert(f.get(1).and_then(|v| u64::from_str_radix(v, 16).ok()).ok_or_else(bad)?);
                }
                "test" if f.len() == 4 => {
                    h.tests.insert(f[1].to_string(), Tally { green: num(2)? as usize, red: num(3)? as usize });
                }
                "home" if f.len() == 3 => {
                    h.homes.insert(f[1].to_string(), f[2].to_string());
                }
                "unplaced" if f.len() == 3 => h.unplaced_red.push(((f[1] != "-").then(|| f[1].to_string()), f[2].to_string())),
                _ => return Err(bad()),
            }
        }
        Ok(h)
    }
}
