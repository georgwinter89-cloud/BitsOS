//! Wave 44 (TH): the thermal taker's refusals that hold its reach to Linux's: the driver table only at its own
//! place and length, no write back of a register that reads all ones, the sensors' read only after a start that
//! passed, and the acknowledge already after one that stopped.

use super::rdna4thermal::{armed, metrics_sent, mp1_writes, pass, th};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakethermal::Alert;
use crate::rdna4::thermalhw::{self, Gated};
use rdna4_core::ih7::Iv;
use rdna4_core::metrics14::METRICS_BYTES;
use rdna4_core::thm14::{self, Source};
use rdna4_core::thmgate::Action;
use std::vec;

#[test]
fn the_driver_table_reads_only_at_its_place_and_length() {
    let (s, mut m, _) = armed();
    let (at, r) = (th(&s).sensors.unwrap().driver_at, th(&s).regs.clone().unwrap());
    let _ = m.reg_read(r.smu_answer);
    let mut g = Gated::new(&mut m, &r, Action::Metrics, Some(at));
    let _ = g.reg_read(r.smu_answer);
    assert!(g.reg_write(r.smu_answer, 0) && g.reg_write(r.smu_argument, 5) && g.reg_write(r.smu_message, 0x12), "{:?}", g.refused);
    let answer = (0..1000).find_map(|_| g.reg_read(r.smu_answer).filter(|v| *v != 0));
    assert_eq!(answer, Some(1), "the transfer was answered OK");
    let mut b = vec![0u8; METRICS_BYTES];
    assert!(!g.mem_read(at + 0x1000, &mut b), "another place in VRAM");
    assert!(!g.mem_read(at - 4, &mut b), "a place just before the table");
    assert!(!g.mem_read(at, &mut vec![0u8; METRICS_BYTES - 4]), "a shorter read");
    assert!(!g.mem_read(at, &mut vec![0u8; METRICS_BYTES + 4]), "a longer read");
    assert!(g.mem_read(at, &mut b), "the table itself, at its length");
}

/// The model with two registers that read all ones (a card that does not answer there).
struct AllOnes<'a> {
    m: &'a mut Model,
    dead: [u32; 2],
}

impl Hw for AllOnes<'_> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        if self.dead.contains(&byte) { Some(u32::MAX) } else { self.m.reg_read(byte) }
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.m.reg_write(byte, value)
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.m.mem_read(at, buf)
    }
    fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.m.mem_write(at, data)
    }
    fn now_us(&mut self) -> u64 {
        self.m.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.m.delay_us(us)
    }
}

#[test]
fn a_register_that_reads_all_ones_is_not_written_back() {
    let (s, mut m, _) = armed();
    let r = m.regs.clone();
    m.raise_thermal(Alert::Smu(thm14::CTX_FAN_ABNORMAL)).unwrap();
    let at = m.writes.len();
    // The acknowledge's register reads all ones: nothing is written, the taker counts the failure.
    let mut t = th(&s).clone();
    let iv = Iv { client_id: thm14::CLIENT_MP1, src_id: thm14::SRC_TO_DRIVER, src_data: [thm14::CTX_FAN_ABNORMAL, 0, 0, 0], ..Iv::default() };
    t.take(Some(&mut AllOnes { m: &mut m, dead: [r.mp1_ih_sw_int_ctrl, 0] }), Source::Smu, &iv, 0);
    assert!(mp1_writes(&m, at).is_empty(), "{:?}", mp1_writes(&m, at));
    assert_eq!((t.failed, t.high_now), (1, None));
    // The acknowledge goes, the threshold's register reads all ones: the threshold is not written.
    let e = thermalhw::acknowledge(&mut AllOnes { m: &mut m, dead: [r.thm_int_ctrl, 0] }, &r, thm14::CTX_FAN_ABNORMAL, Some(108)).unwrap_err();
    assert!(e.contains("THM_THERMAL_INT_CTRL does not read"), "{e}");
    assert_eq!(mp1_writes(&m, at), [(r.mp1_ih_sw_int_ctrl, 0x100)]);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

#[test]
fn a_start_that_stopped_after_the_alert_acknowledges_but_sends_no_transfer() {
    // AllowIHHostInterrupt not answered OK: the alert's four registers went, the start did not pass.
    let (_, mut m, mut s) = super::rdna4pmfw::stopped(|m| m.pmfw.answer_with = Some((0x4D, 0xFF)));
    let sys = s.nb.ih.as_ref().unwrap().place.sys;
    let t = th(&s);
    assert_eq!((t.alert_armed, t.range.map(|r| r.software_shutdown_temp), t.regs.is_some(), t.sensors), (true, Some(118), true, None));
    // An MP1 interrupt (the model raises none without AllowIHHostInterrupt; the card might): acknowledged as
    // Linux acknowledges it, with the fan case's threshold.
    let at = m.writes.len();
    m.pmfw.thm.raised.push_back(thm14::CTX_FAN_RECOVERY);
    let iv = Iv { client_id: thm14::CLIENT_MP1, src_id: thm14::SRC_TO_DRIVER, src_data: [thm14::CTX_FAN_RECOVERY, 0, 0, 0], ..Iv::default() };
    let now = m.card.now_us;
    s.nb.ih.as_mut().unwrap().reader.thermal.take(Some(&mut m), Source::Smu, &iv, now);
    assert_eq!(mp1_writes(&m, at), [(m.regs.mp1_ih_sw_int_ctrl, 0x100), (m.regs.thm_int_ctrl, 0xb8ff_0076)], "{:?}", m.refused);
    assert_eq!((th(&s).failed, th(&s).high_now), (0, Some(118)));
    // A rising alert: the check reads nothing (no transfer after a start that did not pass) and switches off.
    m.raise_thermal(Alert::High).unwrap();
    pass(&mut s, &mut m, sys);
    m.delay_us(50_000);
    pass(&mut s, &mut m, sys);
    let t = th(&s);
    assert!(t.power_off.as_deref().is_some_and(|w| w.contains("could not be read after 50 ms (the power controller's start did not pass in this boot: BitsOS sends no table transfer then, where Linux would read)")), "{:?}", t.power_off);
    assert_eq!((metrics_sent(&m), t.reads, t.checks), (0, 0, 1));
    assert!(m.refused.is_empty(), "the taker tried nothing the model refuses: {:?}", m.refused);
}
