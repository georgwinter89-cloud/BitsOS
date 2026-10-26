//! "Never panic on chip values": the whole ported MAC surface against a hostile chip.
//! Two kinds of chip: one whose every register, config and DMA read answers a constant
//! (0, all ones, 0xDEADBEEF, 0xEAEAEAEA, CMAC_EN) or a seeded xorshift stream; and the
//! golden-trace chip (`golden_chip`, which gets through stage mac) with seeded faults: one
//! read or poll in `n` answers garbage or times out, so the sweep reaches every step. The
//! clock is frozen or ticks. Each call must return Ok or Err (the debug build panics on
//! an overflow, so it is caught here) and end within an op budget (no endless wait).
//! Polls run as on hardware (`poll_loop`).

use alloc::vec::Vec;

use super::tests::{OPTS, fw_file, golden_chip, status_cap_list};
use super::*;
use crate::bringup::{Bringup, RingConfig};
use crate::bus::{BusError, BusResult, MemWidth, Width, poll_loop};
use crate::fw::FwState;
use crate::membus::{MemBus, bump_alloc};
use crate::pci::{self, PciRings};
use crate::tables::regs::*;

#[derive(Debug, Clone, Copy)]
enum Src {
    Const(u32),
    Rand(u64),
    /// the golden chip, one access in `n` faulty (n = 0: none, the control run)
    Flaky {
        seed: u64,
        n: u64,
    },
}

/// The most ops one call may take: the longest bounded wait is the probe's 1 s C2H wait in
/// 1 us steps on a frozen clock (2 M ops), the firmware download's five tries far below
const BUDGET: u64 = 20_000_000;

struct Hostile {
    src: Src,
    rng: u64,
    good: Option<MemBus>,
    ticking: bool,
    clock_us: u64,
    ops: u64,
    exhausted: bool,
    dma_next: u32,
}

impl Hostile {
    fn new(src: Src, ticking: bool) -> Hostile {
        let (rng, good) = match src {
            Src::Const(_) => (1, None),
            Src::Rand(seed) => (seed, None),
            Src::Flaky { seed, .. } => (seed, Some(golden_chip())),
        };
        Hostile {
            src,
            rng,
            good,
            ticking,
            clock_us: 0,
            ops: 0,
            exhausted: false,
            dma_next: 0x10_0000,
        }
    }
    fn rand(&mut self) -> u64 {
        // xorshift64
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
    /// A garbage value
    fn next(&mut self) -> u32 {
        match self.src {
            Src::Const(v) => v,
            _ => (self.rand() >> 16) as u32,
        }
    }
    /// Whether this access goes wrong (always on the all-garbage chips)
    fn fault(&mut self) -> bool {
        match self.src {
            Src::Flaky { n, .. } => n != 0 && self.rand().is_multiple_of(n),
            _ => true,
        }
    }
    /// `good`'s answer, or garbage when the access is faulty
    fn answer(&mut self, good: Option<u32>) -> u32 {
        match good {
            Some(v) if !self.fault() => v,
            _ => self.next(),
        }
    }
    fn op(&mut self) -> BusResult<()> {
        self.ops += 1;
        if self.ops > BUDGET {
            self.exhausted = true;
            return Err(BusError::Gone);
        }
        if let Some(g) = &mut self.good
            && g.trace.len() > 4096
        {
            g.trace.clear();
        }
        Ok(())
    }
    /// Starts a call's budget
    fn call(&mut self) {
        self.ops = 0;
    }
}

impl Bus for Hostile {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.op()?;
        let g = self.good.as_mut().map(|g| g.read(w, addr)).transpose()?;
        Ok(self.answer(g) & w.mask())
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.op()?;
        self.good.as_mut().map_or(Ok(()), |g| g.write(w, addr, val))
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.op()?;
        self.good
            .as_mut()
            .map_or(Ok(()), |g| g.rmw(w, addr, clear, set))
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        if self.good.is_some() {
            self.op()?;
            if !self.fault() {
                return Ok(());
            }
            // a faulty poll waits as hardware would, on garbage
        }
        poll_loop(self, w, addr, mask, want, ms, 1000)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.op()?;
        if self.ticking {
            self.clock_us += u64::from(us);
        }
        Ok(())
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.op()?;
        let g = self.good.as_mut().map(|g| g.cfg_read(off)).transpose()?;
        Ok(self.answer(g))
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.op()?;
        if let Some(g) = &mut self.good {
            g.cfg_write(off, val)?;
            status_cap_list(g); // the status half is read-only on a chip
        }
        Ok(())
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        bump_alloc(&mut self.dma_next, 0xF000_0000, size, align)
            .ok_or(BusError::DmaExhausted { size, align })
    }
    fn dma_free(&mut self, _addr: u32, _size: usize) -> BusResult<()> {
        Ok(())
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.op()?;
        self.good
            .as_mut()
            .map_or(Ok(()), |g| g.mem_write(w, addr, val))
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.op()?;
        let g = self.good.as_mut().map(|g| g.mem_read32(addr)).transpose()?;
        Ok(self.answer(g))
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.op()?;
        self.good
            .as_mut()
            .map_or(Ok(()), |g| g.mem_write_bytes(addr, data))
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.op()?;
        if let Some(g) = &mut self.good {
            g.mem_read_bytes(addr, out)?;
            if !self.fault() {
                return Ok(());
            }
        }
        for b in out.iter_mut() {
            *b = self.next() as u8;
        }
        Ok(())
    }
    fn now_ms(&mut self) -> u64 {
        if self.ticking {
            self.clock_us += 1000;
        }
        self.clock_us / 1000
    }
}

fn sources() -> Vec<Src> {
    let mut v: Vec<Src> = [0, u32::MAX, RTW89_R32_DEAD, RTW89_R32_EA, B_AX_CMAC_EN]
        .into_iter()
        .map(Src::Const)
        .collect();
    let seed = |s: u64| s.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    v.extend((1..=6u64).map(|s| Src::Rand(seed(s))));
    v.push(Src::Flaky { seed: 1, n: 0 });
    for n in [3u64, 50, 400, 1500, 3000, 8000, 20000] {
        v.extend((1..=6u64).map(|s| Src::Flaky {
            seed: seed(s + n),
            n,
        }));
    }
    v
}

/// Runs `f` on a fresh hostile chip of every kind; `f` gets the bus and does its calls
/// through `check`, which bounds each one
fn sweep(mut f: impl FnMut(&mut Hostile, &dyn Fn(&mut Hostile, &str))) {
    let check = |b: &mut Hostile, what: &str| {
        assert!(
            !b.exhausted,
            "{what}: more than {BUDGET} ops on {:?} (ticking {})",
            b.src, b.ticking
        );
        b.call();
    };
    for src in sources() {
        for ticking in [false, true] {
            let mut b = Hostile::new(src, ticking);
            f(&mut b, &check);
        }
    }
}

struct NopHooks;

impl<B: Bus + ?Sized> VifHooks<B> for NopHooks {
    fn macid_pause(&mut self, b: &mut B, _: u32, _: u32, _: bool) -> Rtw89Result<()> {
        b.r32(0)?;
        Ok(())
    }
    fn role_maintain(&mut self, _: &mut B, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn join_info(&mut self, _: &mut B, _: bool) -> Rtw89Result<()> {
        Ok(())
    }
    fn cam_init(&mut self, _: &mut B) -> Rtw89Result<()> {
        Ok(())
    }
    fn addr_cam(&mut self, _: &mut B, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn default_cmac_tbl(&mut self, _: &mut B) -> Rtw89Result<()> {
        Ok(())
    }
}

fn rings(b: &mut Hostile) -> PciRings {
    pci::pci_alloc(b, &RingConfig::js()).expect("ring memory is the host's, not the chip's")
}

#[test]
fn the_bring_up_up_to_stage_mac_never_panics_and_ends() {
    let fw = fw_file();
    // how many runs got past each step: the faulty golden chip must reach them all
    let mut reached = [0usize; 7];
    sweep(|b, check| {
        let mut run = Bringup::new();
        let mut depth = 0;
        let r = (|| -> Rtw89Result<()> {
            run.probe_power_on(b, &RingConfig::js())?;
            check(b, "probe_power_on");
            depth = 1;
            run.partial_init(b, &fw)?;
            check(b, "partial_init");
            depth = 2;
            run.fwdl_look(b)?;
            check(b, "fwdl_look");
            depth = 3;
            run.probe_done(b, OPTS.rfe)?;
            check(b, "probe_done");
            depth = 4;
            run.probe_end(b)?;
            check(b, "probe_end");
            depth = 5;
            run.core_start_mac(b, &fw, &OPTS)?;
            depth = 6;
            Ok(())
        })();
        check(b, "the step that ended it");
        if let Src::Flaky { n: 0, .. } = b.src {
            assert_eq!(r, Ok(()), "the control run without faults");
        }
        for d in &mut reached[..=depth] {
            *d += 1;
        }
        let _ = run.stop(b);
        check(b, "stop");
        assert!(run.mac.events().len() <= crate::events::DEFAULT_EVENT_CAP);
    });
    std::eprintln!("bring-up steps reached: {reached:?}");
    assert!(
        reached.iter().all(|&n| n >= 3),
        "every step reached by a faulty chip too: {reached:?}"
    );
    // and faults ended runs inside the firmware download and inside stage mac
    assert!(
        reached[1] > reached[2] && reached[5] > reached[6],
        "{reached:?}"
    );
}

#[test]
fn power_and_the_init_steps_never_panic_and_end() {
    let fw = fw_file();
    sweep(|b, check| {
        for waits in [MacWaits::probe(), MacWaits::short()] {
            let mut m = MacState::with_waits(waits);
            let mut fws = FwState::new();
            let _ = mac_pwr_on(b, &OPTS, &mut m, &mut fws);
            check(b, "mac_pwr_on");
            let _ = mac_pwr_off(b, OPTS.rfe, &mut m, &mut fws);
            check(b, "mac_pwr_off");
            let _ = sys_init(b);
            check(b, "sys_init");
            let _ = enable_bb_rf(b);
            check(b, "enable_bb_rf");
            let _ = dmac_init(b, &mut m);
            check(b, "dmac_init");
            let _ = cmac_init(b, &mut m);
            check(b, "cmac_init");
            let _ = trx_init(b, &mut m);
            check(b, "trx_init");
            for sel in [MacSel::Dmac, MacSel::Cmac] {
                let _ = check_mac_en(b, sel);
                check(b, "check_mac_en");
            }
            for preinit in [true, false] {
                let mut r = rings(b);
                let c = MacInit {
                    rings: &mut r,
                    mac: &mut m,
                    fws: &mut fws,
                    fw: &fw,
                    opts: &OPTS,
                };
                let _ = mac_init(b, c, preinit);
                check(b, "mac_init");
            }
            let mut r = rings(b);
            let _ = core_start_tail(b, &mut r, &mut fws, None, None);
            check(b, "core_start_tail");
        }
    });
}

#[test]
fn ports_vifs_and_macid_pause_never_panic_at_any_id() {
    let ids = [0u8, 31, 32, 127, 128, u8::MAX];
    let ports = [0u8, 4, 5, u8::MAX];
    let types = [
        RTW89_NET_TYPE_NO_LINK,
        RTW89_NET_TYPE_INFRA,
        RTW89_NET_TYPE_AP_MODE,
        u32::MAX,
    ];
    sweep(|b, check| {
        let mut m = MacState::new();
        for &mac_id in &ids {
            for &port in &ports {
                for &net_type in &types {
                    for beacon_int in [0u16, 100, u16::MAX] {
                        let vif = Vif {
                            net_type,
                            beacon_int,
                            dtim_period: u8::MAX,
                            bss_color: u8::MAX,
                            nontransmitted: true,
                            bssid_index: u8::MAX,
                            mac_idx: u8::MAX,
                            ..Vif::station(mac_id, port, [0xff; 6])
                        };
                        let _ = vif_init(b, &mut m, &vif, &mut NopHooks);
                        check(b, "vif_init");
                    }
                }
            }
            let mut r = rings(b);
            for ready in [false, true] {
                let mut fws = FwState::new();
                fws.ready = ready;
                for pause in [false, true] {
                    let _ = macid_pause(b, &mut r, &mut fws, mac_id, pause);
                    check(b, "macid_pause");
                }
            }
        }
        for v in [0, 31, 32, u32::MAX] {
            let _ = h2c::macid_pause_payload(v, v, true);
        }
        // the events stay bounded however many waits were cut
        assert!(m.events().len() <= crate::events::DEFAULT_EVENT_CAP);
    });
}

#[test]
fn the_filter_and_sch_tx_never_panic_and_end() {
    let sels = [SchTxSel::All, SchTxSel::Hiq, SchTxSel::Mg0, SchTxSel::Macid];
    assert_eq!(SchTxSel::from_linux(u32::MAX), None);
    sweep(|b, check| {
        let mut m = MacState::new();
        for bits in 0..32u32 {
            let flags = FilterFlags {
                allmulti: bits & 1 != 0,
                fcsfail: bits & 2 != 0,
                other_bss: bits & 4 != 0,
                bcn_prbresp_promisc: bits & 8 != 0,
                probe_req: bits & 16 != 0,
            };
            for scanning in [false, true] {
                let _ = rx_filter(b, &mut m, flags, scanning);
                check(b, "rx_filter");
            }
        }
        let _ = set_rx_fltr(b, u32::MAX);
        check(b, "set_rx_fltr");
        let _ = cfg_ppdu_status(b, true);
        check(b, "cfg_ppdu_status");
        let _ = update_rts_threshold(b);
        check(b, "update_rts_threshold");
        for waits in [MacWaits::probe(), MacWaits::short()] {
            let mut m = MacState::with_waits(waits);
            for ready in [false, true] {
                let mut fws = FwState::new();
                fws.ready = ready;
                for sel in sels {
                    let tx_en = stop_sch_tx(b, &mut m, &mut fws, sel).unwrap_or(u16::MAX);
                    check(b, "stop_sch_tx");
                    let _ = resume_sch_tx(b, &mut m, &mut fws, tx_en);
                    check(b, "resume_sch_tx");
                }
            }
        }
    });
}

#[test]
fn the_cmac_read_rule_never_panics_on_a_hostile_chip() {
    use crate::deadread::CmacRead;
    sweep(|b, check| {
        let mut c = CmacRead::new(Hostile::new(b.src, b.ticking));
        let mut m = MacState::new();
        let _ = cmac_init(&mut c, &mut m);
        let _ = rx_filter(&mut c, &mut m, FilterFlags::default(), false);
        let _ = c.read(Width::W8, crate::deadread::R_AX_CMAC_REG_END);
        assert!(!c.inner.exhausted);
        check(b, "cmac rule");
    });
}
