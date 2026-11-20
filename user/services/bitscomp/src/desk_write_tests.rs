use super::*;

const FIELD: [i32; 4] = [10, 60, 300, 24];

fn snap(text: &str, caret: [i32; 4]) -> Snap {
    let n = text.len() as u32;
    Snap { surrounding: Some((text.into(), n, n)), caret: Some(caret) }
}

fn seen(field: Field, proof: Proof) -> Seen {
    Seen { focus_here: true, field, proof, hand: false, served: false, focused: None, selected: None }
}

fn none() -> Seen {
    seen(Field::None, Proof::Wait)
}

fn here(r: Replace) -> Seen {
    seen(Field::Enabled, Proof::Here(r))
}

fn ms(t: Instant, n: u64) -> Instant {
    t + Duration::from_millis(n)
}

fn text(t: Instant, value: &str) -> Write {
    Write::new(t, Kind::Text, Some(value.into()), false)
}

#[test]
fn proof_ii_the_text_inputs_state_must_show_the_field() {
    let url = [10, 10, 380, 24];
    assert_eq!(prove(None, &[FIELD], Some(""), &[]), Proof::Wait, "no fresh state yet");
    assert_eq!(prove(Some(&snap("", [12, 62, 1, 20])), &[FIELD], Some(""), &[]), Proof::Here(Replace::Empty));
    // The Mini-PC's case: the address bar still had the keyboard while the page's box was the target.
    assert_eq!(prove(Some(&snap("www.google.comGeorg", [200, 12, 1, 20])), &[FIELD], Some(""), &[]), Proof::Elsewhere);
    assert_eq!(prove(Some(&snap("anders", [12, 62, 1, 20])), &[FIELD], Some(""), &[]), Proof::Elsewhere, "the right place, another text");
    assert_eq!(prove(Some(&snap("", [200, 12, 1, 20])), &[FIELD], Some(""), &[]), Proof::Elsewhere, "the right text, another place");
    let only_text = Snap { surrounding: Some(("www.google.com".into(), 14, 14)), caret: None };
    assert_eq!(prove(Some(&only_text), &[url], Some("www.google.com"), &[]), Proof::Here(Replace::Delete(14, 0)), "the whole URL goes with the commit");
    let mid = Snap { surrounding: Some(("abcdef".into(), 2, 2)), caret: Some([12, 12, 1, 20]) };
    assert_eq!(prove(Some(&mid), &[url], Some("abcdef"), &[]), Proof::Here(Replace::Delete(2, 4)), "bytes before and after the cursor");
    let umlaut = Snap { surrounding: Some(("Grüße".into(), 7, 0)), caret: None };
    assert_eq!(prove(Some(&umlaut), &[], Some("Grüße"), &[]), Proof::Here(Replace::Delete(0, 0)), "all selected: the commit replaces the selection");
    let shadowed = Snap { surrounding: None, caret: Some([52, 102, 1, 20]) };
    assert_eq!(prove(Some(&shadowed), &[FIELD, [50, 100, 300, 24]], Some(""), &[]), Proof::Here(Replace::Empty), "counted from the window geometry instead of the surface");
    let caret_only = Snap { surrounding: None, caret: Some([12, 62, 1, 20]) };
    assert_eq!(prove(Some(&caret_only), &[FIELD], Some("alt"), &[]), Proof::Here(Replace::Select), "the text unknown: select it through AT-SPI");
    assert_eq!(prove(Some(&caret_only), &[FIELD], Some(""), &[]), Proof::Here(Replace::Empty));
    assert_eq!(prove(Some(&Snap::default()), &[FIELD], Some(""), &[]), Proof::Wait, "a state that says nothing proves nothing");
    let long = "x".repeat(5000);
    let window = Snap { surrounding: Some(("x".repeat(3999), 10, 10)), caret: None };
    assert_eq!(prove(Some(&window), &[], Some(&long), &[]), Proof::Here(Replace::Select), "a cut window is not the whole value");
}

#[test]
fn the_text_input_route_replaces_and_asks_atspid_once_more() {
    let t = Instant::now();
    let mut w = text(t, "www.google.com");
    assert_eq!(w.step(t, &seen(Field::Enabled, Proof::Wait)), vec![], "waits for the fresh state");
    assert_eq!(w.step(ms(t, 40), &here(Replace::Delete(14, 0))), vec![Do::Check]);
    assert_eq!(w.step(ms(t, 60), &here(Replace::Delete(14, 0))), vec![], "waits for atspid");
    assert_eq!(w.step(ms(t, 80), &Seen { focused: Some(true), ..here(Replace::Delete(14, 0)) }), vec![Do::Commit(Replace::Delete(14, 0))]);
    assert_eq!(w.step(ms(t, 300), &none()), vec![Do::End(Ok(Route::TextInput))], "no paste, no restore: the clipboard was never touched");
    assert!(w.over());
    assert_eq!(w.step(ms(t, 5000), &none()), vec![], "over is over");
}

#[test]
fn another_field_with_the_keyboard_writes_nothing() {
    let t = Instant::now();
    let mut w = text(t, "");
    assert_eq!(w.step(t, &seen(Field::Enabled, Proof::Elsewhere)), vec![], "maybe the program is still moving it");
    assert_eq!(w.step(ms(t, 1000), &seen(Field::Enabled, Proof::Elsewhere)), vec![Do::End(Err(ELSEWHERE.into()))]);
    let mut w = text(t, "");
    assert_eq!(w.step(ms(t, 1000), &seen(Field::Enabled, Proof::Wait)), vec![Do::End(Err(UNPROVEN.into()))], "no state that shows the field");
    let mut w = text(t, "");
    w.step(t, &here(Replace::Empty));
    assert_eq!(w.step(ms(t, 50), &Seen { focused: Some(false), ..here(Replace::Empty) }), vec![Do::End(Err(MOVED.into()))]);
    let mut w = text(t, "");
    w.step(t, &here(Replace::Empty));
    assert_eq!(w.step(ms(t, 50), &Seen { focused: Some(true), ..none() }), vec![Do::End(Err(MOVED.into()))], "its text input went away");
    let mut w = text(t, "");
    w.step(t, &here(Replace::Empty));
    assert_eq!(w.step(ms(t, 3100), &here(Replace::Empty)), vec![Do::End(Err(UNCHECKED.into()))]);
}

#[test]
fn a_field_that_cannot_be_emptied_is_not_appended_to() {
    let t = Instant::now();
    let mut w = text(t, "alt");
    assert_eq!(w.step(t, &here(Replace::Select)), vec![Do::Select]);
    assert_eq!(w.step(ms(t, 30), &Seen { selected: Some(false), ..here(Replace::Select) }), vec![Do::End(Err(NOT_EMPTIED.into()))]);
    let mut w = text(t, "alt");
    w.step(t, &here(Replace::Select));
    assert_eq!(w.step(ms(t, 30), &Seen { selected: Some(true), ..here(Replace::Select) }), vec![Do::Check], "selected: the commit replaces it");
    assert_eq!(w.step(ms(t, 60), &Seen { selected: Some(true), focused: Some(true), ..here(Replace::Select) }), vec![Do::Commit(Replace::Empty)]);
}

#[test]
fn without_a_text_input_the_text_is_selected_then_pasted_and_georgs_selection_comes_back() {
    let t = Instant::now();
    let mut w = text(t, "https://example.org/");
    assert_eq!(w.step(ms(t, 999), &none()), vec![]);
    assert_eq!(w.step(ms(t, 1000), &none()), vec![Do::Select], "the URL selected first, so the paste replaces it");
    let sel = Seen { selected: Some(true), ..none() };
    assert_eq!(w.step(ms(t, 1050), &sel), vec![Do::Check], "then: is the field still the one with the keyboard?");
    let still = Seen { focused: Some(true), ..sel };
    assert_eq!(w.step(ms(t, 1150), &still), vec![Do::Offer, Do::PasteKey], "yes: the paste for that window, one key");
    assert_eq!(w.step(ms(t, 1200), &still), vec![]);
    let served = Seen { served: true, ..still };
    assert_eq!(w.step(ms(t, 1300), &served), vec![Do::Restore], "the program read it: Georg's selection back at once");
    assert_eq!(w.step(ms(t, 1500), &served), vec![Do::End(Ok(Route::Paste))]);
    let mut e = text(t, "");
    assert_eq!(e.step(ms(t, 1000), &none()), vec![Do::Check], "an empty field needs no selection");
}

#[test]
fn a_paste_the_program_never_asks_for_ends_and_gives_the_selection_back() {
    let t = Instant::now();
    let mut w = text(t, "");
    w.step(ms(t, 1000), &none());
    let still = Seen { focused: Some(true), ..none() };
    w.step(ms(t, 1100), &still);
    assert_eq!(w.step(ms(t, 3100), &still), vec![Do::Restore, Do::End(Err(TIMEOUT.into()))]);
    let mut u = text(t, "");
    u.step(ms(t, 1000), &none());
    assert_eq!(u.step(ms(t, 4000), &none()), vec![Do::End(Err(UNCHECKED.into()))]);
}

#[test]
fn enter_goes_only_into_the_proven_field() {
    let t = Instant::now();
    let mut w = Write::new(t, Kind::Enter, Some("Georg".into()), false);
    assert_eq!(w.step(t, &here(Replace::Delete(5, 0))), vec![Do::Check], "nothing is removed for Enter");
    assert_eq!(w.step(ms(t, 30), &Seen { focused: Some(true), ..here(Replace::Delete(5, 0)) }), vec![Do::EnterKey]);
    assert_eq!(w.step(ms(t, 300), &none()), vec![Do::End(Ok(Route::Enter))]);
    let mut w = Write::new(t, Kind::Enter, None, false);
    assert_eq!(w.step(ms(t, 1000), &seen(Field::Enabled, Proof::Elsewhere)), vec![Do::End(Err(ELSEWHERE.into()))]);
    let mut w = Write::new(t, Kind::Enter, None, false);
    assert_eq!(w.step(ms(t, 1000), &none()), vec![Do::Check], "a program without a text input: atspid's proof alone, asked again");
    assert_eq!(w.step(ms(t, 1010), &Seen { focus_here: false, ..none() }), vec![Do::End(Err(NO_KEY.into()))]);
}

#[test]
fn georgs_own_hand_stops_it_before_anything_is_written() {
    let t = Instant::now();
    let mut w = text(t, "");
    let hand = Seen { hand: true, ..here(Replace::Empty) };
    assert_eq!(w.step(ms(t, 10), &hand), vec![Do::End(Err(HAND.into()))], "even with the text input ready: nothing committed");
    let mut p = text(t, "");
    p.step(ms(t, 1000), &none());
    p.step(ms(t, 1050), &Seen { focused: Some(true), ..none() });
    assert_eq!(p.step(ms(t, 1100), &Seen { hand: true, focused: Some(true), ..none() }), vec![Do::Restore, Do::End(Err(HAND.into()))], "a paste offered: taken back");
    let mut done = text(t, "");
    done.step(t, &here(Replace::Empty));
    done.step(ms(t, 10), &Seen { focused: Some(true), ..here(Replace::Empty) });
    assert_eq!(done.step(ms(t, 20), &Seen { hand: true, ..here(Replace::Empty) }), vec![Do::End(Ok(Route::TextInput))], "after the text is in it is done");
}

#[test]
fn the_keyboard_on_another_window_or_a_secret_field_stops_it() {
    let t = Instant::now();
    let mut w = text(t, "");
    assert_eq!(w.step(ms(t, 100), &Seen { focus_here: false, ..here(Replace::Empty) }), vec![Do::End(Err(MOVED.into()))]);
    let mut p = text(t, "");
    p.step(ms(t, 1000), &none());
    p.step(ms(t, 1050), &Seen { focused: Some(true), ..none() });
    assert_eq!(p.step(ms(t, 1100), &Seen { focus_here: false, focused: Some(true), ..none() }), vec![Do::Restore, Do::End(Err(MOVED.into()))]);
    let mut s = text(t, "");
    assert_eq!(s.step(t, &seen(Field::Secret, Proof::Wait)), vec![Do::End(Err(SECRET.into()))], "a password field by its text input");
}

#[test]
fn a_stop_from_outside_takes_the_paste_back_and_a_served_paste_is_written() {
    let t = Instant::now();
    let mut w = text(t, "");
    assert_eq!(w.stop("x", false), vec![Do::End(Err("x".into()))]);
    assert_eq!(w.stop("y", false), vec![], "once");
    let pasting = || {
        let mut p = text(t, "");
        p.step(ms(t, 1000), &none());
        p.step(ms(t, 1050), &Seen { focused: Some(true), ..none() });
        p
    };
    let mut p = pasting();
    assert_eq!(p.stop("x", false), vec![Do::Restore, Do::End(Err("x".into()))]);
    let served = Seen { served: true, focused: Some(true), ..none() };
    let mut p = pasting();
    assert_eq!(p.step(ms(t, 1060), &Seen { hand: true, ..served }), vec![Do::Restore], "Georg's move right after the program read it: written");
    assert_eq!(p.step(ms(t, 1070), &Seen { hand: true, ..served }), vec![Do::End(Ok(Route::Paste))]);
    let mut p = pasting();
    assert!(p.written(true) && !p.written(false));
    assert_eq!(p.stop(HAND_STOP, true), vec![Do::Restore, Do::End(Ok(Route::Paste))], "Esc after the program read it: not \"nothing written\"");
}

#[test]
fn b_the_text_must_be_in_the_field_afterwards() {
    assert_eq!(landed(Some("Georg"), "Georg", &[], None), Landed::Arrived);
    assert_eq!(landed(None, "Georg", &[], None), Landed::Unchecked);
    let others = vec![("Adress- und Suchleiste".to_string(), Some("www.google.com".to_string()), Some("www.google.comGeorg".to_string()))];
    assert_eq!(landed(Some(""), "Georg", &others, None), Landed::Missing(Some("Adress- und Suchleiste".into())), "the Mini-PC's case: it landed in the bar");
    assert_eq!(landed(Some("altGeorg"), "Georg", &[], None), Landed::Missing(None), "appended is not replaced");
    let unrelated = vec![("Kommentar".to_string(), Some("a".to_string()), Some("b".to_string()))];
    assert_eq!(landed(Some(""), "Georg", &unrelated, None), Landed::Missing(None));
    assert!(missing("Suche", Some("Adress- und Suchleiste")).contains("nicht in „Suche“ angekommen; er steht jetzt in „Adress- und Suchleiste“"));
}

#[test]
fn terminals_are_known() {
    assert!(terminal("foot") && terminal("org.gnome.Console") && terminal("Alacritty"));
    assert!(!terminal("brave-browser") && !terminal("libreoffice-writer"));
}

#[test]
fn a_text_alone_proves_the_field_only_when_no_other_could_hold_it() {
    let empty = Snap { surrounding: Some((String::new(), 0, 0)), caret: None };
    // The new tab: the empty address bar and the page's empty box give '' == '' both.
    assert_eq!(prove(Some(&empty), &[FIELD], Some(""), &[]), Proof::Wait, "empty is no field's sign");
    assert_eq!(prove(Some(&empty), &[FIELD], Some(""), &[""]), Proof::Wait);
    assert_eq!(prove(Some(&snap("", [200, 12, 1, 20])), &[FIELD], Some(""), &[]), Proof::Elsewhere, "the caret then says it");
    assert_eq!(prove(Some(&snap("", [12, 62, 1, 20])), &[FIELD], Some(""), &[]), Proof::Here(Replace::Empty));
    let same = Snap { surrounding: Some(("Georg".into(), 5, 5)), caret: None };
    assert_eq!(prove(Some(&same), &[FIELD], Some("Georg"), &["Georg"]), Proof::Wait, "another field holds the same");
    assert_eq!(prove(Some(&same), &[FIELD], Some("Georg"), &["anders"]), Proof::Here(Replace::Delete(5, 0)));
    let long = "x".repeat(5000);
    let nothing = Snap { surrounding: Some((String::new(), 0, 0)), caret: None };
    assert_eq!(prove(Some(&nothing), &[FIELD], Some(&long), &[]), Proof::Wait, "an empty window is in every long value");
    let few = Snap { surrounding: Some(("x".repeat(10), 0, 0)), caret: None };
    assert_eq!(prove(Some(&few), &[FIELD], Some(&long), &[]), Proof::Wait, "a few letters match anywhere");
}

#[test]
fn a_program_that_spoke_text_input_must_show_the_field_no_paste_instead() {
    let t = Instant::now();
    let mut w = Write::new(t, Kind::Text, Some(String::new()), true);
    assert_eq!(w.step(ms(t, 1000), &none()), vec![Do::End(Err(UNPROVEN.into()))], "no fresh enable after leave and enter");
    let mut w = Write::new(t, Kind::Enter, None, true);
    assert_eq!(w.step(ms(t, 1000), &none()), vec![Do::End(Err(UNPROVEN.into()))], "no bare Enter either");
    let mut w = Write::new(t, Kind::Text, Some(String::new()), false);
    assert_eq!(w.step(ms(t, 1000), &none()), vec![Do::Check], "a program without one: the paste route");
}

#[test]
fn the_proof_must_still_hold_when_the_text_goes_out() {
    let t = Instant::now();
    let mut w = text(t, "www.google.com");
    assert_eq!(w.step(t, &here(Replace::Delete(14, 0))), vec![Do::Check]);
    // The page moved the keyboard while atspid was asked; atspid still says yes.
    assert_eq!(w.step(ms(t, 40), &Seen { focused: Some(true), ..seen(Field::Enabled, Proof::Elsewhere) }), vec![Do::End(Err(ELSEWHERE.into()))]);
    let mut w = text(t, "www.google.com");
    w.step(t, &here(Replace::Delete(14, 0)));
    assert_eq!(w.step(ms(t, 40), &Seen { focused: Some(true), ..here(Replace::Delete(3, 0)) }), vec![Do::End(Err(MOVED.into()))], "other bytes: not the field planned");
    let mut w = text(t, "www.google.com");
    w.step(t, &here(Replace::Delete(14, 0)));
    assert_eq!(w.step(ms(t, 40), &Seen { focused: Some(true), ..seen(Field::Enabled, Proof::Wait) }), vec![Do::End(Err(MOVED.into()))], "a fresh enable: not known any more");
    let mut w = Write::new(t, Kind::Enter, None, true);
    w.step(t, &here(Replace::Empty));
    assert_eq!(w.step(ms(t, 40), &Seen { focused: Some(true), ..seen(Field::Enabled, Proof::Elsewhere) }), vec![Do::End(Err(ELSEWHERE.into()))], "no Enter into the other field");
}

#[test]
fn an_address_bar_that_completes_the_text_inline_has_it() {
    let v = "www.google.com/maps";
    let sel = |c: u32, a: u32| (v.to_string(), c, a);
    assert_eq!(landed(Some(v), "www.google.com", &[], Some(&sel(19, 14))), Landed::Arrived, "the completion selected after the text");
    assert_eq!(landed(Some(v), "www.google.com", &[], Some(&sel(14, 19))), Landed::Arrived);
    assert_eq!(landed(Some(v), "www.google.com", &[], None), Landed::Missing(None), "without the program's word: not shown");
    assert_eq!(landed(Some(v), "www.google.com", &[], Some(&sel(19, 19))), Landed::Missing(None), "nothing selected: appended to");
    assert_eq!(landed(Some("altwww.google.com"), "www.google.com", &[], Some(&("altwww.google.com".into(), 17, 3))), Landed::Missing(None));
}
