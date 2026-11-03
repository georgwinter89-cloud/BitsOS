//! SN-12: the codec's shutdown list (Linux `snd_hda_codec_shutdown` order).

use super::*;
use crate::caps::WidgetType;
use crate::setup::{Action, D3_WAIT_MS, shutdown};
use crate::verb::*;

fn sets(list: &[Action]) -> Vec<(u8, u32)> {
    list.iter().filter_map(|a| if let Action::Set { nid, verb } = *a { Some((nid, verb)) } else { None }).collect()
}

fn check(text: &str) {
    let (_, codec, outs) = load(text);
    let list = shutdown(&codec);
    let s = sets(&list);
    let tag0 = set(SET_CHANNEL_STREAMID, 0);
    let d3 = set(SET_POWER_STATE, PWRST_D3);
    // Every converter an output can use is taken off its stream tag and format, before any power change.
    for o in &outs {
        let cvt = o.path.last().expect("path").nid;
        assert!(s.contains(&(cvt, tag0)), "converter {cvt:#x} cleared");
        assert!(s.contains(&(cvt, set_format(0))), "converter {cvt:#x} format 0");
    }
    let last_clear = s.iter().rposition(|&(_, v)| v == tag0 || v == set_format(0)).expect("clears");
    let first_d3 = s.iter().position(|&(_, v)| v == d3).expect("d3");
    assert!(last_clear < first_d3, "streams cleaned before the codec goes to D3");
    // The function group first, every powered widget but EAPD pins, then the group's actual state waited for.
    for g in codec.audio_groups() {
        assert!(s.contains(&(g.nid, d3)));
        for w in &g.widgets {
            let want = w.caps.power_control() && !(w.kind() == WidgetType::Pin && w.pin_caps.eapd());
            assert_eq!(s.contains(&(w.nid, d3)), want, "widget {:#x}", w.nid);
        }
    }
    assert!(matches!(list.last(), Some(&Action::WaitFor { verb: GET_POWER_STATE, mask: 0xF0, value: 0x30, timeout_ms: D3_WAIT_MS, .. })));
    // Only set verbs and the one read: nothing in the list reads back for a decision (Quiesce sends it blind).
    assert!(list.iter().all(|a| !matches!(a, Action::UpdateBits { .. })));
}

#[test]
fn shutdown_cleans_streams_then_powers_down_ati() {
    check(AA01);
}

#[test]
fn shutdown_cleans_streams_then_powers_down_analog() {
    check(ALC269);
}
