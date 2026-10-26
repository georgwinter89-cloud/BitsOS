//! The card adapter: smoltcp's `Device` over a `NetDevice`. Received frames are copied once into the card's
//! scratch buffer (at most `rx_left` per round); a frame to send is built in a scratch buffer and handed to the
//! driver. A frame the TX ring has no room for is held and sent first in a later round (NW-7 fixer; before, it
//! was dropped after a 200 µs spin and TCP had to send it again after its 1 s timeout).

use crate::frames;
use alloc::vec::Vec;
use devmodel::net::NetDevice;
use devmodel::DevError;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

#[derive(Default)]
pub(crate) struct TxState {
    pub dropped: u32,
    /// The TX ring was full in this round: no more frames until the next one.
    pub full: bool,
    pub discovers: u32,
    /// A frame the full ring refused, sent before any other (Linux `dev_requeue_skb`,
    /// linux-6.6/net/sched/sch_generic.c:139: the qdisc keeps the packet while the driver's queue is stopped).
    pub held: Vec<u8>,
    pub holding: bool,
    /// Frames held this round (`net.tx_requeued`).
    pub requeued: u32,
}

impl TxState {
    /// Sends the held frame: true when the ring took it (or it is lost to a card without link), false while the
    /// ring is still full.
    fn flush(&mut self, dev: &dyn NetDevice) -> bool {
        if !self.holding {
            return true;
        }
        match dev.transmit(&self.held) {
            Err(DevError::Timeout) => {
                self.full = true;
                false
            }
            r => {
                if r.is_err() {
                    self.dropped += 1;
                }
                self.holding = false;
                true
            }
        }
    }

    /// The ring refused `frame`: hold it (one frame; a second one while holding is dropped, TCP sends it again).
    fn hold(&mut self, frame: &[u8]) {
        self.full = true;
        if self.holding || self.held.try_reserve(frame.len().saturating_sub(self.held.len())).is_err() {
            self.dropped += 1;
            return;
        }
        self.held.clear();
        self.held.extend_from_slice(frame);
        self.holding = true;
        self.requeued += 1;
    }
}

pub(crate) struct Phy<'a> {
    pub dev: &'a dyn NetDevice,
    pub rx: &'a mut [u8],
    pub tx: &'a mut Vec<u8>,
    pub st: &'a mut TxState,
    pub rx_left: usize,
    pub rx_frames: usize,
    pub mtu: usize,
}

pub(crate) struct RxTok<'a> {
    buf: &'a [u8],
}

impl RxToken for RxTok<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.buf)
    }
}

pub(crate) struct TxTok<'a> {
    dev: &'a dyn NetDevice,
    buf: &'a mut Vec<u8>,
    st: &'a mut TxState,
}

impl TxToken for TxTok<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.buf.clear();
        self.buf.resize(len, 0);
        let r = f(self.buf);
        if frames::is_dhcp_discover(self.buf) {
            self.st.discovers += 1;
        }
        // A held frame goes first (a reply token from `receive` can get here while one is held).
        if !self.st.flush(self.dev) {
            self.st.hold(self.buf);
            return r;
        }
        match self.dev.transmit(self.buf) {
            Ok(()) => {}
            // The ring is full: the poll thread comes back after its short busy pause (the round reports busy
            // while a frame is held) instead of spinning here under the card's lock.
            Err(DevError::Timeout) => self.st.hold(self.buf),
            // No link or a stopped card: the frame is lost like on a wire without carrier.
            Err(_) => self.st.dropped += 1,
        }
        r
    }
}

impl Device for Phy<'_> {
    type RxToken<'b>
        = RxTok<'b>
    where
        Self: 'b;
    type TxToken<'b>
        = TxTok<'b>
    where
        Self: 'b;

    fn receive(&mut self, _t: Instant) -> Option<(RxTok<'_>, TxTok<'_>)> {
        if self.rx_left == 0 {
            return None;
        }
        let n = self.dev.receive(self.rx)?;
        self.rx_left -= 1;
        self.rx_frames += 1;
        Some((RxTok { buf: &self.rx[..n.min(self.rx.len())] }, TxTok { dev: self.dev, buf: self.tx, st: self.st }))
    }

    fn transmit(&mut self, _t: Instant) -> Option<TxTok<'_>> {
        if self.st.full || !self.st.flush(self.dev) {
            return None;
        }
        Some(TxTok { dev: self.dev, buf: self.tx, st: self.st })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = self.mtu + 14;
        c.max_burst_size = None;
        c
    }
}
