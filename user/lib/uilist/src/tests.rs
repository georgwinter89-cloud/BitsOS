use super::*;

fn page() -> Window {
    let mut w = Window {
        window: "w7".into(),
        app: "bitscomp-einstellungen".into(),
        title: "Einstellungen".into(),
        pid: 42,
        source: Source::Desktop,
        focused: true,
        rect: [0, 0, 800, 600],
        elements: vec![
            Element::new(Role::Heading, "Aussehen", [10, 10, 200, 20]),
            Element::new(Role::Toggle, "Bewegung", [10, 40, 60, 30]).value("an").with_state(State::Checked).acts(&[Action::Toggle, Action::Press]),
            Element::secret("Schlüssel", [10, 80, 200, 30]).value("sk-live-123").acts(&[Action::Type]),
            Element::new(Role::Group, "Karte", [0, 120, 400, 200]).child(Element::new(Role::Button, "Öffnen", [20, 140, 80, 30]).acts(&[Action::Press])),
        ],
    };
    w.number();
    w
}

#[test]
fn ids_are_stable_short_and_name_their_window() {
    let a = page();
    let b = page();
    let ids: Vec<&str> = a.walk().iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, b.walk().iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), "the same list numbers the same");
    assert!(ids.iter().all(|i| i.starts_with("w7/") && i.len() <= 3 + 7 + 3), "{ids:?}");
    assert_eq!(window_of(ids[0]), Some("w7"));
    for written in ["#w7/1a2b", "[#w7/1a2b]", " [ #w7/1a2b ] ", "w7/1a2b", "[w7/1a2b]"] {
        assert_eq!(clean_id(written), "w7/1a2b", "{written}");
    }
    let mut u: Vec<&str> = ids.clone();
    u.sort();
    u.dedup();
    assert_eq!(u.len(), ids.len(), "every id once");
    // Another element before it does not move an id (keys, not positions).
    let mut c = page();
    c.elements.insert(0, Element::new(Role::Text, "Neu", [0, 0, 1, 1]));
    c.elements.iter_mut().for_each(|e| e.id.clear());
    c.number();
    assert_eq!(c.elements[2].id, a.elements[1].id);
    // Two elements with the same key get -2.
    let mut ids = Ids::new("d");
    let (x, y) = (ids.id("k"), ids.id("k"));
    assert_eq!(y, format!("{x}-2"));
}

#[test]
fn a_secret_never_carries_its_value() {
    let w = page();
    let s = w.walk().into_iter().find(|e| e.name == "Schlüssel").unwrap();
    assert!(s.value.is_none() && s.has(State::Secret));
    let json = serde_json::to_string(&w).unwrap();
    assert!(!json.contains("sk-live"), "{json}");
    assert!(!render(&[w.clone()]).contains("sk-live"));
    // A value put in afterwards by hand is dropped by scrub, children too.
    let mut v = vec![Element::new(Role::Group, "g", [0, 0, 1, 1]).child(Element::secret("p", [0, 0, 1, 1]))];
    v[0].children[0].value = Some("geheim".into());
    scrub(&mut v);
    assert!(v[0].children[0].value.is_none());
    // A value set before the secret state goes too.
    assert!(Element::new(Role::Field, "x", [0; 4]).value("abc").with_state(State::Secret).value.is_none());
}

#[test]
fn json_has_the_contract_shape() {
    let w = page();
    let j: serde_json::Value = serde_json::to_value(&w).unwrap();
    for k in ["window", "app", "title", "pid", "source", "focused", "rect", "elements"] {
        assert!(j.get(k).is_some(), "window field {k}");
    }
    assert_eq!(j["source"], "desktop");
    let t = &j["elements"][1];
    assert_eq!(t["role"], "toggle");
    assert_eq!(t["state"][0], "checked");
    assert_eq!(t["actions"][0], "toggle");
    assert_eq!(t["rect"], serde_json::json!([10, 40, 60, 30]));
    assert!(t.get("visible").is_none(), "visible is written only when false");
    assert!(j["elements"][0].get("value").is_none());
    let back: Window = serde_json::from_value(j).unwrap();
    assert_eq!(back, w);
    // A list written by another lane with plain serde_json reads.
    let other: Window = serde_json::from_str(r#"{"window":"w1","app":"brave","title":"t","pid":3,"source":"atspi","focused":false,"rect":[0,0,1,1],"elements":[{"id":"w1/0","role":"link","name":"Hilfe","state":[],"rect":[1,2,3,4],"actions":["press"],"children":[]}]}"#).unwrap();
    assert_eq!(other.elements[0].role, Role::Link);
    assert!(other.elements[0].visible && !other.elements[0].answers);
    // The answer of a question: written only when set, and it is a confirm step too.
    let yes = serde_json::to_value(Element::new(Role::Button, "Ja", [0; 4]).settles()).unwrap();
    assert_eq!((&yes["answers"], &yes["confirm"]), (&serde_json::json!(true), &serde_json::json!(true)));
    assert!(serde_json::to_value(Element::new(Role::Button, "Nein", [0; 4])).unwrap().get("answers").is_none());
}

#[test]
fn hit_finds_the_innermost_shown_element() {
    let mut w = page();
    assert_eq!(w.at(25.0, 145.0).map(|e| e.name.as_str()), Some("Öffnen"));
    assert_eq!(w.at(300.0, 300.0).map(|e| e.name.as_str()), Some("Karte"));
    assert!(w.at(700.0, 500.0).is_none());
    assert!(w.at_where(300.0, 300.0, &|e| !e.actions.is_empty()).is_none(), "the group alone has no action");
    assert_eq!(w.at_where(25.0, 145.0, &|e| !e.actions.is_empty()).map(|e| e.name.as_str()), Some("Öffnen"));
    w.elements[3].children[0].visible = false;
    assert_eq!(w.at(25.0, 145.0).map(|e| e.name.as_str()), Some("Karte"));
    let id = w.elements[1].id.clone();
    assert_eq!(w.find(&id).map(|e| e.name.as_str()), Some("Bewegung"));
}

#[test]
fn text_has_one_indented_line_per_element() {
    let t = render(&[page()]);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 1 + 5);
    assert!(lines[0].starts_with("window w7 app=\"bitscomp-einstellungen\"") && lines[0].ends_with(" focused"), "{}", lines[0]);
    assert!(lines[2].starts_with("  toggle \"Bewegung\" = \"an\" [checked] @10,40,60,30 #w7/"), "{}", lines[2]);
    assert!(lines[2].ends_with("(toggle press)"));
    assert!(lines[5].starts_with("    button \"Öffnen\""), "children one level deeper: {}", lines[5]);
}

#[test]
fn own_programs_file_is_bounded_and_scrubbed() {
    let dir = std::env::temp_dir().join(format!("uilist-own-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut v = vec![Element::new(Role::Button, "Quadrat", [100, 100, 64, 64]).acts(&[Action::Press]), Element::secret("Passwort", [0, 0, 10, 10])];
    v[1].value = Some("hunter2".into());
    write_own(&dir, 4242, &v).unwrap();
    let bytes = std::fs::read(own_file(&dir, 4242)).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("hunter2"));
    let back = parse_own(&bytes).unwrap();
    assert_eq!(back[0].name, "Quadrat");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(own_file(&dir, 4242)).unwrap().permissions().mode() & 0o777, 0o600);
    // A file with a value in a secret field (written by hand) reads without it.
    let forged = br#"{"elements":[{"id":"","role":"field","name":"p","value":"x1","state":["secret"],"rect":[0,0,1,1]}]}"#;
    assert!(parse_own(forged).unwrap()[0].value.is_none());
    assert!(parse_own(&vec![b' '; OWN_MAX as usize + 1]).is_err());
    assert!(parse_own(b"{").is_err());
    let deep = (0..40).fold(String::from(r#"{"id":"","role":"group","name":"x","rect":[0,0,1,1]}"#), |acc, _| format!(r#"{{"id":"","role":"group","name":"x","rect":[0,0,1,1],"children":[{acc}]}}"#));
    assert!(parse_own(format!(r#"{{"elements":[{deep}]}}"#).as_bytes()).is_err());
    let many = vec![r#"{"id":"","role":"text","name":"x","rect":[0,0,1,1]}"#; 2001].join(",");
    assert!(parse_own(format!(r#"{{"elements":[{many}]}}"#).as_bytes()).is_err());
    remove_own(&dir, 4242);
    assert!(!own_file(&dir, 4242).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_events_leave_out_what_they_do_not_have() {
    let e = Event { t: 5, kind: "press".into(), app: "bitscomp".into(), window: "desktop".into(), element: "desktop/abc".into(), text: None, name: Some("Programme".into()), by: None };
    let s = serde_json::to_string(&e).unwrap();
    assert_eq!(s, r#"{"t":5,"kind":"press","app":"bitscomp","window":"desktop","element":"desktop/abc","name":"Programme"}"#);
    assert_eq!(Action::of_word("toggle"), Some(Action::Toggle));
    assert_eq!(Action::of_word("klick"), None);
}

#[test]
fn a_field_says_whether_it_has_one_line_or_several() {
    let e = Element::new(Role::Field, "Adresse", [0, 0, 10, 10]).with_state(State::Singleline);
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["state"], serde_json::json!(["singleline"]));
    let back: Element = serde_json::from_value(serde_json::json!({"id": "w1/1", "role": "field", "name": "Text", "state": ["multiline"], "rect": [0, 0, 1, 1]})).unwrap();
    assert!(back.has(State::Multiline) && !back.has(State::Singleline));
}
