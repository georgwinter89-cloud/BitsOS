//! BL-2 host tests: frames and their windows under hostile lengths, partitions passing the same frames down,
//! stacked disks without copies, reads into frames side by side, byte reads straight into the caller's buffer.

use crate::bdev::DiskFile;
use crate::disk::{Data, Disk, Dispatch, PageSeg};
use crate::frames::{Concat, HeapFrames, PageFrames};
use crate::gpt::Partition;
use crate::policy::DiskKind;
use crate::queue::Kind;
use crate::ramdisk::{CallOp, Fault, RamDisk};
use crate::BlockError;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, FrameIo, Frames, IoBuf, IoPriority::*};
use ksync::SpinLock;
use vfs::pagecache::{Backend, CachePage, PageCache};

fn pattern(n: usize, salt: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31) ^ (i >> 9) as u8 ^ salt).collect()
}

fn heap(v: Vec<u8>) -> IoBuf {
    IoBuf::new(Arc::new(HeapFrames::new(v)))
}

fn bytes_of(b: &IoBuf) -> Vec<u8> {
    let mut v = vec![0u8; b.len()];
    assert!(b.read(0, &mut v));
    v
}

fn blank_pages(n: u64) -> Vec<Arc<CachePage>> {
    struct Nothing;
    impl Backend for Nothing {
        fn read_page(&self, _: u64, _: &Arc<CachePage>) -> Result<(), i64> {
            Ok(())
        }
        fn write_run(&self, _: u64, _: &[Arc<CachePage>], _: u64) -> Result<(), i64> {
            Ok(())
        }
    }
    let c = PageCache::with_backend(Arc::new(Nothing), n * 4096);
    (0..n).map(|i| c.page_for_map(i, false).unwrap()).collect()
}

#[test]
fn windows_refuse_hostile_lengths() {
    let b = heap(pattern(10_000, 1));
    assert_eq!(b.len(), 10_000);
    assert!(b.slice(0, 10_000).is_some());
    assert!(b.slice(1, 10_000).is_none());
    assert!(b.slice(10_000, 0).is_some_and(|s| s.is_empty()));
    assert!(b.slice(10_001, 0).is_none());
    assert!(b.slice(usize::MAX, 2).is_none());
    assert!(b.slice(5000, usize::MAX).is_none());
    // A window of a window reads the bytes at the sum of both offsets.
    let w = b.slice(1000, 5000).unwrap().slice(250, 100).unwrap();
    assert_eq!(bytes_of(&w), pattern(10_000, 1)[1250..1350]);
    let mut out = [0u8; 8];
    assert!(!w.read(95, &mut out), "past the window");
    assert!(!w.read(usize::MAX, &mut out));
    assert!(!w.write(93, &[0; 8]));
    assert!(!w.zero(1, 100));
    assert!(w.zero(10, 20));
    assert!(w.phys(0).is_none() && w.phys(100).is_none(), "heap memory is not a DMA target");
    let all = bytes_of(&b);
    assert!(all[1260..1280].iter().all(|&x| x == 0));
    assert_eq!(all[1259], pattern(10_000, 1)[1259]);
    assert!(w.same(&w.clone()) && !w.same(&b));
}

#[test]
fn page_frames_index_segments_and_copy_across_them() {
    let pages = blank_pages(3);
    // Hostile segments are refused.
    assert!(PageFrames::new(vec![PageSeg { page: pages[0].clone(), off: 4000, len: 200 }]).is_none());
    assert!(PageFrames::new(vec![PageSeg { page: pages[0].clone(), off: 0, len: 0 }]).is_none());
    assert!(PageFrames::new(vec![PageSeg { page: pages[0].clone(), off: usize::MAX, len: 2 }]).is_none());
    let f = PageFrames::new(vec![
        PageSeg { page: pages[0].clone(), off: 512, len: 1024 },
        PageSeg { page: pages[1].clone(), off: 0, len: 4096 },
        PageSeg { page: pages[2].clone(), off: 0, len: 512 },
    ])
    .unwrap();
    assert_eq!(f.len(), 1024 + 4096 + 512);
    let data = pattern(f.len(), 3);
    assert!(f.write(0, &data));
    assert!(!f.write(1, &data), "one byte past the end");
    let mut back = vec![0u8; 3000];
    assert!(f.read(700, &mut back));
    assert_eq!(back, data[700..3700]);
    // The bytes landed where the segments say.
    let mut p0 = vec![0u8; 4096];
    pages[0].read(0, &mut p0);
    assert!(p0[..512].iter().all(|&x| x == 0) && p0[512..1536] == data[..1024]);
    let mut p2 = vec![0u8; 4096];
    pages[2].read(0, &mut p2);
    assert!(p2[..512] == data[5120..] && p2[512..].iter().all(|&x| x == 0));
    assert!(f.phys(0).is_none(), "host pages have no frame");
    assert!(f.phys(f.len()).is_none());
}

#[test]
fn a_concat_is_its_members_one_after_another() {
    let a = heap(pattern(700, 1));
    let b = heap(pattern(300, 2)).slice(100, 200).unwrap();
    let c = Concat::new(vec![a.clone(), b.clone()]).unwrap();
    assert_eq!(c.len(), 900);
    let mut v = vec![0u8; 900];
    assert!(c.read(0, &mut v));
    assert_eq!(&v[..700], &pattern(700, 1)[..]);
    assert_eq!(&v[700..], &pattern(300, 2)[100..300]);
    assert!(c.write(650, &[9; 100]));
    assert!(bytes_of(&a)[650..].iter().all(|&x| x == 9));
    assert!(bytes_of(&b)[..50].iter().all(|&x| x == 9));
    assert!(!c.read(850, &mut [0; 51]));
}

/// A lower device that records the frames it is handed (the partition must pass the same ones down).
struct Recorder {
    blocks: u64,
    seen: SpinLock<Vec<(u64, IoBuf)>>,
}

impl BlockDevice for Recorder {
    fn name(&self) -> &str {
        "rec"
    }
    fn info(&self) -> BlockInfo {
        BlockInfo { block_size: 512, blocks: self.blocks, read_only: false, max_blocks_per_request: 8, supports_discard: false, has_volatile_cache: false }
    }
    fn read_blocks(&self, _: u64, _: &mut [u8], _: devmodel::block::IoPriority) -> Result<(), DevError> {
        Err(DevError::Io)
    }
    fn write_blocks(&self, _: u64, _: &[u8], _: bool, _: devmodel::block::IoPriority) -> Result<(), DevError> {
        Err(DevError::Io)
    }
    fn flush(&self) -> Result<(), DevError> {
        Ok(())
    }
    fn read_frames(&self, reqs: &[FrameIo], _: devmodel::block::IoPriority) -> Result<(), DevError> {
        self.seen.lock().extend(reqs.iter().map(|r| (r.lba, r.buf.clone())));
        Ok(())
    }
}

#[test]
fn a_partition_passes_the_same_frames_down_offset_and_bounded() {
    let rec = Arc::new(Recorder { blocks: 1000, seen: SpinLock::new(Vec::new()) });
    let p = Partition::new(rec.clone(), "rec", 100, 50, 1, "x").unwrap();
    let a = heap(vec![0; 1024]);
    let b = heap(vec![0; 512]);
    p.read_frames(&[FrameIo { lba: 0, buf: a.clone() }, FrameIo { lba: 49, buf: b.clone() }], Normal).unwrap();
    let seen = rec.seen.lock().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].0 == 100 && seen[0].1.same(&a));
    assert!(seen[1].0 == 149 && seen[1].1.same(&b));
    // Past the end, an odd length, an empty one, an LBA that overflows: refused before anything goes down.
    rec.seen.lock().clear();
    for (lba, len) in [(49u64, 1024usize), (0, 100), (0, 0), (u64::MAX, 512), (u64::MAX - 99, 512)] {
        let r = p.read_frames(&[FrameIo { lba: 0, buf: heap(vec![0; 512]) }, FrameIo { lba, buf: heap(vec![0; len]) }], Normal);
        assert_eq!(r, Err(DevError::InvalidArgument), "lba {lba} len {len}");
    }
    assert!(rec.seen.lock().is_empty(), "a refused list sends nothing");
    assert_eq!(p.write_frames(50, &b, false, Normal), Err(DevError::InvalidArgument));
}

fn ram(blocks: u64, depth: u32) -> Arc<RamDisk> {
    let r = Arc::new(RamDisk::new("hbl2", 512, blocks).with_max_blocks(64).with_queue_depth(depth));
    r.poke(0, &pattern(blocks as usize * 512, 5));
    r
}

#[test]
fn a_page_cache_read_through_a_stacked_partition_copies_nothing() {
    let r = ram(4096, 8);
    let bottom = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    assert_eq!(bottom.slots(), 8);
    let part = Partition::new(Arc::new(bottom.part_port(64, 2048)), "hbl2", 64, 2048, 1, "x").unwrap();
    let pd = Disk::open(Arc::new(part), DiskKind::Stacked, Dispatch::Inline).unwrap();
    assert_eq!(pd.slots(), u32::MAX, "a stacked disk only remaps");
    // The table reads at open copied (a byte read through a request): only what follows counts.
    let base = (bottom.stats().copied_bytes, pd.stats().copied_bytes);
    r.record();
    let pc = DiskFile::page_cache(pd.clone());
    let mut back = vec![0u8; 200 * 1024];
    assert_eq!(pc.read_at(8192 + 100, &mut back).unwrap(), back.len());
    let want = pattern(4096 * 512, 5);
    assert_eq!(back[..], want[64 * 512 + 8192 + 100..][..back.len()]);
    assert_eq!((bottom.stats().copied_bytes, pd.stats().copied_bytes), base, "no zeroed buffer, no copy, no bounce");
    assert!(r.frame_calls() > 0 && r.calls().iter().all(|c| c.op == CallOp::Read && c.lba >= 64));
    // A byte read of the stacked disk goes straight down, also without a copy when the bottom is idle.
    let mut b = vec![0u8; 4096];
    pd.read(10, &mut b, Interactive).unwrap();
    assert_eq!(b[..], want[(64 + 10) * 512..][..4096]);
    assert_eq!((bottom.stats().copied_bytes, pd.stats().copied_bytes), base);
    assert_eq!(pd.pending(), (0, 0));
    assert_eq!(bottom.pending(), (0, 0));
}

#[test]
fn reads_into_frames_queued_together_reach_the_device_in_one_call() {
    let r = ram(4096, 8);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let bufs: Vec<IoBuf> = (0..5).map(|_| heap(vec![0; 4096])).collect();
    // 0 and 8 are adjacent (merged: one batch), 100, 300, 500 apart.
    let lbas = [0u64, 8, 100, 300, 500];
    let reqs: Vec<FrameIo> = lbas.iter().zip(&bufs).map(|(&lba, b)| FrameIo { lba, buf: b.clone() }).collect();
    let base = d.stats().copied_bytes;
    r.record();
    let calls0 = r.frame_calls();
    d.read_frames(&reqs, Normal).unwrap();
    let want = pattern(4096 * 512, 5);
    for (lba, b) in lbas.iter().zip(&bufs) {
        assert_eq!(bytes_of(b), want[*lba as usize * 512..][..4096], "lba {lba}");
    }
    assert_eq!(r.frame_calls() - calls0, 1, "four batches side by side in one call");
    let got: Vec<(u64, u64)> = r.calls().iter().map(|c| (c.lba, c.blocks)).collect();
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(got.contains(&(0, 16)), "the adjacent pair merged: {got:?}");
    assert_eq!(d.stats().merged, 1);
    assert_eq!(d.stats().copied_bytes, base);
    // With one slot they go one after another, still without a copy.
    let r1 = ram(4096, 1);
    let d1 = Disk::open(r1.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let (c0, base1) = (r1.frame_calls(), d1.stats().copied_bytes);
    d1.read_frames(&reqs, Normal).unwrap();
    assert_eq!(r1.frame_calls() - c0, 4);
    assert_eq!(d1.stats().copied_bytes, base1);
}

#[test]
fn a_byte_read_goes_straight_in_only_when_nothing_waits() {
    let r = ram(1024, 1);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let want = pattern(1024 * 512, 5);
    let mut b = vec![0u8; 1024];
    let base = d.stats().copied_bytes;
    d.read(7, &mut b, Normal).unwrap();
    assert_eq!(b[..], want[7 * 512..][..1024]);
    assert_eq!(d.stats().copied_bytes, base, "idle: straight into the buffer");
    // Something queued (submitted, not waited for yet): the read queues behind it by the elevator's rules.
    r.record();
    let t = d.submit(Kind::Read, 300, 0, Data::Bytes(vec![0; 512]), Interactive).unwrap();
    d.read(20, &mut b, Background).unwrap();
    assert_eq!(b[..], want[20 * 512..][..1024]);
    assert_eq!(d.stats().copied_bytes - base, 2 * 1024, "zeroed buffer plus its copy");
    let order: Vec<u64> = r.calls().iter().map(|c| c.lba).collect();
    assert_eq!(order, [300, 20], "the Interactive request went first");
    assert!(t.is_done());
    drop(t);
    // Past the end and odd lengths are refused before anything happens.
    assert_eq!(d.read(1023, &mut b, Normal), Err(crate::BlockError::BeyondEnd));
    assert_eq!(d.read(u64::MAX, &mut b, Normal), Err(crate::BlockError::BeyondEnd));
    assert_eq!(d.read(0, &mut [0u8; 100], Normal), Err(crate::BlockError::Invalid));
}

#[test]
fn a_dropped_ticket_is_still_dispatched() {
    let r = ram(256, 1);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    r.record();
    drop(d.submit(Kind::Write { fua: false, preflush: false }, 9, 0, Data::Bytes(vec![0xAB; 512]), Background).unwrap());
    assert_eq!(r.calls().len(), 1);
    assert_eq!(d.read_vec(9, 512, Normal).unwrap(), vec![0xAB; 512]);
    assert_eq!(d.pending(), (0, 0));
}

/// Polls `cond` for up to 10 s (host threads; the device's pause bounds every wait as well).
fn until(cond: impl Fn() -> bool, what: &str) {
    for _ in 0..10_000 {
        if cond() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("never: {what}");
}

#[test]
fn a_timeout_in_a_call_of_several_reads_fails_them_all_and_sends_none_again() {
    let r = ram(4096, 8);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let lbas = [0u64, 100, 300];
    let bufs: Vec<IoBuf> = lbas.iter().map(|_| heap(vec![0; 4096])).collect();
    let reqs: Vec<FrameIo> = lbas.iter().zip(&bufs).map(|(&lba, b)| FrameIo { lba, buf: b.clone() }).collect();
    r.inject(Fault { write: false, lba: 100, blocks: 8, error: DevError::Timeout, times: None });
    let c0 = r.frame_calls();
    assert_eq!(d.read_frames(&reqs, Normal), Err(BlockError::Timeout));
    assert_eq!(r.frame_calls() - c0, 1, "one call for all three, none sent again after the timeout");
    assert_eq!(d.pending(), (0, 0));
    assert!(d.take_timed_out());
    // A media error belongs to its data: each batch again on its own, only the bad one fails.
    r.clear_faults();
    r.inject(Fault { write: false, lba: 100, blocks: 8, error: DevError::Io, times: None });
    let c0 = r.frame_calls();
    assert_eq!(d.read_frames(&reqs, Normal), Err(BlockError::Io));
    assert_eq!(r.frame_calls() - c0, 1 + 3, "the call, then each batch alone");
    let want = pattern(4096 * 512, 5);
    assert_eq!(bytes_of(&bufs[0]), want[..4096]);
    assert_eq!(bytes_of(&bufs[2]), want[300 * 512..][..4096]);
}

#[test]
fn two_readers_are_in_the_device_at_once() {
    let r = ram(4096, 4);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    r.pause();
    let reader = |lba: u64| {
        let d = d.clone();
        std::thread::spawn(move || {
            let b = heap(vec![0; 4096]);
            d.read_frames(&[FrameIo { lba, buf: b.clone() }], Normal).unwrap();
            bytes_of(&b)
        })
    };
    let a = reader(0);
    until(|| r.paused_calls().0 == 1, "the first read in the device");
    let b = reader(800);
    until(|| r.paused_calls().0 == 2, "the second read beside it");
    assert_eq!(d.stats().inflight, 2);
    r.resume();
    let want = pattern(4096 * 512, 5);
    assert_eq!(a.join().unwrap(), want[..4096]);
    assert_eq!(b.join().unwrap(), want[800 * 512..][..4096]);
    assert_eq!(r.paused_calls(), (0, 2));
    assert!(d.stats().inflight_max >= 2);
}

#[test]
fn a_waiter_returns_by_its_deadline_while_the_device_still_holds_its_frames() {
    // One slot. B is queued first, then a reader A dispatches B with its own read beside it into the device,
    // which hangs. B's waiter gives up at its deadline (B4) while the frames stay held until the device answers;
    // an Interactive read behind them gives up at its deadline too and never reaches the device.
    let r = ram(4096, 1);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    r.pause();
    r.record();
    let hf = Arc::new(HeapFrames::new(vec![0u8; 4096]));
    let t = d.submit(Kind::Read, 200, 0, Data::Frames(IoBuf::new(hf.clone())), Normal).unwrap();
    let a = {
        let d = d.clone();
        std::thread::spawn(move || {
            let b = heap(vec![0; 4096]);
            d.read_frames(&[FrameIo { lba: 0, buf: b.clone() }], Normal).map(|()| bytes_of(&b))
        })
    };
    until(|| r.holding(), "A in the device with B");
    d.set_timeout_ns(50_000_000);
    assert_eq!(t.wait().err(), Some(BlockError::Timeout));
    assert!(Arc::strong_count(&hf) > 1, "the device still has B's frames: they stay alive");
    let late = heap(vec![0; 4096]);
    assert_eq!(d.read_frames(&[FrameIo { lba: 300, buf: late }], Interactive), Err(BlockError::Timeout));
    d.set_timeout_ns(30_000_000_000);
    r.resume();
    let want = pattern(4096 * 512, 5);
    assert_eq!(a.join().unwrap().unwrap(), want[..4096]);
    assert!(!r.calls().iter().any(|c| c.lba == 300), "the timed-out request never reached the device");
    assert_eq!(Arc::strong_count(&hf), 1, "released once the device answered");
    assert_eq!(d.pending(), (0, 0));
}

#[test]
fn an_interactive_waiter_does_not_run_background_work_while_its_own_waits() {
    // Two slots; a write at the device holds back an Interactive read that overlaps it. A Background read
    // elsewhere is queued too. The Interactive waiter does not take it into its own thread (it waits for its
    // own); the Background read's own waiter runs it.
    let r = ram(4096, 2);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    r.pause();
    r.record();
    let w = {
        let d = d.clone();
        std::thread::spawn(move || d.write(10, &[9u8; 512], false, Normal))
    };
    until(|| r.paused_calls().0 == 1, "the write in the device");
    let bg = d.submit(Kind::Read, 1000, 0, Data::Bytes(vec![0; 512]), Background).unwrap();
    let i = {
        let d = d.clone();
        std::thread::spawn(move || d.read_vec(10, 512, Interactive))
    };
    // The Interactive waiter looks (several polls) and leaves the Background read queued.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(r.paused_calls().0, 1, "nothing else went to the device: {:?}", r.calls());
    assert!(!bg.is_done());
    r.resume();
    assert_eq!(w.join().unwrap(), Ok(()));
    assert_eq!(i.join().unwrap().unwrap(), vec![9u8; 512], "the read after the write it waited for");
    assert!(bg.wait().is_ok());
    assert_eq!(d.pending(), (0, 0));
}
