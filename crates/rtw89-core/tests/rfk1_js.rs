//! The rfk1 module against the JS driver's own rfk1.mjs (rfk1-util/-dack/-iqk/-iqkstep/-chan/
//! -tables.mjs), which test-rfk1.mjs checks op by op against its transcription of the C:
//! tests/fake/rfk1-js.mjs runs test-rfk1-seq.mjs's and test-rfk1-fail.mjs's scenarios on
//! test-rfk1.mjs's chip model (RfFake) with test-rfk1-stubs.mjs as the neighbours and prints
//! every op, every neighbour call where it happened, the results and the whole rfk state; it
//! also prints the RFK tables read from the Linux C files. The same scenarios run here on
//! `rfk1::fake::RfFake` (the same chip model in Rust) with stubs that do what the JS stubs do,
//! and the two texts must be equal line by line. Needs node and the read-only WLAN tree;
//! RTW89_SKIP_FAKE=1 skips (with a line on stderr), but never with BITSOS_GATE or CI set.

use std::path::PathBuf;
use std::process::Command;

use rtw89_core::bus::{Bus, field, prep};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::H2c;
use rtw89_core::phy::{Band, Chan, Phy};
use rtw89_core::rfk1::fake::{Key, RfFake};
use rtw89_core::rfk1::iqkstep::{IqkType, iqk_one_shot, lok_finetune_check};
use rtw89_core::rfk1::util::{chan_get, rfk_parser};
use rtw89_core::rfk1::{self, LckStep, RfkHooks, RfkState};
use rtw89_core::tables::regs::*;
use rtw89_core::tables::{Rfk, rfk1 as t};

fn skip(test: &str) -> bool {
    use std::io::Write;
    if std::env::var_os("RTW89_SKIP_FAKE").is_some_and(|v| v == "1") {
        // a gate run never skips the JS oracle: refuse the variable there
        for gate in ["BITSOS_GATE", "CI"] {
            assert!(
                std::env::var_os(gate).is_none_or(|v| v.is_empty() || v == "0"),
                "RTW89_SKIP_FAKE=1 together with {gate}: the gate must run the JS oracle"
            );
        }
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS rfk1.mjs)"
        );
        return true;
    }
    false
}

fn wlan_dir() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into())
}

fn h(v: u32) -> String {
    format!("{v:#x}")
}
fn b01(v: bool) -> &'static str {
    if v { "1" } else { "0" }
}
fn l(a: &[u32]) -> String {
    a.iter().map(|&v| h(v)).collect::<Vec<_>>().join(",")
}
fn lb(a: &[bool]) -> String {
    a.iter().map(|&v| b01(v)).collect::<Vec<_>>().join(",")
}
fn ll<const N: usize>(a: &[[u32; N]]) -> String {
    a.iter().map(|x| l(x)).collect::<Vec<_>>().join(";")
}
fn llb<const N: usize>(a: &[[bool; N]]) -> String {
    a.iter().map(|x| lb(x)).collect::<Vec<_>>().join(";")
}

/// test-rfk1-stubs.mjs in Rust: each call is noted with its position in the trace
#[derive(Default)]
struct Stubs {
    calls: Vec<(usize, String)>,
}

impl Stubs {
    fn log(&mut self, bus: &RfFake, name: &str, args: &[String]) {
        let mut s = format!("= call {name}");
        for a in args {
            s.push(' ');
            s.push_str(a);
        }
        self.calls.push((bus.trace.len(), s));
    }
}

impl RfkHooks<RfFake> for Stubs {
    fn btc_wl_rfk(
        &mut self,
        bus: &mut RfFake,
        phy_map: u32,
        typ: u32,
        state: u32,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, "btcWlRfk", &[h(phy_map), h(typ), h(state)]);
        Ok(())
    }
    fn btc_conn_rfk(&mut self, bus: &mut RfFake, start: bool) -> Result<(), Rtw89Error> {
        self.log(bus, "btcConnRfk", &[b01(start).into()]);
        Ok(())
    }
    fn btc_preserve_bt_time(&mut self, bus: &mut RfFake, ms: u32) -> Result<(), Rtw89Error> {
        self.log(bus, "btcPreserveBtTime", &[h(ms)]);
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut RfFake) -> Result<u16, Rtw89Error> {
        self.log(bus, "stopSchTx", &["all".into()]);
        Ok(bus.r16(R_AX_CTN_TXEN)?)
    }
    fn resume_sch_tx(&mut self, bus: &mut RfFake, tx_en: u16) -> Result<(), Rtw89Error> {
        self.log(bus, "resumeSchTx", &[h(tx_en.into())]);
        Ok(())
    }
    fn h2c(&mut self, bus: &mut RfFake, hd: &H2c, payload: &[u8]) -> Result<(), Rtw89Error> {
        let hex: String = payload.iter().map(|x| format!("{x:02x}")).collect();
        self.log(
            bus,
            "h2cSend",
            &[
                h(hd.cat),
                h(hd.cls),
                h(hd.func),
                b01(hd.rack).into(),
                b01(hd.dack).into(),
                hex,
            ],
        );
        Ok(())
    }
    fn dpk_init(&mut self, bus: &mut RfFake, _: &mut Phy) -> Result<(), Rtw89Error> {
        self.log(bus, "dpkInit", &[]);
        Ok(())
    }
    fn dpk(
        &mut self,
        bus: &mut RfFake,
        _: &mut Phy,
        _: Option<&Chan>,
        phy_idx: u32,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, "dpk", &[h(phy_idx)]);
        Ok(())
    }
    fn dpk_track(&mut self, bus: &mut RfFake, _: &mut Phy) -> Result<(), Rtw89Error> {
        self.log(bus, "dpkTrack", &[]);
        Ok(())
    }
    fn tssi(
        &mut self,
        bus: &mut RfFake,
        _: &mut Phy,
        _: Option<&Chan>,
        phy_idx: u32,
        hwtx_en: bool,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, "tssi", &[h(phy_idx), b01(hwtx_en).into()]);
        Ok(())
    }
    fn tssi_scan(
        &mut self,
        bus: &mut RfFake,
        _: &mut Phy,
        _: Option<&Chan>,
        phy_idx: u32,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, "tssiScan", &[h(phy_idx)]);
        Ok(())
    }
    fn wifi_scan_notify(
        &mut self,
        bus: &mut RfFake,
        _: &mut Phy,
        start: bool,
        phy_idx: u32,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, "wifiScanNotify", &[b01(start).into(), h(phy_idx)]);
        Ok(())
    }
}

/// One device of a scenario: the chip, the rfk state, the phy's tssi_mode, the channel (JS d.chan)
struct Dev {
    b: RfFake,
    st: RfkState,
    phy: Phy,
    hooks: Stubs,
    chan: Option<Chan>,
}

impl Dev {
    fn new(b: RfFake) -> Dev {
        Dev {
            b,
            st: RfkState::new(),
            phy: Phy::default(),
            hooks: Stubs::default(),
            chan: None,
        }
    }
}

fn ch(band: &str, ch: u32, bw: u32) -> Chan {
    Chan {
        band: if band == "5g" { Band::B5g } else { Band::B2g },
        ch,
        bw,
    }
}

/// rfk1-js.mjs `stateLines`
fn state_lines(d: &Dev) -> Vec<String> {
    let (k, q, m) = (&d.st.dack, &d.st.iqk, &d.st.mcc);
    let rck =
        d.st.rck
            .iter()
            .map(|r| {
                r.map_or("-".to_string(), |r| {
                    format!("{}/{}", h(r.rck_val), b01(r.timeout))
                })
            })
            .collect::<Vec<_>>()
            .join(",");
    let lck =
        d.st.lck
            .iter()
            .map(|s| match s {
                LckStep::MmdReset => "mmd reset",
                LckStep::ResetRf18 => "re-set rf 0x18",
                LckStep::SynOffOn => "syn off/on",
            })
            .collect::<Vec<_>>()
            .join(",");
    vec![
        format!("= tssi {}", lb(&d.phy.tssi_mode)),
        format!("= rck {rck}"),
        format!(
            "= dack done {} cnt {} addck {} bias {} dadck {} to {} {} {}",
            b01(k.dack_done),
            k.dack_cnt,
            ll(&k.addck_d),
            ll(&k.biask_d),
            ll(&k.dadck_d),
            lb(&k.addck_timeout),
            lb(&k.msbk_timeout),
            lb(&k.dadck_timeout)
        ),
        format!(
            "= msbk {}",
            k.msbk_d.iter().map(|x| ll(x)).collect::<Vec<_>>().join("|")
        ),
        format!(
            "= addc {}",
            k.addc
                .iter()
                .map(|a| format!("{}:{}:{}", a.path, a.dc_re, a.dc_im))
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!(
            "= iqk init {} fail {} times {} ver {} syn {} en {}{}{}{} nb {}",
            b01(q.is_iqk_init),
            q.iqk_fail_cnt,
            q.iqk_times,
            h(q.version),
            q.syn1to2,
            b01(q.iqk_fft_en),
            b01(q.iqk_sram_en),
            b01(q.iqk_cfir_en),
            b01(q.iqk_xym_en),
            b01(q.is_nbiqk)
        ),
        format!(
            "= iqk fails {} {} {} {} lok {} restore {}",
            llb(&q.lok_cor_fail),
            llb(&q.lok_fin_fail),
            llb(&q.iqk_tx_fail),
            llb(&q.iqk_rx_fail),
            lb(&q.lok_fail),
            lb(&q.restore_fail)
        ),
        format!(
            "= iqk chan {} band {} ch {} bw {} mcc {} tidx {}",
            l(&q.iqk_channel),
            l(&q.iqk_band),
            l(&q.iqk_ch),
            l(&q.iqk_bw),
            ll(&q.iqk_mcc_ch),
            l(&q.iqk_table_idx)
        ),
        format!(
            "= iqk cfir {} {} bp {} {} {} wb {} {}",
            l(&q.nb_txcfir),
            l(&q.nb_rxcfir),
            l(&q.bp_txkresult),
            l(&q.bp_rxkresult),
            l(&q.bp_iqkenable),
            lb(&q.is_wb_txiqk),
            lb(&q.is_wb_rxiqk)
        ),
        format!("= iqk lok {} {}", ll(&q.lok_idac), ll(&q.lok_vbuf)),
        format!(
            "= mcc {} {} {} {} {}",
            l(&m.ch),
            l(&m.band),
            l(&m.bw),
            l(&m.rf18),
            m.table_idx
        ),
        format!("= lck {lck}"),
    ]
}

struct Out(Vec<String>);

impl Out {
    fn scenario(
        &mut self,
        name: &str,
        mut d: Dev,
        f: impl FnOnce(&mut Dev) -> Result<Vec<String>, Rtw89Error>,
    ) {
        self.0.push(format!("## {name}"));
        d.b.sim_us = 0;
        let t0 = d.b.trace.len();
        d.hooks.calls.clear();
        let res = match f(&mut d) {
            Ok(v) => v,
            Err(_) => vec!["! error".to_string()],
        };
        let ops = d.b.lines()[t0..].to_vec();
        for i in 0..=ops.len() {
            for (pos, c) in &d.hooks.calls {
                if pos - t0 == i {
                    self.0.push(c.clone());
                }
            }
            if i < ops.len() {
                self.0.push(ops[i].clone());
            }
        }
        self.0.extend(res);
        self.0.extend(state_lines(&d));
    }
}

fn rfk_line(e: &Rfk) -> String {
    match *e {
        Rfk::Wm { addr, mask, data } => format!("WM {} {} {}", h(addr), h(mask), h(data)),
        Rfk::Ws { addr, bits } => format!("WS {} {}", h(addr), h(bits)),
        Rfk::Wc { addr, bits } => format!("WC {} {}", h(addr), h(bits)),
        Rfk::Delay { us } => format!("DELAY {}", h(us)),
        Rfk::Wrf {
            path,
            addr,
            mask,
            data,
        } => format!("WRF {} {} {} {}", h(path.into()), h(addr), h(mask), h(data)),
    }
}

fn c_tables(out: &mut Vec<String>) {
    out.push("## c tables".into());
    let tables: [(&str, &[Rfk]); 13] = [
        ("afe_init_defs", t::AFE_INIT_DEFS),
        ("check_addc_defs_a", t::CHECK_ADDC_DEFS_A),
        ("check_addc_defs_b", t::CHECK_ADDC_DEFS_B),
        ("check_dadc_en_defs_a", t::CHECK_DADC_EN_DEFS_A),
        ("check_dadc_en_defs_b", t::CHECK_DADC_EN_DEFS_B),
        ("check_dadc_dis_defs_a", t::CHECK_DADC_DIS_DEFS_A),
        ("check_dadc_dis_defs_b", t::CHECK_DADC_DIS_DEFS_B),
        ("dack_s0_1_defs", t::DACK_S0_1_DEFS),
        ("dack_s0_2_defs", t::DACK_S0_2_DEFS),
        ("dack_s0_3_defs", t::DACK_S0_3_DEFS),
        ("dack_s1_1_defs", t::DACK_S1_1_DEFS),
        ("dack_s1_2_defs", t::DACK_S1_2_DEFS),
        ("dack_s1_3_defs", t::DACK_S1_3_DEFS),
    ];
    for (name, tbl) in tables {
        for e in tbl {
            out.push(format!("= tbl {name} {}", rfk_line(e)));
        }
    }
    let u8s = |a: &[u8]| a.iter().map(|&v| u32::from(v)).collect::<Vec<_>>();
    let u16s = |a: &[u16]| a.iter().map(|&v| u32::from(v)).collect::<Vec<_>>();
    let rows = |a: &[Rfk]| {
        a.iter()
            .map(|e| match *e {
                Rfk::Wm { addr, mask, data } => format!("{}/{}/{}", h(addr), h(mask), h(data)),
                _ => "?".into(),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let arrays: [(&str, String); 18] = [
        ("_a_idxrxgain", l(&u16s(t::A_IDXRXGAIN))),
        ("_a_idxattc2", l(&u8s(t::A_IDXATTC2))),
        ("_a_idxattc1", l(&u8s(t::A_IDXATTC1))),
        ("_g_idxrxgain", l(&u16s(t::G_IDXRXGAIN))),
        ("_g_idxattc2", l(&u8s(t::G_IDXATTC2))),
        ("_g_idxattc1", l(&u8s(t::G_IDXATTC1))),
        ("_a_power_range", l(&u8s(t::A_POWER_RANGE))),
        ("_a_track_range", l(&u8s(t::A_TRACK_RANGE))),
        ("_a_gain_bb", l(&u8s(t::A_GAIN_BB))),
        ("_a_itqt", l(&u8s(t::A_ITQT))),
        ("_g_power_range", l(&u8s(t::G_POWER_RANGE))),
        ("_g_track_range", l(&u8s(t::G_TRACK_RANGE))),
        ("_g_gain_bb", l(&u8s(t::G_GAIN_BB))),
        ("_g_itqt", l(&u8s(t::G_ITQT))),
        ("rtw8852b_backup_bb_regs", l(&u16s(t::BACKUP_BB_REGS))),
        ("rtw8852b_backup_rf_regs", l(t::BACKUP_RF_REGS)),
        ("rtw8852b_set_nondbcc_path01", rows(t::SET_NONDBCC_PATH01)),
        (
            "rtw8852b_restore_nondbcc_path01",
            rows(t::RESTORE_NONDBCC_PATH01),
        ),
    ];
    for (name, v) in arrays {
        out.push(format!("= arr {name} {v}"));
    }
}

fn not_bit(bit: u32) -> impl Fn(u32) -> u32 {
    move |v| v & !bit
}

/// test-rfk1-fail.mjs `chanFake`
fn chan_fake(b: &mut RfFake) {
    b.rf_set(0, 0x10018, 0x00c01);
    b.rf_set(1, 0x10018, 0x00c01);
    b.rf.insert((0, RR_LDO), 0x123);
}

fn fresh_chan() -> RfFake {
    let mut b = RfFake::fresh();
    chan_fake(&mut b);
    b
}

fn rust_side() -> Vec<String> {
    let mut o = Out(Vec::new());
    c_tables(&mut o.0);

    // ---- part 1 (test-rfk1-seq.mjs) ----
    o.scenario("parser", Dev::new(RfFake::fresh()), |d| {
        rfk_parser(
            &mut d.b,
            &[
                Rfk::Ws {
                    addr: 0x1234,
                    bits: 0x10,
                },
                Rfk::Wc {
                    addr: 0x1234,
                    bits: 0x4,
                },
                Rfk::Wm {
                    addr: 0x1238,
                    mask: 0xf0,
                    data: 0x5,
                },
                Rfk::Wrf {
                    path: 1,
                    addr: 0x55,
                    mask: 0xff,
                    data: 0x12,
                },
                Rfk::Delay { us: 7 },
            ],
        )?;
        let c = chan_get(None)?;
        Ok(vec![format!(
            "= chan {} {} {} {}",
            h(c.channel),
            h(c.band_type),
            h(c.band_width),
            h(c.subband_type)
        )])
    });
    o.scenario("rck", Dev::new(RfFake::fresh()), |d| {
        rfk1::rck(&mut d.b, &mut d.st)?;
        Ok(vec![])
    });
    {
        let mut b = RfFake::fresh();
        let mut sample = 0u32;
        b.answer_fn(Key::Bb(R_DBG32_D), move |_| {
            let v = if sample % 2 == 1 { 0xfff003 } else { 0xffe003 };
            sample += 1;
            v
        });
        o.scenario("dack", Dev::new(b), |d| {
            rfk1::dack(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks)?;
            Ok(vec![])
        });
    }
    for tssi in [false, true] {
        let mut d = Dev::new(RfFake::fresh());
        d.phy.tssi_mode[1] = tssi;
        o.scenario(&format!("rx dck tssi {}", b01(tssi)), d, |d| {
            rfk1::rx_dck(
                &mut d.b,
                &d.phy,
                &mut d.st,
                d.chan.as_ref(),
                &mut d.hooks,
                0,
            )?;
            Ok(vec![])
        });
    }
    for (band, c, lok, mhz) in [
        ("2g", 6, false, 20),
        ("5g", 36, false, 20),
        ("2g", 1, true, 20),
        ("2g", 6, false, 40),
        ("5g", 42, false, 80),
    ] {
        let mut b = RfFake::fresh();
        if lok {
            b.rf.insert((0, RR_TXMO), 0);
        }
        let mut d = Dev::new(b);
        d.chan = Some(ch(band, c, mhz));
        o.scenario(
            &format!("iqk {band} {c} {mhz}{}", if lok { " lok" } else { "" }),
            d,
            |d| {
                rfk1::iqk(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks, 0)?;
                Ok(vec![])
            },
        );
    }
    for (band, c, bw) in [
        ("2g", 1, 20),
        ("2g", 14, 20),
        ("5g", 36, 20),
        ("2g", 6, 40),
        ("2g", 6, 80),
    ] {
        o.scenario(
            &format!("channel {band} {c} {bw}"),
            Dev::new(fresh_chan()),
            |d| {
                rfk1::set_channel_rf(&mut d.b, &mut d.st, Some(&ch(band, c, bw)), 0)?;
                Ok(vec![format!(
                    "= rf18 {} {} {} {}",
                    h(d.b.peek(0, RR_CFGCH)),
                    h(d.b.peek(1, RR_CFGCH)),
                    h(d.b.peek(0, 0x10018)),
                    h(d.b.peek(1, 0x10018))
                )])
            },
        );
    }
    o.scenario("mcc slot", Dev::new(RfFake::fresh()), |d| {
        rfk1::mcc_get_ch_info(&mut d.st, Some(&ch("2g", 6, 20)), 0)?;
        rfk1::mcc_get_ch_info(&mut d.st, Some(&ch("5g", 36, 20)), 0)?;
        Ok(vec![])
    });
    {
        let mut d = Dev::new(RfFake::fresh());
        d.chan = Some(ch("2g", 11, 20));
        o.scenario("rfk channel", d, |d| {
            rfk1::rfk_channel(
                &mut d.b,
                &mut d.phy,
                &mut d.st,
                d.chan.as_ref(),
                &mut d.hooks,
                0,
            )?;
            Ok(vec![])
        });
    }
    {
        let mut d = Dev::new(RfFake::fresh());
        d.st.mcc.ch = [6, 36, 0];
        d.st.mcc.table_idx = 1;
        o.scenario("mccch slot 1", d, |d| {
            rfk1::h2c_rf_ntfy_mcc(&mut d.b, &d.st, &mut d.hooks)?;
            Ok(vec![])
        });
    }
    {
        let mut d = Dev::new(RfFake::fresh());
        d.chan = Some(ch("2g", 6, 20));
        rfk1::mcc_get_ch_info(&mut d.st, d.chan.as_ref(), 0).unwrap();
        d.phy.tssi_mode = [true, true];
        o.scenario("rfk init", d, |d| {
            rfk1::rfk_init(
                &mut d.b,
                &mut d.phy,
                &mut d.st,
                d.chan.as_ref(),
                &mut d.hooks,
            )?;
            Ok(vec![])
        });
    }
    o.scenario("rfk2 entries", Dev::new(RfFake::fresh()), |d| {
        rfk1::rfk_band_changed(&mut d.b, &mut d.phy, d.chan.as_ref(), &mut d.hooks)?;
        rfk1::rfk_scan(&mut d.b, &mut d.phy, true, &mut d.hooks, 0)?;
        rfk1::rfk_scan(&mut d.b, &mut d.phy, false, &mut d.hooks, 0)?;
        rfk1::rfk_track(&mut d.b, &mut d.phy, &mut d.hooks)?;
        Ok(vec![])
    });

    // ---- part 2 (test-rfk1-fail.mjs) ----
    {
        let mut b = RfFake::fresh();
        b.slow_fn(Key::Rf(0, RR_RCKS), not_bit(RR_RCKS))
            .slow_fn(Key::Rf(1, RR_RCKS), not_bit(RR_RCKS));
        o.scenario("slow rck", Dev::new(b), |d| {
            rfk1::rck(&mut d.b, &mut d.st)?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.slow_fn(Key::Bb(R_DRCK_RS), not_bit(B_DRCK_RS_DONE))
            .slow_fn(Key::Bb(R_ADDCKR0), not_bit(1))
            .slow_fn(Key::Bb(R_ADDCKR1), not_bit(1))
            .slow_fn(Key::Bb(R_DACK_S0P0), not_bit(B_DACK_S0P0_OK))
            .slow_fn(Key::Bb(R_DACK_S0P2), not_bit(B_DACK_S0P2_OK))
            .slow_fn(Key::Bb(R_DACK_S1P0), not_bit(B_DACK_S1P0_OK))
            .slow_fn(Key::Bb(R_DACK10S), not_bit(B_DACK_S1P2_OK));
        o.scenario("slow dack", Dev::new(b), |d| {
            rfk1::dack(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks)?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.slow_fn(Key::Rf(0, RR_MOD), |v| {
            (v & !RR_MOD_MASK) | prep(RR_MOD_MASK, 2)
        });
        o.scenario("slow rx dck", Dev::new(b), |d| {
            rfk1::rx_dck(
                &mut d.b,
                &d.phy,
                &mut d.st,
                d.chan.as_ref(),
                &mut d.hooks,
                0,
            )?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.slow_fn(Key::Bb(0xbff8), |_| 0);
        let mut d = Dev::new(b);
        d.chan = Some(ch("2g", 6, 20));
        o.scenario("slow iqk", d, |d| {
            rfk1::iqk(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks, 0)?;
            Ok(vec![])
        });
    }
    {
        let mut b = fresh_chan();
        b.slow_fn(Key::Rf(0, RR_LPF), |v| v | RR_LPF_BUSY);
        o.scenario("slow channel", Dev::new(b), |d| {
            rfk1::set_channel_rf(&mut d.b, &mut d.st, Some(&ch("2g", 1, 20)), 0)?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.rf.insert((0, RR_RCKS), 0);
        b.rf.insert((1, RR_RCKS), 0);
        o.scenario("rck timeout", Dev::new(b), |d| {
            rfk1::rck(&mut d.b, &mut d.st)?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.answer(Key::Bb(R_DRCK_RS), prep(B_DRCK_RS_LPS, 0x15))
            .answer(
                Key::Bb(R_ADDCKR0),
                prep(B_ADDCKR0_A0, 0x123) | prep(B_ADDCKR0_A1, 0x2a4),
            )
            .answer(Key::Bb(R_DACK_S0P0), 0)
            .answer(Key::Bb(R_DACK_S1P0), 0)
            .answer(Key::Bb(R_DACK_S1P1), 0)
            .answer_fn(Key::Bb(R_DACK10S), |r| {
                prep(B_DACK10S, 0x60 + field(r.bb(R_DACK10), B_DACK10))
            })
            .answer_fn(Key::Bb(R_DACK11S), |r| {
                prep(B_DACK11S, 0x70 + field(r.bb(R_DACK11), B_DACK11))
            });
        o.scenario("dack timeouts", Dev::new(b), |d| {
            rfk1::dack(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks)?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.rf.insert((0, RR_MOD), prep(RR_MOD_MASK, 2));
        o.scenario("rx mode timeout", Dev::new(b), |d| {
            rfk1::rx_dck(
                &mut d.b,
                &d.phy,
                &mut d.st,
                d.chan.as_ref(),
                &mut d.hooks,
                0,
            )?;
            Ok(vec![])
        });
    }
    {
        let mut b = RfFake::fresh();
        b.answer(Key::Bb(0xbff8), 0);
        let mut d = Dev::new(b);
        d.chan = Some(ch("2g", 1, 20));
        o.scenario("one-shot timeout", d, |d| {
            let fail = iqk_one_shot(&mut d.b, &mut d.st, 0, 0, IqkType::Txk)?;
            Ok(vec![format!("= fail {}", b01(fail))])
        });
    }
    {
        let mut b = fresh_chan();
        b.rf.insert((0, RR_LPF), RR_LPF_BUSY);
        o.scenario("lpf timeout", Dev::new(b), |d| {
            rfk1::set_channel_rf(&mut d.b, &mut d.st, Some(&ch("2g", 1, 20)), 0)?;
            Ok(vec![])
        });
    }
    // run 1: path A's coarse LOK and TXK group 2, path B's fine LOK, RXK groups 0 and 3 and the
    // restore one-shot; run 2: only path B's RXK group 1
    type Fails = (
        [bool; 2],
        [bool; 2],
        [&'static [u32]; 2],
        [&'static [u32]; 2],
        [bool; 2],
    );
    let fail_runs: [Fails; 2] = [
        (
            [true, false],
            [false, true],
            [&[2], &[]],
            [&[], &[0, 3]],
            [false, true],
        ),
        (
            [false, false],
            [false, false],
            [&[], &[]],
            [&[], &[1]],
            [false, false],
        ),
    ];
    for (i, (cor, fin, tx, rx, restore)) in fail_runs.into_iter().enumerate() {
        let mut b = RfFake::fresh();
        b.answer_fn(Key::Bb(R_NCTL_RPT), move |r| {
            let cmd = r.bb(R_NCTL_CFG).wrapping_sub(1);
            let p = ((cmd >> 5) & 1) as usize;
            let kind = (cmd >> 8) & 0xf;
            let lut = r.bb(R_CFIR_LUT + ((p as u32) << 8));
            let hit = (kind == 1 && cor[p])
                || (kind == 2 && fin[p])
                || (kind == 8 && tx[p].contains(&(lut & B_CFIR_LUT_GP)))
                || (kind == 0xb && rx[p].contains(&(lut & B_CFIR_LUT_GP_V1)))
                || (kind == 0xe && restore[p]);
            if hit { B_NCTL_RPT_FLG } else { 0 }
        });
        let mut d = Dev::new(b);
        d.chan = Some(ch("2g", 6, 20));
        o.scenario(&format!("iqk fails {i}"), d, |d| {
            rfk1::iqk(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks, 0)?;
            Ok(vec![])
        });
    }
    for [ci, cq, vi, vq] in [
        [0x10, 0x10, 0x20, 0x20],
        [0x2, 0x2, 0x2, 0x2],
        [0x1d, 0x1d, 0x3d, 0x3d],
        [0x1, 0x10, 0x20, 0x20],
        [0x10, 0x1, 0x20, 0x20],
        [0x1e, 0x10, 0x20, 0x20],
        [0x10, 0x1e, 0x20, 0x20],
        [0x10, 0x10, 0x1, 0x20],
        [0x10, 0x10, 0x20, 0x1],
        [0x10, 0x10, 0x3e, 0x20],
        [0x10, 0x10, 0x20, 0x3e],
    ] {
        let mut b = RfFake::fresh();
        b.rf.insert((1, RR_TXMO), prep(RR_TXMO_COI, ci) | prep(RR_TXMO_COQ, cq));
        b.rf.insert(
            (1, RR_LOKVB),
            prep(RR_LOKVB_COI, vi) | prep(RR_LOKVB_COQ, vq),
        );
        o.scenario(
            &format!("lok check {}", l(&[ci, cq, vi, vq])),
            Dev::new(b),
            |d| {
                let fail = lok_finetune_check(&mut d.b, &mut d.st, 1)?;
                Ok(vec![format!("= fail {}", b01(fail))])
            },
        );
    }
    for after in [1u32, 2, 3] {
        let mut b = fresh_chan();
        b.rf.insert((0, RR_POW), 0x3456c);
        b.rf.insert((0, RR_SX), 0x789ab);
        b.rf_answer_fn(
            0,
            RR_SYNFB,
            move |_, k| if k < after { 0 } else { RR_SYNFB_LK },
        );
        o.scenario(&format!("lo lock after {after}"), Dev::new(b), |d| {
            rfk1::set_channel_rf(&mut d.b, &mut d.st, Some(&ch("2g", 11, 20)), 0)?;
            Ok(vec![])
        });
    }
    {
        let mut d = Dev::new(RfFake::fresh());
        d.chan = Some(ch("2g", 6, 20));
        o.scenario("iqk again", d, |d| {
            rfk1::iqk(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks, 0)?;
            rfk1::iqk(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks, 0)?;
            d.st.iqk.iqk_times = 0xfe;
            rfk1::iqk(&mut d.b, &mut d.st, d.chan.as_ref(), &mut d.hooks, 0)?;
            Ok(vec![])
        });
    }
    o.0
}

#[test]
fn rfk1_equals_the_js_module() {
    if skip("rfk1_equals_the_js_module") {
        return;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/rfk1-js.mjs");
    let res = Command::new("node")
        .arg(&script)
        .env("WLAN_TOOLS", wlan_dir())
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        res.status.success(),
        "rfk1-js.mjs: {}\n{}",
        res.status,
        String::from_utf8_lossy(&res.stderr)
    );
    let js: Vec<String> = String::from_utf8(res.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let ours = rust_side();
    let scenarios = js.iter().filter(|l| l.starts_with("## ")).count();
    assert_eq!(scenarios, 48, "the JS side's scenarios");
    let mut scen = String::new();
    for (i, (a, j)) in ours.iter().zip(&js).enumerate() {
        if j.starts_with("## ") {
            scen = j.clone();
        }
        assert_eq!(
            a,
            j,
            "line {} ({scen}) differs: Rust {a:?}, JS {j:?}\n  Rust before: {:?}",
            i + 1,
            &ours[i.saturating_sub(4)..i]
        );
    }
    assert_eq!(ours.len(), js.len(), "line count (Rust vs JS)");
    // the oracle really ran the calibrations
    assert!(js.len() > 100_000, "{} lines", js.len());
}
