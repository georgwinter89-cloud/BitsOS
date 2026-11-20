use uilist::{Action, Role, Source};

use super::*;

fn page() -> Window {
    let mut w = Window { window: "w3".into(), app: "bitscomp.einstellungen".into(), title: "Einstellungen".into(), pid: 7, source: Source::Desktop, focused: true, rect: [0, 0, 800, 600], elements: Vec::new() };
    w.elements = vec![
        Element::new(Role::Choice, "Helles oder dunkles Design", [10, 10, 300, 40]).key("row:theme.mode").value("dunkel").acts(&[Action::Choose]),
        Element::secret("Schlüssel von wolke", [10, 60, 300, 40]).key("row:ai.provider.wolke.key"),
        Element::new(Role::Field, "WLAN-Passwort", [10, 110, 300, 40]).key("psk").value("geheim123").with_state(State::Secret),
        Element::new(Role::Button, "Versteckt", [10, 900, 30, 30]).key("hidden").shown(false),
    ];
    w.number();
    w
}

#[test]
fn the_window_goes_in_with_the_pointed_element_marked() {
    let w = page();
    let id = w.elements[0].id.clone();
    let q = build(&Inputs { window: &w, pointed: Some(&id), access: None, picture: false });
    assert!(q.list.contains(&format!("choice \"Helles oder dunkles Design\" = \"dunkel\" #{id}  <- darauf zeigt Georg")), "{}", q.list);
    assert_eq!(q.kinds, [Kind::Liste]);
    assert!(!q.list.contains("Versteckt"), "what is not shown is not described");
}

#[test]
fn secrets_never_go_in() {
    let mut w = page();
    // Even an element a careless provider filled: the secret state alone keeps the value out.
    w.elements[1].value = Some("sk-ant-0000".into());
    let q = build(&Inputs { window: &w, pointed: None, access: None, picture: false });
    assert!(q.list.contains("field \"Schlüssel von wolke\" [secret]") && q.list.contains("field \"WLAN-Passwort\" [secret]"), "{}", q.list);
    for never in ["sk-ant-0000", "geheim123"] {
        assert!(!q.list.contains(never), "{never} went out: {}", q.list);
    }
}

#[test]
fn only_the_window_goes_in_never_a_history_or_files() {
    // PLAN K4: the window's list and its picture; what Georg did lately and file names are the model's own tools'.
    let w = page();
    let q = build(&Inputs { window: &w, pointed: None, access: None, picture: true });
    assert_eq!(q.kinds, [Kind::Liste, Kind::Bild]);
    assert!(q.extras.is_empty() && !q.list.contains("zuletzt getan") && !q.list.contains("Dateien, die"), "{}", q.list);
    let c = q.context(&w, Some("QUJD".into()));
    assert_eq!(c.get("window").and_then(|x| x.get("app")).and_then(Json::as_str), Some("bitscomp.einstellungen"));
    assert_eq!(c.get("image").and_then(|x| x.get("media_type")).and_then(Json::as_str), Some("image/png"));
    assert!(c.get("list").and_then(Json::as_str).is_some_and(|l| l.contains("Helles oder dunkles Design")) && c.get("extras").is_none());
    assert_eq!(q.words(), "liste,bild");
}

#[test]
fn names_of_keys_and_passwords_are_secret_paths() {
    for p in ["/data/home/1000/.ssh/id_ed25519", "/data/home/1000/Dokumente/passwoerter.txt", "/data/home/1000/tresor.kdbx", "/x/id_rsa"] {
        assert!(secret_path(p), "{p}");
    }
    assert!(!secret_path("/data/home/1000/Dokumente/steuer-2025.pdf"));
}

#[test]
fn the_access_log_only_for_a_question_about_what_a_program_did() {
    assert!(about_program("Was hat Brave gerade im Hintergrund gemacht?"));
    assert!(about_program("Mit wem ist das Programm verbunden?"));
    assert!(!about_program("Wie stelle ich das dunkle Design ein?"));
    let w = page();
    let log = "liest /data/home/1000/Dokumente/brief.odt\nliest /data/home/1000/.ssh/id_rsa\nverbindet search.brave.com:443";
    let q = build(&Inputs { window: &w, pointed: None, access: Some(log), picture: false });
    assert!(q.extras.contains("brief.odt") && q.extras.contains("search.brave.com") && !q.extras.contains(".ssh"), "{}", q.extras);
    assert!(q.kinds.contains(&Kind::Zugriffe));
    assert!(q.context(&w, None).get("extras").and_then(Json::as_str).is_some_and(|e| e.contains("brief.odt")));
}

#[test]
fn a_picture_window_says_so() {
    let mut w = page();
    w.source = Source::Picture;
    w.elements = vec![Element::new(Role::Picture, "Bild des Fensters", [0, 0, 800, 600]).key("picture")];
    w.number();
    let q = build(&Inputs { window: &w, pointed: None, access: None, picture: false });
    assert!(q.list.contains("nur ein Bild"));
}

#[test]
fn ids_are_taken_out_of_the_answer() {
    let (shown, ids) = ids_in("Hier wählst du das Design [#w3/1a]. Der Schlüssel [#w3/9z] bleibt geheim [#w3/1a].");
    assert_eq!(shown, "Hier wählst du das Design. Der Schlüssel bleibt geheim.");
    assert_eq!(ids, ["w3/1a", "w3/9z"]);
    // While it streams, half a bracket is not shown.
    assert_eq!(ids_in("Das ist der Knopf [#w3/").0, "Das ist der Knopf ");
    assert_eq!(ids_in("eine Liste [# mit Leerzeichen] bleibt").0, "eine Liste [# mit Leerzeichen] bleibt");
}
