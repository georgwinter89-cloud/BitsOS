//! "Never panic on chip values, files or state": the whole phy surface against a hostile chip
//! and hostile inputs. The chip answers every read with a constant (0, all ones, 0xDEADBEEF,
//! 0xEAEAEAEA), a seeded xorshift stream, or test-phy.mjs's chip model with one access in `n`
//! garbage or a timed-out poll; the clock is frozen. Each call must return Ok or Err (the debug
//! build panics on an overflow, so it is caught here) within an op budget (no endless wait).
//! Then the pure parts get random tables, efuse maps, PPDUs and tracking state.

use crate::rfk1::MacLink;
use alloc::vec;
use alloc::vec::Vec;

use super::chipbus::{ChipBus, Model, phycap_c2h};
use super::*;
use crate::bus::{BusError, BusResult, MemWidth, Width};
use crate::fw::{Firmware, FwState, RfRegInfo};
use crate::pci::{PciRings, RingConfig, pci_alloc};

/// The most ops one call may take: the phycap C2H wait is 1 s in 1 us steps on a frozen
/// clock (1 M reads), everything else far below
const BUDGET: u64 = 5_000_000;

#[derive(Debug, Clone, Copy)]
enum Src {
    Const(u32),
    Rand,
    /// the chip model, one access in `n` faulty
    Flaky(u64),
}

struct Hostile {
    src: Src,
    rng: u64,
    good: ChipBus,
    ops: u64,
}

impl Hostile {
    fn new(src: Src, seed: u64) -> Hostile {
        Hostile {
            src,
            rng: seed | 1,
            good: ChipBus::new(Model {
                c2h: phycap_c2h(2, 2, 0, 0, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP),
                ..Model::default()
            }),
            ops: 0,
        }
    }
    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
    fn fault(&mut self) -> bool {
        match self.src {
            Src::Flaky(n) => self.rand().is_multiple_of(n),
            _ => true,
        }
    }
    fn garbage(&mut self) -> u32 {
        match self.src {
            Src::Const(v) => v,
            _ => (self.rand() >> 16) as u32,
        }
    }
    fn op(&mut self) -> BusResult<()> {
        self.ops += 1;
        if self.good.mem.trace.len() > 4096 {
            self.good.mem.trace.clear();
        }
        if self.ops > BUDGET {
            return Err(BusError::Gone);
        }
        Ok(())
    }
}

impl Bus for Hostile {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.op()?;
        let v = self.good.read(w, addr)?;
        Ok(if self.fault() {
            self.garbage() & w.mask()
        } else {
            v
        })
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.op()?;
        self.good.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.op()?;
        self.good.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.op()?;
        let r = self.good.poll(w, addr, mask, want, ms);
        if self.fault() {
            let last = self.garbage();
            return Err(BusError::PollTimeout {
                addr,
                width: w,
                mask,
                want,
                last,
            });
        }
        r
    }
    fn delay_us(&mut self, _us: u32) -> BusResult<()> {
        // the frozen clock: a wait never moves it
        self.op()
    }
    fn cfg_read(&mut self, _off: u16) -> BusResult<u32> {
        self.op()?;
        Ok(self.garbage())
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.op()?;
        self.good.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.good.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.good.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.op()?;
        self.good.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, _addr: u32) -> BusResult<u32> {
        self.op()?;
        Ok(self.garbage())
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.good.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.good.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        0
    }
}

struct NoOps;
impl<B: Bus + ?Sized> ChipOps<B> for NoOps {
    fn init_txpwr_unit(&mut self, _: &mut B, _: &Phy) -> Rtw89Result<()> {
        Ok(())
    }
    fn rfk_init(&mut self, _: &mut B, _: &mut Phy, _: Option<MacLink<'_>>) -> Rtw89Result<()> {
        Ok(())
    }
    fn set_txpwr_ctrl(&mut self, _: &mut B, _: &Phy) -> Rtw89Result<()> {
        Ok(())
    }
    fn power_trim(&mut self, _: &mut B, _: &Phy) -> Rtw89Result<()> {
        Ok(())
    }
    fn cfg_txrx_path(&mut self, _: &mut B, _: &Phy) -> Rtw89Result<()> {
        Ok(())
    }
}

fn firmware() -> Firmware {
    let p = std::path::PathBuf::from(
        std::env::var_os("WLAN_TOOLS")
            .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into()),
    )
    .join("firmware/rtw8852b_fw-2.bin");
    Firmware::parse(std::fs::read(p).expect("the firmware file"), 1, false).unwrap()
}

/// Every phy call in the bring-up's order and the track work's, each within the budget;
/// returns how many went through
fn sweep(b: &mut Hostile, fw: &Firmware) -> usize {
    let mut ok = 0;
    let mut run = |b: &mut Hostile, r: Rtw89Result<()>| {
        assert!(b.ops <= BUDGET + 1, "a call ran past the op budget");
        b.ops = 0;
        if r.is_ok() {
            ok += 1;
        }
    };
    let mut rings: PciRings = pci_alloc(&mut b.good, &RingConfig::js()).unwrap();
    let mut fws = FwState::new();
    let mut phy = Phy::new(1);
    phy.load_tables(fw);
    let r = efuse::dump_maps(b, 1, &mut |_, _| {});
    let mut maps = r.unwrap_or_default();
    run(b, Ok(()));
    let r = efuse_parse(b, &mut phy, &mut maps);
    run(b, r);
    if phy.efuse.is_none() {
        phy.efuse = Some(Efuse::default());
    }
    let r = mac_setup_phycap(b, &mut phy, &mut fws, 0).map(|_| ());
    run(b, r);
    phy.hal.rx_nss = phy.hal.rx_nss.max(1);
    let r = bb_afe(b, &mut phy).map(|_| ());
    run(b, r);
    let r = reset_bb_rf(b);
    run(b, r);
    let r = init_bb_reg(b, &mut phy, &mut NoOps);
    run(b, r);
    let r = init_rf_reg(b, &mut phy, &mut rings, &mut fws);
    run(b, r);
    let r = check_tables(&phy);
    run(b, r);
    let far = Chan {
        band: Band::B5g,
        ch: 165,
        bw: 160,
    };
    for (chan, sta) in [(None, 0), (Some(far), 3)] {
        let r = phy_dm_init(b, &mut phy, chan, sta, None, &mut NoOps);
        run(b, r);
    }
    let r = init_rf_nctl(b, &mut phy);
    run(b, r);
    let mut ap = ApRssi {
        avg_rssi_internal: u32::MAX,
        mac_id: u32::MAX,
        prev_rssi: 3,
    };
    for round in 0..6u32 {
        let r = trk::stat_thermal_update(b, &mut phy);
        run(b, r);
        let r = env::env_monitor_track(b, &mut phy.env);
        run(b, r);
        dig::stat_rssi_update(&mut phy, Some(&mut ap));
        for sta in [0, 1, 2] {
            let ch = Chan {
                band: [Band::B2g, Band::B5g, Band::B6g][round as usize % 3],
                ch: round,
                bw: [20, 40, 80, 160, 0, 7][round as usize],
            };
            let r = dig::phy_dig(b, &mut phy, sta, &ch);
            run(b, r);
            let r = dig::edcca_track(b, &mut phy, sta, u32::MAX);
            run(b, r);
            let link = UlTbLink {
                total_sta_assoc: sta,
                station_assoc: true,
                dyn_tb_bedge_en: true,
                def_tri_idx: u32::MAX,
            };
            let r = dig::ul_tb_ctrl_track(b, &mut phy, &link, round * 60);
            run(b, r);
            for v in [i32::MIN, -1, 0, 900, i32::MAX] {
                trk::cfo_parse(&mut phy, v, round);
            }
            let r = trk::cfo_track(b, &mut phy.cfo, sta, round * 50, u32::MAX);
            run(b, r);
            let r = trk::cfo_track_work(b, &mut phy.cfo, sta).map(|_| ());
            run(b, r);
        }
    }
    // the BB helpers with any argument
    for &v in &[0u32, 1, 2, 3, 4, u32::MAX] {
        let en = v & 1 == 1;
        let band = [Band::B2g, Band::B5g, Band::B6g][(v % 3) as usize];
        let rs = [
            bbx::bb_reset_en(b, band, en),
            bbx::bb_set_pmac_pkt_tx(b, en, v, v, band),
            bbx::bb_set_power(b, v as i32),
            bbx::bb_cfg_tx_path(b, v),
            bbx::bb_tx_mode_switch(b, v),
            bbx::ctrl_nbtg_bt_tx(b, en),
            bbx::ctrl_btg_bt_rx(b, en),
            bbx::bb_ctrl_rf_mode_rx_path(b, v),
            bbx::get_thermal(b, v, en).map(|_| ()),
            bbx::bb_backup_tssi(b).and_then(|k| bbx::bb_restore_tssi(b, &k)),
            bbx::bb_sethw(b, &mut efuse::EfuseGain::default()),
            bbx::bb_set_plcp_tx(b),
            io::rf_r(b, v, v, v).map(|_| ()),
            io::rf_w(b, v, v, v, v),
        ];
        for r in rs {
            run(b, r);
        }
    }
    ok
}

#[test]
fn a_hostile_chip_never_panics() {
    let fw = firmware();
    for src in [
        Src::Const(0),
        Src::Const(0xffff_ffff),
        Src::Const(0xdead_beef),
        Src::Const(0xeaea_eaea),
        Src::Rand,
    ] {
        sweep(&mut Hostile::new(src, 7), &fw);
    }
    let mut seen_ok = 0;
    for n in [3, 7, 50, 400, 5000] {
        for seed in 1..4 {
            seen_ok += sweep(&mut Hostile::new(Src::Flaky(n), seed), &fw);
        }
    }
    assert!(seen_ok > 0, "some calls went through on the flaky chip");
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[test]
fn hostile_tables_maps_and_ppdus_never_panic() {
    let mut s = 0x1234_5678_9abc_def1u64;
    // random tables through the walker, with conditions mixed in
    for _ in 0..2000 {
        let n = (xorshift(&mut s) % 40) as usize;
        let regs = (0..n)
            .map(|_| {
                let r = xorshift(&mut s);
                let cond: u32 = [0, 0, 0, 0x4, 0x8, 0x9, 0xa, 0xb, 0xf][(r % 9) as usize];
                (
                    (cond << 28) | ((r >> 8) as u32 & 0x0fff_ffff),
                    (r >> 40) as u32,
                )
            })
            .collect();
        let t = RegTable {
            regs,
            rf_path: 0,
            ver: [0; 4],
        };
        let (rfe, cv) = (
            (xorshift(&mut s) % 300) as u32,
            (xorshift(&mut s) % 300) as u32,
        );
        let _ = tbl::sel_headline(&t, rfe, cv);
        let mut g = BbGain::default();
        let r = tbl::init_reg(&t, rfe, cv, |a, d| {
            let _ = tbl::config_bb_gain_ax(&mut g, a, d);
            Ok(())
        });
        assert!(r.is_ok());
    }
    // random efuse maps of any length
    for len in [0usize, 1, 5, 100, 1215, 1216, 1300, 2048, 2064, 4000] {
        for _ in 0..20 {
            let m: Vec<u8> = (0..len).map(|_| xorshift(&mut s) as u8).collect();
            let mut log = vec![0xffu8; 2048];
            let _ = efuse::logical_efuse_map(&m, &mut log);
            let mut short = vec![0xffu8; (xorshift(&mut s) % 40) as usize];
            let _ = efuse::logical_efuse_map(&m, &mut short);
            let maps = EfuseMaps {
                ctrl: None,
                log: m.clone(),
                dav: Some(m.clone()),
                phycap: None,
            };
            let _ = efuse::logical_maps(&maps);
            let mut e = Efuse::default();
            efuse::read_efuse_8852bx(&m, &mut e);
            efuse::read_phycap_8852bx(&m, &mut e);
        }
    }
    // PPDUs with any bytes
    for _ in 0..5000 {
        let r = xorshift(&mut s);
        let mut p = Ppdu {
            rssi_avg: r as u8,
            rpl_avg: (r >> 8) as u8,
            rssi: [(r >> 16) as u8, (r >> 24) as u8],
            chan_idx: (r >> 32) as u8,
            valid: r & (1 << 40) != 0,
        };
        let _ = rx_status(&mut p);
    }
    // tracking state left anywhere by a caller
    let mut phy = Phy::new(1);
    phy.cfo.cfo_tail = vec![i32::MAX; 3];
    phy.cfo.cfo_cnt = vec![u32::MAX; 5];
    phy.cfo.crystal_cap = u32::MAX;
    phy.cfo.lock_cnt = u32::MAX;
    phy.cfo.phy_cfo_trk_cnt = u32::MAX;
    phy.cfo.divergence_lock_en = true;
    phy.dig.igi_fa_rssi = u32::MAX;
    phy.dig.fa_rssi_ofst = u32::MAX;
    phy.dig.pd_low_th_ofst = u32::MAX;
    phy.dig.dyn_pd_max_cnt = u32::MAX;
    phy.env.ifs_clm_cck_fa_permil = u32::MAX;
    phy.env.ifs_clm_ofdm_fa_permil = u32::MAX;
    phy.env.ccx_unit_idx = u32::MAX;
    phy.env.ccx_period = u32::MAX;
    phy.ch_info.rssi_min = u32::MAX;
    phy.stat.ewma_thermal = [u32::MAX; 2];
    let mut b = ChipBus::new(Model::default());
    for sta in [0, 1, 2] {
        trk::cfo_parse(&mut phy, i32::MAX, 0);
        trk::cfo_parse(&mut phy, i32::MIN, 2);
        let _ = trk::cfo_track(&mut b, &mut phy.cfo, sta, u32::MAX, u32::MAX);
        let _ = trk::cfo_track_work(&mut b, &mut phy.cfo, sta);
        let _ = trk::stat_thermal_update(&mut b, &mut phy);
        let _ = dig::phy_dig(&mut b, &mut phy, sta, &DEFAULT_CHAN);
        let _ = dig::edcca_track(&mut b, &mut phy, sta, u32::MAX);
        let _ = env::env_monitor_track(&mut b, &mut phy.env);
    }
    // the RF store past its pages: refused, never past the buffer
    let mut info = RfRegInfo::new(0);
    for i in 0..2000 {
        let _ = tbl::config_rf_reg_v1(&mut b, 0x100 + i, i, 0, &mut info);
    }
    assert_eq!(
        info.curr_idx,
        RTW89_H2C_RF_PAGE_SIZE * RTW89_H2C_RF_PAGE_NUM
    );
}
