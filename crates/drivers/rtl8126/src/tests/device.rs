//! `device::Card` against the register-level fake: the bring-up's order, its failure paths, the data path, link
//! watch, restart rules and Quiesce. Every test that brings a card up ends with the fake having refused nothing.

use crate::device::*;
use crate::fakecard::{Fake, Knobs, Op, AN_DELAY_NS, MAC};
use crate::hw::Hw;
use crate::machine::Machine;
use crate::regs::*;
use crate::stage::{Stage, Verdict};
use devmodel::DevError;
use std::string::String;
use std::vec;
use std::vec::Vec;

pub fn card_on(fake: Fake, cfg: Config) -> Card<Fake> {
    card_named("rtl0", fake, cfg)
}

/// A card with a name of its own: for tests that look at what the stack's (process-wide) card list was told.
pub fn card_named(name: &str, fake: Fake, cfg: Config) -> Card<Fake> {
    Card::new(name.into(), "83:00.0".into(), fake, cfg).unwrap()
}

pub fn card() -> Card<Fake> {
    card_on(Fake::new(), Config::default())
}

/// Lets time pass in poller-sized steps, the poller running.
pub fn pass(c: &Card<Fake>, ns: u64) {
    let step = 50_000_000;
    for _ in 0..ns.div_ceil(step) {
        c.hw().advance(step);
        c.poll();
    }
}

/// A card that runs with link.
pub fn running() -> Card<Fake> {
    let c = card();
    c.start(false).unwrap();
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert!(c.link().up, "link after autonegotiation");
    c
}

pub fn clean(c: &Card<Fake>) {
    assert_eq!(c.hw().violations(), Vec::<String>::new());
}

pub fn frame(seq: u32, len: usize) -> Vec<u8> {
    let mut f = vec![0u8; len];
    f[..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&MAC);
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    for (i, b) in f.iter_mut().enumerate().skip(14) {
        *b = (seq as usize + i) as u8;
    }
    f
}

/// A firmware file of the real one's shape (13 232 bytes, 3279 actions from offset 112, the measured version
/// string) whose actions do nothing but two PHY writes at the start. Typed from the facts in `chip::FIRMWARE_3`.
pub fn firmware() -> Vec<u8> {
    let mut f = vec![0u8; 112];
    f[4..4 + 25].copy_from_slice(b"rtl8126a-3_0.0.5 08/30/24");
    f[36..40].copy_from_slice(&112u32.to_le_bytes());
    f[40..44].copy_from_slice(&3279u32.to_le_bytes());
    for i in 0..3279u32 {
        let action: u32 = match i {
            0 => 0x801f_0a43,
            1 => 0x8010_0127,
            2 => 0x801f_0000,
            _ => 0x7000_0000,
        };
        f.extend_from_slice(&action.to_le_bytes());
    }
    f.extend_from_slice(&[0; 4]);
    let sum = f.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    f[44] = sum.wrapping_neg();
    assert_eq!(f.len(), 13_232);
    f
}

fn line(c: &Card<Fake>, s: Stage) -> (Verdict, String) {
    let l = c.report().get(s).unwrap_or_else(|| panic!("no line for {s:?}: {:?}", c.report().lines()));
    (l.verdict, l.text)
}

fn is_phy_write(op: &Op) -> bool {
    matches!(op, Op::W32(GPHY_OCP, v) if v & 0x8000_0000 != 0)
}

#[test]
fn bring_up_in_linuxs_order() {
    let c = card();
    c.start(false).unwrap();
    assert_eq!(c.state(), State::Running);
    clean(&c);
    let f = c.hw();
    assert!(f.started(), "the fake saw everything Linux writes before the enable");
    // PCI first: MSI and bus master off, memory decoding, ASPM off, and only then a register is touched.
    let first_write = f.pos(|o| matches!(o, Op::W8(..) | Op::W16(..) | Op::W32(..)));
    assert_eq!(&f.ops()[..3], &[Op::PciOff, Op::Enable(false), Op::AspmOff]);
    assert!(first_write > 2);
    // M:5034-5037: bus master before the PHY, the PHY before the cleanup's reset, the start after it.
    let master = f.pos(|o| *o == Op::Enable(true));
    let first_phy = f.pos(is_phy_write);
    let resets: Vec<usize> = f.ops().iter().enumerate().filter(|(_, o)| **o == Op::Reset).map(|(i, _)| i).collect();
    let started = f.pos(|o| *o == Op::Started);
    assert_eq!(resets.len(), 2, "the probe's reset and the cleanup's");
    assert!(resets[0] < master && master < first_phy && first_phy < resets[1] && resets[1] < started);
    // `phy_start` comes last (M:5042): the advertisement and the restart of autonegotiation.
    let advert = f.pos_last(|o| matches!(o, Op::W32(GPHY_OCP, v) if v & 0xffff_8000 == 0x8000_0000 | (0xa5d4 << 15)));
    assert!(advert > started);
    // Polled: nobody unmasked an interrupt, nobody asked for a vector.
    assert_eq!(f.reg32(INTR_MASK), 0);
    assert!(!f.ops().contains(&Op::IrqSetup));
    assert_eq!(c.irq_setup(), "polled");
    assert_eq!(c.mac_bytes(), MAC);
    assert_eq!(c.xid(), 0x64a);
    assert_eq!(line(&c, Stage::Probe), (Verdict::Ok, "Karte erkannt: RTL8126A, Kennung 64a, Steckplatz 83:00.0".into()));
    assert_eq!(line(&c, Stage::Firmware).0, Verdict::Note, "no file in this test");
    assert_eq!(line(&c, Stage::Phy), (Verdict::Ok, "Leitungsteil erkannt, Kennung 001cc862".into()));
    assert_eq!(c.checking(), Some(Stage::Link));
}

#[test]
fn advertises_what_linux_advertises_and_no_eee() {
    let c = card();
    c.start(false).unwrap();
    let f = c.hw();
    assert_eq!(f.phy(0xa5d4), 0x0180, "2.5G and 5G");
    assert_eq!(f.phy(0xa408), 0x0de1, "10/100 half and full, both pauses, the selector kept");
    assert_eq!(f.phy(0xa412) & 0x0300, 0x0200, "1000 full only");
    assert_eq!((f.phy(0xa5d0), f.phy(0xa6d4)), (0, 0), "no EEE mode advertised");
    let slow = card_on(Fake::new(), Config { max_mbps: 1000, ..Config::default() });
    slow.start(false).unwrap();
    assert_eq!(slow.hw().phy(0xa5d4), 0);
    assert_eq!(slow.hw().phy(0xa412) & 0x0300, 0x0200);
    clean(&c);
    clean(&slow);
}

#[test]
fn firmware_is_played_before_the_settings_and_named() {
    let c = card_on(Fake::with(Knobs { firmware: Some(firmware()), ..Knobs::default() }), Config::default());
    c.start(false).unwrap();
    clean(&c);
    let f = c.hw();
    // The script sets bit 2 of page 0xa43 register 0x10; Linux's ALDPS setting clears it afterwards.
    assert_eq!(f.phy(0xa430), 0x0123);
    assert_eq!(line(&c, Stage::Firmware), (Verdict::Ok, "Firmware geladen: rtl8126a-3_0.0.5 08/30/24".into()));
    let id_read = f.pos(|o| *o == Op::W32(GPHY_OCP, 0xa404 << 15));
    let script = f.pos(|o| matches!(o, Op::W32(GPHY_OCP, v) if v & 0x8000_ffff == 0x8000_0127));
    assert!(id_read < script, "no script reaches a PHY whose identity was not read");
}

#[test]
fn another_firmware_version_is_played_and_said() {
    let mut other = firmware();
    other[4 + 15] = b'6';
    other[44] = 0;
    let sum = other.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    other[44] = sum.wrapping_neg();
    let c = card_on(Fake::with(Knobs { firmware: Some(other), ..Knobs::default() }), Config::default());
    c.start(false).unwrap();
    let (v, text) = line(&c, Stage::Firmware);
    assert_eq!(v, Verdict::Note);
    assert!(text.contains("rtl8126a-3_0.0.6") && text.contains("andere Fassung"), "{text}");
    assert_eq!(c.hw().phy(0xa430), 0x0123);
}

#[test]
fn a_damaged_firmware_file_is_not_played() {
    let mut bad = firmware();
    bad[200] ^= 1;
    let c = card_on(Fake::with(Knobs { firmware: Some(bad), ..Knobs::default() }), Config::default());
    c.start(false).unwrap();
    assert_eq!(c.state(), State::Running);
    assert_eq!(line(&c, Stage::Firmware), (Verdict::Note, "Firmware-Datei beschädigt: die Karte läuft ohne sie weiter".into()));
    assert_eq!(c.hw().phy(0xa430) & 0x0123, 0, "nothing of it reached the PHY");
    clean(&c);
}

#[test]
fn the_first_start_waits_for_the_stick_and_only_on_a_thread_that_may_sleep() {
    let late = || Fake::with(Knobs { firmware: Some(firmware()), firmware_from_ns: 8_000_000_000, ..Knobs::default() });
    // The worker: waits in 1 s steps until the file is there.
    let c = card_on(late(), Config::default());
    c.start(true).unwrap();
    assert_eq!(line(&c, Stage::Firmware).0, Verdict::Ok);
    assert!((8..=10).contains(&c.hw().fw_reads()), "{} reads", c.hw().fw_reads());
    // Probe without a worker: one look, then on without it.
    let c = card_on(late(), Config::default());
    c.start(false).unwrap();
    assert_eq!(line(&c, Stage::Firmware).0, Verdict::Note);
    assert_eq!(c.hw().fw_reads(), 1);
    // A stick that never comes costs the configured wait once, and the card still runs.
    let c = card_on(Fake::new(), Config { fw_wait_ns: 5_000_000_000, ..Config::default() });
    c.start(true).unwrap();
    assert_eq!(c.state(), State::Running);
    let first = c.hw().fw_reads();
    assert!((5..=7).contains(&first), "{first} reads");
    c.restart("test").unwrap();
    assert_eq!(c.hw().fw_reads(), first + 1, "the wait is spent: later bring-ups look once");
    clean(&c);
}

#[test]
fn an_unknown_chip_is_not_touched() {
    let c = card_on(Fake::with(Knobs { tx_config: 0x6410_0000, ..Knobs::default() }), Config::default());
    assert!(matches!(c.start(false), Err(e) if e.fatal()));
    assert_eq!(c.state(), State::Stopped);
    let f = c.hw();
    // Not one register of it was written, not even to stop it: only the PCI function was switched off.
    assert!(!f.ops().iter().any(|o| matches!(o, Op::W8(..) | Op::W16(..) | Op::W32(..) | Op::Cfg(..))), "{:?}", f.ops());
    assert!(!f.bus_master());
    c.quiesce();
    assert!(!f.ops().iter().any(|o| matches!(o, Op::W8(..) | Op::W16(..) | Op::W32(..))), "a Quiesce does not write to it either");
    assert_eq!(line(&c, Stage::Probe), (Verdict::Failed, "Unbekannter Chip, Kennung 641: nicht angefasst".into()));
    // Stopped for good: the poller never retries it.
    let before = f.ops().len();
    pass(&c, 120_000_000_000);
    assert_eq!(f.ops().len(), before);
}

#[test]
fn a_card_that_reads_all_ones_is_retried_with_back_off() {
    let c = card_on(Fake::with(Knobs { tx_config: u32::MAX, ..Knobs::default() }), Config::default());
    assert!(matches!(c.start(false), Err(e) if !e.fatal()));
    assert_eq!(c.state(), State::Failed);
    assert_eq!(line(&c, Stage::Probe).1, "Die Karte antwortet nicht (alle Werte lesen sich als Einsen)");
    pass(&c, 9_000_000_000);
    assert_eq!(c.retry_info().0, 1, "not before 10 s");
    c.hw().knobs(|k| k.tx_config = 0x64a0_0000);
    pass(&c, 2_000_000_000);
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.retry_info().0, 0);
    clean(&c);
}

#[test]
fn a_wrong_phy_or_a_fibre_module_stops_the_card_for_good() {
    let c = card_on(Fake::with(Knobs { phy_id: 0x001c_c800, ..Knobs::default() }), Config::default());
    assert!(matches!(c.start(false), Err(BringUpError::Phy(_))));
    assert_eq!(c.state(), State::Stopped);
    assert!(!c.hw().ops().iter().any(is_phy_write), "nothing was written to a PHY this driver does not know");
    assert!(!c.hw().bus_master() && !c.hw().started());
    assert_eq!(line(&c, Stage::Phy), (Verdict::Failed, "Leitungsteil mit unbekannter Kennung 001cc800: Karte angehalten".into()));
    assert_eq!(c.report().failed_at(), Some(Stage::Phy));

    let c = card_on(Fake::with(Knobs { sfp: true, ..Knobs::default() }), Config::default());
    assert_eq!(c.start(false), Err(BringUpError::Sfp));
    assert_eq!(c.state(), State::Stopped);
    assert!(!c.hw().ops().contains(&Op::Enable(true)), "bus master was never on");
}

#[test]
fn a_reset_that_never_ends_is_an_error_not_a_hang() {
    let c = card_on(Fake::with(Knobs { stuck_reset: true, ..Knobs::default() }), Config::default());
    assert!(c.start(false).is_err());
    assert_eq!(c.state(), State::Failed);
    assert!(!c.hw().bus_master());
    assert_eq!(c.hw().reg32(INTR_MASK), 0);
    assert_eq!(line(&c, Stage::Probe).0, Verdict::Failed);
}

#[test]
fn waits_that_run_out_are_passed_over_as_linux_does() {
    // The FIFOs never report empty and the link list never ready: Linux logs each wait and carries on (M:898-913).
    let c = card();
    c.hw().poke8(MCU, 0);
    c.hw().poke8(INTR_MITIGATE, 0);
    c.hw().set_mac_ocp(0xe00e, 1 << 13);
    c.start(false).unwrap();
    assert_eq!(c.state(), State::Running);
    assert!(c.hw().started());
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert_eq!(c.transmit(&frame(0, 60)), Ok(()));
    assert_eq!(c.hw().sent().len(), 1);
    clean(&c);
}

#[test]
fn a_dead_phy_window_fails_the_phy_stage_and_leaves_nothing_on() {
    let c = card_on(Fake::with(Knobs { stuck_phy: true, ..Knobs::default() }), Config::default());
    assert!(matches!(c.start(false), Err(BringUpError::Phy(_))));
    assert_eq!(c.state(), State::Failed);
    assert_eq!(line(&c, Stage::Probe).0, Verdict::Ok, "the stages before it stay as they were");
    assert_eq!(line(&c, Stage::Phy).0, Verdict::Failed);
    let f = c.hw();
    // Bus master was on for the PHY stage (Linux's order) and is off again; the chip never started.
    assert!(f.pos(|o| *o == Op::Enable(true)) < f.pos_last(|o| *o == Op::PciOff));
    assert!(!f.bus_master() && !f.started());
}

#[test]
fn pci_enable_refused_is_final_and_failed_is_retried() {
    let c = card_on(Fake::with(Knobs { enable_refused: true, ..Knobs::default() }), Config::default());
    assert!(matches!(c.start(false), Err(BringUpError::Pci(_))));
    assert_eq!(c.state(), State::Stopped);
    let c = card_on(Fake::with(Knobs { enable_fails: true, ..Knobs::default() }), Config::default());
    assert!(c.start(false).is_err());
    assert_eq!(c.state(), State::Failed);
}

#[test]
fn without_extended_config_space_the_card_still_starts() {
    let c = card_on(Fake::with(Knobs { no_cfg: true, ..Knobs::default() }), Config::default());
    c.start(false).unwrap();
    assert!(c.hw().started());
    assert!(!c.hw().ops().iter().any(|o| matches!(o, Op::Cfg(..) | Op::ReadRq)));
    clean(&c);
}

#[test]
fn dma_memory_above_the_limit_is_refused_before_the_card_hears_of_it() {
    let f = Fake::with(Knobs { dma_base: DMA_LIMIT - 0x1000, ..Knobs::default() });
    let e = Card::new("rtl0".into(), "83:00.0".into(), f, Config::default()).err().unwrap();
    assert!(e.contains("Layout"), "{e}");
}

#[test]
fn frames_go_out_padded_and_in_order_around_the_ring() {
    let c = running();
    let f = c.hw();
    assert_eq!(c.transmit(&frame(0, 14)), Ok(()));
    let sent = f.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].len(), 60, "padded to 60 (M:4448)");
    assert!(sent[0][14..].iter().all(|b| *b == 0), "the filler is zero even in memory that held 0xEE");
    // Three times around the ring.
    for i in 0..800u32 {
        let len = 60 + (i as usize * 7) % 1455;
        assert_eq!(c.transmit(&frame(i, len)), Ok(()), "frame {i}");
        assert_eq!(f.sent(), [frame(i, len)]);
    }
    assert_eq!(c.transmit(&frame(0, 1519)), Err(DevError::InvalidArgument), "longer than MTU + header + one VLAN tag");
    assert_eq!(c.transmit(&[0u8; 13]), Err(DevError::InvalidArgument), "shorter than an Ethernet header");
    assert_eq!(c.stats().tx_frames, 801);
    assert_eq!(c.tx_done(), Some(true));
    assert_eq!(c.tx_info(), (0, 801));
    clean(&c);
}

#[test]
fn frames_come_in_and_the_descriptor_goes_back_whatever_the_chip_left_in_it() {
    let c = running();
    let f = c.hw();
    f.knobs(|k| k.scribble_addr = true);
    let mut buf = vec![0u8; 2048];
    assert_eq!(c.receive(&mut buf), None);
    // More than twice around the ring, with the chip zeroing the address field each time.
    for i in 0..600u32 {
        let len = 60 + (i as usize * 11) % 1459;
        assert!(f.rx(&frame(i, len)), "frame {i} had a descriptor");
        assert_eq!(c.receive(&mut buf), Some(len));
        assert_eq!(&buf[..len], &frame(i, len)[..]);
    }
    assert_eq!(c.receive(&mut buf), None);
    assert_eq!(c.stats().rx_frames, 600);
    clean(&c);
}

#[test]
fn a_burst_fills_the_ring_and_nothing_is_lost_or_doubled() {
    let c = running();
    let f = c.hw();
    let mut took = 0;
    for i in 0..300u32 {
        took += f.rx(&frame(i, 100)) as u32;
    }
    assert_eq!(took, 256, "the ring holds 256; the rest found no descriptor");
    let mut buf = vec![0u8; 2048];
    for i in 0..256u32 {
        assert_eq!(c.receive(&mut buf), Some(100));
        assert_eq!(&buf[..100], &frame(i, 100)[..]);
    }
    assert_eq!(c.receive(&mut buf), None);
    assert!(f.rx(&frame(7, 100)));
    assert_eq!(c.receive(&mut buf), Some(100));
    clean(&c);
}

#[test]
fn a_frame_too_long_for_the_callers_buffer_is_dropped_and_the_ring_goes_on() {
    let c = running();
    let f = c.hw();
    assert!(f.rx(&frame(1, 1000)));
    assert!(f.rx(&frame(2, 70)));
    let mut small = vec![0u8; 512];
    assert_eq!(c.receive(&mut small), Some(70));
    assert_eq!(c.stats().rx_dropped, 1);
    clean(&c);
}

#[test]
fn nothing_is_sent_without_link_or_while_stopped() {
    let c = card();
    assert_eq!(c.transmit(&frame(0, 60)), Err(DevError::Io), "not started");
    c.start(false).unwrap();
    assert_eq!(c.transmit(&frame(0, 60)), Err(DevError::Io), "no link yet");
    assert!(!c.poll_tx(&frame(0, 60)));
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert_eq!(c.transmit(&frame(0, 60)), Ok(()));
    assert!(c.poll_tx(&frame(1, 60)));
    assert!(!c.with_tx_locked(|| c.poll_tx(&frame(2, 60))), "a held TX lock (a panic inside a transmit) drops the frame");
    assert_eq!(c.with_tx_locked(|| c.tx_done()), None);
    assert_eq!(c.hw().sent().len(), 2);
    clean(&c);
}

#[test]
fn link_edges_are_seen_and_the_mac_is_told_not_to_nap() {
    let c = card();
    c.hw().set_mac_ocp(0xe040, 0x1003);
    c.start(false).unwrap();
    assert!(!c.link().up);
    pass(&c, AN_DELAY_NS + 200_000_000);
    let l = c.link();
    assert!(l.up && l.speed_mbps == 1000 && l.full_duplex);
    assert_eq!(c.hw().mac_ocp(0xe040) & 0x0003, 0, "M:4959 with EEE not negotiated");
    c.hw().cable(false);
    pass(&c, 200_000_000);
    assert!(!c.link().up);
    c.hw().knobs(|k| k.speed_bits = 0x0210);
    c.hw().cable(true);
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert_eq!(c.link().speed_mbps, 2500);
    clean(&c);
}

#[test]
fn a_phy_window_that_stops_answering_restarts_the_card() {
    let c = running();
    c.hw().knobs(|k| k.stuck_phy = true);
    pass(&c, 1_200_000_000);
    assert_eq!(c.state(), State::Failed, "the restart's own PHY stage fails while the window is dead");
    c.hw().knobs(|k| k.stuck_phy = false);
    pass(&c, 11_000_000_000);
    assert_eq!(c.state(), State::Running);
    clean(&c);
}

#[test]
fn frames_the_card_keeps_restart_it_and_after_three_restarts_stop_it() {
    let c = running();
    let f = c.hw();
    f.knobs(|k| k.tx_stuck = true);
    for round in 1..=TX_TIMEOUT_STOP {
        assert_eq!(c.transmit(&frame(round, 60)), Ok(()));
        pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
        assert_eq!(c.state(), State::Running, "restart {round} brought it back");
        assert_eq!(c.tx_info().0, 0, "the ring is empty after the restart");
        pass(&c, AN_DELAY_NS + 200_000_000);
    }
    assert_eq!(c.transmit(&frame(9, 60)), Ok(()));
    pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
    assert_eq!(c.state(), State::Stopped);
    assert!(!f.bus_master() && f.reg32(INTR_MASK) == 0);
    assert_eq!(line(&c, Stage::Rings).1, "Gesendete Pakete bleiben hängen, auch nach 3 Neustarts: Karte angehalten");
    clean(&c);
}

#[test]
fn frames_that_waited_out_a_pulled_cable_are_not_a_transmit_timeout() {
    let c = running();
    let f = c.hw();
    f.knobs(|k| k.tx_stuck = true);
    assert_eq!(c.transmit(&frame(1, 60)), Ok(()));
    f.cable(false);
    pass(&c, 30_000_000_000);
    assert_eq!(c.state(), State::Running, "no link, no timeout");
    f.knobs(|k| k.tx_stuck = false);
    f.cable(true);
    pass(&c, AN_DELAY_NS + 1_500_000_000);
    assert_eq!(f.ops().iter().filter(|o| **o == Op::Started).count(), 1, "no restart when the link came back");
    // The card sends what it held once it is rung again (a real chip goes on by itself).
    assert_eq!(c.transmit(&frame(2, 60)), Ok(()));
    assert_eq!(f.sent().len(), 2);
    clean(&c);
}

#[test]
fn one_frame_sent_ends_a_run_of_transmit_timeouts() {
    let c = running();
    let f = c.hw();
    for _ in 0..5 {
        f.knobs(|k| k.tx_stuck = true);
        assert_eq!(c.transmit(&frame(1, 60)), Ok(()));
        pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
        pass(&c, AN_DELAY_NS + 200_000_000);
        f.knobs(|k| k.tx_stuck = false);
        assert_eq!(c.transmit(&frame(2, 60)), Ok(()));
        pass(&c, 1_100_000_000);
    }
    assert_eq!(c.state(), State::Running);
    clean(&c);
}

#[test]
fn a_full_ring_says_timeout_and_takes_frames_again_when_the_card_catches_up() {
    let c = running();
    let f = c.hw();
    f.knobs(|k| k.tx_stuck = true);
    for i in 0..256 {
        assert_eq!(c.transmit(&frame(i, 60)), Ok(()));
    }
    assert_eq!(c.transmit(&frame(256, 60)), Err(DevError::Timeout));
    assert_eq!(c.tx_done(), Some(false));
    f.knobs(|k| k.tx_stuck = false);
    // A real chip goes on by itself; the fake only works on a doorbell.
    c.hw().wr16(TX_POLL, 1);
    assert_eq!(c.transmit(&frame(256, 60)), Ok(()));
    assert_eq!(f.sent().len(), 257);
    clean(&c);
}

#[test]
fn quiesce_takes_the_bus_master_first_and_leaves_the_chip_reset_and_masked() {
    let c = running();
    let f = c.hw();
    assert_eq!(c.transmit(&frame(0, 60)), Ok(()));
    f.clear_ops();
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    let ops = f.ops();
    assert_eq!(ops[0], Op::PciOff, "M:5018: bus master off before the chip is stopped");
    assert!(ops.contains(&Op::Reset));
    assert!(!f.bus_master() && !f.started() && f.reg32(INTR_MASK) == 0);
    assert_eq!(f.reg32(RX_CONFIG) & 0x3f, 0, "accepts nothing");
    assert_eq!(f.mac_ocp(0xc0ac) & 0x1f80, 0, "M:5022 rtl_disable_exit_l1");
    assert_eq!(c.transmit(&frame(1, 60)), Err(DevError::Io));
    assert_eq!(c.receive(&mut [0u8; 2048]), None);
    // Nothing brings a quiesced card back by itself.
    pass(&c, 120_000_000_000);
    assert_eq!(c.state(), State::Quiesced);
    c.restart("test").unwrap();
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert_eq!(c.transmit(&frame(2, 60)), Ok(()));
    clean(&c);
}

#[test]
fn quiesce_on_a_card_that_never_started_and_with_the_tx_lock_held_ends() {
    let c = card();
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    assert_eq!(c.restart("test").err(), None, "an explicit restart brings it up again");
    pass(&c, AN_DELAY_NS + 200_000_000);
    c.with_tx_locked(|| c.quiesce());
    assert_eq!(c.state(), State::Quiesced);
    assert!(!c.hw().bus_master());
    clean(&c);
}

#[test]
fn a_whole_life_writes_only_what_the_list_allows() {
    let c = card_on(Fake::with(Knobs { firmware: Some(firmware()), ..Knobs::default() }), Config::default());
    c.start(false).unwrap();
    pass(&c, AN_DELAY_NS + 200_000_000);
    let mut buf = vec![0u8; 2048];
    for i in 0..300 {
        assert_eq!(c.transmit(&frame(i, 64)), Ok(()));
        assert!(c.hw().rx(&frame(i, 64)));
        assert_eq!(c.receive(&mut buf), Some(64));
    }
    c.hw().cable(false);
    pass(&c, 500_000_000);
    c.hw().cable(true);
    pass(&c, AN_DELAY_NS + 200_000_000);
    c.restart("test").unwrap();
    c.quiesce();
    // The fake refuses anything outside `regs::WRITES`, any `Cfg9346` value but lock and unlock, any config byte but
    // the two: so no EEPROM, eFuse or flash path was taken, because none exists.
    clean(&c);
    for op in c.hw().ops() {
        let (off, width) = match op {
            Op::W8(o, _) => (o, Width::B1),
            Op::W16(o, _) => (o, Width::B2),
            Op::W32(o, _) => (o, Width::B4),
            _ => continue,
        };
        assert!(write_allowed(off, width) && !NEVER_WRITTEN.contains(&off), "{op:?}");
    }
}
