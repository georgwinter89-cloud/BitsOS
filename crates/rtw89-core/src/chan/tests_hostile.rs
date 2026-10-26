//! "Never panic on chip values, files or state": every chan entry against rfk1's hostile chip
//! (constant answers 0, all ones, 0xDEADBEEF, 0xEAEAEAEA, a random stream, the chip model with
//! one access in 3/17/500 faulty; frozen clock; op budget), then random state (tables from random
//! element bytes, wild regulations, efuse gains, BB gains, antennas, channels, bandwidths) and
//! random REGD/TX power element bytes through the loaders. Each call returns Ok or Err (the debug
//! build panics on an overflow, so it is caught here).

use alloc::vec::Vec;

use super::fw::{TxpwrConf, load_regd_map_from, load_rfe_data_from};
use super::*;
use crate::phy::efuse::Efuse;
use crate::rfk1::fake::RfFake;
use crate::rfk1::tests_hostile::{BUDGET, Hostile, sources};
use crate::tables::chan::TXPWR_CONF;

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// The scheduler's TX enables from the chip, nothing else
struct Quiet;

impl<B: Bus + ?Sized> ChanHooks<B> for Quiet {
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        Ok(bus.r16(R_AX_CTN_TXEN)?)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        bus.w16(R_AX_CTN_TXEN, tx_en.into())?;
        Ok(())
    }
    fn set_channel_rf(&mut self, _: &mut B, _: &Chan) -> Rtw89Result<()> {
        Ok(())
    }
    fn btc_switch_band(&mut self, _: &mut B, _: Band) -> Rtw89Result<()> {
        Ok(())
    }
    fn rfk_band_changed(&mut self, _: &mut B, _: &mut Phy, _: &Chan) -> Rtw89Result<()> {
        Ok(())
    }
    fn rfk_channel(&mut self, _: &mut B, _: &mut Phy, _: &Chan) -> Rtw89Result<()> {
        Ok(())
    }
}

// TX power configurations of random bytes (sometimes claiming more entries than they hold)
fn wild_confs(next: &mut impl FnMut() -> u64) -> Vec<TxpwrConf> {
    TXPWR_CONF
        .iter()
        .map(|c| {
            let ent_sz = (next() % 20) as u8;
            let len = (next() % 200) as usize;
            let data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let mut num_ents = if ent_sz == 0 {
                (next() % 4) as u32
            } else {
                (len / usize::from(ent_sz)) as u32
            };
            if next().is_multiple_of(9) {
                num_ents = num_ents.wrapping_add(next() as u32);
            }
            TxpwrConf {
                key: c.key,
                id: c.id.into(),
                rfe: next() as u8,
                ent_sz,
                num_ents,
                data,
            }
        })
        .collect()
}

fn wild_efuse(next: &mut impl FnMut() -> u64) -> Efuse {
    let mut e = Efuse::default();
    let g = &mut e.gain;
    for v in g
        .offset
        .as_flattened_mut()
        .iter_mut()
        .chain(g.comp.as_flattened_mut())
        .chain(g.offset_base.iter_mut())
        .chain(g.rssi_base.iter_mut())
    {
        *v = next() as i8;
    }
    g.offset_valid = next().is_multiple_of(2);
    g.comp_valid = next().is_multiple_of(2);
    let t = &mut e.pwr_trim;
    t.pg_thermal_trim = next().is_multiple_of(2);
    t.pg_pa_bias_trim = next().is_multiple_of(2);
    t.thermal_trim = [next() as u8, next() as u8];
    t.pa_bias_trim = [next() as u8, next() as u8];
    e
}

fn wild_phy(next: &mut impl FnMut() -> u64) -> Phy {
    let mut p = Phy::new(1);
    p.efuse = Some(wild_efuse(next));
    let g = &mut p.bb_gain;
    for v in g
        .lna_gain
        .as_flattened_mut()
        .as_flattened_mut()
        .iter_mut()
        .chain(g.tia_gain.as_flattened_mut().as_flattened_mut())
        .chain(g.rpl_ofst_20.as_flattened_mut())
        .chain(g.rpl_ofst_40.as_flattened_mut().as_flattened_mut())
        .chain(g.rpl_ofst_80.as_flattened_mut().as_flattened_mut())
    {
        *v = next() as i8;
    }
    p.hal.antenna_rx = (next() % 9) as u32;
    p.hal.rx_nss = (next() % 5) as u32;
    p
}

/// A state as after `load` and phy_dm_init, with tables from the good hand-made elements or,
/// when they load, from random bytes
fn wild_state(next: &mut impl FnMut() -> u64) -> ChanState {
    let mut st = ChanState::new();
    let t = load_rfe_data_from(&wild_confs(next));
    st.tables = Some(t.unwrap_or_else(|_| {
        let empty: Vec<TxpwrConf> = TXPWR_CONF
            .iter()
            .map(|c| TxpwrConf {
                key: c.key,
                id: c.id.into(),
                rfe: 0,
                ent_sz: 1,
                num_ents: 0,
                data: Vec::new(),
            })
            .collect();
        load_rfe_data_from(&empty).unwrap()
    }));
    let regd = |next: &mut dyn FnMut() -> u64| Regd {
        alpha2: [next() as u8, next() as u8],
        txpwr_regd: [next() as u32 % 300, next() as u32 % 300, next() as u32],
        fmap: next() as u32,
    };
    let mut map: Vec<Regd> = (0..(next() % 4)).map(|_| regd(next)).collect();
    map.push(Regd {
        alpha2: *b"DE",
        txpwr_regd: [next() as u32 % 20, next() as u32 % 20, 4],
        fmap: 0,
    });
    st.regd_map = Some(map);
    let _ = regd_apply(&mut st, DEFAULT_COUNTRY);
    st.dm_init_done = true;
    st
}

const REQS: [(Band, u32); 6] = [
    (Band::B2g, 1),
    (Band::B2g, 13),
    (Band::B5g, 36),
    (Band::B5g, 100),
    (Band::B5g, 165),
    (Band::B2g, 6),
];

#[test]
fn every_entry_survives_a_hostile_chip() {
    for (src, seed) in sources() {
        let mut b = Hostile::new(src, seed);
        let mut next = rng(seed);
        let mut phy = wild_phy(&mut next);
        let mut st = wild_state(&mut next);
        let check = |what: &str, b: &mut Hostile, r: Rtw89Result<()>| {
            assert!(b.ops <= BUDGET, "{what} on {src:?}: {} ops", b.ops);
            if let Err(e) = r {
                assert!(
                    matches!(e, Rtw89Error::Bus(_) | Rtw89Error::Invalid { .. }),
                    "{what} on {src:?}: {e:?}"
                );
            }
            b.ops = 0;
        };
        let r = init_txpwr_unit(&mut b, &mut st);
        check("init_txpwr_unit", &mut b, r);
        let r = set_txpwr_ctrl(&mut b, &mut st);
        check("set_txpwr_ctrl", &mut b, r);
        let r = power_trim(&mut b, &phy);
        check("power_trim", &mut b, r);
        let r = cfg_txrx_path(&mut b, &phy, &mut st);
        check("cfg_txrx_path", &mut b, r);
        st.dm_init_done = true;
        for (band, ch) in REQS {
            let req = ChanReq { band, ch, bw: 20 };
            let r = set_channel(&mut b, &mut phy, &mut st, req, &mut Quiet, true);
            check("set_channel", &mut b, r);
            let r = set_txpwr(&mut b, &mut st);
            check("set_txpwr", &mut b, r);
            let r = set_channel_mac(&mut b, &chan_create(band, ch), &mut st);
            check("set_channel_mac", &mut b, r);
            let r = help_enter(&mut b, &mut st, band, &mut Quiet)
                .and_then(|tx| help_leave(&mut b, &mut st, band, &mut Quiet, tx));
            check("help", &mut b, r);
        }
        let r = regd_set(&mut b, &mut st, *b"XX").map(|_| ());
        check("regd_set", &mut b, r);
        assert!(st.events().len() <= crate::events::DEFAULT_EVENT_CAP);
    }
}

#[test]
fn hostile_state_is_refused_or_harmless() {
    let mut next = rng(0x0c0f_fee0_dead_1234);
    for round in 0..300 {
        let mut b = RfFake::new();
        let mut phy = wild_phy(&mut next);
        let mut st = wild_state(&mut next);
        let bands = [Band::B2g, Band::B5g, Band::B6g];
        // a wild channel context before the switch
        st.chan = chan_create(bands[(next() % 3) as usize], (next() % 256) as u32);
        st.chan.subband_type = (next() % 20) as u32;
        st.entity_active = next().is_multiple_of(2);
        let req = ChanReq {
            band: bands[(next() % 3) as usize],
            ch: (next() % 256) as u32,
            bw: [20, 20, 20, 40, 0][(next() % 5) as usize],
        };
        let ok = |what: &str, r: Rtw89Result<()>| {
            if let Err(e) = r {
                assert!(
                    matches!(e, Rtw89Error::Invalid { .. }),
                    "round {round} {what}: {e:?}"
                );
            }
        };
        ok("cfg_txrx_path", cfg_txrx_path(&mut b, &phy, &mut st));
        ok("power_trim", power_trim(&mut b, &phy));
        ok(
            "set_channel",
            set_channel(&mut b, &mut phy, &mut st, req, &mut Quiet, false),
        );
        ok("set_txpwr", set_txpwr(&mut b, &mut st));
        ok(
            "bb",
            bb::set_channel_bb(&mut b, &phy, &chan_create(req.band, req.ch)),
        );
        let t = st.tables.clone().unwrap();
        let r = st.regd.unwrap_or(Regd::WW);
        for band in [RTW89_BAND_2G, RTW89_BAND_5G, 2, 77] {
            let ch = (next() % 256) as u32;
            let _ = txpwr::fill_limit_20m(&t, &r, band, (next() % 3) as u32, ch);
            let _ = txpwr::fill_limit_ru_20m(&t, &r, band, (next() % 3) as u32, ch);
            let _ = txpwr::tx_shape_idx(&t, &r, band, (next() % 4) as u32);
            let d = RateDesc {
                rs: (next() % 7) as u32,
                nss: (next() % 6) as u32,
                ofdma: (next() % 3) as u32,
                idx: (next() % 20) as u32,
            };
            let _ = txpwr::read_byrate(&t, band, (next() % 6) as u32, d);
        }
        // channel flags for anything
        let _ = st.channel_flags(req.band, req.ch);
    }
}

#[test]
fn random_element_bytes_never_panic() {
    let mut next = rng(0x005e_ed0f_e1e3);
    for _ in 0..2000 {
        let _ = load_rfe_data_from(&wild_confs(&mut next));
        let elems: Vec<Vec<u8>> = (0..(next() % 3))
            .map(|_| {
                let len = (next() % 64) as usize;
                let mut p: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                if p.len() >= 8 && next().is_multiple_of(2) {
                    // a plausible count, so the entries are read
                    let n = (next() % 8) as u32;
                    p[4..8].copy_from_slice(&n.to_le_bytes());
                }
                p
            })
            .collect();
        let _ = load_regd_map_from(elems.iter().map(Vec::as_slice));
    }
}
