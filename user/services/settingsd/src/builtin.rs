//! The key groups settingsd brings itself. The names are an interface: WY-5 (bitscomp) reads `theme.*`, the
//! kernel reads the CONFIG.TXT names of `kernel.*`.
//!
//! Theme (plan/spec/desktop.md §2/§4): dark first, neutral colours with one accent (default holographic cyan),
//! Exo 2 plus a monospace, scale 125 %, motion medium with a calm setting; the effects are always the full GPU ones
//! (DECISIONS 18.11.), so they have no key.

use crate::key::{Applier, KeyDef, Restart, Safety};
use crate::value::{LEVELS_DE, Type, Value};

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

/// (value, German name) pairs: every built-in choice names its values for the settings page (CL-8, WY-9/1).
fn choice(c: &[(&str, &str)]) -> Type {
    Type::Choice {
        values: c.iter().map(|p| p.0.to_string()).collect(),
        de: c.iter().map(|p| p.1.to_string()).collect(),
    }
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
        t(
            "theme.mode",
            choice(&[("dark", "Dunkel"), ("light", "Hell"), ("auto", "Nach Uhrzeit")]),
            text("dark"),
            "Helles oder dunkles Design",
            "Light or dark look",
        ),
        // WY-SET: the hours of theme.mode `auto` (Georg 14.11.: light from 07:00 to 19:00).
        t("theme.auto.light_from", int(0, 23, "Uhr"), Value::Int(7), "Nach Uhrzeit: hell ab dieser Stunde", "By time: light from this hour"),
        t("theme.auto.dark_from", int(0, 23, "Uhr"), Value::Int(19), "Nach Uhrzeit: dunkel ab dieser Stunde", "By time: dark from this hour"),
        t(
            "theme.accent",
            Type::Color,
            text("#7fcfa0"),
            "Akzentfarbe für hell und dunkel (Standard Moos: folgt dem Modus)",
            "Accent colour for light and dark (default moss: follows the mode)",
        ),
        t(
            "theme.font.display",
            Type::Text { max: 64 },
            text("Bricolage Grotesque"),
            "Schrift für Überschriften (auch: Figtree, JetBrains Mono, Exo 2)",
            "Font for headlines (also: Figtree, JetBrains Mono, Exo 2)",
        ),
        t(
            "theme.font.ui",
            Type::Text { max: 64 },
            text("Figtree"),
            "Schrift der Oberfläche (auch: Exo 2, Bricolage Grotesque, JetBrains Mono)",
            "Interface font (also: Exo 2, Bricolage Grotesque, JetBrains Mono)",
        ),
        t(
            "theme.font.mono",
            Type::Text { max: 64 },
            text("JetBrains Mono"),
            "Schrift für Beschriftungen und Adressen",
            "Font for labels and addresses",
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
            "theme.motion",
            choice(&[("medium", "Mittel"), ("calm", "Ruhig"), ("off", "Aus")]),
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
        t(
            "theme.scanlines",
            Type::Bool,
            Value::Bool(true),
            "Feine Rasterlinien im dunklen Hintergrund (nie über Fenstern)",
            "Fine scanlines in the dark background (never over windows)",
        ),
    ];
    // The palettes of design v2 (Georg 28.10.2026, plan/spec/desktop.md §0): "Absicht dunkel" and "Absicht" (hell).
    // bitscomp's built-in defaults are the same (its test reads this file). theme.{mode}.accent is the mode's own
    // accent; theme.accent above, when set to another colour, replaces it in both modes.
    let palette: [(&str, &str, &str, &str, &str); 19] = [
        ("bg", "#121a16", "#e3e7e3", "Hintergrund", "Background"),
        ("surface", "#1b2520", "#fbfcfa", "Leisten und Kacheln", "Bars and tiles"),
        ("surface2", "#0c120f", "#d5dbd5", "Vertiefte Fläche (Felder)", "Sunken surface (fields)"),
        ("paper", "#1b2520", "#fbfcfa", "Gezeigter Reiter", "Shown tab"),
        ("text", "#e8eee9", "#15201a", "Text", "Text"),
        ("text_dim", "#a3b0a8", "#4e5b52", "Nebentext", "Secondary text"),
        ("border", "#2b3731", "#c3cbc3", "Rahmenlinien", "Border lines"),
        ("line_strong", "#6b7a71", "#6f7b71", "Kräftige Linien", "Strong lines"),
        ("accent", "#7fcfa0", "#2f6b4f", "Eigene Akzentfarbe des Modus", "The mode's own accent"),
        ("accent_soft", "#1f3a2c", "#cfe3d6", "Akzent, sanft (hinter Ausgewähltem)", "Soft accent (behind selections)"),
        ("on_accent", "#0f1712", "#fbfcfa", "Text auf dem Akzent", "Text on the accent"),
        ("claude", "#7fcfa0", "#2f6b4f", "Farbe des Assistenten", "The assistant's colour"),
        ("warnung", "#ff7b6b", "#b3261e", "Warnungen und Fehler", "Warnings and errors"),
        ("terminal", "#0a0f0c", "#15201a", "Terminal-Hintergrund", "Terminal background"),
        ("on_terminal", "#d5e2da", "#e8eee9", "Terminal-Text", "Terminal text"),
        ("kontext_1", "#d9765b", "#e2694b", "Kontextfarbe 1", "Context colour 1"),
        ("kontext_2", "#e3bd5f", "#f2c14e", "Kontextfarbe 2 (auch: wartet)", "Context colour 2 (also: waiting)"),
        ("kontext_3", "#86afd9", "#8db9e6", "Kontextfarbe 3", "Context colour 3"),
        ("on_kontext", "#121a16", "#15201a", "Text auf Kontextfarben", "Text on context colours"),
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
            "theme.layout.preset",
            choice(&[("buehne", "Bühne"), ("classic", "Klassisch")]),
            text("buehne"),
            "Aufbau des Bildschirms: Bühne (Kopfleiste, Bühne, Verlauf) oder klassisch (Leisten wie bisher)",
            "Screen layout: Bühne (header, stage, history strip) or classic (the bars as before)",
        ),
        t(
            "theme.layout.address_bar",
            choice(&[("top", "Oben"), ("tile", "Pro Kachel")]),
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
    ]
}

/// Kernel keys: the CONFIG.TXT names the kernel reads at boot, and the `/sys/bitsos/` file that applies them at
/// once (change request CL-1/3; until the kernel has the file, a change takes effect at the next start).
pub fn kernel() -> Vec<KeyDef> {
    let k = |config: &str, sys: Option<&str>| Applier::Kernel { config: config.into(), sys: sys.map(Into::into) };
    vec![
        def(
            "kernel.loglevel",
            // CL-8: shown as a choice of the five levels; warn,net=debug still works from the CLI and MCP.
            Type::LogSpec,
            text("info"),
            "Wie viel der Kern protokolliert",
            "How much the kernel logs",
            k("loglevel", Some("loglevel")),
            Restart::None,
        ),
        def(
            "kernel.console_log",
            // The console's own filter (crates/console/src/logfilter.rs): off and all, no trace.
            choice(&[
                ("off", "Aus"),
                ("error", LEVELS_DE[0]),
                ("warn", LEVELS_DE[1]),
                ("info", LEVELS_DE[2]),
                ("debug", LEVELS_DE[3]),
                ("all", LEVELS_DE[4]),
            ]),
            text("warn"),
            "Welche Kernmeldungen die Konsole zeigt",
            "Which kernel messages the console shows",
            k("console_log", Some("console_log")),
            Restart::None,
        ),
        def(
            "kernel.console_font",
            choice(&[("noto", "Noto"), ("vga", "VGA")]),
            text("noto"),
            "Schrift der Textkonsole",
            "Font of the text console",
            k("console_font", None),
            Restart::Reboot,
        ),
        // SN-10: hdmi-off is an action (MCP snd, the console), not a stored value, so it is not a choice here.
        def(
            "kernel.snd_output",
            choice(&[("auto", "Automatisch"), ("jack", "Klinke"), ("hdmi", "HDMI"), ("usb", "USB")]),
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
            choice(&[("de", "Deutsch"), ("en", "Englisch")]),
            text("de"),
            "Sprache der Beschreibungen",
            "Language of the descriptions",
            Applier::Settingsd,
            Restart::None,
        ),
    ]
}

/// The AI keys every part reads (CL-3a, CL-5s; DECISIONS 21.10.: no AI is built in by name, so every provider
/// default is empty). Nothing applies them: `assist` reads them for each request. The provider keys
/// `ai.provider.<id>.*` are not in this list: their names depend on the list `ai.providers`, `providers.rs`
/// makes them. EINS (plan/spec/assistent-eins K2): one level for everything the assistant does, `ai.level`,
/// `auto` unless Georg chooses less (DECISIONS 18.11.: everything built is on); loosening it again is his own yes
/// (`leash.rs`).
pub fn ai() -> Vec<KeyDef> {
    let a = |name: &str, ty: Type, default: Value, de: &str, en: &str| {
        def(name, ty, default, de, en, Applier::Settingsd, Restart::None)
    };
    let mut v = vec![a(
        crate::leash::LEVEL,
        choice(&[("answer", "Nur antworten"), ("ask", "Fragt vorher"), ("auto", "Automodus")]),
        text("auto"),
        "Wie viel der Assistent darf: nur antworten, vor jedem Eingriff fragen, oder im Automodus alles ohne Nachfrage (die festen Regeln gelten immer)",
        "How much the assistant may do: only answer, ask before each change, or do everything unasked in automode (the fixed rules always hold)",
    )];
    v.extend(ai_fixed(&a));
    v
}

/// The bytes a list of provider names may take; a role holds one of those names, so it gets the same room.
/// Not a count of providers: a text key names its size so one key cannot fill the stick (`Type::Text`).
const NAMES_MAX: usize = 65536;

fn ai_fixed(a: &dyn Fn(&str, Type, Value, &str, &str) -> KeyDef) -> Vec<KeyDef> {
    vec![
        a(
            "ai.providers",
            Type::Text { max: NAMES_MAX },
            text(""),
            "Namen der eingerichteten KI-Anbieter, durch Kommas getrennt. Ein neuer Name legt einen Anbieter an; ein gestrichener Name entfernt ihn mit seinen Angaben und seinem Schlüssel",
            "Names of the AI providers that are set up, separated by commas. A new name adds a provider; a name taken off removes it with its settings and its key",
        ),
        a(
            "ai.role.chat",
            Type::Text { max: NAMES_MAX },
            text(""),
            "Welcher Anbieter im Gespräch antwortet (leer: keiner)",
            "Which provider answers in the chat (empty: none)",
        ),
        a(
            "ai.role.chat_fallback",
            Type::Text { max: NAMES_MAX },
            text(""),
            "Ersatz für das Gespräch, wenn der erste Anbieter nicht erreichbar ist: Namen der Reihe nach, durch Kommas getrennt (leer: keiner)",
            "Fallback for the chat when the first provider is away: names in order, separated by commas (empty: none)",
        ),
        a(
            "ai.assistant.name",
            Type::Text { max: 64 },
            text("Claude"),
            "Name des Assistenten",
            "The assistant's name",
        ),
        a(
            "ai.cloud_notice",
            Type::Bool,
            Value::Bool(true),
            "Am Ring zeigen, wenn etwas aus deinen Dateien an einen Anbieter in der Cloud geht („geht an …“)",
            "Show at the ring when something from your files goes to a cloud provider (\"goes to …\")",
        ),
        a(
            "ai.local_only",
            Type::Bool,
            Value::Bool(false),
            "Nur lokal: kein Anbieter in der Cloud wird angesprochen",
            "Local only: no cloud provider is contacted",
        ),
        a(
            "ai.timeout_s",
            int(1, 86_400, "s"),
            Value::Int(600),
            "Wie lange eine Antwort des Anbieters höchstens dauern darf",
            "The longest a provider's answer may take",
        ),
        a(
            "ai.retries",
            int(0, 20, ""),
            Value::Int(2),
            "Wie oft eine Anfrage wiederholt wird, wenn der Anbieter überlastet ist oder um eine Pause bittet",
            "How often a request is tried again when the provider is overloaded or asks for a pause",
        ),
        a(
            "ai.tool_rounds",
            int(1, 1000, ""),
            Value::Int(20),
            "Wie viele Werkzeug-Runden der Assistent je Frage höchstens macht",
            "The most rounds of tool calls the assistant makes per question",
        ),
        a(
            "ai.max_tokens",
            int(256, 1_000_000, "Tokens"),
            Value::Int(64_000),
            "Längste Antwort (nur für Anbieter, die eine Grenze verlangen)",
            "Longest answer (only for providers that require a limit)",
        ),
        // `assist --mcp` hands BitsOS's tools to another program; on (Georg 18.11.: everything built is on), that
        // program asks him itself. Off: only what the chat runs unasked.
        a(
            "ai.mcp.ask_tools",
            Type::Bool,
            Value::Bool(true),
            "Dürfen andere Programme über „assist --mcp“ auch Werkzeuge benutzen, die etwas ändern (das Programm fragt dich dann selbst, BitsOS fragt nicht mehr)",
            "May other programs, through \"assist --mcp\", also use tools that change something (that program then asks you itself, BitsOS no longer does)",
        ),
    ]
}

pub fn all() -> Vec<KeyDef> {
    let mut v = theme();
    v.extend(kernel());
    v.extend(own());
    v.extend(ai());
    v.extend(machine());
    v
}

/// GP-CPU2: keys only some machines have. `kernel.cpu_policy` exists only on BitsOS (`/proc/bitsos`) with an Intel CPU
/// that has HWP with an energy preference (`hwp_epp` in `/proc/cpuinfo`): the Gaming-PC. The Mini-PC's AMD CPU and
/// every QEMU guest lack the flag, so their key lists and settings pages stay as they were; host tests run on Linux,
/// where `/proc/bitsos` does not exist.
pub fn machine() -> Vec<KeyDef> {
    let on_bitsos = std::path::Path::new("/proc/bitsos").is_dir();
    if on_bitsos && cpu_policy_applies(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default()) { vec![cpu_policy()] } else { Vec::new() }
}

/// Whether `/proc/cpuinfo` (its first `flags` line) names `hwp_epp` on a GenuineIntel CPU.
pub fn cpu_policy_applies(cpuinfo: &str) -> bool {
    let field = |name: &str| cpuinfo.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == name).map(|(_, v)| v.trim()));
    field("vendor_id") == Some("GenuineIntel") && field("flags").is_some_and(|f| f.split_whitespace().any(|x| x == "hwp_epp"))
}

/// CONFIG `cpu_policy=` (GP-CPU2, read by `hal::cpu::hwp::parse_policy` at boot): Linux's two energy preferences,
/// shown on the page "Leistung" next to the night measurements so the self-optimisation can compare them.
pub fn cpu_policy() -> KeyDef {
    def(
        "kernel.cpu_policy",
        choice(&[("balanced", "Ausgewogen (wie Linux)"), ("performance", "Höchstleistung (mehr Strom)")]),
        text("balanced"),
        "Wie der Prozessor zwischen Tempo und Stromverbrauch abwägt",
        "How the processor weighs speed against power",
        Applier::Kernel { config: "cpu_policy".into(), sys: None },
        Restart::Reboot,
    )
}

/// Namespaces only settingsd defines keys in. `theme` is open: bitscomp may add tokens of its own (WY-5).
/// kernel.preload.max_mb's word for "no cap" (FS-13b; the kernel reads it in preload_max_mb=).
pub const PRELOAD_ALL: &str = "all";

pub const RESERVED: &[&str] = &["kernel", "settings"];
/// Namespaces several owners share (each key still has one owner). `desktop` and `claude` are open too: settingsd
/// brought their first keys (WY-5; `claude.name` became `ai.assistant.name` with CL-5s). `ai`: settingsd brings
/// the fixed keys and makes the provider keys, `assist` registers the levels of the tools it finds.
pub const SHARED: &[&str] = &["theme", "desktop", "claude", "ai"];
/// Keys of a shared namespace that are one system program's alone (CL-S): prefix -> owner, which must be the
/// program of that name in settingsd's own directory. Only below such a prefix does a shared namespace hold
/// secrets, and nobody else registers a key there, so a provider key stored earlier is never adopted under
/// another owner. "Any program from that directory" would not do: `nc` or the `settings` command live there
/// too and say whatever their caller types.
/// `ai.tools` (CL-T, retired with EINS): once the per-tool levels; nobody registers there any more, and the entry
/// keeps it so (a key of that name would look like a level that counts).
/// `ai.provider` (CL-5s): settingsd makes these keys itself from `ai.providers` (`providers.rs`), for this
/// program; the entry still says who alone may register there (an older `assist` did).
/// `ai.assistant` and `ai.cloud_notice` (CL-3s): the assistant reads its name (it goes into the system prompt
/// of every question) and whether Georg is told before a question goes to a cloud provider from these.
/// settingsd brings both keys since CL-5s; nobody else adds a key beside the name.
pub const HOLDERS: &[(&str, &str)] = &[("ai.provider", "assist"), ("ai.tools", "assist"), ("ai.assistant", "assist"), ("ai.cloud_notice", "assist")];
