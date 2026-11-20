//! A discard through /data's stack (a stacked disk on a GPT partition on a `PartPort` of an NVMe-like disk):
//! it reaches the device at the priority its caller gave (bitsvol sends its freed chunks at Normal, trim.rs), moved
//! by the partition's offset, and never outside the partition or the armed write window.

use crate::disk::{Disk, DiskDevice, Dispatch};
use crate::gpt::Partition;
use crate::policy::DiskKind;
use crate::ramdisk::{Call, CallOp, RamDisk};
use alloc::sync::Arc;
use devmodel::DevError;
use devmodel::block::{BlockDevice, IoPriority, IoPriority::*};

const FIRST: u64 = 64;
const BLOCKS: u64 = 2048;

/// The RAM disk, its disk, and the stacked disk on its partition `FIRST..FIRST + BLOCKS` (through a port of it).
fn stack(window: bool) -> (Arc<RamDisk>, Arc<Disk>, DiskDevice) {
    let r = Arc::new(RamDisk::new("dsc", 512, 4096).with_max_blocks(64).with_queue_depth(4));
    let bottom = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    if window {
        bottom.arm_window(FIRST, BLOCKS, None);
    }
    let part = Partition::new(Arc::new(bottom.part_port(FIRST, BLOCKS)), "dsc", FIRST, BLOCKS, 1, "x").unwrap();
    let top = Disk::open(Arc::new(part), DiskKind::Stacked, Dispatch::Inline).unwrap();
    r.record();
    (r, bottom, DiskDevice(top))
}

fn discards(r: &RamDisk) -> alloc::vec::Vec<Call> {
    r.calls().into_iter().filter(|c| c.op == CallOp::Discard).collect()
}

#[test]
fn a_discard_reaches_the_device_at_its_callers_priority() {
    let (r, _bottom, top) = stack(true);
    r.poke(0, &alloc::vec![0xAB; 4096 * 512]);
    for (i, prio) in [Normal, Interactive, Background].into_iter().enumerate() {
        top.discard_at(10 + 100 * i as u64, 16, prio).unwrap();
    }
    top.discard(500, 8).unwrap();
    let want = |lba: u64, blocks: u64, prio: IoPriority| Call { op: CallOp::Discard, lba: FIRST + lba, blocks, prio };
    assert_eq!(discards(&r), [want(10, 16, Normal), want(110, 16, Interactive), want(210, 16, Background), want(500, 8, Background)]);
    let mut b = [0u8; 512];
    r.read_blocks(FIRST + 10, &mut b, Normal).unwrap();
    assert_eq!(b, [0; 512], "discarded");
    r.read_blocks(FIRST + 26, &mut b, Normal).unwrap();
    assert_eq!(b, [0xAB; 512], "the block after the range stays");
}

#[test]
fn a_discard_never_leaves_its_partition_or_the_write_window() {
    let (r, bottom, top) = stack(true);
    // Beyond the stacked disk's end, and straight at the partition beyond its end.
    assert_eq!(top.discard_at(BLOCKS - 8, 16, Normal), Err(DevError::NoSpace));
    let part = Partition::new(Arc::new(bottom.part_port(FIRST, BLOCKS)), "dsc", FIRST, BLOCKS, 1, "x").unwrap();
    assert_eq!(part.discard_at(BLOCKS - 8, 16, Normal), Err(DevError::InvalidArgument));
    assert_eq!(part.discard_at(0, 0, Normal), Err(DevError::InvalidArgument));
    // A port of another range than the armed window: refused before the device.
    let other = bottom.part_port(FIRST + BLOCKS, 1024);
    assert_eq!(other.discard_at(FIRST + BLOCKS, 16, Normal), Err(DevError::Refused));
    // The window's own port, outside the window.
    let port = bottom.part_port(FIRST, BLOCKS);
    assert_eq!(port.discard_at(FIRST - 8, 16, Normal), Err(DevError::Refused));
    assert_eq!(port.discard_at(FIRST + BLOCKS - 8, 16, Normal), Err(DevError::Refused));
    assert!(discards(&r).is_empty(), "a refused discard reached the device: {:?}", discards(&r));
    // Inside the window it goes through; with the window disarmed nothing does.
    port.discard_at(FIRST, 8, Normal).unwrap();
    assert_eq!(discards(&r).len(), 1);
    bottom.disarm_window();
    assert_eq!(top.discard_at(0, 8, Normal), Err(DevError::Refused));
    assert_eq!(discards(&r).len(), 1);
}

#[test]
fn without_a_window_the_port_refuses_every_discard() {
    let (r, _bottom, top) = stack(false);
    assert_eq!(top.discard_at(0, 8, Normal), Err(DevError::Refused));
    assert!(discards(&r).is_empty());
}
