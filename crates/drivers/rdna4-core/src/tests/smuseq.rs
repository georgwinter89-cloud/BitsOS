//! Wave 43: Linux's start of the power controller as a sequence (`smuseq::Script`) and the gate that opens the
//! SMU's mailbox for exactly that sequence (`smugate::Gate`). The answers are the ones the real card gave Linux
//! (plan/hw/gaming-pc-gpu-2026-11-18, init.txt lines 893-1883).

use crate::pptable14::{self, PcieLevels, Pptable};
use crate::smu14::{self, msg};
use crate::smuseq::{Script, Step, Stopped, Tables};
use std::vec::Vec;

pub const TABLES: Tables = Tables { driver_mc: 0x80_0531_A000, tools_mc: 0x80_0530_1000 };
/// The Gaming-PC's link as Linux saw it: 32 GT/s x16 all the way.
pub const CAPS: (u8, u8) = (4, 6);
pub const FEATURES: u64 = 0x048C_F19E_38FF_FCFB;
/// (PPCLK id, fine-grained, MHz) as the card answered.
const LEVELS: [(u8, bool, &[u32]); 7] =
    [(1, true, &[417, 1476]), (0, true, &[500, 3450]), (2, false, &[96, 456, 772, 875, 1124, 1258]), (3, true, &[313, 2400]), (5, true, &[800, 2934]), (4, true, &[800, 2200]), (9, true, &[147, 1800])];

pub fn table() -> Pptable {
    Pptable {
        structuresize: pptable14::TABLE_BYTES as u16,
        format_revision: 23,
        content_revision: 0,
        own: Pptable::COMPILED,
        platform_caps: 0x18,
        thermal_controller_type: 0,
        software_shutdown_temp: 118,
        features_to_run: [FEATURES as u32, (FEATURES >> 32) as u32],
        pcie: PcieLevels { gen_speed: [0, 2, 4], lane_count: [6, 6, 6], lclk_freq: [310, 619, 619] },
        game_clock_ac: 2400,
        temperature_limit: [100, 110, 108],
        fan_abnormal_temp_limit_offset: 0,
        socket_power_limit_ac: 304,
        msg_limit_power_ac: 374,
    }
}

pub fn table_bytes(t: &Pptable) -> Vec<u8> {
    let mut b = std::vec![0u8; smu14::DRIVER_TABLE_BYTES as usize];
    pptable14::synthetic(t, &mut b);
    b
}

/// What the card answered a question in the argument register.
pub fn value(m: u16, arg: u32, features: u64) -> u32 {
    match m {
        msg::GET_DRIVER_IF_VERSION => 0x33,
        msg::GET_SMU_VERSION => 0x0068_5000,
        msg::GET_RUNNING_SMU_FEATURES_HIGH => (features >> 32) as u32,
        msg::GET_RUNNING_SMU_FEATURES_LOW => features as u32,
        msg::GET_DPM_FREQ_BY_INDEX => {
            let (_, fine, mhz) = LEVELS.iter().find(|l| u32::from(l.0) == arg >> 16).unwrap();
            let flag = if *fine { 0x8000_0000 } else { 0 };
            if arg & 0xFFFF == 0xFF { flag | mhz.len() as u32 } else { flag | mhz[(arg & 0xFFFF) as usize] }
        }
        _ => 0,
    }
}

/// The script run to its end on the card's answers: every message with its argument, in order.
pub fn play(features: u64, t: &Pptable) -> (Script, Vec<(u16, u32)>) {
    let mut s = Script::new(TABLES, CAPS);
    let mut sent = Vec::new();
    loop {
        match s.next() {
            Step::End => return (s, sent),
            Step::Table => s.table(&table_bytes(t)).unwrap(),
            Step::Thermal => s.thermal_done(),
            Step::Send { msg: m, arg, value: wants } => {
                sent.push((m, arg));
                s.sent();
                if s.answered(wants.then(|| value(m, arg, features))).is_err() {
                    return (s, sent);
                }
            }
        }
    }
}

/// The messages of the trace that are no feature questions, in the trace's order (init.txt line, message, argument).
pub const TRACE: [(u32, u16, u32); 49] = [
    (895, 0x03, 0), (903, 0x02, 0), (911, 0x0E, 0x83), (918, 0x0F, 0xDAAF_8000), (925, 0x10, 0x83), (932, 0x11, 0xDAAC_0000), (939, 0x12, 1), (946, 0x36, 0),
    (953, 0x20, 0x406), (960, 0x20, 0x1_0406), (967, 0x20, 0x2_0406), (974, 0x06, 0), (1029, 0x2A, 0), (1036, 0x2C, 0),
    (1075, 0x1F, 0x1_00FF), (1099, 0x1F, 0x1_00FF), (1123, 0x1F, 0x1_0000), (1147, 0x1F, 0x1_0001),
    (1187, 0x1F, 0xFF), (1211, 0x1F, 0xFF), (1235, 0x1F, 0), (1259, 0x1F, 1),
    (1299, 0x1F, 0x2_00FF), (1323, 0x1F, 0x2_00FF), (1347, 0x1F, 0x2_0000), (1371, 0x1F, 0x2_0001), (1395, 0x1F, 0x2_0002), (1419, 0x1F, 0x2_0003), (1443, 0x1F, 0x2_0004), (1467, 0x1F, 0x2_0005),
    (1507, 0x1F, 0x3_00FF), (1531, 0x1F, 0x3_00FF), (1555, 0x1F, 0x3_0000), (1579, 0x1F, 0x3_0001),
    (1619, 0x1F, 0x5_00FF), (1643, 0x1F, 0x5_00FF), (1667, 0x1F, 0x5_0000), (1691, 0x1F, 0x5_0001),
    (1731, 0x1F, 0x4_00FF), (1755, 0x1F, 0x4_00FF), (1779, 0x1F, 0x4_0000), (1803, 0x1F, 0x4_0001),
    (1827, 0x1F, 0x9_00FF), (1835, 0x1F, 0x9_00FF), (1843, 0x1F, 0x9_0000), (1851, 0x1F, 0x9_0001),
    (1859, 0x2D, 0), (1866, 0x2B, 0), (1880, 0x4D, 0),
];

#[test]
fn the_script_on_the_cards_answers_is_the_traces_125_messages() {
    let (s, sent) = play(FEATURES, &table());
    assert!(s.passed() && s.stopped().is_none());
    assert_eq!(sent.len(), 125);
    // Everything but the feature questions, in the trace's order; the tables' addresses are BitsOS's own.
    let plain: Vec<(u16, u32)> = sent.iter().copied().filter(|(m, _)| !matches!(*m, 0x0C | 0x0D)).collect();
    let want: Vec<(u16, u32)> = TRACE
        .iter()
        .map(|(_, m, a)| match m {
            0x0E => (*m, (TABLES.driver_mc >> 32) as u32),
            0x0F => (*m, TABLES.driver_mc as u32),
            0x10 => (*m, (TABLES.tools_mc >> 32) as u32),
            0x11 => (*m, TABLES.tools_mc as u32),
            _ => (*m, *a),
        })
        .collect();
    assert_eq!(plain, want);
    // 38 feature questions, each the high half and then the low one (smu_cmn.c:832-840), argument 0.
    let pairs: Vec<(u16, u32)> = sent.iter().copied().filter(|(m, _)| matches!(*m, 0x0C | 0x0D)).collect();
    assert_eq!(pairs.len(), 76);
    assert!(pairs.chunks(2).all(|p| p == [(0x0D, 0), (0x0C, 0)]));
    // Where they stand: three after EnableAllSmuFeatures, one before each clock, one before every clock question
    // but DCEFCLK's four (smu_cmn.c:802-803).
    let at = |i: usize| sent[i].0;
    assert_eq!((at(11), at(12), at(18), at(19), at(20)), (0x06, 0x0D, 0x2A, 0x2C, 0x0D));
    assert_eq!(sent[116..122].iter().map(|x| x.0).collect::<Vec<_>>(), [0x0D, 0x0C, 0x1F, 0x1F, 0x1F, 0x1F]);
    // What the start learned.
    let f = &s.facts;
    assert_eq!((f.fw_if, f.version, f.supported, f.dpm_running), (Some(0x33), Some(0x0068_5000), Some(FEATURES), Some(true)));
    assert!(f.enable_sent && f.enabled && !f.vcn_up && !f.jpeg_up && f.read_back.is_none());
    let levels: Vec<(&str, u8, bool, Vec<u32>)> = f.dpm.iter().map(|t| (t.clock, t.ppclk, t.fine_grained, t.mhz.clone())).collect();
    assert_eq!(levels[0], ("SOCCLK", 1, true, std::vec![417, 1476]));
    // GFXCLK's top level is capped to the table's GameClockAc (smu_v14_0_2_ppt.c:502-509): 3450 becomes 2400.
    assert_eq!(levels[1], ("GFXCLK", 0, true, std::vec![500, 2400]));
    assert_eq!(levels[2], ("UCLK", 2, false, std::vec![96, 456, 772, 875, 1124, 1258]));
    assert_eq!(levels[3..].iter().map(|l| (l.0, l.3.clone())).collect::<Vec<_>>(), [("FCLK", std::vec![313, 2400]), ("VCLK", std::vec![800, 2934]), ("DCLK", std::vec![800, 2200]), ("DCEFCLK", std::vec![147, 1800])]);
    assert!(f.dpm.iter().all(|t| t.from_pmfw));
    let th = f.thermal.unwrap();
    assert_eq!((th.min, th.max, th.edge_emergency_max, th.hotspot_crit_max, th.mem_emergency_max, th.software_shutdown_temp), (-273_150, 100_000, 105_000, 110_000, 113_000, 118));
}

#[test]
fn a_clock_whose_feature_does_not_run_is_not_asked_about() {
    // UCLK's DPM off (bit 3): Linux keeps the VBIOS's boot value and asks the PMFW nothing about UCLK.
    let (s, sent) = play(FEATURES & !(1 << 3), &table());
    assert!(s.passed());
    assert!(!sent.iter().any(|(m, a)| *m == 0x1F && a >> 16 == 2), "{sent:x?}");
    let uclk = s.facts.dpm.iter().find(|t| t.clock == "UCLK").unwrap();
    assert!(!uclk.from_pmfw && uclk.mhz.is_empty());
    assert_eq!((s.facts.dpm.len(), s.facts.dpm_running), (7, Some(false)));
    // 125 less UCLK's eight questions, each with its feature pair.
    assert_eq!(sent.len(), 125 - 8 * 3);
}

#[test]
fn a_table_without_link_levels_sends_no_override() {
    let t = Pptable { pcie: PcieLevels::default(), ..table() };
    let (s, sent) = play(FEATURES, &t);
    assert!(s.passed() && !sent.iter().any(|(m, _)| *m == 0x20));
    assert_eq!(sent[7..9], [(0x36, 0), (0x06, 0)]);
}

/// Drives `s` with the card's answers until the next step is `stop_at` (a message with this argument).
fn until(s: &mut Script, stop_at: (u16, u32)) {
    loop {
        match s.next() {
            Step::Send { msg: m, arg, .. } if (m, arg) == stop_at => return,
            Step::Send { msg: m, arg, value: wants } => {
                s.sent();
                s.answered(wants.then(|| value(m, arg, FEATURES))).unwrap();
            }
            Step::Table => s.table(&table_bytes(&table())).unwrap(),
            Step::Thermal => s.thermal_done(),
            Step::End => panic!("the script ended before {stop_at:x?}"),
        }
    }
}

fn rest(s: &mut Script) -> Vec<u16> {
    let mut out = Vec::new();
    while let Step::Send { msg: m, arg, value: wants } = s.next() {
        out.push(m);
        s.sent();
        s.answered(wants.then(|| value(m, arg, FEATURES))).unwrap();
    }
    out
}

#[test]
fn a_stop_leaves_what_linuxs_err_out_does_and_the_read_back() {
    let fresh = || Script::new(TABLES, CAPS);
    // Before EnableAllSmuFeatures went out: nothing.
    let mut s = fresh();
    until(&mut s, (0x36, 0));
    s.sent();
    s.failed(0xFF);
    assert_eq!((s.stopped(), s.next(), s.passed()), (Some(Stopped::Answer { msg: 0x36, arg: 0, answer: 0xFF }), Step::End, false));
    // EnableAllSmuFeatures answered with an error: what runs is read back, once.
    let mut s = fresh();
    until(&mut s, (0x06, 0));
    s.sent();
    s.failed(0xFD);
    assert!(s.facts.enable_sent && !s.facts.enabled);
    assert_eq!(rest(&mut s), [0x0D, 0x0C]);
    assert_eq!((s.facts.read_back, s.next()), (Some(FEATURES), Step::End));
    // No answer to it within Linux's bound, or a code Linux takes for a hang: nothing more is sent.
    for answer in [0, 0xFB, u32::MAX] {
        let mut s = fresh();
        until(&mut s, (0x06, 0));
        s.sent();
        s.failed(answer);
        assert_eq!((s.next(), s.facts.read_back), (Step::End, None), "{answer:#x}");
    }
    // A clock question fails while the video block is up: JPEG down, VCN down (amdgpu_smu.c:915-921), read-back.
    let mut s = fresh();
    until(&mut s, (0x1F, 0x2_0003));
    s.sent();
    s.failed(0xFF);
    assert!(s.facts.vcn_up && s.facts.jpeg_up);
    assert_eq!(rest(&mut s), [0x2D, 0x2B, 0x0D, 0x0C]);
    assert!(!s.facts.vcn_up && !s.facts.jpeg_up && s.facts.read_back == Some(FEATURES));
    // PowerUpJpeg fails: Linux's err_out powers VCN down again.
    let mut s = fresh();
    until(&mut s, (0x2C, 0));
    s.sent();
    s.failed(0xFF);
    assert_eq!(rest(&mut s), [0x2B, 0x0D, 0x0C]);
    // PowerDownJpeg fails: it is not tried a second time, and VCN is powered down all the same (Linux drops
    // PowerDownJpeg's result and falls into err_out, amdgpu_smu.c:912-921).
    let mut s = fresh();
    until(&mut s, (0x2D, 0));
    s.sent();
    s.failed(0xFF);
    assert_eq!(s.stopped(), Some(Stopped::Answer { msg: 0x2D, arg: 0, answer: 0xFF }));
    assert_eq!(rest(&mut s), [0x2B, 0x0D, 0x0C]);
    assert!(s.facts.jpeg_up && !s.facts.vcn_up && s.facts.read_back == Some(FEATURES), "JPEG said as left up, VCN down");
    // PowerDownVcn fails: nothing is left to power down, the read-back follows.
    let mut s = fresh();
    until(&mut s, (0x2B, 0));
    s.sent();
    s.failed(0xFF);
    assert_eq!(rest(&mut s), [0x0D, 0x0C]);
    assert!(!s.facts.jpeg_up && s.facts.vcn_up, "VCN said as left up");
    // No answer to PowerDownJpeg, or a code Linux takes for a hang: nothing more, VCN included.
    for answer in [0, 0xFB] {
        let mut s = fresh();
        until(&mut s, (0x2D, 0));
        s.sent();
        s.failed(answer);
        assert_eq!(s.next(), Step::End, "{answer:#x}");
    }
    // The same on what a stop left to do: PowerDownJpeg answered with an error there, VCN goes down still.
    let mut s = fresh();
    until(&mut s, (0x1F, 0x2_0003));
    s.sent();
    s.failed(0xFF);
    assert_eq!(s.next(), Step::Send { msg: 0x2D, arg: 0, value: false });
    s.sent();
    s.failed(0xFF);
    assert_eq!(rest(&mut s), [0x2B, 0x0D, 0x0C]);
    assert_eq!(s.stopped(), Some(Stopped::Answer { msg: 0x1F, arg: 0x2_0003, answer: 0xFF }), "the first stop stays the reason");
    assert!(s.facts.jpeg_up && !s.facts.vcn_up);
    // PowerDownVcn answered with an error there: the read-back still.
    let mut s = fresh();
    until(&mut s, (0x2C, 0));
    s.sent();
    s.failed(0xFF);
    s.sent();
    s.failed(0xFE);
    assert_eq!(rest(&mut s), [0x0D, 0x0C]);
    // What ends it: no answer or a hang code on what the stop left to do, a failed read-back, the stage's own
    // trouble with the bus.
    for answer in [0, 0xFB] {
        let mut s = fresh();
        until(&mut s, (0x1F, 0x2_0003));
        s.sent();
        s.failed(0xFF);
        s.sent();
        s.failed(answer);
        assert_eq!(s.next(), Step::End, "{answer:#x}");
    }
    let mut s = fresh();
    until(&mut s, (0x06, 0));
    s.sent();
    s.failed(0xFF);
    s.sent();
    s.failed(0xFF);
    assert_eq!((s.next(), s.facts.read_back), (Step::End, None), "a read-back that fails is not asked again");
    let mut s = fresh();
    until(&mut s, (0x1F, 0x2_0003));
    s.sent();
    s.failed(0xFF);
    s.mute();
    assert_eq!(s.next(), Step::End, "a register that does not read while the stop's rest runs");
    // The script's own stop (an answer Linux's code cannot go on with) after the features are on: the
    // read-back still.
    let mut s = fresh();
    until(&mut s, (0x4D, 0));
    s.stop(Stopped::Levels { clock: "SOCCLK", count: 17 });
    assert_eq!(rest(&mut s), [0x0D, 0x0C]);
    // A PMFW whose answer register does not read as answered before a message, or a bus that failed the
    // stage, gets nothing more.
    let mut s = fresh();
    until(&mut s, (0x4D, 0));
    s.mute();
    assert_eq!(s.next(), Step::End);
}

#[test]
fn answers_linuxs_own_code_cannot_go_on_with_stop_the_script() {
    // A clock's feature runs when its table is asked for and no longer at a later question.
    let mut s = Script::new(TABLES, CAPS);
    until(&mut s, (0x1F, 0x1_00FF));
    s.sent();
    s.answered(Some(0x8000_0002)).unwrap();
    s.sent();
    s.answered(Some((FEATURES >> 32) as u32)).unwrap();
    s.sent();
    assert_eq!(s.answered(Some((FEATURES & !(1 << 5)) as u32)), Err(Stopped::FeatureWentOff { clock: "SOCCLK" }));
    assert_eq!(s.stopped(), Some(Stopped::FeatureWentOff { clock: "SOCCLK" }));
    // More levels than Linux's table holds.
    let mut s = Script::new(TABLES, CAPS);
    until(&mut s, (0x1F, 0x1_00FF));
    s.sent();
    assert_eq!(s.answered(Some(0x8000_0011)), Err(Stopped::Levels { clock: "SOCCLK", count: 17 }));
    // Sixteen are taken.
    let mut s = Script::new(TABLES, CAPS);
    until(&mut s, (0x1F, 0x1_00FF));
    s.sent();
    assert_eq!(s.answered(Some(16)), Ok(()));
    // A table that is none ends the start before RunDcBtc.
    let mut s = Script::new(TABLES, CAPS);
    until(&mut s, (0x12, 1));
    s.sent();
    s.answered(None).unwrap();
    assert_eq!(s.next(), Step::Table);
    assert_eq!(s.table(&[0u8; 0x4000]), Err(Stopped::Table(pptable14::Bad::Empty)));
    assert_eq!(s.next(), Step::End);
}

#[test]
fn a_table_is_taken_only_at_its_step() {
    let good = table_bytes(&table());
    // Before the transfer was answered: the script stays where it is and learns nothing.
    for upto in [(0x03, 0), (0x0E, (TABLES.driver_mc >> 32) as u32), (0x12, 1)] {
        let mut s = Script::new(TABLES, CAPS);
        until(&mut s, upto);
        let (next, facts) = (s.next(), s.facts.clone());
        assert_eq!(s.table(&good), Err(Stopped::Stage), "{upto:x?}");
        assert_eq!((s.next(), &s.facts, s.stopped()), (next, &facts, None), "{upto:x?}");
    }
    // At its step, once.
    let mut s = Script::new(TABLES, CAPS);
    until(&mut s, (0x36, 0));
    assert!(s.facts.table.is_some());
    // After it (RunDcBtc is next, then the link levels, then the features): a second table would put the
    // script back before RunDcBtc and change the arguments it holds the stage to.
    for upto in [(0x36, 0), (0x20, 0x1_0406), (0x06, 0), (0x4D, 0)] {
        let mut s = Script::new(TABLES, CAPS);
        until(&mut s, upto);
        let (next, facts) = (s.next(), s.facts.clone());
        let other = table_bytes(&Pptable { pcie: PcieLevels::default(), software_shutdown_temp: 90, ..table() });
        assert_eq!(s.table(&other), Err(Stopped::Stage), "{upto:x?}");
        assert_eq!((s.next(), &s.facts, s.stopped()), (next, &facts, None), "{upto:x?}");
    }
    // And not on a script that stopped.
    let mut s = Script::new(TABLES, CAPS);
    until(&mut s, (0x12, 1));
    s.sent();
    s.answered(None).unwrap();
    s.mute();
    assert_eq!(s.table(&good), Err(Stopped::Stage));
    assert!(s.facts.table.is_none());
}
