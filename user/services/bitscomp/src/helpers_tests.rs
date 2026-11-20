use std::os::unix::fs::PermissionsExt;

use super::*;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitscomp-helpers-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).expect("mode");
    d
}

fn program_in(d: &Path) -> String {
    let p = d.join("helfer");
    std::fs::write(&p, "#!/bin/sh\n").expect("program");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("mode");
    p.display().to_string()
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

#[test]
fn a_description_is_read_and_everything_odd_is_left_out() {
    let d = tmp("parse");
    let p = program_in(&d);
    let good = format!("# a comment\nbitsos_helper = 2\nname = Mein Helfer\nabout = Antwortet über ein Programm.\npath = {p}\nplace = cloud\nlater = ignored\n");
    let h = parse("helfer", &good).expect("a good description");
    assert_eq!(h, Helper { id: "helfer".into(), name: "Mein Helfer".into(), about: "Antwortet über ein Programm.".into(), path: p.clone(), place: Some("cloud".into()), models: Vec::new() });
    assert_eq!(parse("helfer", &format!("bitsos_helper=1\nname=X\npath={p}\n")).expect("version 1, no about, no place").place, None);
    // ABOBILD: version 3 announces pictures; the page lists it like any other.
    assert_eq!(parse("helfer", &good.replace("bitsos_helper = 2", "bitsos_helper = 3").replace("later = ignored", "pictures = yes")).expect("version 3"), h);
    // ABOGLEICH: version 4 (assist's tools through the answer's tool socket) changes nothing on the page.
    assert_eq!(parse("helfer", &good.replace("bitsos_helper = 2", "bitsos_helper = 4")).expect("version 4"), h);
    for (id, text) in [
        ("Helfer", good.clone()),
        ("helfer", good.replace("bitsos_helper = 2", "bitsos_helper = 5")),
        ("helfer", good.replace("bitsos_helper = 2\n", "")),
        ("helfer", good.replace("name = Mein Helfer\n", "")),
        ("helfer", good.replace(&format!("path = {p}"), "path = relativ/helfer")),
        ("helfer", good.replace(&format!("path = {p}"), &format!("path = {}", d.join("fehlt").display()))),
        ("helfer", good.replace(&format!("path = {p}"), &format!("path = {}", d.display()))),
        ("helfer", good.replace("place = cloud", "place = mond")),
        ("helfer", good.replace("Mein Helfer", &"x".repeat(NAME_MAX + 1))),
        ("helfer", good.replace("name = Mein Helfer", "kaputt")),
    ] {
        assert!(parse(id, &text).is_err(), "{id}: {text}");
    }
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).expect("mode");
    assert!(parse("helfer", &good).is_err(), "a file that cannot run is no helper");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_models_line_announces_models_and_a_wrong_one_costs_only_that_line() {
    let d = tmp("models");
    let p = program_in(&d);
    let with = |line: &str| parse("abo", &format!("bitsos_helper = 2\nname = Abo\npath = {p}\n{line}\n")).expect("still a description");
    let m = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
    assert_eq!(with("models = fable: Fable, opus: Opus, sonnet: Sonnet, haiku: Haiku").models, m(&[("fable", "Fable"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")]));
    assert_eq!(with("models=claude-opus-5-5[1m]:Opus 5.5 (lang)").models, m(&[("claude-opus-5-5[1m]", "Opus 5.5 (lang)")]));
    assert_eq!(with("# no line").models, m(&[]));
    let many = (0..=MODELS_MAX).map(|i| format!("m{i}: M{i}")).collect::<Vec<_>>().join(", ");
    for bad in [
        "models =",
        "models = opus",
        "models = opus: Opus,",
        "models = : Leer",
        "models = opus:",
        "models = --dangerously-skip-permissions: Alles",
        "models = mit leerzeichen: X",
        "models = opus: Opus, opus: Nochmal",
        "models = opus: Opus: lang",
        &format!("models = {}: X", "m".repeat(MODEL_MAX + 1)),
        &format!("models = opus: {}", "x".repeat(LABEL_MAX + 1)),
        &format!("models = {many}"),
    ] {
        assert_eq!(with(bad).models, m(&[]), "{bad}");
    }
    assert_eq!(with("models = opus: Opus\nmodels = haiku: Haiku").models, m(&[("opus", "Opus")]), "the first models line counts");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_folder_lists_only_its_owners_plain_descriptions() {
    let d = tmp("scan");
    let p = program_in(&d);
    let desc = |name: &str| format!("bitsos_helper = 2\nname = {name}\npath = {p}\n");
    std::fs::write(d.join("b-zwei.helper"), desc("Zwei")).expect("write");
    std::fs::write(d.join("a-eins.helper"), desc("Eins")).expect("write");
    std::fs::write(d.join("kaputt.helper"), "nichts davon").expect("write");
    std::fs::write(d.join("andere.txt"), desc("Andere")).expect("write");
    std::fs::write(d.join("gross.helper"), "#".repeat(5000)).expect("write");
    std::fs::write(d.join("offen.helper"), desc("Offen")).expect("write");
    std::fs::set_permissions(d.join("offen.helper"), std::fs::Permissions::from_mode(0o666)).expect("mode");
    std::os::unix::fs::symlink(d.join("a-eins.helper"), d.join("link.helper")).expect("link");
    let names: Vec<String> = scan(&d, uid()).into_iter().map(|h| format!("{}={}", h.id, h.name)).collect();
    assert_eq!(names, ["a-eins=Eins", "b-zwei=Zwei"]);
    assert!(scan(&d.join("fehlt"), uid()).is_empty(), "no folder: nothing, no error");
    assert!(scan(&d, uid().wrapping_add(4242)).is_empty() || uid() == 0, "another user's folder is not read");
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).expect("mode");
    assert!(scan(&d, uid()).is_empty(), "a folder anybody can write is not read");
    let _ = std::fs::remove_dir_all(&d);
}
