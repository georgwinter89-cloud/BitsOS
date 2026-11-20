//! DECISIONS 18.11. (everything built is on): the assistant's level is `auto`, `assist --mcp` may hand over the
//! changing tools, and a stored old default (`ask`, `false`, `deny`) becomes the new default once, logged; the
//! values of the removed keys (the effect level, ctxd's file cap) leave the files and the journal.

mod common;
use common::*;
use settingsd::json::Json;

fn stick(env: &Env, files: &[(&str, &str)]) {
    let dir = env.paths.stick.join("settings");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap();
    }
}

#[test]
fn the_new_defaults_are_on() {
    let env = Env::new("defaults_new", true);
    let (r, _) = env.open();
    assert_eq!(value(&r, "ai.level"), s("auto"));
    assert_eq!(value(&r, "ai.mcp.ask_tools"), Json::Bool(true));
    assert!(r.describe("theme.effects").is_err(), "the effect level has no key any more");
    assert!(env.stick_file("settings/MIGRATED").lines().any(|l| l == "defaults_on"));
}

#[test]
fn stored_old_defaults_take_the_new_once() {
    let env = Env::new("defaults_old", true);
    let journal = r#"{"seq":1,"t_ms":5,"key":"theme.effects","old":null,"new":"gpu","source":"page","op":"set"}"#.to_string() + "\n";
    stick(
        &env,
        &[
            ("ai.toml", "ai.level = \"ask\"\nai.mcp.ask_tools = false\nai.role.chat = \"abo\"\n"),
            ("theme.toml", "theme.effects = \"gpu\"\ntheme.radius = 4\n"),
            ("claude.toml", "claude.actions.brave_fill_form = \"deny\"\nclaude.actions.brave_download = \"allow\"\n"),
            ("leistung.toml", "leistung.nachts = false\n"),
            ("assistant.toml", "assistant.sidebar_sees = false\n"),
            ("context.toml", "context.index.max_files = 100000\ncontext.roots = \"/home/user/Dokumente\"\n"),
            ("journal.log", &journal),
        ],
    );
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.level"), s("auto"));
    assert_eq!(value(&r, "ai.mcp.ask_tools"), Json::Bool(true));
    let log = ev.lock().unwrap().join("\n");
    for line in [
        "migrated key=ai.level from=ask to=auto",
        "migrated key=ai.mcp.ask_tools from=off to=on",
        "migrated key=claude.actions.brave_fill_form from=deny",
        "migrated key=leistung.nachts from=off",
        "migrated key=assistant.sidebar_sees from=off",
        "migrated key=theme.effects removed=yes",
        "migrated key=context.index.max_files removed=yes",
    ] {
        assert!(log.contains(line), "{line}: {log}");
    }
    assert!(!log.contains("brave_download from"), "Georg's own allow is no old default: {log}");
    let ai = env.stick_file("settings/ai.toml");
    assert!(!ai.contains("ai.level") && !ai.contains("ask_tools") && ai.contains("ai.role.chat"), "{ai}");
    let theme = env.stick_file("settings/theme.toml");
    assert!(!theme.contains("effects") && theme.contains("theme.radius = 4"), "only the radius stays: {theme}");
    let claude = env.stick_file("settings/claude.toml");
    assert!(!claude.contains("brave_fill_form") && claude.contains("brave_download"), "{claude}");
    assert_eq!(env.stick_file("settings/leistung.toml"), "", "the file goes with its last line");
    assert_eq!(env.stick_file("settings/assistant.toml"), "");
    let ctx = env.stick_file("settings/context.toml");
    assert!(!ctx.contains("max_files") && ctx.contains("Dokumente"), "context.roots stays: {ctx}");
    let j = env.stick_file("settings/journal.log");
    assert!(!j.contains("theme.effects"), "{j}");
    assert!(j.contains(r#""key":"ai.level""#) && j.contains(r#""op":"migrate""#), "the level's change is journaled: {j}");
    drop(r);

    // Once: Georg chooses ask afterwards, a restart keeps it.
    let (mut r, _) = env.open();
    r.set("ai.level", &s("ask"), &WHO).unwrap();
    drop(r);
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.level"), s("ask"));
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("migrated")), "{:?}", ev.lock().unwrap());
}

#[test]
fn a_tighter_level_georg_chose_stays() {
    let env = Env::new("defaults_answer", true);
    stick(&env, &[("ai.toml", "ai.level = \"answer\"\n")]);
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.level"), s("answer"));
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("migrated key=ai.level")));
}
