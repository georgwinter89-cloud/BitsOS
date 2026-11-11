//! BL-2: reads into frames (`NsDevice::read_frames`) and the PRP rules for a request's own pages (`frame_pages`).

use super::up;
use crate::fake;
use crate::io::frame_pages;
use crate::ns::NsDevice;
use crate::regs::PAGE;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use block::HeapFrames;
use devmodel::DevError;
use devmodel::block::{BlockDevice, FrameIo, Frames, IoBuf, IoPriority};

/// Frames made of pieces at given device addresses (only `phys` is used).
struct Runs(Vec<(u64, usize)>);

impl Frames for Runs {
    fn len(&self) -> usize {
        self.0.iter().map(|r| r.1).sum()
    }
    fn phys(&self, at: usize) -> Option<(u64, usize)> {
        let mut base = 0usize;
        for &(a, n) in &self.0 {
            if at < base + n {
                return Some((a + (at - base) as u64, n - (at - base)));
            }
            base += n;
        }
        None
    }
    fn read(&self, _: usize, _: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _: usize, _: &[u8]) -> bool {
        false
    }
}

fn runs(r: &[(u64, usize)]) -> IoBuf {
    IoBuf::new(Arc::new(Runs(r.to_vec())))
}

const P: u64 = PAGE as u64;

#[test]
fn frame_pages_follow_the_prp_rules() {
    // Whole pages anywhere: one entry each.
    assert_eq!(frame_pages(&runs(&[(7 * P, PAGE), (3 * P, PAGE), (90 * P, PAGE)])), Some(vec![7 * P, 3 * P, 90 * P]));
    // One contiguous run of three pages: three entries.
    assert_eq!(frame_pages(&runs(&[(10 * P, 3 * PAGE)])), Some(vec![10 * P, 11 * P, 12 * P]));
    // A first piece inside its page (dword aligned) running to the page end, then a page, then a short last one.
    assert_eq!(frame_pages(&runs(&[(5 * P + 512, PAGE - 512), (9 * P, PAGE), (2 * P, 1024)])), Some(vec![5 * P + 512, 9 * P, 2 * P]));
    // A window that starts inside a piece keeps the offset.
    let w = runs(&[(4 * P, 2 * PAGE)]).slice(1024, PAGE).unwrap();
    assert_eq!(frame_pages(&w), Some(vec![4 * P + 1024, 5 * P]));
    // Refused: a later piece not on a page, a piece ending inside a page before the end, a first piece not dword
    // aligned, memory the device cannot reach, more pages than one list holds.
    assert_eq!(frame_pages(&runs(&[(5 * P, PAGE), (9 * P + 512, PAGE)])), None);
    assert_eq!(frame_pages(&runs(&[(5 * P, 2048), (9 * P, PAGE)])), None);
    assert_eq!(frame_pages(&runs(&[(5 * P + 2, PAGE - 2)])), None);
    assert_eq!(frame_pages(&IoBuf::new(Arc::new(HeapFrames::new(vec![0; PAGE])))), None);
    let many: Vec<(u64, usize)> = (0..(PAGE / 8 + 2) as u64).map(|k| ((k * 2 + 1) * P, PAGE)).collect();
    assert_eq!(frame_pages(&runs(&many)), None);
    assert!(frame_pages(&runs(&many[..PAGE / 8 + 1])).is_some(), "a full list is fine");
}

#[test]
fn namespace_reads_into_frames_are_its_bytes() {
    let (c, _hw) = up(vec![fake::ns(1, 9, 4096)]);
    let d = NsDevice::new(c.clone(), 1).unwrap();
    assert!(d.queue_depth() > 1, "the controller's tags: {}", d.queue_depth());
    assert_eq!(d.queue_depth() as usize, c.queue_stats().iter().map(|q| q.tags).sum::<usize>());
    let xfer = c.info().max_transfer;
    // Pieces of different sizes, one larger than a transfer (cut into commands), all at once.
    let lens = [512usize, 4096, xfer + 3 * 512, 8 * 512];
    let lbas = [0u64, 7, 100, 4000];
    let bufs: Vec<IoBuf> = lens.iter().map(|&n| IoBuf::new(Arc::new(HeapFrames::new(vec![0; n])))).collect();
    let reqs: Vec<FrameIo> = lbas.iter().zip(&bufs).map(|(&lba, b)| FrameIo { lba, buf: b.clone() }).collect();
    d.read_frames(&reqs, IoPriority::Normal).unwrap();
    for ((lba, n), b) in lbas.iter().zip(lens).zip(&bufs) {
        let mut want = vec![0u8; n];
        d.read_blocks(*lba, &mut want, IoPriority::Normal).unwrap();
        let mut got = vec![0u8; n];
        assert!(b.read(0, &mut got));
        assert_eq!(got, want, "lba {lba}");
    }
    // Past the end, odd lengths, empty: refused before a command is built.
    for (lba, n) in [(4095u64, 1024usize), (0, 100), (0, 0), (u64::MAX, 512)] {
        let r = d.read_frames(&[FrameIo { lba, buf: IoBuf::new(Arc::new(HeapFrames::new(vec![0; n]))) }], IoPriority::Normal);
        assert_eq!(r, Err(DevError::InvalidArgument), "lba {lba} len {n}");
    }
}
