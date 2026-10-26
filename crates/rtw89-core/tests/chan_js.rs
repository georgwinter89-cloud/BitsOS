//! The chan module against the JS driver's own chan.mjs (chan-bb/-txpwr/-path/-fw/-efuse/
//! -regd.mjs), which test-chan.mjs checks against register sequences written from the Linux C:
//! tests/fake/chan-js.mjs runs the JS side of test-chan.mjs's scenarios (and more) on its device
//! (chan-test-expect.mjs `dev`: the chip facts, the real firmware file and efuse dump) with its
//! stand-ins of macinit, rfk1 and btc, and prints every op, every neighbour call where it
//! happened, the results and the channel state, plus what chan reads from the firmware file and
//! the efuse (the TX power configurations, the REGD map, every loaded table, the limits of four
//! countries). The same runs here on `rfk1::fake::RfFake` set up as `dev`, with stubs that do
//! what the JS stubs do (the efuse values from phy's parse, the BB gain from phy's walk), and the
//! two texts must be equal line by line. Needs node and the read-only WLAN tree;
//! RTW89_SKIP_FAKE=1 skips (with a line on stderr), but never with BITSOS_GATE or CI set.

use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use rtw89_core::bus::{Bus, prep};
use rtw89_core::chan::{
    self, ChanHooks, ChanReq, ChanState, RtwChan, S8Table, chan_create, fw as cfw, txpwr,
};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::Firmware;
use rtw89_core::phy::bbx::set_gain_offset;
use rtw89_core::phy::efuse::{Efuse, read_efuse_8852bx, read_phycap_8852bx};
use rtw89_core::phy::io::BB;
use rtw89_core::phy::tbl::{self, BbGain, PhyTables};
use rtw89_core::phy::{Band, Chan, Phy};
use rtw89_core::rfk1::fake::{Key, RfFake};
use rtw89_core::tables::chan::TXPWR_CONF;
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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS chan.mjs)"
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

fn ld<T: std::fmt::Display>(a: &[T]) -> String {
    a.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
fn b01(v: bool) -> &'static str {
    if v { "1" } else { "0" }
}
fn band_name(b: Band) -> &'static str {
    match b {
        Band::B2g => "2g",
        Band::B5g => "5g",
        Band::B6g => "6g",
    }
}
fn a2(a: [u8; 2]) -> String {
    String::from_utf8_lossy(&a).into_owned()
}

/// stubs/chan-macinit.mjs, chan-rfk1.mjs and chan-btc.mjs in Rust: each call is noted with its
/// position
#[derive(Default)]
struct Stubs {
    calls: Vec<(usize, String)>,
    fail_rf: bool,
}

impl Stubs {
    fn log(&mut self, bus: &RfFake, s: String) {
        self.calls.push((bus.trace.len(), format!("= call {s}")));
    }
    // rtw89_set_hw_sch_tx_en's register path as the stand-in has it
    fn set_hw_sch_tx_en(bus: &mut RfFake, tx_en: u32, mask: u32) -> Result<(), Rtw89Error> {
        let v = bus.r32(R_AX_CMAC_FUNC_EN)?;
        if v & B_AX_CMAC_EN == 0 {
            return Ok(());
        }
        let cur = u32::from(bus.r16(R_AX_CTN_TXEN)?);
        bus.w16(R_AX_CTN_TXEN, ((cur & !mask) | (tx_en & mask)) & 0xffff)?;
        Ok(())
    }
}

impl ChanHooks<RfFake> for Stubs {
    fn stop_sch_tx(&mut self, bus: &mut RfFake) -> Result<u16, Rtw89Error> {
        self.log(bus, "stopSchTx".into());
        let tx_en = bus.r16(R_AX_CTN_TXEN)?;
        Stubs::set_hw_sch_tx_en(bus, 0, B_AX_CTN_TXEN_ALL_MASK)?;
        Ok(tx_en)
    }
    fn resume_sch_tx(&mut self, bus: &mut RfFake, tx_en: u16) -> Result<(), Rtw89Error> {
        self.log(bus, format!("resumeSchTx {tx_en:#x}"));
        Stubs::set_hw_sch_tx_en(bus, tx_en.into(), B_AX_CTN_TXEN_ALL_MASK)
    }
    fn set_channel_rf(&mut self, bus: &mut RfFake, chan: &Chan) -> Result<(), Rtw89Error> {
        self.log(bus, format!("setChannelRf {}", chan.ch));
        if self.fail_rf {
            bus.poll32(BB + R_SWSI_V1, B_SWSI_W_BUSY_V1 | B_SWSI_R_BUSY_V1, 0, 1)?;
        }
        Ok(())
    }
    fn btc_switch_band(&mut self, bus: &mut RfFake, _: Band) -> Result<(), Rtw89Error> {
        self.log(bus, "btcSwitchBand".into());
        Ok(())
    }
    fn rfk_band_changed(
        &mut self,
        bus: &mut RfFake,
        _: &mut Phy,
        chan: &Chan,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, format!("rfkBandChanged {}", chan.ch));
        Ok(())
    }
    fn rfk_channel(
        &mut self,
        bus: &mut RfFake,
        _: &mut Phy,
        chan: &Chan,
    ) -> Result<(), Rtw89Error> {
        self.log(bus, format!("rfkChannel {}", chan.ch));
        Ok(())
    }
}

struct Files {
    fw: Rc<Firmware>,
    efuse_log: Vec<u8>,
}

fn files() -> Files {
    let fw = std::fs::read(wlan_dir().join("firmware/rtw8852b_fw-2.bin")).expect("fw-2.bin");
    Files {
        fw: Rc::new(Firmware::parse(fw, 1, false).expect("rtw89_fw_recognize")),
        efuse_log: std::fs::read(wlan_dir().join("kit/efuse-log.bin")).expect("efuse-log.bin"),
    }
}

/// One device of a scenario: the chip, phy's part chan reads, the channel state, the stubs
struct Dev {
    b: RfFake,
    phy: Phy,
    st: ChanState,
    stubs: Stubs,
    fw: Rc<Firmware>,
}

struct Opts<'a> {
    phycap: Option<&'a [u8]>,
    efuse_log: Option<&'a [u8]>,
    fw: Option<Rc<Firmware>>,
    tx_en: u32,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts {
            phycap: None,
            efuse_log: None,
            fw: None,
            tx_en: 0x00ff,
        }
    }
}

// rtwdev->bb_gain as phy's init_bb_reg leaves it (the BB_GAIN walk for rfe 1 / cv 1)
fn bb_gain_of(fw: &Firmware) -> BbGain {
    let t = PhyTables::from_firmware(fw, 0);
    let mut g = BbGain::default();
    let table = t.bb_gain.expect("BB_GAIN element");
    tbl::init_reg(&table, 1, 1, |a, d| {
        tbl::config_bb_gain_ax(&mut g, a, d);
        Ok(())
    })
    .expect("BB_GAIN walk");
    g
}

// the efuse values as phy's parse leaves them (offset_base/rssi_base as bb_sethw reads them)
fn efuse_of(log: &[u8], phycap: Option<&[u8]>) -> Efuse {
    let none = [0xffu8; 128];
    let mut ef = Efuse::default();
    read_efuse_8852bx(log, &mut ef);
    read_phycap_8852bx(phycap.unwrap_or(&none), &mut ef);
    ef.gain.offset_base = [-12, 0];
    ef.gain.rssi_base = [16, 0];
    ef
}

// chan-test-expect.mjs dev: CMAC on, BB RX path both, the RPL bias registers as the BB table
// leaves them, R_AX_CTN_TXEN = tx_en, the CMAC byte registers of set_channel_mac with other bits
fn dev(f: &Files, o: Opts<'_>) -> Dev {
    let fw = o.fw.unwrap_or_else(|| f.fw.clone());
    let mut b = RfFake::new();
    b.answer(
        Key::Mac(R_AX_CMAC_FUNC_EN),
        B_AX_CMAC_EN | B_AX_CMAC_TXEN | B_AX_CMAC_RXEN,
    )
    .answer(Key::Bb(R_CHBW_MOD_V1), prep(B_ANT_RX_SEG0, 3))
    .answer(Key::Bb(R_P0_RPL1), 0xf4)
    .answer(Key::Bb(R_P1_RPL1), 0x10);
    b.poke(R_AX_CTN_TXEN, rtw89_core::bus::Width::W32, o.tx_en);
    b.poke(R_AX_WMAC_RFMOD, rtw89_core::bus::Width::W32, 0x5a5a5aa7);
    b.poke(R_AX_TXRATE_CHK, rtw89_core::bus::Width::W32, 0x3c3c3ce5);
    let mut phy = Phy::new(1);
    phy.efuse = Some(efuse_of(o.efuse_log.unwrap_or(&f.efuse_log), o.phycap));
    phy.bb_gain = bb_gain_of(&fw);
    Dev {
        b,
        phy,
        st: ChanState::new(),
        stubs: Stubs::default(),
        fw,
    }
}

impl Dev {
    // chan.mjs `setup`: the tables on the first call that needs them
    fn setup(&mut self) -> Result<(), Rtw89Error> {
        if self.st.tables.is_none() {
            self.st.load(&self.fw, 1, 0)?;
        }
        Ok(())
    }
    fn cfg_txrx_path(&mut self) -> Result<Vec<String>, Rtw89Error> {
        self.setup()?;
        chan::cfg_txrx_path(&mut self.b, &self.phy, &mut self.st)?;
        Ok(vec![])
    }
    fn switch(
        &mut self,
        band: Band,
        ch: u32,
        bw: u32,
        rfk: bool,
    ) -> Result<Vec<String>, Rtw89Error> {
        if bw == 20 {
            self.setup()?;
        }
        let req = ChanReq { band, ch, bw };
        chan::set_channel(
            &mut self.b,
            &mut self.phy,
            &mut self.st,
            req,
            &mut self.stubs,
            rfk,
        )?;
        Ok(vec![])
    }
    fn set_txpwr(&mut self) -> Result<Vec<String>, Rtw89Error> {
        chan::set_txpwr(&mut self.b, &mut self.st)?;
        Ok(vec![])
    }
    fn regd_set(&mut self, cc: &str) -> Result<chan::Regd, Rtw89Error> {
        let c = cc.as_bytes();
        chan::regd_set(&mut self.b, &mut self.st, [c[0], c[1]])
    }
    fn mac(&mut self, ch: u32) -> Result<Vec<String>, Rtw89Error> {
        let c = chan_for(ch);
        chan::set_channel_mac(&mut self.b, &c, &mut self.st)?;
        Ok(vec![])
    }
    fn bb(&mut self, ch: u32) -> Result<Vec<String>, Rtw89Error> {
        let c = chan_for(ch);
        chan::bb::set_channel_bb(&mut self.b, &self.phy, &c)?;
        Ok(vec![])
    }
}

fn band_of(ch: u32) -> Band {
    if ch > 14 { Band::B5g } else { Band::B2g }
}

// test-chan.mjs chanFor: a struct rtw89_chan for the MAC and BB parts alone
fn chan_for(ch: u32) -> RtwChan {
    chan_create(band_of(ch), ch)
}

fn chan_line(d: &Dev) -> String {
    let c = &d.st.chan;
    let s = &d.st;
    format!(
        "= chan {} {} {} {} {} {} {} {} {} {} rcd {} {} {} {} active {} dm {} regd {}",
        band_name(c.band),
        c.channel,
        c.primary_channel,
        c.band_width,
        c.freq,
        c.subband_type,
        c.tx_comp_band,
        c.pri_ch_idx,
        c.pri_sb_idx,
        c.rfsi_band,
        s.prev_ch,
        band_name(s.prev_band),
        s.prev_band_type,
        b01(s.band_changed),
        b01(s.entity_active),
        b01(s.dm_init_done),
        s.regd.map_or("-".to_string(), |r| a2(r.alpha2))
    )
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
        let t0 = d.b.trace.len();
        d.stubs.calls.clear();
        let res = f(d).unwrap_or_else(|_| vec!["! error".into()]);
        let ops: Vec<String> = d.b.lines()[t0..].to_vec();
        let calls = std::mem::take(&mut d.stubs.calls);
        for i in 0..=ops.len() {
            for (pos, c) in &calls {
                if pos - t0 == i {
                    self.0.push(c.clone());
                }
            }
            if let Some(o) = ops.get(i) {
                self.0.push(o.clone());
            }
        }
        self.0.extend(res);
        self.0.push(chan_line(d));
    }
}

// chan-test-raw.mjs synthBbGain: the BB_GAIN element keeps its addresses, every data byte differs
fn synth_bb_gain(fw: &Firmware) -> Rc<Firmware> {
    let mut file = fw.file().to_vec();
    let e = fw
        .element(RTW89_FW_ELEMENT_ID_BB_GAIN)
        .expect("BB_GAIN element");
    for i in 0..(e.size / 8) as usize {
        let at = e.payload.start + 12 + 8 * i;
        let v = 0x04030201u32.wrapping_add((i as u32).wrapping_mul(0x05050505));
        file[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    Rc::new(Firmware::parse(file, 1, false).expect("synthetic BB_GAIN"))
}

// chan-test-raw.mjs synthByrate: the rfe 0 TXPWR_BYRATE element keeps every entry's band, nss,
// rate section, shift and length, every data byte differs
fn synth_byrate(fw: &Firmware) -> Rc<Firmware> {
    let mut file = fw.file().to_vec();
    for e in fw
        .elements
        .iter()
        .filter(|e| e.id == RTW89_FW_ELEMENT_ID_TXPWR_BYRATE)
    {
        let p = e.payload.start;
        if file[p + 2] != 0 {
            continue;
        }
        let sz = usize::from(file[p + 3]);
        let n = u32::from_le_bytes(file[p + 4..p + 8].try_into().unwrap()) as usize;
        for i in 0..n {
            let ent = p + 8 + i * sz;
            let off = u32::from(file[ent + 2]) == RTW89_RS_OFFSET;
            let b: Vec<u8> = (0..4)
                .map(|k| {
                    if off {
                        ((2 * (k + 1 + i)) & 0x1e) as u8
                    } else {
                        (0x10 + ((i * 4 + k) * 7) % 0x60) as u8
                    }
                })
                .collect();
            file[ent + 5..ent + 9].copy_from_slice(&b);
        }
    }
    Rc::new(Firmware::parse(file, 1, false).expect("synthetic byrate"))
}

fn byr_line(h: &cfw::ByrHead) -> String {
    let n3 = |a: &[[i8; 16]; 4]| a.iter().map(|x| ld(x)).collect::<Vec<_>>().join("/");
    let h3 = |a: &[[i8; 4]; 2]| a.iter().map(|x| ld(x)).collect::<Vec<_>>().join("/");
    format!(
        "{} {} {} {} {}",
        ld(&h.cck),
        ld(&h.ofdm),
        h.mcs.iter().map(n3).collect::<Vec<_>>().join("|"),
        h.hedcm.iter().map(h3).collect::<Vec<_>>().join("|"),
        ld(&h.offset)
    )
}

fn dump(out: &mut Vec<String>, name: &str, t: &S8Table) {
    let dims = t.dims();
    let last = *dims.last().unwrap();
    for (r, row) in t.data().chunks(last).enumerate() {
        if row.iter().all(|&v| v == 0) {
            continue;
        }
        let mut idx = Vec::new();
        let mut q = r;
        for k in (0..dims.len() - 1).rev() {
            idx.insert(0, q % dims[k]);
            q /= dims[k];
        }
        out.push(format!("= {name} {}: {}", ld(&idx), ld(row)));
    }
}

fn inputs(f: &Files, out: &mut Vec<String>) {
    out.push("## inputs".into());
    let confs = cfw::recognize_txpwr(&f.fw, 1, 0);
    out.push(format!(
        "= confs {}",
        confs
            .iter()
            .map(|c| format!("{}={}/{}/{}", c.key, c.rfe, c.ent_sz, c.num_ents))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let map = cfw::load_regd_map(&f.fw).unwrap().map.unwrap();
    out.push(format!("= regd map {}", map.len()));
    for r in &map {
        out.push(format!(
            "= regd {} {} {:#x}",
            a2(r.alpha2),
            ld(&r.txpwr_regd),
            r.fmap
        ));
    }
    let gain = |ef: &Efuse| {
        let g = &ef.gain;
        format!(
            "{} {} offset {} comp {}",
            b01(g.offset_valid),
            b01(g.comp_valid),
            g.offset.iter().map(|o| ld(o)).collect::<Vec<_>>().join(";"),
            g.comp.iter().map(|o| ld(o)).collect::<Vec<_>>().join(";")
        )
    };
    out.push(format!("= gain {}", gain(&efuse_of(&f.efuse_log, None))));
    let (glog, gpc) = programmed(&f.efuse_log);
    out.push(format!(
        "= gain prog {}",
        gain(&efuse_of(&glog, Some(&gpc)))
    ));
    let trim = |ef: &Efuse| {
        let t = &ef.pwr_trim;
        let h = |a: &[u8; 2]| format!("{:#x},{:#x}", a[0], a[1]);
        format!(
            "{} {} {} {}",
            b01(t.pg_thermal_trim),
            h(&t.thermal_trim),
            b01(t.pg_pa_bias_trim),
            h(&t.pa_bias_trim)
        )
    };
    let tpc = trim_phycap();
    out.push(format!(
        "= trim {} | {}",
        trim(&efuse_of(&f.efuse_log, None)),
        trim(&efuse_of(&f.efuse_log, Some(&tpc)))
    ));

    let mut st = ChanState::new();
    st.load(&f.fw, 1, 0).expect("chan tables");
    let t = st.tables.clone().unwrap();
    out.push(format!(
        "= rfe {} da {}",
        t.rfe
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" "),
        b01(t.has_da)
    ));
    for band in 0..3 {
        for bw in 0..5 {
            out.push(format!(
                "= byr {band} {bw} {}",
                byr_line(t.byr_head(band, bw).unwrap())
            ));
        }
    }
    dump(out, "lmt2g", &t.lmt_2g);
    dump(out, "lmt5g", &t.lmt_5g);
    dump(out, "lmtru2g", &t.lmt_ru_2g);
    dump(out, "lmtru5g", &t.lmt_ru_5g);
    dump(out, "shape", &t.tx_shape_lmt);
    dump(out, "shaperu", &t.tx_shape_lmt_ru);

    let mut chans: Vec<(u32, u32)> = (1..=14).map(|c| (0, c)).collect();
    for (a, b) in [(36, 64), (100, 144), (149, 177)] {
        chans.extend((a..=b).map(|c| (1, c)));
    }
    for cc in ["DE", "TV", "GB", "XX"] {
        let c = cc.as_bytes();
        let r = chan::regd_apply(&mut st, [c[0], c[1]]).unwrap();
        out.push(format!(
            "= limits {cc} -> {} {}",
            a2(r.alpha2),
            ld(&r.txpwr_regd)
        ));
        for &(band, ch) in &chans {
            let mut v = Vec::new();
            for bw in 0..5 {
                for ntx in 0..2 {
                    for rs in 0..3 {
                        for bf in 0..2 {
                            v.push(txpwr::read_txpwr_limit(&t, &r, band, bw, ntx, rs, bf, ch));
                        }
                    }
                }
            }
            let mut ru = Vec::new();
            for x in 0..8 {
                for ntx in 0..2 {
                    ru.push(txpwr::read_txpwr_limit_ru(&t, &r, band, x, ntx, ch));
                }
            }
            out.push(format!("= lmt {band} {ch}: {} ru {}", ld(&v), ld(&ru)));
        }
    }
    let _ = TXPWR_CONF;
}

// the programmed efuse offsets and phycap comp of chan-js.mjs (test-chan.mjs's, plus 5 GHz high
// and the 5 GHz high comp bytes)
fn programmed(log: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut glog = log.to_vec();
    let mut gpc = vec![0xffu8; 128];
    for (a, v) in [
        (0x2d4, 0x1e),
        (0x2d6, 0x2f),
        (0x2d8, 0xe1),
        (0x2da, 0x90),
        (0x2dc, 0x7f),
    ] {
        glog[a] = v;
    }
    for (a, v) in [
        (0x5bb, 0x03),
        (0x590, 0x0e),
        (0x5ba, 0x0d),
        (0x58f, 0x02),
        (0x5b9, 0x8f),
        (0x58d, 0x71),
    ] {
        gpc[a - 0x580] = v;
    }
    (glog, gpc)
}

fn trim_phycap() -> Vec<u8> {
    let mut p = vec![0xffu8; 128];
    for (a, v) in [(0x5df, 0x13), (0x5dc, 0x0c), (0x5de, 0x5a), (0x5db, 0x21)] {
        p[a - 0x580] = v;
    }
    p
}

const CHS: [u32; 9] = [1, 6, 13, 36, 44, 100, 140, 149, 165];

fn rust_side() -> Vec<String> {
    let f = files();
    let mut o = Out(Vec::new());
    inputs(&f, &mut o.0);

    // ---- the PHY init parts ----
    {
        let mut d = dev(&f, Opts::default());
        o.scenario("init_txpwr_unit", &mut d, |d| {
            chan::init_txpwr_unit(&mut d.b, &mut d.st).map(|()| vec![])
        });
        o.scenario("set_txpwr_ctrl", &mut d, |d| {
            d.setup()?;
            chan::set_txpwr_ctrl(&mut d.b, &mut d.st).map(|()| vec![])
        });
        o.scenario("power_trim without phycap", &mut d, |d| {
            d.setup()?;
            chan::power_trim(&mut d.b, &d.phy).map(|()| vec![])
        });
        o.scenario("cfg_txrx_path", &mut d, |d| d.cfg_txrx_path());
        for ch in CHS {
            o.scenario(&format!("mac ch {ch}"), &mut d, |d| d.mac(ch));
            o.scenario(&format!("bb ch {ch}"), &mut d, |d| d.bb(ch));
        }
    }
    {
        let pc = trim_phycap();
        let mut d = dev(
            &f,
            Opts {
                phycap: Some(&pc),
                ..Opts::default()
            },
        );
        o.scenario("power_trim with phycap", &mut d, |d| {
            d.setup()?;
            chan::power_trim(&mut d.b, &d.phy).map(|()| vec![])
        });
        let mut p2 = vec![0xffu8; 128];
        p2[0x5dc - 0x580] = 0x1f;
        p2[0x5db - 0x580] = 0xf0;
        let mut d2 = dev(
            &f,
            Opts {
                phycap: Some(&p2),
                ..Opts::default()
            },
        );
        o.scenario("power_trim path B only", &mut d2, |d| {
            d.setup()?;
            chan::power_trim(&mut d.b, &d.phy).map(|()| vec![])
        });
    }
    {
        let (glog, gpc) = programmed(&f.efuse_log);
        let mut g = dev(
            &f,
            Opts {
                efuse_log: Some(&glog),
                phycap: Some(&gpc),
                ..Opts::default()
            },
        );
        for ant_b in [false, true] {
            g.phy.hal.antenna_rx = if ant_b { RF_B } else { 0 };
            for sb in [0u32, 1, 3, 4] {
                let name = format!(
                    "set_gain_offset subband {sb}{}",
                    if ant_b { " antenna B" } else { "" }
                );
                o.scenario(&name, &mut g, |d| {
                    let gain = d.phy.efuse.as_ref().unwrap().gain.clone();
                    set_gain_offset(&mut d.b, sb, &gain, d.phy.hal.antenna_rx)?;
                    Ok(vec![])
                });
            }
        }
        g.phy.hal.antenna_rx = 0;
        o.scenario("cfg_txrx_path programmed gains", &mut g, |d| {
            d.cfg_txrx_path()
        });
        for ch in [1, 36, 100, 149] {
            o.scenario(&format!("bb ch {ch} programmed gains"), &mut g, |d| {
                d.bb(ch)
            });
        }
    }
    for (ant, name, seg) in [(RF_A, "A", 1u32), (RF_B, "B", 2)] {
        let mut d = dev(&f, Opts::default());
        d.phy.hal.antenna_rx = ant;
        d.phy.hal.rx_nss = 1;
        o.scenario(&format!("cfg_txrx_path antenna {name}"), &mut d, |d| {
            d.cfg_txrx_path()
        });
        d.b.answer(Key::Bb(R_CHBW_MOD_V1), prep(B_ANT_RX_SEG0, seg));
        o.scenario(&format!("bb ch 1 rx path {name}"), &mut d, |d| d.bb(1));
        o.scenario(&format!("bb ch 36 rx path {name}"), &mut d, |d| d.bb(36));
    }
    {
        let mut s = dev(
            &f,
            Opts {
                fw: Some(synth_bb_gain(&f.fw)),
                ..Opts::default()
            },
        );
        o.scenario("cfg_txrx_path synthetic bb gain", &mut s, |d| {
            d.cfg_txrx_path()
        });
        for ch in [1, 36, 100, 149] {
            o.scenario(&format!("bb ch {ch} synthetic bb gain"), &mut s, |d| {
                d.bb(ch)
            });
        }
    }

    // ---- whole switches ----
    let mut e = dev(&f, Opts::default());
    o.scenario("switch before cfg_txrx_path", &mut e, |d| {
        d.switch(Band::B2g, 1, 20, false)
    });
    o.scenario("cfg_txrx_path before switching", &mut e, |d| {
        d.cfg_txrx_path()
    });
    for ch in CHS {
        let name = format!("switch {} {ch}", if ch > 14 { "5g" } else { "2g" });
        o.scenario(&name, &mut e, |d| d.switch(band_of(ch), ch, 20, false));
    }
    o.scenario("set_txpwr after 165", &mut e, |d| d.set_txpwr());
    o.scenario("switch 2g 14 refused for DE", &mut e, |d| {
        d.switch(Band::B2g, 14, 20, false)
    });
    o.scenario("switch 40 MHz refused", &mut e, |d| {
        d.switch(Band::B5g, 36, 40, false)
    });
    o.scenario("switch 6g refused", &mut e, |d| {
        d.switch(Band::B6g, 37, 20, false)
    });
    o.scenario("switch 5g 38 refused (off grid)", &mut e, |d| {
        d.switch(Band::B5g, 38, 20, false)
    });
    o.scenario("switch 2g 6 with rfk_channel", &mut e, |d| {
        d.switch(Band::B2g, 6, 20, true)
    });
    o.scenario("switch 2g 11 same band", &mut e, |d| {
        d.switch(Band::B2g, 11, 20, false)
    });
    e.stubs.fail_rf = true;
    e.b.fail_polls.insert(BB + R_SWSI_V1);
    o.scenario("switch 5g 36 with the RF step failing", &mut e, |d| {
        d.switch(Band::B5g, 36, 20, false)
    });
    e.stubs.fail_rf = false;
    e.b.fail_polls.clear();
    o.scenario("switch 5g 44 after the failure", &mut e, |d| {
        d.switch(Band::B5g, 44, 20, false)
    });

    // ---- the country ----
    let flags = |d: &Dev, band: Band, list: &[u32]| {
        list.iter()
            .map(|&ch| {
                let f = d.st.channel_flags(band, ch);
                if !f.allowed {
                    return "-".to_string();
                }
                let s = format!(
                    "{}{}",
                    if f.no_ir { "N" } else { "" },
                    if f.radar { "R" } else { "" }
                );
                if s.is_empty() { "+".into() } else { s }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let c2 = [0, 1, 11, 12, 13, 14, 15];
    let c5 = [
        32, 36, 38, 40, 48, 52, 64, 68, 100, 140, 144, 148, 149, 153, 165, 169, 173, 177, 181,
    ];
    let flag_lines = |d: &Dev| {
        vec![
            format!("= flags 2g {}", flags(d, Band::B2g, &c2)),
            format!("= flags 5g {}", flags(d, Band::B5g, &c5)),
            format!("= flags 6g {}", flags(d, Band::B6g, &[1, 5])),
        ]
    };
    o.scenario("flags DE", &mut e, |d| Ok(flag_lines(d)));
    o.scenario("regd TV", &mut e, |d| {
        Ok(vec![format!(
            "= regd {}",
            ld(&d.regd_set("TV")?.txpwr_regd)
        )])
    });
    o.scenario("regd DE", &mut e, |d| {
        Ok(vec![format!(
            "= regd {}",
            ld(&d.regd_set("DE")?.txpwr_regd)
        )])
    });
    o.scenario("switch 5g 149", &mut e, |d| {
        d.switch(Band::B5g, 149, 20, false)
    });
    o.scenario("regd XX (world)", &mut e, |d| {
        let r = d.regd_set("XX")?;
        let mut v = vec![format!("= regd {}", a2(r.alpha2))];
        v.extend(flag_lines(d));
        Ok(v)
    });
    o.scenario("switch 2g 14 in the world", &mut e, |d| {
        d.switch(Band::B2g, 14, 20, false)
    });
    o.scenario("switch 2g 12 in the world", &mut e, |d| {
        d.switch(Band::B2g, 12, 20, false)
    });
    o.scenario("regd GB", &mut e, |d| {
        Ok(vec![format!(
            "= regd {}",
            ld(&d.regd_set("GB")?.txpwr_regd)
        )])
    });
    o.scenario("regd DE again", &mut e, |d| {
        let mut v = vec![format!("= regd {}", ld(&d.regd_set("DE")?.txpwr_regd))];
        v.extend(flag_lines(d));
        Ok(v)
    });

    // ---- CMAC reads of 0xDEADBEEF ----
    for n in [2u32, 11] {
        let mut k = dev(&f, Opts::default());
        let dead = Rc::new(Cell::new(n));
        let dd = dead.clone();
        k.b.answer_fn(Key::Mac(R_AX_WMAC_RFMOD), move |r| {
            if dd.get() > 0 {
                dd.set(dd.get() - 1);
                0xdeadbeef
            } else {
                r.get(R_AX_WMAC_RFMOD, rtw89_core::bus::Width::W32)
            }
        });
        o.scenario(&format!("mac ch 1 with {n} dead reads"), &mut k, |d| {
            d.mac(1)
        });
    }
    {
        let mut k = dev(&f, Opts::default());
        k.b.answer(Key::Mac(R_AX_CMAC_FUNC_EN), 0);
        o.scenario("help with the CMAC off", &mut k, |d| {
            let bak = chan::help_enter(&mut d.b, &mut d.st, Band::B5g, &mut d.stubs)?;
            chan::help_leave(&mut d.b, &mut d.st, Band::B5g, &mut d.stubs, bak)?;
            Ok(vec![])
        });
    }

    // ---- order of the bring-up ----
    {
        let mut fd = dev(&f, Opts::default());
        o.scenario("order cfg_txrx_path", &mut fd, |d| d.cfg_txrx_path());
        o.scenario("order switch 2g 1", &mut fd, |d| {
            d.switch(Band::B2g, 1, 20, false)
        });
        chan::power_off(&mut fd.st);
        o.scenario("order set_txpwr after power off", &mut fd, |d| {
            d.set_txpwr()
        });
        o.scenario("order switch after power off", &mut fd, |d| {
            d.switch(Band::B2g, 6, 20, false)
        });
        o.scenario("order cfg_txrx_path again", &mut fd, |d| d.cfg_txrx_path());
        o.scenario("order switch 2g 6 after power on", &mut fd, |d| {
            d.switch(Band::B2g, 6, 20, false)
        });
    }

    // ---- the TX power with a TXPWR_BYRATE element whose bytes all differ ----
    {
        let mut y = dev(
            &f,
            Opts {
                fw: Some(synth_byrate(&f.fw)),
                ..Opts::default()
            },
        );
        o.scenario("synthetic byrate cfg_txrx_path", &mut y, |d| {
            d.cfg_txrx_path()
        });
        for ch in [1, 36, 149] {
            o.scenario(&format!("synthetic byrate switch {ch}"), &mut y, |d| {
                d.switch(band_of(ch), ch, 20, false)
            });
            o.scenario(&format!("synthetic byrate set_txpwr {ch}"), &mut y, |d| {
                d.set_txpwr()
            });
        }
    }
    o.0
}

#[test]
fn chan_equals_the_js_module() {
    if skip("chan_equals_the_js_module") {
        return;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/chan-js.mjs");
    let res = Command::new("node")
        .arg(&script)
        .env("WLAN_TOOLS", wlan_dir())
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        res.status.success(),
        "chan-js.mjs: {}\n{}",
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
    assert_eq!(scenarios, 94, "the JS side's scenarios");
    // the oracle really switched channels and loaded the tables
    assert!(js.len() > 7_000, "{} lines", js.len());
}
