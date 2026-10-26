//! The rfk2 module against the JS driver's own rfk2.mjs (rfk2-common/-dpk/-dpkflow/-tssi/-alimk/
//! -trk/-tables/-trkdata.mjs), which test-rfk2.mjs checks op by op against the Linux C itself
//! (rfk2-crun.mjs runs rtw8852b_rfk.c's functions): tests/fake/rfk2-js.mjs runs the JS side of
//! test-rfk2.mjs's scenarios on its chip model (rfk2-tchip.mjs makeChip: the real efuse dump and
//! firmware file) with its stand-ins of btc and macinit, and prints every op, every neighbour call
//! where it happened, the results and the whole rfk2 state, plus the efuse values and TXPWR_TRK
//! tables it starts from. The same scenarios run here on `rfk1::fake::RfFake` set up as makeChip,
//! with stubs that do what the JS stubs do, and the two texts must be equal line by line. Needs
//! node and the read-only WLAN tree; RTW89_SKIP_FAKE=1 skips (with a line on stderr), but never
//! with BITSOS_GATE or CI set.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use rtw89_core::bus::{Bus, Width, field, lowbit, prep};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::Firmware;
use rtw89_core::phy::bbx::{
    TssiBak, bb_cfg_tx_path, bb_restore_tssi, bb_set_pmac_pkt_tx, bb_set_power, bb_tx_mode_switch,
    ctrl_rx_path,
};
use rtw89_core::phy::efuse::{Efuse, read_efuse_8852bx};
use rtw89_core::phy::io::{BB, RFREG_MASK, rf_w};
use rtw89_core::phy::{Band, Chan, Phy};
use rtw89_core::rfk1::fake::{Key, RfFake};
use rtw89_core::rfk1::util::chan_get;
use rtw89_core::rfk2::trk::build_from_elm;
use rtw89_core::rfk2::{self, Rfk2Hooks, Rfk2State, dpk as dp, dpkflow, poll_soft, tssi as ts};
use rtw89_core::tables::regs::*;
use rtw89_core::tables::trk::TRK_CFG;

fn skip(test: &str) -> bool {
    use std::io::Write;
    if std::env::var_os("RTW89_SKIP_FAKE").is_some_and(|v| v == "1") {
        for gate in ["BITSOS_GATE", "CI"] {
            assert!(
                std::env::var_os(gate).is_none_or(|v| v.is_empty() || v == "0"),
                "RTW89_SKIP_FAKE=1 together with {gate}: the gate must run the JS oracle"
            );
        }
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS rfk2.mjs)"
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
fn ld<T: std::fmt::Display>(a: &[T]) -> String {
    a.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
fn lb(a: &[bool]) -> String {
    a.iter().map(|&v| b01(v)).collect::<Vec<_>>().join(",")
}
fn ll<const N: usize>(a: &[[u32; N]]) -> String {
    a.iter().map(|x| l(x)).collect::<Vec<_>>().join(";")
}
fn lld<T: std::fmt::Display, const N: usize>(a: &[[T; N]]) -> String {
    a.iter().map(|x| ld(x)).collect::<Vec<_>>().join(";")
}

/// stubs/rfk2-btc.mjs and stubs/rfk2-macinit.mjs in Rust: each call is noted with its position
#[derive(Default)]
struct Stubs {
    calls: Vec<(usize, String)>,
}

impl Stubs {
    fn log(&mut self, bus: &RfFake, s: String) {
        self.calls.push((bus.trace.len(), format!("= call {s}")));
    }
}

impl Rfk2Hooks<RfFake> for Stubs {
    fn btc_wl_rfk(
        &mut self,
        bus: &mut RfFake,
        phy_map: u32,
        typ: u32,
        state: u32,
    ) -> Result<(), Rtw89Error> {
        self.log(
            bus,
            format!("btcWlRfk {} {} {}", h(phy_map), h(typ), h(state)),
        );
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut RfFake) -> Result<u16, Rtw89Error> {
        self.log(bus, "stopSchTx all".into());
        let tx_en = bus.r16(R_AX_CTN_TXEN)?;
        bus.mask(Width::W16, R_AX_CTN_TXEN, B_AX_CTN_TXEN_ALL_MASK, 0)?;
        Ok(tx_en)
    }
    fn resume_sch_tx(&mut self, bus: &mut RfFake, tx_en: u16) -> Result<(), Rtw89Error> {
        self.log(bus, format!("resumeSchTx {}", h(tx_en.into())));
        let m = B_AX_CTN_TXEN_ALL_MASK;
        bus.mask(
            Width::W16,
            R_AX_CTN_TXEN,
            m,
            (u32::from(tx_en) & m) >> lowbit(m),
        )?;
        Ok(())
    }
}

/// One device of a scenario: the chip, the phy part rfk2 reads, the rfk2 state, the channel
struct Dev {
    b: RfFake,
    phy: Phy,
    st: Rfk2State,
    hooks: Stubs,
    chan: Option<Chan>,
    /// rfk2-tchip.mjs `log.txenAtPmac`
    txen: Rc<RefCell<Vec<u32>>>,
}

fn ch(band: &str, ch: u32, bw: u32) -> Chan {
    Chan {
        band: match band {
            "5g" => Band::B5g,
            "6g" => Band::B6g,
            _ => Band::B2g,
        },
        ch,
        bw,
    }
}

/// rfk2-tchip.mjs makeChip's options
struct Opts {
    cw_ready: bool,
    fail_poll: u32,
    chan: Chan,
    efuse: Option<Efuse>,
    corr: u32,
    dgain: u32,
    txagc_bb: u32,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            cw_ready: true,
            fail_poll: 0,
            chan: ch("2g", 1, 20),
            efuse: None,
            corr: 200,
            dgain: 0x600,
            txagc_bb: 0x00ff_0020,
        }
    }
}

struct Inputs {
    fw: Firmware,
    efuse: Efuse,
}

/// rfk2-tchip.mjs makeChip on RfFake
fn make_chip(inp: &Inputs, o: Opts) -> Dev {
    let mut b = RfFake::new();
    b.rf.insert((0, RR_TM), 0x1c << 1); // thermal 0x1c / 0x1d
    b.rf.insert((1, RR_TM), 0x1d << 1);
    let txen = Rc::new(RefCell::new(Vec::new()));
    let log = txen.clone();
    b.answer_fn(Key::Bb(R_PMAC_TX_PRD), move |r| {
        log.borrow_mut().push(r.get(R_AX_CTN_TXEN, Width::W16));
        r.bb(R_PMAC_TX_PRD)
    });
    let (corr, dgain) = (o.corr, o.dgain);
    let mut gl_reads = 0u32;
    b.answer_fn(Key::Bb(R_RPT_COM), move |r| {
        let sel = field(r.bb(R_KIP_RPT1), B_KIP_RPT1_SEL);
        match sel {
            0 => (dgain << 16) | (corr << 8) | 5, // dgain, corr at idx 5
            9 => (10 << 16) | 0xff0,              // DC I 10, DC Q -16
            6 if field(r.bb(R_DPK_CFG2), B_DPK_CFG2_ST) != 0 => {
                gl_reads += 1;
                if gl_reads % 2 == 1 { 0x00 } else { 0x30 }
            }
            6 => {
                if field(r.bb(R_DPK_CFG3), MASKBYTE3) != 0 {
                    0x0040_0040
                } else {
                    0x0100_0100
                }
            }
            _ => 0,
        }
    });
    b.answer(Key::Bb(R_DPD_BF), (16 << 12) | 28); // ofdm backoff + scale = 44
    for a in [0x1c18u32, 0x3c18] {
        let ready = o.cw_ready;
        b.answer_fn(Key::Bb(a), move |r| {
            let pwr = field(r.bb(R_TXPWR), B_TXPWR_MSK);
            if ready {
                (1 << 16) | 0x100u32.wrapping_sub(pwr)
            } else {
                0
            }
        });
    }
    b.answer(Key::Bb(R_TXAGC_BB), o.txagc_bb);
    b.answer(Key::Bb(R_TXAGC_BB + 0x2000), o.txagc_bb);
    if o.fail_poll != 0 {
        b.fail_polls.insert(BB + o.fail_poll);
    }
    b.poke(R_AX_CTN_TXEN, Width::W16, 0xffff); // MAC TX scheduler on
    b.poke(BB + 0x813c, Width::W32, 0x1122_3344); // KIP registers DPK backs up
    b.poke(BB + 0x8124, Width::W32, 0x5566_7788);
    let mut phy = Phy::new(1);
    phy.efuse = Some(o.efuse.unwrap_or_else(|| inp.efuse.clone()));
    let mut st = Rfk2State::new();
    st.load_txpwr_trk(&inp.fw, 0)
        .expect("TXPWR_TRK of fw-2.bin");
    Dev {
        b,
        phy,
        st,
        hooks: Stubs::default(),
        chan: Some(o.chan),
        txen,
    }
}

/// A plain chip (JS `new FakeDev`) with no efuse
fn plain(b: RfFake) -> Dev {
    Dev {
        b,
        phy: Phy::new(1),
        st: Rfk2State::new(),
        hooks: Stubs::default(),
        chan: None,
        txen: Rc::default(),
    }
}

/// rfk2-js.mjs `stateLines`
fn state_lines(d: &Dev) -> Vec<String> {
    let (k, t) = (&d.st.dpk, &d.st.tssi);
    let mut bych = Vec::new();
    for p in 0..2 {
        for (i, &c) in t.check_backup_aligmk[p].iter().enumerate() {
            let v = t.alignment_backup_by_ch[p][i];
            if c || v.iter().any(|&x| x != 0) {
                bych.push(format!("{p}:{i}:{}:{}", b01(c), l(&v)));
            }
        }
    }
    let bp =
        k.bp.iter()
            .map(|p| {
                p.iter()
                    .map(|b| {
                        [
                            b.band,
                            b.bw,
                            b.ch,
                            b.path_ok.into(),
                            b.txagc_dpk,
                            b.ther_dpk,
                            b.gs,
                            b.pwsf,
                        ]
                        .iter()
                        .map(|&v| h(v))
                        .collect::<Vec<_>>()
                        .join("/")
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(";");
    vec![
        format!(
            "= tssi mode {} timeouts {}",
            lb(&d.phy.tssi_mode),
            d.st.poll_timeouts
        ),
        format!(
            "= dpk en {} reload {} gs {} cur {} corr {} idx {} dc {} {}",
            b01(k.is_dpk_enable),
            b01(k.is_dpk_reload_en),
            l(&k.dpk_gs),
            l(&k.cur_idx),
            ll(&k.corr_val),
            ll(&k.corr_idx),
            ll(&k.dc_i),
            ll(&k.dc_q)
        ),
        format!("= dpk bp {bp}"),
        format!(
            "= tssi done {}",
            t.alignment_done
                .iter()
                .map(|x| lb(x))
                .collect::<Vec<_>>()
                .join(";")
        ),
        format!(
            "= tssi value {}",
            t.alignment_value
                .iter()
                .map(|x| x.iter().map(|v| l(v)).collect::<Vec<_>>().join("|"))
                .collect::<Vec<_>>()
                .join(";")
        ),
        format!("= tssi bych {}", bych.join(" ")),
    ]
}

struct Out(Vec<String>);

impl Out {
    fn scenario(
        &mut self,
        name: &str,
        d: &mut Dev,
        f: impl FnOnce(&mut Dev) -> Result<Vec<String>, Rtw89Error>,
    ) {
        self.0.push(format!("## {name}"));
        d.b.sim_us = 0;
        let t0 = d.b.trace.len();
        d.hooks.calls.clear();
        let res = match f(d) {
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
        self.0.extend(state_lines(d));
    }
}

// a register value as the chip may hold it (no trace op)
fn poke(d: &mut Dev, a: u32, m: u32, v: u32) {
    let old = d.b.regs.get(BB + a, Width::W32);
    d.b.poke(BB + a, Width::W32, (old & !m) | prep(m, v));
}

fn custom_efuse(base: &Efuse) -> Efuse {
    let mut e = base.clone();
    for i in 0..19 {
        e.tssi.tssi_mcs[0][i] = (i as i32 * 5 - 40) as i8;
        e.tssi.tssi_mcs[1][i] = (37 - i as i32 * 4) as i8;
    }
    e.tssi.tssi_cck = [[1, 2, 3, 4, 5, 6], [-1, -2, -3, -4, -5, -6]];
    e.tssi.tssi_trim = [
        [1, -2, 3, -4, 5, -6, 7, -8],
        [-9, 10, -11, 12, -13, 14, -15, 16],
    ];
    e
}

fn rust_side() -> Vec<String> {
    let wz = wlan_dir();
    let fw_file = std::fs::read(wz.join("firmware/rtw8852b_fw-2.bin")).expect("fw-2.bin");
    let fw = Firmware::parse(fw_file, 1, false).expect("rtw89_fw_recognize");
    let log = std::fs::read(wz.join("kit/efuse-log.bin")).expect("efuse-log.bin");
    let mut efuse = Efuse::default();
    read_efuse_8852bx(&log, &mut efuse);
    let inp = Inputs { fw, efuse };
    let mut o = Out(Vec::new());

    // ---- inputs ----
    o.0.push("## inputs".into());
    let (t, g) = (&inp.efuse.tssi, &inp.efuse.gain);
    o.0.push(format!(
        "= efuse thermal {} cck {} mcs {} trim {}",
        l(&t.thermal.map(u32::from)),
        lld(&t.tssi_cck),
        lld(&t.tssi_mcs),
        lld(&t.tssi_trim)
    ));
    let five = |a: &[i8]| ld(&a[..5]);
    o.0.push(format!(
        "= gain {} {} offset {};{} comp {};{} base {} rssi {}",
        b01(g.offset_valid),
        b01(g.comp_valid),
        five(&g.offset[0]),
        five(&g.offset[1]),
        five(&g.comp[0]),
        five(&g.comp[1]),
        ld(&g.offset_base),
        ld(&g.rssi_base)
    ));
    let elm = inp
        .fw
        .element(RTW89_FW_ELEMENT_ID_TXPWR_TRK)
        .expect("TXPWR_TRK element");
    o.0.push(format!("= trk elm {} {}", elm.size, elm.aid));
    let payload = inp.fw.payload(elm);
    let built = build_from_elm(payload, elm.size, elm.aid, 0)
        .unwrap()
        .unwrap();
    let rows = |r: &[[i8; 30]]| r.iter().map(|x| ld(x)).collect::<Vec<_>>().join(";");
    for (typ, d) in built.delta.iter().enumerate() {
        o.0.push(format!("= trk {typ} {}", rows(d)));
    }
    for (typ, d) in TRK_CFG.iter().enumerate() {
        o.0.push(format!("= trkcfg {typ} {}", rows(d.unwrap_or(&[]))));
    }
    let other = build_from_elm(payload, elm.size, 0x1234, 0).unwrap();
    o.0.push(format!(
        "= trk other aid {}",
        if other.is_none() { "ignored" } else { "taken" }
    ));
    let zeros = vec![0u8; 8 + 600];
    let r = build_from_elm(&zeros, elm.size, elm.aid, 0);
    o.0.push(format!(
        "= trk no bitmap {}",
        if r.is_err() { "refused" } else { "taken" }
    ));

    // ---- single functions ----
    let mut d = make_chip(&inp, Opts::default());
    o.scenario("one-shot", &mut d, |d| {
        dp::dpk_one_shot(&mut d.b, &mut d.st, 0, 1, 0x10)?;
        Ok(vec![])
    });
    let mut d = make_chip(&inp, Opts::default());
    o.scenario("dgain mapping", &mut d, |_| {
        let v: Vec<i32> = (0..0x1000).map(dp::dpk_dgain_mapping).collect();
        Ok(vec![format!("= dgain {}", ld(&v))])
    });
    let mut d = make_chip(
        &inp,
        Opts {
            efuse: Some(custom_efuse(&inp.efuse)),
            ..Opts::default()
        },
    );
    o.scenario("efuse to de", &mut d, |d| {
        for (band, lo, hi) in [("2g", 1, 14), ("5g", 36, 177)] {
            for c in lo..=hi {
                let rc = chan_get(Some(&ch(band, c, 20)))?;
                ts::tssi_set_efuse_to_de(&mut d.b, &d.phy, &rc)?;
            }
        }
        Ok(vec![])
    });
    for thermal in [inp.efuse.tssi.thermal, [0xff, 0xff]] {
        let mut e = inp.efuse.clone();
        e.tssi.thermal = thermal;
        let mut d = make_chip(
            &inp,
            Opts {
                efuse: Some(e),
                ..Opts::default()
            },
        );
        let name = format!("tssi setup thermal {}", l(&thermal.map(u32::from)));
        o.scenario(&name, &mut d, |d| {
            for (band, c) in [
                ("2g", 1),
                ("2g", 14),
                ("5g", 36),
                ("5g", 64),
                ("5g", 100),
                ("5g", 144),
                ("5g", 149),
                ("5g", 177),
            ] {
                let rc = chan_get(Some(&ch(band, c, 20)))?;
                for path in [0, 1] {
                    ts::tssi_rf_setting(&mut d.b, path, &rc)?;
                    ts::tssi_set_sys(&mut d.b, path, &rc)?;
                    ts::tssi_set_tmeter_tbl(&mut d.b, &d.phy, &d.st, path, &rc)?;
                    for all in [true, false] {
                        ts::tssi_alignment_default(&mut d.b, path, all, &rc)?;
                    }
                }
            }
            Ok(vec![])
        });
    }
    {
        let mut e = inp.efuse.clone();
        e.gain.offset_valid = true;
        e.gain.comp_valid = true;
        e.gain.offset = [[0; 13]; 2];
        e.gain.comp = [[0; 13]; 2];
        e.gain.offset[0][..5].copy_from_slice(&[1, -2, 3, -4, 5]);
        e.gain.offset[1][..5].copy_from_slice(&[-3, 4, -5, 6, -7]);
        e.gain.comp[0][..5].copy_from_slice(&[1, -2, 0, 3, -4]);
        e.gain.comp[1][..5].copy_from_slice(&[2, 0, -1, 1, 5]);
        e.gain.offset_base = [-12, 0];
        e.gain.rssi_base = [16, 0];
        let mut d = make_chip(
            &inp,
            Opts {
                efuse: Some(e),
                ..Opts::default()
            },
        );
        o.scenario("dpk steps and bb helpers", &mut d, |d| {
            for gs in [0x7f, 0x5b] {
                d.st.dpk.dpk_gs[0] = gs;
                for (path, kidx, g) in [(0, 0, 1), (1, 1, 1), (1, 0, 0)] {
                    dp::dpk_fill_result(&mut d.b, &mut d.st, 0, path, kidx, g, 0x32)?;
                }
            }
            for (band, c, bw) in [("2g", 6, 20), ("5g", 36, 40), ("5g", 42, 80)] {
                let chan = ch(band, c, bw);
                let rc = chan_get(Some(&chan))?;
                for path in [0u32, 1] {
                    let bp = &mut d.st.dpk.bp[path as usize][1];
                    bp.band = rc.band_type;
                    bp.bw = rc.band_width;
                    dp::dpk_rf_setting(&mut d.b, &mut d.st, 1, path, 1)?;
                    dp::dpk_kip_pwr_clk_on(&mut d.b, path)?;
                    dp::dpk_kip_restore(&mut d.b, path)?;
                }
                dp::dpk_bb_afe_setting(&mut d.b, &rc)?;
                dp::dpk_bb_afe_restore(&mut d.b, &rc)?;
                for enable in [true, false] {
                    bb_set_pmac_pkt_tx(&mut d.b, enable, 100, 5000, chan.band)?;
                }
                for antenna_rx in [0, RF_B] {
                    d.phy.hal.antenna_rx = antenna_rx;
                    let gain = d.phy.efuse.as_ref().unwrap().gain.clone();
                    for rx in [RF_A, RF_B, RF_AB, RF_ABCD] {
                        ctrl_rx_path(&mut d.b, rx, chan.band, rc.subband_type, &gain, antenna_rx)?;
                    }
                }
            }
            for p in [RF_PATH_A, RF_PATH_B, RF_PATH_AB, RF_PATH_ABCD] {
                bb_cfg_tx_path(&mut d.b, p)?;
            }
            for mode in [0, 1] {
                bb_tx_mode_switch(&mut d.b, mode)?;
            }
            bb_set_power(&mut d.b, 48)?;
            for tx_path in [RF_AB, RF_A] {
                let bak = TssiBak {
                    tx_path,
                    rx_path: 2,
                    p0_rfmode: 0x1234,
                    p0_rfmode_ftm: 0x5678,
                    p1_rfmode: 0x9abc,
                    p1_rfmode_ftm: 0xdef0,
                    tx_pwr: -3,
                };
                bb_restore_tssi(&mut d.b, &bak)?;
            }
            ts::tssi_enable(&mut d.b, &mut d.phy)?;
            ts::tssi_disable(&mut d.b, &mut d.phy)?;
            Ok(vec![])
        });
    }

    // ---- whole calibrations, step by step ----
    fn dpk_init(d: &mut Dev) -> Result<Vec<String>, Rtw89Error> {
        rfk2::dpk_init(&mut d.b, &mut d.phy, &mut d.st)?;
        Ok(vec![])
    }
    fn tssi(d: &mut Dev, hwtx: bool) -> Result<Vec<String>, Rtw89Error> {
        let c = d.chan;
        rfk2::tssi(
            &mut d.b,
            &mut d.phy,
            &mut d.st,
            c.as_ref(),
            &mut d.hooks,
            0,
            hwtx,
        )?;
        Ok(vec![])
    }
    fn dpk(d: &mut Dev) -> Result<Vec<String>, Rtw89Error> {
        let c = d.chan;
        rfk2::dpk(&mut d.b, &mut d.phy, &mut d.st, c.as_ref(), &mut d.hooks, 0)?;
        Ok(vec![])
    }
    fn track(d: &mut Dev) -> Result<Vec<String>, Rtw89Error> {
        rfk2::dpk_track(&mut d.b, &mut d.phy, &mut d.st)?;
        Ok(vec![])
    }
    fn scan(d: &mut Dev, start: bool) -> Result<Vec<String>, Rtw89Error> {
        let c = d.chan;
        rfk2::wifi_scan_notify(
            &mut d.b,
            &mut d.phy,
            &mut d.st,
            c.as_ref(),
            &mut d.hooks,
            start,
            0,
        )?;
        Ok(vec![])
    }
    fn tssi_scan(d: &mut Dev) -> Result<Vec<String>, Rtw89Error> {
        let c = d.chan;
        rfk2::tssi_scan(&mut d.b, &mut d.phy, &mut d.st, c.as_ref(), 0)?;
        Ok(vec![])
    }
    let tagf = |c: &Chan| {
        format!(
            "{} ch {} bw {}",
            if c.band == Band::B5g { "5g" } else { "2g" },
            c.ch,
            c.bw
        )
    };
    for (chan, next) in [
        (ch("2g", 1, 20), ch("5g", 36, 20)),
        (ch("5g", 42, 80), ch("5g", 151, 40)),
        (ch("2g", 13, 40), ch("5g", 100, 20)),
    ] {
        let mut d = make_chip(
            &inp,
            Opts {
                chan,
                txagc_bb: 0x05fe_0021,
                ..Opts::default()
            },
        );
        let tag = tagf(&chan);
        let ntag = format!(
            "{} ch {}",
            if next.band == Band::B5g { "5g" } else { "2g" },
            next.ch
        );
        o.scenario(&format!("dpkInit {tag}"), &mut d, dpk_init);
        o.scenario(&format!("tssi {tag}"), &mut d, |d| tssi(d, true));
        o.scenario(&format!("dpk {tag}"), &mut d, dpk);
        for (th, txdpd, com_of, dis) in [
            ([0x1a, 0x1a], 0, 1, 0),
            ([0x24, 0x10], 1, 1, 0),
            ([0x24, 0x10], 1, 0, 0),
            ([0x10, 0x24], 0, 0, 0),
            ([0, 0], 0, 0, 0),
            ([0x30, 0x08], 0, 1, 1),
        ] {
            d.phy.avg_thermal = th;
            for path in [0u32, 1] {
                poke(&mut d, R_P0_TXDPD + (path << 13), B_P0_TXDPD, txdpd);
                poke(&mut d, R_DPD_COM + (path << 8), B_DPD_COM_OF, com_of);
            }
            poke(&mut d, R_DPK_TRK, B_DPK_TRK_DIS, dis);
            let name = format!(
                "dpkTrack {tag} thermal {} txdpd {txdpd} com_of {com_of} dis {dis}",
                l(&th)
            );
            o.scenario(&name, &mut d, track);
        }
        o.scenario(&format!("scan start {tag}"), &mut d, |d| scan(d, true));
        d.chan = Some(next);
        o.scenario(&format!("tssiScan to {ntag}"), &mut d, tssi_scan);
        d.chan = Some(chan);
        o.scenario(&format!("tssiScan back to {tag}"), &mut d, tssi_scan);
        o.scenario(&format!("scan end {tag}"), &mut d, |d| scan(d, false));
        d.chan = Some(next);
        o.scenario(&format!("tssi on {ntag}"), &mut d, |d| tssi(d, true));
        o.scenario(&format!("dpk on {ntag}"), &mut d, dpk);
        o.scenario(&format!("track on {ntag}"), &mut d, track);
        d.chan = Some(chan);
        o.scenario(&format!("tssi again on {tag}"), &mut d, |d| tssi(d, true));
        o.scenario(&format!("dpk again on {tag}"), &mut d, dpk);
    }

    // ---- the paths Linux only logs ----
    let mut d = make_chip(
        &inp,
        Opts {
            cw_ready: false,
            ..Opts::default()
        },
    );
    o.scenario("tssi without a TSSI report", &mut d, |d| tssi(d, true));
    let mut d = make_chip(
        &inp,
        Opts {
            fail_poll: 0xbff8,
            ..Opts::default()
        },
    );
    o.scenario("dpkInit (one-shots fail)", &mut d, dpk_init);
    o.scenario("dpk with one-shot timeouts", &mut d, dpk);
    let mut d = make_chip(
        &inp,
        Opts {
            chan: ch("5g", 36, 20),
            ..Opts::default()
        },
    );
    o.scenario("tssi without hw tx", &mut d, |d| tssi(d, false));
    for (corr, dgain) in [(150, 0x600), (200, 0x7d0), (200, 0x100), (180, 0xbf0)] {
        let mut d = make_chip(
            &inp,
            Opts {
                corr,
                dgain,
                ..Opts::default()
            },
        );
        o.scenario(
            &format!("dpk with corr {corr} dgain {}", h(dgain)),
            &mut d,
            dpk,
        );
    }
    let mut d = make_chip(
        &inp,
        Opts {
            chan: ch("5g", 149, 20),
            ..Opts::default()
        },
    );
    o.scenario("dpkInit, no TSSI", &mut d, dpk_init);
    o.scenario("dpk without TSSI", &mut d, dpk);
    for th in [[0x30, 0x08], [0x08, 0x30]] {
        d.phy.avg_thermal = th;
        o.scenario(
            &format!("dpkTrack without TSSI, thermal {}", l(&th)),
            &mut d,
            track,
        );
    }
    let mut d = make_chip(&inp, Opts::default());
    o.scenario("scan start with TSSI off (runs tssi)", &mut d, |d| {
        scan(d, true)
    });

    // the TX AGC search at its ends
    for (name, gl_high) in [
        ("dpk with gain loss high", true),
        ("dpk with gain loss low", false),
    ] {
        let mut d = make_chip(&inp, Opts::default());
        d.b.answer_fn(Key::Bb(R_RPT_COM), move |r| {
            match field(r.bb(R_KIP_RPT1), B_KIP_RPT1_SEL) {
                0 => (0x600 << 16) | (200 << 8) | 5,
                9 => (10 << 16) | 0xff0,
                6 if field(r.bb(R_DPK_CFG2), B_DPK_CFG2_ST) != 0 => {
                    if gl_high {
                        0x70
                    } else {
                        0x00
                    }
                }
                6 if field(r.bb(R_DPK_CFG3), MASKBYTE3) != 0 => 0x0100_0100,
                6 => 0x0040_0040,
                _ => 0,
            }
        });
        o.scenario(name, &mut d, dpk);
    }

    // the boundaries of the AGC search and the tracking (rfk2-js.mjs's own)
    for dgain in [0x555, 0x556, 0x4c1, 0x262, 0x220, 342, 341, 1922, 1923] {
        let mut d = make_chip(
            &inp,
            Opts {
                chan: ch("5g", 42, 80),
                dgain,
                ..Opts::default()
            },
        );
        o.scenario(&format!("dpk 80 MHz dgain {}", h(dgain)), &mut d, dpk);
    }
    let mut d = make_chip(
        &inp,
        Opts {
            corr: 170,
            ..Opts::default()
        },
    );
    o.scenario("dpk with corr 170", &mut d, dpk);
    let mut d = make_chip(&inp, Opts::default());
    d.b.answer_fn(Key::Bb(R_RPT_COM), |r| {
        match field(r.bb(R_KIP_RPT1), B_KIP_RPT1_SEL) {
            0 => (0x600 << 16) | (200 << 8) | 5,
            9 => (10 << 16) | 0xff0,
            6 if field(r.bb(R_DPK_CFG2), B_DPK_CFG2_ST) != 0 => 0x00,
            6 if field(r.bb(R_DPK_CFG3), MASKBYTE3) != 0 => 0x0005_0000,
            6 => 0x0006_0002, // 6^2 + 2^2 == 5^2 * 8 / 5
            _ => 0,
        }
    });
    o.scenario("dpk with the PAS check at equality", &mut d, dpk);
    let mut d = make_chip(&inp, Opts::default());
    d.phy.avg_thermal = [0x1a, 0x1a];
    o.scenario("dpkTrack before any DPK", &mut d, track);
    let mut d = make_chip(
        &inp,
        Opts {
            txagc_bb: 0x05fe_0040,
            ..Opts::default()
        },
    );
    o.scenario("dpkInit, TX AGC RF 0", &mut d, dpk_init);
    o.scenario("tssi, TX AGC RF 0", &mut d, |d| tssi(d, true));
    o.scenario("dpk, TX AGC RF 0", &mut d, dpk);
    d.phy.avg_thermal = [0x24, 0x10];
    o.scenario("dpkTrack, TX AGC RF 0", &mut d, track);
    d.b.answer(Key::Bb(R_TXAGC_BB + 0x2000), 0x05fe_0021); // path B's TX AGC RF field back on
    d.phy.avg_thermal = [0x24, 0]; // path B uses path A's ini_diff
    o.scenario("dpkTrack, TX AGC RF 0 on path A only", &mut d, track);
    let mut d = make_chip(&inp, Opts::default());
    d.phy.tssi_mode = [false, true];
    o.scenario("scan start with TSSI on path B only", &mut d, |d| {
        scan(d, true)
    });
    {
        let mut e = inp.efuse.clone();
        e.gain.offset_valid = true;
        e.gain.comp_valid = true;
        e.gain.offset = [[0; 13]; 2];
        e.gain.comp = [[0; 13]; 2];
        e.gain.offset[0][..5].copy_from_slice(&[-8, 7, -8, 7, -8]);
        e.gain.offset[1][..5].copy_from_slice(&[7, -8, 7, -8, 7]);
        e.gain.comp[0][..5].copy_from_slice(&[7, -8, 7, -8, 7]);
        e.gain.comp[1][..5].copy_from_slice(&[-8, 7, -8, 7, -8]);
        e.gain.offset_base = [20, 0];
        e.gain.rssi_base = [-20, 0];
        let mut d = make_chip(
            &inp,
            Opts {
                efuse: Some(e),
                ..Opts::default()
            },
        );
        o.scenario("rx path with extreme gain offsets", &mut d, |d| {
            let gain = d.phy.efuse.as_ref().unwrap().gain.clone();
            for (band, c) in [("2g", 6), ("5g", 36), ("5g", 100), ("5g", 149)] {
                let chan = ch(band, c, 20);
                let rc = chan_get(Some(&chan))?;
                for antenna_rx in [0, RF_B] {
                    for rx in [RF_A, RF_B, RF_AB, RF_ABCD] {
                        ctrl_rx_path(&mut d.b, rx, chan.band, rc.subband_type, &gain, antenna_rx)?;
                    }
                }
            }
            Ok(vec![])
        });
    }

    // ---- the good chip run ----
    let mut d = make_chip(&inp, Opts::default());
    o.scenario("good run dpkInit", &mut d, dpk_init);
    o.scenario("good run tssi", &mut d, |d| {
        tssi(d, true)?;
        Ok(vec![format!("= txen at pmac {}", l(&d.txen.borrow()))])
    });
    o.scenario("good run dpk", &mut d, dpk);
    d.phy.avg_thermal = [0x1a, 0x1a];
    o.scenario("good run track", &mut d, track);

    // ---- RF mode stuck in TX, an external PA, the reload branch ----
    let mut d = make_chip(&inp, Opts::default());
    d.b.rf.insert((0, RR_MOD), prep(RR_MOD_MASK, 2));
    o.scenario("dpk with RF mode stuck in TX", &mut d, dpk);
    let mut d = make_chip(&inp, Opts::default());
    d.st.fem.epa_2g = true;
    o.scenario("dpk with an external 2 GHz PA", &mut d, dpk);
    let mut d = make_chip(&inp, Opts::default());
    o.scenario("dpk before reload", &mut d, dpk);
    d.st.dpk.is_dpk_reload_en = true;
    o.scenario("dpk reload same channel", &mut d, |d| {
        let rc = chan_get(d.chan.as_ref())?;
        dpkflow::dpk_run(&mut d.b, &d.phy, &mut d.st, 0, false, &rc)?;
        Ok(vec![])
    });
    d.chan = Some(ch("2g", 6, 20));
    o.scenario("dpk reload other channel", &mut d, |d| {
        let rc = chan_get(d.chan.as_ref())?;
        dpkflow::dpk_run(&mut d.b, &d.phy, &mut d.st, 0, false, &rc)?;
        Ok(vec![])
    });

    // ---- errors ----
    let mut d = make_chip(&inp, Opts::default());
    d.chan = Some(ch("6g", 1, 20));
    o.scenario("dpk on 6 GHz", &mut d, |d| {
        d.b.w32(0x1234, 1)?; // the caller's op
        dpk(d)
    });
    let mut b = RfFake::new();
    b.fail_polls.insert(BB + 0xbff8);
    b.fail_polls.insert(BB + R_SWSI_V1);
    let mut d = plain(b);
    o.scenario("soft poll timeout", &mut d, |d| {
        let ok = poll_soft(&mut d.b, &mut d.st, 0, 0xbff8, 0xff, 0x55, 20000)?;
        Ok(vec![format!("= soft {}", b01(ok))])
    });
    o.scenario("hard poll before a soft one", &mut d, |d| {
        rf_w(&mut d.b, 0, RR_MOD, RFREG_MASK, 0x50220)?;
        poll_soft(&mut d.b, &mut d.st, 0, 0x80fc, 0xffff, 0x8000, 2000)?;
        Ok(vec![])
    });
    o.0
}

#[test]
fn rfk2_equals_the_js_module() {
    if skip("rfk2_equals_the_js_module") {
        return;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/rfk2-js.mjs");
    let res = Command::new("node")
        .arg(&script)
        .env("WLAN_TOOLS", wlan_dir())
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        res.status.success(),
        "rfk2-js.mjs: {}\n{}",
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
    assert_eq!(scenarios, 107, "the JS side's scenarios");
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
    assert!(js.len() > 40_000, "{} lines", js.len());
}
