//! GP-GPU1: the device registry: keys by PCI address, the stable order, the tools' `device` argument.

use crate::attach::Gop;
use crate::devs::{Family, Key, Registry, Seen, family_of, order};
use rdna4_core::card::{CARD_A, CARD_B};
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

pub fn key_a() -> Key {
    Key::new(0, CARD_A.at.bus, CARD_A.at.device, CARD_A.at.function)
}
pub fn key_b() -> Key {
    Key::new(0, CARD_B.at.bus, CARD_B.at.device, CARD_B.at.function)
}
/// The Mini-PC's 780M (c7:00.0), BAR0 256 MiB at 0xD0_0000_0000 here.
pub const KEY_780M: Key = Key::new(0, 0xc7, 0, 0);

fn card(c: &rdna4_core::card::Card) -> Seen {
    Seen { key: Key::new(0, c.at.bus, c.at.device, c.at.function), family: Family::Rdna4, bar0: Some((c.bar0_base, c.bar0_bytes)) }
}
fn apu() -> Seen {
    Seen { key: KEY_780M, family: Family::Phoenix, bar0: Some((0xD0_0000_0000, 256 << 20)) }
}
fn keys(r: &Registry) -> Vec<(usize, String, bool)> {
    r.all().iter().map(|d| (d.index, format!("{}", d.key), d.display)).collect()
}

#[test]
fn a_key_is_the_pci_address() {
    assert_eq!(format!("{}", key_a()), "0000:04:00.0");
    assert_eq!(Key::parse("0000:04:00.0"), Some(key_a()));
    assert_eq!(Key::parse("07:00.0"), Some(key_b()));
    assert_eq!(Key::parse(" c7:00.0 "), Some(KEY_780M));
    assert_eq!(Key::parse("0001:c7:1f.7"), Some(Key::new(1, 0xc7, 0x1f, 7)));
    for bad in ["", "04", "04:00", "4:00.0", "04:0.0", "04:20.0", "04:00.8", "zz:00.0", "0000:0000:04:00.0", "card0"] {
        assert_eq!(Key::parse(bad), None, "{bad:?}");
    }
    // The two cards share everything but this.
    assert!(key_a() != key_b() && key_a() < key_b());
}

#[test]
fn families_are_keyed_by_id() {
    assert_eq!(family_of(0x1002, 0x7550), Some(Family::Rdna4));
    // Every other AMD display function is tried as the 780M's driver always tried it (its read gate decides).
    assert_eq!(family_of(0x1002, 0x1900), Some(Family::Phoenix));
    assert_eq!(family_of(0x1002, 0x15BF), Some(Family::Phoenix));
    assert_eq!(family_of(0x1234, 0x1111), None, "QEMU's VGA is nobody's");
}

#[test]
fn the_boot_display_comes_first_then_the_address() {
    let gop_in = |base: u64| Some(Gop { phys: base + (1 << 20), bytes: 2560 * 1440 * 4 });
    // The Gaming-PC: the card with the picture is card0 whichever address it has.
    let mut s = vec![card(&CARD_B), card(&CARD_A)];
    assert_eq!(order(&mut s, gop_in(CARD_A.bar0_base)), Some(key_a()));
    assert_eq!(s.iter().map(|x| x.key).collect::<Vec<_>>(), [key_a(), key_b()]);
    let mut s = vec![card(&CARD_A), card(&CARD_B)];
    assert_eq!(order(&mut s, gop_in(CARD_B.bar0_base)), Some(key_b()));
    assert_eq!(s.iter().map(|x| x.key).collect::<Vec<_>>(), [key_b(), key_a()]);
    // No framebuffer (headless), one outside every BAR0, one only partly inside, an empty one: the address alone.
    let end_of_a = CARD_A.bar0_base + CARD_A.bar0_bytes;
    for gop in [None, Some(Gop { phys: 0x8000_0000, bytes: 1 << 20 }), Some(Gop { phys: end_of_a - 4096, bytes: 8192 }), Some(Gop { phys: CARD_A.bar0_base, bytes: 0 })] {
        let mut s = vec![card(&CARD_B), card(&CARD_A)];
        assert_eq!(order(&mut s, gop), None, "{gop:?}");
        assert_eq!(s.iter().map(|x| x.key).collect::<Vec<_>>(), [key_a(), key_b()]);
    }
    // Two BAR0s that both hold it: nobody is the display (a guess here is the mistake the rule prevents).
    let twin = Seen { key: Key::new(0, 0x09, 0, 0), ..card(&CARD_A) };
    let mut s = vec![twin, card(&CARD_A)];
    assert_eq!(order(&mut s, gop_in(CARD_A.bar0_base)), None);
    // The Mini-PC: one device, index 0, with or without the picture in its BAR0.
    let mut s = vec![apu()];
    assert_eq!(order(&mut s, gop_in(0xD0_0000_0000)), Some(KEY_780M));
    // Later, a card beside the 780M (OCuLink): the 780M shows the picture and stays card0 although its address is higher.
    let mut s = vec![card(&CARD_A), apu()];
    assert_eq!(order(&mut s, gop_in(0xD0_0000_0000)), Some(KEY_780M));
    assert_eq!(s.iter().map(|x| x.key).collect::<Vec<_>>(), [KEY_780M, key_a()]);
}

#[test]
fn the_registry_names_and_picks_devices() {
    let r = Registry::build(vec![(card(&CARD_B), None), (card(&CARD_A), None)], Some(Gop { phys: CARD_A.bar0_base, bytes: 4096 }));
    assert_eq!(keys(&r), [(0, String::from("0000:04:00.0"), true), (1, String::from("0000:07:00.0"), false)]);
    assert_eq!(r.display().map(|d| d.key), Some(key_a()));
    assert_eq!(r.first(Family::Rdna4).map(|d| d.key), Some(key_a()));
    assert!(r.first(Family::Phoenix).is_none());
    assert_eq!((r.all()[0].card_name(), r.all()[0].render_minor()), (String::from("card0"), 128));
    assert_eq!((r.all()[1].card_name(), r.all()[1].render_minor()), (String::from("card1"), 129));
    // No argument is the first device, so every tool call of before means what it meant.
    assert_eq!(r.pick(None).map(|d| d.key), Ok(key_a()));
    assert_eq!(r.pick(Some("")).map(|d| d.key), Ok(key_a()));
    for (arg, want) in [("card1", key_b()), ("1", key_b()), ("card0", key_a()), ("07:00.0", key_b()), ("0000:04:00.0", key_a())] {
        assert_eq!(r.pick(Some(arg)).map(|d| d.key), Ok(want), "{arg}");
    }
    let e = r.pick(Some("card2")).map(|d| d.key).unwrap_err();
    assert_eq!(e, "no GPU device 'card2': there is card0 0000:04:00.0, card1 0000:07:00.0");
    assert!(r.pick(Some("05:00.0")).is_err());
    let none = Registry::build(Vec::new(), None);
    assert_eq!(none.pick(None).map(|d| d.key), Err(String::from("no AMD GPU function on this machine")));
    assert_eq!(none.pick(Some("card0")).map(|d| d.key).unwrap_err(), "no GPU device 'card0': this machine has none");
}

/// The two-device ktest (`gx.devs.side_by_side`) on the host: a job on the 780M's fake passes while the card's fake,
/// attached read-only beside it, sees no write.
#[test]
fn two_devices_side_by_side() {
    crate::ktests_devs::side_by_side().unwrap();
}
