//! Version 3 of the helper contract: the newest question's picture goes to a helper program only when the
//! helper announced in its description that it takes pictures; every other helper gets text as before and
//! still answers. Helper scripts written here, a real engine and a real settingsd; no real provider.

mod common;
use assist::conv::{Image, Stop};
use assist::engine::{Answer, Session};
use assist::json::{self, Json};
use assist::provider::program::pictures::DIR;
use assist::tools::settings::SettingsTools;
use common::Rec;
use common::stage::Stage;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn arr(j: &Json) -> Option<&Vec<Json>> {
    if let Json::Arr(a) = j { Some(a) } else { None }
}

const BUDGET: usize = 64 << 20;
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// A helper as one of version 1 was written: it reads its line, keeps it, answers with text.
fn helper(s: &Stage) -> PathBuf {
    s.helper(r#"n=$(cat calls 2>/dev/null || echo 0); n=$((n+1)); echo $n > calls
cat > request.$n
echo '{"text":"Ich sehe das Fenster."}'
echo '{"done":true,"stop":"end","error":null}'"#)
}

/// The owner's home of this stage with the helpers' folder in it (only its owner writes it).
fn home(s: &Stage) -> PathBuf {
    let home = s.w.root.join("home");
    let dir = home.join(DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    home
}

fn describe(home: &Path, name: &str, text: &str, mode: u32) {
    let file = home.join(DIR).join(format!("{name}.helper"));
    std::fs::write(&file, text).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn picture() -> Image {
    Image { media_type: "image/png".into(), base64: PNG.into() }
}

/// Asks once with the picture and once without; the two request lines the helper got.
fn two_questions(s: &Stage, home: &Path) -> (Json, Json, Rec) {
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    session.helpers = Some(home.to_path_buf());
    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask_with("Was ist das?", Some(picture()), &mut ui), Ok(Stop::End), "{:?}", ui.logs);
    assert_eq!(session.ask("Und das da?", &mut ui), Ok(Stop::End));
    assert_eq!(ui.text, "Ich sehe das Fenster.Ich sehe das Fenster.");
    let req = |n: u32| json::parse(s.file(&format!("request.{n}")).trim()).unwrap();
    (req(1), req(2), ui)
}

fn images(req: &Json) -> Vec<Json> {
    req.get("messages").and_then(arr).unwrap().iter().filter_map(|m| m.get("image").cloned()).collect()
}

#[test]
fn a_helper_that_says_nothing_about_pictures_gets_text_and_answers() {
    let s = Stage::new("picture-none");
    let path = helper(&s);
    let home = home(&s);
    // Descriptions that do not announce pictures for this helper.
    let p = path.display();
    describe(&home, "zwei", &format!("bitsos_helper = 2\nname = Zwei\npath = {p}\npictures = yes\n"), 0o644);
    describe(&home, "ohne", &format!("bitsos_helper = 3\nname = Ohne\npath = {p}\n"), 0o644);
    describe(&home, "fremd", "bitsos_helper = 3\nname = Fremd\npath = /bin/sh\npictures = yes\n", 0o644);
    let (first, second, ui) = two_questions(&s, &home);
    assert_eq!(first.get("bitsos_helper").and_then(Json::as_i64), Some(5));
    assert!(images(&first).is_empty() && images(&second).is_empty(), "{first:?}");
    assert_eq!(first.get("messages").map(Json::to_text).as_deref(), Some(r#"[{"role":"user","text":"Was ist das?"}]"#));
    assert!(ui.logs.iter().any(|l| l == "provider=h picture left out: its helper does not say it takes pictures"), "{:?}", ui.logs);
}

#[test]
fn a_description_someone_else_could_write_announces_nothing() {
    let s = Stage::new("picture-writable");
    let path = helper(&s);
    let home = home(&s);
    describe(&home, "offen", &format!("bitsos_helper = 3\nname = Offen\npath = {}\npictures = yes\n", path.display()), 0o666);
    let (first, _, _) = two_questions(&s, &home);
    assert!(images(&first).is_empty(), "{first:?}");
}

#[test]
fn a_helper_that_takes_pictures_gets_the_newest_question_s_picture() {
    let s = Stage::new("picture-yes");
    let path = helper(&s);
    let home = home(&s);
    // The description names a link to the helper: links are followed, as for the settings page.
    let link = s.w.root.join("helper-link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    describe(&home, "bild", &format!("# a helper of version 3\nbitsos_helper = 3\nname = Bild\npath = {}\npictures = yes\n", link.display()), 0o644);
    let (first, second, ui) = two_questions(&s, &home);
    assert_eq!(images(&first), [json::parse(&format!(r#"{{"media_type":"image/png","data":"{PNG}"}}"#)).unwrap()], "exactly the picture, base64 as it was");
    let last = first.get("messages").and_then(arr).unwrap().last().unwrap();
    assert_eq!((last.str("role"), last.str("text")), (Some("user"), Some("Was ist das?")), "the picture is on the question it was taken for");
    // Georg 14.11.: a follow-up about the same window must still see it (the helper keeps nothing between
    // requests), so the newest picture rides on the newest question again, and only there.
    assert_eq!(images(&second), images(&first), "the follow-up carries the same picture once: {second:?}");
    let msgs = second.get("messages").and_then(arr).unwrap();
    assert_eq!(msgs.len(), 3, "the follow-up still carries the conversation as text");
    assert!(msgs[..2].iter().all(|m| m.get("image").is_none()), "only the newest question carries it: {second:?}");
    assert!(msgs[2].get("image").is_some(), "on the newest question: {second:?}");
    assert!(!ui.logs.iter().any(|l| l.contains("picture left out")), "{:?}", ui.logs);
}
