//! "Never panic on chip values or state": every rfk1 entry against a hostile chip and hostile
//! state. The chip answers every read with a constant (0, all ones, 0xDEADBEEF, 0xEAEAEAEA), a
//! seeded xorshift stream, or test-rfk1.mjs's chip model with one access in `n` garbage or a
//! timed-out SWSI poll; the clock is frozen (only the waited delays end a poll). Each call must
//! return Ok or Err (the debug build panics on an overflow, so it is caught here) within an op
//! budget (no endless wait). Then random state: slots, bands, bandwidths, channels, paths.
//! rfk2's hostile tests use the same chip (`Hostile`, `sources`).

use alloc::vec::Vec;

use super::fake::RfFake;
use super::iqkstep::{IqkType, iqk_one_shot, iqk_rxclk_setting, lok_finetune_check, rxk_group_sel};
use super::*;
use crate::bus::{BusError, BusResult, MemWidth, Width};
use crate::phy::{Band, Chan, Phy};

/// The most ops one call may take: rfk_channel's IQK has 2 x 21 one-shots whose 8.2 ms NCTL
/// poll ends after ~10 k 1 us steps on a frozen clock, all else is far below
pub(crate) const BUDGET: u64 = 3_000_000;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Src {
    Const(u32),
    Rand,
    /// the chip model, one access in `n` faulty
    Flaky(u64),
}

pub(crate) struct Hostile {
    pub(crate) src: Src,
    rng: u64,
    pub(crate) good: RfFake,
    pub(crate) ops: u64,
}

impl Hostile {
    pub(crate) fn new(src: Src, seed: u64) -> Hostile {
        Hostile {
            src,
            rng: seed | 1,
            good: RfFake::fresh(),
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
        if self.good.trace.len() > 4096 {
            self.good.trace.clear();
            self.good.times.clear();
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
        if matches!(self.src, Src::Flaky(_)) && self.fault() {
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
        self.op() // the frozen clock: a wait never moves it
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

/// Neighbours that do nothing but read the scheduler's TX enables (from the hostile chip)
struct Quiet;

impl<B: Bus + ?Sized> RfkHooks<B> for Quiet {
    fn btc_wl_rfk(&mut self, _: &mut B, _: u32, _: u32, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn btc_conn_rfk(&mut self, _: &mut B, _: bool) -> Rtw89Result<()> {
        Ok(())
    }
    fn btc_preserve_bt_time(&mut self, _: &mut B, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        Ok(bus.r16(R_AX_CTN_TXEN)?)
    }
    fn resume_sch_tx(&mut self, _: &mut B, _: u16) -> Rtw89Result<()> {
        Ok(())
    }
    fn h2c(&mut self, _: &mut B, _: &H2c, _: &[u8]) -> Rtw89Result<()> {
        Ok(())
    }
    fn dpk_init(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        Ok(())
    }
    fn dpk(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn dpk_track(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
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
        Ok(())
    }
    fn tssi_scan(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn wifi_scan_notify(&mut self, _: &mut B, _: &mut Phy, _: bool, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
}

pub(crate) const CHANS: [Chan; 5] = [
    Chan {
        band: Band::B2g,
        ch: 1,
        bw: 20,
    },
    Chan {
        band: Band::B2g,
        ch: 14,
        bw: 40,
    },
    Chan {
        band: Band::B5g,
        ch: 36,
        bw: 20,
    },
    Chan {
        band: Band::B5g,
        ch: 149,
        bw: 80,
    },
    Chan {
        band: Band::B5g,
        ch: 0,
        bw: 40,
    },
];

pub(crate) fn sources() -> Vec<(Src, u64)> {
    let mut v = Vec::new();
    for c in [0, 0xffff_ffff, 0xdead_beef, 0xeaea_eaea] {
        v.push((Src::Const(c), 1));
    }
    for seed in 1..=3 {
        v.push((Src::Rand, seed));
        for n in [3, 17, 500] {
            v.push((Src::Flaky(n), seed * 7919 + n));
        }
    }
    v
}

#[test]
fn every_entry_survives_a_hostile_chip() {
    for (src, seed) in sources() {
        let mut b = Hostile::new(src, seed);
        let mut phy = Phy::default();
        let mut st = RfkState::new();
        let check = |what: &str, b: &mut Hostile, r: Rtw89Result<()>| {
            assert!(b.ops <= BUDGET, "{what} on {src:?}: {} ops", b.ops);
            if let Err(e) = r {
                // the only errors here: the bus's (with register and value) or a refused request
                assert!(
                    matches!(e, Rtw89Error::Bus(_) | Rtw89Error::Invalid { .. }),
                    "{what} on {src:?}: {e:?}"
                );
            }
            b.ops = 0;
        };
        let r = rfk_init(&mut b, &mut phy, &mut st, None, &mut Quiet);
        check("rfk_init", &mut b, r);
        for c in &CHANS {
            let r = set_channel_rf(&mut b, &mut st, Some(c), 0);
            check("set_channel_rf", &mut b, r);
            phy.tssi_mode = [seed % 2 == 0, true];
            let r = rfk_channel(&mut b, &mut phy, &mut st, Some(c), &mut Quiet, 0);
            check("rfk_channel", &mut b, r);
        }
        let r = dack(&mut b, &mut st, None, &mut Quiet);
        check("dack again", &mut b, r);
        let _ = st.take_events();
    }
}

#[test]
fn hostile_state_is_refused_or_harmless() {
    let mut rng = 0x1234_5678_9abc_def1u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for _ in 0..300 {
        let mut b = RfFake::fresh();
        let mut st = RfkState::new();
        let mut phy = Phy::default();
        st.mcc.table_idx = next() as u8;
        st.mcc.ch = [next() as u32, next() as u32, next() as u32];
        st.iqk.iqk_times = next() as u32;
        st.iqk.iqk_fail_cnt = next() as u32;
        st.iqk.iqk_band = [next() as u32, next() as u32];
        st.iqk.iqk_bw = [next() as u32, next() as u32];
        st.dack.dack_cnt = u32::MAX;
        let band = [Band::B2g, Band::B5g, Band::B6g][(next() % 3) as usize];
        let c = Chan {
            band,
            ch: next() as u32,
            bw: [20, 40, 80, 160, 5, 0][(next() % 6) as usize],
        };
        let path = (next() % 4) as u32;
        let _ = h2c_rf_ntfy_mcc(&mut b, &st, &mut Quiet);
        let _ = lok_finetune_check(&mut b, &mut st, path);
        let _ = iqk_one_shot(&mut b, &mut st, 0, path, IqkType::Rxk);
        let _ = iqk_one_shot(&mut b, &mut st, 0, path, IqkType::Txk);
        let _ = iqk_rxclk_setting(&mut b, &st, path);
        let _ = rxk_group_sel(&mut b, &mut st, 0, path);
        let _ = mcc_get_ch_info(&mut st, Some(&c), 0);
        let _ = set_channel_rf(&mut b, &mut st, Some(&c), 0);
        let _ = rfk_channel(&mut b, &mut phy, &mut st, Some(&c), &mut Quiet, 0);
        let _ = rfk_init(&mut b, &mut phy, &mut st, Some(&c), &mut Quiet);
        let _ = rx_dck(&mut b, &phy, &mut st, Some(&c), &mut Quiet, next() as u32);
        let _ = iqk(&mut b, &mut st, Some(&c), &mut Quiet, next() as u32);
    }
}
