//! The phy module against the JS driver's own phy.mjs (phy-efuse/-tbl/-bbx/-dm/-trk/-env/
//! -dig.mjs with the real fw.mjs and pci.mjs) and against the C as phy-test-expect.mjs
//! transcribes it: tests/fake/phy-js.mjs runs test-phy.mjs's scenarios (plus the tracking
//! parts test-track.mjs drives) on test-phy.mjs's chip model and prints every op, every result
//! and the state; it also prints the C's register sequences (`## spec ...`). The same scenarios
//! run here on ChipBus (the same chip model in Rust), and the two texts must be equal line by
//! line. Needs node and the read-only WLAN tree; RTW89_SKIP_FAKE=1 skips (with a line on
//! stderr), but never with BITSOS_GATE or CI set.

use rtw89_core::rfk1::MacLink;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::bus::{Bus, BusError, Width, field, prep};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::{self, Firmware, FwState, RfRegInfo};
use rtw89_core::pci::{PciRings, RingConfig, pci_alloc};
use rtw89_core::phy::chipbus::{ChipBus, Model, phycap_c2h};
use rtw89_core::phy::efuse::{self, Efuse, EfuseMaps};
use rtw89_core::phy::io::BB;
use rtw89_core::phy::tbl::{self, BbGain, RegLoad, RegTable, WalkStop};
use rtw89_core::phy::{
    self, ApRssi, Band, Cfo, Chan, ChipOps, Dig, Env, Hal, Phy, PhyEvent, Ppdu, UlTbLink, bbx, dig,
    dm, env, trk,
};
use rtw89_core::tables::regs::*;

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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS phy.mjs)"
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
fn kit(f: &str) -> Vec<u8> {
    let p = wlan_dir().join("kit").join(f);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}
fn firmware() -> Firmware {
    let p = wlan_dir().join("firmware/rtw8852b_fw-2.bin");
    let f = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    Firmware::parse(f, 1, false).expect("the real file")
}

fn hx(v: u32) -> String {
    format!("0x{v:08x}")
}
fn list<T: ToString>(v: impl IntoIterator<Item = T>) -> String {
    v.into_iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
fn b01(v: bool) -> u32 {
    u32::from(v)
}

/// phy-js.mjs `kind`: FwError, RtwError (the bus), anything else
fn kind(e: &Rtw89Error) -> &'static str {
    use Rtw89Error::*;
    match e {
        H2cRegBusy { .. } | C2hRegTimeout { .. } => "fw",
        Bus(BusError::PollTimeout { .. } | BusError::Refused { .. }) => "device",
        _ => "other",
    }
}

/// phy's own log lines (what the JS driver's d.log prints for Linux's warnings)
fn log_lines(ev: Vec<PhyEvent>) -> Vec<String> {
    ev.into_iter()
        .filter(|e| !matches!(e, PhyEvent::BbGainUnknownBw { .. }))
        .map(|e| format!("= log {e}"))
        .collect()
}

struct Out(Vec<String>);

impl Out {
    /// A scenario on a bus: its ops since now, then what `f` returned (or its error's kind)
    fn on(
        &mut self,
        name: &str,
        b: &mut ChipBus,
        f: impl FnOnce(&mut ChipBus) -> Result<Vec<String>, Rtw89Error>,
    ) {
        self.0.push(format!("## {name}"));
        let t0 = b.mem.trace.len();
        let r = f(b);
        self.0.extend(b.lines()[t0..].iter().cloned());
        match r {
            Ok(v) => self.0.extend(v),
            Err(e) => self.0.push(format!("! error {}", kind(&e))),
        }
    }
    fn logs(&mut self, phy: &mut Phy) {
        self.0.extend(log_lines(phy.take_events()));
    }
    /// The C's sequence as the Rust side runs it on a fresh chip with `model`
    fn spec(&mut self, name: &str, model: Model, f: impl FnOnce(&mut ChipBus)) {
        self.0.push(format!("## spec {name}"));
        let mut b = ChipBus::new(model);
        f(&mut b);
        self.0.extend(b.lines());
    }
}

/// chan's and rfk1's chip ops as test-phy.mjs's spies: they note where they ran
#[derive(Default)]
struct Spy(Vec<String>);

impl Spy {
    fn note(&mut self, name: &str, b: &mut ChipBus) {
        self.0
            .push(format!("= call {name} at {}", b.mem.trace.len()));
    }
    fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.0)
    }
}

impl ChipOps<ChipBus> for Spy {
    fn init_txpwr_unit(&mut self, b: &mut ChipBus, _: &Phy) -> Result<(), Rtw89Error> {
        self.note("initTxpwrUnit", b);
        Ok(())
    }
    fn rfk_init(
        &mut self,
        b: &mut ChipBus,
        _: &mut Phy,
        _: Option<MacLink<'_>>,
    ) -> Result<(), Rtw89Error> {
        self.note("rfkInit", b);
        Ok(())
    }
    fn set_txpwr_ctrl(&mut self, b: &mut ChipBus, _: &Phy) -> Result<(), Rtw89Error> {
        self.note("setTxpwrCtrl", b);
        Ok(())
    }
    fn power_trim(&mut self, b: &mut ChipBus, _: &Phy) -> Result<(), Rtw89Error> {
        self.note("powerTrim", b);
        Ok(())
    }
    fn cfg_txrx_path(&mut self, b: &mut ChipBus, _: &Phy) -> Result<(), Rtw89Error> {
        self.note("cfgTxrxPath", b);
        Ok(())
    }
}

/// test-phy.mjs newDev: the dumps, the firmware's tables, the PCIe rings, the spies
struct Dev {
    b: ChipBus,
    rings: PciRings,
    fws: FwState,
    phy: Phy,
    maps: EfuseMaps,
    spy: Spy,
}

fn maps() -> EfuseMaps {
    EfuseMaps {
        ctrl: Some(0x2020),
        log: kit("efuse-log.bin"),
        dav: Some(kit("efuse-dav.bin")),
        phycap: None,
    }
}

fn new_dev(fw: &Firmware, c2h: Option<[u32; 4]>, rdy_fails: u32) -> Dev {
    let model = Model {
        c2h: c2h.unwrap_or(phycap_c2h(2, 2, 0, 0, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP)),
        rdy_fails,
        ..Model::default()
    };
    let mut b = ChipBus::new(model);
    let rings = pci_alloc(&mut b, &RingConfig::js()).expect("rings");
    b.mem.trace.clear();
    let mut phy = Phy::new(1);
    phy.load_tables(fw);
    Dev {
        b,
        rings,
        fws: FwState::new(),
        phy,
        maps: maps(),
        spy: Spy::default(),
    }
}

// ---- state as text (phy-js.mjs's helpers) ----

fn ilist<T: Copy + Into<i32>>(v: &[T]) -> String {
    list(v.iter().map(|x| (*x).into()))
}

fn efuse_lines(ef: &Efuse, hal: Option<&Hal>) -> Vec<String> {
    let (g, t, p) = (&ef.gain, &ef.tssi, &ef.pwr_trim);
    let mut v = vec![
        format!(
            "= efuse valid {} pk {} mac {} rfe {} xtal {} country {}",
            b01(ef.valid),
            b01(ef.power_k_valid),
            ef.mac_text(),
            ef.rfe_type,
            ef.xtal_cap,
            list(ef.country_code)
        ),
        format!(
            "= tssi thermal {} cck {} / {}",
            list(t.thermal),
            ilist(&t.tssi_cck[0]),
            ilist(&t.tssi_cck[1])
        ),
        format!(
            "= tssi mcs {} / {}",
            ilist(&t.tssi_mcs[0]),
            ilist(&t.tssi_mcs[1])
        ),
        format!(
            "= tssi trim {} / {}",
            ilist(&t.tssi_trim[0]),
            ilist(&t.tssi_trim[1])
        ),
        format!(
            "= trim thermal {} pg {} pa {} pg {}",
            list(p.thermal_trim),
            b01(p.pg_thermal_trim),
            list(p.pa_bias_trim),
            b01(p.pg_pa_bias_trim)
        ),
        format!(
            "= gain offset {} {} / {}",
            b01(g.offset_valid),
            ilist(&g.offset[0]),
            ilist(&g.offset[1])
        ),
        format!(
            "= gain comp {} {} / {} base {},{}",
            b01(g.comp_valid),
            ilist(&g.comp[0]),
            ilist(&g.comp[1]),
            g.offset_base[0],
            g.rssi_base[0]
        ),
        format!(
            "= dav {}",
            ef.dav_log
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect::<String>()
        ),
    ];
    if let Some(h) = hal {
        v.push(format!(
            "= hal cckpd {} igi {}",
            b01(h.support_cckpd),
            b01(h.support_igi)
        ));
    }
    v
}

fn hal_line(h: &Hal) -> String {
    format!(
        "= hal tx {} rx {} ant {},{} div {} noeht {}",
        h.tx_nss,
        h.rx_nss,
        h.antenna_tx,
        h.antenna_rx,
        b01(h.tx_path_diversity),
        b01(h.no_eht)
    )
}

fn sum(pairs: &[(u32, u32)]) -> u32 {
    pairs.iter().fold(0u32, |s, (a, v)| {
        s.wrapping_add(a.wrapping_mul(31)).wrapping_add(*v)
    })
}
fn pair(p: Option<(u32, u32)>) -> String {
    p.map_or("-".into(), |(a, d)| format!("{},{}", hx(a), hx(d)))
}
fn load_line(name: &str, r: Option<&RegLoad>, h2c: bool) -> String {
    let Some(r) = r else {
        return format!("= load {name} -");
    };
    let head = r
        .headline
        .map_or("-".into(), |h| format!("{}/{}", h.size, h.idx));
    let mut s = format!(
        "= load {name} applied {} aborted {} head {head} first {} last {}",
        r.applied,
        b01(r.aborted),
        pair(r.first),
        pair(r.last)
    );
    if h2c {
        s += &format!(" h2c {} {}", r.h2c_values, b01(r.h2c_ok));
    }
    s
}
fn gain_lines(g: &BbGain) -> Vec<String> {
    let mut o = Vec::new();
    for gb in 0..8 {
        for p in 0..2 {
            o.push(format!(
                "= bbgain {gb} {p} lna {} tia {} byp {} op1db {} tiaop {} r20 {} r40 {} r80 {} r160 {}",
                ilist(&g.lna_gain[gb][p]),
                ilist(&g.tia_gain[gb][p]),
                ilist(&g.lna_gain_bypass[gb][p]),
                ilist(&g.lna_op1db[gb][p]),
                ilist(&g.tia_lna_op1db[gb][p]),
                g.rpl_ofst_20[gb][p],
                ilist(&g.rpl_ofst_40[gb][p]),
                ilist(&g.rpl_ofst_80[gb][p]),
                ilist(&g.rpl_ofst_160[gb][p])
            ));
        }
    }
    o
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn h2c_lines(b: &ChipBus, rings: &PciRings, from: usize) -> Vec<String> {
    let bufs = &rings.tx("CH12").unwrap().bufs;
    b.writes[from..]
        .iter()
        .filter(|(a, _)| bufs.contains(a))
        .map(|(_, d)| {
            let (w0, w1) = (le32(d, 24), le32(d, 28));
            let len = field(w1, H2C_HDR_TOTAL_LEN).saturating_sub(H2C_HEADER_LEN) as usize;
            let words: Vec<u32> = (0..len / 4).map(|k| le32(d, 32 + k * 4)).collect();
            format!(
                "= h2c {} {} n {} first {} last {} sum {}",
                hx(w0),
                hx(w1),
                words.len(),
                hx(words.first().copied().unwrap_or(0)),
                hx(words.last().copied().unwrap_or(0)),
                hx(words.iter().fold(0u32, |s, w| s.wrapping_add(*w)))
            )
        })
        .collect()
}
fn cfo_lines(c: &Cfo) -> Vec<String> {
    vec![format!(
        "= cfo cap {} def {} ofst {} ub {} lb {} adj {} lock {},{} status {} timer {},{} trk {} pkt {},{} tail0 {} cnt0 {} dcfo {},{} avgpre {}",
        c.crystal_cap,
        c.def_x_cap,
        c.x_cap_ofst,
        c.x_cap_ub,
        c.x_cap_lb,
        b01(c.is_adjust),
        b01(c.divergence_lock_en),
        c.lock_cnt,
        c.phy_cfo_status,
        b01(c.cfo_trig_by_timer_en),
        c.cfo_timer_ms,
        c.phy_cfo_trk_cnt,
        c.packet_count,
        c.packet_count_pre,
        c.cfo_tail[0],
        c.cfo_cnt[0],
        c.dcfo_avg,
        c.dcfo_avg_pre,
        c.cfo_avg_pre
    )]
}
fn env_lines(e: &Env) -> Vec<String> {
    vec![
        format!(
            "= env app {} mntr {} period {} unit {} rac {} ongoing {} wd {} thl {} thh {}",
            e.ifs_clm_app,
            e.ifs_clm_mntr_time,
            e.ccx_period,
            e.ccx_unit_idx,
            e.ccx_rac_lv,
            b01(e.ccx_ongoing),
            e.ccx_watchdog_result,
            list(e.ifs_clm_th_l),
            list(e.ifs_clm_th_h)
        ),
        format!(
            "= env rep {} his {} avg {} cca {} total {}",
            list([
                e.ifs_clm_tx,
                e.ifs_clm_edcca_excl_cca,
                e.ifs_clm_cckcca_excl_fa,
                e.ifs_clm_ofdmcca_excl_fa,
                e.ifs_clm_cckfa,
                e.ifs_clm_ofdmfa
            ]),
            list(e.ifs_clm_his),
            list(e.ifs_clm_avg),
            list(e.ifs_clm_cca),
            e.ifs_clm_total_ifs
        ),
        format!(
            "= env ratio {} ifsavg {} ccaavg {} edcca {}",
            list([
                e.ifs_clm_tx_ratio,
                e.ifs_clm_edcca_excl_cca_ratio,
                e.ifs_clm_cck_fa_ratio,
                e.ifs_clm_ofdm_fa_ratio,
                e.ifs_clm_cck_cca_excl_fa_ratio,
                e.ifs_clm_ofdm_cca_excl_fa_ratio,
                e.ifs_clm_cck_fa_permil,
                e.ifs_clm_ofdm_fa_permil
            ]),
            list(e.ifs_clm_ifs_avg),
            list(e.ifs_clm_cca_avg),
            e.edcca_clm_ratio
        ),
    ]
}
fn dig_lines(g: &Dig) -> Vec<String> {
    vec![format!(
        "= dig rssi {} fa {} ofst {} max {} min {} pdmax {} pdcnt {} pden {} low {} linked {} g {} fath {} force {} bak {} noisy {} diag {} bypass {}",
        g.igi_rssi,
        g.igi_fa_rssi,
        g.fa_rssi_ofst,
        g.dyn_igi_max,
        g.dyn_igi_min,
        g.dyn_pd_th_max,
        g.dyn_pd_max_cnt,
        b01(g.dyn_pd_th_en),
        g.pd_low_th_ofst,
        b01(g.is_linked_pre),
        b01(g.lna_gain_is_g),
        list(g.fa_th),
        b01(g.force_gaincode_idx_en),
        g.bak_dig,
        g.noisy_lv,
        g.diag_pd_cnt,
        b01(g.bypass_dig)
    )]
}
fn band_text(b: Option<Band>) -> &'static str {
    match b {
        Some(Band::B2g) => "2g",
        Some(Band::B5g) => "5g",
        Some(Band::B6g) => "6g",
        None => "-",
    }
}
fn opt(v: Option<u32>) -> String {
    v.map_or("-".into(), |x| x.to_string())
}

fn mk(pairs: &[(u32, u32)], rf_path: u32) -> RegTable {
    RegTable {
        regs: pairs.to_vec(),
        rf_path,
        ver: [0; 4],
    }
}
fn big(n: u32, path: u32) -> RegTable {
    let p: Vec<(u32, u32)> = (0..n).map(|i| (0x10000 | (i & 0xff), i)).collect();
    mk(&p, path)
}
fn poke_field(b: &mut ChipBus, reg: u32, mask: u32, v: u32) {
    let a = BB + reg;
    let old = b.mem.peek(a, Width::W32);
    b.mem.poke(a, Width::W32, (old & !mask) | prep(mask, v));
}
fn prime(b: &mut ChipBus) {
    b.mem
        .poke(BB + R_TXPATH_SEL, Width::W32, prep(B_TXPATH_SEL_MSK, 3));
    b.mem
        .poke(BB + R_CHBW_MOD_V1, Width::W32, prep(B_ANT_RX_SEG0, 2));
    for (i, r) in [
        R_P0_RFMODE,
        R_P0_RFMODE_FTM_RX,
        R_P1_RFMODE,
        R_P1_RFMODE_FTM_RX,
    ]
    .into_iter()
    .enumerate()
    {
        b.mem.poke(BB + r, Width::W32, 0x1233312 + i as u32);
    }
    b.mem
        .poke(BB + R_TXPWR, Width::W32, prep(B_TXPWR_MSK, 0x1f0));
}
fn helper(
    out: &mut Out,
    name: &str,
    f: impl FnOnce(&mut ChipBus) -> Result<Vec<String>, Rtw89Error>,
) {
    let mut b = ChipBus::new(Model::default());
    prime(&mut b);
    out.on(name, &mut b, f);
}

const CHAN_2G6: Chan = Chan {
    band: Band::B2g,
    ch: 6,
    bw: 20,
};

fn walk_line(name: &str, t: &RegTable, rfe: u32, cv: u32, small: bool) -> Vec<String> {
    let mut seen = Vec::new();
    let (r, stop) = tbl::init_reg(t, rfe, cv, |a, d| {
        seen.push((a, d));
        Ok(())
    })
    .unwrap();
    let mut v = vec![load_line(name, Some(&r), false)];
    if small {
        let s: Vec<String> = seen.iter().map(|p| pair(Some(*p))).collect();
        v.push(format!("= seen {}", s.join(" ")));
    } else {
        v.push(format!("= sum {} {}", seen.len(), hx(sum(&seen))));
    }
    match stop {
        Some(WalkStop::InvalidPackage { rfe, cv }) => {
            v.push(format!("= log invalid PHY package: {rfe}/{cv}"))
        }
        Some(WalkStop::LoadCr { addr, data }) => {
            v.push(format!("= log failed to load CR {addr:x}/{data:x}"))
        }
        None => {}
    }
    v
}

fn rust_side() -> Vec<String> {
    let fw = firmware();
    let mut out = Out(Vec::new());

    // ---- 1. efuse ----
    {
        let mut d = new_dev(&fw, None, 0);
        out.on("efuse parse", &mut d.b, |b| {
            phy::efuse_parse(b, &mut d.phy, &mut d.maps)?;
            Ok(efuse_lines(d.phy.efuse.as_ref().unwrap(), Some(&d.phy.hal)))
        });
        out.logs(&mut d.phy);
        let mut d = new_dev(&fw, None, 4);
        out.on("efuse parse, four failed phycap dumps", &mut d.b, |b| {
            phy::efuse_parse(b, &mut d.phy, &mut d.maps)?;
            Ok(efuse_lines(d.phy.efuse.as_ref().unwrap(), None))
        });
        out.logs(&mut d.phy);
        let mut d = new_dev(&fw, None, 5);
        out.on("efuse parse, five failed phycap dumps", &mut d.b, |b| {
            phy::efuse_parse(b, &mut d.phy, &mut d.maps)?;
            Ok(efuse_lines(d.phy.efuse.as_ref().unwrap(), None))
        });
        out.logs(&mut d.phy);
        let mut d = new_dev(&fw, None, 0);
        d.maps.ctrl = None;
        out.on("efuse parse, autoload register read", &mut d.b, |b| {
            phy::efuse_parse(b, &mut d.phy, &mut d.maps)?;
            Ok(efuse_lines(d.phy.efuse.as_ref().unwrap(), None))
        });
        out.logs(&mut d.phy);
        let mut d = new_dev(&fw, None, 0);
        let mut pc = vec![0xffu8; 128];
        for (a, v) in [
            (0x5e9, 0xaa),
            (0x5d6, 0x05),
            (0x5cf, 0xfe),
            (0x5ab, 0x81),
            (0x5df, 0x12),
            (0x5dc, 0x13),
            (0x5de, 0x21),
            (0x5db, 0x43),
            (0x5bb, 0x0f),
            (0x58d, 0x07),
            (0x5ba, 0xf8),
            (0x590, 0x31),
        ] {
            pc[a - efuse::PHYCAP_ADDR as usize] = v;
        }
        d.maps.phycap = Some(pc);
        d.maps.log.truncate(2048);
        out.on("efuse parse, phycap given, 2048-byte log", &mut d.b, |b| {
            phy::efuse_parse(b, &mut d.phy, &mut d.maps)?;
            Ok(efuse_lines(d.phy.efuse.as_ref().unwrap(), None))
        });
        out.logs(&mut d.phy);
        let mut g2 = kit("efuse-log.bin");
        g2[EF_RX_GAIN2G_CCK_OF] = 0x3e;
        g2[EF_RX_GAIN5G_HIGH_OF] = 0xf1;
        let mut e2 = Efuse::default();
        out.on("read_efuse gain nibbles", &mut d.b, |_| {
            efuse::read_efuse_8852bx(&g2, &mut e2);
            Ok(efuse_lines(&e2, None)[5..6].to_vec())
        });
        for o in [
            EF_RX_GAIN2G_CCK_OF,
            EF_RX_GAIN2G_OFDM_OF,
            EF_RX_GAIN5G_LOW_OF,
            EF_RX_GAIN5G_MID_OF,
            EF_RX_GAIN5G_HIGH_OF,
        ] {
            g2[o] = 0xff;
        }
        out.on("read_efuse gain all 0xff", &mut d.b, |_| {
            efuse::read_efuse_8852bx(&g2, &mut e2);
            Ok(efuse_lines(&e2, None)[5..6].to_vec())
        });
        out.on("logical maps", &mut d.b, |_| {
            let mut log = vec![0xffu8; 2048];
            efuse::logical_efuse_map(&kit("efuse-phy.bin"), &mut log)?;
            let mut dav = vec![0xffu8; 16];
            efuse::logical_efuse_map(&kit("efuse-dav.bin"), &mut dav)?;
            let hex: String = dav.iter().map(|x| format!("{x:02x}")).collect();
            Ok(vec![format!(
                "= log {} dav {hex}",
                b01(log[..] == kit("efuse-log.bin")[..2048])
            )])
        });
    }

    // ---- 2. firmware tables and the walker ----
    let t = phy::PhyTables::from_firmware(&fw, 0);
    {
        let mut b = ChipBus::new(Model::default());
        let n = |x: &Option<RegTable>| x.as_ref().unwrap().regs.len();
        out.on("tables", &mut b, |_| {
            Ok(vec![format!(
                "= tables {},{},{},{},{} paths {},{} afe {} rf2 {}",
                n(&t.bb),
                n(&t.bb_gain),
                n(&t.rf[0]),
                n(&t.rf[1]),
                n(&t.nctl),
                t.rf[0].as_ref().unwrap().rf_path,
                t.rf[1].as_ref().unwrap().rf_path,
                b01(t.afe.is_some()),
                b01(t.rf[2].is_some())
            )])
        });
        for (name, tb) in [
            ("BB_REG", &t.bb),
            ("BB_GAIN", &t.bb_gain),
            ("RADIO_A", &t.rf[0]),
            ("RADIO_B", &t.rf[1]),
            ("RF_NCTL", &t.nctl),
        ] {
            for (rfe, cv) in [(1, 1), (1, 0), (2, 1), (0x29, 1), (1, 2), (5, 1), (0, 0)] {
                out.on(&format!("walk {name} {rfe}/{cv}"), &mut b, |_| {
                    Ok(walk_line(name, tb.as_ref().unwrap(), rfe, cv, false))
                });
            }
        }
        let h = |rfe: u32, cv: u32| 0xf000_0000 | (rfe << 16) | cv;
        let (iff, elif) = (|x: u32| 0x8000_0000 | x, |x: u32| 0x9000_0000 | x);
        let (chk, els, end) = (0x4000_0000u32, 0xa000_0000u32, 0xb000_0000u32);
        let smalls = [
            (
                "small",
                mk(
                    &[
                        (h(1, 0), 0),
                        (h(1, 3), 0),
                        (h(0xff, 2), 0),
                        (0x10, 1),
                        (iff(0x10003), 0),
                        (chk, 0),
                        (0x11, 2),
                        (elif(0x10000), 0),
                        (chk, 0),
                        (0x12, 3),
                        (els, 0),
                        (0x13, 4),
                        (end, 0),
                        (0x14, 5),
                    ],
                    0,
                ),
            ),
            (
                "case2",
                mk(
                    &[(h(1, 3), 0), (h(1, 0xff), 0), (h(1, 0xff), 0), (0x10, 1)],
                    0,
                ),
            ),
            ("nohead", mk(&[(h(2, 3), 0), (0x10, 1)], 0)),
            ("flat", mk(&[(0x20, 1), (0x21, 2)], 0)),
            (
                "double check",
                mk(
                    &[
                        (iff(0x10001), 0),
                        (chk, 0),
                        (0x30, 1),
                        (elif(0x10001), 0),
                        (chk, 0),
                        (0x31, 2),
                        (end, 0),
                        (0x32, 3),
                    ],
                    0,
                ),
            ),
        ];
        for (name, tb) in &smalls {
            for (rfe, cv) in [(1, 1), (1, 0), (7, 0), (0xff, 2)] {
                out.on(&format!("walk {name} {rfe}/{cv}"), &mut b, |_| {
                    Ok(walk_line(name, tb, rfe, cv, true))
                });
            }
        }
    }

    // ---- 3. config functions, the RF H2C store ----
    {
        let mut b = ChipBus::new(Model::default());
        out.on("config_bb_reg", &mut b, |b| {
            for (a, v) in [
                (0xfe, 0),
                (0xfd, 0),
                (0xfc, 0),
                (0xfb, 0),
                (0xfa, 0),
                (0xf9, 0),
                (0x900, BYPASS_CR_DATA),
                (0x904, 7),
            ] {
                tbl::config_bb_reg(b, a, v)?;
            }
            Ok(vec![])
        });
        let mut b = ChipBus::new(Model::default());
        let mut info = RfRegInfo::new(RF_PATH_A);
        out.on("config_rf_reg_v1", &mut b, |b| {
            for (a, v) in [(0xfe, 0), (0x10005, 3), (0x05, 7), (0x105, 9), (0xfd, 1)] {
                tbl::config_rf_reg_v1(b, a, v, 0, &mut info)?;
            }
            Ok(vec![format!("= idx {}", info.curr_idx)])
        });
        let mut b = ChipBus::new(Model::default());
        let mut info_b = RfRegInfo::new(RF_PATH_B);
        out.on("config_rf_reg_v1 path B direct", &mut b, |b| {
            for (a, v) in [(0x10018, 0x12345), (0x10042, 0x1), (0x18, 0xfffff)] {
                tbl::config_rf_reg_v1(b, a, v, 1, &mut info_b)?;
            }
            Ok(vec![format!("= idx {}", info_b.curr_idx)])
        });
        let mut d = new_dev(&fw, None, 0);
        let m0 = d.b.writes.len();
        out.on("rf reg upload", &mut d.b, |b| {
            fw::rf_reg_upload(b, &mut d.rings, &mut d.fws, &mut info)?;
            b.flush()?;
            Ok(h2c_lines(b, &d.rings, m0))
        });
    }

    // ---- 4. PHY capabilities from the firmware ----
    for (name, c2h) in [
        (
            "1,2,2,2",
            phycap_c2h(1, 2, 2, 2, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP),
        ),
        (
            "0,3,1,2 11be",
            phycap_c2h(
                0,
                3,
                1,
                2,
                RTW89_C2HREG_PHYCAP_W1_PROT_11BE,
                RTW89_FWCMD_C2HREG_FUNC_PHY_CAP,
            ),
        ),
        (
            "2,2,2,1",
            phycap_c2h(2, 2, 2, 1, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP),
        ),
        (
            "wrong id",
            phycap_c2h(2, 2, 0, 0, 3, RTW89_FWCMD_C2HREG_FUNC_NULL),
        ),
    ] {
        let mut d = new_dev(&fw, Some(c2h), 0);
        phy::efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
        if name == "1,2,2,2" {
            out.on("dm init before phycap", &mut d.b, |b| {
                phy::phy_dm_init(b, &mut d.phy, None, 0, None, &mut d.spy)?;
                Ok(vec![])
            });
            out.logs(&mut d.phy);
        }
        out.on(&format!("mac_setup_phycap {name}"), &mut d.b, |b| {
            let c = phy::mac_setup_phycap(b, &mut d.phy, &mut d.fws, 0)?;
            Ok(vec![
                format!("= cap {},{},{},{}", c.tx_nss, c.rx_nss, c.tx_ant, c.rx_ant),
                hal_line(&d.phy.hal),
            ])
        });
        out.logs(&mut d.phy);
    }

    // ---- 5. BB/RF reset, BB table and RF tables ----
    {
        let mut d = new_dev(&fw, None, 0);
        phy::efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
        phy::mac_setup_phycap(&mut d.b, &mut d.phy, &mut d.fws, 0).unwrap();
        out.on("reset_bb_rf", &mut d.b, |b| {
            phy::reset_bb_rf(b)?;
            Ok(vec![])
        });
        out.logs(&mut d.phy);
        out.on("init_bb_reg", &mut d.b, |b| {
            phy::init_bb_reg(b, &mut d.phy, &mut d.spy)?;
            let mut v = d.spy.take();
            v.push(load_line("bb", d.phy.loads.bb.as_ref(), false));
            v.push(load_line("bbgain", d.phy.loads.bb_gain.as_ref(), false));
            v.extend(gain_lines(&d.phy.bb_gain));
            Ok(v)
        });
        out.logs(&mut d.phy);
        let rf_loads = |p: &Phy| -> Vec<String> {
            p.loads
                .rf
                .iter()
                .enumerate()
                .map(|(i, l)| load_line(&format!("rf{i}"), Some(l), true))
                .collect()
        };
        let m0 = d.b.writes.len();
        out.on("init_rf_reg", &mut d.b, |b| {
            phy::init_rf_reg(b, &mut d.phy, &mut d.rings, &mut d.fws)?;
            let mut v = rf_loads(&d.phy);
            v.extend(h2c_lines(b, &d.rings, m0));
            Ok(v)
        });
        out.logs(&mut d.phy);
        d.phy.tables.as_mut().unwrap().rf = [Some(big(1100, 0)), Some(big(1600, 1)), None, None];
        let m0 = d.b.writes.len();
        out.on("init_rf_reg 1100 and 1600 values", &mut d.b, |b| {
            phy::init_rf_reg(b, &mut d.phy, &mut d.rings, &mut d.fws)?;
            let mut v = rf_loads(&d.phy);
            v.extend(h2c_lines(b, &d.rings, m0));
            Ok(v)
        });
        out.logs(&mut d.phy);
        d.phy.tables.as_mut().unwrap().rf = [Some(big(3, 0)), Some(big(3, 1)), None, None];
        let ch12 = d.rings.tx("CH12").unwrap().idx;
        d.b.poll_ok = Some(Box::new(move |a| a != ch12));
        out.on("init_rf_reg, CH12 stuck", &mut d.b, |b| {
            phy::init_rf_reg(b, &mut d.phy, &mut d.rings, &mut d.fws)?;
            Ok(rf_loads(&d.phy))
        });
        out.logs(&mut d.phy);
        d.phy.tables.as_mut().unwrap().rf = [Some(big(3, 0)), None, None, None];
        out.on("init_rf_reg, no RADIO_B", &mut d.b, |b| {
            phy::init_rf_reg(b, &mut d.phy, &mut d.rings, &mut d.fws)?;
            Ok(vec![])
        });
        out.logs(&mut d.phy);
    }
    {
        let mut d = new_dev(&fw, None, 0);
        phy::efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
        d.phy.tables.as_mut().unwrap().bb = None;
        out.on("init_bb_reg, no BB_REG", &mut d.b, |b| {
            phy::init_bb_reg(b, &mut d.phy, &mut d.spy)?;
            Ok(vec![])
        });
        out.logs(&mut d.phy);
    }

    // ---- 6. dm init ----
    let ready = |c2h: Option<[u32; 4]>| {
        let mut d = new_dev(&fw, c2h, 0);
        phy::efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
        phy::mac_setup_phycap(&mut d.b, &mut d.phy, &mut d.fws, 0).unwrap();
        d
    };
    {
        let mut d = ready(None);
        out.on("dm init", &mut d.b, |b| {
            phy::phy_dm_init(b, &mut d.phy, None, 0, None, &mut d.spy)?;
            let p = &d.phy;
            let g = &p.efuse.as_ref().unwrap().gain;
            let mut v = d.spy.take();
            v.push(format!(
                "= thermal {} max {} cap {} ofst {} bandedge {} nctl {} base {},{}",
                list(p.avg_thermal),
                p.stat.last_thermal_max,
                p.cfo.crystal_cap,
                p.cfo.x_cap_ofst,
                p.ul_tb.def_if_bandedge,
                b01(p.nctl_ready),
                g.offset_base[0],
                g.rssi_base[0]
            ));
            v.push(load_line("nctl", p.loads.nctl.as_ref(), false));
            v.extend(env_lines(&p.env));
            v.extend(dig_lines(&p.dig));
            v.extend(cfo_lines(&p.cfo));
            Ok(v)
        });
        out.logs(&mut d.phy);
        d.b.model.nctl_reads = 0;
        d.b.model.nctl = Box::new(|n| if n >= 2 { 4 } else { 0 });
        out.on("init_rf_nctl 3 rounds", &mut d.b, |b| {
            phy::init_rf_nctl(b, &mut d.phy)?;
            Ok(vec![format!("= nctl {}", b01(d.phy.nctl_ready))])
        });
        out.logs(&mut d.phy);
        d.b.model.nctl = Box::new(|_| 0);
        out.on("init_rf_nctl timeout", &mut d.b, |b| {
            phy::init_rf_nctl(b, &mut d.phy)?;
            Ok(vec![format!("= nctl {}", b01(d.phy.nctl_ready))])
        });
        out.logs(&mut d.phy);
        let mut e = ready(None);
        let ch = Chan {
            band: Band::B5g,
            ch: 36,
            bw: 80,
        };
        out.on("dm init 5g 80 linked", &mut e.b, |b| {
            phy::phy_dm_init(b, &mut e.phy, Some(ch), 1, None, &mut e.spy)?;
            let mut v = e.spy.take();
            v.extend(dig_lines(&e.phy.dig));
            Ok(v)
        });
        out.logs(&mut e.phy);
    }

    // ---- 7. tracking: thermal, CFO ----
    {
        let mut d = ready(None);
        phy::phy_dm_init(&mut d.b, &mut d.phy, None, 0, None, &mut d.spy).unwrap();
        d.spy.take();
        let th = |p: &Phy| {
            vec![format!(
                "= thermal {} max {} last {}",
                list(p.avg_thermal),
                p.stat.last_thermal_max,
                list(p.stat.thermal)
            )]
        };
        for (name, v) in [("thermal 0x20", 0x20), ("thermal 0", 0)] {
            d.b.model.thermal = v;
            out.on(name, &mut d.b, |b| {
                trk::stat_thermal_update(b, &mut d.phy)?;
                Ok(th(&d.phy))
            });
            out.logs(&mut d.phy);
        }
        d.b.model.thermal = 0x3f;
        d.phy.tssi_mode = [false, true];
        d.b.mem
            .poke(BB + 0x1c10 + (1 << 13), Width::W32, 0x2a00_0000);
        out.on("thermal path B in TSSI mode", &mut d.b, |b| {
            trk::stat_thermal_update(b, &mut d.phy)?;
            Ok(th(&d.phy))
        });
        out.logs(&mut d.phy);
        d.phy.tssi_mode = [false, false];
        out.on("cfo parse", &mut d.b, |_| {
            for _ in 0..10 {
                trk::cfo_parse(&mut d.phy, 200, 0);
            }
            trk::cfo_parse(&mut d.phy, 200, 64);
            trk::cfo_parse(&mut d.phy, -7, 63);
            Ok(cfo_lines(&d.phy.cfo))
        });
        out.logs(&mut d.phy);
        let track = |out: &mut Out, d: &mut Dev, name: &str, sta: u32, tp: u32, tf: u32| {
            out.on(name, &mut d.b, |b| {
                trk::cfo_track(b, &mut d.phy.cfo, sta, tp, tf)?;
                Ok(cfo_lines(&d.phy.cfo))
            });
            out.logs(&mut d.phy);
        };
        track(&mut out, &mut d, "cfo track", 1, 0, 0);
        track(&mut out, &mut d, "cfo track 150 Mbps", 1, 150, 0);
        for _ in 0..4 {
            trk::cfo_parse(&mut d.phy, -50, 0);
        }
        d.b.model.xtal.clear();
        out.on("cfo track work", &mut d.b, |b| {
            let ran = trk::cfo_track_work(b, &mut d.phy.cfo, 1)?;
            let mut v = vec![format!("= ran {}", b01(ran))];
            v.extend(cfo_lines(&d.phy.cfo));
            Ok(v)
        });
        for (tp, tf) in [(120, 400), (120, 400), (10, 0)] {
            track(
                &mut out,
                &mut d,
                &format!("cfo track {tp} Mbps {tf} tf"),
                1,
                tp,
                tf,
            );
        }
        out.on("cfo track work idle", &mut d.b, |b| {
            Ok(vec![format!(
                "= ran {}",
                b01(trk::cfo_track_work(b, &mut d.phy.cfo, 1)?)
            )])
        });
        track(&mut out, &mut d, "cfo track unassociated", 0, 0, 0);
        for round in 0..24 {
            for _ in 0..3 {
                trk::cfo_parse(&mut d.phy, 900, 5);
            }
            track(&mut out, &mut d, &format!("cfo walk {round}"), 1, 1, 0);
        }
        trk::cfo_parse(&mut d.phy, 300, 1);
        track(&mut out, &mut d, "cfo two stations", 2, 0, 0);
    }

    // ---- 8. environment monitor, DIG, EDCCA, UL TB ----
    {
        let mut d = ready(None);
        phy::phy_dm_init(&mut d.b, &mut d.phy, None, 0, None, &mut d.spy).unwrap();
        d.spy.take();
        out.on("env track, nothing ready", &mut d.b, |b| {
            env::env_monitor_track(b, &mut d.phy.env)?;
            Ok(env_lines(&d.phy.env))
        });
        for (r, m, v) in [
            (R_IFSCNT, B_IFSCNT_DONE_MSK, 1),
            (R_IFSCNT, B_IFSCNT_TOTAL_CNT_MSK, 900),
            (R_IFS_CLM_TX_CNT, B_IFS_CLM_TX_CNT_MSK, 3000),
            (R_IFS_CLM_TX_CNT, B_IFS_CLM_EDCCA_EXCLUDE_CCA_FA_MSK, 1200),
            (R_IFS_CLM_CCA, B_IFS_CLM_CCKCCA_EXCLUDE_FA_MSK, 40000),
            (R_IFS_CLM_CCA, B_IFS_CLM_OFDMCCA_EXCLUDE_FA_MSK, 77),
            (R_IFS_CLM_FA, B_IFS_CLM_CCK_FA_MSK, 500),
            (R_IFS_CLM_FA, B_IFS_CLM_OFDM_FA_MSK, 9000),
            (R_IFS_HIS, B_IFS_T1_HIS_MSK, 3),
            (R_IFS_HIS, B_IFS_T2_HIS_MSK, 0),
            (R_IFS_HIS, B_IFS_T3_HIS_MSK, 200),
            (R_IFS_HIS, B_IFS_T4_HIS_MSK, 9),
            (R_IFS_AVG_L, B_IFS_T1_AVG_MSK, 7),
            (R_IFS_AVG_L, B_IFS_T2_AVG_MSK, 100),
            (R_IFS_AVG_H, B_IFS_T3_AVG_MSK, 0xffff),
            (R_IFS_AVG_H, B_IFS_T4_AVG_MSK, 1),
            (R_IFS_CCA_L, B_IFS_T1_CCA_MSK, 30),
            (R_IFS_CCA_L, B_IFS_T2_CCA_MSK, 0),
            (R_IFS_CCA_H, B_IFS_T3_CCA_MSK, 60000),
            (R_IFS_CCA_H, B_IFS_T4_CCA_MSK, 77),
            (R_CLM_EDCCA_RDY, B_CLM_EDCCA_RDY, 1),
            (R_CLM_EDCCA_RESULT, B_CLM_EDCCA_RESULT, 45000),
        ] {
            poke_field(&mut d.b, r, m, v);
        }
        for name in ["env track, reports ready", "env track again"] {
            out.on(name, &mut d.b, |b| {
                env::env_monitor_track(b, &mut d.phy.env)?;
                Ok(env_lines(&d.phy.env))
            });
        }
        d.phy.env.ccx_manual_ctrl = true;
        out.on("env track, manual", &mut d.b, |b| {
            env::env_monitor_track(b, &mut d.phy.env)?;
            Ok(env_lines(&d.phy.env))
        });
        d.phy.env.ccx_manual_ctrl = false;
        d.phy.env.ccx_ongoing = true;
        d.phy.env.ccx_rac_lv = 3;
        out.on("env track, racing", &mut d.b, |b| {
            env::env_monitor_track(b, &mut d.phy.env)?;
            Ok(env_lines(&d.phy.env))
        });

        let ch = |p: &Phy| {
            vec![format!(
                "= ch rssi {} macid {}",
                p.ch_info.rssi_min, p.ch_info.rssi_min_macid
            )]
        };
        out.on("rssi update, no AP", &mut d.b, |_| {
            let mut v = vec![format!(
                "= changed {}",
                b01(dig::stat_rssi_update(&mut d.phy, None))
            )];
            v.extend(ch(&d.phy));
            Ok(v)
        });
        out.on("dig unlinked", &mut d.b, |b| {
            dig::phy_dig(b, &mut d.phy, 0, &CHAN_2G6)?;
            Ok(dig_lines(&d.phy.dig))
        });
        out.on("edcca unlinked", &mut d.b, |b| {
            dig::edcca_track(b, &mut d.phy, 0, 0)?;
            Ok(vec![format!("= edcca {}", d.phy.edcca_th_old)])
        });
        let mut ap = ApRssi {
            avg_rssi_internal: 80 << 10,
            mac_id: 3,
            prev_rssi: 0,
        };
        for (i, rssi, tp) in [
            (0, 80, 0),
            (1, 82, 20),
            (2, 100, 20),
            (3, 20, 0),
            (4, 160, 30),
            (5, 160, 30),
            (6, 160, 30),
        ] {
            ap.avg_rssi_internal = rssi << 10;
            out.on(&format!("rssi update {i}"), &mut d.b, |_| {
                let changed = dig::stat_rssi_update(&mut d.phy, Some(&mut ap));
                let mut v = vec![format!("= changed {} prev {}", b01(changed), ap.prev_rssi)];
                v.extend(ch(&d.phy));
                Ok(v)
            });
            out.on(&format!("dig linked {i}"), &mut d.b, |b| {
                dig::phy_dig(b, &mut d.phy, 1, &CHAN_2G6)?;
                Ok(dig_lines(&d.phy.dig))
            });
            out.on(&format!("edcca linked {i}"), &mut d.b, |b| {
                dig::edcca_track(b, &mut d.phy, 1, tp)?;
                Ok(vec![format!(
                    "= edcca {} diag {}",
                    d.phy.edcca_th_old, d.phy.diag_edcca_cnt
                )])
            });
        }
        d.phy.dig.bypass_dig = true;
        out.on("dig bypass", &mut d.b, |b| {
            dig::phy_dig(b, &mut d.phy, 1, &CHAN_2G6)?;
            Ok(dig_lines(&d.phy.dig))
        });
        d.phy.dig.pause_dig = true;
        out.on("dig paused", &mut d.b, |b| {
            dig::phy_dig(b, &mut d.phy, 1, &CHAN_2G6)?;
            Ok(dig_lines(&d.phy.dig))
        });
        d.phy.dig.pause_dig = false;
        let ch5 = Chan {
            band: Band::B5g,
            ch: 100,
            bw: 40,
        };
        out.on("dig 5g unlinked again", &mut d.b, |b| {
            dig::phy_dig(b, &mut d.phy, 0, &ch5)?;
            Ok(dig_lines(&d.phy.dig))
        });

        let sta = UlTbLink {
            total_sta_assoc: 1,
            station_assoc: true,
            dyn_tb_bedge_en: true,
            def_tri_idx: 2,
        };
        for (name, link, tf) in [
            ("high", sta, 200),
            ("low", sta, 10),
            ("between", sta, 80),
            (
                "no bedge",
                UlTbLink {
                    dyn_tb_bedge_en: false,
                    ..sta
                },
                10,
            ),
            (
                "not assoc",
                UlTbLink {
                    station_assoc: false,
                    ..sta
                },
                200,
            ),
            (
                "two stations",
                UlTbLink {
                    total_sta_assoc: 2,
                    ..sta
                },
                200,
            ),
        ] {
            out.on(&format!("ul tb {name}"), &mut d.b, |b| {
                dig::ul_tb_ctrl_track(b, &mut d.phy, &link, tf)?;
                Ok(vec![])
            });
        }
        d.phy.ul_tb.dyn_tb_tri_en = false;
        out.on("ul tb, triangle off", &mut d.b, |b| {
            dig::ul_tb_ctrl_track(b, &mut d.phy, &sta, 200)?;
            Ok(vec![])
        });
        out.logs(&mut d.phy);
    }

    // ---- 9. AFE, RX status, the BB helpers ----
    {
        let mut d = new_dev(&fw, None, 0);
        phy::efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
        out.on("bb afe, none", &mut d.b, |b| {
            Ok(vec![format!("= rows {}", phy::bb_afe(b, &mut d.phy)?)])
        });
        out.logs(&mut d.phy);
        let rows: [[u32; 6]; 7] = [
            [0, 2, 5, 0x1234, 0xff00, 0x12],
            [0, 0, 0, 0x4000, 0xf, 3],
            [1, 0, 5, 50, 0, 0],
            [2, 2, 0, 0x88, 0x1, 0],
            [2, 2, 1, 0x8c, 0xff, 0x5a],
            [0, 1, 3, 0x4004, 0xffff_ffff, 7],
            [0, 4, 1, 0x2000, 0xffff_ffff, 9],
        ];
        let mut afe = vec![0u8; 8 + 7 * 24];
        for (i, r) in rows.iter().enumerate() {
            for (j, v) in r.iter().enumerate() {
                let o = 8 + i * 24 + j * 4;
                afe[o..o + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        d.phy.tables.as_mut().unwrap().afe = Some((7 * 24, afe));
        out.on("bb afe, seven rows", &mut d.b, |b| {
            Ok(vec![format!("= rows {}", phy::bb_afe(b, &mut d.phy)?)])
        });
        out.logs(&mut d.phy);
        let rxs: [(u8, u8, u8, u8, u8, bool); 10] = [
            (100, 110, 100, 90, 0x06, true),
            (0, 0, 80, 60, 0x24, true),
            (120, 100, 130, 110, 0, true),
            (50, 60, 40, 255, 0x0e, true),
            (50, 50, 70, 70, 0x5c, true),
            (50, 50, 70, 70, 0x36, true),
            (50, 50, 70, 70, 0x71, true),
            (50, 50, 70, 70, 0x81, true),
            (50, 50, 70, 70, 0xf3, true),
            (50, 50, 70, 70, 0x16, false),
        ];
        out.on("rx status", &mut d.b, |_| {
            Ok(rxs
                .iter()
                .map(|&(rssi_avg, rpl_avg, a, b, chan_idx, valid)| {
                    let s = phy::rx_status(&mut Ppdu {
                        rssi_avg,
                        rpl_avg,
                        rssi: [a, b],
                        chan_idx,
                        valid,
                    });
                    format!(
                        "= rx {} {} {} {} {} {}",
                        s.signal,
                        list(s.chain_signal),
                        s.chains,
                        band_text(s.band),
                        opt(s.ch),
                        opt(s.freq)
                    )
                })
                .collect())
        });

        let ok = |r: Result<(), Rtw89Error>| r.map(|_| vec![]);
        helper(&mut out, "set_plcp_tx", |b| ok(bbx::bb_set_plcp_tx(b)));
        for en in [false, true] {
            let e = b01(en);
            helper(&mut out, &format!("nbtg_bt_tx {e}"), |b| {
                ok(bbx::ctrl_nbtg_bt_tx(b, en))
            });
            helper(&mut out, &format!("btg_bt_rx {e}"), |b| {
                ok(bbx::ctrl_btg_bt_rx(b, en))
            });
            helper(&mut out, &format!("reset_en 2g {e}"), |b| {
                ok(bbx::bb_reset_en(b, Band::B2g, en))
            });
            helper(&mut out, &format!("reset_en 5g {e}"), |b| {
                ok(bbx::bb_reset_en(b, Band::B5g, en))
            });
        }
        helper(&mut out, "pmac start", |b| {
            ok(bbx::bb_set_pmac_pkt_tx(b, true, 10, 20, Band::B2g))
        });
        helper(&mut out, "pmac stop 2g", |b| {
            ok(bbx::bb_set_pmac_pkt_tx(b, false, 10, 20, Band::B2g))
        });
        helper(&mut out, "pmac stop 5g", |b| {
            ok(bbx::bb_set_pmac_pkt_tx(b, false, 10, 20, Band::B5g))
        });
        helper(&mut out, "tx_mode_switch 0", |b| {
            ok(bbx::bb_tx_mode_switch(b, 0))
        });
        helper(&mut out, "tx_mode_switch 1", |b| {
            ok(bbx::bb_tx_mode_switch(b, 1))
        });
        helper(&mut out, "set_power -5", |b| ok(bbx::bb_set_power(b, -5)));
        for (n, p) in [
            ("RF_PATH_A", RF_PATH_A),
            ("RF_PATH_B", RF_PATH_B),
            ("RF_PATH_AB", RF_PATH_AB),
        ] {
            helper(&mut out, &format!("cfg_tx_path {n}"), |b| {
                ok(bbx::bb_cfg_tx_path(b, p))
            });
        }
        helper(&mut out, "backup/restore tssi", |b| {
            let bak = bbx::bb_backup_tssi(b)?;
            bbx::bb_restore_tssi(b, &bak)?;
            Ok(vec![format!(
                "= bak {} {} {} {} {}",
                bak.tx_path,
                bak.rx_path,
                bak.tx_pwr,
                hx(bak.p0_rfmode),
                hx(bak.p1_rfmode_ftm)
            )])
        });
        for (n, p) in [("RF_A", RF_A), ("RF_B", RF_B), ("RF_AB", RF_AB), ("0", 0)] {
            helper(&mut out, &format!("rf_mode_rx_path {n}"), |b| {
                ok(bbx::bb_ctrl_rf_mode_rx_path(b, p))
            });
        }
        helper(&mut out, "bb reset", |b| ok(bbx::bb_reset(b)));
        helper(&mut out, "bb sethw", |b| {
            let mut g = efuse::EfuseGain::default();
            bbx::bb_sethw(b, &mut g)?;
            Ok(vec![format!(
                "= base {},{}",
                g.offset_base[0], g.rssi_base[0]
            )])
        });
        helper(&mut out, "get_thermal A", |b| {
            Ok(vec![format!(
                "= thermal {}",
                bbx::get_thermal(b, 0, false)?
            )])
        });
        helper(&mut out, "get_thermal B tssi", |b| {
            b.mem.poke(BB + 0x3c10, Width::W32, 0x2a00_0000);
            Ok(vec![format!("= thermal {}", bbx::get_thermal(b, 1, true)?)])
        });
        helper(&mut out, "disable_bb_rf", |b| ok(bbx::disable_bb_rf(b)));
        helper(&mut out, "physts parsing init", |b| {
            ok(dm::physts_parsing_init(b))
        });
    }

    // ---- the C's sequences (here: what the Rust side does for each) ----
    let walk = |b: &mut ChipBus, t: &Option<RegTable>, rf: Option<u32>| {
        let t = t.as_ref().unwrap();
        let mut info = RfRegInfo::new(t.rf_path);
        tbl::init_reg(t, 1, 1, |a, d| match rf {
            None => tbl::config_bb_reg(b, a, d),
            Some(p) => tbl::config_rf_reg_v1(b, a, d, p, &mut info).map(|_| ()),
        })
        .unwrap();
    };
    out.spec("reset_bb_rf", Model::default(), |b| {
        phy::reset_bb_rf(b).unwrap()
    });
    out.spec("bb_reg + bb_reset", Model::default(), |b| {
        walk(b, &t.bb, None);
        bbx::bb_reset(b).unwrap();
    });
    for path in [0u32, 1] {
        out.spec(&format!("radio {path}"), Model::default(), |b| {
            walk(b, &t.rf[path as usize], Some(path))
        });
    }
    out.spec("dm_init", Model::default(), |b| {
        let mut p = Phy::new(1);
        p.tables = Some(t.clone());
        let mut m = EfuseMaps {
            phycap: Some(vec![0xff; 128]),
            ..maps()
        };
        phy::efuse_parse(b, &mut p, &mut m).unwrap();
        p.hal.rx_nss = 2;
        phy::phy_dm_init(b, &mut p, None, 0, None, &mut Spy::default()).unwrap();
    });
    let nctl3 = Model {
        nctl: Box::new(|n| if n >= 2 { 4 } else { 0 }),
        ..Model::default()
    };
    out.spec("preinit nctl 3", nctl3, |b| {
        dm::preinit_rf_nctl(b).unwrap();
    });
    let nctl0 = Model {
        nctl: Box::new(|_| 0),
        ..Model::default()
    };
    out.spec("preinit nctl 101", nctl0, |b| {
        dm::preinit_rf_nctl(b).unwrap();
    });
    out.spec("set cap 0x3f", Model::default(), |b| {
        trk::set_crystal_cap(b, &mut Cfo::default(), 0x3f, true).unwrap()
    });
    out.spec("pmac_ht20_mcs7_tbl", Model::default(), |b| {
        bbx::bb_set_plcp_tx(b).unwrap()
    });
    out.spec("preagc dis", Model::default(), |b| {
        bbx::ctrl_nbtg_bt_tx(b, false).unwrap()
    });
    out.spec("preagc en", Model::default(), |b| {
        bbx::ctrl_nbtg_bt_tx(b, true).unwrap()
    });
    for en in [false, true] {
        out.spec(&format!("btg_bt_rx {}", b01(en)), Model::default(), |b| {
            bbx::ctrl_btg_bt_rx(b, en).unwrap()
        });
        out.spec(&format!("reset_en 2g {}", b01(en)), Model::default(), |b| {
            bbx::bb_reset_en(b, Band::B2g, en).unwrap()
        });
    }
    out.spec("pmac start", Model::default(), |b| {
        bbx::bb_set_pmac_pkt_tx(b, true, 10, 20, Band::B2g).unwrap()
    });
    out.spec("pmac stop 5g", Model::default(), |b| {
        bbx::bb_set_pmac_pkt_tx(b, false, 10, 20, Band::B5g).unwrap()
    });
    out.spec("tx_mode_switch 0", Model::default(), |b| {
        bbx::bb_tx_mode_switch(b, 0).unwrap()
    });
    out.spec("set_power -5", Model::default(), |b| {
        bbx::bb_set_power(b, -5).unwrap()
    });
    out.spec("cfg_tx_path AB", Model::default(), |b| {
        bbx::bb_cfg_tx_path(b, RF_PATH_AB).unwrap()
    });
    out.spec("rf_mode_rx_path AB", Model::default(), |b| {
        bbx::bb_ctrl_rf_mode_rx_path(b, RF_AB).unwrap()
    });
    out.spec("cfo dm", Model::default(), |b| {
        // the state cfo_init leaves for the efuse's 0x3c, ten PPDUs of 200 kHz on MAC ID 0
        let mut p = Phy::new(1);
        let c = &mut p.cfo;
        c.crystal_cap_default = 0x3c;
        c.crystal_cap = 0x3c;
        c.def_x_cap = 0x3c;
        c.x_cap_ub = 0x7c;
        c.x_cap_lb = 1;
        c.cfo_ul_ofdma_acc_mode = RTW89_CFO_UL_OFDMA_ACC_ENABLE;
        for _ in 0..10 {
            trk::cfo_parse(&mut p, 200, 0);
        }
        trk::cfo_track(b, &mut p.cfo, 1, 0, 0).unwrap();
    });
    out.0
}

// struct rtw8852bx_efuse offsets of the RX gain bytes (phy-test-expect.mjs OF)
const EF_RX_GAIN2G_OFDM_OF: usize = EF_RX_GAIN2G_OFDM;
const EF_RX_GAIN2G_CCK_OF: usize = EF_RX_GAIN2G_CCK;
const EF_RX_GAIN5G_LOW_OF: usize = EF_RX_GAIN5G_LOW;
const EF_RX_GAIN5G_MID_OF: usize = EF_RX_GAIN5G_MID;
const EF_RX_GAIN5G_HIGH_OF: usize = EF_RX_GAIN5G_HIGH;
use rtw89_core::tables::phy::{
    EF_RX_GAIN2G_CCK, EF_RX_GAIN2G_OFDM, EF_RX_GAIN5G_HIGH, EF_RX_GAIN5G_LOW, EF_RX_GAIN5G_MID,
};

fn js_side() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/phy-js.mjs");
    let o = Command::new("node")
        .arg(&script)
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        o.status.success(),
        "phy-js.mjs: {}\n{}",
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn phy_does_what_the_js_driver_and_the_c_do() {
    if skip("phy_does_what_the_js_driver_and_the_c_do") {
        return;
    }
    let js = js_side();
    let ours = rust_side();
    let mut scenario = String::new();
    for (i, (o, j)) in ours.iter().zip(js.iter()).enumerate() {
        if j.starts_with("## ") {
            scenario = j.clone();
        }
        assert_eq!(o, j, "line {} in {scenario}", i + 1);
    }
    assert_eq!(ours.len(), js.len(), "as many lines");
    let scenarios = js.iter().filter(|l| l.starts_with("## ")).count();
    let specs = js.iter().filter(|l| l.starts_with("## spec ")).count();
    assert!(
        scenarios >= 200 && specs == 22,
        "{scenarios} scenarios, {specs} specs"
    );
    assert!(js.len() > 26000, "{} lines", js.len());
}

/// The spec sections are the C's sequences (phy-test-expect.mjs), not the JS driver's: the
/// Rust ops equal them on their own, so a JS regression cannot hide a Rust one
#[test]
fn spec_sections_are_long_enough() {
    if skip("spec_sections_are_long_enough") {
        return;
    }
    let js = js_side();
    let count = |name: &str| {
        let at = js.iter().position(|l| l == name).unwrap();
        js[at + 1..]
            .iter()
            .take_while(|l| !l.starts_with("## "))
            .count()
    };
    assert_eq!(
        count("## spec dm_init"),
        1563,
        "test-phy.mjs: dm init 1563 ops"
    );
    assert_eq!(count("## spec pmac_ht20_mcs7_tbl"), 120);
    assert_eq!(count("## spec preinit nctl 101"), 6 + 101 * 3 + 100);
    let _ = (Dig::default(), Hal::default(), UlTbLink::default());
}
