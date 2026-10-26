//! Host tests against the register-level fake (`fake.rs`): the reset order, the PHY semaphore cases, the rings
//! and the restart rules. The scenarios are V1's `nic-test.mjs` / `igb-test.mjs` ones (spec net.md §6),
//! expressed against registers instead of guest memory.

use crate::device::{self, BringUpError, Card, Config, State};
use crate::irq::IrqSetup;
use crate::fake::{Ev, Fake, AN_DELAY_NS, MAC};
use crate::hw::{cmd, PciFail};
use crate::phy::{self, PhyError};
use crate::regs::{self, Model};
use crate::reset;
use alloc::string::String;
use alloc::vec::Vec;
use devmodel::DevError;

const MS: u64 = 1_000_000;

fn small() -> Config {
    Config { rx_ring: 32, tx_ring: 32, ..Config::default() }
}

static NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Each card gets its own name, so a test finds its own events in the shared klog ring (tests run in parallel).
fn card_on(fake: Fake, model: Model, cfg: Config) -> Card<Fake> {
    klog::heap_ready();
    let n = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    Card::new(alloc::format!("igct{n}"), model, fake, cfg).expect("rings")
}

fn card(model: Model) -> Card<Fake> {
    card_on(Fake::new(model), model, small())
}

/// Started, autonegotiated (500 ms in the fake) and the link seen by a poll.
fn running(model: Model) -> Card<Fake> {
    let c = card(model);
    c.start().expect("bring-up");
    settle(&c);
    assert!(c.link().up, "link after autonegotiation");
    c
}

fn settle(c: &Card<Fake>) {
    c.hw().advance(AN_DELAY_NS + 10 * MS);
    c.poll();
}

fn frame(seq: u32, len: usize) -> Vec<u8> {
    let mut f = alloc::vec![0u8; len];
    f[0..6].copy_from_slice(&[0xFF; 6]);
    f[6..12].copy_from_slice(&MAC);
    f[12..14].copy_from_slice(&0x88B5u16.to_be_bytes());
    for (i, b) in f[14..].iter_mut().enumerate() {
        *b = (seq as usize * 7 + i) as u8;
    }
    if len >= 18 {
        f[14..18].copy_from_slice(&seq.to_be_bytes());
    }
    f
}

fn pos(t: &[Ev], want: impl Fn(&Ev) -> bool) -> usize {
    t.iter().position(want).unwrap_or_else(|| panic!("event not in trace: {t:x?}"))
}

fn wr(off: usize) -> impl Fn(&Ev) -> bool {
    move |e| matches!(e, Ev::Wr(o, _) if *o == off)
}

fn wr_val(t: &[Ev], off: usize) -> u32 {
    t.iter().rev().find_map(|e| match e { Ev::Wr(o, v) if *o == off => Some(*v), _ => None }).expect("written")
}

// ---------------------------------------------------------------------------------------------------------
// Bring-up order

/// nic-test boot 1: firmware left bus master, receive, transmit, DRV_LOAD and its MSI on. Bus master goes off
/// before anything, MSI off before the first register, the reset follows the spec's order, bus master comes back
/// only with the rings, after DRV_LOAD.
#[test]
fn bring_up_order_after_firmware_left_everything_on_i226() {
    let f = Fake::new(Model::I226);
    f.set_pci(cmd::MEMORY | cmd::BUS_MASTER);
    f.set_msi(true, true);
    f.poke(regs::RCTL, regs::RCTL_ON);
    f.poke(regs::TCTL, regs::TCTL_EN | regs::TCTL_PSP);
    f.poke(regs::CTRL_EXT, regs::CTRL_EXT_DRV_LOAD);
    f.poke(regs::PTM_CTRL, 0xC000_0001);
    f.poke(regs::IPCNFG, 0x1C | 0x3);
    f.poke(regs::EEER, 0x7_0000 | 0x1);
    let c = card_on(f, Model::I226, small());
    c.start().expect("bring-up");
    let f = c.hw();
    let t = f.trace();
    assert_eq!(t[0], Ev::MsiOff, "MSI off first (disable_device): {:x?}", t[0]);
    assert!(matches!(t[1], Ev::Pci(v) if v & cmd::BUS_MASTER == 0), "then bus master off: {:x?}", t[1]);
    let msi = pos(&t, |e| *e == Ev::MsiOff);
    let decode = pos(&t, |e| matches!(e, Ev::Pci(v) if v & (cmd::MEMORY | cmd::INTX_DISABLE) == cmd::MEMORY | cmd::INTX_DISABLE));
    let first_wr = pos(&t, |e| matches!(e, Ev::Wr(..)));
    assert!(msi < first_wr && decode < first_wr, "MSI off and enable_device(false) before the first register write");
    assert!(pos(&t, |e| *e == Ev::AspmOff) < first_wr, "ASPM off on the I226");
    let ptm = pos(&t, wr(regs::PTM_CTRL));
    let gio = pos(&t, |e| matches!(e, Ev::Wr(regs::CTRL, v) if v & regs::ctrl::GIO_MASTER_DISABLE != 0));
    let reset_at = pos(&t, |e| *e == Ev::Reset);
    assert!(ptm < gio && gio < reset_at, "PTM off, GIO master disable, then the reset");
    assert_eq!(wr_val(&t[..reset_at], regs::PTM_CTRL), 0x0000_0001, "PTM enable bits cleared, the rest kept");
    let rctl0 = t[..reset_at].contains(&Ev::Wr(regs::RCTL, 0));
    let tctl = t[..reset_at].contains(&Ev::Wr(regs::TCTL, regs::TCTL_PSP));
    assert!(rctl0 && tctl, "RCTL = 0 and TCTL = PSP before the reset");
    assert!(t[gio..reset_at].contains(&Ev::Wr(Model::I226.imc(), !0)), "masked before the reset (I226 IMC at 0x150C)");
    assert!(t[reset_at..].contains(&Ev::Wr(Model::I226.imc(), !0)), "masked again after it");
    assert!(t[reset_at..].contains(&Ev::Wr(regs::EIMC, !0)));
    for (r, v) in [(regs::RXPBS, regs::RXPBS_I226), (regs::TXPBS, regs::TXPBS_I226)] {
        assert!(pos(&t, |e| *e == Ev::Wr(r, v)) > reset_at, "I226 value {r:#x} after the reset");
    }
    assert!(f.peek(regs::RFCTL) & regs::RFCTL_IPV6_EXT_DIS != 0);
    assert_eq!(f.peek(regs::IPCNFG), 0x3, "EEE off, other bits kept");
    assert_eq!(f.peek(regs::EEER), 0x1);
    assert_eq!(f.peek(regs::PHPM) & regs::PHPM_FW_LOCKS, 0, "firmware's PHPM locks cleared");
    assert_eq!(t.iter().filter(|e| **e == Ev::PhyReset).count(), 1, "one PHY reset");
    assert!(pos(&t, |e| *e == Ev::PhyReset) > reset_at);
    let drv = pos(&t, |e| matches!(e, Ev::Wr(regs::CTRL_EXT, v) if v & regs::CTRL_EXT_DRV_LOAD != 0));
    let dma = pos(&t, |e| *e == Ev::EnableDma);
    let rx_off = pos(&t, |e| *e == Ev::Wr(regs::RXDCTL, 0));
    let rx_base = pos(&t, wr(regs::RDBAL));
    assert!(reset_at < drv && drv < dma && dma < rx_off && rx_off < rx_base, "DRV_LOAD, bus master, then the rings");
    assert!(!t.contains(&Ev::DmaWithoutBme));
    assert_eq!(f.msi(), (false, false));
    assert_eq!(f.peek(regs::CTRL_EXT) & regs::CTRL_EXT_DRV_LOAD, regs::CTRL_EXT_DRV_LOAD, "ours now");
    assert_eq!(f.peek(regs::RCTL), regs::RCTL_ON);
    assert_eq!(f.peek(regs::TCTL) & (regs::TCTL_EN | regs::TCTL_PSP), regs::TCTL_EN | regs::TCTL_PSP);
    assert_eq!(f.peek(regs::RDT), 31, "one RX descriptor held back");
    assert_eq!(f.peek(regs::SRRCTL), regs::SRRCTL_ADV_2K);
    assert_eq!(f.peek(regs::RXDCTL), regs::QUEUE_ENABLE | regs::RX_WTHRESH1);
    assert_eq!(f.peek(regs::CTRL) & (regs::ctrl::SLU | regs::ctrl::FORCE_SPEED_DUPLEX), regs::ctrl::SLU);
    // The PHY: Linux's advertisement, autonegotiation on and restarted, semaphore given back.
    assert_eq!(f.phy_reg(regs::phy::ANAR), 0x01E1, "10/100 half+full, selector, no pause");
    assert_eq!(f.phy_reg(regs::phy::CTRL1000), regs::phy::CTRL1000_FD);
    assert_eq!(f.mmd_2500(), regs::phy::MULTIGBT_2500_FD, "2500 full advertised through MMD 7");
    assert_eq!(f.phy_reg(regs::phy::BMCR) & (regs::phy::BMCR_AN_ENABLE | regs::phy::BMCR_POWER_DOWN), regs::phy::BMCR_AN_ENABLE);
    assert_eq!(f.peek(regs::SW_FW_SYNC), 0, "PHY0 given back");
    assert_eq!(f.peek(regs::SWSM), 0, "SWSM given back");
    assert_eq!(c.mac_bytes(), MAC);
    assert!(f.pci() & cmd::BUS_MASTER != 0);
    // Interrupts stay masked while polled.
    assert_eq!(f.peek(Model::I226.ims()), 0);
    assert_eq!(f.peek(regs::EIMS), 0);
    assert!(!c.link().up, "no link before autonegotiation finished");
    settle(&c);
    let l = c.link();
    assert!(l.up && l.speed_mbps == 1000 && l.full_duplex);
    assert!(klog_has(&c, "link.up"));
}

/// QEMU's 82576: IMC/ICR at the old offsets, PHY at address 1, no PTM, no I226 values, no PHY reset, no ASPM,
/// no MMD (an MDIC error there), RXDCTL coming enabled out of reset is turned off before the ring is set.
#[test]
fn bring_up_82576_differs_where_the_kinds_differ() {
    let c = card(Model::I82576);
    c.start().expect("bring-up");
    let f = c.hw();
    let t = f.trace();
    let reset_at = pos(&t, |e| *e == Ev::Reset);
    assert!(!t.iter().any(wr(regs::PTM_CTRL)) && !t.iter().any(wr(regs::RXPBS)) && !t.iter().any(wr(regs::PHPM)));
    assert!(!t.contains(&Ev::PhyReset) && !t.contains(&Ev::AspmOff));
    assert!(t[reset_at..].contains(&Ev::Wr(0xD8, !0)), "82576 IMC at 0xD8");
    assert!(!t.iter().any(wr(0x150C)));
    let rx_off = pos(&t, |e| *e == Ev::Wr(regs::RXDCTL, 0));
    assert!(rx_off > reset_at && rx_off < pos(&t, wr(regs::RDBAL)), "RXDCTL off before the ring");
    assert_eq!(f.mmd_2500(), 0);
    assert_eq!(f.phy_reg(regs::phy::ANAR), 0x01E1);
    settle(&c);
    assert!(c.link().up);
}

/// igb-test: the whole probe as an I226 on an 82576 (PHY address 0 is an MDIC error there): it fails, and
/// leaves nothing claimed: bus master off, DRV_LOAD off, semaphore given back; the card waits for a retry.
#[test]
fn failed_bring_up_leaves_nothing_claimed() {
    let c = card_on(Fake::new(Model::I82576), Model::I226, small());
    let e = c.start().unwrap_err();
    assert_eq!(e, BringUpError::Phy(PhyError::MdicError));
    let f = c.hw();
    assert_eq!(f.pci() & cmd::BUS_MASTER, 0);
    assert_eq!(f.peek(regs::CTRL_EXT) & regs::CTRL_EXT_DRV_LOAD, 0);
    assert_eq!(f.peek(regs::SW_FW_SYNC) & regs::SWFW_PHY0_SW, 0);
    assert_eq!(f.peek(regs::SWSM), 0);
    assert!(!f.trace().contains(&Ev::EnableDma));
    assert_eq!(c.state(), State::Failed);
    let (n, at) = c.retry_info();
    assert_eq!(n, 1);
    assert_eq!(at, f.now() + device::RETRY_SOON_NS);
}

#[test]
fn no_mac_refuses_the_card() {
    let f = Fake::new(Model::I226);
    f.poke(regs::RAH, 0x0000_1234);
    let c = card_on(f, Model::I226, small());
    let e = c.start().unwrap_err();
    assert!(e.fatal() && e == BringUpError::NoMac);
    assert_eq!(c.state(), State::Down);
    assert_eq!(c.hw().pci() & cmd::BUS_MASTER, 0);
}

#[test]
fn card_reading_all_ones_is_gone() {
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.all_ones = true);
    let c = card_on(f, Model::I226, small());
    assert_eq!(c.start().unwrap_err(), BringUpError::Gone);
    assert!(!c.link().up);
}

#[test]
fn d3hot_card_is_woken_and_lost_bars_are_restored() {
    let f = Fake::new(Model::I226);
    f.knobs(|k| {
        k.d3hot = true;
        k.all_ones = true;
        k.bars_lost = true;
    });
    let c = card_on(f, Model::I226, small());
    c.start().expect("woken, BARs back, up");
    assert!(klog_has(&c, "igc.config_restored"));
    // BARs first, then `enable_device` wakes the card (PC-2 does the D3hot -> D0 and its 10 ms).
    let t = c.hw().trace();
    assert!(pos(&t, |e| *e == Ev::BarsRestored) < pos(&t, |e| *e == Ev::Woke), "{t:x?}");
}

/// PC-2's `enable_device` refusing the card (R8d/R8a) is final; a failure (power state stuck, busy) is retried.
/// Neither leaves bus master on.
#[test]
fn enable_device_refused_is_final_and_failed_is_retried() {
    for refused in [true, false] {
        let f = Fake::new(Model::I226);
        f.knobs(|k| k.enable_fails = Some(PciFail { refused, text: "enable_device(false): test".into() }));
        let c = card_on(f, Model::I226, small());
        let e = c.start().unwrap_err();
        assert!(matches!(&e, BringUpError::Pci(p) if p.refused == refused), "{e:?}");
        assert_eq!(e.fatal(), refused);
        assert_eq!(c.hw().pci() & cmd::BUS_MASTER, 0);
        assert!(!c.hw().trace().contains(&Ev::EnableDma));
        assert!(!c.link().up);
    }
}

#[test]
fn reset_goes_on_when_gio_master_never_clears_and_mdic_dead_fails_bounded() {
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.gio_stuck = true);
    let c = card_on(f, Model::I226, small());
    c.start().expect("Linux goes on after the GIO wait too");
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.mdic_dead = true);
    let t0 = f.now();
    let c = card_on(f, Model::I226, small());
    assert_eq!(c.start().unwrap_err(), BringUpError::Phy(PhyError::MdicTimeout));
    assert!(c.hw().now() - t0 < 2_000 * MS, "bounded");
}

#[test]
fn queue_that_never_enables_is_an_error_not_a_hang() {
    let f = Fake::new(Model::I82576);
    f.knobs(|k| k.queue_wont_enable = true);
    let c = card_on(f, Model::I82576, small());
    assert!(matches!(c.start().unwrap_err(), BringUpError::Ring("RX", _)));
    assert_eq!(c.hw().pci() & cmd::BUS_MASTER, 0, "bus master off again after the failure");
}

// ---------------------------------------------------------------------------------------------------------
// PHY semaphore (igb-test's I226 paths)

fn sem(f: &Fake) -> (u32, u32) {
    (f.peek(regs::SW_FW_SYNC), f.peek(regs::SWSM) & 3)
}

#[test]
fn phy_semaphore_free_is_taken_at_once_and_given_back() {
    let f = Fake::new(Model::I226);
    let t0 = f.now();
    let a = phy::acquire(&f).unwrap();
    assert_eq!(a, phy::Acquired::default());
    assert!(f.now() - t0 < 500 * MS);
    assert_eq!(sem(&f), (2, 0));
    phy::release(&f).unwrap();
    assert_eq!(sem(&f), (0, 0));
}

#[test]
fn phy_semaphore_held_by_firmware_fails_after_a_second_untouched() {
    let f = Fake::new(Model::I226);
    f.poke(regs::SW_FW_SYNC, regs::SWFW_PHY0_FW);
    f.knobs(|k| k.fw_releases_phy_at = u64::MAX);
    let t0 = f.now();
    assert_eq!(phy::acquire(&f), Err(PhyError::FirmwareHoldsPhy));
    let ms = (f.now() - t0) / MS;
    assert!((1000..3000).contains(&ms), "{ms} ms");
    assert_eq!(sem(&f), (regs::SWFW_PHY0_FW, 0), "its bit untouched, SWSM free");
}

#[test]
fn phy_semaphore_firmware_lets_go() {
    let f = Fake::new(Model::I226);
    f.poke(regs::SW_FW_SYNC, regs::SWFW_PHY0_FW);
    f.knobs(|k| k.fw_releases_phy_at = f.now() + 300 * MS);
    let a = phy::acquire(&f).unwrap();
    assert!(a.waited_for_firmware);
    assert_eq!(sem(&f), (2, 0));
}

#[test]
fn smbi_left_set_is_cleared_once_after_a_second() {
    let f = Fake::new(Model::I226);
    f.poke(regs::SWSM, regs::swsm::SMBI);
    let t0 = f.now();
    let a = phy::acquire(&f).unwrap();
    let ms = (f.now() - t0) / MS;
    assert!(a.smbi_cleared && (1000..3000).contains(&ms), "{a:?} {ms} ms");
    assert_eq!(sem(&f), (2, 0));
}

#[test]
fn smbi_stuck_for_good_and_swesmbi_held_fail_bounded() {
    let f = Fake::new(Model::I226);
    f.poke(regs::SWSM, regs::swsm::SMBI);
    f.knobs(|k| k.smbi_locked = true);
    let t0 = f.now();
    assert_eq!(phy::acquire(&f), Err(PhyError::SmbiStuck));
    assert!(f.now() - t0 <= 3_100 * MS);
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.fw_holds_swesmbi_until = u64::MAX);
    assert_eq!(phy::acquire(&f), Err(PhyError::SwesmbiHeld));
    assert_eq!(f.peek(regs::SWSM) & 3, 0, "SMBI given back too");
}

/// A probe cut short (Ctrl+C in V1, a crash in V2) leaves our own PHY0 bit set: taken over, not waited for.
#[test]
fn own_stale_bit_is_taken_over() {
    let f = Fake::new(Model::I226);
    f.poke(regs::SW_FW_SYNC, regs::SWFW_PHY0_SW);
    let t0 = f.now();
    let a = phy::acquire(&f).unwrap();
    assert!(a.took_over_own_bit && !a.waited_for_firmware);
    assert!(f.now() - t0 < 500 * MS);
    assert_eq!(sem(&f), (2, 0));
}

#[test]
fn phy_reset_skipped_while_manc_blocks_it() {
    let f = Fake::new(Model::I226);
    f.poke(regs::MANC, regs::MANC_BLK_PHY_RST);
    assert_eq!(phy::reset(&f), phy::PhyReset::Blocked);
    assert!(!f.trace().contains(&Ev::PhyReset));
    f.poke(regs::MANC, 0);
    assert_eq!(phy::reset(&f), phy::PhyReset::Done);
    assert_eq!(f.trace().iter().filter(|e| **e == Ev::PhyReset).count(), 1);
}

// ---------------------------------------------------------------------------------------------------------
// Rings

/// nic-test/igb-test: transmit stalled (as after a link loss on the I226): 31 frames queued, the 32nd refused
/// (a full ring must not look empty), all sent once it runs again, in order.
#[test]
fn tx_stalled_31_queued_32nd_refused_all_sent_on_resume() {
    let c = running(Model::I226);
    let f = c.hw();
    f.knobs(|k| k.tx_stalled = true);
    for i in 0..31 {
        c.transmit(&frame(i, 60)).unwrap_or_else(|e| panic!("frame {i}: {e:?}"));
    }
    assert_eq!(c.transmit(&frame(31, 60)), Err(DevError::Timeout), "32nd refused");
    assert_eq!(f.peek(regs::TDT), 31);
    assert_eq!(f.run_tx(), 0, "stalled");
    f.knobs(|k| k.tx_stalled = false);
    assert_eq!(f.run_tx(), 31);
    let sent = f.sent();
    for (i, s) in sent.iter().enumerate() {
        assert_eq!(s, &frame(i as u32, 60), "frame {i} intact and in order");
    }
    for i in 31..200 {
        c.transmit(&frame(i, 60 + i as usize)).unwrap();
        f.run_tx();
    }
    assert_eq!(f.sent().len(), 169, "the ring wraps many times");
    assert_eq!(c.stats().tx_frames, 200);
}

#[test]
fn short_frame_padded_to_17_and_bounds_refused() {
    let c = running(Model::I82576);
    let f = c.hw();
    c.transmit(&frame(1, 14)).unwrap();
    f.run_tx();
    let s = f.sent();
    assert_eq!(s[0].len(), 17);
    assert_eq!(&s[0][..14], &frame(1, 14)[..]);
    assert_eq!(&s[0][14..], &[0, 0, 0]);
    assert_eq!(c.transmit(&[0u8; 13]), Err(DevError::InvalidArgument));
    assert_eq!(c.transmit(&alloc::vec![0u8; 1519]), Err(DevError::InvalidArgument));
    c.transmit(&alloc::vec![0u8; 1518]).unwrap();
}

/// The write-back overwrites the buffer address: hundreds of frames through a ring of 32, each intact.
#[test]
fn rx_through_many_wraps_with_address_overwritten() {
    let c = running(Model::I226);
    let f = c.hw();
    let mut buf = [0u8; 2048];
    for i in 0..300u32 {
        let fr = frame(i, 60 + (i as usize * 13) % 1450);
        assert!(f.deliver(&fr), "frame {i} taken by the card");
        assert_eq!(c.receive(&mut buf), Some(fr.len()));
        assert_eq!(&buf[..fr.len()], &fr[..], "frame {i}");
    }
    assert_eq!(c.receive(&mut buf), None);
    assert_eq!(c.stats().rx_frames, 300);
}

/// igb-test boot 3: more frames than the ring holds while nobody receives: the card fills all but the one held
/// back, then every frame comes out in order and the card goes on.
#[test]
fn rx_ring_full_holds_one_back_then_drains_in_order() {
    let c = running(Model::I82576);
    let f = c.hw();
    let mut n = 0;
    while f.deliver(&frame(n, 64)) {
        n += 1;
        assert!(n < 100);
    }
    assert_eq!(n, 31, "a ring of 32 takes 31");
    let mut buf = [0u8; 2048];
    for i in 0..n {
        assert_eq!(c.receive(&mut buf), Some(64));
        assert_eq!(&buf[14..18], &i.to_be_bytes());
    }
    assert_eq!(c.receive(&mut buf), None);
    for i in 0..40 {
        assert!(f.deliver(&frame(100 + i, 64)), "the card works on after a full ring");
        assert_eq!(c.receive(&mut buf), Some(64));
    }
}

/// Written-back descriptors with bad news are dropped and given back; the next good one is taken. Hostile
/// lengths never panic.
#[test]
fn bad_rx_descriptors_dropped_next_good_taken() {
    let c = running(Model::I226);
    let f = c.hw();
    let good = frame(7, 60);
    assert!(f.deliver_with(&good, Some(0), None), "no end of packet");
    assert!(f.deliver_with(&good, Some(regs::RX_EOP | regs::RX_RXE), None), "frame error");
    assert!(f.deliver_with(&good, None, Some(1519)), "too long");
    for len in [0u16, 13, 2048, 65535] {
        assert!(f.deliver_with(&good, None, Some(len)));
    }
    assert!(f.deliver(&good));
    let mut buf = [0u8; 2048];
    assert_eq!(c.receive(&mut buf), Some(60));
    assert_eq!(&buf[..60], &good[..]);
    assert_eq!(c.stats().rx_dropped, 7);
    // A frame larger than the caller's buffer is dropped and counted, nothing breaks.
    assert!(f.deliver(&frame(8, 200)));
    let mut tiny = [0u8; 100];
    assert_eq!(c.receive(&mut tiny), None);
    assert_eq!(c.stats().rx_dropped, 8);
    assert!(f.deliver(&frame(9, 90)));
    assert_eq!(c.receive(&mut tiny), Some(90));
}

#[test]
fn rdt_writes_are_batched_and_flushed() {
    let c = running(Model::I226);
    let f = c.hw();
    let mut buf = [0u8; 2048];
    f.clear_trace();
    for i in 0..5 {
        assert!(f.deliver(&frame(i, 60)));
    }
    for _ in 0..5 {
        assert!(c.receive(&mut buf).is_some());
    }
    assert!(!f.trace().iter().any(wr(regs::RDT)), "five frames, no doorbell yet");
    assert_eq!(c.receive(&mut buf), None);
    assert_eq!(f.peek(regs::RDT), 4, "flushed when the ring ran empty: the last given back");
}

// ---------------------------------------------------------------------------------------------------------
// poll_tx, MSI preparation

#[test]
fn poll_tx_never_waits_for_a_held_lock() {
    let c = running(Model::I226);
    let fr = frame(1, 60);
    assert!(!c.with_tx_locked(|| c.poll_tx(&fr)), "dropped, not waited");
    assert!(c.poll_tx(&fr));
    c.hw().run_tx();
    assert_eq!(c.hw().sent().len(), 1);
    assert!(!c.poll_tx(&[0u8; 5]));
}

/// MSI (one message, V1's measured rules): the bring-up programs IVAR0/EIMS/GPIE/EITR0/IMS; an interrupt makes
/// the next `receive` acknowledge (ICR read, EICR = ~0) before it looks; a restart programs it again.
#[test]
fn msi_programs_the_eicr_rules_and_receive_acks_both() {
    for model in [Model::I226, Model::I82576] {
        let f = Fake::new(model);
        f.knobs(|k| k.irq = IrqSetup::Msi);
        let c = card_on(f, model, small());
        c.start().unwrap();
        let f = c.hw();
        assert_eq!(c.irq_setup(), "msi");
        assert_eq!(f.peek(regs::IVAR0), regs::IVAR0_Q0_TO_0);
        assert_eq!(f.peek(regs::EIMS), regs::EIMS_Q0_OTHER);
        assert_eq!(f.peek(regs::GPIE) & regs::GPIE_NSICR, regs::GPIE_NSICR);
        assert_eq!(f.peek(regs::EITR0), regs::eitr_interval(regs::ITR_US_DEFAULT) | regs::EITR_CNT_IGNR);
        assert_eq!(f.peek(model.ims()), regs::IMS_CAUSES, "{model:?} IMS offset");
        f.poke(regs::EICR, 1);
        f.poke(model.icr(), 0x9D);
        let mut buf = [0u8; 2048];
        // No interrupt noted: nothing is acknowledged (no MMIO per frame).
        assert_eq!(c.receive(&mut buf), None);
        assert_eq!(f.peek(regs::EICR), 1, "untouched without an interrupt");
        assert!(f.fire(0));
        assert_eq!(c.receive(&mut buf), None);
        assert_eq!(f.peek(regs::EICR), 0, "EICR cleared before the look");
        assert_eq!(f.peek(model.icr()), 0, "ICR read");
        // A restart programs it again (the PCI core gives a new set).
        f.poke(regs::EIMS, 0);
        c.restart("test").unwrap();
        assert_eq!(f.peek(regs::EIMS), regs::EIMS_Q0_OTHER);
        assert_eq!(f.irq_setups.load(core::sync::atomic::Ordering::Relaxed), 2);
    }
}

/// MSI-X, two vectors (Linux igc's layout): link causes on vector 0 through IVAR_MISC, queue 0 on vector 1
/// through IVAR0, both auto-cleared and auto-masked; the queue vector is re-armed (EIMS) when the ring is empty,
/// the other vector after ICR was read; a link change found by that read is looked at at once. Health sends a
/// software interrupt (EICS) once a second, as Linux's watchdog does.
#[test]
fn msix_two_vectors_rearm_after_the_look() {
    for model in [Model::I226, Model::I82576] {
        let f = Fake::new(model);
        f.knobs(|k| k.irq = IrqSetup::MsiX(2));
        let c = card_on(f, model, small());
        c.start().unwrap();
        settle(&c);
        let f = c.hw();
        assert_eq!(c.irq_setup(), "msix");
        let g = f.peek(regs::GPIE);
        assert_eq!(g, regs::GPIE_MSIX_MODE | regs::GPIE_PBA | regs::GPIE_EIAME | regs::GPIE_NSICR);
        assert_eq!(f.peek(regs::IVAR_MISC), (0 | regs::IVAR_VALID) << 8, "other causes -> vector 0");
        assert_eq!(f.peek(regs::IVAR0), 0x8181, "queue 0 RX and TX -> vector 1");
        assert_eq!(f.peek(regs::eitr(1)), regs::eitr_interval(regs::ITR_US_DEFAULT) | regs::EITR_CNT_IGNR);
        assert_eq!((f.peek(regs::EIAC), f.peek(regs::EIAM), f.peek(regs::EIMS)), (3, 3, 3));
        assert_eq!(f.peek(model.ims()), regs::ICR_LSC);
        // The queue vector fired and masked itself (EIAM): the look re-arms it once the ring is empty.
        f.poke(regs::EIMS, 1);
        assert!(f.fire(1));
        let mut buf = [0u8; 2048];
        assert!(f.deliver(&frame(9, 60)));
        assert_eq!(c.receive(&mut buf), Some(60));
        assert_eq!(f.peek(regs::EIMS) & 2, 0, "still masked while frames come");
        assert_eq!(c.receive(&mut buf), None);
        assert_eq!(f.peek(regs::EIMS), 2, "re-armed when empty (the fake's EIMS is a plain register)");
        // The other vector: ICR read (clears it), link looked at, vector 0 re-armed.
        f.poke(regs::EIMS, 0);
        f.poke(model.icr(), regs::ICR_LSC);
        assert!(f.fire(0));
        assert_eq!(c.receive(&mut buf), None);
        assert_eq!(f.peek(model.icr()), 0, "ICR read");
        assert_eq!(f.peek(regs::EIMS), 2, "the last write re-armed the queue; vector 0 was written before it");
        assert!(f.trace().contains(&Ev::Wr(regs::EIMS, 1)), "vector 0 re-armed");
        // Watchdog's software interrupt.
        f.advance(device::HEALTH_EVERY_NS + 1);
        c.poll();
        assert!(f.trace().contains(&Ev::Wr(regs::EICS, 3)), "EICS on both vectors");
    }
}

/// One MSI-X vector: both cause kinds on vector 0, ICR read with every look, re-armed when the ring is empty.
/// No vectors at all, or `irqs: false`: polled with every cause masked; the stack is told which (net::netdev).
#[test]
fn one_vector_and_polled_fallbacks() {
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.irq = IrqSetup::MsiX(1));
    let c = card_on(f, Model::I226, small());
    c.start().unwrap();
    let f = c.hw();
    assert_eq!((f.peek(regs::IVAR_MISC), f.peek(regs::IVAR0)), (0x80 << 8, 0x8080));
    assert_eq!(f.peek(regs::EIMS), 1);
    f.poke(regs::EIMS, 0);
    f.poke(Model::I226.icr(), regs::ICR_LSC);
    assert!(f.fire(0));
    let mut buf = [0u8; 2048];
    assert_eq!(c.receive(&mut buf), None);
    assert_eq!(f.peek(Model::I226.icr()), 0);
    assert_eq!(f.peek(regs::EIMS), 1);
    c.report_irq();
    assert!(net::netdev::is_irq_driven(c.name()));
    f.knobs(|k| k.irq_degraded = true);
    c.report_irq();
    assert!(!net::netdev::is_irq_driven(c.name()), "a stormed or released set: the stack polls");

    let f = Fake::new(Model::I226);
    f.knobs(|k| k.irq = IrqSetup::MsiX(2));
    let c = card_on(f, Model::I226, Config { irqs: false, ..small() });
    c.start().unwrap();
    assert_eq!(c.irq_setup(), "polled");
    assert_eq!(c.hw().irq_setups.load(core::sync::atomic::Ordering::Relaxed), 0, "not even asked");
    assert_eq!(c.hw().peek(Model::I226.imc()), !0, "every cause masked");
    assert!(!c.hw().fire(0));
}

// ---------------------------------------------------------------------------------------------------------
// Link watch and restart rules

/// nic-test boots 1, 7: two cards, the first without link. Both run; the first refuses frames until its cable
/// comes, the second carries traffic meanwhile.
#[test]
fn two_cards_first_without_link() {
    let f0 = Fake::new(Model::I82576);
    f0.knobs(|k| k.cable = false);
    let c0 = card_on(f0, Model::I82576, small());
    let c1 = card(Model::I82576);
    c0.start().unwrap();
    c1.start().unwrap();
    settle(&c0);
    settle(&c1);
    assert!(!c0.link().up && c1.link().up);
    assert_eq!(c0.state(), State::Running, "a card without link still runs (link watch on all cards)");
    assert_eq!(c0.transmit(&frame(1, 60)), Err(DevError::Io), "no frame to a card without link");
    assert!(!c0.poll_tx(&frame(1, 60)));
    c1.transmit(&frame(1, 60)).unwrap();
    assert_eq!(c1.hw().run_tx(), 1);
    c0.hw().set_cable(true);
    c0.poll();
    assert!(!c0.link().up, "autonegotiation takes its time");
    settle(&c0);
    assert!(c0.link().up);
    c0.transmit(&frame(2, 60)).unwrap();
    assert_eq!(c0.hw().run_tx(), 1);
}

/// netwatch-test: the link goes with frames queued; the I226 never sends them until restarted, so a second
/// look with the head still is a restart (Linux `igc_watchdog_task`).
#[test]
fn frames_stuck_while_link_down_restart_the_card() {
    let c = running(Model::I226);
    let f = c.hw();
    for i in 0..3 {
        c.transmit(&frame(i, 60)).unwrap();
    }
    f.set_cable(false);
    f.advance(device::LINK_EVERY_NS);
    c.poll();
    assert!(!c.link().up);
    assert!(klog_has(&c, "link.down"));
    f.clear_trace();
    f.advance(device::HEALTH_EVERY_NS);
    c.poll();
    assert!(!f.trace().contains(&Ev::Reset), "first look only notes the head");
    f.advance(device::HEALTH_EVERY_NS);
    c.poll();
    assert!(f.trace().contains(&Ev::Reset), "second look: restarted");
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.ring_info().2, 0, "no frame survives the restart");
    f.set_cable(true);
    settle(&c);
    assert!(c.link().up);
}

/// Three sends in a row found no free descriptor within 100 ms while the link is up: restart.
#[test]
fn three_late_sends_restart_the_card() {
    let c = running(Model::I82576);
    let f = c.hw();
    f.knobs(|k| k.tx_stalled = true);
    for i in 0..31 {
        c.transmit(&frame(i, 60)).unwrap();
    }
    assert_eq!(c.transmit(&frame(99, 60)), Err(DevError::Timeout));
    assert_eq!(c.tx_late(), 0, "full, but not yet late");
    f.advance(device::TX_LATE_NS);
    for _ in 0..3 {
        assert_eq!(c.transmit(&frame(99, 60)), Err(DevError::Timeout));
    }
    assert_eq!(c.tx_late(), 3);
    f.clear_trace();
    f.advance(device::HEALTH_EVERY_NS);
    c.poll();
    assert!(f.trace().contains(&Ev::Reset), "restarted");
    assert!(klog_has(&c, "igc.restart"));
    assert_eq!(c.tx_late(), 0);
}

/// Registers all ones (I225/I226 dropping off the bus): restart; while it does not come back, retries every
/// 10 s six times, then every 60 s; back to normal once it answers again.
#[test]
fn card_gone_is_retried_10s_six_times_then_60s() {
    let c = running(Model::I226);
    let f = c.hw();
    f.knobs(|k| k.all_ones = true);
    f.advance(device::HEALTH_EVERY_NS);
    c.poll();
    assert_eq!(c.state(), State::Failed);
    assert!(!c.link().up && c.transmit(&frame(0, 60)).is_err());
    let mut delays = Vec::new();
    for _ in 0..8 {
        let (_, at) = c.retry_info();
        delays.push((at - f.now()) / 1_000_000_000);
        f.advance(at - f.now() - 1);
        c.poll();
        assert_eq!(c.retry_info().1, at, "not before its time");
        f.advance(1);
        c.poll();
        assert_eq!(c.state(), State::Failed);
    }
    assert_eq!(delays, [10, 10, 10, 10, 10, 10, 60, 60]);
    f.knobs(|k| k.all_ones = false);
    let (_, at) = c.retry_info();
    f.advance(at - f.now());
    c.poll();
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.retry_info().0, 0);
}

#[test]
fn hardware_missed_counter_is_collected() {
    let c = running(Model::I226);
    c.hw().knobs(|k| k.missed = 5);
    c.hw().advance(device::HEALTH_EVERY_NS);
    c.poll();
    assert_eq!(c.hw_stats().missed, 5);
    assert_eq!(c.stats().rx_dropped, 5);
}

// ---------------------------------------------------------------------------------------------------------
// Quiesce

/// Quiesce before Reload: receive off first, the TX ring drained (the mirror's last lines), transmit off, the
/// reset, DRV_LOAD off, MSI off before bus master off. Then nothing moves until an explicit restart.
#[test]
fn quiesce_order_and_drain() {
    let c = running(Model::I226);
    let f = c.hw();
    f.set_msi(true, false);
    f.knobs(|k| k.tx_autorun = true);
    for i in 0..5 {
        c.transmit(&frame(i, 100)).unwrap();
    }
    f.clear_trace();
    c.quiesce();
    let t = f.trace();
    assert_eq!(t[0], Ev::Wr(regs::RCTL, 0), "receive off first");
    assert_eq!(f.sent().len(), 5, "drained before transmit went off");
    let tctl_off = pos(&t, |e| matches!(e, Ev::Wr(regs::TCTL, v) if v & regs::TCTL_EN == 0));
    let reset_at = pos(&t, |e| *e == Ev::Reset);
    let drv = pos(&t, |e| matches!(e, Ev::Wr(regs::CTRL_EXT, v) if v & regs::CTRL_EXT_DRV_LOAD == 0));
    let msi = pos(&t, |e| *e == Ev::MsiOff);
    let bme = pos(&t, |e| matches!(e, Ev::Pci(v) if v & cmd::BUS_MASTER == 0));
    assert!(tctl_off < reset_at && reset_at < drv && drv < msi && msi < bme, "{t:x?}");
    assert_eq!(f.msi(), (false, false));
    assert_eq!(c.state(), State::Quiesced);
    assert!(c.transmit(&frame(9, 60)).is_err() && !c.poll_tx(&frame(9, 60)));
    let mut buf = [0u8; 2048];
    assert_eq!(c.receive(&mut buf), None);
    c.fail_for_test();
    f.advance(100 * device::HEALTH_EVERY_NS);
    c.poll();
    assert_eq!(c.state(), State::Quiesced, "neither a failure nor the poller restarts a quiesced card");
    assert!(klog_has(&c, "igc.quiesce"));
    c.restart("test").unwrap();
    settle(&c);
    assert!(c.link().up);
    c.transmit(&frame(10, 60)).unwrap();
}

/// Quiesce with the link gone does not wait for a drain that cannot happen.
#[test]
fn quiesce_without_link_does_not_wait() {
    let c = running(Model::I82576);
    let f = c.hw();
    f.knobs(|k| k.tx_stalled = true);
    c.transmit(&frame(0, 60)).unwrap();
    f.set_cable(false);
    f.advance(device::LINK_EVERY_NS);
    c.poll();
    let t0 = f.now();
    c.quiesce();
    assert!(f.now() - t0 < 200 * MS, "reset waits only");
}

// ---------------------------------------------------------------------------------------------------------
// Pure helpers

#[test]
fn link_from_status_decodes() {
    // The Mini-PC's I226 at 1 Gb/s full duplex (HARDWARE-MINIPC).
    assert_eq!(reset::link_from_status(Model::I226, 0x0028_0683), (true, 1000, true));
    assert_eq!(reset::link_from_status(Model::I226, 0x0068_0683), (true, 2500, true));
    assert_eq!(reset::link_from_status(Model::I82576, 0x0068_0683), (true, 1000, true));
    assert_eq!(reset::link_from_status(Model::I226, 0x43), (true, 100, true));
    assert_eq!(reset::link_from_status(Model::I226, 0x02), (true, 10, false));
    assert_eq!(reset::link_from_status(Model::I226, 0x0028_0681), (false, 0, false));
    assert_eq!(reset::link_from_status(Model::I226, !0), (false, 0, false), "all ones = gone, no link");
}

#[test]
fn mac_needs_the_valid_bit_and_a_unicast_address() {
    let f = Fake::new(Model::I226);
    assert_eq!(reset::read_mac(&f), Some(MAC));
    f.poke(regs::RAH, 0x0000_0834);
    assert_eq!(reset::read_mac(&f), None, "no address-valid bit");
    f.poke(regs::RAH, 1 << 31);
    f.poke(regs::RAL, 0);
    assert_eq!(reset::read_mac(&f), None, "all zeros");
    f.poke(regs::RAL, 0x0000_0001);
    assert_eq!(reset::read_mac(&f), None, "multicast");
}

#[test]
fn ring_sizes_are_checked() {
    let f = Fake::new(Model::I226);
    assert!(Card::new(String::from("x"), Model::I226, f, Config { rx_ring: 30, tx_ring: 32, ..Config::default() }).is_err());
    let c = card_on(Fake::new(Model::I226), Model::I226, Config::default());
    let (rx, tx, _, cap) = c.ring_info();
    assert_eq!((rx, tx, cap), (1024, 256, 255), "NW-7: 1024 RX descriptors by default");
}

/// NW-7: while frames keep coming (no empty look), taken descriptors go back to the card every
/// `RX_WRITE_EVERY` frames (Linux `IGC_RX_BUFFER_WRITE`), not only after a quarter of the ring: the card sees
/// its free descriptors early in a burst.
#[test]
fn rx_descriptors_go_back_every_16_frames() {
    let c = card_on(Fake::new(Model::I226), Model::I226, Config::default());
    c.start().expect("bring-up");
    settle(&c);
    let f = c.hw();
    let tail0 = f.peek(regs::RDT);
    for i in 0..40u32 {
        assert!(f.deliver(&frame(i, 64)));
    }
    let mut buf = [0u8; 2048];
    for _ in 0..crate::device::RX_WRITE_EVERY - 1 {
        assert_eq!(c.receive(&mut buf), Some(64));
    }
    assert_eq!(f.peek(regs::RDT), tail0, "not yet");
    assert_eq!(c.receive(&mut buf), Some(64));
    let t = f.peek(regs::RDT);
    assert_eq!((t as usize + 1024 - tail0 as usize) % 1024, crate::device::RX_WRITE_EVERY, "16 given back at once");
}

/// An event of this card (by its `ifname`) with this name is in the klog ring.
fn klog_has(c: &Card<Fake>, name: &str) -> bool {
    let me = alloc::format!("ifname={} ", c.name());
    klog::tail(1 << 20).iter().any(|r| r.name == name && alloc::format!("{} ", r.text).contains(&me))
}

// ---------------------------------------------------------------------------------------------------------
// Checker round (NW-2 fixes)

/// Lost BARs are written back before memory decode is switched on (spec §2.1/§2.3 "restore BARs first"): with
/// BAR0 at 0 and decode on, the card would answer at address 0.
#[test]
fn bars_restored_before_memory_decode_goes_on() {
    let f = Fake::new(Model::I226);
    f.set_pci(cmd::BUS_MASTER);
    f.knobs(|k| {
        k.all_ones = true;
        k.bars_lost = true;
    });
    let c = card_on(f, Model::I226, small());
    c.start().expect("BARs back, up");
    let t = c.hw().trace();
    assert!(matches!(t[1], Ev::Pci(v) if v & (cmd::BUS_MASTER | cmd::MEMORY) == 0), "bus master off, decode not yet on: {:x?}", t[1]);
    let bars = pos(&t, |e| *e == Ev::BarsRestored);
    let mem_on = pos(&t, |e| matches!(e, Ev::Pci(v) if v & cmd::MEMORY != 0));
    assert!(bars < mem_on, "{t:x?}");
}

/// With a restart worker the poller only queues a restart (it may take seconds and the poller shares its
/// thread with every driver); the worker runs it with sleeping waits. A Quiesce in between cancels it, and
/// Quiesce itself never sleeps.
#[test]
fn restart_worker_runs_queued_restarts_with_sleeping_waits() {
    let c = running(Model::I226);
    c.set_restart_worker(true);
    let f = c.hw();
    f.knobs(|k| k.all_ones = true);
    f.advance(device::HEALTH_EVERY_NS);
    f.clear_trace();
    c.poll();
    assert!(c.restart_due(), "queued");
    assert_eq!(c.state(), State::Running, "not restarted by the poller");
    assert!(!f.trace().contains(&Ev::Reset));
    f.knobs(|k| k.all_ones = false);
    let p0 = f.pauses();
    c.run_due_restart();
    assert!(!c.restart_due());
    assert_eq!(c.state(), State::Running);
    assert!(f.trace().contains(&Ev::Reset), "the worker restarted it");
    assert!(f.pauses() > p0, "the worker's waits sleep");
    settle(&c);
    assert!(c.link().up);
    // Queued, then quiesced: the worker leaves the card alone.
    f.knobs(|k| k.all_ones = true);
    f.advance(device::HEALTH_EVERY_NS);
    c.poll();
    assert!(c.restart_due());
    f.knobs(|k| k.all_ones = false);
    let p1 = f.pauses();
    c.quiesce();
    assert_eq!(f.pauses(), p1, "Quiesce spins, never sleeps (panic path)");
    f.clear_trace();
    c.run_due_restart();
    assert_eq!(c.state(), State::Quiesced);
    assert!(!f.trace().contains(&Ev::Reset));
    // An explicit restart supersedes a queued one.
    c.restart("test").unwrap();
    assert!(!c.restart_due());
}

/// A data-path failure stops the card at once: no DMA into rings the driver considers broken during the
/// second before the retry.
#[test]
fn failure_stops_the_card_at_once() {
    let c = running(Model::I82576);
    let f = c.hw();
    f.clear_trace();
    c.fail_for_test();
    assert_eq!(c.state(), State::Failed);
    let t = f.trace();
    assert!(t.contains(&Ev::Reset), "{t:x?}");
    assert_eq!(f.pci() & cmd::BUS_MASTER, 0);
    assert_eq!(f.peek(regs::CTRL_EXT) & regs::CTRL_EXT_DRV_LOAD, 0);
    assert!(!f.deliver(&frame(1, 60)), "receives nothing");
    f.advance(device::FAIL_RETRY_NS);
    c.poll();
    assert_eq!(c.state(), State::Running);
}

/// No MAC refuses a card only at probe; after a restart (a half-read RAL/RAH) it is retried with back-off.
#[test]
fn no_mac_on_a_restart_is_retried_not_refused() {
    let c = running(Model::I226);
    let f = c.hw();
    f.poke(regs::RAH, 0);
    c.fail_for_test();
    f.advance(device::FAIL_RETRY_NS);
    c.poll();
    assert_eq!(c.state(), State::Failed, "kept for a retry");
    assert_eq!(c.retry_info().0, 1);
    assert!(!klog_has(&c, "igc.refused"));
    f.poke(regs::RAH, (1 << 31) | u16::from_le_bytes([MAC[4], MAC[5]]) as u32);
    let (_, at) = c.retry_info();
    f.advance(at - f.now());
    c.poll();
    assert_eq!(c.state(), State::Running);
}

/// The I226 restart rotates CTRL.DEV_RST (Linux igc's reset bit) in from the fourth failed attempt on; RST
/// stays the default and stays in the rotation. Quiesce always uses RST.
#[test]
fn i226_restart_tries_dev_rst_after_failed_restarts() {
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.mdic_dead = true);
    let c = card_on(f, Model::I226, small());
    let f = c.hw();
    let first_bit = |t: &[Ev]| t.iter().find_map(|e| match e { Ev::ResetBit(b) => Some(*b), _ => None }).expect("a reset");
    assert!(c.start().is_err());
    let mut bits = alloc::vec![first_bit(&f.trace())];
    for _ in 0..5 {
        let (_, at) = c.retry_info();
        f.advance(at - f.now());
        f.clear_trace();
        c.poll();
        assert_eq!(c.state(), State::Failed);
        bits.push(first_bit(&f.trace()));
    }
    let (rst, dev) = (regs::ctrl::RST, regs::ctrl::DEV_RST);
    assert_eq!(bits, [rst, rst, rst, dev, rst, dev]);
    assert!(klog_has(&c, "igc.reset_fallback"));
    f.knobs(|k| k.mdic_dead = false);
    let (_, at) = c.retry_info();
    f.advance(at - f.now());
    c.poll();
    assert_eq!(c.state(), State::Running);
    f.clear_trace();
    c.quiesce();
    assert_eq!(first_bit(&f.trace()), rst, "Quiesce uses RST");
}

/// A failed MMD access still sets the window back to clause-22 registers, so ANAR/BMCR afterwards are not aimed
/// at an MMD register; the bring-up goes on with the PHY's own advertisement.
#[test]
fn mmd_window_is_set_back_after_a_failed_access() {
    let f = Fake::new(Model::I226);
    f.knobs(|k| k.mmd_data_error = true);
    assert_eq!(phy::advertise(&f, Model::I226), Err(PhyError::MdicError));
    assert_eq!(f.mmd_window(), (0, false));
    let c = card_on(f, Model::I226, small());
    c.start().expect("up with the PHY's own advertisement");
    assert!(klog_has(&c, "igc.advertise_failed"));
    assert_ne!(c.hw().phy_reg(regs::phy::BMCR) & regs::phy::BMCR_AN_ENABLE, 0);
    settle(&c);
    assert!(c.link().up);
}

/// PHY0 that cannot be given back (the firmware took SWSM meanwhile) is reported, not forced clear without
/// SWSM; the next acquire takes our stale bit over.
#[test]
fn phy0_not_given_back_is_reported_and_taken_over_next_time() {
    let f = Fake::new(Model::I226);
    let at = f.now() + MS;
    f.knobs(|k| k.fw_takes_swesmbi_at = at);
    let r = phy::up(&f, Model::I226).expect("PHY up");
    assert_eq!(r.release_error, Some(PhyError::SwesmbiHeld));
    assert_ne!(f.peek(regs::SW_FW_SYNC) & regs::SWFW_PHY0_SW, 0, "our bit stays set");
    assert_eq!(f.peek(regs::SWSM), 0, "SWSM itself given back");
    f.knobs(|k| k.fw_takes_swesmbi_at = u64::MAX);
    let a = phy::acquire(&f).unwrap();
    assert!(a.took_over_own_bit);
    phy::release(&f).unwrap();
    assert_eq!(f.peek(regs::SW_FW_SYNC), 0);
}
