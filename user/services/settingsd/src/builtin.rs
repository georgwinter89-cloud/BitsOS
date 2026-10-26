//! The key groups settingsd brings itself. The names are an interface: WY-5 (bitscomp) reads `theme.*`, the
//! kernel reads the CONFIG.TXT names of `kernel.*`. plan/notes/CL-1.md lists them.
//!
//! Theme (plan/spec/desktop.md §2/§4): dark first, neutral colours with one accent (default holographic cyan),
//! Exo 2 plus a monospace, scale 125 %, motion medium with a calm setting, effect level cheap until the GPU path.

use crate::key::{Applier, KeyDef, Restart, Safety};
use crate::value::{Type, Value};

fn def(name: &str, ty: Type, default: Value, de: &str, en: &str, applier: Applier, restart: Restart) -> KeyDef {
    KeyDef {
        name: name.into(),
        ty,
        default,
        de: de.into(),
        en: en.into(),
        applier,
        restart,
        safety: Safety::Normal,
        owner: "settingsd".into(),
    }
}

fn choice(c: &[&str]) -> Type {
    Type::Choice(c.iter().map(|s| s.to_string()).collect())
}
fn int(min: i64, max: i64, unit: &str) -> Type {
    Type::Int { min, max, unit: unit.into(), word: None }
}
fn text(s: &str) -> Value {
    Value::Text(s.into())
}

pub fn theme() -> Vec<KeyDef> {
    let comp = || Applier::Service("bitscomp".into());
    let t = |name: &str, ty: Type, default: Value, de: &str, en: &str| {
        def(name, ty, default, de, en, comp(), Restart::None)
    };
    let mut v = vec![
        t("theme.mode", choice(&["dark", "light"]), text("dark"), "Helles oder dunkles Design", "Light or dark look"),
        t(
            "theme.accent",
            Type::Color,
            text("#00e5ff"),
            "Akzentfarbe (Standard: holografisches Cyan)",
            "Accent colour (default: holographic cyan)",
        ),
        t("theme.font.ui", Type::Text { max: 64 }, text("Exo 2"), "Schrift der Oberfläche", "Interface font"),
        t(
            "theme.font.mono",
            Type::Text { max: 64 },
            text("Noto Sans Mono"),
            "Schrift für Code und Terminals",
            "Font for code and terminals",
        ),
        t(
            "theme.font.size",
            int(8, 48, "px"),
            Value::Int(15),
            "Grundschriftgröße bei 100 %",
            "Base font size at 100 %",
        ),
        t("theme.scale", int(100, 300, "%"), Value::Int(125), "Skalierung der Oberfläche", "Interface scale"),
        t("theme.cursor.size", int(16, 128, "px"), Value::Int(32), "Größe des Mauszeigers", "Pointer size"),
        t(
            "theme.spacing",
            int(0, 64, "px"),
            Value::Int(8),
            "Grundabstand zwischen Elementen",
            "Base spacing between elements",
        ),
        t(
            "theme.radius",
            int(0, 32, "px"),
            Value::Int(8),
            "Eckenradius von Fenstern und Knöpfen",
            "Corner radius of windows and buttons",
        ),
        t(
            "theme.effects",
            choice(&["cheap", "gpu"]),
            text("cheap"),
            "Effektstufe: sparsam (ohne Grafiktreiber) oder GPU",
            "Effect level: cheap (no GPU driver) or GPU",
        ),
        t(
            "theme.motion",
            choice(&["medium", "calm", "off"]),
            text("medium"),
            "Bewegung: mittel, ruhig oder aus",
            "Motion: medium, calm or off",
        ),
        t(
            "theme.glow",
            int(0, 100, "%"),
            Value::Int(60),
            "Stärke des holografischen Leuchtens",
            "Strength of the holographic glow",
        ),
    ];
    // Neutral palettes per mode, the values of the mock-up Georg approved on 21.10.2026 (plan/spec/desktop.md §6,
    // WY-5); the accent is shared. bitscomp's built-in defaults are the same (its test reads this file).
    let palette: [(&str, &str, &str, &str, &str); 7] = [
        ("bg", "#0b0f14", "#e9eef3", "Hintergrund", "Background"),
        ("surface", "#111821", "#ffffff", "Leisten und Kacheln", "Bars and tiles"),
        ("surface2", "#17212c", "#f2f5f8", "Zweite Fläche (Reiterleiste, Felder)", "Second surface (tab strip, fields)"),
        ("paper", "#131c26", "#ffffff", "Gezeigter Reiter", "Shown tab"),
        ("text", "#e6edf3", "#101820", "Text", "Text"),
        ("text_dim", "#93a4b8", "#4d5b6b", "Nebentext", "Secondary text"),
        ("border", "#26364a", "#d3dce6", "Rahmenlinien", "Border lines"),
    ];
    for (mode, de_mode, en_mode, dark) in [("dark", "dunkel", "dark", true), ("light", "hell", "light", false)] {
        for (name, d, l, de, en) in palette {
            v.push(t(
                &format!("theme.{mode}.{name}"),
                Type::Color,
                text(if dark { d } else { l }),
                &format!("{de} ({de_mode})"),
                &format!("{en} ({en_mode})"),
            ));
        }
    }
    v.extend(layout());
    v
}

/// The desktop's layout (theme.layout.*), its favourites and the assistant's name (WY-5). bitscomp applies them.
pub fn layout() -> Vec<KeyDef> {
    let comp = || Applier::Service("bitscomp".into());
    let t = |name: &str, ty: Type, default: Value, de: &str, en: &str| {
        def(name, ty, default, de, en, comp(), Restart::None)
    };
    vec![
        t(
            "theme.layout.address_bar",
            choice(&["top", "tile"]),
            text("top"),
            "Adresszeile: eine oben oder eine pro Kachel",
            "Address bar: one at the top or one per tile",
        ),
        t(
            "theme.layout.favourites_bar",
            Type::Bool,
            Value::Bool(true),
            "Favoritenleiste unter der Adresszeile zeigen",
            "Show the favourites bar under the address bar",
        ),
        t(
            "theme.layout.workspaces_bar",
            Type::Bool,
            Value::Bool(true),
            "Liste der Arbeitsräume links zeigen (aus: eingeklappt)",
            "Show the list of workspaces on the left (off: folded away)",
        ),
        t(
            "theme.layout.claude_sidebar",
            Type::Bool,
            Value::Bool(true),
            "Claudes Seitenleiste rechts zeigen",
            "Show Claude's sidebar on the right",
        ),
        t("theme.layout.statusbar", Type::Bool, Value::Bool(true), "Statusleiste unten zeigen", "Show the status bar at the bottom"),
        t(
            "desktop.favourites",
            Type::Text { max: 4096 },
            text("programm/terminal, einstellungen/aussehen"),
            "Favoriten: Adressen, durch Kommas getrennt",
            "Favourites: addresses separated by commas",
        ),
        t(
            "claude.name",
            Type::Text { max: 64 },
            text("Claude"),
            "Name des Assistenten",
            "The assistant's name",
        ),
    ]
}

/// Kernel keys: the CONFIG.TXT names the kernel reads at boot, and the `/sys/bitsos/` file that applies them at
/// once (change request CL-1/3; until the kernel has the file, a change takes effect at the next start).
pub fn kernel() -> Vec<KeyDef> {
    let k = |config: &str, sys: Option<&str>| Applier::Kernel { config: config.into(), sys: sys.map(Into::into) };
    vec![
        def(
            "kernel.loglevel",
            Type::LogSpec,
            text("info"),
            "Was der Kern protokolliert (z. B. info oder warn,net=debug)",
            "What the kernel logs (e.g. info or warn,net=debug)",
            k("loglevel", Some("loglevel")),
            Restart::None,
        ),
        def(
            "kernel.console_log",
            choice(&["off", "error", "warn", "info", "debug", "all"]),
            text("warn"),
            "Welche Kernmeldungen die Konsole zeigt",
            "Which kernel messages the console shows",
            k("console_log", Some("console_log")),
            Restart::None,
        ),
        def(
            "kernel.console_font",
            choice(&["noto", "vga"]),
            text("noto"),
            "Schrift der Textkonsole",
            "Font of the text console",
            k("console_font", None),
            Restart::Reboot,
        ),
        // SN-10: hdmi-off is an action (MCP snd, the console), not a stored value, so it is not a choice here.
        def(
            "kernel.snd_output",
            choice(&["auto", "jack", "hdmi", "usb"]),
            text("auto"),
            "Wo der Ton spielt (auto folgt den Steckern; hdmi schaltet HDMI-Ton ein)",
            "Where sound plays (auto follows the plugs; hdmi turns HDMI sound on)",
            k("snd_output", Some("snd_output")),
            Restart::None,
        ),
        // FS-13: the start-up preload remembers which file pages programs read when they start and reads them
        // in the background after the next boot (record on /data; clearing it: /sys/bitsos/preload_clear).
        def(
            "kernel.preload.enabled",
            Type::Bool,
            Value::Bool(true),
            "Programme nach dem Hochfahren vorladen (merkt sich, was sie beim Start lesen)",
            "Preload programs after boot (remembers what they read when they start)",
            k("preload", Some("preload")),
            Restart::None,
        ),
        // FS-13b: `all` (the default) reads as much as memory is free; reclaim gives unused preloaded pages back.
        def(
            "kernel.preload.max_mb",
            Type::Int { min: 0, max: 1_048_576, unit: "MB".into(), word: Some(PRELOAD_ALL.into()) },
            text(PRELOAD_ALL),
            "So viel liest das Vorladen nach jedem Hochfahren höchstens (alles = so viel freier Speicher da ist; 0 = angehalten)",
            "The most the preload reads after each boot (all = as much as memory is free; 0 = paused)",
            k("preload_max_mb", Some("preload_max_mb")),
            Restart::None,
        ),
    ]
}

/// settingsd's own keys.
pub fn own() -> Vec<KeyDef> {
    vec![
        def(
            "settings.history_keep",
            int(10, 1_000_000, ""),
            Value::Int(1000),
            "Wie viele Änderungen das Journal aufhebt",
            "How many changes the journal keeps",
            Applier::Settingsd,
            Restart::None,
        ),
        def(
            "settings.language",
            choice(&["de", "en"]),
            text("de"),
            "Sprache der Beschreibungen",
            "Language of the descriptions",
            Applier::Settingsd,
            Restart::None,
        ),
    ]
}

pub fn all() -> Vec<KeyDef> {
    let mut v = theme();
    v.extend(kernel());
    v.extend(own());
    v
}

/// Namespaces only settingsd defines keys in. `theme` is open: bitscomp may add tokens of its own (WY-5).
/// kernel.preload.max_mb's word for "no cap" (FS-13b; the kernel reads it in preload_max_mb=).
pub const PRELOAD_ALL: &str = "all";

pub const RESERVED: &[&str] = &["kernel", "settings"];
/// Namespaces several owners share (each key still has one owner). `desktop` and `claude` are open too: settingsd
/// brings their first keys (WY-5), CL-3 and later desktop items add their own.
pub const SHARED: &[&str] = &["theme", "desktop", "claude"];
