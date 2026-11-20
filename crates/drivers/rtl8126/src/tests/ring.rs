//! The rings on memory: layout checks at `new`, ownership order, and that nothing the chip writes is trusted.

use super::fake::{Fake, Op};
use crate::desc::*;
use crate::mem::{DmaMem, VecMem};
use crate::regs::TX_POLL;
use crate::ring::*;
use std::vec;
use std::vec::Vec;

const LIMIT: u64 = 1 << 32;

fn rx_mem(base: u64) -> (VecMem, Vec<VecMem>) {
    let chunks = (0..RX_CHUNKS).map(|i| VecMem::new(base + 0x10_0000 * (i as u64 + 1), RX_CHUNK_BYTES)).collect();
    (VecMem::new(base, RING_BYTES), chunks)
}

fn rx_ring() -> (RxRing<VecMem>, VecMem, Vec<VecMem>) {
    let (desc, chunks) = rx_mem(0x100_0000);
    (RxRing::new(desc.clone(), chunks.clone(), LIMIT).unwrap(), desc, chunks)
}

fn tx_ring() -> (TxRing<VecMem>, VecMem, VecMem) {
    let (desc, bufs) = (VecMem::new(0x200_0000, RING_BYTES), VecMem::new(0x300_0000, TX_BUF_BYTES));
    (TxRing::new(desc.clone(), bufs.clone(), LIMIT).unwrap(), desc, bufs)
}

fn desc_at(m: &VecMem, entry: u32) -> Desc {
    let mut b = [0u8; DESC_BYTES];
    assert!(m.copy_out(entry as usize * DESC_BYTES, &mut b));
    Desc::from_bytes(&b)
}

#[test]
fn memory_that_cannot_be_a_ring_is_refused() {
    let (desc, chunks) = rx_mem(0x100_0000);
    let layout = |r: Result<RxRing<VecMem>, RingError>| matches!(r, Err(RingError::Layout(_)));
    assert!(RxRing::new(desc.clone(), chunks.clone(), LIMIT).is_ok());
    assert!(layout(RxRing::new(VecMem::new(0x100_0080, RING_BYTES), chunks.clone(), LIMIT)), "not 256-byte aligned");
    assert!(layout(RxRing::new(VecMem::new(0, RING_BYTES), chunks.clone(), LIMIT)), "address 0");
    assert!(layout(RxRing::new(VecMem::new(0x100_0000, RING_BYTES - 16), chunks.clone(), LIMIT)), "a descriptor short");
    assert!(layout(RxRing::new(desc.clone(), chunks[..3].to_vec(), LIMIT)), "a run of buffers missing");
    let mut short = chunks.clone();
    short[2] = VecMem::new(0x900_0000, RX_CHUNK_BYTES - 1);
    assert!(layout(RxRing::new(desc.clone(), short, LIMIT)), "a run one byte short");
    assert!(layout(RxRing::new(desc.clone(), chunks.clone(), 0x130_0000)), "a run above the card's reach");
    let mut high = chunks.clone();
    high[3] = VecMem::new(LIMIT - RX_CHUNK_BYTES as u64 + 0x1000, RX_CHUNK_BYTES);
    assert!(layout(RxRing::new(desc.clone(), high, LIMIT)), "a run that ends above the limit");
    let mut wrap = chunks.clone();
    wrap[0] = VecMem::new(u64::MAX - 0x1000, RX_CHUNK_BYTES);
    assert!(layout(RxRing::new(desc, wrap, u64::MAX)), "a run that wraps around");
    let tx = |d: VecMem, b: VecMem| matches!(TxRing::new(d, b, LIMIT), Err(RingError::Layout(_)));
    assert!(tx(VecMem::new(0x200_0010, RING_BYTES), VecMem::new(0x300_0000, TX_BUF_BYTES)));
    assert!(tx(VecMem::new(0x200_0000, RING_BYTES), VecMem::new(0x300_0000, TX_BUF_BYTES - 1)));
    assert!(tx(VecMem::new(0x200_0000, RING_BYTES), VecMem::new(LIMIT - 0x1000, TX_BUF_BYTES)));
}

#[test]
fn arming_gives_every_descriptor_its_own_16_kib_and_only_the_last_the_ring_end() {
    let (mut rx, desc, chunks) = rx_ring();
    rx.arm_all().unwrap();
    let mut seen = Vec::new();
    for i in 0..NUM_RX {
        let d = desc_at(&desc, i);
        assert_eq!(d.opts1, DESC_OWN | RX_BUF_SIZE | if i == 255 { RING_END } else { 0 }, "descriptor {i}");
        assert_eq!(d.opts2, 0);
        let chunk = &chunks[i as usize / RX_CHUNK_BUFS];
        assert!(d.addr >= chunk.addr && d.addr + u64::from(RX_BUF_SIZE) < chunk.addr + RX_CHUNK_BYTES as u64, "descriptor {i} inside its run");
        assert_eq!(d.addr % RX_STRIDE as u64, 0);
        seen.push(d.addr);
    }
    seen.sort_unstable();
    assert!(seen.windows(2).all(|w| w[1] - w[0] >= RX_STRIDE as u64), "no two buffers overlap");
    assert_eq!(RX_STRIDE as u32, RX_BUF_SIZE + 1);
    assert_eq!(RX_CHUNKS * RX_CHUNK_BYTES, RX_MEMORY);
}

/// What the chip does to a descriptor it filled.
fn fill(desc: &VecMem, chunks: &[VecMem], entry: u32, opts1: u32, data: &[u8]) {
    let (chunk, off) = (entry as usize / RX_CHUNK_BUFS, entry as usize % RX_CHUNK_BUFS * RX_STRIDE);
    assert!(chunks[chunk].copy_in(off, data));
    assert!(desc.wr32(entry as usize * DESC_BYTES, opts1));
}

#[test]
fn receive_decodes_drops_and_always_hands_the_descriptor_back_with_its_own_address() {
    let (mut rx, desc, chunks) = rx_ring();
    rx.arm_all().unwrap();
    let mut buf = vec![0u8; 2048];
    assert_eq!(rx.take(&mut buf), Ok(RxOut::Empty));
    let whole = FIRST_FRAG | LAST_FRAG;
    fill(&desc, &chunks, 0, whole | 104, &[0x5a; 104]);
    // The chip may have left anything in the address field.
    assert!(desc.wr64(8, 0xdead_0000_beef));
    assert_eq!(rx.take(&mut buf), Ok(RxOut::Frame(100)));
    assert_eq!(&buf[..100], &[0x5a; 100][..]);
    assert_eq!(desc_at(&desc, 0), rx_init(0, chunks[0].addr), "armed again from the index");
    // An error, a fragment, a runt, a jumbo, a frame too long for the caller: each dropped, each handed back.
    let cases = [
        ((1 << 21) | (1 << 19) | whole | 64, RxDrop::Error { crc: true }),
        (FIRST_FRAG | 1518, RxDrop::Fragmented),
        (whole | 3, RxDrop::BadLength),
        (whole | 17, RxDrop::BadLength),
        (whole | 1523, RxDrop::BadLength),
        (whole | 1400, RxDrop::TooBig),
    ];
    let mut small = vec![0u8; 1024];
    for (i, (opts1, why)) in cases.into_iter().enumerate() {
        let entry = 1 + i as u32;
        fill(&desc, &chunks, entry, opts1, &[0; 16]);
        assert_eq!(rx.take(&mut small), Ok(RxOut::Dropped(why)), "{opts1:#x}");
        assert_eq!(desc_at(&desc, entry), rx_init(entry, rx.buf_addr(entry)));
    }
    // The longest frame handed up: 1500 + header + VLAN tag.
    fill(&desc, &chunks, 7, whole | 1522, &[1; 1522]);
    assert_eq!(rx.take(&mut buf), Ok(RxOut::Frame(1518)));
    assert_eq!(rx.take(&mut buf), Ok(RxOut::Empty));
}

#[test]
fn the_last_descriptor_keeps_its_ring_end_whatever_the_chip_wrote_back() {
    let (mut rx, desc, chunks) = rx_ring();
    rx.arm_all().unwrap();
    let mut buf = vec![0u8; 2048];
    for i in 0..NUM_RX {
        // Written back without the ring-end bit, as a chip that does not preserve it would.
        fill(&desc, &chunks, i, FIRST_FRAG | LAST_FRAG | 64, &[i as u8; 64]);
        assert_eq!(rx.take(&mut buf), Ok(RxOut::Frame(60)));
        assert_eq!(buf[0], i as u8);
    }
    assert_eq!(desc_at(&desc, 255).opts1, DESC_OWN | RING_END | RX_BUF_SIZE);
    fill(&desc, &chunks, 0, FIRST_FRAG | LAST_FRAG | 64, &[0x77; 64]);
    assert_eq!(rx.take(&mut buf), Ok(RxOut::Frame(60)), "wrapped to the first descriptor");
}

#[test]
fn a_frame_goes_into_its_slot_and_the_ownership_bit_is_the_last_thing_written() {
    let hw = Fake::new();
    let (mut tx, desc, bufs) = tx_ring();
    tx.reset(5).unwrap();
    assert_eq!(desc_at(&desc, 0), Desc::default(), "the ring starts cleared (memory held 0xEE)");
    let frame = [0xabu8; 100];
    tx.put(&hw, &frame, 10).unwrap();
    let d = desc_at(&desc, 0);
    assert_eq!(d, Desc { opts1: DESC_OWN | FIRST_FRAG | LAST_FRAG | 100, opts2: 0, addr: bufs.addr });
    assert_eq!(hw.ops(), [Op::W16(TX_POLL, 1)], "one 16-bit doorbell, after the descriptor");
    let mut sent = [0u8; 100];
    assert!(bufs.copy_out(0, &mut sent));
    assert_eq!(sent, frame);
    assert_eq!((tx.in_flight(), tx.progress_ns()), (1, 10));
    // Second frame: next slot, 2 KiB on.
    tx.put(&hw, &[1u8; 14], 11).unwrap();
    let d = desc_at(&desc, 1);
    assert_eq!((d.addr, d.opts1 & 0xffff), (bufs.addr + TX_SLOT as u64, 60));
    let mut padded = [0xffu8; 60];
    assert!(bufs.copy_out(TX_SLOT, &mut padded));
    assert!(padded[..14] == [1u8; 14] && padded[14..] == [0u8; 46]);
    assert_eq!(tx.put(&hw, &[0u8; 13], 0), Err(TxError::BadFrame));
    assert_eq!(tx.put(&hw, &[0u8; 1519], 0), Err(TxError::BadFrame));
}

#[test]
fn reclaim_takes_back_in_order_clears_and_rings_again_while_frames_wait() {
    let hw = Fake::new();
    let (mut tx, desc, _) = tx_ring();
    tx.reset(0).unwrap();
    for _ in 0..3 {
        tx.put(&hw, &[0u8; 64], 1).unwrap();
    }
    assert_eq!(tx.reclaim(&hw, 2), Ok(0), "the card still owns the first");
    // The card finished the second before the first is looked at: nothing comes back out of order.
    assert!(desc.wr32(DESC_BYTES, desc_at(&desc, 1).opts1 & !DESC_OWN));
    assert_eq!(tx.reclaim(&hw, 3), Ok(0));
    assert!(desc.wr32(0, desc_at(&desc, 0).opts1 & !DESC_OWN));
    let bells = hw.ops().len();
    assert_eq!(tx.reclaim(&hw, 4), Ok(2));
    assert_eq!((tx.in_flight(), tx.progress_ns(), tx.done()), (1, 4, 2));
    assert_eq!(desc_at(&desc, 0), Desc::default());
    assert_eq!(hw.ops().len(), bells + 1, "M:4743-4752: an extra doorbell while a frame is still with the card");
    assert!(desc.wr32(2 * DESC_BYTES, desc_at(&desc, 2).opts1 & !DESC_OWN));
    assert_eq!(tx.reclaim(&hw, 5), Ok(1));
    assert_eq!(hw.ops().len(), bells + 1, "none when the ring is empty");
}

#[test]
fn the_ring_takes_256_frames_and_the_last_descriptor_carries_the_ring_end() {
    let hw = Fake::new();
    let (mut tx, desc, _) = tx_ring();
    tx.reset(0).unwrap();
    for i in 0..256 {
        assert_eq!(tx.put(&hw, &[0u8; 64], 0), Ok(()), "frame {i}");
    }
    assert_eq!(tx.put(&hw, &[0u8; 64], 0), Err(TxError::Full));
    assert_eq!(desc_at(&desc, 255).opts1 & RING_END, RING_END);
    assert_eq!(desc_at(&desc, 254).opts1 & RING_END, 0);
    // One comes back: one fits, in the first slot again.
    assert!(desc.wr32(0, desc_at(&desc, 0).opts1 & !DESC_OWN));
    assert_eq!(tx.put(&hw, &[9u8; 64], 0), Ok(()));
    assert_eq!(desc_at(&desc, 0).opts1, DESC_OWN | FIRST_FRAG | LAST_FRAG | 64);
    // A reset empties it whatever the card still held.
    tx.reset(7).unwrap();
    assert_eq!((tx.in_flight(), tx.done()), (0, 1));
    assert_eq!(desc_at(&desc, 100), Desc::default());
}
