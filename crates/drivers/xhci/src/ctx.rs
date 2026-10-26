//! Slot and endpoint contexts (xHCI 1.2 §6.2; V1 `xhcictx.asm`). Every offset is a multiple of the context size
//! (32 bytes in QEMU, 64 on the Mini-PC); the fields inside a context are the same for both. The field math is
//! pure (host-tested); `Ctx` writes and reads it in an input or output context buffer.

use crate::mem::DmaMem;
use crate::regs::ContextSize;
use usb_core::{Endpoint, EpKind, Speed};

/// Endpoint context states (dword 0 bits 2:0).
pub mod ep_state {
    pub const DISABLED: u32 = 0;
    pub const RUNNING: u32 = 1;
    pub const HALTED: u32 = 2;
    pub const STOPPED: u32 = 3;
    pub const ERROR: u32 = 4;
}

/// Endpoint types (dword 1 bits 5:3).
pub mod ep_type {
    pub const ISOCH_OUT: u32 = 1;
    pub const BULK_OUT: u32 = 2;
    pub const INTERRUPT_OUT: u32 = 3;
    pub const CONTROL: u32 = 4;
    pub const ISOCH_IN: u32 = 5;
    pub const BULK_IN: u32 = 6;
    pub const INTERRUPT_IN: u32 = 7;
}

pub const SLOT_MTT: u32 = 1 << 25;
pub const SLOT_HUB: u32 = 1 << 26;
/// Average TRB length of a bulk endpoint (xHCI §4.14.1.1: 3 KB; V1 `bulk.asm`).
pub const BULK_AVG_TRB: u32 = 3072;

/// Slot context dword 0: route string, speed, MTT, Hub, Context Entries.
pub fn slot_dw0(route: u32, speed: Speed, mtt: bool, hub: bool, entries: u8) -> u32 {
    (route & 0xF_FFFF)
        | (speed.xhci_psiv() as u32) << 20
        | if mtt { SLOT_MTT } else { 0 }
        | if hub { SLOT_HUB } else { 0 }
        | ((entries & 0x1F) as u32) << 27
}
/// Slot context dword 1: root hub port number, number of ports (hubs).
pub fn slot_dw1(root_port: u8, ports: u8) -> u32 {
    (root_port as u32) << 16 | (ports as u32) << 24
}
/// Slot context dword 2: TT hub slot, TT port, TT think time (interrupter target 0).
pub fn slot_dw2(tt_slot: u8, tt_port: u8, think: u8) -> u32 {
    tt_slot as u32 | (tt_port as u32) << 8 | ((think & 3) as u32) << 16
}

/// The Interval field (2^n x 125 us) of an endpoint (Linux `xhci_get_endpoint_interval`, V1 `xhci_interval`):
/// high and super speed periodic endpoints give the exponent + 1 directly (n = bInterval - 1, 0..15); full speed
/// isochronous gives it in frames (n = bInterval - 1 + 3); full/low speed interrupt gives milliseconds
/// (n = floor(log2(bInterval * 8)), 3..10). Control and bulk use 0.
pub fn interval(speed: Speed, ep: &Endpoint) -> u8 {
    let b = ep.interval;
    let exp = |v: u8| v.clamp(1, 16) - 1;
    match (speed, ep.kind()) {
        (_, EpKind::Control | EpKind::Bulk) => 0,
        (Speed::High | Speed::Super | Speed::SuperPlus, _) => exp(b),
        (Speed::Full, EpKind::Isoch) => exp(b) + 3,
        (_, _) => {
            let micro = (b.max(1) as u32) * 8;
            ((31 - micro.leading_zeros()) as u8).clamp(3, 10)
        }
    }
}

fn periodic(ep: &Endpoint) -> bool {
    matches!(ep.kind(), EpKind::Interrupt | EpKind::Isoch)
}

/// Max Burst Size: SuperSpeed from the companion, high speed periodic from wMaxPacketSize bits 12:11.
pub fn max_burst(speed: Speed, ep: &Endpoint) -> u8 {
    match speed {
        Speed::Super | Speed::SuperPlus => ep.companion.map(|c| c.max_burst.min(15)).unwrap_or(0),
        Speed::High if periodic(ep) => ep.hs_mult(),
        _ => 0,
    }
}

/// Max ESIT Payload of a periodic endpoint (bytes per service interval; V1 `xhci_ctx_intr`, Linux
/// `xhci_get_max_esit_payload`): packet size x (burst + 1) x (mult + 1) (spec usb.md §3.3). SuperSpeed takes the
/// companion's wBytesPerInterval when it lies within that (a device may reserve less); a companion saying 0 or
/// more than the endpoint can move gets the full value (a periodic endpoint with payload 0 is refused by
/// controllers with a Parameter Error; Linux's descriptor parser caps it the same way). 0 for other endpoints.
pub fn max_esit_payload(speed: Speed, ep: &Endpoint) -> u32 {
    if !periodic(ep) {
        return 0;
    }
    let full = ep.max_packet() as u32 * (max_burst(speed, ep) as u32 + 1) * (ss_mult(speed, ep) + 1);
    match ep.companion {
        Some(c) if speed.is_superspeed() && c.bytes_per_interval != 0 => (c.bytes_per_interval as u32).min(full),
        _ => full,
    }
}

/// Mult of a SuperSpeed isochronous endpoint (companion bmAttributes bits 1:0); 0 for everything else.
fn ss_mult(speed: Speed, ep: &Endpoint) -> u32 {
    match ep.companion {
        Some(c) if speed.is_superspeed() && ep.kind() == EpKind::Isoch => (c.attributes & 3) as u32,
        _ => 0,
    }
}

fn ep_type(ep: &Endpoint) -> u32 {
    match (ep.kind(), ep.is_in()) {
        (EpKind::Control, _) => ep_type::CONTROL,
        (EpKind::Isoch, false) => ep_type::ISOCH_OUT,
        (EpKind::Isoch, true) => ep_type::ISOCH_IN,
        (EpKind::Bulk, false) => ep_type::BULK_OUT,
        (EpKind::Bulk, true) => ep_type::BULK_IN,
        (EpKind::Interrupt, false) => ep_type::INTERRUPT_OUT,
        (EpKind::Interrupt, true) => ep_type::INTERRUPT_IN,
    }
}

/// Dwords 0..4 of an endpoint context for `ep` with its ring at `deq` (cycle state 1).
pub fn ep_context(speed: Speed, ep: &Endpoint, deq: u64) -> [u32; 5] {
    let esit = max_esit_payload(speed, ep);
    let isoch = ep.kind() == EpKind::Isoch;
    let mult = ss_mult(speed, ep);
    let cerr = if isoch { 0 } else { 3 };
    let mps = match (speed.is_superspeed(), ep.kind()) {
        (true, EpKind::Control) => 512,
        _ => ep.max_packet() as u32,
    };
    // Periodic: one service interval's payload per TRB, at least 1 (a zero average is a Parameter Error).
    let avg = match ep.kind() {
        EpKind::Control => 8,
        EpKind::Bulk => BULK_AVG_TRB,
        _ => esit.clamp(1, 0xFFFF),
    };
    [
        mult << 8 | (interval(speed, ep) as u32) << 16 | (esit >> 16) << 24,
        cerr << 1 | ep_type(ep) << 3 | (max_burst(speed, ep) as u32) << 8 | mps << 16,
        (deq as u32 & !0xF) | 1,
        (deq >> 32) as u32,
        avg | (esit & 0xFFFF) << 16,
    ]
}

/// Endpoint 0's context: control, 3 errors, `mps0`, its ring from the start, average TRB length 8
/// (V1 `xhci_ep0_ctx`).
pub fn ep0_context(mps0: u16, deq: u64) -> [u32; 5] {
    [0, 3 << 1 | ep_type::CONTROL << 3 | (mps0 as u32) << 16, (deq as u32 & !0xF) | 1, (deq >> 32) as u32, 8]
}

/// A context buffer: an input context (control context first, then slot and endpoints shifted by one) or an
/// output (device) context.
pub struct Ctx<'a, M: DmaMem> {
    pub mem: &'a M,
    pub csz: ContextSize,
    pub input: bool,
}

impl<M: DmaMem> Ctx<'_, M> {
    /// Byte offset of dword `dw` of context `index` (0 = slot, n = DCI n).
    fn off(&self, index: usize, dw: usize) -> usize {
        let base = if self.input { self.csz.input_offset(index) } else { self.csz.device_offset(index) };
        base + 4 * dw
    }
    pub fn get(&self, index: usize, dw: usize) -> Option<u32> {
        self.mem.rd32(self.off(index, dw))
    }
    pub fn set(&self, index: usize, dw: usize, v: u32) -> bool {
        self.mem.wr32(self.off(index, dw), v)
    }
    pub fn set_all(&self, index: usize, dws: &[u32]) -> bool {
        dws.iter().enumerate().all(|(i, &v)| self.set(index, i, v))
    }
    /// Input control context: drop flags (dword 0) and add flags (dword 1).
    pub fn set_flags(&self, drop: u32, add: u32) -> bool {
        debug_assert!(self.input);
        self.mem.wr32(0, drop) && self.mem.wr32(4, add)
    }
    /// The eight defined dwords of a context (the rest of a 64-byte context is reserved).
    pub fn read8(&self, index: usize) -> Option<[u32; 8]> {
        let mut v = [0u32; 8];
        for (i, d) in v.iter_mut().enumerate() {
            *d = self.get(index, i)?;
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usb_core::SsCompanion;

    fn ep(address: u8, attributes: u8, mps: u16, interval: u8) -> Endpoint {
        Endpoint { address, attributes, max_packet_raw: mps, interval, companion: None }
    }

    #[test]
    fn audio_isochronous_contexts() {
        // JBL Quantum 800 (full speed): game OUT 0x02 288 bytes, chat 0x06 OUT and mic 0x81 IN 144 bytes, 1 ms.
        let game = ep(0x02, 0x0D, 0x0120, 1);
        let c = ep_context(Speed::Full, &game, 0x1_2340);
        assert_eq!(c[0], 3 << 16, "mult 0, interval 3 (2^3 x 125 us), ESIT high 0");
        assert_eq!(c[1], ep_type::ISOCH_OUT << 3 | 288 << 16, "CErr 0, burst 0");
        assert_eq!(c[4], 288 | 288 << 16, "average TRB = Max ESIT Payload = 288");
        let mic = ep_context(Speed::Full, &ep(0x81, 0x0D, 0x0090, 1), 0);
        assert_eq!((mic[1] >> 3 & 7, mic[1] >> 16, mic[4]), (ep_type::ISOCH_IN, 144, 144 | 144 << 16));
        // High speed, 3 x 1024 per microframe: burst field 2 (Linux puts the HS mult into Max Burst), Mult 0
        // (SuperSpeed only, Linux `xhci_get_endpoint_mult`), ESIT 3072.
        let hs = ep_context(Speed::High, &ep(0x81, 0x05, 0x1400, 1), 0);
        assert_eq!((hs[0] >> 8 & 3, hs[0] >> 16 & 0xFF, hs[1] >> 8 & 0xFF, hs[4] >> 16), (0, 0, 2, 3072));
        // SuperSpeed isochronous: Mult from the companion's bmAttributes, ESIT high byte past 64 KiB.
        let c = SsCompanion { max_burst: 15, attributes: 2, bytes_per_interval: 0 };
        let ss = ep_context(Speed::Super, &Endpoint { companion: Some(c), ..ep(0x81, 0x05, 1024, 1) }, 0);
        assert_eq!((ss[0] >> 8 & 3, ss[0] >> 24, ss[4] >> 16), (2, 0, 1024 * 16 * 3 & 0xFFFF));
        assert_eq!(max_esit_payload(Speed::Super, &Endpoint { companion: Some(c), ..ep(0x81, 0x05, 1024, 1) }), 49152);
    }

    #[test]
    fn intervals_per_speed() {
        let int_in = |i| ep(0x81, 3, 8, i);
        // Full/low speed interrupt: bInterval ms -> floor(log2(ms * 8)), clamped 3..10.
        assert_eq!(interval(Speed::Full, &int_in(1)), 3);
        assert_eq!(interval(Speed::Full, &int_in(10)), 6, "the DragonRise pad's 10 ms -> 8 ms");
        assert_eq!(interval(Speed::Low, &int_in(255)), 10);
        assert_eq!(interval(Speed::Low, &int_in(0)), 3, "0 is read as 1");
        // High/super speed: n = bInterval - 1, 0..15.
        assert_eq!(interval(Speed::High, &int_in(4)), 3);
        assert_eq!(interval(Speed::Super, &int_in(1)), 0);
        assert_eq!(interval(Speed::High, &int_in(200)), 15);
        assert_eq!(interval(Speed::High, &int_in(0)), 0);
        // Full speed isochronous: frames -> + 3.
        assert_eq!(interval(Speed::Full, &ep(0x83, 1, 192, 1)), 3);
        assert_eq!(interval(Speed::Full, &ep(0x83, 1, 192, 4)), 6);
        // Bulk and control: 0 (a high speed bulk NAK rate is not an interval).
        assert_eq!(interval(Speed::High, &ep(0x02, 2, 512, 255)), 0);
    }

    #[test]
    fn endpoint_contexts() {
        // FS interrupt IN, 8 bytes, 10 ms.
        let c = ep_context(Speed::Full, &ep(0x81, 3, 8, 10), 0x1234_5000);
        assert_eq!(c[0], 6 << 16);
        assert_eq!(c[1], 3 << 1 | ep_type::INTERRUPT_IN << 3 | 8 << 16);
        assert_eq!((c[2], c[3]), (0x1234_5001, 0));
        assert_eq!(c[4], 8 | 8 << 16, "average TRB length = ESIT payload = one report");
        // HS interrupt with two extra transactions per microframe: burst 2, ESIT 3 x 1024.
        let hs = ep(0x81, 3, 1024 | 2 << 11, 1);
        assert_eq!(max_burst(Speed::High, &hs), 2);
        assert_eq!(max_esit_payload(Speed::High, &hs), 3072);
        let c = ep_context(Speed::High, &hs, 0x1_0000_0000);
        assert_eq!((c[1] >> 8) & 0xFF, 2);
        assert_eq!(c[1] >> 16, 1024, "bits 12:11 are not part of the packet size");
        assert_eq!(c[3], 1, "high dword of the dequeue pointer");
        // SS bulk with a companion: burst 15, average 3 KB, no ESIT.
        let mut b = ep(0x82, 2, 1024, 0);
        b.companion = Some(SsCompanion { max_burst: 15, attributes: 0, bytes_per_interval: 0 });
        let c = ep_context(Speed::Super, &b, 0x4000);
        assert_eq!(c[1], 3 << 1 | ep_type::BULK_IN << 3 | 15 << 8 | 1024 << 16);
        assert_eq!(c[4], BULK_AVG_TRB);
        // SS interrupt: ESIT from wBytesPerInterval, high byte into dword 0.
        let mut s = ep(0x83, 3, 1024, 1);
        s.companion = Some(SsCompanion { max_burst: 0, attributes: 0, bytes_per_interval: 0x400 });
        assert_eq!(max_esit_payload(Speed::Super, &s), 0x400);
        // A companion saying 0 bytes per interval (seen on SuperSpeed hubs and HID devices): packet size x burst,
        // never a zero payload or zero average TRB length.
        let mut z = ep(0x81, 3, 2, 8);
        z.companion = Some(SsCompanion { max_burst: 0, attributes: 0, bytes_per_interval: 0 });
        assert_eq!(max_esit_payload(Speed::Super, &z), 2);
        let c = ep_context(Speed::Super, &z, 0x4000);
        assert_eq!((c[4] & 0xFFFF, c[4] >> 16, c[0] >> 24), (2, 2, 0));
        z.companion = Some(SsCompanion { max_burst: 1, attributes: 0, bytes_per_interval: 0 });
        assert_eq!(max_esit_payload(Speed::Super, &z), 4, "burst counts");
        // More than the endpoint can move: capped. Less: taken (the device reserves less).
        z.companion = Some(SsCompanion { max_burst: 0, attributes: 0, bytes_per_interval: 0x8000 });
        assert_eq!(max_esit_payload(Speed::Super, &z), 2);
        s.companion = Some(SsCompanion { max_burst: 0, attributes: 0, bytes_per_interval: 8 });
        assert_eq!(max_esit_payload(Speed::Super, &s), 8);
        // SuperSpeed isochronous: mult counts in the full value.
        let mut iso = ep(0x84, 1, 1024, 1);
        iso.companion = Some(SsCompanion { max_burst: 1, attributes: 2, bytes_per_interval: 0 });
        assert_eq!(max_esit_payload(Speed::Super, &iso), 1024 * 2 * 3);
        // Bulk OUT and isochronous: CErr 3 and 0.
        assert_eq!(ep_context(Speed::High, &ep(0x01, 2, 512, 0), 0)[1] & 6, 6);
        assert_eq!(ep_context(Speed::High, &ep(0x01, 1, 512, 1), 0)[1] & 6, 0);
        let e0 = ep0_context(64, 0xABC0);
        assert_eq!((e0[1], e0[2], e0[4]), (3 << 1 | 4 << 3 | 64 << 16, 0xABC1, 8));
    }

    #[test]
    fn slot_context_fields() {
        assert_eq!(slot_dw0(0x41, Speed::Low, true, false, 1), 0x41 | 2 << 20 | SLOT_MTT | 1 << 27);
        assert_eq!(slot_dw0(0x1F_FFFF, Speed::High, false, true, 31), 0xF_FFFF | 3 << 20 | SLOT_HUB | 31 << 27);
        assert_eq!(slot_dw1(6, 4), 6 << 16 | 4 << 24);
        assert_eq!(slot_dw2(5, 3, 2), 5 | 3 << 8 | 2 << 16);
    }
}
