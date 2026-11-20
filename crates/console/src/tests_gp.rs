//! GP-ROUND: the report page's colours and its end at a program's handover (checker findings 3 and 7).

use crate::report_page;
use alloc::string::String;
use alloc::vec::Vec;
use gp_report::page::wrap;

#[test]
fn a_wrapped_red_row_is_red_on_every_screen_row() {
    let mut lines: Vec<String> = alloc::vec![String::from("BitsOS-Bericht                     Seite 1 von 1"), String::new(), String::from("Netzwerk")];
    let red = "  Netzwerk 5/8: Kein Datenpaket nach 30 Sekunden (gesendet 3, davon fertig 0): Karte angehalten bei Schritt 5";
    let wrapped = wrap(red, 40);
    assert!(wrapped.len() >= 3, "{wrapped:?}");
    // The mark itself lands on a continuation row: the first row is red because of it.
    assert!(!gp_report::page::must_keep(&wrapped[0]));
    lines.extend(wrapped.iter().cloned());
    let plain = wrap("  Netzwerk 4/8: Kabelverbindung steht: 1000 Mbit/s, die Gegenstelle antwortet", 40);
    lines.extend(plain.iter().cloned());
    let c = report_page::colors(&lines);
    assert_eq!(c[0], crate::FG_PROMPT, "the head");
    assert_eq!(c[2], crate::FG_PROMPT, "the group's title");
    let at = 3;
    for (i, row) in wrapped.iter().enumerate() {
        assert_eq!(c[at + i], crate::FG_ERR, "row {i} of the stopped stage: {row:?}");
    }
    for i in 0..plain.len() {
        assert_eq!(c[at + wrapped.len() + i], crate::FG, "a plain row and its continuation stay normal");
    }
}

#[test]
fn a_program_taking_the_screen_ends_the_page() {
    let page = alloc::vec![alloc::vec![String::from("Seite 1 von 2")], alloc::vec![String::from("Seite 2 von 2")]];
    assert!(report_page::show(page.clone()));
    assert!(report_page::active());
    crate::vt::set_kd_mode(true, Some(4242));
    let gone = !report_page::active();
    let again = report_page::show(page);
    let turned = report_page::turn();
    crate::vt::set_kd_mode(false, None);
    assert!(gone, "the handover ended the page");
    assert!(!again, "the page does not take the screen back from the program or the shell");
    assert_eq!(turned, None, "the timer turns nothing");
}

/// VB-1: only a real keyboard's key answers the report page's question (bitsinstall's device rule).
#[test]
fn only_a_real_keyboard_answers_the_question() {
    assert!(report_page::real_key(false, crate::evdev::BUS_USB, "xhci0-4/input0", "USB HID 046d:c31c Keyboard"));
    assert!(!report_page::real_key(true, crate::evdev::BUS_USB, "xhci0-4/input0", "USB HID 046d:c31c Keyboard"), "an event MCP or a program injected");
    assert!(!report_page::real_key(false, crate::evdev::BUS_VIRTUAL, "", "ktest fake"), "a virtual device");
    assert!(!report_page::real_key(false, crate::evdev::BUS_USB, "mcp/input0", "x"), "MCP's keyboard");
    assert!(!report_page::real_key(false, crate::evdev::BUS_USB, "", "BitsOS MCP keyboard"));
}
