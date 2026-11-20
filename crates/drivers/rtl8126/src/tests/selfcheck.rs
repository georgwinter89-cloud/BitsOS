//! The stages after the start: link, first frame, address, router, interrupt. Each has a deadline and one line.

use super::device::{card, card_named, card_on, clean, frame, pass};
use crate::device::*;
use crate::fakecard::{Fake, Knobs, Op, AN_DELAY_NS};
use crate::machine::{IrqSetup, Machine};
use crate::regs::*;
use crate::stage::{Stage, Verdict};
use core::cell::Cell;
use std::string::String;
use std::vec;

/// The stack as the self-check sees it. A ping makes the router answer, as on a wire.
struct Lan<'a> {
    card: &'a Card<Fake>,
    lease: Cell<Option<([u8; 4], Option<[u8; 4]>)>>,
    router_answers: Cell<bool>,
    pings: Cell<u32>,
}

const ADDR: [u8; 4] = [192, 168, 100, 50];
const ROUTER: [u8; 4] = [192, 168, 100, 1];

impl<'a> Lan<'a> {
    fn new(card: &'a Card<Fake>) -> Self {
        Lan { card, lease: Cell::new(None), router_answers: Cell::new(true), pings: Cell::new(0) }
    }
}

impl Uplink for Lan<'_> {
    fn lease(&self, ifname: &str) -> Option<([u8; 4], Option<[u8; 4]>)> {
        assert_eq!(ifname, self.card.name());
        self.lease.get()
    }
    fn ping(&self, dst: [u8; 4], timeout_ns: u64) -> Option<u64> {
        assert_eq!(dst, ROUTER);
        self.pings.set(self.pings.get() + 1);
        if !self.router_answers.get() {
            self.card.hw().advance(timeout_ns);
            return None;
        }
        // The answer is a frame on the wire; the stack's thread takes it and looks until the ring is empty.
        assert!(self.card.hw().rx(&frame(self.pings.get(), 98)));
        assert_eq!(self.card.receive(&mut [0u8; 2048]), Some(98));
        assert_eq!(self.card.receive(&mut [0u8; 2048]), None);
        Some(350_000)
    }
}

fn line(c: &Card<Fake>, s: Stage) -> (Verdict, String) {
    let l = c.report().get(s).unwrap_or_else(|| panic!("no line for {s:?}: {:?}", c.report().lines()));
    (l.verdict, l.text)
}

/// Time passes with the poller and the self-check running, as in the kernel (worker every 250 ms).
fn run(c: &Card<Fake>, lan: &Lan<'_>, ns: u64) {
    for _ in 0..ns.div_ceil(250_000_000) {
        pass(c, 250_000_000);
        c.run_due_restart();
        c.advance(lan);
    }
}

fn up_to_lease<'a>(c: &'a Card<Fake>) -> Lan<'a> {
    let lan = Lan::new(c);
    c.start(false).unwrap();
    run(c, &lan, AN_DELAY_NS + 500_000_000);
    assert!(c.hw().rx(&frame(0, 342)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(342));
    run(c, &lan, 250_000_000);
    lan.lease.set(Some((ADDR, Some(ROUTER))));
    lan
}

#[test]
fn every_stage_passes_and_says_its_line() {
    let c = card_named("rtl-stages", Fake::new(), Config::default());
    let lan = Lan::new(&c);
    c.start(false).unwrap();
    assert_eq!(c.checking(), Some(Stage::Link));
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert_eq!(line(&c, Stage::Link), (Verdict::Ok, "Kabelverbindung steht: 1000 Mbit/s".into()));
    assert_eq!(c.checking(), Some(Stage::Rings));
    // The first frame from the wire (a DHCP offer, say) proves the receive ring.
    assert!(c.hw().rx(&frame(0, 342)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(342));
    run(&c, &lan, 250_000_000);
    assert_eq!(line(&c, Stage::Rings), (Verdict::Ok, "Empfang läuft: das erste Datenpaket ist angekommen".into()));
    assert_eq!(c.checking(), Some(Stage::Lease));
    lan.lease.set(Some((ADDR, Some(ROUTER))));
    run(&c, &lan, 250_000_000);
    assert_eq!(line(&c, Stage::Lease), (Verdict::Ok, "Adresse vom Router erhalten: 192.168.100.50".into()));
    run(&c, &lan, 250_000_000);
    assert_eq!(line(&c, Stage::Gateway), (Verdict::Ok, "Der Router antwortet (350 Mikrosekunden)".into()));
    // Only now a vector is asked for and the mask opened; the next frame's interrupt is the proof.
    assert_eq!(c.irq_setup(), "polled");
    assert!(!c.hw().ops().contains(&Op::IrqSetup));
    run(&c, &lan, 250_000_000);
    assert_eq!(c.irq_setup(), "irq");
    assert_eq!(c.hw().reg32(INTR_MASK), crate::irq::IRQ_MASK);
    assert!(!net::netdev::is_irq_driven("rtl-stages"), "the stack keeps polling until an interrupt was seen");
    run(&c, &lan, 500_000_000);
    assert_eq!(line(&c, Stage::Irq), (Verdict::Ok, "Die Karte meldet sich von selbst (Unterbrechung)".into()));
    assert_eq!(c.checking(), None);
    assert!(c.irq_count() > 0);
    c.report_irq();
    assert!(net::netdev::is_irq_driven("rtl-stages"));
    assert_eq!(c.report().lines().len(), 8);
    assert_eq!(c.report().lines()[0], "Netzwerk 1/8: Karte erkannt: RTL8126A, Kennung 64a, Steckplatz 83:00.0");
    assert_eq!(c.report().failed_at(), None);
    clean(&c);
    c.quiesce();
    c.report_irq();
    assert!(!net::netdev::is_irq_driven("rtl-stages"));
}

#[test]
fn no_cable_is_said_after_twenty_seconds_and_the_card_keeps_waiting() {
    let c = card_on(Fake::with(Knobs { cable: false, ..Knobs::default() }), Config::default());
    let lan = Lan::new(&c);
    c.start(false).unwrap();
    run(&c, &lan, LINK_WAIT_NS - 1_000_000_000);
    assert!(c.report().get(Stage::Link).is_none());
    run(&c, &lan, 2_000_000_000);
    assert_eq!(line(&c, Stage::Link), (Verdict::Failed, "Keine Kabelverbindung nach 20 Sekunden (Kabel prüfen); die Karte wartet weiter".into()));
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.report().failed_at(), Some(Stage::Link));
    // The cable an hour later: the check goes on from there.
    run(&c, &lan, 3_600_000_000_000);
    c.hw().cable(true);
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert_eq!(line(&c, Stage::Link).0, Verdict::Ok);
    assert_eq!(c.checking(), Some(Stage::Rings));
    clean(&c);
}

#[test]
fn no_frame_at_all_restarts_the_card_once_and_then_stops_it() {
    let c = card();
    let lan = Lan::new(&c);
    c.start(false).unwrap();
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert_eq!(c.transmit(&frame(0, 342)), Ok(()));
    run(&c, &lan, RX_WAIT_NS + 500_000_000);
    assert_eq!(c.state(), State::Running, "restarted once");
    assert_eq!(c.hw().ops().iter().filter(|o| **o == Op::Started).count(), 2);
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert_eq!(c.checking(), Some(Stage::Rings));
    run(&c, &lan, RX_WAIT_NS + 500_000_000);
    assert_eq!(c.state(), State::Stopped);
    let f = c.hw();
    assert!(!f.bus_master() && !f.started() && f.reg32(INTR_MASK) == 0, "bus master off, chip reset, interrupts masked");
    let (v, text) = line(&c, Stage::Rings);
    assert_eq!(v, Verdict::Failed);
    assert_eq!(text, "Kein Datenpaket nach 30 Sekunden, auch nach dem Neustart (gesendet 1, davon fertig 1): Karte angehalten");
    assert_eq!(c.report().failed_at(), Some(Stage::Rings));
    // Stopped for good.
    run(&c, &lan, 600_000_000_000);
    assert_eq!(c.state(), State::Stopped);
    assert_eq!(c.hw().ops().iter().filter(|o| **o == Op::Started).count(), 2);
    clean(&c);
}

#[test]
fn a_frame_after_the_restart_ends_the_matter() {
    let c = card();
    let lan = Lan::new(&c);
    c.start(false).unwrap();
    run(&c, &lan, AN_DELAY_NS + RX_WAIT_NS + 1_000_000_000);
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert!(c.hw().rx(&frame(0, 60)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(60));
    run(&c, &lan, 250_000_000);
    assert_eq!(line(&c, Stage::Rings).0, Verdict::Ok);
    assert_eq!(c.state(), State::Running);
}

#[test]
fn a_router_that_gives_no_address_is_said_and_nothing_is_stopped() {
    let c = card();
    let lan = Lan::new(&c);
    c.start(false).unwrap();
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert!(c.hw().rx(&frame(0, 60)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(60));
    run(&c, &lan, LEASE_WAIT_NS + 1_000_000_000);
    let (v, text) = line(&c, Stage::Lease);
    assert_eq!(v, Verdict::Failed);
    assert_eq!(text, "Noch keine Adresse vom Router nach 30 Sekunden (empfangen 1, gesendet 0, davon fertig 0)");
    assert_eq!(c.state(), State::Running);
    lan.lease.set(Some((ADDR, None)));
    run(&c, &lan, 500_000_000);
    assert_eq!(line(&c, Stage::Lease).0, Verdict::Ok, "a late address replaces the line");
    assert_eq!(line(&c, Stage::Gateway), (Verdict::Note, "Der Router hat sich selbst nicht genannt: keine Probe".into()));
}

#[test]
fn a_silent_router_is_asked_three_times() {
    let c = card();
    let lan = up_to_lease(&c);
    lan.router_answers.set(false);
    run(&c, &lan, 5_000_000_000);
    assert_eq!(line(&c, Stage::Gateway), (Verdict::Failed, "Der Router antwortet nicht auf 3 Anfragen".into()));
    assert_eq!(c.state(), State::Running);
    assert!(c.checking() != Some(Stage::Gateway), "the check goes on to the interrupt");
}

#[test]
fn an_interrupt_that_never_arrives_leaves_the_card_polled_and_working() {
    let c = card_named("rtl-silent", Fake::with(Knobs { irq_arrives: false, ..Knobs::default() }), Config::default());
    let lan = up_to_lease(&c);
    run(&c, &lan, 1_000_000_000);
    assert_eq!(c.irq_setup(), "irq", "armed, under proof");
    run(&c, &lan, IRQ_PROOF_NS + 1_000_000_000);
    assert_eq!(line(&c, Stage::Irq), (Verdict::Note, "Die Karte meldet sich nicht von selbst: sie wird weiter regelmäßig abgefragt".into()));
    assert_eq!(c.irq_setup(), "polled");
    assert_eq!(c.hw().reg32(INTR_MASK), 0);
    assert!(!net::netdev::is_irq_driven("rtl-silent"));
    assert_eq!(c.checking(), None);
    assert!(lan.pings.get() > 3, "the proof made its own traffic");
    // Frames still move, and a restart does not try the interrupt again in this boot.
    assert!(c.hw().rx(&frame(1, 60)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(60));
    let setups = c.hw().ops().iter().filter(|o| **o == Op::IrqSetup).count();
    c.restart("test").unwrap();
    run(&c, &lan, AN_DELAY_NS + 3_000_000_000);
    assert_eq!(c.hw().ops().iter().filter(|o| **o == Op::IrqSetup).count(), setups);
    assert_eq!(c.irq_setup(), "polled");
    assert_eq!(c.report().failed_at(), None, "a polled card is no failure");
    clean(&c);
}

#[test]
fn no_vector_means_polled_with_a_line() {
    let c = card_on(Fake::with(Knobs { irq: IrqSetup::Polled, ..Knobs::default() }), Config::default());
    let lan = up_to_lease(&c);
    run(&c, &lan, 1_000_000_000);
    assert_eq!(line(&c, Stage::Irq).0, Verdict::Note);
    assert_eq!(c.checking(), None);
}

#[test]
fn a_proven_interrupt_is_armed_again_by_a_restart_and_serves_the_ring() {
    let c = card();
    let lan = up_to_lease(&c);
    run(&c, &lan, 1_500_000_000);
    assert_eq!(c.checking(), None);
    let f = c.hw();
    // Interrupt mode as the stack drives it: the hook's note, then `receive` acknowledges, drains and unmasks.
    let before = c.irq_count();
    assert!(f.rx(&frame(5, 60)));
    assert_eq!(c.irq_count(), before + 1);
    assert!(f.rx(&frame(6, 60)), "a second frame before the driver looked");
    assert_eq!(c.irq_count(), before + 1, "no second message while the first is unacknowledged");
    let mut buf = vec![0u8; 2048];
    assert_eq!(c.receive(&mut buf), Some(60));
    assert_eq!(f.reg32(INTR_MASK), 0, "masked while the ring is drained (M:4875)");
    assert_eq!(c.receive(&mut buf), Some(60));
    assert_eq!(c.receive(&mut buf), None);
    assert_eq!(f.reg32(INTR_MASK), crate::irq::IRQ_MASK, "unmasked when the ring is empty (M:4925-4926)");
    assert_eq!(f.reg32(INTR_STATUS) & crate::irq::RX_OK, 0, "acknowledged");
    assert!(f.rx(&frame(7, 60)));
    assert_eq!(c.irq_count(), before + 2);
    assert_eq!(c.receive(&mut buf), Some(60));
    assert_eq!(c.receive(&mut buf), None);
    // A restart arms it at once and says so.
    c.restart("test").unwrap();
    assert_eq!(c.irq_setup(), "irq");
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert!(f.rx(&frame(8, 60)));
    assert_eq!(c.receive(&mut buf), Some(60));
    run(&c, &lan, 2_000_000_000);
    assert_eq!(line(&c, Stage::Irq), (Verdict::Ok, "Die Karte meldet sich von selbst (wie zuvor)".into()));
    clean(&c);
}

#[test]
fn a_proven_interrupt_that_stops_arriving_is_given_up_after_five_seconds_of_frames() {
    let c = card_named("rtl-lost", Fake::new(), Config::default());
    let lan = up_to_lease(&c);
    run(&c, &lan, 1_500_000_000);
    assert_eq!(c.irq_setup(), "irq");
    c.report_irq();
    assert!(net::netdev::is_irq_driven("rtl-lost"));
    c.hw().knobs(|k| k.irq_arrives = false);
    let mut buf = vec![0u8; 2048];
    for i in 0..8 {
        // The stack's slow safety look finds the frames.
        assert!(c.hw().rx(&frame(i, 60)));
        assert_eq!(c.receive(&mut buf), Some(60));
        assert_eq!(c.receive(&mut buf), None);
        run(&c, &lan, 1_000_000_000);
    }
    assert_eq!(c.irq_setup(), "polled");
    assert!(!net::netdev::is_irq_driven("rtl-lost"));
    assert_eq!(c.state(), State::Running);
    clean(&c);
}

#[test]
fn a_link_change_interrupt_makes_the_next_poll_look_at_once() {
    let c = card();
    let lan = up_to_lease(&c);
    run(&c, &lan, 1_500_000_000);
    assert!(c.link().up);
    c.hw().cable(false);
    // The stack's thread was woken by the hook and calls receive; one poll later the link is known to be gone,
    // well before the 100 ms look.
    assert_eq!(c.receive(&mut [0u8; 2048]), None);
    c.hw().advance(1_000_000);
    c.poll();
    assert!(!c.link().up);
    clean(&c);
}

#[test]
fn an_interrupt_proof_without_any_frame_ends_after_thirty_seconds_and_is_asked_again_later() {
    let c = card_named("rtl-noproof", Fake::new(), Config::default());
    let lan = up_to_lease(&c);
    lan.router_answers.set(false);
    run(&c, &lan, 5_000_000_000);
    assert_eq!(line(&c, Stage::Gateway).0, Verdict::Failed);
    assert_eq!((c.checking(), c.irq_setup()), (Some(Stage::Irq), "irq"), "armed, under proof, and no frame comes");
    assert!(c.report().get(Stage::Irq).is_none());
    run(&c, &lan, IRQ_PROOF_LIMIT_NS);
    assert_eq!(
        line(&c, Stage::Irq),
        (Verdict::Note, "Ob die Karte sich von selbst meldet, ließ sich nicht prüfen (kein Datenpaket in 30 Sekunden): sie wird weiter regelmäßig abgefragt".into())
    );
    assert_eq!((c.checking(), c.irq_setup()), (None, "polled"));
    assert_eq!(c.hw().reg32(INTR_MASK), 0);
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.report().lines().len(), 8, "the last stage has its line");
    let pings = lan.pings.get();
    run(&c, &lan, 60_000_000_000);
    assert_eq!(lan.pings.get(), pings, "the stage is over: nobody asks the router any more");
    // Not given up for the boot: the next bring-up asks for a vector again, and this time a frame proves it.
    lan.router_answers.set(true);
    let setups = c.hw().ops().iter().filter(|o| **o == Op::IrqSetup).count();
    c.restart("test").unwrap();
    run(&c, &lan, AN_DELAY_NS + 500_000_000);
    assert!(c.hw().rx(&frame(9, 60)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(60));
    run(&c, &lan, 2_000_000_000);
    assert_eq!(c.hw().ops().iter().filter(|o| **o == Op::IrqSetup).count(), setups + 1);
    assert_eq!(line(&c, Stage::Irq), (Verdict::Ok, "Die Karte meldet sich von selbst (Unterbrechung)".into()));
    clean(&c);
}

#[test]
fn a_mac_restart_puts_a_proven_interrupt_back_without_asking_for_a_vector() {
    let c = card();
    let lan = up_to_lease(&c);
    run(&c, &lan, 1_500_000_000);
    assert_eq!((c.checking(), c.irq_setup()), (None, "irq"));
    let f = c.hw();
    let setups = f.ops().iter().filter(|o| **o == Op::IrqSetup).count();
    f.knobs(|k| k.tx_stuck = true);
    assert_eq!(c.transmit(&frame(1, 60)), Ok(()));
    run(&c, &lan, TX_TIMEOUT_NS + 1_500_000_000);
    assert_eq!(c.state(), State::Running);
    assert!(c.link().up, "a MAC-only restart: the link stayed");
    assert_eq!(c.irq_setup(), "irq");
    assert_eq!(f.reg32(INTR_MASK), crate::irq::IRQ_MASK, "rtl_hw_start ends with rtl_irq_enable (M:4136)");
    assert_eq!(f.ops().iter().filter(|o| **o == Op::IrqSetup).count(), setups, "the vector was never given back");
    let before = c.irq_count();
    assert!(f.rx(&frame(2, 60)));
    assert_eq!(c.irq_count(), before + 1);
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(60));
    clean(&c);
}
