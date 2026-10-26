//! "Never panic on chip values or state": every rfk2 entry against rfk1's hostile chip (reads
//! of 0, all ones, 0xDEADBEEF, 0xEAEAEAEA, a seeded stream, or the chip model with one access in
//! `n` garbage or a timed-out poll; a frozen clock) and against random state. Each call must
//! return Ok or Err (the debug build panics on an overflow, so it is caught here) within the op
//! budget (no endless wait), and the only errors are the bus's or a refused request.

use super::*;
use crate::phy::efuse::Efuse;
use crate::phy::{Band, Chan};
use crate::rfk1::fake::RfFake;
use crate::rfk1::tests_hostile::{BUDGET, CHANS, Hostile, sources};

/// Neighbours that do nothing but read the scheduler's TX enables (from the hostile chip)
struct Quiet;

impl<B: Bus + ?Sized> Rfk2Hooks<B> for Quiet {
    fn btc_wl_rfk(&mut self, _: &mut B, _: u32, _: u32, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        Ok(bus.r16(R_AX_CTN_TXEN)?)
    }
    fn resume_sch_tx(&mut self, _: &mut B, _: u16) -> Rtw89Result<()> {
        Ok(())
    }
}

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut r = seed | 1;
    move || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r
    }
}

/// An efuse with every value drawn from `next` (s8 extremes included)
fn wild_efuse(next: &mut impl FnMut() -> u64) -> Efuse {
    let mut e = Efuse::default();
    e.tssi.thermal = [next() as u8, next() as u8];
    for p in 0..2 {
        e.tssi.tssi_cck[p]
            .iter_mut()
            .for_each(|v| *v = next() as i8);
        e.tssi.tssi_mcs[p]
            .iter_mut()
            .for_each(|v| *v = next() as i8);
        e.tssi.tssi_trim[p]
            .iter_mut()
            .for_each(|v| *v = next() as i8);
        e.gain.offset[p].iter_mut().for_each(|v| *v = next() as i8);
        e.gain.comp[p].iter_mut().for_each(|v| *v = next() as i8);
    }
    e.gain.offset_valid = next().is_multiple_of(2);
    e.gain.comp_valid = next().is_multiple_of(2);
    e.gain.offset_base = [next() as i8, next() as i8];
    e.gain.rssi_base = [next() as i8, next() as i8];
    e
}

#[test]
fn every_entry_survives_a_hostile_chip() {
    for (src, seed) in sources() {
        let mut b = Hostile::new(src, seed);
        let mut next = rng(seed);
        let mut phy = Phy::new(1);
        phy.efuse = Some(wild_efuse(&mut next));
        let mut st = Rfk2State::new();
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
        let r = dpk_init(&mut b, &mut phy, &mut st);
        check("dpk_init", &mut b, r);
        for c in &CHANS {
            let r = tssi(&mut b, &mut phy, &mut st, Some(c), &mut Quiet, 0, true);
            check("tssi", &mut b, r);
            let r = dpk(&mut b, &mut phy, &mut st, Some(c), &mut Quiet, 0);
            check("dpk", &mut b, r);
            phy.avg_thermal = [next() as u32, next() as u32];
            let r = dpk_track(&mut b, &mut phy, &mut st);
            check("dpk_track", &mut b, r);
            for start in [true, false] {
                let r = wifi_scan_notify(&mut b, &mut phy, &mut st, Some(c), &mut Quiet, start, 0);
                check("wifi_scan_notify", &mut b, r);
            }
            let r = tssi_scan(&mut b, &mut phy, &mut st, Some(c), 0);
            check("tssi_scan", &mut b, r);
        }
        assert!(st.events().len() <= crate::events::DEFAULT_EVENT_CAP);
        let _ = st.take_events();
    }
}

#[test]
fn hostile_state_is_refused_or_harmless() {
    let mut next = rng(0x1234_5678_9abc_def1);
    for _ in 0..300 {
        let mut b = RfFake::fresh();
        let mut phy = Phy::new(1);
        phy.efuse = Some(wild_efuse(&mut next));
        phy.tssi_mode = [next().is_multiple_of(2), next().is_multiple_of(2)];
        phy.avg_thermal = [next() as u32, next() as u32];
        phy.hal.antenna_rx = (next() % 5) as u32;
        let mut st = Rfk2State::new();
        st.dpk.is_dpk_reload_en = next().is_multiple_of(2);
        st.dpk.cur_idx = [(next() % 3) as u32, (next() % 3) as u32];
        st.dpk.dpk_gs = [next() as u32, next() as u32];
        for p in 0..2 {
            for bp in st.dpk.bp[p].iter_mut() {
                bp.band = (next() % 4) as u32;
                bp.bw = next() as u32;
                bp.ch = (next() % 200) as u32;
                bp.ther_dpk = next() as u32;
                bp.pwsf = next() as u32;
                bp.path_ok = next().is_multiple_of(2);
            }
            for d in st.tssi.alignment_done[p].iter_mut() {
                *d = next().is_multiple_of(2);
            }
            for c in st.tssi.check_backup_aligmk[p].iter_mut() {
                *c = next().is_multiple_of(3);
            }
        }
        st.fem = Fem {
            epa_2g: next().is_multiple_of(4),
            epa_5g: next().is_multiple_of(4),
        };
        let band = [Band::B2g, Band::B5g, Band::B6g][(next() % 3) as usize];
        let c = Chan {
            band,
            ch: (next() % 256) as u32,
            bw: [20, 40, 80, 160, 5, 0][(next() % 6) as usize],
        };
        let phy_idx = (next() % 3) as u32;
        let _ = dpk_init(&mut b, &mut phy, &mut st);
        let _ = tssi(
            &mut b,
            &mut phy,
            &mut st,
            Some(&c),
            &mut Quiet,
            phy_idx,
            next().is_multiple_of(2),
        );
        let _ = dpk(&mut b, &mut phy, &mut st, Some(&c), &mut Quiet, phy_idx);
        let _ = dpk_track(&mut b, &mut phy, &mut st);
        let _ = tssi_scan(&mut b, &mut phy, &mut st, Some(&c), phy_idx);
        let _ = wifi_scan_notify(
            &mut b,
            &mut phy,
            &mut st,
            Some(&c),
            &mut Quiet,
            next().is_multiple_of(2),
            phy_idx,
        );
        let _ = wifi_scan_notify(&mut b, &mut phy, &mut st, None, &mut Quiet, false, 0);
        if let Ok(rc) = crate::rfk1::util::chan_get(Some(&c)) {
            let _ = dpkflow::dpk_run(&mut b, &phy, &mut st, phy_idx, false, &rc);
        }
        // keep the fake's trace short
        b.trace.clear();
        b.times.clear();
    }
}
