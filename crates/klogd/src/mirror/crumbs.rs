//! RB-16: the mirror as the breadcrumbs' wire store (`klog::crumb`). A crumb is one screen-stream line
//! (`crumb 7 device xhci0`) through the route card's `poll_tx`, then the card asked until it has sent it
//! (bounded), so the line is on the wire before the step it announces runs. While the start is open the line
//! joins its screen stream; after the end datagram (the listener retired that id) it goes under a stream id of
//! its own, which the listener shows after one "its start was missed" line. Lock-free and silent like the rest
//! of the raw path: route snapshot and card only tried, no allocation, no klog.

use super::wire::{self, CHUNK, FRAME_HEAD, FRAME_MAX, HEAD, Kind};
use super::{Mirror, RAW_RETRY_NS, RouteInfo, drop_wait};
use core::sync::atomic::Ordering;

/// How long a crumb waits for the card's "sent" (a card that cannot say gets `CRUMB_SETTLE_NS`).
pub const CRUMB_DONE_NS: u64 = 5_000_000;
pub const CRUMB_SETTLE_NS: u64 = 1_000_000;
/// The crumb stream's id after the end: the start's id with these bits flipped (never the start's own).
pub const CRUMB_ID_FLIP: u32 = 0xC0DE_0000;

klog::counter!(CRUMB_FRAMES, "mirror.crumb_frames");
klog::counter!(CRUMB_NOT_SENT, "mirror.crumb_not_sent");

impl Mirror {
    /// The id crumbs use: the start's while it is open, a stream of their own after the end datagram.
    pub fn crumb_stream(&self) -> (u32, bool) {
        if self.stopped.load(Ordering::Acquire) { (self.id() ^ CRUMB_ID_FLIP, true) } else { (self.id(), false) }
    }

    /// One crumb line out; false without a route or when the card refused it.
    pub fn send_crumb(&self, line: &[u8]) -> bool {
        let Some(r) = self.route() else { return false };
        let mut f = [0u8; FRAME_MAX];
        let n = {
            let body = &mut f[FRAME_HEAD + HEAD..FRAME_HEAD + HEAD + CHUNK];
            let n = line.len().min(body.len() - 1);
            body[..n].copy_from_slice(&line[..n]);
            body[n] = b'\n';
            n + 1
        };
        let (id, after_end) = self.crumb_stream();
        let pos = if after_end { &self.crumb_pos } else { &self.scr_pos }.fetch_add(n as u64, Ordering::AcqRel);
        wire::head(&mut f[FRAME_HEAD..], Kind::Screen, id, pos);
        if !self.send_frame(&r, &mut f, HEAD + n) {
            CRUMB_NOT_SENT.inc();
            return false;
        }
        CRUMB_FRAMES.inc();
        self.crumb_sent(&r);
        true
    }

    /// Asks the card until it has sent the crumb (bounded); then the frame's way to the wire.
    fn crumb_sent(&self, r: &RouteInfo) {
        let deadline = (self.clock)().saturating_add(CRUMB_DONE_NS);
        loop {
            match r.done.as_ref().and_then(|p| p.tx_done()) {
                Some(true) => return drop_wait(self.clock, super::DONE_SETTLE_NS),
                Some(false) if (self.clock)() < deadline => drop_wait(self.clock, RAW_RETRY_NS),
                _ => return drop_wait(self.clock, CRUMB_SETTLE_NS),
            }
        }
    }
}

impl klog::crumb::Store for Mirror {
    fn leave(&self, c: &klog::crumb::Crumb<'_>) -> bool {
        let mut b = [0u8; 96];
        let n = klog::crumb::line(c, &mut b);
        self.send_crumb(&b[..n])
    }
}
