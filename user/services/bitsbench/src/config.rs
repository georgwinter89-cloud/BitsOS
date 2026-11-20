//! What the set starts: the programs' commands and where things lie. Defaults are the stick's launchers on
//! BitsOS and the usual names under CachyOS; `<dir>/bench.conf` (lines `key = value`, words split at spaces) may
//! replace any of them (the QEMU scenario points them at the fakes, Brave, Steam and games run only on the
//! Mini-PC). Unknown keys are reported, never guessed.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub root: PathBuf,
    pub brave: Vec<String>,
    pub steam: Vec<String>,
    pub steam_stop: Vec<String>,
    /// The app id (or its start) of Steam's first window; `steam_app_*` (a game) never counts.
    pub steam_window: String,
    pub game: Vec<String>,
    pub bitsprof: Vec<String>,
    /// Folders the memory benchmark's readers read (the desk's load round reads the Steam games).
    pub load_dirs: Vec<PathBuf>,
    /// Only these benchmarks (empty: all of the set).
    pub only: Vec<String>,
    pub runs: usize,
    /// BitsOS (its /proc/bitsos) or another Linux (the CachyOS comparison).
    pub bitsos: bool,
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

impl Config {
    pub fn defaults(root: &Path, bitsos: bool) -> Config {
        let tmp = root.join("tmp");
        let brave_flags = format!("--remote-debugging-pipe --mute-audio --no-first-run --no-default-browser-check --user-data-dir={}", tmp.join("brave").display());
        let (brave, steam, steam_stop, game) = if bitsos {
            (
                words(&format!("/bin/sh /boot/BITSOS2/bin/brave -- {brave_flags}")),
                words("/bin/sh /boot/BITSOS2/bin/steam"),
                // The launcher's own stop (user/ports/steam/bitsos/steam): Steam's -shutdown, then TERM and KILL.
                words("/bin/sh /boot/BITSOS2/bin/steam --stop"),
                words("/bin/sh /boot/BITSOS2/bin/vkquake -nosound -noudp +timedemo demo1"),
            )
        } else {
            (words(&format!("brave {brave_flags}")), words("steam"), words("steam -shutdown"), words("vkquake -nosound -noudp +timedemo demo1"))
        };
        Config {
            root: root.to_path_buf(),
            brave,
            steam,
            steam_stop,
            steam_window: "steam".into(),
            game,
            bitsprof: words(if bitsos { "/bin/bitsprof" } else { "bitsprof" }),
            load_dirs: vec![PathBuf::from("/data/home/1000/.local/share/Steam/steamapps/common")],
            only: Vec::new(),
            runs: crate::catalog::RUNS,
            bitsos,
        }
    }

    /// The defaults with `text` (bench.conf) over them; the second value lists lines that were not understood.
    pub fn with_text(mut self, text: &str) -> (Config, Vec<String>) {
        let mut bad = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let Some((k, v)) = line.split_once('=') else {
                bad.push(line.to_string());
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "brave" => self.brave = words(v),
                "steam" => self.steam = words(v),
                "steam_stop" => self.steam_stop = words(v),
                "steam_window" => self.steam_window = v.to_string(),
                "game" => self.game = words(v),
                "bitsprof" => self.bitsprof = words(v),
                "load_dirs" => self.load_dirs = v.split_whitespace().map(PathBuf::from).collect(),
                "only" => self.only = v.split([',', ' ']).filter(|s| !s.is_empty()).map(String::from).collect(),
                "runs" => match v.parse::<usize>() {
                    Ok(n) if (1..=crate::catalog::RUNS).contains(&n) => self.runs = n,
                    _ => bad.push(line.to_string()),
                },
                _ => bad.push(line.to_string()),
            }
        }
        (self, bad)
    }

    pub fn load(root: &Path, bitsos: bool) -> Config {
        let c = Config::defaults(root, bitsos);
        let text = std::fs::read_to_string(root.join("bench.conf")).unwrap_or_default();
        let (c, bad) = c.with_text(&text);
        for b in bad {
            crate::events::event("config.line_ignored", &format!("line={}", crate::events::q(&b)));
        }
        c
    }

    pub fn wants(&self, id: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|o| o == id)
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

/// Whether the program a command starts is there: the script after `sh`, else the program itself (absolute,
/// or found in PATH).
pub fn present(cmd: &[String]) -> bool {
    let target = match cmd {
        [sh, script, ..] if sh.ends_with("/sh") && !script.starts_with('-') => script.as_str(),
        [p, ..] => p.as_str(),
        [] => return false,
    };
    if target.contains('/') {
        return Path::new(target).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(target).is_file()))
}
