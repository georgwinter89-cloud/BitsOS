//! WY-9: the settings page's rows come from settingsd's schema (`describe`, user/services/settingsd/src/key.rs), so
//! a new key needs no change here: a choice gets its pills, a number − and + with its range and unit, a bool a
//! switch, a colour or a text a field, grouped by the key's group. What this file adds by hand: the mock-up's order
//! of the first rows, the accent's swatches, the scale's step, the German names of choice values (the registry has
//! raw values only; change request WY-9/1 asks settingsd for them) and the German group names.

use mcpreg::Json;

use crate::page::{FAVOURITES, Kind, Model, Row, SCALE_STEP, Section};

/// A key's type as settingsd describes it (`schema` of `describe`).
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Bool,
    /// `word`: a word that may stand instead of a number (`all` for kernel.preload.max_mb, FS-13b).
    Int { min: i64, max: i64, unit: String, word: Option<String> },
    Choice(Vec<String>),
    Color,
    Text { max: usize },
    LogSpec,
    /// A type this bitscomp does not know (a newer settingsd): shown, not changed.
    Other(String),
}

/// One key of the registry.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub key: String,
    pub ty: Ty,
    pub default: Option<String>,
    /// The German description.
    pub de: String,
    /// German names of the choices from the registry, when it has them (same order as the choices).
    pub names: Option<Vec<String>>,
    /// none | service | reboot
    pub restart: String,
    /// normal | confirm | secret
    pub safety: String,
}

fn shown(j: &Json) -> Option<String> {
    match j {
        Json::Str(s) => Some(s.clone()),
        Json::Bool(b) => Some(b.to_string()),
        Json::Num(_) => j.as_i64().map(|n| n.to_string()),
        _ => None,
    }
}

fn strings(j: Option<&Json>) -> Option<Vec<String>> {
    let a = j?.as_array()?;
    let v: Vec<String> = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    (v.len() == a.len() && !v.is_empty() && v.iter().all(|s| !s.is_empty() && !s.chars().any(char::is_control))).then_some(v)
}

fn ty_of(s: &Json) -> Option<Ty> {
    let name = s.get("type")?.as_str()?;
    Some(match name {
        "bool" => Ty::Bool,
        "int" => {
            let (min, max) = (s.get("min")?.as_i64()?, s.get("max")?.as_i64()?);
            if min > max {
                return None;
            }
            let word = match s.get("word") {
                None => None,
                Some(w) => Some(w.as_str().filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphabetic()))?.to_string()),
            };
            Ty::Int { min, max, unit: s.get("unit").and_then(Json::as_str).unwrap_or("").to_string(), word }
        }
        "choice" => Ty::Choice(strings(s.get("choices"))?),
        "color" => Ty::Color,
        "text" => Ty::Text { max: s.get("max_bytes").and_then(Json::as_u64).map_or(usize::MAX, |n| n as usize) },
        "logspec" => Ty::LogSpec,
        other => Ty::Other(other.to_string()),
    })
}

/// A key from settingsd's `describe` answer (its `key` object) or from a `list` entry that carries a `schema`.
/// None: no key name, or a schema that makes no sense (the row then shows the value only).
pub fn spec_of(j: &Json) -> Option<Spec> {
    let key = j.get("key")?.as_str()?.to_string();
    if key.is_empty() || key.chars().any(char::is_control) {
        return None;
    }
    let schema = j.get("schema")?;
    let ty = ty_of(schema)?;
    let de = match j.get("text") {
        Some(Json::Str(s)) => s.clone(),
        Some(t) => t.get("de").and_then(Json::as_str).unwrap_or("").to_string(),
        None => String::new(),
    };
    let names = strings(schema.get("choices_de")).filter(|n| matches!(&ty, Ty::Choice(c) if c.len() == n.len()));
    Some(Spec {
        key,
        ty,
        default: j.get("default").and_then(shown),
        de,
        names,
        restart: j.get("restart").and_then(Json::as_str).unwrap_or("none").to_string(),
        safety: j.get("safety").and_then(Json::as_str).unwrap_or("normal").to_string(),
    })
}

/// German names of choice values the registry does not name itself: (key, value, name); key "" for any key.
const NAMES: &[(&str, &str, &str)] = &[
    ("theme.mode", "dark", "Dunkel"),
    ("theme.mode", "light", "Hell"),
    ("theme.motion", "medium", "Mittel"),
    ("theme.motion", "calm", "Ruhig"),
    ("theme.effects", "cheap", "Sparsam"),
    ("theme.effects", "gpu", "GPU"),
    ("theme.layout.address_bar", "top", "Oben"),
    ("theme.layout.address_bar", "tile", "Pro Kachel"),
    ("kernel.snd_output", "jack", "Klinke"),
    ("kernel.console_log", "error", "Fehler"),
    ("kernel.console_log", "warn", "Warnungen"),
    ("kernel.console_log", "all", "Alles"),
    ("settings.language", "de", "Deutsch"),
    ("settings.language", "en", "Englisch"),
    ("", "auto", "Automatisch"),
    ("", "off", "Aus"),
    ("", "on", "An"),
    ("", "none", "Keine"),
    ("", "info", "Info"),
    ("", "debug", "Debug"),
    ("", "hdmi", "HDMI"),
    ("", "usb", "USB"),
    ("", "vga", "VGA"),
    ("", "noto", "Noto"),
];

/// The German name of choice `value` of `key`; a value nobody named shows as it is.
pub fn choice_name(key: &str, value: &str) -> String {
    let find = |k: &str| NAMES.iter().find(|(nk, v, _)| *nk == k && *v == value).map(|(_, _, n)| n.to_string());
    find(key).or_else(|| find("")).unwrap_or_else(|| value.to_string())
}

/// German group heads by key prefix (the longest one wins), in the page's order. FAVORITEN comes after AUFBAU.
const GROUPS: &[(&str, &str)] = &[
    ("theme", "AUSSEHEN"),
    ("theme.layout", "AUFBAU"),
    ("theme.font", "SCHRIFT"),
    ("theme.dark", "FARBEN IM DUNKLEN DESIGN"),
    ("theme.light", "FARBEN IM HELLEN DESIGN"),
    ("desktop", "SCHREIBTISCH"),
    ("claude", "CLAUDE"),
    ("ki", "KI"),
    ("net", "NETZ"),
    ("snd", "TON"),
    ("kernel", "KERN"),
    ("settings", "EINSTELLUNGSDIENST"),
];
/// Groups before the favourites (the mock-up's order).
const BEFORE_FAVOURITES: usize = 2;

/// The group of `key`: its index in GROUPS, or its first segment in capitals (a namespace nobody named yet).
fn group_of(key: &str) -> Result<usize, String> {
    let under = |p: &str| key == p || key.starts_with(&format!("{p}."));
    GROUPS
        .iter()
        .enumerate()
        .filter(|(_, (p, _))| under(p))
        .max_by_key(|(_, (p, _))| p.len())
        .map(|(i, _)| i)
        .ok_or_else(|| key.split('.').next().unwrap_or(key).to_uppercase())
}

/// The hand-made rows: their order first in their group, a short name for answer lines, a kind that adds something.
const HAND: [(&str, &str); 10] = [
    ("theme.mode", "Design"),
    ("theme.accent", "Akzentfarbe"),
    ("theme.scale", "Skalierung"),
    ("theme.motion", "Bewegung"),
    ("theme.effects", "Effektstufe"),
    ("theme.layout.address_bar", "Adresszeile"),
    ("theme.layout.favourites_bar", "Favoritenleiste"),
    ("theme.layout.workspaces_bar", "Liste der Arbeitsräume"),
    ("theme.layout.claude_sidebar", "Claudes Seitenleiste"),
    ("theme.layout.statusbar", "Statusleiste"),
];

/// The page's keys while settingsd has not answered (its defaults as in settingsd/src/builtin.rs).
pub fn builtin() -> Vec<Spec> {
    let s = |key: &str, ty: Ty, default: &str, de: &str| Spec {
        key: key.into(),
        ty,
        default: Some(default.into()),
        de: de.into(),
        names: None,
        restart: "none".into(),
        safety: "normal".into(),
    };
    let ch = |v: &[&str]| Ty::Choice(v.iter().map(|x| x.to_string()).collect());
    vec![
        s("theme.mode", ch(&["dark", "light"]), "dark", "Helles oder dunkles Design"),
        s("theme.accent", Ty::Color, "#00e5ff", "Akzentfarbe"),
        s("theme.scale", Ty::Int { min: 100, max: 300, unit: "%".into(), word: None }, "125", "Skalierung der Oberfläche"),
        s("theme.motion", ch(&["medium", "calm", "off"]), "medium", "Bewegung"),
        s("theme.effects", ch(&["cheap", "gpu"]), "cheap", "Effektstufe"),
        s("theme.layout.address_bar", ch(&["top", "tile"]), "top", "Adresszeile"),
        s("theme.layout.favourites_bar", Ty::Bool, "true", "Favoritenleiste"),
        s("theme.layout.workspaces_bar", Ty::Bool, "true", "Liste der Arbeitsräume"),
        s("theme.layout.claude_sidebar", Ty::Bool, "true", "Claudes Seitenleiste"),
        s("theme.layout.statusbar", Ty::Bool, "true", "Statusleiste"),
    ]
}

/// A key settingsd listed but did not describe (an older settingsd, a broken connection): bitscomp's own row
/// when it has one, else a row that only shows the value.
pub fn fallback(key: &str, text: &str) -> Spec {
    let mut s = builtin().into_iter().find(|s| s.key == key).unwrap_or_else(|| Spec {
        key: key.into(),
        ty: Ty::Other(String::new()),
        default: None,
        de: String::new(),
        names: None,
        restart: "none".into(),
        safety: "normal".into(),
    });
    if !text.trim().is_empty() {
        s.de = text.into();
    }
    s
}

/// The step of − and +: the hand-made one, else 1 for small ranges, else a round step (1, 2, 5 × 10^n) that
/// crosses the range in at most 20 steps.
pub fn step_of(key: &str, min: i64, max: i64) -> i64 {
    if key == "theme.scale" {
        return SCALE_STEP;
    }
    let range = max.saturating_sub(min);
    if range <= 64 {
        return 1;
    }
    let mut unit: i64 = 1;
    loop {
        for m in [1, 2, 5] {
            let s = unit.saturating_mul(m);
            if range / s <= 20 || s == i64::MAX {
                return s;
            }
        }
        unit = unit.saturating_mul(10);
    }
}

/// The German name of a number row's word (settingsd has the raw word only).
fn word_name(w: &str) -> String {
    match w {
        "all" => "alles".into(),
        other => other.into(),
    }
}

fn kind_of(s: &Spec) -> Kind {
    // A confirm key gets its control like any other; the page asks before it sends (WY-10, `page_confirm`).
    if s.safety == "secret" {
        return Kind::Fixed("Geheim: nur sein Dienst sieht den Wert.".into());
    }
    match &s.ty {
        _ if s.key == "theme.accent" => Kind::Accent,
        Ty::Bool => Kind::Switch,
        Ty::Int { min, max, unit, word } => Kind::Number {
            min: *min,
            max: *max,
            step: step_of(&s.key, *min, *max),
            unit: unit.clone(),
            word: word.as_ref().map(|w| (w.clone(), word_name(w))),
        },
        Ty::Choice(c) => Kind::Choice(
            c.iter()
                .enumerate()
                .map(|(i, v)| {
                    let name = s.names.as_ref().and_then(|n| n.get(i)).cloned().unwrap_or_else(|| choice_name(&s.key, v));
                    (v.clone(), name)
                })
                .collect(),
        ),
        Ty::Color => Kind::Field { colour: true, max: 7 },
        Ty::Text { max } => Kind::Field { colour: false, max: *max },
        Ty::LogSpec => Kind::Field { colour: false, max: usize::MAX },
        Ty::Other(t) if t.is_empty() => Kind::Fixed("Ohne Beschreibung vom Einstellungsdienst: nur angezeigt.".into()),
        Ty::Other(t) => Kind::Fixed(format!("Typ „{t}“: hier nicht änderbar.")),
    }
}

fn note_of(s: &Spec) -> &'static str {
    match s.restart.as_str() {
        "reboot" => "wirkt nach dem nächsten Start",
        "service" => "wirkt nach dem Neustart des Dienstes",
        _ => "",
    }
}

fn row_of(s: &Spec) -> Row {
    let short = HAND.iter().find(|(k, _)| *k == s.key).map(|(_, n)| n.to_string());
    let label = if s.de.trim().is_empty() { s.key.clone() } else { s.de.clone() };
    Row {
        key: s.key.clone(),
        kind: kind_of(s),
        default: s.default.clone().unwrap_or_default(),
        short: short.unwrap_or_else(|| label.clone()),
        label,
        note: note_of(s).into(),
        confirm: s.safety == "confirm",
    }
}

/// The page's rows and sections from the registry's keys: grouped (GROUPS' order, then new namespaces in the
/// order they come), the hand-made rows first in their group, the rest in settingsd's order. The favourites have
/// their own section; a key twice counts once.
pub fn model(specs: &[Spec]) -> Model {
    let mut seen = std::collections::HashSet::new();
    let mut named: Vec<Vec<&Spec>> = vec![Vec::new(); GROUPS.len()];
    let mut other: Vec<(String, Vec<&Spec>)> = Vec::new();
    let rank = |k: &str| HAND.iter().position(|(h, _)| *h == k).unwrap_or(HAND.len());
    let mut sorted: Vec<&Spec> = specs.iter().filter(|s| s.key != FAVOURITES && seen.insert(s.key.as_str())).collect();
    sorted.sort_by_key(|s| rank(&s.key));
    for s in sorted {
        match group_of(&s.key) {
            Ok(g) => named[g].push(s),
            Err(name) => match other.iter_mut().find(|(n, _)| *n == name) {
                Some((_, v)) => v.push(s),
                None => other.push((name, vec![s])),
            },
        }
    }
    let mut m = Model::default();
    let add = |m: &mut Model, name: &str, v: &[&Spec]| {
        if v.is_empty() {
            return;
        }
        let from = m.rows.len();
        m.rows.extend(v.iter().map(|s| row_of(s)));
        m.sections.push(Section::Rows(name.to_string(), from..m.rows.len()));
    };
    for (g, (_, name)) in GROUPS.iter().enumerate() {
        if g == BEFORE_FAVOURITES {
            m.sections.push(Section::Favourites);
        }
        add(&mut m, name, &named[g]);
    }
    for (name, v) in &other {
        add(&mut m, name, v);
    }
    m
}

/// A field being edited in row `r` of `old` follows its key into `new` (a re-list may reorder the rows): the
/// key's new row while it still takes typed text, else None (the edit ends, nothing is written).
pub fn field_row(old: &Model, new: &Model, r: usize) -> Option<usize> {
    let key = &old.rows.get(r)?.key;
    new.rows.iter().position(|n| n.key == *key && matches!(n.kind, Kind::Number { .. } | Kind::Field { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<Spec> {
        spec_of(&Json::parse(s).unwrap())
    }

    #[test]
    fn describe_answers_become_specs_and_nonsense_does_not() {
        let s = parse(r#"{"key":"kernel.snd_output","schema":{"type":"choice","choices":["auto","jack","hdmi","usb"]},"default":"auto","value":"auto","text":{"de":"Wo der Ton spielt","en":"x"},"restart":"none","safety":"normal"}"#).unwrap();
        assert_eq!(s.ty, Ty::Choice(vec!["auto".into(), "jack".into(), "hdmi".into(), "usb".into()]));
        assert_eq!((s.de.as_str(), s.default.as_deref()), ("Wo der Ton spielt", Some("auto")));
        let n = parse(r#"{"key":"theme.glow","schema":{"type":"int","min":0,"max":100,"unit":"%"},"default":60,"text":"Leuchten"}"#).unwrap();
        assert_eq!(n.ty, Ty::Int { min: 0, max: 100, unit: "%".into(), word: None });
        assert_eq!((n.de.as_str(), n.default.as_deref(), n.safety.as_str()), ("Leuchten", Some("60"), "normal"));
        let named = parse(r#"{"key":"a.b","schema":{"type":"choice","choices":["x","y"],"choices_de":["Iks","Ypsilon"]}}"#).unwrap();
        assert_eq!(named.names, Some(vec!["Iks".into(), "Ypsilon".into()]));
        let wrong = parse(r#"{"key":"a.b","schema":{"type":"choice","choices":["x","y"],"choices_de":["Iks"]}}"#).unwrap();
        assert_eq!(wrong.names, None, "names that do not match the choices are left out");
        assert_eq!(parse(r#"{"key":"a.b","schema":{"type":"blob"}}"#).unwrap().ty, Ty::Other("blob".into()));
        for bad in [
            r#"{"schema":{"type":"bool"}}"#,
            r#"{"key":"","schema":{"type":"bool"}}"#,
            r#"{"key":"a\nb","schema":{"type":"bool"}}"#,
            r#"{"key":"a.b"}"#,
            r#"{"key":"a.b","schema":7}"#,
            r#"{"key":"a.b","schema":{"type":"int","min":5,"max":1}}"#,
            r#"{"key":"a.b","schema":{"type":"int","min":"0","max":1}}"#,
            r#"{"key":"a.b","schema":{"type":"choice","choices":[]}}"#,
            r#"{"key":"a.b","schema":{"type":"choice","choices":["x",3]}}"#,
            r#"{"key":"a.b","schema":{"type":"choice","choices":[""]}}"#,
            r#"{"key":7,"schema":{"type":"bool"}}"#,
            r#"[1,2]"#,
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn steps_cross_a_range_in_few_clicks() {
        assert_eq!(step_of("theme.scale", 100, 300), 25);
        assert_eq!(step_of("theme.font.size", 8, 48), 1);
        assert_eq!(step_of("theme.glow", 0, 100), 5);
        assert_eq!(step_of("settings.history_keep", 10, 1_000_000), 50_000);
        assert!(step_of("x", i64::MIN, i64::MAX) > 0, "no overflow");
    }

    #[test]
    fn rows_are_grouped_with_the_hand_made_ones_first() {
        let mut specs = builtin();
        specs.reverse();
        specs.push(Spec { key: "kernel.snd_output".into(), ty: Ty::Choice(vec!["auto".into(), "hdmi".into()]), ..builtin()[0].clone() });
        specs.push(Spec { key: "zz.new".into(), ty: Ty::Bool, ..builtin()[0].clone() });
        specs.push(Spec { key: "theme.glow".into(), ty: Ty::Int { min: 0, max: 100, unit: "%".into(), word: None }, ..builtin()[0].clone() });
        specs.push(Spec { key: "net.secret".into(), safety: "secret".into(), ..builtin()[0].clone() });
        specs.push(Spec { key: "net.risky".into(), ty: Ty::Bool, safety: "confirm".into(), ..builtin()[0].clone() });
        specs.push(Spec { key: FAVOURITES.into(), ty: Ty::Text { max: 9 }, ..builtin()[0].clone() });
        specs.push(builtin()[0].clone());
        let m = model(&specs);
        let keys: Vec<&str> = m.rows.iter().map(|r| r.key.as_str()).collect();
        let hand: Vec<&str> = HAND.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys[..5], hand[..5], "AUSSEHEN in the mock-up's order");
        assert_eq!(keys[5], "theme.glow", "a new theme key joins AUSSEHEN after the hand-made rows");
        assert_eq!(keys[6..11], hand[5..]);
        assert_eq!(&keys[11..], ["net.secret", "net.risky", "kernel.snd_output", "zz.new"]);
        let heads: Vec<String> = m
            .sections
            .iter()
            .map(|s| match s {
                Section::Rows(n, r) => format!("{n}:{}", r.len()),
                Section::Favourites => "FAV".into(),
            })
            .collect();
        assert_eq!(heads, ["AUSSEHEN:6", "AUFBAU:5", "FAV", "NETZ:2", "KERN:1", "ZZ:1"]);
        assert!(matches!(&m.rows[0].kind, Kind::Choice(o) if o[0] == ("dark".into(), "Dunkel".into())));
        assert_eq!(m.rows[0].short, "Design");
        assert_eq!(m.rows[1].kind, Kind::Accent);
        assert!(matches!(m.rows[2].kind, Kind::Number { min: 100, max: 300, step: 25, .. }));
        assert!(matches!(m.rows[11].kind, Kind::Fixed(_)), "a secret gets no control");
        assert!(!m.rows[11].confirm && m.rows[12].confirm && !m.rows[0].confirm);
        assert_eq!(m.rows[12].kind, Kind::Switch, "a confirm key gets its control (the page asks first)");
        assert!(matches!(&m.rows[13].kind, Kind::Choice(o) if o[1] == ("hdmi".into(), "HDMI".into())));
        assert_eq!(model(&builtin()).rows.len(), 10, "offline: the hand-made rows");
    }

    #[test]
    fn an_open_field_follows_its_key_when_the_rows_move() {
        let spec = |k: &str, ty: &str| parse(&format!(r#"{{"key":"{k}","schema":{ty},"text":"x"}}"#)).unwrap();
        let int = r#"{"type":"int","min":0,"max":1000,"unit":""}"#;
        let old = model(&[spec("zz.a", int), spec("zz.b", r#"{"type":"text","max_bytes":600}"#)]);
        let r = old.rows.iter().position(|w| w.key == "zz.a").unwrap();
        // A re-list puts a new number key where zz.a was: Enter must not write zz.a's 500 into it.
        let new = model(&[spec("zz.0", int), spec("zz.a", int), spec("zz.b", r#"{"type":"text"}"#)]);
        assert_eq!(new.rows[r].key, "zz.0", "the old row index now names another key");
        let n = field_row(&old, &new, r).unwrap();
        assert_eq!(new.rows[n].key, "zz.a");
        let gone = model(&[spec("zz.0", int), spec("zz.b", r#"{"type":"text"}"#)]);
        assert_eq!(field_row(&old, &gone, r), None, "the key is gone: the edit ends");
        let untyped = model(&[spec("zz.a", r#"{"type":"bool"}"#)]);
        assert_eq!(field_row(&old, &untyped, r), None, "no longer typed: the edit ends");
        assert_eq!(field_row(&old, &new, 99), None);
    }

    /// FS-13b: an int with a word (kernel.preload.max_mb `all`) gets a pill named `alles` after +; a bad word is
    /// no schema.
    #[test]
    fn an_int_with_a_word_gets_its_pill() {
        let n = parse(r#"{"key":"kernel.preload.max_mb","schema":{"type":"int","min":0,"max":1048576,"unit":"MB","word":"all"},"default":"all","text":"x"}"#).unwrap();
        assert_eq!(n.ty, Ty::Int { min: 0, max: 1_048_576, unit: "MB".into(), word: Some("all".into()) });
        let m = model(&[n]);
        let r = m.rows.iter().find(|r| r.key == "kernel.preload.max_mb").unwrap();
        assert!(matches!(&r.kind, Kind::Number { word: Some((w, name)), .. } if w == "all" && name == "alles"), "{:?}", r.kind);
        assert!(crate::page::controls_width(r, 2000) > crate::page::NUM_W + 2 * crate::page::CTL_H + crate::page::OPT_W);
        assert!(parse(r#"{"key":"a.b","schema":{"type":"int","min":0,"max":1,"word":"a b"}}"#).is_none());
    }
}
