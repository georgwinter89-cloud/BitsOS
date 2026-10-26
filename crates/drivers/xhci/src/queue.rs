//! What sits on a bulk or interrupt endpoint's ring (US-3): TDs in submission order, each made of Normal TRBs
//! that never cross a 64 KiB boundary (xHCI 4.11.7.1; V1 `bulk.asm`), with CH on all but the last, IOC on the
//! last, ISP on IN TRBs and the TD Size of xHCI 4.11.2.4 (packets left after the TRB, at most 31, 0 on the last;
//! controllers before xHCI 1.0 count KiB left including the TRB instead, Linux `xhci_td_remainder`).
//! A TD remembers every TRB address and length, so an event naming any of its TRBs (a short packet in the middle,
//! an error, a stop) gives the exact byte count.

use crate::trb::Trb;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use usb_core::{Buffer, IsoUrb, Urb};

/// A TRB never crosses a multiple of this and moves at most this much.
pub const TRB_BOUNDARY: u64 = 0x1_0000;

/// Pieces (device address, length) of `len` bytes at `offset` of a buffer made of `segs`, cut at every 64 KiB
/// boundary. A zero-length transfer is one empty piece. `None` if the range is outside the segments.
pub fn pieces(segs: &[(u64, usize)], offset: usize, len: usize) -> Option<Vec<(u64, u32)>> {
    let total: usize = segs.iter().map(|s| s.1).sum();
    if offset.checked_add(len)? > total {
        return None;
    }
    let mut out = Vec::new();
    let (mut skip, mut left) = (offset, len);
    for &(addr, n) in segs {
        if left == 0 {
            break;
        }
        if skip >= n {
            skip -= n;
            continue;
        }
        let mut a = addr.checked_add(skip as u64)?;
        let mut here = (n - skip).min(left);
        skip = 0;
        left -= here;
        while here > 0 {
            let to_boundary = (TRB_BOUNDARY - (a % TRB_BOUNDARY)) as usize;
            let k = here.min(to_boundary);
            out.push((a, k as u32));
            a += k as u64;
            here -= k;
        }
    }
    if out.is_empty() {
        let a = segs.iter().map(|s| s.0).next().unwrap_or(0);
        out.push((a.saturating_add(offset as u64), 0));
    }
    Some(out)
}

/// xHCI 4.11.7.1 (Linux `xhci_align_td`): where a TD goes on past a Link TRB, the data before the link must be a
/// whole number of packets. `room` = TRB places left before the next link (at least 1), `per_seg` = places per
/// segment. The TRB in front of a link is shortened by the odd part, which moves into the next TRB (still inside
/// the same 64 KiB). `None` if that TRB is shorter than the odd part (the caller pads up to the link instead).
pub fn align_at_links(pieces: &[(u64, u32)], mps: usize, room: usize, per_seg: usize) -> Option<Vec<(u64, u32)>> {
    let mps = mps.max(1) as u64;
    let (mut room, per_seg) = (room.max(1), per_seg.max(2));
    let mut out = Vec::with_capacity(pieces.len() + 2);
    let mut bytes = 0u64;
    let mut q: alloc::collections::VecDeque<(u64, u32)> = pieces.iter().copied().collect();
    while let Some((a, n)) = q.pop_front() {
        if room == 0 {
            room = per_seg;
        }
        if room == 1 && !q.is_empty() {
            let odd = (bytes + n as u64) % mps;
            if odd != 0 {
                if n as u64 <= odd {
                    return None;
                }
                let keep = n - odd as u32;
                out.push((a, keep));
                bytes += keep as u64;
                q.push_front((a + keep as u64, odd as u32));
                room = 0;
                continue;
            }
        }
        out.push((a, n));
        bytes += n as u64;
        room -= 1;
    }
    Some(out)
}

/// The Normal TRBs of one TD over `pieces` for an endpoint with packet size `mps`. `kib_td_size`: the
/// controller is older than xHCI 1.0 (HCIVERSION < 0x100), whose TD Size counts KiB, not packets.
pub fn td_trbs(pieces: &[(u64, u32)], mps: usize, dir_in: bool, kib_td_size: bool) -> Vec<Trb> {
    let mps = mps.max(1);
    let total: usize = pieces.iter().map(|p| p.1 as usize).sum();
    let packets = total.div_ceil(mps);
    let mut done = 0usize;
    let n = pieces.len();
    pieces.iter().enumerate().map(|(i, &(a, len))| {
        let before = done;
        done += len as usize;
        let last = i + 1 == n;
        // Linux `xhci_td_remainder`: 1.0 and later, packets of the TD minus those complete after this TRB; 0.96,
        // the bytes left including this TRB in KiB. 0 on the last either way (`Trb::normal` caps at 31).
        let td_size = match (last, kib_td_size) {
            (true, _) => 0,
            (false, false) => packets.saturating_sub(done / mps) as u32,
            (false, true) => ((total - before) >> 10).min(31) as u32,
        };
        Trb::normal(a, len, td_size, last, dir_in)
    }).collect()
}

/// Who waits for a TD.
pub(crate) enum Owner {
    /// A submitted transfer: its URB and its buffer (kept alive while the controller may touch it).
    Urb { urb: Arc<Urb>, _buf: Buffer, stuck: Arc<AtomicBool> },
    /// Buffer `n` of the endpoint's interrupt pipe.
    Pipe(usize),
    /// Packet `idx` of an isochronous request (one TD per packet, `isoch.rs`).
    Iso { urb: Arc<IsoUrb>, idx: usize, _buf: Buffer, stuck: Arc<AtomicBool> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TdState {
    Live,
    /// A cancel is stopping the endpoint for it.
    Cancelling,
    /// Done for its owner but still on the ring (turned into No-Ops, or its endpoint did not stop): retired when
    /// the controller is known to be past it.
    Zombie,
}

pub(crate) struct Td {
    /// (address, length) of every TRB, in order (Link TRBs not counted).
    pub(crate) trbs: Vec<(u64, u32)>,
    pub(crate) first_cycle: bool,
    pub(crate) owner: Owner,
    pub(crate) state: TdState,
    /// Bytes moved when a Stop Endpoint caught the endpoint inside this TD.
    pub(crate) stopped_bytes: Option<usize>,
    /// Order of queueing on its endpoint (`EpRing::queue` sets it).
    pub(crate) seq: u64,
}

impl Td {
    pub(crate) fn len(&self) -> usize {
        self.trbs.iter().map(|t| t.1 as usize).sum()
    }
    pub(crate) fn first(&self) -> (u64, bool) {
        (self.trbs[0].0, self.first_cycle)
    }
    pub(crate) fn index_of(&self, addr: u64) -> Option<usize> {
        self.trbs.iter().position(|t| t.0 == addr)
    }
    /// Bytes moved when TRB `i` reported `residual` bytes not moved (all before it moved whole).
    pub(crate) fn moved(&self, i: usize, residual: u32) -> usize {
        let before: usize = self.trbs[..i].iter().map(|t| t.1 as usize).sum();
        before + (self.trbs[i].1.saturating_sub(residual)) as usize
    }
    pub(crate) fn is_urb(&self, u: &Arc<Urb>) -> bool {
        matches!(&self.owner, Owner::Urb { urb, .. } if Arc::ptr_eq(urb, u))
    }
    /// Marks the TD's buffer as possibly still used by a controller that did not stop (refused for new transfers),
    /// or free again.
    pub(crate) fn set_stuck(&self, on: bool) {
        if let Owner::Urb { stuck, .. } | Owner::Iso { stuck, .. } = &self.owner {
            stuck.store(on, Ordering::Release);
        }
    }
    pub(crate) fn is_pipe(&self) -> bool {
        matches!(self.owner, Owner::Pipe(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trb::{ty, CHAIN, IOC, ISP};

    #[test]
    fn pieces_cut_at_64k_boundaries() {
        // One segment starting 0x3000 below a boundary: 0x3000, then whole 64 KiB, then the rest.
        let p = pieces(&[(0x1_D000, 0x2_0000)], 0, 0x2_0000).unwrap();
        assert_eq!(p, [(0x1_D000, 0x3000), (0x2_0000, 0x1_0000), (0x3_0000, 0xD000)]);
        assert!(p.iter().all(|&(a, n)| a / TRB_BOUNDARY == (a + n as u64 - 1) / TRB_BOUNDARY));
        // Offset into the second of two segments; a range that ends inside it.
        let p = pieces(&[(0x10_0000, 0x1000), (0x20_F800, 0x1000)], 0x1100, 0x800).unwrap();
        assert_eq!(p, [(0x20_F900, 0x700), (0x21_0000, 0x100)]);
        assert_eq!(pieces(&[(0x5000, 64)], 0, 0).unwrap(), [(0x5000, 0)], "zero-length transfer: one empty TRB");
        assert_eq!(pieces(&[(0x5000, 64)], 60, 5), None);
        assert_eq!(pieces(&[(0x5000, 64)], usize::MAX, 2), None);
    }

    #[test]
    fn data_before_a_link_is_whole_packets() {
        // Three TRBs, one place before the link: the first (100 bytes into a page, 3996 bytes) is cut to 3584
        // (7 packets of 512); the 412 left go first after the link.
        let p = [(0x1_0064, 3996), (0x1_1000, 4096), (0x1_2000, 100)];
        let a = align_at_links(&p, 512, 1, 255).unwrap();
        assert_eq!(a, [(0x1_0064, 3584), (0x1_0064 + 3584, 412), (0x1_1000, 4096), (0x1_2000, 100)]);
        assert_eq!(a.iter().map(|x| x.1).sum::<u32>(), 8192);
        // Aligned already, room enough, or the TD ends right before the link: unchanged.
        assert_eq!(align_at_links(&[(0x1000, 1024), (0x2000, 10)], 512, 1, 255).unwrap(), [(0x1000, 1024), (0x2000, 10)]);
        assert_eq!(align_at_links(&p, 512, 3, 255).unwrap(), p);
        assert_eq!(align_at_links(&p[..1], 512, 1, 255).unwrap(), &p[..1]);
        // A TRB shorter than the odd part cannot be cut: the caller pads to the link.
        assert_eq!(align_at_links(&[(0x1000, 100), (0x2000, 100)], 512, 1, 255), None);
        // Several links: a segment of 2 places (per_seg 2) means a link after every second TRB.
        let p = [(0x1000, 700), (0x2000, 700), (0x3000, 700), (0x4000, 700)];
        let a = align_at_links(&p, 512, 2, 2).unwrap();
        let mut bytes = 0;
        let mut room = 2;
        for (i, x) in a.iter().enumerate() {
            bytes += x.1;
            room -= 1;
            if room == 0 && i + 1 < a.len() {
                assert_eq!(bytes % 512, 0, "link after TRB {i}");
                room = 2;
            }
        }
        assert_eq!(bytes, 2800);
    }

    #[test]
    fn trbs_chain_td_size_and_flags() {
        // 1100 bytes, MPS 512: 3 packets. TRBs 600 / 500: after the first 1 packet complete -> 2 left.
        let t = td_trbs(&[(0x1000, 600), (0x2000, 500)], 512, true, false);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].status, 600 | 2 << 17);
        assert_eq!(t[1].status, 500, "last TRB: TD Size 0");
        assert_eq!(t[0].control & (CHAIN | IOC | ISP), CHAIN | ISP);
        assert_eq!(t[1].control & (CHAIN | IOC | ISP), IOC | ISP);
        assert!(t.iter().all(|x| x.trb_type() == ty::NORMAL));
        // OUT: no ISP. TD Size saturates at 31.
        let big: Vec<(u64, u32)> = (0..20).map(|i| (0x10_0000 + i * 0x1_0000, 0x1_0000)).collect();
        let t = td_trbs(&big, 512, false, false);
        assert_eq!(t[0].status >> 17, 31);
        assert_eq!(t[18].status >> 17, 31, "128 packets left after TRB 18, saturated");
        assert!(t.iter().all(|x| x.control & ISP == 0));
        let z = td_trbs(&[(0x1000, 0)], 64, false, false);
        assert_eq!((z.len(), z[0].status, z[0].control & IOC), (1, 0, IOC));
    }

    #[test]
    fn td_size_in_kib_before_xhci_1_0() {
        // 0.96: KiB left including the TRB. 3 KiB + 2 KiB + 1000 bytes: 6120 bytes = 5 KiB, then 3048 bytes = 2 KiB, then 0.
        let t = td_trbs(&[(0x1000, 3072), (0x2000, 2048), (0x3000, 1000)], 512, true, true);
        assert_eq!(t.iter().map(|x| x.status >> 17).collect::<Vec<_>>(), [5, 2, 0]);
        assert_eq!(t[0].status & 0x1_FFFF, 3072);
        // Capped at 31 like the packet count.
        let big: Vec<(u64, u32)> = (0..4).map(|i| (0x10_0000 + i * 0x1_0000, 0x1_0000)).collect();
        assert_eq!(td_trbs(&big, 512, false, true)[0].status >> 17, 31);
        // 1.0 and later for the same TD: packets left after the TRB (12 packets in all).
        let t = td_trbs(&[(0x1000, 3072), (0x2000, 2048), (0x3000, 1000)], 512, true, false);
        assert_eq!(t.iter().map(|x| x.status >> 17).collect::<Vec<_>>(), [6, 2, 0]);
    }
}
