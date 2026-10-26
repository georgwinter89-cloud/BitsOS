//! Shared helpers: repo root, lane identity, ports, argument parsing, running commands.
use std::path::{Path, PathBuf};
use std::process::Command;

/// The repo root: `host/xtask/../..`, canonical.
pub fn root() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    p.canonicalize().unwrap_or(p)
}

/// `<root>/target/xtask`, where every run artefact lives.
pub fn xdir(root: &Path) -> PathBuf {
    root.join("target").join("xtask")
}

/// Lane identity: env `BITSOS_LANE`, else the first line of `<root>/.lane`, else `desk`.
pub fn lane(root: &Path) -> String {
    if let Ok(v) = std::env::var("BITSOS_LANE") {
        if !v.trim().is_empty() {
            return v.trim().to_lowercase();
        }
    }
    if let Ok(s) = std::fs::read_to_string(root.join(".lane")) {
        if let Some(l) = s.lines().next() {
            if !l.trim().is_empty() {
                return l.trim().to_lowercase();
            }
        }
    }
    "desk".to_string()
}

/// Port ranges per lane (V2-LANES.md section 6). Inclusive.
const LANE_PORTS: &[(&str, u16, u16)] = &[
    ("desk", 20000, 20499), ("bt", 20500, 20999), ("cp", 21000, 21499), ("mm", 21500, 21999),
    ("sc", 22000, 22499), ("pc", 22500, 22999), ("rb", 23000, 23499), ("ab", 23500, 23999),
    ("fs", 24000, 24499), ("nw", 24500, 24999), ("mc", 25000, 25499), ("us", 25500, 25999),
    ("in", 26000, 26499), ("dp", 26500, 26999), ("gx", 27000, 27499), ("sn", 27500, 27999),
    ("wl", 28000, 28499), ("ul", 28500, 28999), ("yt", 29000, 29499), ("nv", 19000, 19499),
    ("ks", 19500, 19999), ("cx", 29500, 29999), ("cl", 30000, 30499), ("wy", 30500, 30999),
    ("br", 31000, 31499), ("nd", 31500, 31999), ("gm", 32000, 32499), ("sx", 32500, 32767),
    ("vp", 16500, 16999), ("pm", 17000, 17499), ("ac", 17500, 17999), ("vs", 18000, 18499),
    ("pk", 18500, 18999),
];

pub fn lane_range(lane: &str) -> Option<(u16, u16)> {
    let l = lane.to_lowercase();
    LANE_PORTS.iter().find(|(n, _, _)| *n == l).map(|&(_, lo, hi)| (lo, hi))
}

/// Minimal option parser. `--name value` and `--name=value`; everything after a lone `--` is `extra`.
pub struct Args {
    pub args: Vec<String>,
    pub extra: Vec<String>,
}

impl Args {
    pub fn new(mut a: Vec<String>) -> Args {
        let extra = match a.iter().position(|x| x == "--") {
            Some(i) => {
                let e = a.split_off(i);
                e[1..].to_vec()
            }
            None => Vec::new(),
        };
        Args { args: a, extra }
    }

    pub fn flag(&mut self, name: &str) -> bool {
        match self.args.iter().position(|x| x == name) {
            Some(i) => {
                self.args.remove(i);
                true
            }
            None => false,
        }
    }

    /// All values of a repeatable option.
    pub fn values(&mut self, name: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let pre = format!("{name}=");
        let mut i = 0;
        while i < self.args.len() {
            if self.args[i] == name {
                if i + 1 >= self.args.len() {
                    return Err(format!("{name} needs a value"));
                }
                out.push(self.args[i + 1].clone());
                self.args.drain(i..i + 2);
            } else if let Some(v) = self.args[i].strip_prefix(&pre) {
                out.push(v.to_string());
                self.args.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(out)
    }

    pub fn value(&mut self, name: &str) -> Result<Option<String>, String> {
        Ok(self.values(name)?.pop())
    }

    pub fn num<T: std::str::FromStr>(&mut self, name: &str) -> Result<Option<T>, String> {
        match self.value(name)? {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| format!("{name}: bad number '{v}'")),
        }
    }

    /// What is left must be positional (no leading dash).
    pub fn positionals(&self) -> Result<Vec<String>, String> {
        if let Some(bad) = self.args.iter().find(|a| a.starts_with('-') && a.len() > 1) {
            return Err(format!("unknown option {bad}"));
        }
        Ok(self.args.clone())
    }
}

/// `cargo` as the one that started us (keeps the pinned toolchain), without any inherited target dir so the
/// artefacts land under `<root>/target` where the rest of xtask looks for them.
pub fn cargo(root: &Path) -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root).env_remove("CARGO_TARGET_DIR");
    c
}

pub fn run_status(c: &mut Command, what: &str) -> Result<(), String> {
    eprintln!("xtask: {what}");
    let st = c.status().map_err(|e| format!("{what}: cannot start: {e}"))?;
    if st.success() { Ok(()) } else { Err(format!("{what}: failed ({st})")) }
}

#[cfg(test)]
pub fn have_tool(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Args {
        Args::new(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn options_and_extra() {
        let mut x = a(&["--name", "t", "--hostfwd=1:2", "--hostfwd", "3:4", "--release", "--", "-s"]);
        assert!(x.flag("--release"));
        assert_eq!(x.value("--name").unwrap().as_deref(), Some("t"));
        assert_eq!(x.values("--hostfwd").unwrap(), vec!["1:2", "3:4"]);
        assert_eq!(x.extra, vec!["-s"]);
        assert!(x.positionals().unwrap().is_empty());
    }

    #[test]
    fn lane_table() {
        assert_eq!(lane_range("desk"), Some((20000, 20499)));
        assert_eq!(lane_range("MM"), Some((21500, 21999)));
        assert_eq!(lane_range("nope"), None);
    }
}
