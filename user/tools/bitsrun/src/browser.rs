//! BR-9: a Chromium browser from the repositories (Arch's `brave-bin`) starts as the stick's launcher starts Brave
//! (user/ports/brave-launcher/bitsos/brave; plan/spec/programs.md 1.5). Pure: the facts come in, the plan bitsrun
//! made for the program's view is rewritten (program, arguments, environment, notes).
//!
//! The vendor's start script (`/usr/bin/brave`: flags from `~/.config/brave-flags.conf`, then the binary) is
//! skipped: the binary gets the launcher's flags itself. The view stays bitsrun's; Chromium's namespace sandbox
//! nests inside it (bwrap's user namespace is level 1, Chromium's level 2; Linux allows 32,
//! linux-7.3.1/kernel/user_namespace.c:92), never `--no-sandbox` (BR-6b).

use crate::plan::{Kind, Plan, Tree};
use std::collections::BTreeMap;
use std::ffi::OsString;

/// A browser tree bitsrun knows.
pub struct Browser {
    /// What the notes call it.
    pub name: &'static str,
    /// The binary, relative to the tree's root (in the view: the same path under `/`).
    pub binary: &'static str,
    /// The vendor's start script, which a `.desktop` file names; the binary is started instead.
    pub wrapper: &'static str,
    /// The profile below the program's data folder: Chromium's default place under `XDG_CONFIG_HOME`, which
    /// bitsrun sets to `<data>/config`.
    pub profile: &'static str,
    /// The data folder (`/data/appdata/<this>`) of the stick's launcher, whose profile the first start takes over.
    pub stick_data: &'static str,
}

pub const BRAVE: Browser = Browser {
    name: "Brave",
    binary: "opt/brave-bin/brave",
    wrapper: "usr/bin/brave",
    profile: "config/BraveSoftware/Brave-Browser",
    stick_data: "brave",
};

const KNOWN: [&Browser; 1] = [&BRAVE];

/// The stick launcher's FLAGS (BR-1's reference run, BR-8's GPU, text-input-v3); tests/browser.rs compares them with the
/// launcher's file, so the two Braves never drift apart.
pub const FLAGS: [&str; 12] = [
    "--ozone-platform=wayland",
    "--use-gl=angle",
    "--use-angle=gl",
    "--enable-features=AcceleratedVideoDecoder,AcceleratedVideoDecodeLinuxGL,VaapiIgnoreDriverChecks",
    "--password-store=basic",
    "--disable-breakpad",
    "--disable-crash-reporter",
    "--no-first-run",
    "--no-default-browser-check",
    "--autoplay-policy=no-user-gesture-required",
    "--enable-wayland-ime",
    "--wayland-text-input-version=3",
];

/// The browser whose binary or start script `program` (a path in the view) is, when the tree has the binary.
pub fn detect(t: &dyn Tree, program: &str) -> Option<&'static Browser> {
    let rel = program.trim_start_matches('/');
    KNOWN.iter().copied().find(|b| (rel == b.binary || rel == b.wrapper) && t.kind(b.binary) == Kind::File)
}

/// What the start needs to know about the desktop.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// The desktop is dark now (settingsd's theme.mode, `dark_now`).
    pub dark: bool,
    /// The accessibility bus's socket when atspid runs (Durchblick, DB5a).
    pub a11y_bus: Option<String>,
    /// The caller's XCURSOR_SIZE when it is a number.
    pub cursor_size: Option<String>,
}

/// An hour setting as bitscomp's theme_auto.rs and the launcher read it: digits only, at most 23, else `default`.
fn hour_or(v: Option<&str>, default: u32) -> u32 {
    match v.map(str::trim) {
        Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse::<u32>().map_or(23, |h| h.min(23)),
        _ => default,
    }
}

/// The launcher's `theme_dark`: "dark" is dark; "auto" ("Nach Uhrzeit") is light from `light_from` (7) up to
/// `dark_from` (19), across midnight too, dark the rest; anything else is light.
pub fn dark_now(mode: &str, hour: Option<&str>, light_from: Option<&str>, dark_from: Option<&str>) -> bool {
    match mode.trim() {
        "dark" => true,
        "auto" => {
            let (h, f, t) = (hour_or(hour, 0), hour_or(light_from, 7), hour_or(dark_from, 19));
            if f <= t { h < f || h >= t } else { h < f && h >= t }
        }
        _ => false,
    }
}

/// The program's arguments in the launcher's form `[URL] [-- Brave arguments]`: the first lone `--` goes
/// (Chromium would take every switch after it for an address, as the sofa's `-- --start-fullscreen`).
fn user_args(args: &[OsString]) -> Vec<OsString> {
    let mut v = args.to_vec();
    if let Some(i) = v.iter().position(|a| a == "--") {
        v.remove(i);
    }
    v
}

/// Rewrites the view's plan for browser `b`: the binary with the launcher's flags, its environment, the notes.
pub fn adjust(p: &mut Plan, b: &Browser, f: &Facts) {
    let mut argv: Vec<OsString> = vec![OsString::from(format!("/{}", b.binary))];
    argv.extend(FLAGS.iter().map(OsString::from));
    if f.dark {
        argv.push("--force-dark-mode".into());
    }
    if f.a11y_bus.is_some() {
        argv.push("--force-renderer-accessibility".into());
    }
    argv.extend(user_args(p.argv.get(1..).unwrap_or(&[])));
    p.argv = argv;

    let mut env: BTreeMap<String, String> = p.env.drain(..).collect();
    let mut set = |k: &str, v: &str| {
        env.insert(k.to_string(), v.to_string());
    };
    // No portal, D-Bus or PulseAudio here: Chromium then uses ALSA; GSettings from memory (no dconf).
    for (k, v) in [
        ("GTK_USE_PORTAL", "0"),
        ("DBUS_SESSION_BUS_ADDRESS", "disabled:"),
        ("DBUS_SYSTEM_BUS_ADDRESS", "disabled:"),
        ("PULSE_SERVER", "none"),
        ("GSETTINGS_BACKEND", "memory"),
        ("XCURSOR_THEME", "Adwaita"),
    ] {
        set(k, v);
    }
    set("XCURSOR_SIZE", f.cursor_size.as_deref().unwrap_or("32"));
    match &f.a11y_bus {
        Some(bus) => {
            set("AT_SPI_BUS_ADDRESS", &format!("unix:path={bus}"));
            set("ACCESSIBILITY_ENABLED", "1");
        }
        None => set("NO_AT_BRIDGE", "1"),
    }
    env.entry("LANG".into()).or_insert_with(|| "de_DE.UTF-8".into());
    // Chromium's singleton socket lies in a fresh folder under TMPDIR (process_singleton_posix.cc); every view has
    // a /tmp of its own, so a second start would not reach the running Brave. The runtime folder is bound at the
    // same path into every view: there the second start hands its address over.
    let tmp = env.get("XDG_RUNTIME_DIR").cloned();
    match tmp {
        Some(r) => {
            env.insert("TMPDIR".into(), r);
        }
        None => p.notes.push(format!("{}: kein Laufzeitordner, ein zweiter Start erreicht das laufende {} nicht", b.name, b.name)),
    }
    p.env = env.into_iter().collect();
    p.notes.push(format!(
        "{}: wie vom Stick gestartet (/{} statt /{}, Wayland, Grafikchip über Mesa, eigene Sandbox, Profil {})",
        b.name, b.binary, b.wrapper, b.profile
    ));
}
