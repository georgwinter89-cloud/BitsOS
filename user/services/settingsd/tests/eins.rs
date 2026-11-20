//! EINS (plan/spec/assistent-eins K8): the one level `ai.level` takes the place of the per-tool levels, the
//! helper's per-kind levels and Durchblick's per-program Automodus; the removed keys leave the files and the
//! journal once; the level is `auto` unless Georg chose another (DECISIONS 18.11.); loosening it is his own yes.

mod common;
use common::*;
use settingsd::change::Who;
use settingsd::json::Json;

/// Georg's own values on the Mini-PC (plan/spec/system-einstellungen/settings-list-gen151.txt, the lines EINS
/// touches): the Automodus on for two programs, every helper kind but reading on ask, the explain and jobs roles.
const GEORG_AI: &str = "ai.providers = \"abo\"\nai.role.chat = \"abo\"\nai.role.explain = \"abo\"\nai.role.jobs = \"abo\"\nai.role.jobs_fallback = \"abo\"\n\
ai.tools.settings_set.allow = \"ask\"\nai.tools.durchblick_handeln.allow = \"ask\"\nai.tools.version.allow = \"always\"\n";
const GEORG_DB: &str = "durchblick.antwort = \"auto\"\ndurchblick.bild.auto.steam = true\ndurchblick.handeln.auto.bitscomp_einstellungen = true\n\
durchblick.handeln.auto.steam = true\ndurchblick.handeln.auto.foot = false\ndurchblick.handeln.unumkehrbar_fragen = true\n\
durchblick.vorschau.bild = true\ndurchblick.vorschau.liste = true\ndurchblick.taste = \"win_f1\"\n";

fn entry(seq: u32, key: &str, new: &str) -> String {
    format!(r#"{{"seq":{seq},"t_ms":5,"key":"{key}","old":null,"new":{new},"source":"page","op":"set"}}"#)
}

fn stick(env: &Env, files: &[(&str, &str)]) {
    let dir = env.paths.stick.join("settings");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap();
    }
}

#[test]
fn georgs_values_leave_once_and_the_level_is_auto() {
    let env = Env::new("eins_georg", true);
    let journal = [entry(1, "durchblick.handeln.auto.steam", "true"), entry(2, "ai.role.explain", "\"abo\""), entry(3, "durchblick.taste", "\"win_f1\"")].join("\n") + "\n";
    stick(&env, &[("ai.toml", GEORG_AI), ("durchblick.toml", GEORG_DB), ("journal.log", &journal)]);
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.level"), s("auto"), "the default (DECISIONS 18.11.)");
    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd level.migrated level_set=false removed_keys=14"), "{log}");
    let ai = env.stick_file("settings/ai.toml");
    assert!(!ai.contains("ai.level") && ai.contains("ai.role.chat = \"abo\""), "{ai}");
    for gone in ["ai.role.explain", "ai.role.jobs", "ai.tools.", "ai.helper."] {
        assert!(!ai.contains(gone), "{gone} left ai.toml: {ai}");
    }
    // bitscomp's keys that EINS removed go; the ones it keeps (durchblick.taste) stay for its registration.
    let db = env.stick_file("settings/durchblick.toml");
    assert!(db.contains("durchblick.taste") && !db.contains("handeln") && !db.contains("vorschau") && !db.contains("bild.auto") && !db.contains("antwort"), "{db}");
    let j = env.stick_file("settings/journal.log");
    assert!(!j.contains("durchblick.handeln.auto.steam") && !j.contains("ai.role.explain") && j.contains("durchblick.taste"), "{j}");
    assert!(env.stick_file("settings/MIGRATED").lines().any(|l| l == "eins_level"));
    drop(r);

    // Once: Georg sets ask afterwards, a restart keeps it, nothing more is written.
    let (mut r, _) = env.open();
    r.set("ai.level", &s("ask"), &WHO).unwrap();
    drop(r);
    let files = |env: &Env| ["ai.toml", "durchblick.toml", "journal.log", "MIGRATED"].map(|f| env.stick_file(&format!("settings/{f}")));
    let before = files(&env);
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.level"), s("ask"));
    assert_eq!(before, files(&env), "the second start writes nothing");
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("migrated")), "{:?}", ev.lock().unwrap());
}

#[test]
fn a_level_already_set_stays() {
    let env = Env::new("eins_set", true);
    stick(&env, &[("ai.toml", "ai.level = \"answer\"\n"), ("durchblick.toml", "durchblick.handeln.auto.steam = false\n")]);
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.level"), s("answer"));
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("level.migrated level_set=true removed_keys=1")));
    assert!(!env.stick_file("settings/durchblick.toml").contains("handeln"));
}

#[test]
fn only_georg_loosens_the_level() {
    let env = Env::new("eins_leash", true);
    let (mut r, _) = env.open();
    let anyone = Who { source: "test", confirmed: false, asker: Some(&prog("assist")) };
    r.set("ai.level", &s("answer"), &anyone).unwrap();
    for looser in ["ask", "auto"] {
        let e = r.set("ai.level", &s(looser), &anyone).unwrap_err();
        assert!(matches!(e.code, "needs_confirm" | "not_confirmer"), "{looser}: {}", e.code);
    }
    assert_eq!(value(&r, "ai.level"), s("answer"));
    assert_eq!(r.set("ai.level", &s("immer"), &anyone).unwrap_err().code, "bad_value");
    let d = r.describe("ai.level").unwrap();
    let schema = d.get("key").and_then(|k| k.get("schema")).cloned().unwrap();
    assert_eq!(schema.get("choices").map(Json::to_text).as_deref(), Some(r#"["answer","ask","auto"]"#));
    assert_eq!(schema.get("choices_de").map(Json::to_text).as_deref(), Some(r#"["Nur antworten","Fragt vorher","Automodus"]"#));
}
