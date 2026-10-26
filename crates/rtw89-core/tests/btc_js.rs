//! The btc module against the JS driver's own btc.mjs (btc-run/-hw/-h2c/-c2h/-wlsta.mjs), which
//! test-btc.mjs checks against register sequences and H2C layouts written from the Linux C:
//! tests/fake/btc-js.mjs runs test-btc.mjs's scenarios (and more) on its device (FakeDev with the
//! CMAC on, rfe 1, cv 1, the real firmware's version) with fw.mjs's h2cSend as a stand-in that
//! notes every coex H2C (header and payload) where it happened, and prints every op, the H2Cs,
//! the results and the whole coexistence state. The same runs here on `rfk1::fake::RfFake` as
//! the FakeDev, and the two texts must be equal line by line. Needs node and the read-only WLAN
//! tree; RTW89_SKIP_FAKE=1 skips (with a line on stderr), but never with BITSOS_GATE or CI set.

use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use rtw89_core::btc::{
    self, BtcHooks, BtcState, ChipFacts, Packet, RoleReq, RoleState, WlStaLink, h2c, hw,
    state::{BtReport, Rlink, Traffic},
};
use rtw89_core::bus::Width;
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::{C2h, Firmware, H2c};
use rtw89_core::phy::io::BB;
use rtw89_core::phy::{Band, Chan};
use rtw89_core::rfk1::fake::{Key, RfFake};
use rtw89_core::rfk1::util::btc_phymap;
use rtw89_core::tables::regs::*;

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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS btc.mjs)"
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
fn ld<T: std::fmt::Display>(a: &[T]) -> String {
    a.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
fn ret_b(v: bool) -> Vec<String> {
    vec![format!("= ret {}", b01(v))]
}
fn ret_h(v: u32) -> Vec<String> {
    vec![format!("= ret {}", h(v))]
}

/// stubs/btc-fw.mjs in Rust: each coex H2C noted with its position, header and payload
#[derive(Default)]
struct Rec {
    calls: Vec<(usize, String)>,
}

impl BtcHooks<RfFake> for Rec {
    fn h2c(&mut self, bus: &mut RfFake, x: &H2c, payload: &[u8]) -> Result<(), Rtw89Error> {
        let hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
        self.calls.push((
            bus.trace.len(),
            format!(
                "= call h2c {} {} {} {} {} {hex}",
                x.cat,
                x.cls,
                x.func,
                b01(x.rack),
                b01(x.dack)
            ),
        ));
        Ok(())
    }
}

/// btc-js.mjs's `newDev`: the FakeDev with its chip facts and the btc state once powered on
struct Dev {
    bus: RfFake,
    st: Option<BtcState>,
    rfe: u32,
    code: u32,
    band: Band,
    ch: u32,
    port: u32,
    mac_id: u32,
}

impl Dev {
    fn new(cmac_on: bool, rfe: u32, code: u32) -> Dev {
        let mut bus = RfFake::new();
        if cmac_on {
            bus.answer(Key::Mac(R_AX_CMAC_FUNC_EN), B_AX_CMAC_EN);
        }
        Dev {
            bus,
            st: None,
            rfe,
            code,
            band: Band::B2g,
            ch: 1,
            port: 0,
            mac_id: 0,
        }
    }
    fn st(&mut self) -> &mut BtcState {
        self.st.as_mut().expect("btcPowerOn first")
    }
    fn req(&self) -> RoleReq {
        RoleReq {
            port: self.port,
            mac_id: self.mac_id,
            mode: 0,
            band: self.band,
            ch: self.ch,
            bw: RTW89_CHANNEL_WIDTH_20,
        }
    }
    fn chan(&self) -> Chan {
        Chan {
            band: self.band,
            ch: self.ch,
            bw: 20,
        }
    }
    // btcPowerOn: the state the first time (rtw89_coex_recognize_ver, rtw89_btc_init)
    fn power_on(&mut self) -> Result<(), Rtw89Error> {
        if self.st.is_none() {
            self.st = Some(BtcState::new(self.code)?);
        }
        btc::power_on(self.st());
        Ok(())
    }
}

/// Splits a Dev into the bus, the state and the H2C recorder for one call
macro_rules! run {
    ($d:expr, $rec:expr, |$b:ident, $s:ident| $body:expr) => {{
        let Dev { bus: $b, st, .. } = &mut *$d;
        let $s = st.as_mut().expect("btcPowerOn first");
        let _ = &$rec;
        $body
    }};
}

fn state_lines(st: Option<&BtcState>) -> Vec<String> {
    let Some(b) = st else {
        return vec!["= btc none".into()];
    };
    let (wl, bt0, dm, md, fw, ctrl) = (&b.wl, &b.bt0, &b.dm, &b.md, &b.fwinfo, &b.ctrl);
    let (s, r, k) = (&wl.status, &wl.role, &wl.rfk_info);
    let mut o = Vec::new();
    o.push(format!(
        "= wl scbd {} chg {} coex {} reqmac {} {} lchg {} bg {} ver {} {} hi {} go {} noa {} dbcc {} band {} scan {} {} rssi {}",
        h(wl.scbd),
        wl.scbd_chg,
        wl.coex_mode,
        wl.pta_req_mac,
        b01(wl.pta_reg_mac_chg),
        wl.link_mode_chg,
        wl.bg_mode,
        h(wl.ver_fw_coex),
        h(wl.ver_fw),
        wl.is_5g_hi_ch,
        wl.go_client_exist,
        wl.noa_exist,
        wl.dbcc_chg,
        ld(&wl.rf_band_map),
        ld(&wl.scan_band),
        wl.scan_phy_map,
        wl.rssi_level
    ));
    o.push(format!(
        "= st init {} rf {} {} lps {} {} busy {} scan {} tr {} 4way {} dhcp {} hs {} roam {} dir {}",
        b01(s.init_ok),
        s.rf_off,
        s.rf_off_pre,
        s.lps,
        s.lps_pre,
        s.busy,
        s.scan,
        s.transacting,
        s.four_way,
        s.dhcp,
        s.handshake,
        s.roaming,
        s.traffic_dir
    ));
    o.push(format!(
        "= rfk {} {} {} {} {} {}",
        k.state,
        k.typ,
        k.path_map,
        k.phy_map,
        k.band,
        b01(k.con_rfk)
    ));
    o.push(format!(
        "= role mode {} {} chg {} p2p {} req {} dbcc {} {} {} map {} mrole {} {} cnt {}",
        r.link_mode,
        r.link_mode_hb1,
        r.link_mode_chg,
        r.p2p_exist,
        r.pta_req_band,
        r.dbcc_en,
        r.dbcc_chg,
        r.dbcc_2g_phy,
        h(r.role_map),
        h(r.mrole_type),
        h(r.mrole_noa_duration),
        r.connect_cnt
    ));
    for (i, x) in r.rlink.iter().enumerate() {
        o.push(format!(
            "= rlink {i} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            x.connected,
            x.pid,
            x.phy,
            x.noa,
            x.rf_band,
            b01(x.active),
            x.bw,
            x.role,
            x.ch,
            h(x.noa_dur),
            x.client_cnt,
            x.mode,
            x.mac_id,
            x.client_ps
        ));
    }
    for (i, x) in wl.rlink_info.iter().enumerate() {
        let Some(x) = x else {
            o.push(format!("= info {i} -"));
            continue;
        };
        let tr = x.traffic.map_or("-".to_string(), |t| {
            format!(
                "{} {} {} {} {} {}",
                t.tx_tfc_lv, t.rx_tfc_lv, t.tx_throughput, t.rx_throughput, t.tx_rate, t.rx_rate
            )
        });
        o.push(format!(
            "= info {i} {} {} {} {} {} {} {} {} {} {} tr {tr} rssi {} st {} busy {} dir {} drop {}",
            x.mode,
            x.role,
            x.phy,
            x.pid,
            b01(x.active),
            x.band,
            x.ch,
            x.bw,
            x.mac_id,
            x.connected,
            x.rssi.map_or("-".into(), |v| v.to_string()),
            x.rssi_state.map_or("-".into(), |v| ld(&v)),
            x.busy,
            x.dir,
            x.rx_rate_drop_cnt
        ));
    }
    o.push(format!(
        "= bt0 {} {} patch {} scbd {} {} {} {} {} {} {} {} {} {} {} {} {} {} lowpri {} ext {}",
        bt0.enable_now,
        bt0.enable_last,
        bt0.run_patch_code,
        h(bt0.scbd_c2h),
        h(bt0.scbd),
        bt0.mbx_avl,
        bt0.whql_test,
        bt0.btg_type,
        bt0.a2dp,
        bt0.pan,
        bt0.hfp,
        bt0.lna_constrain,
        bt0.hi_lna_rx,
        bt0.connect,
        bt0.link_cnt_now,
        bt0.info_updates,
        h(bt0.ver_fw),
        b01(bt0.scan_rx_low_pri),
        b.bt_ext_func_type
    ));
    o.push(format!(
        "= ctrl {} {} {} {} {}",
        ctrl.manual,
        ctrl.igno_bt,
        b01(ctrl.always_freerun),
        ctrl.trace_step,
        ctrl.fw_scan
    ));
    let gnt: Vec<String> = dm
        .gnt_set
        .iter()
        .map(|g| {
            format!(
                "{}{}{}{}",
                g.gnt_bt, g.gnt_bt_sw_en, g.gnt_wl, g.gnt_wl_sw_en
            )
        })
        .collect();
    o.push(format!(
        "= dm rsn {} act {} only {} {} inst {} lps {} {} ant {} gnt {} agc {} {} btg {} {} stb {} lna2 {} err {}{} cnt {} {}",
        dm.run_reason,
        dm.run_action,
        dm.wl_only,
        dm.bt_only,
        dm.tdma_instant_excute,
        b01(dm.lps_ctrl_scbd),
        b01(dm.lps_ctrl_change),
        h(dm.set_ant_path),
        gnt.join(","),
        dm.wl_pre_agc,
        dm.wl_pre_agc_rb,
        dm.wl_btg_rx,
        dm.wl_btg_rx_rb,
        dm.wl_stb_chg,
        dm.wl_lna2,
        b01(dm.error_tdma),
        b01(dm.error_slot),
        dm.cnt_tdma_nonsync,
        dm.cnt_slot_nonsync
    ));
    let bind = |x: &btc::state::Bind| {
        format!(
            "{}/{}/{}/{}/{}",
            x.rf_band, x.bt_sel, x.bt_link_weight, x.wl_hwb_sel, x.wl_link_mode
        )
    };
    o.push(format!(
        "= dm2 txpwr {} rxg {} lvl {} oob {} tdd {} fdd {} bind {} {} free {} fddt {} req {} {} pta {} leak {} sta {} {} offl {} txl {} {} {}",
        ld(&dm.wl_tx_power),
        ld(&dm.wl_rx_gain),
        dm.trx_para_level,
        b01(dm.out_of_band),
        b01(dm.tdd_en),
        b01(dm.fdd_en),
        bind(&dm.tdd_bind),
        bind(&dm.fdd_bind),
        b01(dm.freerun),
        dm.fddt_train,
        b01(dm.bt_req_en),
        ld(&dm.bt_req_len),
        b01(dm.pta_owner_warn),
        dm.leak_ap,
        dm.cnt_notify_wl_sta,
        dm.cnt_dm_wl_sta_last,
        dm.wl_fw_cx_offload,
        dm.tx_limit_enable,
        dm.tx_time,
        dm.tx_retry
    ));
    o.push(format!("= tdma {} now {}", ld(&dm.tdma), ld(&dm.tdma_now)));
    let slots = |a: &[rtw89_core::tables::BtcSlot]| {
        a.iter()
            .map(|s| format!("{}/{}/{}", s.dur, h(s.cxtbl), s.cxtype))
            .collect::<Vec<_>>()
            .join(" ")
    };
    o.push(format!("= slot {}", slots(&dm.slot)));
    o.push(format!("= slotnow {}", slots(&dm.slot_now)));
    let ii = &dm.init_info;
    o.push(format!(
        "= init {} {} {} {} {} {} {}",
        ii.init_mode,
        ii.wl_init_ok,
        ii.wl_only,
        ii.bt_only,
        ii.cx_other,
        ii.wl_guard_ch,
        ii.dbcc_en
    ));
    let a = &md.ant;
    o.push(format!(
        "= md {} {} {} {} {} {} {} ant {} {} {} {} {} {} {} type {} btg {}",
        md.rfe_type,
        md.kt_ver,
        md.bt_solo,
        md.bt0_sw_type,
        md.bt0_pos,
        md.wa_type,
        md.kt_ver_adie,
        a.typ,
        a.num,
        a.isolation,
        a.diversity,
        a.single_pos,
        a.btg_pos,
        a.stream_cnt,
        b.ant_type,
        b.btg_pos
    ));
    o.push(format!(
        "= fw rpt {} h2c {} fail {} policy {} force {}",
        h(fw.rpt_en_map),
        fw.cnt_h2c,
        fw.cnt_h2c_fail,
        h(b.policy_type),
        b01(b.update_policy_force)
    ));
    let t = &dm.trx_info;
    o.push(format!(
        "= trx {} {} {} {} {} {} {}",
        t.tx_lvl, t.rx_lvl, t.tx_rate, t.rx_rate, t.tx_tp, t.rx_tp, t.wl_rssi
    ));
    let reps: Vec<String> = b
        .bt_reports()
        .iter()
        .map(|x| match x {
            BtReport::Scbd { bid, scbd } => format!("scbd {bid} {}", h(*scbd)),
            BtReport::Info { bid, data } => {
                let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
                format!("info {bid} {hex}")
            }
        })
        .collect();
    o.push(format!(
        "= reports {}",
        if reps.is_empty() {
            "-".into()
        } else {
            reps.join(" ; ")
        }
    ));
    o
}

/// One scenario as btc-js.mjs's `scenario`: the ops, the H2Cs where they happened, the results
/// (or `! error`), the state
fn scenario(
    out: &mut Vec<String>,
    name: &str,
    d: &mut Dev,
    f: impl FnOnce(&mut Dev, &mut Rec) -> Result<Vec<String>, Rtw89Error>,
) {
    out.push(format!("## {name}"));
    let t0 = d.bus.trace.len();
    let mut rec = Rec::default();
    let res = match f(d, &mut rec) {
        Ok(r) => r,
        Err(e) => {
            if std::env::var_os("BTC_JS_VERBOSE").is_some() {
                eprintln!("{name}: {e:?}");
            }
            vec!["! error".into()]
        }
    };
    let ops = d.bus.lines()[t0..].to_vec();
    for i in 0..=ops.len() {
        for (pos, c) in &rec.calls {
            if pos - t0 == i {
                out.push(c.clone());
            }
        }
        if let Some(l) = ops.get(i) {
            out.push(l.clone());
        }
    }
    out.extend(res);
    out.extend(state_lines(d.st.as_ref()));
}

fn ver_code(v: [u8; 4]) -> u32 {
    u32::from_be_bytes(v)
}

fn stats(tx: u32, rx: u32, tx_tp: u32, rx_tp: u32) -> Traffic {
    Traffic {
        tx_tfc_lv: tx,
        rx_tfc_lv: rx,
        tx_throughput: tx_tp,
        rx_throughput: rx_tp,
        ..Traffic::default()
    }
}
fn link(avg_rssi: u32, s: Traffic, tx_hw_rate: u32, rx_hw_rate: u32) -> WlStaLink {
    WlStaLink {
        port: 0,
        avg_rssi,
        stats: s,
        tx_hw_rate,
        rx_hw_rate,
    }
}
fn c2h_of(cat: u32, cls: u32, func: u32, payload: Vec<u8>) -> C2h {
    C2h {
        cat,
        cls,
        func,
        len: payload.len() as u32 + 8,
        seq: None,
        payload,
        ack: None,
    }
}
fn rpt_of(typ: u8, c: &[u8], len: Option<u16>) -> Vec<u8> {
    let mut b = vec![typ];
    b.extend_from_slice(&len.unwrap_or(c.len() as u16).to_le_bytes());
    b.extend_from_slice(c);
    b
}
fn ctrl_rpt(en: u32, fwc: u32, fwv: u32) -> Vec<u8> {
    let mut c = vec![0u8; 80];
    c[0] = 5;
    c[24..28].copy_from_slice(&fwc.to_le_bytes());
    c[28..32].copy_from_slice(&fwv.to_le_bytes());
    c[32..36].copy_from_slice(&en.to_le_bytes());
    c
}
fn mreg_rpt(v4688: u32, v980: u32, num: u8) -> Vec<u8> {
    let mut c = vec![0u8; 84];
    c[0] = 2;
    c[1] = num;
    c[4 + 4 * 12..4 + 4 * 13].copy_from_slice(&v980.to_le_bytes());
    c[4 + 4 * 14..4 + 4 * 15].copy_from_slice(&v4688.to_le_bytes());
    c
}
fn tdma_rpt(t: &[u8]) -> Vec<u8> {
    let mut c = vec![3, 0, 0, 0];
    c.extend_from_slice(t);
    c
}

const RAW_CTRL: u8 = 0;
const RAW_TDMA: u8 = 1;
const RAW_SLOT: u8 = 2;
const RAW_CYSTA: u8 = 3;
const RAW_STEP: u8 = 4;
const RAW_MREG: u8 = 6;
const RAW_GPIO: u8 = 7;
const RAW_TEST: u8 = 12;
const RAW_BIG: u8 = 40;

fn rpt(d: &mut Dev, reports: &[Vec<u8>]) -> Vec<String> {
    let payload: Vec<u8> = reports.concat();
    ret_b(btc::c2h(d.st(), &c2h_of(2, 0x12, 0, payload)))
}

fn init_normal(d: &mut Dev, rec: &mut Rec) -> Result<(), Rtw89Error> {
    let rfe = d.rfe;
    run!(d, rec, |b, s| btc::init(
        b,
        s,
        rec,
        ChipFacts { rfe, cv: 1 },
        BTC_MODE_NORMAL
    ))
}

fn rust_side() -> Vec<String> {
    let mut out = Vec::new();
    let file = std::fs::read(wlan_dir().join("firmware/rtw8852b_fw-2.bin")).expect("firmware file");
    let fw = Firmware::parse(file, 1, false).expect("rtw89_fw_recognize");
    let v = fw.suit.ver;
    let code = fw.suit.ver.code;
    assert_eq!(code, ver_code([v.major, v.minor, v.sub, v.idx]));

    // ---- the bring-up's coexistence on one device, as test-btc.mjs ----
    let mut d = Dev::new(true, 1, code);
    out.push(format!("= fw {}.{}.{}.{}", v.major, v.minor, v.sub, v.idx));
    scenario(&mut out, "power on", &mut d, |d, _| {
        d.power_on().map(|_| vec![])
    });
    scenario(&mut out, "init", &mut d, |d, r| {
        init_normal(d, r).map(|_| vec![])
    });
    let pm = btc_phymap(Some(&d.chan()), 0, 0).unwrap();
    let rfk = |d: &mut Dev, r: &mut Rec, pm: u32, typ: u32, state: u32| {
        run!(d, r, |b, s| btc::role::wl_rfk(b, s, r, pm, typ, state)).map(ret_b)
    };
    scenario(&mut out, "dack start", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_DACK, BTC_WRFK_START)
    });
    scenario(&mut out, "dack stop", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_DACK, BTC_WRFK_STOP)
    });
    scenario(&mut out, "iqk one-shot", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_START)
    });
    scenario(&mut out, "rxdck one-shot", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_RXDCK, BTC_WRFK_ONESHOT_START)
    });
    scenario(&mut out, "dack start twice", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_DACK, BTC_WRFK_START)?;
        rfk(d, r, pm, BTC_WRFKT_DACK, BTC_WRFK_START)?;
        rfk(d, r, pm, BTC_WRFKT_DACK, BTC_WRFK_STOP).map(|_| vec![])
    });
    scenario(&mut out, "rfk state 7", &mut d, |d, r| {
        rfk(d, r, 0x13, BTC_WRFKT_DPK, 7)
    });
    scenario(&mut out, "conn rfk", &mut d, |d, _| {
        btc::role::conn_rfk(d.st(), true);
        let Dev { bus, st, .. } = d;
        btc::role::preserve_bt_time(bus, st.as_ref().unwrap(), 30)?;
        btc::role::conn_rfk(d.st(), false);
        Ok(vec![])
    });
    scenario(&mut out, "iqk start", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_IQK, BTC_WRFK_START).map(|_| vec![])
    });
    let swband = |d: &mut Dev, r: &mut Rec| {
        let band = d.band;
        run!(d, r, |b, s| btc::switch_band(b, s, r, band)).map(|_| vec![])
    };
    scenario(&mut out, "band switch inside iqk", &mut d, swband);
    scenario(&mut out, "iqk stop", &mut d, |d, r| {
        rfk(d, r, pm, BTC_WRFKT_IQK, BTC_WRFK_STOP).map(|_| vec![])
    });
    scenario(&mut out, "radio on", &mut d, |d, r| {
        run!(d, r, |b, s| btc::radio_on(b, s, r)).map(|_| vec![])
    });
    scenario(&mut out, "band switch", &mut d, swband);
    d.ch = 6;
    let scan = |d: &mut Dev, r: &mut Rec, start: bool| {
        let band = d.band;
        run!(d, r, |b, s| btc::scan(b, s, r, start, band)).map(|_| vec![])
    };
    scenario(&mut out, "scan start", &mut d, |d, r| scan(d, r, true));
    scenario(&mut out, "scan finish", &mut d, |d, r| scan(d, r, false));
    let role = |d: &mut Dev, r: &mut Rec, state: RoleState| {
        let req = d.req();
        run!(d, r, |b, s| btc::role::role(b, s, r, state, &req)).map(|_| vec![])
    };
    let connect = |d: &mut Dev, r: &mut Rec, start: bool| {
        let req = d.req();
        run!(d, r, |b, s| btc::role::connect(b, s, r, start, &req)).map(|_| vec![])
    };
    scenario(&mut out, "role start", &mut d, |d, r| {
        role(d, r, RoleState::Start)
    });
    scenario(&mut out, "connect start", &mut d, |d, r| {
        connect(d, r, true)
    });
    scenario(&mut out, "connect end", &mut d, |d, r| connect(d, r, false));
    let packet = |d: &mut Dev, r: &mut Rec, p: Packet| {
        run!(d, r, |b, s| btc::role::packet(b, s, r, p)).map(|_| vec![])
    };
    for (name, p) in [
        ("arp", Packet::Arp),
        ("icmp", Packet::Icmp),
        ("eapol", Packet::Eapol),
        ("eapolEnd", Packet::EapolEnd),
        ("dhcp", Packet::Dhcp),
    ] {
        scenario(&mut out, &format!("packet {name}"), &mut d, |d, r| {
            packet(d, r, p)
        });
    }
    let act1 =
        |d: &mut Dev, r: &mut Rec| run!(d, r, |b, s| btc::role::act1_work(b, s, r)).map(|_| vec![]);
    scenario(&mut out, "act1 work", &mut d, act1);
    scenario(&mut out, "act1 work again", &mut d, act1);
    let wl_sta = |d: &mut Dev, r: &mut Rec, l: Option<WlStaLink>| {
        run!(d, r, |b, s| btc::wlsta::wl_sta(b, s, r, l.as_ref())).map(|_| vec![])
    };
    scenario(&mut out, "wl_sta no link", &mut d, |d, r| {
        wl_sta(d, r, None)
    });
    scenario(&mut out, "wl_sta rssi 80 idle", &mut d, |d, r| {
        wl_sta(d, r, Some(link(160, stats(0, 0, 0, 0), 0, 0)))
    });
    scenario(&mut out, "wl_sta rssi 55 rx", &mut d, |d, r| {
        wl_sta(d, r, Some(link(110, stats(0, 2, 0, 900), 0, 0x10b)))
    });
    scenario(&mut out, "wl_sta rssi 55 rx again", &mut d, |d, r| {
        wl_sta(d, r, Some(link(111, stats(0, 2, 0, 950), 0, 0x10b)))
    });
    scenario(&mut out, "wl_sta rssi 35 tx", &mut d, |d, r| {
        wl_sta(d, r, Some(link(70, stats(3, 1, 4000, 10), 0, 0x10b)))
    });
    scenario(&mut out, "wl_sta rssi 41 tx", &mut d, |d, r| {
        wl_sta(d, r, Some(link(83, stats(3, 1, 4000, 10), 0, 0x10b)))
    });
    scenario(&mut out, "wl_sta rssi 42 tx", &mut d, |d, r| {
        wl_sta(d, r, Some(link(84, stats(3, 1, 4000, 10), 0, 0x10b)))
    });
    scenario(&mut out, "wl_sta rssi 43 idle", &mut d, |d, r| {
        wl_sta(d, r, Some(link(87, stats(0, 0, 0, 0), 0, 0)))
    });
    scenario(&mut out, "packet dhcp after traffic", &mut d, |d, r| {
        packet(d, r, Packet::Dhcp)
    });
    for i in 0..21 {
        scenario(&mut out, &format!("wl_sta quiet {i}"), &mut d, |d, r| {
            wl_sta(d, r, Some(link(87, stats(0, 0, 0, 0), 0, 0)))
        });
    }
    scenario(&mut out, "wl_sta other port", &mut d, |d, r| {
        let mut l = link(87, stats(1, 1, 0, 0), 0, 0);
        l.port = 3;
        wl_sta(d, r, Some(l))
    });
    scenario(&mut out, "role disconnect", &mut d, |d, r| {
        role(d, r, RoleState::Disconnect)
    });
    scenario(&mut out, "wl_sta after disconnect", &mut d, |d, r| {
        wl_sta(d, r, Some(link(87, stats(0, 1, 0, 0), 0, 0)))
    });
    scenario(&mut out, "role stop", &mut d, |d, r| {
        role(d, r, RoleState::Stop)
    });
    scenario(&mut out, "role port 6", &mut d, |d, r| {
        d.port = 6;
        let x = role(d, r, RoleState::ConnStart);
        d.port = 0;
        x
    });
    scenario(&mut out, "scoreboard read", &mut d, |d, _| {
        btc::bt_scoreboard(&mut d.bus).map(ret_h)
    });
    scenario(&mut out, "radio off", &mut d, |d, r| {
        run!(d, r, |b, s| btc::radio_off(b, s, r)).map(|_| vec![])
    });
    scenario(&mut out, "power off", &mut d, |d, r| {
        run!(d, r, |b, s| btc::power_off(b, s, r)).map(|_| vec![])
    });
    scenario(&mut out, "scan after power off", &mut d, |d, r| {
        scan(d, r, true)
    });
    scenario(&mut out, "packet after power off", &mut d, |d, r| {
        packet(d, r, Packet::Dhcp)
    });
    scenario(&mut out, "radio on after power off", &mut d, |d, r| {
        run!(d, r, |b, s| btc::radio_on(b, s, r)).map(|_| vec![])
    });
    scenario(&mut out, "band switch after radio on", &mut d, swband);

    // ---- C2Hs: the BT side and the firmware's reports ----
    let c2h = |d: &mut Dev, cat: u32, cls: u32, func: u32, p: &[u8]| {
        Ok(ret_b(btc::c2h(d.st(), &c2h_of(cat, cls, func, p.to_vec()))))
    };
    scenario(&mut out, "c2h bt scoreboard off", &mut d, |d, _| {
        c2h(d, 2, 0x12, 2, &[0, 0, 0, 0])
    });
    scenario(&mut out, "c2h bt scoreboard on (2nd BT)", &mut d, |d, _| {
        c2h(d, 2, 0x12, 0x82, &[2, 0, 0, 0x40])
    });
    scenario(&mut out, "c2h bt scoreboard short", &mut d, |d, _| {
        c2h(d, 2, 0x12, 2, &[2, 0, 0])
    });
    scenario(&mut out, "c2h bt info", &mut d, |d, _| {
        c2h(d, 2, 0x12, 1, &[1, 2, 3, 4, 5])
    });
    scenario(&mut out, "c2h other class", &mut d, |d, _| {
        c2h(d, 2, 0x10, 2, &[2, 0, 0, 0])
    });
    scenario(&mut out, "c2h other category", &mut d, |d, _| {
        c2h(d, 1, 0x12, 2, &[2, 0, 0, 0])
    });
    scenario(&mut out, "c2h other event", &mut d, |d, _| {
        c2h(d, 2, 0x12, 5, &[2, 0, 0, 0])
    });

    let agc: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let mut m = Dev::new(true, 1, code);
    {
        let agc = agc.clone();
        m.bus.answer_fn(Key::Bb(0x4688), move |regs| {
            agc.get()
                .unwrap_or_else(|| regs.get(BB + 0x4688, Width::W32))
        });
    }
    let mreg = |a: u32, b: u32, n: u8| rpt_of(RAW_MREG, &mreg_rpt(a, b, n), None);
    scenario(&mut out, "m power on + init", &mut m, |d, r| {
        d.power_on()?;
        init_normal(d, r).map(|_| vec![])
    });
    scenario(&mut out, "m ctrl + mreg", &mut m, |d, _| {
        Ok(rpt(
            d,
            &[
                rpt_of(RAW_CTRL, &ctrl_rpt(4, 0x1d1d0012, 0x001d1d12), None),
                mreg(0x1a000000, 0, 16),
            ],
        ))
    });
    scenario(&mut out, "m scan", &mut m, |d, r| scan(d, r, true));
    scenario(&mut out, "m band switch", &mut m, swband);
    scenario(&mut out, "m mreg on", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x80000000, 0, 16)]))
    });
    scenario(&mut out, "m eapol", &mut m, |d, r| {
        packet(d, r, Packet::Eapol)
    });
    scenario(&mut out, "m mreg on again", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x80000000, 0, 16)]))
    });
    agc.set(Some(0x80123456));
    scenario(&mut out, "m act1 register on", &mut m, act1);
    agc.set(None);
    scenario(&mut out, "m ctrl wrong length", &mut m, |d, _| {
        Ok(rpt(
            d,
            &[
                rpt_of(RAW_CTRL, &ctrl_rpt(0x7f, 0, 0), Some(79)),
                mreg(0x1a000000, 0, 16),
            ],
        ))
    });
    scenario(&mut out, "m unknown type", &mut m, |d, _| {
        Ok(rpt(
            d,
            &[rpt_of(RAW_TEST, &[0; 4], None), mreg(0x1a000000, 0, 16)],
        ))
    });
    scenario(&mut out, "m type past the max", &mut m, |d, _| {
        Ok(rpt(d, &[rpt_of(RAW_BIG, &[0; 4], None)]))
    });
    scenario(&mut out, "m ctrl map 0x7f", &mut m, |d, _| {
        Ok(rpt(d, &[rpt_of(RAW_CTRL, &ctrl_rpt(0x7f, 0, 0), None)]))
    });
    scenario(&mut out, "m dhcp after the map", &mut m, |d, r| {
        packet(d, r, Packet::Dhcp)
    });
    scenario(&mut out, "m mreg gnt mux", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x1a000000, 1 << 17, 16)]))
    });
    scenario(&mut out, "m mreg few entries", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x80000000, 0, 13)]))
    });
    scenario(&mut out, "m mreg none", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x80000000, 0, 0)]))
    });
    scenario(&mut out, "m header past the end", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x1a000000, 0, 16), vec![6, 0x54]]))
    });
    scenario(&mut out, "m length past the end", &mut m, |d, _| {
        Ok(rpt(d, &[mreg(0x1a000000, 0, 16)[..50].to_vec()]))
    });
    scenario(&mut out, "m other sizes", &mut m, |d, _| {
        Ok(rpt(
            d,
            &[
                rpt_of(RAW_CYSTA, &[0; 1384], None),
                rpt_of(RAW_STEP, &[0; 208], None),
                rpt_of(RAW_GPIO, &[0; 16], None),
                mreg(0x1a000000, 0, 16),
            ],
        ))
    });
    for i in 0..3 {
        scenario(&mut out, &format!("m tdma differs {i}"), &mut m, |d, _| {
            Ok(rpt(
                d,
                &[rpt_of(RAW_TDMA, &tdma_rpt(&[1, 0, 0, 0, 0, 0, 0, 0]), None)],
            ))
        });
    }
    scenario(&mut out, "m dhcp forced", &mut m, |d, r| {
        packet(d, r, Packet::Dhcp)
    });
    scenario(&mut out, "m tdma in sync", &mut m, |d, _| {
        let now = d.st().dm.tdma_now;
        Ok(rpt(d, &[rpt_of(RAW_TDMA, &tdma_rpt(&now), None)]))
    });
    let mut slots = vec![0u8; 8 + 8 * 18];
    slots[0] = 1;
    slots[1] = 18;
    for (i, s) in m.st().dm.slot_now.iter().enumerate() {
        h2c::put_slot(&mut slots[8 + 8 * i..], s);
    }
    for i in 0..3 {
        scenario(&mut out, &format!("m slot in sync {i}"), &mut m, |d, _| {
            Ok(rpt(d, &[rpt_of(RAW_SLOT, &slots, None)]))
        });
    }
    slots[8] ^= 1;
    for i in 0..3 {
        scenario(&mut out, &format!("m slot differs {i}"), &mut m, |d, _| {
            Ok(rpt(d, &[rpt_of(RAW_SLOT, &slots, None)]))
        });
    }
    scenario(&mut out, "m two reports", &mut m, |d, _| {
        Ok(rpt(
            d,
            &[
                rpt_of(RAW_SLOT, &slots, None),
                rpt_of(RAW_TDMA, &tdma_rpt(&[0, 0, 0, 1, 0, 0, 0, 0]), None),
            ],
        ))
    });
    scenario(&mut out, "m act1 forced", &mut m, act1);

    // ---- role v2 layout ----
    let mut r5 = Dev::new(true, 1, code);
    scenario(&mut out, "r5 init", &mut r5, |d, r| {
        d.power_on()?;
        init_normal(d, r).map(|_| vec![])
    });
    {
        let rr = &mut r5.st().wl.role;
        let rl = |connected, pid, phy, noa, rf_band, client_ps, bw, role, ch, noa_dur| Rlink {
            connected,
            pid,
            phy,
            noa,
            rf_band,
            client_ps,
            bw,
            role,
            ch,
            noa_dur,
            ..Rlink::default()
        };
        rr.rlink[1] = rl(1, 1, 0, 1, 1, 1, 2, 1, 40, 0x11223344);
        rr.rlink[3] = rl(1, 3, 1, 0, 2, 0, 1, 2, 0x33, 0x55667788);
        rr.rlink[4] = rl(1, 4, 1, 1, 1, 1, 3, 3, 0x44, 0x99aabbcc);
        rr.mrole_type = 0x0a0b0c0d;
        rr.mrole_noa_duration = 0x01020304;
    }
    scenario(&mut out, "r5 role layout", &mut r5, |d, r| {
        run!(d, r, |b, s| h2c::h2c_cx_role(b, s, r)).map(|_| vec![])
    });

    // ---- a 5 GHz association ----
    let mut f5 = Dev::new(true, 1, code);
    scenario(&mut out, "f5 init", &mut f5, |d, r| {
        d.power_on()?;
        init_normal(d, r).map(|_| vec![])
    });
    (f5.band, f5.ch, f5.port, f5.mac_id) = (Band::B5g, 36, 0, 3);
    scenario(&mut out, "f5 role start", &mut f5, |d, r| {
        role(d, r, RoleState::Start)
    });
    let both = |d: &mut Dev, r: &mut Rec| {
        connect(d, r, true)?;
        connect(d, r, false)
    };
    scenario(&mut out, "f5 connect", &mut f5, both);
    scenario(&mut out, "f5 phy map", &mut f5, |d, _| {
        Ok(ret_h(btc_phymap(Some(&d.chan()), 0, 3)?))
    });
    scenario(&mut out, "f5 scan", &mut f5, |d, r| {
        scan(d, r, true)?;
        scan(d, r, false)
    });
    f5.ch = 149;
    scenario(&mut out, "f5 connect 149", &mut f5, both);
    f5.ch = 100;
    scenario(&mut out, "f5 connect 100", &mut f5, both);
    (f5.band, f5.ch, f5.port, f5.mac_id) = (Band::B2g, 11, 1, 4);
    scenario(&mut out, "f5 second port on 2.4 GHz", &mut f5, both);

    // ---- the chip ops not reached on this path ----
    let mut t = Dev::new(true, 1, code);
    t.power_on().unwrap();
    scenario(&mut out, "txpwr ctrl", &mut t, |d, _| {
        for v in [0xffffffff, 0xffff0010, 0x0010ffff, 0x01ff01ff, 0x0100fe00] {
            hw::btc_set_wl_txpwr_ctrl(&mut d.bus, v)?;
        }
        Ok(vec![])
    });
    scenario(&mut out, "rx gain", &mut t, |d, r| {
        for v in [2, 0, 1, 3] {
            run!(d, r, |b, s| hw::btc_set_wl_rx_gain(b, s, v))?;
        }
        Ok(vec![])
    });
    scenario(&mut out, "s1 standby", &mut t, |d, _| {
        hw::btc_wl_s1_standby(&mut d.bus, true)?;
        hw::btc_wl_s1_standby(&mut d.bus, false)?;
        Ok(vec![])
    });
    scenario(&mut out, "bt rssi", &mut t, |_, _| {
        let v: Vec<String> = [-10, 0, -120, -106, -105, 120, 127, 200]
            .iter()
            .map(|&x| hw::btc_get_bt_rssi(x).to_string())
            .collect();
        Ok(vec![format!("= rssi {}", v.join(","))])
    });
    scenario(&mut out, "wl pri", &mut t, |d, _| {
        for m in [1, 2, 3, 0, 4] {
            hw::btc_set_wl_pri(&mut d.bus, m, true)?;
            hw::btc_set_wl_pri(&mut d.bus, m, false)?;
        }
        Ok(vec![])
    });
    scenario(&mut out, "lte read", &mut t, |d, _| {
        hw::read_lte(&mut d.bus, 0x38).map(ret_h)
    });
    scenario(&mut out, "preserve with a2dp", &mut t, |d, _| {
        d.st().bt0.a2dp = 1;
        let Dev { bus, st, .. } = d;
        btc::role::preserve_bt_time(bus, st.as_ref().unwrap(), 30)?;
        d.st().bt0.a2dp = 0;
        Ok(vec![])
    });

    // ---- guards ----
    let mut c = Dev::new(false, 1, code);
    scenario(&mut out, "cmac off init", &mut c, |d, r| {
        d.power_on()?;
        init_normal(d, r).map(|_| vec![])
    });
    let mut w = Dev::new(true, 1, code);
    w.bus.answer(Key::Mac(0x73), 0x04);
    scenario(&mut out, "pta owner warning", &mut w, |d, r| {
        d.power_on()?;
        init_normal(d, r).map(|_| vec![])
    });
    let mut bt = Dev::new(true, 1, code);
    bt.power_on().unwrap();
    bt.st().bt0.scbd_c2h = 1 << 1;
    scenario(&mut out, "bt on stops the init", &mut bt, |d, r| {
        init_normal(d, r).map(|_| vec![])
    });
    let mut b2 = Dev::new(true, 1, code);
    b2.power_on().unwrap();
    b2.st().bt0.scbd_c2h = 0xffffffff;
    scenario(&mut out, "scoreboard all ones", &mut b2, |d, r| {
        init_normal(d, r).map(|_| vec![])
    });
    scenario(&mut out, "scoreboard all ones band switch", &mut b2, swband);
    let mut wo = Dev::new(true, 1, code);
    wo.power_on().unwrap();
    let init_mode = |d: &mut Dev, r: &mut Rec, mode: u32| {
        let rfe = d.rfe;
        run!(d, r, |b, s| btc::init(
            b,
            s,
            r,
            ChipFacts { rfe, cv: 1 },
            mode
        ))
        .map(|_| vec![])
    };
    scenario(&mut out, "wl only init", &mut wo, |d, r| {
        init_mode(d, r, BTC_MODE_WL)
    });
    scenario(&mut out, "wl only band switch", &mut wo, swband);
    scenario(&mut out, "wl only radio off", &mut wo, |d, r| {
        run!(d, r, |b, s| btc::radio_off(b, s, r)).map(|_| vec![])
    });
    for mode in [2, 3, 4] {
        let mut x = Dev::new(true, 1, code);
        x.power_on().unwrap();
        scenario(&mut out, &format!("mode {mode} refused"), &mut x, |d, r| {
            init_mode(d, r, mode)
        });
    }
    let mut ded = Dev::new(true, 2, code);
    ded.power_on().unwrap();
    scenario(&mut out, "dedicated antenna refused", &mut ded, |d, r| {
        init_normal(d, r).map(|_| vec![])
    });
    let mut r0 = Dev::new(true, 0, code);
    r0.power_on().unwrap();
    scenario(&mut out, "rfe 0 init", &mut r0, |d, r| {
        init_normal(d, r).map(|_| vec![])
    });
    for (name, v) in [
        ("0.29.28.255", [0, 29, 28, 255]),
        ("0.29.29.0", [0, 29, 29, 0]),
        ("0.29.121.255", [0, 29, 121, 255]),
        ("0.29.122.0", [0, 29, 122, 0]),
    ] {
        let mut o = Dev::new(true, 1, ver_code(v));
        scenario(&mut out, &format!("firmware {name}"), &mut o, |d, _| {
            d.power_on().map(|_| vec![])
        });
    }
    let mut po = Dev::new(true, 1, code);
    scenario(&mut out, "po init", &mut po, |d, r| {
        d.power_on()?;
        init_normal(d, r).map(|_| vec![])
    });
    scenario(
        &mut out,
        "po power off without radio off",
        &mut po,
        |d, r| run!(d, r, |b, s| btc::power_off(b, s, r)).map(|_| vec![]),
    );
    scenario(&mut out, "po radio on", &mut po, |d, r| {
        run!(d, r, |b, s| btc::radio_on(b, s, r)).map(|_| vec![])
    });
    scenario(&mut out, "po band switch", &mut po, swband);
    let mut early = Dev::new(true, 1, code);
    early.power_on().unwrap();
    scenario(&mut out, "scan before init", &mut early, |d, r| {
        scan(d, r, true)
    });
    scenario(&mut out, "dack before init", &mut early, |d, r| {
        rfk(d, r, 0x10, BTC_WRFKT_DACK, BTC_WRFK_START).map(|_| vec![])
    });
    out
}

#[test]
fn btc_equals_the_js_module() {
    if skip("btc_equals_the_js_module") {
        return;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/btc-js.mjs");
    let res = Command::new("node")
        .arg(&script)
        .env("WLAN_TOOLS", wlan_dir())
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        res.status.success(),
        "btc-js.mjs: {}\n{}",
        res.status,
        String::from_utf8_lossy(&res.stderr)
    );
    let js: Vec<String> = String::from_utf8(res.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let ours = rust_side();
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
    let scenarios = js.iter().filter(|l| l.starts_with("## ")).count();
    assert_eq!(scenarios, 147, "the JS side's scenarios");
    let errors = js.iter().filter(|l| *l == "! error").count();
    assert_eq!(errors, 9, "the refusals (BT on, modes, antenna, firmware)");
    // the oracle really sent the coex H2Cs and wrote the registers
    assert!(js.iter().filter(|l| l.starts_with("= call h2c")).count() > 100);
    assert!(js.len() > 5_000, "{} lines", js.len());
}
