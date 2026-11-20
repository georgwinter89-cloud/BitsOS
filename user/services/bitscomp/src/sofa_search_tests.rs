use std::time::Duration;

use super::*;
use crate::pad::Dir::{self, Down, Left, Right, Up};
use crate::sofa_focus::{Item, index, step};
use crate::sofa_layout::{self as lay, Level};

fn go(items: &[Item], from: &str, d: Dir) -> Option<String> {
    step(items, index(items, from).expect(from), d).map(|i| items[i].id.clone())
}

#[test]
fn the_text_is_edited_by_character_with_umlauts_and_sz() {
    let mut f = Field::default();
    for c in "Grße".chars() {
        assert!(f.insert(c));
    }
    f.step(-2);
    assert_eq!(f.cursor, 2);
    assert!(f.insert('ö'), "an umlaut is one character");
    assert_eq!((f.text.as_str(), f.cursor, f.len()), ("Größe", 3, 5));
    assert!(f.backspace());
    assert_eq!((f.text.as_str(), f.cursor), ("Grße", 2));
    assert!(f.delete(), "Entf takes the ß after the cursor");
    assert_eq!(f.text, "Gre");
    f.home();
    assert!(!f.backspace(), "nothing before the start");
    f.end();
    assert!(!f.delete(), "nothing after the end");
    f.step(10);
    assert_eq!(f.cursor, 3, "the cursor stays inside the text");
    // The bound counts characters, not bytes; deleting lets the next one in again.
    let mut f = Field::of(&"ä".repeat(TYPED_MAX + 5));
    assert_eq!((f.len(), f.cursor), (TYPED_MAX, TYPED_MAX));
    assert!(!f.insert('ü') && f.full);
    assert!(f.backspace() && !f.full);
    assert!(f.insert('ü'));
}

#[test]
fn the_keys_do_what_their_labels_say() {
    assert_eq!(key("⌫ Löschen"), Some(Key::Delete));
    assert_eq!(key("⇧ Groß"), Some(Key::Shift));
    assert_eq!(key("&123"), Some(Key::Level));
    assert_eq!(key("abc"), Some(Key::Level));
    assert_eq!(key("Leerzeichen"), Some(Key::Space));
    assert_eq!(key("Suchen"), Some(Key::Enter));
    assert_eq!(key("Fertig"), Some(Key::Enter));
    assert_eq!(key("ß"), Some(Key::Char('ß')));
    assert_eq!(key("€"), Some(Key::Char('€')));
    assert_eq!(key("\""), Some(Key::Char('"')));
    assert_eq!(key("xy"), None);
    assert_eq!(shifted('ä', true), 'Ä');
    assert_eq!(shifted('ü', true), 'Ü');
    assert_eq!(shifted('ß', true), 'ß', "ß has no capital of one character here: it stays");
    assert_eq!(shifted('1', true), '1');
    assert_eq!(shifted('q', false), 'q');
    // The second level holds the 24 symbols of §2.6 and the digits; each label is a key.
    let all: Vec<&str> = lay::key_rows(Level::Symbols, "Suchen").into_iter().flatten().map(|(l, _)| l).collect();
    for s in "!\"§$%&/()=?@€#+*'_:;,<>1234567890".chars() {
        assert!(all.contains(&s.to_string().as_str()), "{s} on the second level");
    }
    for row in [lay::key_rows(Level::Letters, "Suchen"), lay::key_rows(Level::Symbols, "Fertig")] {
        for (l, _) in row.into_iter().flatten() {
            assert!(key(l).is_some(), "{l} does something");
        }
    }
}

#[test]
fn the_brave_address_is_url_encoded() {
    assert_eq!(url_encode("dungeon keeper 2"), "dungeon%20keeper%202");
    assert_eq!(url_encode("Größe"), "Gr%C3%B6%C3%9Fe");
    assert_eq!(url_encode("a&b=c?d#e/f+g"), "a%26b%3Dc%3Fd%23e%2Ff%2Bg", "nothing of the text becomes part of the address");
    assert_eq!(url_encode("x-y.z_~"), "x-y.z_~");
    assert_eq!(brave_url("  dungeon keeper 2 "), "https://search.brave.com/search?q=dungeon%20keeper%202");
}

#[test]
fn a_held_key_repeats_after_500_ms_then_every_33_ms() {
    let t0 = Duration::from_secs(10);
    let mut r = Repeat::start(t0);
    assert!(!r.fire(t0 + Duration::from_millis(499)));
    assert!(r.fire(t0 + Duration::from_millis(500)));
    assert!(!r.fire(t0 + Duration::from_millis(532)));
    assert!(r.fire(t0 + Duration::from_millis(533)));
    // A late tick (the loop stalled) repeats once, then the rhythm goes on from now.
    assert!(r.fire(t0 + Duration::from_millis(2000)));
    assert!(!r.fire(t0 + Duration::from_millis(2010)));
    assert_eq!(r.next, t0 + Duration::from_millis(2033));
}

#[test]
fn the_keyboard_walk_wraps_per_row_on_both_levels_and_reaches_the_second_hit_row() {
    let (bw, bh) = (1920, 1080);
    let items = lay::search(bw, bh, true, 4, 0, Level::Letters, "Suchen");
    // Up from the digits: the second hit row, the card nearest by x; up again the first row.
    assert_eq!(go(&items, "key:1", Up).as_deref(), Some("hit:3"));
    assert_eq!(go(&items, "hit:3", Up).as_deref(), Some("hit:0"));
    assert_eq!(go(&items, "hit:5", Down).as_deref(), Some("key:⌫ Löschen"), "down to the key nearest by x");
    // The second row walks on past the three shown and wraps in its row.
    assert_eq!(go(&items, "hit:5", Right).as_deref(), Some("hit:6"));
    assert_eq!(go(&items, "hit:6", Right).as_deref(), Some("hit:3"));
    let items = lay::search(bw, bh, false, 0, 0, Level::Symbols, "Suchen");
    assert!(index(&items, "hit:0").is_none(), "no cards without a text");
    assert_eq!(go(&items, "key:1", Up), None);
    assert_eq!(go(&items, "key:!", Left).as_deref(), Some("key:?"));
    assert_eq!(go(&items, "key:>", Right).as_deref(), Some("key:@"));
    assert_eq!(go(&items, "key:abc", Right).as_deref(), Some("key:Leerzeichen"));
    assert_eq!(go(&items, "key:.", Right).as_deref(), Some("key:,"), "the short row wraps too");
    assert_eq!(go(&items, "key:-", Down).as_deref(), Some("key:Leerzeichen"));
    // Every key row is centred (board Tastatur: justify-content center).
    for y in [554, 644, 734, 824, 914] {
        let row: Vec<&Item> = items.iter().filter(|i| i.rect.y == y).collect();
        assert!(!row.is_empty(), "a row at {y}");
        let (l, r) = (row[0].rect.x, row[row.len() - 1].rect.x + row[row.len() - 1].rect.w);
        assert!((l - (bw - r)).abs() <= 1, "row {y} centred: {l}..{r}");
    }
}

#[test]
fn the_second_row_follows_the_focus() {
    let mut s = Search::default();
    s.follow(5);
    assert_eq!(s.first, 3, "card 5 is the last of the three shown");
    s.follow(4);
    assert_eq!(s.first, 3);
    s.follow(1);
    assert_eq!(s.first, 1);
    assert_eq!(s.more(), 0, "no text: no cards");
    assert!(!s.shows_hits());
    s.field = Field::of("  ");
    assert!(!s.shows_hits(), "spaces alone find nothing");
    s.target = Target::Setting { key: "net.wifi.ssid".into(), title: "WLAN-Name".into(), secret: false };
    s.field = Field::of("Wohnzimmer");
    assert!(!s.shows_hits(), "a settings field has no hits");
    assert_eq!(s.target.enter(), "Fertig");
}
