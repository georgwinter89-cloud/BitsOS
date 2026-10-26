//! WY-5b: foot's colours follow the theme. bitscomp writes `$XDG_RUNTIME_DIR/bitscomp-foot.ini` from the theme
//! tokens (foot.ini(5) of foot 1.28: [colors-dark] and [colors-light], initial-color-theme) and starts every foot
//! with `--config=` that file (added at exec time only: the session keeps the argv without it, so a restored
//! session takes the colours of its day). Background = `paper` (a shown tab runs into its content), foreground =
//! `text`, cursor = the accent with the paper as the text under it; the 16 ANSI colours are fixed per mode, chosen
//! for at least 4.5:1 against the default paper (the colour meant to sit on the background's side, black in dark
//! and white in light, excepted). A mode change reaches running terminals at once: foot switches its theme on
//! SIGUSR1 (dark) / SIGUSR2 (light), foot(1) SIGNALS. Accent and palette changes only reach new terminals (foot
//! cannot load its configuration again while it runs). The default action of SIGUSR1/2 ends a process, so a foot
//! is only signalled once its window has shown a first frame (its handlers are in place by then); a mode change
//! before that reaches it at that first frame.

use std::path::{Path, PathBuf};

use crate::settings::Values;
use crate::theme::{Rgba, Theme, alpha};

pub const FILE: &str = "bitscomp-foot.ini";

/// regular0..7, bright0..7 for a dark background.
pub const ANSI_DARK: [&str; 16] = [
    "2a3a4d", "ff6b6b", "3ddc97", "ffb020", "5aa9ff", "b388ff", "00e5ff", "c9d4e0", // regular
    "93a4b8", "ff8f8f", "6ff0b4", "ffd166", "8cc4ff", "cfb3ff", "6ff3ff", "ffffff", // bright
];
/// regular0..7, bright0..7 for a light background.
pub const ANSI_LIGHT: [&str; 16] = [
    "1c2733", "c62828", "1b7f4c", "8a5a00", "1f5fbf", "7b3fc4", "00788a", "d3dce6", // regular
    "4d5b6b", "d32f2f", "16804a", "946200", "2a6ad0", "8646d4", "00808f", "f2f5f8", // bright
];

/// A premultiplied token as foot's RRGGBB (opaque tokens: the same numbers).
pub fn rrggbb(c: Rgba) -> String {
    let a = if c[3] > 0.0 { c[3] } else { 1.0 };
    let v = |x: f32| ((x / a).clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02x}{:02x}{:02x}", v(c[0]), v(c[1]), v(c[2]))
}

/// Premultiplied `a` over opaque `b`.
fn over(a: Rgba, b: Rgba) -> Rgba {
    let k = 1.0 - a[3];
    [a[0] + b[0] * k, a[1] + b[1] * k, a[2] + b[2] * k, 1.0]
}

/// The theme of `v` in mode `dark` (whatever theme.mode says).
fn theme_in(v: &Values, dark: bool) -> Theme {
    let mut v = v.clone();
    v.set("theme.mode", if dark { "dark" } else { "light" });
    Theme::build(&v)
}

fn section(out: &mut String, name: &str, t: &Theme, ansi: &[&str; 16]) {
    let (bg, fg, acc) = (rrggbb(t.paper), rrggbb(t.text), rrggbb(t.accent));
    out.push_str(&format!("\n[{name}]\nbackground={bg}\nforeground={fg}\ncursor={bg} {acc}\n"));
    for (i, c) in ansi.iter().enumerate() {
        let (kind, n) = if i < 8 { ("regular", i) } else { ("bright", i - 8) };
        out.push_str(&format!("{kind}{n}={c}\n"));
    }
    out.push_str(&format!("selection-foreground={fg}\nselection-background={}\n", rrggbb(over(alpha(t.accent, 0.3), t.paper))));
}

/// foot.ini for the settings `v`.
pub fn ini(v: &Values) -> String {
    let dark = Theme::build(v).dark;
    let mut out = String::from("# Written by bitscomp from the theme (theme.*); changes here are overwritten.\n");
    out.push_str(&format!("[main]\ninitial-color-theme={}\n", if dark { "dark" } else { "light" }));
    section(&mut out, "colors-dark", &theme_in(v, true), &ANSI_DARK);
    section(&mut out, "colors-light", &theme_in(v, false), &ANSI_LIGHT);
    out
}

pub fn path(rundir: &Path) -> PathBuf {
    rundir.join(FILE)
}

/// Writes `text` to `p` through a temporary file and a rename, so a starting foot never reads half of it.
pub fn write(p: &Path, text: &str) -> std::io::Result<()> {
    let tmp = p.with_extension("ini.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p)
}

/// Whether `argv` starts foot (its program name, a path to it too).
pub fn is_foot(argv0: &str) -> bool {
    Path::new(argv0).file_name().is_some_and(|n| n == "foot")
}

/// foot's signal for a theme mode (foot(1) SIGNALS).
pub fn mode_signal(dark: bool) -> libc::c_int {
    if dark { libc::SIGUSR1 } else { libc::SIGUSR2 }
}

/// Sends `sig` to `pid` (a foot bitscomp started and has not reaped); errors only logged by the caller.
pub fn signal(pid: u32, sig: libc::c_int) -> std::io::Result<()> {
    let p = libc::pid_t::try_from(pid).map_err(|_| std::io::Error::from_raw_os_error(libc::ESRCH))?;
    // SAFETY: kill(2) with a pid of a child of ours (not yet reaped, so the pid is not reused) and a plain signal.
    if unsafe { libc::kill(p, sig) } < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// What bitscomp last wrote and the mode running terminals were told.
#[derive(Default)]
pub struct FootState {
    /// Where foot.ini goes (`None` until the run directory is known).
    pub path: Option<PathBuf>,
    /// The file is written and current: foot is started with it.
    pub ready: bool,
    pub text: String,
    pub dark: Option<bool>,
    /// Each foot bitscomp started: the mode it shows (or will show at its first frame).
    pub shows: std::collections::HashMap<u32, bool>,
    /// The feet of `shows` that drew a first frame: only these may be signalled.
    pub drawn: std::collections::HashSet<u32>,
}

impl FootState {
    /// The feet to signal for `dark` now, and how many wait for their first frame.
    pub fn due(&self, dark: bool) -> (Vec<u32>, usize) {
        let off = self.shows.iter().filter(|(_, d)| **d != dark).map(|(p, _)| *p);
        let (mut now, later): (Vec<u32>, Vec<u32>) = off.partition(|p| self.drawn.contains(p));
        now.sort_unstable();
        (now, later.len())
    }

    /// The mode a foot started now shows: the file's, else foot's own default (colors-dark).
    pub fn started(&mut self, pid: u32, with_file: bool) {
        self.shows.insert(pid, if with_file { self.dark.unwrap_or(true) } else { true });
    }

    /// Forgets the feet that are no longer running.
    pub fn retain(&mut self, alive: impl Fn(u32) -> bool) {
        self.shows.retain(|p, _| alive(*p));
        self.drawn.retain(|p| alive(*p));
    }
}

impl crate::state::App {
    /// foot.ini follows the settings; a mode change switches the running terminals bitscomp started.
    pub fn foot_sync(&mut self) {
        let Some(p) = self.foot.path.clone() else { return };
        let text = ini(&self.settings);
        if text != self.foot.text {
            match write(&p, &text) {
                Ok(()) => {
                    self.foot.ready = true;
                    eprintln!("bitscomp: foot colours written to {}", p.display());
                }
                Err(e) => {
                    // Fallback: foot's own colours (a stale file would show an old theme).
                    self.foot.ready = false;
                    let _ = std::fs::remove_file(&p);
                    eprintln!("bitscomp: foot colours: {}: {e} (new terminals use foot's own colours)", p.display());
                }
            }
            self.foot.text = text;
        }
        let dark = self.theme.dark;
        if self.foot.dark.replace(dark) != Some(dark) {
            self.foot_tell(dark);
        }
    }

    /// Switches the drawn feet that show the other mode; the others get it at their first frame.
    fn foot_tell(&mut self, dark: bool) {
        let (now, later) = self.foot.due(dark);
        if now.is_empty() && later == 0 {
            return;
        }
        let mut told = 0;
        for pid in &now {
            match signal(*pid, mode_signal(dark)) {
                Ok(()) => {
                    told += 1;
                    self.foot.shows.insert(*pid, dark);
                }
                Err(e) => eprintln!("bitscomp: foot {pid}: theme signal: {e}"),
            }
        }
        let mode = if dark { "dark" } else { "light" };
        eprintln!("bitscomp: {told} of {} terminals switched to {mode}", now.len());
        if later > 0 {
            eprintln!("bitscomp: {later} terminals not drawn yet: {mode} at their first frame");
        }
    }

    /// A window of `pid` drew its first frame: a foot of ours can take signals now (and gets a missed switch).
    pub fn foot_drawn(&mut self, pid: u32) {
        if self.foot.shows.contains_key(&pid) && self.foot.drawn.insert(pid) {
            self.foot_tell(self.theme.dark);
        }
    }

    /// The `--config=` argument for a foot started now, when the file is current.
    pub fn foot_config_arg(&self) -> Option<String> {
        self.foot.path.as_ref().filter(|_| self.foot.ready).map(|p| format!("--config={}", p.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(h: &str) -> f64 {
        let c = |i: usize| {
            let v = u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * c(0) + 0.7152 * c(2) + 0.0722 * c(4)
    }

    /// WCAG contrast ratio.
    fn contrast(a: &str, b: &str) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    fn get<'a>(ini: &'a str, section: &str, key: &str) -> &'a str {
        let s = ini.split(&format!("[{section}]\n")).nth(1).unwrap();
        s.lines().find_map(|l| l.strip_prefix(&format!("{key}="))).unwrap()
    }

    #[test]
    fn the_ini_carries_the_theme_tokens() {
        let mut v = Values::default();
        let i = ini(&v);
        assert!(i.contains("initial-color-theme=dark"));
        assert_eq!(get(&i, "colors-dark", "background"), "131c26");
        assert_eq!(get(&i, "colors-dark", "foreground"), "e6edf3");
        assert_eq!(get(&i, "colors-dark", "cursor"), "131c26 00e5ff");
        assert_eq!(get(&i, "colors-light", "background"), "ffffff");
        assert_eq!(get(&i, "colors-light", "foreground"), "101820");
        assert_eq!(get(&i, "colors-light", "regular1"), ANSI_LIGHT[1]);
        assert_eq!(get(&i, "colors-dark", "bright7"), ANSI_DARK[15]);
        v.set("theme.mode", "light");
        v.set("theme.accent", "#ffb020");
        v.set("theme.dark.paper", "#202020");
        let i = ini(&v);
        assert!(i.contains("initial-color-theme=light"));
        assert_eq!(get(&i, "colors-light", "cursor"), "ffffff ffb020");
        assert_eq!(get(&i, "colors-dark", "background"), "202020");
    }

    #[test]
    fn ansi_colours_are_readable_on_both_papers() {
        let i = ini(&Values::default());
        for (sec, ansi, own) in [("colors-dark", &ANSI_DARK, [0usize].as_slice()), ("colors-light", &ANSI_LIGHT, [7, 15].as_slice())] {
            let bg = get(&i, sec, "background");
            assert!(contrast(get(&i, sec, "foreground"), bg) >= 7.0, "{sec} text");
            for (n, c) in ansi.iter().enumerate().filter(|(n, _)| !own.contains(n)) {
                let r = contrast(c, bg);
                assert!(r >= 4.5, "{sec} colour {n} #{c}: {r:.2}:1 on #{bg}");
            }
        }
    }

    #[test]
    fn only_drawn_feet_are_signalled() {
        let mut f = FootState { dark: Some(true), ..Default::default() };
        f.started(10, true);
        f.started(11, true);
        f.dark = Some(false);
        f.started(12, true);
        f.drawn.insert(10);
        // 10 is drawn and dark: now; 11 waits for its first frame; 12 already starts light.
        assert_eq!(f.due(false), (vec![10], 1));
        assert_eq!(f.due(true), (vec![], 1));
        f.retain(|p| p != 11);
        assert_eq!(f.due(false), (vec![10], 0));
        let mut g = FootState::default();
        g.started(5, false);
        assert_eq!(g.shows[&5], true, "without the file foot starts in colors-dark");
    }

    #[test]
    fn foot_is_known_by_its_name_and_the_signals_by_mode() {
        assert!(is_foot("foot") && is_foot("/usr/bin/foot"));
        assert!(!is_foot("footclient") && !is_foot("bitscomp"));
        assert_eq!(mode_signal(true), libc::SIGUSR1);
        assert_eq!(mode_signal(false), libc::SIGUSR2);
        assert_eq!(rrggbb([0.5, 0.25, 0.0, 0.5]), "ff8000");
    }
}
