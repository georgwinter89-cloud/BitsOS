//! The fake card refuses what Linux would not do (NIC-10). Proved by mutation: every write of the driver's sequences
//! is left out in turn, and every pair of neighbouring writes swapped, and each time the fake must refuse to start,
//! refuse the reset, or pass no frame. So a sequence that passes the device tests is Linux's, write for write.

use super::device::frame;
use crate::bringup;
use crate::device::DMA_LIMIT;
use crate::desc::RING_BYTES;
use crate::fakecard::{first_missing, Fake, Knobs, Need, Op, AFTER_ENABLE, AN_DELAY_NS, BEFORE_ENABLE, BEFORE_PROBE_RESET, BEFORE_RESET};
use crate::hw::Hw;
use crate::machine::Machine;
use crate::mem::VecMem;
use crate::ocp::Mdio;
use crate::phy;
use crate::regs::*;
use crate::ring::{RxRing, TxRing, RX_CHUNKS, RX_CHUNK_BYTES, TX_BUF_BYTES};
use crate::seq::{self, play, StartCtx, Step};
use std::format;
use std::string::String;
use std::vec::Vec;

struct Bench {
    f: Fake,
    rx: RxRing<VecMem>,
    tx: TxRing<VecMem>,
    ctx: StartCtx,
}

/// A card after a legal probe, bus master on, the PHY out of power-down, rings in the fake's memory.
fn bench() -> Bench {
    let f = Fake::new();
    let chunks = (0..RX_CHUNKS).map(|_| f.alloc_dma(RX_CHUNK_BYTES).unwrap()).collect();
    let rx = RxRing::new(f.alloc_dma(RING_BYTES).unwrap(), chunks, DMA_LIMIT).unwrap();
    let tx = TxRing::new(f.alloc_dma(RING_BYTES).unwrap(), f.alloc_dma(TX_BUF_BYTES).unwrap(), DMA_LIMIT).unwrap();
    f.enable(false).unwrap();
    let p = bringup::probe(&f).unwrap();
    f.enable(true).unwrap();
    phy::bring_up(&f, &mut Mdio::default(), None).unwrap();
    let ctx = StartCtx::new(p.cp_cmd, tx.desc_addr(), rx.desc_addr()).unwrap();
    Bench { f, rx, tx, ctx }
}

fn flat(seq: seq::Seq) -> Vec<Step> {
    seq.iter().flat_map(|part| part.iter().copied()).collect()
}

fn is_write(s: &Step) -> bool {
    !matches!(s, Step::Commit | Step::Delay(_) | Step::Wait { .. })
}

/// Cleanup, ring arming and start as given, then the link: does the chip run, pass a frame each way, and refuse nothing?
fn works(cleanup: &[Step], start: &[Step]) -> bool {
    let mut b = bench();
    let _ = play(&b.f, &[cleanup], None);
    b.rx.arm_all().unwrap();
    b.tx.reset(0).unwrap();
    let _ = play(&b.f, &[start], Some(&b.ctx));
    phy::config_aneg(&b.f, &mut Mdio::default(), phy::advert(phy::mode::GAMING_PC)).unwrap();
    b.f.advance(AN_DELAY_NS + 1);
    let mut buf = [0u8; 2048];
    let rx = b.f.rx(&frame(1, 100)) && b.rx.take(&mut buf) == Ok(crate::ring::RxOut::Frame(100));
    let tx = b.tx.put(&b.f, &frame(2, 100), 0).is_ok() && b.f.sent() == [frame(2, 100)];
    b.f.violations().is_empty() && b.f.started() && rx && tx
}

fn without(steps: &[Step], i: usize) -> Vec<Step> {
    let mut v = steps.to_vec();
    v.remove(i);
    v
}

#[test]
fn the_sequences_as_they_are_work() {
    assert!(works(&flat(seq::CLEANUP), &flat(seq::HW_START)));
}

#[test]
fn leaving_out_any_write_of_the_start_is_refused() {
    let (cleanup, start) = (flat(seq::CLEANUP), flat(seq::HW_START));
    let survivors: Vec<String> = (0..start.len())
        .filter(|i| is_write(&start[*i]))
        .filter(|i| works(&cleanup, &without(&start, *i)))
        .map(|i| format!("{i}: {:?}", start[i]))
        .collect();
    assert_eq!(survivors, Vec::<String>::new());
    assert!(start.iter().filter(|s| is_write(s)).count() >= 60);
}

#[test]
fn swapping_two_neighbouring_writes_of_the_start_is_refused() {
    let (cleanup, start) = (flat(seq::CLEANUP), flat(seq::HW_START));
    let writes: Vec<usize> = (0..start.len()).filter(|i| is_write(&start[*i])).collect();
    let mut survivors = Vec::new();
    for pair in writes.windows(2) {
        if start[pair[0]] == start[pair[1]] {
            continue;
        }
        let mut swapped = start.clone();
        swapped.swap(pair[0], pair[1]);
        if works(&cleanup, &swapped) {
            survivors.push(format!("{:?} <-> {:?}", start[pair[0]], start[pair[1]]));
        }
    }
    assert_eq!(survivors, Vec::<String>::new());
}

#[test]
fn leaving_out_any_write_of_the_cleanup_is_refused() {
    let (cleanup, start) = (flat(seq::CLEANUP), flat(seq::HW_START));
    for i in (0..cleanup.len()).filter(|i| is_write(&cleanup[*i])) {
        assert!(!works(&without(&cleanup, i), &start), "{:?} left out", cleanup[i]);
    }
}

#[test]
fn leaving_out_any_write_of_the_probe_makes_the_reset_illegal() {
    let probe = flat(seq::PROBE);
    for i in (0..probe.len() - 2).filter(|i| is_write(&probe[*i])) {
        let f = Fake::new();
        let _ = play(&f, &[&without(&probe, i)], None);
        assert!(f.violations().iter().any(|v| v.starts_with("chip reset without")), "{:?} left out: {:?}", probe[i], f.violations());
    }
    let f = Fake::new();
    play(&f, seq::PROBE, None).unwrap();
    assert_eq!(f.violations(), Vec::<String>::new());
}

#[test]
fn the_stop_is_a_legal_reset_too() {
    let b = bench();
    assert_eq!(bringup::stop(&b.f), 0);
    assert_eq!(b.f.violations(), Vec::<String>::new());
}

#[test]
fn a_start_right_after_the_probe_without_the_cleanup_is_refused() {
    let mut b = bench();
    b.rx.arm_all().unwrap();
    b.tx.reset(0).unwrap();
    play(&b.f, seq::HW_START, Some(&b.ctx)).unwrap();
    assert!(!b.f.started());
    assert_eq!(b.f.violations(), ["enable without rtl8169_cleanup's reset before it"]);
}

#[test]
fn rings_that_are_not_armed_or_not_in_the_drivers_memory_are_refused() {
    // Not armed: the memory still holds what it held.
    let b = bench();
    play(&b.f, seq::CLEANUP, None).unwrap();
    play(&b.f, seq::HW_START, Some(&b.ctx)).unwrap();
    assert!(!b.f.started());
    assert!(b.f.violations()[0].starts_with("RX descriptor 0 is"), "{:?}", b.f.violations());
    // A ring address that is nobody's memory.
    let mut b = bench();
    play(&b.f, seq::CLEANUP, None).unwrap();
    b.rx.arm_all().unwrap();
    b.tx.reset(0).unwrap();
    let wild = StartCtx::new(0x2061, 0x7000_0000, b.rx.desc_addr()).unwrap();
    play(&b.f, seq::HW_START, Some(&wild)).unwrap();
    assert!(!b.f.started());
    assert!(b.f.violations().iter().any(|v| v.starts_with("DMA outside the driver's memory")), "{:?}", b.f.violations());
}

#[test]
fn a_start_with_bus_master_off_is_refused() {
    let mut b = bench();
    b.f.pci_off();
    play(&b.f, seq::CLEANUP, None).unwrap();
    b.rx.arm_all().unwrap();
    b.tx.reset(0).unwrap();
    play(&b.f, seq::HW_START, Some(&b.ctx)).unwrap();
    assert!(!b.f.started());
    assert_eq!(b.f.violations(), ["enable with bus master off"]);
}

#[test]
fn writes_linux_never_makes_are_refused() {
    let refused = |act: &dyn Fn(&Fake)| {
        let f = Fake::new();
        act(&f);
        f.violations().len()
    };
    // The persistent parts of the card and other chips' ways to them.
    for off in NEVER_WRITTEN {
        assert_eq!(refused(&|f| f.wr32(*off, 0)), 1, "{off:#x}");
        assert_eq!(refused(&|f| f.wr8(*off, 0)), 1, "{off:#x}");
    }
    // Cfg9346: 0x80 and 0x40 are "program" and "autoload", the EEPROM pins hang on the low bits.
    for v in [0x80, 0x40, 0x01, 0x02, 0x08, 0xc1] {
        assert_eq!(refused(&|f| f.wr8(CFG9346, v)), 1, "{v:#x}");
    }
    assert_eq!(refused(&|f| f.wr8(CFG9346, CFG9346_UNLOCK)), 0);
    // The right register at the wrong width would touch its neighbours.
    assert_eq!(refused(&|f| f.wr32(CHIP_CMD - 3, 0)), 1);
    assert_eq!(refused(&|f| f.wr16(CFG9346, 0)), 1);
    assert_eq!(refused(&|f| f.wr8(TX_POLL, 1)), 1);
    // Config and MAC registers are written under the unlock only.
    assert_eq!(refused(&|f| f.wr8(CONFIG3, 0)), 1);
    assert_eq!(refused(&|f| f.wr32(MAC0, 0)), 1);
    // An interrupt mask Linux does not use, and config space beyond the two bytes.
    assert_eq!(refused(&|f| f.wr32(INTR_MASK, 0xffff)), 1);
    assert_eq!(refused(&|f| assert!(!f.cfg_wr8(0x04, 0x06))), 1);
    assert_eq!(refused(&|f| assert!(f.cfg_wr8(0x70f, 0x27))), 0);
}

#[test]
fn the_lists_are_a_second_transcription_and_agree_with_the_tables() {
    // Every write of the tables is something a list expects, and the lists expect nothing the tables lack: the log
    // of one clean run holds each list in order, and the run is what `works` accepts.
    let mut b = bench();
    let after_probe = b.f.ops().len();
    assert!(first_missing(BEFORE_PROBE_RESET, &b.f.ops(), false).is_none());
    play(&b.f, seq::CLEANUP, None).unwrap();
    assert!(first_missing(BEFORE_RESET, &b.f.ops()[after_probe..], false).is_none());
    b.rx.arm_all().unwrap();
    b.tx.reset(0).unwrap();
    let before = b.f.ops().len();
    play(&b.f, seq::HW_START, Some(&b.ctx)).unwrap();
    let ops = b.f.ops();
    let started = ops.iter().position(|o| *o == Op::Started).unwrap();
    assert!(first_missing(BEFORE_ENABLE, &ops[before..started], false).is_none());
    assert!(first_missing(AFTER_ENABLE, &ops[started..], false).is_none());
    // The lists are long enough to mean something.
    assert_eq!((BEFORE_PROBE_RESET.len(), BEFORE_RESET.len(), BEFORE_ENABLE.len(), AFTER_ENABLE.len()), (11, 5, 57, 6));
    assert!(BEFORE_ENABLE.iter().filter(|n| matches!(n, Need::Ocp(..))).count() == 27);
}

#[test]
fn a_frame_longer_than_the_buffer_the_descriptor_names_is_not_written() {
    let c = super::device::running();
    assert!(c.hw().rx(&frame(1, 1514)));
    // 16 380 bytes of frame and 4 of checksum would be 16 384: one more than the buffer.
    assert!(!c.hw().rx(&std::vec![0u8; 16_380]));
    assert!(c.hw().rx(&std::vec![0u8; 16_379]), "exactly the buffer: taken (and dropped by the driver as too long)");
    let mut buf = [0u8; 2048];
    assert_eq!(c.receive(&mut buf), Some(1514));
    assert_eq!(c.receive(&mut buf), None);
    assert_eq!(c.stats().rx_dropped, 1);
    assert_eq!(c.hw().violations(), Vec::<String>::new());
}

#[test]
fn no_knob_of_the_fake_is_on_by_default() {
    let k = Knobs::default();
    assert!(k.cable && k.irq_arrives && !k.no_cfg && !k.stuck_reset && !k.stuck_phy && !k.tx_stuck && !k.scribble_addr);
}
