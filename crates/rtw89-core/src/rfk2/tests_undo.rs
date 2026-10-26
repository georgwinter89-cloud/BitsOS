//! Not Linux (its steps cannot fail): when a TSSI or DPK step fails half-way, what the
//! calibration stopped or saved is put back before the error goes up. The scheduler's TX
//! enables, btc's RFK state, the PMAC's test packets, the saved BB/KIP/RF registers and the
//! TSSI pause. Every op position of a good run is made to fail once and the chip is checked
//! afterwards; a second failure in the putting back, a dead chip and an all-ones one-shot too.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::tests::{CH1, Neigh, chip, phy};
use super::*;
use crate::bus::{BusResult, MemWidth, Width, field};
use crate::rfk1::fake::RfFake;
use crate::rfk1::tests_hostile::{BUDGET, Hostile, Src};
use crate::tables::rfk1::BACKUP_BB_REGS;
use crate::tables::rfk2::ALIM_BB_REG;
use crate::trace::{OpKind, TraceEntry};

type Rule = Box<dyn FnMut(u64, u32) -> bool>;

/// The fake chip with injected failures: `rule(n, addr)` says whether op `n` (from 1, register
/// ops, polls and delays) on `addr` fails; the failed addresses are kept
struct Faulty {
    b: RfFake,
    n: u64,
    rule: Rule,
    failed: Vec<u32>,
}

fn injected(addr: u32) -> Rtw89Error {
    Rtw89Error::Bus(BusError::Refused {
        kind: "injected",
        addr,
        len: 4,
    })
}

impl Faulty {
    fn new(b: RfFake, rule: Rule) -> Faulty {
        Faulty {
            b,
            n: 0,
            rule,
            failed: Vec::new(),
        }
    }
    /// Op `at` fails, nothing else
    fn once(b: RfFake, at: u64) -> Faulty {
        Faulty::new(b, Box::new(move |n, _| n == at))
    }
    fn hit(&mut self, addr: u32) -> BusResult<()> {
        self.n += 1;
        if (self.rule)(self.n, addr) {
            self.failed.push(addr);
            return Err(BusError::Refused {
                kind: "injected",
                addr,
                len: 4,
            });
        }
        Ok(())
    }
    fn reg(&self, a: u32) -> u32 {
        self.b.regs.get(a, Width::W32)
    }
    /// The register at `a` unless a failed op touched it (then it may rightly differ)
    fn untouched(&self, a: u32) -> bool {
        !self.failed.iter().any(|&f| f & !3 == a & !3)
    }
}

impl Bus for Faulty {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.hit(addr)?;
        self.b.read(w, addr)
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.hit(addr)?;
        self.b.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.hit(addr)?;
        self.b.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.hit(addr)?;
        self.b.poll(w, addr, mask, want, ms)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.hit(u32::MAX)?;
        self.b.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.b.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.b.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.b.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.b.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.b.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.b.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.b.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.b.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.b.now_ms()
    }
}

fn txen(b: &Faulty) -> u32 {
    b.b.regs.get(R_AX_CTN_TXEN, Width::W16)
}

/// The registers of `regs` (BB offsets) whose value a good run leaves as it found them
fn kept(regs: &[u32], run: impl FnOnce(&mut Faulty)) -> Vec<(u32, u32)> {
    let mut b = Faulty::once(chip(), 0);
    let pre: Vec<(u32, u32)> = regs.iter().map(|&a| (BB + a, b.reg(BB + a))).collect();
    run(&mut b);
    pre.into_iter().filter(|&(a, v)| b.reg(a) == v).collect()
}

/// How many ops a good run takes
fn ops_of(run: impl FnOnce(&mut Faulty)) -> u64 {
    let mut b = Faulty::once(chip(), 0);
    run(&mut b);
    b.n
}

/// Every op position of `entry` failed once: the error goes up unchanged, then `check` sees
/// the chip. Positions are walked with `stride` (1: all).
fn each_failure(
    total: u64,
    stride: u64,
    mut run: impl FnMut(&mut Faulty, &mut Rfk2State, &mut Neigh) -> Rtw89Result<()>,
    mut check: impl FnMut(u64, &Faulty, &Rfk2State, &Neigh),
) {
    let mut at = 1;
    while at <= total {
        let mut b = Faulty::once(chip(), at);
        let (mut st, mut n) = (Rfk2State::new(), Neigh::default());
        let r = run(&mut b, &mut st, &mut n);
        assert_eq!(b.failed.len(), 1, "op {at} of {total}");
        assert_eq!(r, Err(injected(b.failed[0])), "op {at} of {total}");
        check(at, &b, &st, &n);
        at += stride;
    }
}

#[test]
fn dpk_puts_everything_back_after_any_failure() {
    let tssi_on = || {
        let mut p = phy();
        p.tssi_mode = [true, true]; // TSSI is paused around DPK then
        p
    };
    let good = |b: &mut Faulty| {
        let (mut p, mut st, mut n) = (tssi_on(), Rfk2State::new(), Neigh::default());
        dpk(b, &mut p, &mut st, Some(&CH1), &mut n, 0).unwrap();
        assert!(st.events().is_empty());
    };
    let bb: Vec<u32> = BACKUP_BB_REGS.iter().map(|&a| a.into()).collect();
    let bb_kept = kept(&bb, good);
    assert_eq!(
        bb_kept.len(),
        bb.len(),
        "a good DPK puts every saved BB register back"
    );
    let total = ops_of(good);
    assert!(total > 500, "{total}");
    each_failure(
        total,
        1,
        |b, st, n| dpk(b, &mut tssi_on(), st, Some(&CH1), n, 0),
        |at, b, st, n| {
            let error = injected(b.failed[0]);
            assert_eq!(
                st.events().last(),
                Some(&Rfk2Event::Aborted {
                    entry: "dpk",
                    error
                }),
                "op {at}"
            );
            assert!(
                !st.events()
                    .iter()
                    .any(|e| matches!(e, Rfk2Event::UndoFailed { .. })),
                "op {at}: {:?}",
                st.events()
            );
            assert_eq!(
                n.btc,
                [
                    (0x10, BTC_WRFKT_DPK, BTC_WRFK_START),
                    (0x10, BTC_WRFKT_DPK, BTC_WRFK_STOP)
                ],
                "op {at}: btc's RFK state ends"
            );
            if b.untouched(R_AX_CTN_TXEN) {
                assert_eq!(txen(b), 0xffff, "op {at}: scheduler TX back on");
            }
            for path in 0..2 {
                let a = BB + R_P0_TSSI_TRK + (path << 13);
                if b.untouched(a) {
                    assert_eq!(
                        field(b.reg(a), B_P0_TSSI_TRK_EN),
                        0,
                        "op {at}: S{path} TSSI"
                    );
                }
            }
            for &(a, v) in &bb_kept {
                // a failure inside the BB restore leaves the registers after it
                if b.failed.iter().all(|f| !bb.iter().any(|&r| BB + r == *f)) {
                    assert_eq!(b.reg(a), v, "op {at}: BB 0x{a:x} put back");
                }
            }
        },
    );
}

#[test]
fn tssi_puts_everything_back_after_any_failure() {
    let good = |b: &mut Faulty| {
        let (mut p, mut st, mut n) = (phy(), Rfk2State::new(), Neigh::default());
        tssi(b, &mut p, &mut st, Some(&CH1), &mut n, 0, true).unwrap();
        assert!(st.events().is_empty());
    };
    let alim_kept = kept(ALIM_BB_REG, good);
    assert!(!alim_kept.is_empty());
    let total = ops_of(good);
    assert!(total > 500, "{total}");
    let pmac = BB + R_PMAC_TX_PRD;
    each_failure(
        total,
        1,
        |b, st, n| tssi(b, &mut phy(), st, Some(&CH1), n, 0, true),
        |at, b, st, n| {
            let error = injected(b.failed[0]);
            assert_eq!(
                st.events().last(),
                Some(&Rfk2Event::Aborted {
                    entry: "tssi",
                    error
                }),
                "op {at}"
            );
            assert_eq!(
                n.btc,
                [
                    (0x13, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_START),
                    (0x13, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_STOP)
                ],
                "op {at}: btc's one-shot ends"
            );
            if b.untouched(R_AX_CTN_TXEN) {
                assert_eq!(txen(b), 0xffff, "op {at}: scheduler TX back on");
            }
            if b.untouched(pmac) {
                assert_eq!(field(b.reg(pmac), B_PMAC_PTX_EN), 0, "op {at}: PMAC off");
            }
            for &(a, v) in &alim_kept {
                if b.failed
                    .iter()
                    .all(|f| !ALIM_BB_REG.iter().any(|&r| BB + r == *f))
                {
                    assert_eq!(b.reg(a), v, "op {at}: BB 0x{a:x} put back");
                }
            }
        },
    );
}

#[test]
fn a_failed_put_back_step_is_noted_and_the_rest_still_runs() {
    // every access to the PMAC's TX register fails: the test packets cannot start, nor can the
    // putting back switch them off
    let pmac = BB + R_PMAC_TX_PRD;
    let mut b = Faulty::new(chip(), Box::new(move |_, a| a == pmac));
    let (mut p, mut st, mut n) = (phy(), Rfk2State::new(), Neigh::default());
    let r = tssi(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0, true);
    assert_eq!(r, Err(injected(pmac)));
    assert_eq!(
        st.take_events(),
        [
            Rfk2Event::UndoFailed {
                step: "tssi pmac tx off",
                error: injected(pmac)
            },
            Rfk2Event::Aborted {
                entry: "tssi",
                error: injected(pmac)
            },
        ]
    );
    // the BB registers, the scheduler and btc still came back after the failed step
    let restored =
        b.b.lines()
            .iter()
            .rev()
            .take_while(|l| !l.contains(&alloc::format!("{pmac:08X}")))
            .count();
    assert!(restored >= ALIM_BB_REG.len(), "{restored}");
    assert_eq!(txen(&b), 0xffff);
    assert_eq!(
        n.btc.last(),
        Some(&(0x13, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_STOP))
    );
}

#[test]
fn a_dead_chip_is_not_put_back() {
    // the chip leaves the bus 300 ops into DPK: nothing more is tried, no failure per step
    let mut b = Hostile::new(Src::Const(0), 1);
    b.ops = BUDGET - 300;
    struct Quiet;
    impl<B: Bus + ?Sized> Rfk2Hooks<B> for Quiet {
        fn btc_wl_rfk(&mut self, _: &mut B, _: u32, _: u32, _: u32) -> Rtw89Result<()> {
            Ok(())
        }
        fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
            Ok(bus.r16(R_AX_CTN_TXEN)?)
        }
        fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
            Ok(bus.w16(R_AX_CTN_TXEN, tx_en.into())?)
        }
    }
    let (mut p, mut st) = (phy(), Rfk2State::new());
    let r = dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut Quiet, 0);
    assert_eq!(r, Err(Rtw89Error::Bus(BusError::Gone)));
    assert_eq!(
        b.ops,
        BUDGET + 1,
        "one access found it gone, no more were tried"
    );
    let ev = st.take_events();
    assert_eq!(
        ev.last(),
        Some(&Rfk2Event::Aborted {
            entry: "dpk",
            error: Rtw89Error::Bus(BusError::Gone)
        })
    );
    assert!(
        !ev.iter().any(|e| matches!(e, Rfk2Event::UndoFailed { .. })),
        "{ev:?}"
    );
}

#[test]
fn an_all_ones_one_shot_stops_dpk_after_one_wait() {
    let mut b = chip();
    b.fail_polls.insert(BB + 0xbff8);
    b.poke(BB + 0xbff8, Width::W32, u32::MAX);
    let (mut p, mut st, mut n) = (phy(), Rfk2State::new(), Neigh::default());
    let r = dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0);
    let gone = BusError::PollTimeout {
        addr: BB + 0xbff8,
        width: Width::W32,
        mask: MASKBYTE0,
        want: 0x55,
        last: u32::MAX,
    };
    assert_eq!(r, Err(Rtw89Error::Bus(gone.clone())));
    let polls = b
        .trace
        .iter()
        .filter(|e| matches!(e, TraceEntry::Op(o) if o.kind == OpKind::Poll && o.a == BB + 0xbff8))
        .count();
    assert_eq!(polls, 1, "no further one-shot waited out");
    assert_eq!(st.poll_timeouts, 1);
    assert_eq!(
        st.take_events(),
        [
            Rfk2Event::OneShotTimeout {
                path: 0,
                addr: 0xbff8,
                last: u32::MAX
            },
            Rfk2Event::Aborted {
                entry: "dpk",
                error: Rtw89Error::Bus(gone)
            },
        ]
    );
    // all ones is not taken as "gone" for the putting back: TX and btc come back
    assert_eq!(b.regs.get(R_AX_CTN_TXEN, Width::W16), 0xffff);
    assert_eq!(n.btc.last(), Some(&(0x10, BTC_WRFKT_DPK, BTC_WRFK_STOP)));
}

#[test]
fn the_new_events_read_well() {
    let e = Rfk2Event::Aborted {
        entry: "dpk",
        error: injected(0x1_0000),
    };
    assert!(
        e.to_string()
            .starts_with("[RFK] dpk stopped on Bus(Refused")
    );
    let e = Rfk2Event::UndoFailed {
        step: "resume_sch_tx",
        error: Rtw89Error::Bus(BusError::Gone),
    };
    assert_eq!(
        e.to_string(),
        "[RFK] putting back failed at resume_sch_tx: Bus(Gone)"
    );
}
