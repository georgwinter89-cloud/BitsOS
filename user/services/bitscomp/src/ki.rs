//! CL-5: what the settings page "KI" (`einstellungen/ki`) shows, as rows of the settings page (`page`): the
//! assistant's name and the two switches, who does which job, a section per provider and the form that adds one.
//! Pure: built from what settingsd said (`settings::Values`, with "is a key stored"), the form's own fields and
//! the tests' results. A provider's key is never part of it: the row knows only "gespeichert" or "fehlt".
//! The rules of names and lists are settingsd's (user/services/settingsd/src/providers.rs); settingsd checks again.

use std::collections::BTreeMap;

use crate::page::{Kind, Model, Row, Section, pill_w};
use crate::page_draw::Tone;
use crate::settings::Values;

pub const LIST: &str = "ai.providers";
pub const NAME: &str = "ai.assistant.name";
pub const LOCAL_ONLY: &str = "ai.local_only";
pub const NOTICE: &str = "ai.cloud_notice";
pub const UNSET: &str = "unset";
pub const PROGRAM: &str = "program";
/// The form's rows: no keys of settingsd, only names for `ctl hot` and the edit.
pub const ADD_NAME: &str = "ki.add.name";
pub const ADD_KIND: &str = "ki.add.kind";
pub const ADD_PATH: &str = "ki.add.path";
pub const ADD_GO: &str = "ki.add";

pub const TITLE: &str = "KI";
pub const HINT: &str = "Änderungen lassen sich rückgängig machen; ein Schlüssel und ein entfernter Anbieter nicht.";
pub const STORED: &str = "gespeichert";
pub const MISSING: &str = "fehlt";
const KIND_SUB: &str = "Wie der Anbieter erreicht wird; was jede Art ist, steht daneben.";

/// The kinds a provider can have when settingsd's own list is not known (its providers.rs).
const KINDS: [&str; 6] = [UNSET, "anthropic", "openai", "gemini", "openai_compatible", PROGRAM];
const PLACES: [&str; 3] = [UNSET, "cloud", "local"];

/// Each job: its key's last word, its name, what it is, and what "Keiner" means for it. PLAN K8: one assistant,
/// one role (`ai.role.search` and `ai.role.jobs` went with EINS-A).
const JOBS: [(&str, &str, &str); 1] = [("chat", "Gespräch mit dem Assistenten", "Wer antwortet, wenn du mit dem Assistenten sprichst.")];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Provider {
    pub id: String,
    pub kind: String,
    pub place: String,
    pub address: String,
    pub model: String,
    /// A key is stored for it.
    pub stored: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub providers: Vec<Provider>,
    /// `ai.role.*` keys and their values.
    pub roles: BTreeMap<String, String>,
    pub name: String,
    pub local_only: bool,
    pub cloud_notice: bool,
}

/// The form "Anbieter hinzufügen".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Form {
    pub name: String,
    pub kind: String,
    pub path: String,
}

/// A provider's test ("Testen"): `assist --test NAME`, its one line and its exit code.
#[derive(Clone, Debug, PartialEq)]
pub enum Test {
    Running,
    /// 0: reached, 1: asked in vain, 2: not asked.
    Done { code: Option<i32>, line: String },
    /// No answer within the page's wait: the test program was ended.
    Late,
    /// The test could not run; the German reason.
    Failed(String),
}

pub fn key_of(id: &str, field: &str) -> String {
    format!("ai.provider.{id}.{field}")
}

/// (provider, field) of a key `ai.provider.<id>.<field>`.
pub fn provider_of(key: &str) -> Option<(&str, &str)> {
    let (id, field) = key.strip_prefix("ai.provider.")?.split_once('.')?;
    (!id.is_empty() && !field.is_empty() && !field.contains('.')).then_some((id, field))
}

pub fn role_key(job: &str, fallback: bool) -> String {
    format!("ai.role.{job}{}", if fallback { "_fallback" } else { "" })
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != UNSET && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The names a list holds, in its order, each once.
pub fn ids(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in list.split([',', ' ', ';']).map(str::trim).filter(|s| valid_id(s)) {
        if !out.iter().any(|i| i == id) {
            out.push(id.to_string());
        }
    }
    out
}

pub fn list_with(list: &str, id: &str) -> String {
    let mut v = ids(list);
    if !v.iter().any(|i| i == id) {
        v.push(id.to_string());
    }
    v.join(", ")
}

pub fn list_without(list: &str, id: &str) -> String {
    ids(list).into_iter().filter(|i| i != id).collect::<Vec<_>>().join(", ")
}

/// A click on a name of a fallback list: on the list it goes off, else it goes to its end.
pub fn toggled(list: &str, id: &str) -> String {
    if ids(list).iter().any(|i| i == id) { list_without(list, id) } else { list_with(list, id) }
}

pub fn facts(v: &Values) -> Facts {
    let text = |k: &str| v.text(k).unwrap_or("").trim().to_string();
    let choice = |k: &str| Some(text(k)).filter(|s| !s.is_empty()).unwrap_or_else(|| UNSET.into());
    let providers = ids(&text(LIST))
        .into_iter()
        .map(|id| Provider {
            kind: choice(&key_of(&id, "kind")),
            place: choice(&key_of(&id, "place")),
            address: text(&key_of(&id, "address")),
            model: text(&key_of(&id, "model")),
            stored: v.stored(&key_of(&id, "key")),
            id,
        })
        .collect();
    let mut roles = BTreeMap::new();
    for (job, _, _) in JOBS {
        for k in [role_key(job, false), role_key(job, true)] {
            roles.insert(k.clone(), text(&k));
        }
    }
    Facts {
        providers,
        roles,
        name: v.text(NAME).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Claude").to_string(),
        local_only: v.flag(LOCAL_ONLY).unwrap_or(false),
        cloud_notice: v.flag(NOTICE).unwrap_or(true),
    }
}

impl Facts {
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// The jobs that name `id` (German, for the removal's dialog).
    pub fn jobs_of(&self, id: &str) -> Vec<&'static str> {
        JOBS.iter()
            .filter(|(job, _, _)| [role_key(job, false), role_key(job, true)].iter().any(|k| self.roles.get(k).is_some_and(|v| ids(v).iter().any(|i| i == id))))
            .map(|(_, name, _)| *name)
            .collect()
    }
}

pub fn kind_name(v: &str) -> String {
    match v {
        UNSET => "Nicht gesetzt",
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "gemini" => "Gemini",
        "openai_compatible" => "OpenAI-kompatibel",
        PROGRAM => "Hilfsprogramm",
        "cloud" => "Cloud",
        "local" => "Lokal",
        other => other,
    }
    .to_string()
}

/// Why the form's name cannot be added, in German; Ok: it can.
pub fn check_name(name: &str, f: &Facts) -> Result<(), String> {
    if name.is_empty() {
        return Err("Erst einen Namen für den Anbieter eintippen.".into());
    }
    if !valid_id(name) {
        return Err(format!("Der Name „{name}“ geht nicht: nur Kleinbuchstaben, Ziffern, - und _ (zum Beispiel claude oder strata)."));
    }
    if f.provider(name).is_some() {
        return Err(format!("Einen Anbieter „{name}“ gibt es schon."));
    }
    Ok(())
}

/// The lines of the dialog before provider `id` is removed.
pub fn remove_lines(f: &Facts, id: &str) -> Vec<String> {
    let mut lines = vec!["Seine Adresse, sein Modell und alle seine Angaben werden gelöscht.".to_string()];
    lines.push(if f.provider(id).is_some_and(|p| p.stored) {
        "Der gespeicherte Schlüssel wird mit gelöscht.".into()
    } else {
        "Für ihn ist kein Schlüssel gespeichert.".into()
    });
    let jobs = f.jobs_of(id);
    if !jobs.is_empty() {
        lines.push(format!("Er wird auch hier ausgetragen: {}.", jobs.join(", ")));
    }
    lines.push("Das lässt sich nicht rückgängig machen.".into());
    lines
}

pub fn test_line(t: Option<&Test>) -> (String, Tone) {
    match t {
        None => ("Noch nicht getestet. „Testen“ stellt dem Anbieter eine einzige kurze Frage.".into(), Tone::Plain),
        Some(Test::Running) => ("Test läuft … (höchstens eine Minute)".into(), Tone::Plain),
        Some(Test::Done { code, line }) if line.is_empty() => {
            (format!("Der Test hat nichts gesagt ({}).", code.map_or("abgebrochen".to_string(), |c| format!("Ende {c}"))), Tone::Bad)
        }
        Some(Test::Done { code, line }) => (
            line.clone(),
            match code {
                Some(0) => Tone::Good,
                Some(2) => Tone::Plain,
                _ => Tone::Bad,
            },
        ),
        Some(Test::Late) => ("Keine Antwort nach 65 Sekunden: der Test wurde beendet.".into(), Tone::Bad),
        Some(Test::Failed(why)) => (why.clone(), Tone::Bad),
    }
}

/// The page's rows, the text each shows and the sentence under each label.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Built {
    pub model: Model,
    pub values: Vec<String>,
    pub subs: Vec<Option<(String, Tone)>>,
}

impl Built {
    pub(crate) fn row(&mut self, key: &str, kind: Kind, label: &str, short: &str, value: &str, sub: (String, Tone)) {
        self.model.rows.push(Row {
            key: key.into(),
            kind,
            default: String::new(),
            label: label.into(),
            short: short.into(),
            note: String::new(),
            confirm: false,
            stack: false,
        });
        self.values.push(value.into());
        self.subs.push(Some(sub));
    }

    pub(crate) fn section(&mut self, name: &str, from: usize) {
        self.model.sections.push(Section::Rows(name.into(), from..self.model.rows.len()));
    }

    pub fn row_of(&self, key: &str) -> Option<usize> {
        self.model.rows.iter().position(|r| r.key == key)
    }
}

pub(crate) fn plain(s: &str) -> (String, Tone) {
    (s.to_string(), Tone::Plain)
}

fn pills(values: Vec<(String, String)>, multi: bool) -> Kind {
    let w = pill_w(values.iter().map(|(_, n)| n.as_str()));
    Kind::Pills { opts: values, multi, w, notes: Vec::new() }
}

/// The kinds' pills, each with its line (`ki_kinds::note`).
fn kind_pills(values: Vec<String>) -> Kind {
    let notes = values.iter().map(|v| crate::ki_kinds::note(v).to_string()).collect();
    match pills(named(values), false) {
        Kind::Pills { opts, multi, w, .. } => Kind::Pills { opts, multi, w, notes },
        k => k,
    }
}

/// settingsd's choices of `key` (a newer settingsd may know more kinds), else `own`.
fn choices(base: &Model, key: &str, own: &[&str]) -> Vec<String> {
    match base.rows.iter().find(|r| r.key == key).map(|r| &r.kind) {
        Some(Kind::Choice(o)) if !o.is_empty() => o.iter().map(|(v, _)| v.clone()).collect(),
        _ => own.iter().map(|s| s.to_string()).collect(),
    }
}

fn max_of(base: &Model, key: &str, own: usize) -> usize {
    match base.rows.iter().find(|r| r.key == key).map(|r| &r.kind) {
        Some(Kind::Field { max, .. }) => *max,
        _ => own,
    }
}

fn named(values: Vec<String>) -> Vec<(String, String)> {
    values.into_iter().map(|v| (v.clone(), kind_name(&v))).collect()
}

fn roles(b: &mut Built, f: &Facts) {
    let from = b.model.rows.len();
    for (job, name, what) in JOBS {
        let (key, fall) = (role_key(job, false), role_key(job, true));
        let now = f.roles.get(&key).cloned().unwrap_or_default();
        let offered: Vec<&Provider> = f.providers.iter().collect();
        let mut opts = vec![(String::new(), "Keiner".to_string())];
        opts.extend(offered.iter().map(|p| (p.id.clone(), p.id.clone())));
        let known = now.is_empty() || offered.iter().any(|p| p.id == now);
        if !known {
            opts.push((now.clone(), now.clone()));
        }
        let sub = match f.provider(&now) {
            _ if now.is_empty() => plain(what),
            None => (format!("Einen Anbieter „{now}“ gibt es nicht: bitte neu wählen."), Tone::Bad),
            Some(p) if f.local_only && p.place != "local" => (format!("„{now}“ ist kein lokaler Anbieter: mit „Nur lokal“ wird er nicht gefragt."), Tone::Bad),
            Some(_) => plain(what),
        };
        b.row(&key, pills(opts, false), name, name, &now, sub);
        let list = f.roles.get(&fall).cloned().unwrap_or_default();
        let short = format!("Ersatz: {name}");
        if offered.is_empty() && ids(&list).is_empty() {
            let why = if f.providers.is_empty() { "Erst unten einen Anbieter hinzufügen." } else { "Erst einen Anbieter als lokal eintragen." };
            b.row(&fall, Kind::Fixed(why.into()), "Ersatz dafür", &short, "Keiner", plain("Wird gefragt, wenn der erste nicht antwortet."));
            continue;
        }
        let mut opts: Vec<(String, String)> = offered.iter().map(|p| (p.id.clone(), p.id.clone())).collect();
        for id in ids(&list) {
            if !opts.iter().any(|(v, _)| *v == id) {
                opts.push((id.clone(), id));
            }
        }
        let order = ids(&list);
        let sub = match order.len() {
            0 => "Keiner. Ein Klick trägt einen ein; er wird gefragt, wenn der erste nicht antwortet.".to_string(),
            1 => "Wird gefragt, wenn der erste nicht antwortet.".to_string(),
            _ => format!("Der Reihe nach gefragt, wenn der erste nicht antwortet: {}.", order.join(", dann ")),
        };
        b.row(&fall, pills(opts, true), "Ersatz dafür", &short, &order.join(","), (sub, Tone::Plain));
    }
    b.section("WER MACHT WAS", from);
}

fn provider(b: &mut Built, p: &Provider, test: Option<&Test>, base: &Model, helpers: bool) {
    let from = b.model.rows.len();
    let id = p.id.as_str();
    let short = |what: &str| format!("Anbieter „{id}“, {what}");
    let key = |f: &str| key_of(id, f);
    let program = p.kind == PROGRAM;
    b.row(&key("kind"), kind_pills(choices(base, &key("kind"), &KINDS)), "Art", &short("Art"), &p.kind, plain(KIND_SUB));
    b.row(
        &key("place"),
        pills(named(choices(base, &key("place"), &PLACES)), false),
        "Ort",
        &short("Ort"),
        &p.place,
        plain("Das legst du fest, es wird nie geraten. Lokal: im Heimnetz."),
    );
    let (label, sub) = if program {
        ("Pfad des Hilfsprogramms", "Der ganze Pfad des Programms auf diesem Rechner, ohne Zusätze dahinter.")
    } else {
        ("Adresse", "Die Adresse der Schnittstelle, zum Beispiel http://192.168.100.20:8080/v1 (http:// nur lokal im Heimnetz).")
    };
    let advice = crate::ki_kinds::address_advice(&p.kind, &p.address, helpers);
    let stack = advice.is_some();
    let sub = advice.map_or_else(|| plain(sub), |a| (a, Tone::Bad));
    b.row(&key("address"), Kind::Field { colour: false, max: max_of(base, &key("address"), 2048) }, label, &short(label), &p.address, sub);
    if let Some(r) = b.model.rows.last_mut() {
        r.stack = stack;
    }
    b.row(&key("model"), Kind::Field { colour: false, max: max_of(base, &key("model"), 512) }, "Modell", &short("Modell"), &p.model, plain("Der Name des Modells, wie der Anbieter es nennt."));
    if program && !p.stored {
        b.row(&key("key"), Kind::Fixed("Ein Hilfsprogramm braucht keinen.".into()), "Schlüssel", &short("Schlüssel"), "–", plain("Das Hilfsprogramm meldet sich selbst an; hier wird nichts gespeichert."));
    } else if p.stored {
        b.row(
            &key("key"),
            Kind::Secret { clear: true },
            "Schlüssel",
            &short("Schlüssel"),
            STORED,
            plain("Gilt für Adresse, Art und Ort, wie sie jetzt sind; zum Ändern erst den Schlüssel löschen."),
        );
    } else {
        b.row(
            &key("key"),
            Kind::Secret { clear: false },
            "Schlüssel",
            &short("Schlüssel"),
            MISSING,
            plain("Klick ins Feld: erst siehst du, wohin er geht, dann tippen und Enter. Er wird nie angezeigt."),
        );
    }
    let buttons = Kind::Buttons(vec![("test".into(), "Testen".into()), ("remove".into(), "Entfernen …".into())]);
    b.row(&key("test"), buttons, "Verbindung", &short("Test"), "", test_line(test));
    b.section(&format!("ANBIETER „{id}“"), from);
}

fn form(b: &mut Built, form: &Form, f: &Facts, base: &Model) {
    let from = b.model.rows.len();
    b.row(
        ADD_NAME,
        Kind::Field { colour: false, max: 64 },
        "Name",
        "Name",
        &form.name,
        plain("Frei gewählt: Kleinbuchstaben, Ziffern, - und _ (zum Beispiel claude oder strata)."),
    );
    // settingsd's kinds as an existing provider has them, without "not set".
    let known = f.providers.first().map(|p| choices(base, &key_of(&p.id, "kind"), &KINDS)).unwrap_or_else(|| KINDS.iter().map(|s| s.to_string()).collect());
    let kinds: Vec<String> = known.into_iter().filter(|k| k != UNSET).collect();
    b.row(ADD_KIND, kind_pills(kinds), "Art", "Art", &form.kind, plain(KIND_SUB));
    if form.kind == PROGRAM {
        b.row(
            ADD_PATH,
            Kind::Field { colour: false, max: 2048 },
            "Pfad des Hilfsprogramms",
            "Pfad",
            &form.path,
            plain("Der ganze Pfad des Programms auf diesem Rechner."),
        );
    }
    b.row(
        ADD_GO,
        Kind::Buttons(vec![("add".into(), "Hinzufügen".into())]),
        "Anbieter anlegen",
        "Hinzufügen",
        "",
        plain("Danach trägst du in seinem Abschnitt Ort, Adresse, Modell und Schlüssel ein."),
    );
    b.section("ANBIETER HINZUFÜGEN", from);
}

/// The page. `base`: the settings page's rows from settingsd's schema (the choices and limits it gave).
pub fn build(f: &Facts, fm: &Form, tests: &BTreeMap<String, Test>, base: &Model, helpers: &[crate::helpers::Helper]) -> Built {
    let mut b = Built::default();
    b.row(
        NAME,
        Kind::Field { colour: false, max: max_of(base, NAME, 64) },
        "Name des Assistenten",
        "Name des Assistenten",
        &f.name,
        plain("So heißt er in der Seitenleiste und am Ring."),
    );
    b.row(
        LOCAL_ONLY,
        Kind::Switch,
        "Nur lokal",
        "Nur lokal",
        &f.local_only.to_string(),
        plain("Ein: Kein Cloud-Anbieter wird mehr gefragt, nur noch Anbieter mit dem Ort „Lokal“."),
    );
    b.row(
        NOTICE,
        Kind::Switch,
        "Hinweis bei Cloud",
        "Hinweis bei Cloud",
        &f.cloud_notice.to_string(),
        plain("Ein: Der Ring zeigt „geht an <Anbieter>“, wenn Inhalte aus deinen Dateien an einen Cloud-Anbieter gehen."),
    );
    b.section("ASSISTENT", 0);
    roles(&mut b, f);
    crate::ki_helpers::section(&mut b, f, helpers);
    for p in &f.providers {
        provider(&mut b, p, tests.get(&p.id), base, !helpers.is_empty());
    }
    form(&mut b, fm, f, base);
    b
}
