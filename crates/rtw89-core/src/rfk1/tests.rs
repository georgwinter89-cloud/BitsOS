//! rfk1 on the Rust chip model (no node): what the JS oracle (tests/rfk1_js.rs) cannot see —
//! the events for the caller's log (B6), the poll loop's deadline and its bound on a frozen
//! clock (B4), the refusals of impossible paths, slots and channels, and the MAC side through
//! the real macinit and fw.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::fake::{DOp, Key, RfFake};
use super::iqkstep::{IqkType, iqk_one_shot, lok_finetune_check};
use super::util::{
    btc_phymap, chan_get, poll_read, rfk_backup_bb_reg, rfk_backup_rf_reg, rfk_restore_bb_reg,
    rfk_restore_rf_reg,
};
use super::*;
use crate::bus::{Bus, prep};
use crate::fw::FwState;
use crate::macinit::MacState;
use crate::membus::MemBus;
use crate::pci::{RingConfig, pci_alloc};
use crate::phy::{Band, Chan, Phy};

/// The neighbours as test-rfk1-stubs.mjs has them: notes only, the sch TX stop reads CTN_TXEN
#[derive(Default)]
struct Stubs(Vec<&'static str>);

impl<B: Bus + ?Sized> RfkHooks<B> for Stubs {
    fn btc_wl_rfk(&mut self, _: &mut B, _: u32, _: u32, _: u32) -> Rtw89Result<()> {
        self.0.push("btc_wl_rfk");
        Ok(())
    }
    fn btc_conn_rfk(&mut self, _: &mut B, _: bool) -> Rtw89Result<()> {
        self.0.push("btc_conn_rfk");
        Ok(())
    }
    fn btc_preserve_bt_time(&mut self, _: &mut B, _: u32) -> Rtw89Result<()> {
        self.0.push("btc_preserve_bt_time");
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.0.push("stop_sch_tx");
        Ok(bus.r16(R_AX_CTN_TXEN)?)
    }
    fn resume_sch_tx(&mut self, _: &mut B, _: u16) -> Rtw89Result<()> {
        self.0.push("resume_sch_tx");
        Ok(())
    }
    fn h2c(&mut self, _: &mut B, _: &H2c, _: &[u8]) -> Rtw89Result<()> {
        self.0.push("h2c");
        Ok(())
    }
    fn dpk_init(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        self.0.push("dpk_init");
        Ok(())
    }
    fn dpk(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        self.0.push("dpk");
        Ok(())
    }
    fn dpk_track(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        self.0.push("dpk_track");
        Ok(())
    }
    fn tssi(
        &mut self,
        _: &mut B,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
        _: bool,
    ) -> Rtw89Result<()> {
        self.0.push("tssi");
        Ok(())
    }
    fn tssi_scan(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        self.0.push("tssi_scan");
        Ok(())
    }
    fn wifi_scan_notify(&mut self, _: &mut B, _: &mut Phy, _: bool, _: u32) -> Rtw89Result<()> {
        self.0.push("wifi_scan_notify");
        Ok(())
    }
}

fn ch(band: Band, ch: u32, bw: u32) -> Chan {
    Chan { band, ch, bw }
}
const CH6: Chan = Chan {
    band: Band::B2g,
    ch: 6,
    bw: 20,
};

#[test]
fn a_good_chip_gives_no_events_and_linux_results() {
    let mut b = RfFake::fresh();
    let (mut phy, mut st, mut hooks) = (Phy::default(), RfkState::new(), Stubs::default());
    phy.tssi_mode = [true, true];
    st.mcc.table_idx = 1;
    rfk_init(&mut b, &mut phy, &mut st, None, &mut hooks).unwrap();
    assert_eq!(
        phy.tssi_mode,
        [false, false],
        "rfk_init clears the TSSI mode"
    );
    assert_eq!(st.mcc, Mcc::default(), "and the RFK channel slots");
    assert_eq!(
        hooks.0,
        [
            "dpk_init",
            "btc_wl_rfk",
            "btc_wl_rfk",
            "btc_wl_rfk",
            "stop_sch_tx",
            "resume_sch_tx",
            "btc_wl_rfk"
        ]
    );
    let rck = Rck {
        rck_val: 0x11,
        timeout: false,
    };
    assert_eq!(st.rck, [Some(rck), Some(rck)]);
    assert!(st.dack.dack_done && st.dack.dack_cnt == 1);
    assert_eq!(st.dack.addck_d, [[0x123, 0x2a5], [0x0f0, 0x0c3]]);
    assert_eq!(
        (
            st.dack.msbk_d[0][0][5],
            st.dack.msbk_d[0][1][15],
            st.dack.msbk_d[1][0][0]
        ),
        (0x45, 0x5f, 0x60)
    );
    let a: Vec<_> = st
        .dack
        .addc
        .iter()
        .map(|a| (a.path, a.dc_re, a.dc_im))
        .collect();
    assert_eq!(
        a,
        [(0, -2, 3), (0, -2, 3), (1, -2, 3), (1, -2, 3), (1, -2, 3)]
    );
    rfk_channel(&mut b, &mut phy, &mut st, Some(&CH6), &mut hooks, 0).unwrap();
    assert_eq!(st.iqk.nb_txcfir, [0x4000_0000; 2]);
    assert_eq!(st.iqk.nb_rxcfir, [0x4000_0000; 2]);
    assert_eq!(
        (st.iqk.iqk_times, st.iqk.iqk_fail_cnt),
        (2, 0),
        "one count per path"
    );
    assert_eq!((st.mcc.ch[0], st.mcc.table_idx), (6, 0));
    assert!(st.events().is_empty(), "{:?}", st.events());
}

#[test]
fn timeouts_and_failures_become_events() {
    // RCK never done on both paths
    let mut b = RfFake::fresh();
    b.rf.insert((0, RR_RCKS), 0);
    b.rf.insert((1, RR_RCKS), 0);
    let mut st = RfkState::new();
    rck(&mut b, &mut st).unwrap();
    assert_eq!(
        st.take_events(),
        [
            RfkEvent::RckTimeout { path: 0, value: 0 },
            RfkEvent::RckTimeout { path: 1, value: 0 }
        ]
    );
    // DRCK, S0 ADDCK, both MSBK, S1 DADCK never done
    let mut b = RfFake::fresh();
    b.answer(Key::Bb(R_DRCK_RS), prep(B_DRCK_RS_LPS, 0x15))
        .answer(Key::Bb(R_ADDCKR0), 0x2)
        .answer(Key::Bb(R_DACK_S0P0), 0)
        .answer(Key::Bb(R_DACK_S1P0), 0)
        .answer(Key::Bb(R_DACK_S1P1), 0)
        .answer(Key::Bb(R_DACK10S), 0)
        .answer(Key::Bb(R_DACK11S), 0);
    let mut st = RfkState::new();
    dack(&mut b, &mut st, None, &mut Stubs::default()).unwrap();
    assert_eq!(
        st.take_events(),
        [
            RfkEvent::DrckTimeout { value: 0 },
            RfkEvent::AddckTimeout { path: 0, value: 0 },
            RfkEvent::MsbkTimeout { path: 0 },
            RfkEvent::MsbkTimeout { path: 1 },
            RfkEvent::DadckTimeout { path: 1 },
        ]
    );
    assert_eq!(
        (
            st.dack.addck_timeout,
            st.dack.msbk_timeout,
            st.dack.dadck_timeout
        ),
        ([true, false], [true, true], [false, true])
    );
    // path A stays in TX mode
    let mut b = RfFake::fresh();
    b.rf.insert((0, RR_MOD), prep(RR_MOD_MASK, 2));
    let mut st = RfkState::new();
    rx_dck(
        &mut b,
        &Phy::default(),
        &mut st,
        None,
        &mut Stubs::default(),
        0,
    )
    .unwrap();
    assert_eq!(
        st.take_events(),
        [RfkEvent::RxModeTimeout { path: 0, mode: 2 }]
    );
    // the LPF stays busy: the LO check is skipped
    let mut b = RfFake::fresh();
    b.rf.insert((0, RR_LPF), RR_LPF_BUSY);
    let mut st = RfkState::new();
    set_channel_rf(&mut b, &mut st, Some(&ch(Band::B2g, 1, 20)), 0).unwrap();
    assert_eq!(st.take_events(), [RfkEvent::LpfBusyTimeout { value: 1 }]);
    // the LO locks only after all three remedies
    let mut b = RfFake::fresh();
    b.rf_answer_fn(0, RR_SYNFB, |_, _| 0);
    let mut st = RfkState::new();
    set_channel_rf(&mut b, &mut st, Some(&ch(Band::B2g, 11, 20)), 0).unwrap();
    assert_eq!(
        st.lck,
        [LckStep::MmdReset, LckStep::ResetRf18, LckStep::SynOffOn]
    );
    let ev = st.take_events();
    assert_eq!(ev.len(), 3);
    assert_eq!(ev[2].to_string(), "[LCK] LO not locked: syn off/on");
}

#[test]
fn iqk_failures_become_events() {
    // the LOK codes on the rail on path A: three tries, then failed
    let mut b = RfFake::fresh();
    b.rf.insert((0, RR_TXMO), 0);
    let mut st = RfkState::new();
    iqk(&mut b, &mut st, Some(&CH6), &mut Stubs::default(), 0).unwrap();
    assert_eq!(
        st.take_events(),
        [
            RfkEvent::LokRetry { path: 0, ibias: 2 },
            RfkEvent::LokRetry { path: 0, ibias: 3 },
            RfkEvent::LokFailed { path: 0 },
        ]
    );
    assert_eq!(st.iqk.lok_fail, [true, false]);
    // every one-shot reports a failure: TXK, RXK and the restore fail on both paths
    let mut b = RfFake::fresh();
    b.answer(Key::Bb(R_NCTL_RPT), B_NCTL_RPT_FLG);
    let mut st = RfkState::new();
    iqk(&mut b, &mut st, Some(&CH6), &mut Stubs::default(), 0).unwrap();
    let ev = st.take_events();
    for path in [0, 1] {
        for e in [
            RfkEvent::TxkFailed { path },
            RfkEvent::RxkFailed { path },
            RfkEvent::IqkRestoreFailed { path },
        ] {
            assert!(ev.contains(&e), "{e:?} in {ev:?}");
        }
    }
    assert_eq!(st.iqk.nb_txcfir, [0x4000_0002; 2]);
    assert_eq!(st.iqk.iqk_fail_cnt, 2);
    // the NCTL engine never answers: a failed one-shot without the report read (only the
    // debug read of R_NCTL_RPT stays)
    let mut b = RfFake::fresh();
    b.answer(Key::Bb(0xbff8), 0x12);
    let mut st = RfkState::new();
    assert!(iqk_one_shot(&mut b, &mut st, 0, 1, IqkType::Txk).unwrap());
    assert_eq!(
        st.take_events(),
        [RfkEvent::NctlTimeout {
            path: 1,
            value: 0x12
        }]
    );
    let reads = b
        .ops()
        .iter()
        .filter(|o| **o == DOp::BbR { addr: R_NCTL_RPT })
        .count();
    assert_eq!(reads, 1);
    // the type without a command does nothing
    let mut b = RfFake::fresh();
    assert!(!iqk_one_shot(&mut b, &mut st, 0, 0, IqkType::TxAgc).unwrap());
    assert!(b.trace.is_empty());
}

#[test]
fn the_event_log_is_capped() {
    let mut st = RfkState::new();
    st.set_event_cap(2);
    for path in 0..5 {
        st.push(RfkEvent::MsbkTimeout { path });
    }
    assert_eq!(st.events_dropped(), 3);
    assert_eq!(
        st.take_events(),
        [
            RfkEvent::MsbkTimeout { path: 3 },
            RfkEvent::MsbkTimeout { path: 4 }
        ]
    );
}

// test-rfk1-fail.mjs part 2's pollRead check: reads that take 400 us each; like
// read_poll_timeout_atomic the read that decides the timeout starts after the deadline
#[test]
fn poll_read_reads_once_more_after_the_deadline() {
    let mut b = RfFake::new();
    b.sim_us = 7300;
    let deadline = (7 + 2) * 1000; // whole ms: 1000 us from 7.3 ms end at 9 ms
    let mut starts = Vec::new();
    let p = poll_read(
        &mut b,
        |b| {
            starts.push(b.sim_us);
            b.sim_us += 400;
            Ok(starts.len() as u32)
        },
        |_| false,
        1,
        1000,
    )
    .unwrap();
    assert!(p.timed_out);
    assert_eq!(p.val as usize, starts.len());
    let n = starts.len();
    assert!(
        starts[n - 1] >= deadline && starts[n - 2] < deadline,
        "{starts:?}"
    );
    let mut k = 0;
    let p = poll_read(
        &mut b,
        |_| {
            k += 1;
            Ok(k)
        },
        |v| v == 2,
        1,
        1000,
    )
    .unwrap();
    assert_eq!((p.timed_out, p.val), (false, 2));
}

// B4: a clock that never moves (MemBus counts whole ms per delay, so 1 us steps never move
// it) cannot make a poll endless: the waited delays end it
#[test]
fn poll_read_ends_on_a_frozen_clock() {
    let mut b = MemBus::new();
    let mut reads = 0u32;
    let p = poll_read(
        &mut b,
        |_| {
            reads += 1;
            Ok(0)
        },
        |v| v != 0,
        1,
        1000,
    )
    .unwrap();
    assert!(p.timed_out);
    assert_eq!(reads, 2002, "2 ms of 1 us steps, then the read after");
    // a poll without a delay is bounded too
    let mut reads = 0u32;
    let p = poll_read(
        &mut b,
        |_| {
            reads += 1;
            Ok(0)
        },
        |v| v != 0,
        0,
        30,
    )
    .unwrap();
    assert!(p.timed_out);
    assert_eq!(reads, 2002);
}

#[test]
fn impossible_paths_slots_and_channels_are_refused() {
    let mut b = RfFake::fresh();
    let mut st = RfkState::new();
    for path in [2, 3, u32::MAX] {
        assert!(matches!(
            lok_finetune_check(&mut b, &mut st, path),
            Err(Rtw89Error::Invalid { value, .. }) if value == path
        ));
        assert!(iqk_one_shot(&mut b, &mut st, 0, path, IqkType::Rxk).is_err());
    }
    st.mcc.table_idx = 2;
    assert!(matches!(
        lok_finetune_check(&mut b, &mut st, 0),
        Err(Rtw89Error::Invalid { value: 2, .. })
    ));
    assert!(iqk(&mut b, &mut st, Some(&CH6), &mut Stubs::default(), 0).is_err());
    st.mcc.table_idx = 3;
    assert!(matches!(
        h2c_rf_ntfy_mcc(&mut b, &st, &mut Stubs::default()),
        Err(Rtw89Error::Invalid { value: 3, .. })
    ));
    st.mcc.table_idx = 2; // the third channel exists in the H2C's struct
    assert!(h2c_rf_ntfy_mcc(&mut b, &st, &mut Stubs::default()).is_ok());
    for (c, value) in [
        (ch(Band::B6g, 37, 20), 37),
        (ch(Band::B2g, 1, 160), 160),
        (ch(Band::B5g, 36, 10), 10),
    ] {
        assert!(matches!(
            chan_get(Some(&c)),
            Err(Rtw89Error::Invalid { value: v, .. }) if v == value
        ));
        let n = b.trace.len();
        assert!(set_channel_rf(&mut b, &mut st, Some(&c), 0).is_err());
        assert_eq!(b.trace.len(), n, "refused before any access");
    }
    let sub = |c: Chan| chan_get(Some(&c)).unwrap().subband_type;
    assert_eq!(sub(ch(Band::B5g, 149, 80)), RTW89_CH_5G_BAND_4);
    assert_eq!(sub(ch(Band::B5g, 100, 40)), RTW89_CH_5G_BAND_3);
    assert_eq!(sub(ch(Band::B5g, 36, 20)), RTW89_CH_5G_BAND_1);
    assert_eq!(btc_phymap(None, 0, RF_AB).unwrap(), 0x13);
    assert_eq!(
        btc_phymap(Some(&ch(Band::B5g, 36, 20)), 0, 0).unwrap(),
        0x50
    );
}

// MacLink: the real macinit (firmware not running: the CTN_TXEN register itself) and fw's H2C
#[test]
fn the_mac_link_uses_macinit_and_fw() {
    let mut b = RfFake::fresh();
    b.answer(Key::Mac(R_AX_CMAC_FUNC_EN), B_AX_CMAC_EN);
    let (mut mac, mut fw) = (MacState::new(), FwState::new());
    let mut link = MacLink {
        mac: &mut mac,
        fw: &mut fw,
        rings: None,
    };
    assert_eq!(link.stop_sch_tx(&mut b).unwrap(), 0xffff);
    link.resume_sch_tx(&mut b, 0x1234).unwrap();
    let all = B_AX_CTN_TXEN_ALL_MASK;
    assert_eq!(
        b.lines(),
        [
            "rd16 0x0000C348 0x00000000 0x00000000".to_string(),
            format!("rd32 0x{R_AX_CMAC_FUNC_EN:08X} 0x00000000 0x00000000"),
            format!("rmw16 0x0000C348 0x{all:08X} 0x00000000"),
            format!("rd32 0x{R_AX_CMAC_FUNC_EN:08X} 0x00000000 0x00000000"),
            format!("rmw16 0x0000C348 0x{all:08X} 0x{:08X}", 0x1234 & all),
        ]
    );
    let h = H2c {
        func: 2,
        ..H2c::default()
    };
    assert!(matches!(
        link.h2c(&mut b, &h, &[0; 20]),
        Err(Rtw89Error::Invalid { value: 2, .. })
    ));
    // with the rings: the H2C goes out on CH12, the sequence moves on
    let mut mb = MemBus::new();
    let mut rings = pci_alloc(&mut mb, &RingConfig::js()).unwrap();
    let mut link = MacLink {
        mac: &mut mac,
        fw: &mut fw,
        rings: Some(&mut rings),
    };
    let seq = link.fw.h2c_seq;
    let r = link.h2c(&mut mb, &h, &[0; 20]);
    assert_eq!(fw.h2c_seq, seq.wrapping_add(1), "{r:?}");
}

#[test]
fn rfk2_entries_go_to_the_hooks() {
    let mut b = RfFake::fresh();
    let (mut phy, mut hooks) = (Phy::default(), Stubs::default());
    rfk_band_changed(&mut b, &mut phy, None, &mut hooks).unwrap();
    rfk_scan(&mut b, &mut phy, true, &mut hooks, 0).unwrap();
    rfk_track(&mut b, &mut phy, &mut hooks).unwrap();
    assert_eq!(hooks.0, ["tssi_scan", "wifi_scan_notify", "dpk_track"]);
    assert!(b.trace.is_empty());
}

#[test]
fn a_backup_of_the_wrong_length_is_refused_before_any_write() {
    let mut b = RfFake::fresh();
    let bb = rfk_backup_bb_reg(&mut b).unwrap();
    let rf = rfk_backup_rf_reg(&mut b, 0).unwrap();
    let before = b.trace.len();
    for short in [&bb[..0], &bb[..bb.len() - 1]] {
        assert!(matches!(
            rfk_restore_bb_reg(&mut b, short),
            Err(Rtw89Error::Invalid { value, .. }) if value as usize == short.len()
        ));
    }
    let mut long = bb.clone();
    long.push(0);
    assert!(rfk_restore_bb_reg(&mut b, &long).is_err());
    for short in [&rf[..0], &rf[..rf.len() - 1]] {
        assert!(matches!(
            rfk_restore_rf_reg(&mut b, short, 0),
            Err(Rtw89Error::Invalid { value, .. }) if value as usize == short.len()
        ));
    }
    assert_eq!(b.trace.len(), before, "no access before the refusal");
    rfk_restore_bb_reg(&mut b, &bb).unwrap();
    rfk_restore_rf_reg(&mut b, &rf, 0).unwrap();
    assert!(b.trace.len() > before);
}
