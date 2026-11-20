//! The report page's sound line (GP-SOUND, checker finding): named from the output the mixer plays or the target
//! leads to, the same way the mixer resolves it, on the Mini-PC (HDMI/DP and a JBL) and the Gaming-PC (no hda).

use crate::backend::{Endpoint, Port, Target};
use crate::tone::{HdaCount, sound_line};
use crate::usbback::{pair_resolve, usb_resolve};
use alloc::string::String;
use alloc::vec::Vec;
use usb_audio::OutputInfo;

fn jbl() -> OutputInfo {
    OutputInfo { key: String::from("usb:xhci0-1"), id: 4, control: 0, iface: 2, channels: 2, terminal_type: 0x0402, board: false }
}

fn board_speaker() -> OutputInfo {
    OutputInfo { key: String::from("usb:xhci1-9"), id: 5, control: 0, iface: 3, channels: 2, terminal_type: 0x0301, board: true }
}

fn hda_ep(port: Port) -> Endpoint {
    Endpoint { dev: String::from("hda0"), port, cad: 0, pin: 3 }
}

/// The Mini-PC's hda: an HDMI/DP output only (`HdaBackend` falls back to `auto`, which then is HDMI).
fn mini_hda(_: &Target) -> Option<Endpoint> {
    Some(hda_ep(Port::Hdmi))
}

fn line(t: &Target, playing: Option<&str>, usb: &[OutputInfo], hda: &dyn Fn(&Target) -> Option<Endpoint>, count: HdaCount) -> String {
    let would = pair_resolve(t, &mut |x| usb_resolve(usb, x), &mut |x| hda(x));
    sound_line(t, playing, would.as_ref(), usb, count, false, &[], true)
}

const MINI: HdaCount = HdaCount { jacks: 0, hdmi: 1 };

#[test]
fn the_hdmi_target_with_a_headset_names_the_monitor() {
    let usb = [jbl()];
    let l = line(&Target::Prefer(Port::Hdmi), None, &usb, &mini_hda, MINI);
    assert!(l.starts_with("Ton: still, ginge auf den Bildschirm (HDMI/DP) (HDMI); USB-Ausgänge: 1 = USB-Headset (usb:xhci0-1/usb/0:0x2); \
        Buchsen am Rechner: 0; Bildschirm-Ausgänge: 1"), "{l}");
    let l = line(&Target::Prefer(Port::Hdmi), Some("hda0/hdmi/0:0x3"), &usb, &mini_hda, MINI);
    assert!(l.starts_with("Ton: spielt auf den Bildschirm (HDMI/DP) (HDMI);"), "{l}");
}

#[test]
fn auto_without_a_usb_output_names_what_hda_plays_on() {
    let l = line(&Target::Auto, None, &[], &mini_hda, MINI);
    assert!(l.starts_with("Ton: still, ginge auf den Bildschirm (HDMI/DP) (automatisch); USB-Ausgänge: keine; Buchsen am Rechner: 0; \
        Bildschirm-Ausgänge: 1"), "{l}");
    let jack = |_: &Target| Some(hda_ep(Port::Jack));
    let l = line(&Target::Auto, None, &[], &jack, HdaCount { jacks: 1, hdmi: 0 });
    assert!(l.starts_with("Ton: still, ginge auf die Buchse am Rechner (automatisch);"), "{l}");
}

#[test]
fn the_gaming_pc_plays_on_the_headset_and_jack_means_the_board() {
    let usb = [jbl(), board_speaker()];
    let none = |_: &Target| None;
    let l = line(&Target::Auto, Some("usb:xhci0-1/usb/0:0x2"), &usb, &none, HdaCount::default());
    assert_eq!(l, "Ton: spielt auf USB-Headset (automatisch); USB-Ausgänge: 1 = USB-Headset (usb:xhci0-1/usb/0:0x2), \
        2 = Lautsprecher-Buchse (usb:xhci1-9/usb/0:0x3); Buchsen am Rechner: 0; Bildschirm-Ausgänge: 0; Probe: snd ton");
    let l = line(&Target::Prefer(Port::Jack), None, &usb, &none, HdaCount::default());
    assert!(l.starts_with("Ton: still, ginge auf Lautsprecher-Buchse (Buchse);"), "{l}");
    let l = line(&Target::Auto, None, &[], &none, HdaCount::default());
    assert!(l.starts_with("Ton: still, kein Ausgang da (automatisch);"), "{l}");
}

#[test]
fn mute_refusals_and_the_mini_pc_rule_show() {
    let refused: Vec<String> = alloc::vec![String::from("S/PDIF: Takt 8 meldet ungültig (bei 48000 Hz)")];
    let l = sound_line(&Target::Auto, None, None, &[], HdaCount::default(), true, &refused, false);
    assert_eq!(l, "Ton: still, kein Ausgang da (automatisch); USB-Ausgänge: keine; Buchsen am Rechner: 0; Bildschirm-Ausgänge: 0; \
        Hauptschalter stumm; nicht bereit: S/PDIF: Takt 8 meldet ungültig (bei 48000 Hz)");
}
