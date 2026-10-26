//! Transfer Request Blocks (xHCI 1.2 §6.4): 16 bytes, the cycle bit in bit 0 of the control dword.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

pub const TRB_BYTES: usize = 16;

pub mod ty {
    pub const NORMAL: u8 = 1;
    pub const SETUP: u8 = 2;
    pub const DATA: u8 = 3;
    pub const STATUS: u8 = 4;
    /// Isoch TRB: the first TRB of an isochronous TD (xHCI 6.4.1.3).
    pub const ISOCH: u8 = 5;
    pub const LINK: u8 = 6;
    /// Transfer ring No-Op: a cancelled TD's TRBs become these (Linux `td_to_noop`).
    pub const TRANSFER_NOOP: u8 = 8;
    pub const ENABLE_SLOT: u8 = 9;
    pub const DISABLE_SLOT: u8 = 10;
    pub const ADDRESS_DEVICE: u8 = 11;
    pub const CONFIGURE_ENDPOINT: u8 = 12;
    pub const EVALUATE_CONTEXT: u8 = 13;
    pub const RESET_ENDPOINT: u8 = 14;
    pub const STOP_ENDPOINT: u8 = 15;
    pub const SET_TR_DEQUEUE: u8 = 16;
    pub const NOOP_COMMAND: u8 = 23;
    pub const TRANSFER_EVENT: u8 = 32;
    pub const COMMAND_COMPLETION: u8 = 33;
    pub const PORT_STATUS_CHANGE: u8 = 34;
    pub const BANDWIDTH_REQUEST: u8 = 35;
    pub const DOORBELL_EVENT: u8 = 36;
    pub const HOST_CONTROLLER: u8 = 37;
    pub const DEVICE_NOTIFICATION: u8 = 38;
    pub const MFINDEX_WRAP: u8 = 39;
}

/// Completion codes (§6.4.5).
pub mod cc {
    pub const INVALID: u8 = 0;
    pub const SUCCESS: u8 = 1;
    pub const DATA_BUFFER: u8 = 2;
    pub const BABBLE: u8 = 3;
    pub const USB_TRANSACTION: u8 = 4;
    pub const TRB_ERROR: u8 = 5;
    pub const STALL: u8 = 6;
    pub const RESOURCE: u8 = 7;
    pub const BANDWIDTH: u8 = 8;
    pub const NO_SLOTS: u8 = 9;
    pub const SLOT_NOT_ENABLED: u8 = 11;
    pub const ENDPOINT_NOT_ENABLED: u8 = 12;
    pub const SHORT_PACKET: u8 = 13;
    /// Isochronous OUT ring empty when the controller wanted the next TD (xHCI 4.10.3.1).
    pub const RING_UNDERRUN: u8 = 14;
    /// Isochronous IN ring empty when data came (xHCI 4.10.3.1).
    pub const RING_OVERRUN: u8 = 15;
    pub const BANDWIDTH_OVERRUN: u8 = 18;
    /// An isochronous TD's service interval passed before it ran (xHCI 4.10.3.2).
    pub const MISSED_SERVICE: u8 = 23;
    /// The device sent more than the isochronous TD's buffer (xHCI 4.10.3.3).
    pub const ISOCH_BUFFER_OVERRUN: u8 = 31;
    pub const SPLIT_TRANSACTION: u8 = 36;
    pub const EVENT_RING_FULL: u8 = 21;
    pub const COMMAND_RING_STOPPED: u8 = 24;
    pub const COMMAND_ABORTED: u8 = 25;
    pub const STOPPED: u8 = 26;
    pub const STOPPED_LENGTH_INVALID: u8 = 27;
    /// xHCI 1.1+: stopped after a short packet in the TD.
    pub const STOPPED_SHORT_PACKET: u8 = 28;
}

pub const CYCLE: u32 = 1 << 0;
/// Link TRB: Toggle Cycle.
pub const LINK_TC: u32 = 1 << 1;
/// Transfer TRB flags (xHCI §6.4.1).
pub const ISP: u32 = 1 << 2;
pub const CHAIN: u32 = 1 << 4;
pub const IOC: u32 = 1 << 5;
pub const IDT: u32 = 1 << 6;
/// Block Event Interrupt: the event of this IOC is written without an interrupt (xHCI 6.4.1.1).
pub const BEI: u32 = 1 << 9;
/// Isoch TRB: Start Isoch ASAP (xHCI 6.4.1.3; Linux `TRB_SIA`).
pub const SIA: u32 = 1 << 31;
/// Address Device: Block Set Address Request (xHCI §6.4.3.4).
pub const ADDRESS_BSR: u32 = 1 << 9;
/// Data/Status Stage direction: IN.
pub const DIR_IN: u32 = 1 << 16;
/// Setup Stage Transfer Type: no data, OUT data, IN data.
pub const TRT_NONE: u32 = 0;
pub const TRT_OUT: u32 = 2 << 16;
pub const TRT_IN: u32 = 3 << 16;
/// Transfer Event: the event carries Event Data, not a TRB pointer.
pub const EVENT_DATA: u32 = 1 << 2;

impl Trb {
    pub fn trb_type(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }
    pub fn cycle(&self) -> bool {
        self.control & CYCLE != 0
    }
    /// Completion code of an event TRB.
    pub fn code(&self) -> u8 {
        (self.status >> 24) as u8
    }
    /// Slot ID of an event TRB (command completion, transfer).
    pub fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }
    /// Endpoint ID (DCI) of a transfer event.
    pub fn endpoint(&self) -> u8 {
        ((self.control >> 16) & 0x1F) as u8
    }
    /// Port ID of a port status change event.
    pub fn port(&self) -> u8 {
        (self.param >> 24) as u8
    }

    fn with_type(t: u8) -> Trb {
        Trb { param: 0, status: 0, control: (t as u32) << 10 }
    }
    pub fn noop_command() -> Trb {
        Trb::with_type(ty::NOOP_COMMAND)
    }
    pub fn enable_slot(slot_type: u8) -> Trb {
        let mut t = Trb::with_type(ty::ENABLE_SLOT);
        t.control |= ((slot_type & 0x1F) as u32) << 16;
        t
    }
    pub fn disable_slot(slot: u8) -> Trb {
        Trb::slot_cmd(ty::DISABLE_SLOT, slot)
    }
    fn slot_cmd(t: u8, slot: u8) -> Trb {
        let mut c = Trb::with_type(t);
        c.control |= (slot as u32) << 24;
        c
    }
    fn input_cmd(t: u8, input: u64, slot: u8) -> Trb {
        let mut c = Trb::slot_cmd(t, slot);
        c.param = input;
        c
    }
    fn ep_cmd(t: u8, slot: u8, dci: u8) -> Trb {
        let mut c = Trb::slot_cmd(t, slot);
        c.control |= ((dci & 0x1F) as u32) << 16;
        c
    }
    /// Address Device. BSR 0: the controller sends SET_ADDRESS; BSR 1 (Block Set Address Request, control
    /// bit 9): the slot only reaches the Default state.
    pub fn address_device(input: u64, slot: u8, bsr: bool) -> Trb {
        let mut c = Trb::input_cmd(ty::ADDRESS_DEVICE, input, slot);
        if bsr {
            c.control |= ADDRESS_BSR;
        }
        c
    }
    pub fn configure_endpoint(input: u64, slot: u8) -> Trb {
        Trb::input_cmd(ty::CONFIGURE_ENDPOINT, input, slot)
    }
    pub fn evaluate_context(input: u64, slot: u8) -> Trb {
        Trb::input_cmd(ty::EVALUATE_CONTEXT, input, slot)
    }
    pub fn reset_endpoint(slot: u8, dci: u8) -> Trb {
        Trb::ep_cmd(ty::RESET_ENDPOINT, slot, dci)
    }
    pub fn stop_endpoint(slot: u8, dci: u8) -> Trb {
        Trb::ep_cmd(ty::STOP_ENDPOINT, slot, dci)
    }
    /// Set TR Dequeue Pointer to `ptr` with cycle state `cycle` (stream 0).
    pub fn set_tr_dequeue(slot: u8, dci: u8, ptr: u64, cycle: bool) -> Trb {
        let mut c = Trb::ep_cmd(ty::SET_TR_DEQUEUE, slot, dci);
        c.param = (ptr & !0xF) | cycle as u64;
        c
    }
    /// Setup Stage: the 8-byte packet as immediate data.
    pub fn setup_stage(packet: u64, trt: u32) -> Trb {
        Trb { param: packet, status: 8, control: (ty::SETUP as u32) << 10 | IDT | trt }
    }
    /// Data Stage of `len` bytes at `buf`; IN data gets ISP so a short packet is reported.
    pub fn data_stage(buf: u64, len: u32, dir_in: bool) -> Trb {
        let flags = if dir_in { DIR_IN | ISP } else { 0 };
        Trb { param: buf, status: len & 0x1_FFFF, control: (ty::DATA as u32) << 10 | flags }
    }
    /// Status Stage with IOC: IN when there was no data or the data went out, OUT after IN data.
    pub fn status_stage(dir_in: bool) -> Trb {
        Trb { param: 0, status: 0, control: (ty::STATUS as u32) << 10 | IOC | if dir_in { DIR_IN } else { 0 } }
    }
    /// One Normal TRB with IOC and ISP (an interrupt IN transfer of `len` bytes).
    pub fn normal_in(buf: u64, len: u32) -> Trb {
        Trb::normal(buf, len, 0, true, true)
    }
    /// A Normal TRB of a TD: `td_size` = packets left in the TD after this TRB (xHCI 4.11.2.4, at most 31);
    /// `last` gets IOC, every other one CH; IN TRBs get ISP so a short packet ends the TD with an event.
    pub fn normal(buf: u64, len: u32, td_size: u32, last: bool, dir_in: bool) -> Trb {
        let mut control = (ty::NORMAL as u32) << 10 | if last { IOC } else { CHAIN };
        if dir_in {
            control |= ISP;
        }
        Trb { param: buf, status: (len & 0x1_FFFF) | td_size.min(31) << 17, control }
    }
    /// Turns the first Normal TRB of a TD (`td_trbs`) into its Isoch TRB (xHCI 6.4.1.3, Linux
    /// `xhci_queue_isoc_tx`): Transfer Burst Count (bits 8:7), Transfer Last Burst Packet Count (bits 19:16), and
    /// either Start Isoch ASAP or the Frame ID (bits 30:20) of the bus frame the TD belongs to.
    pub fn into_isoch(self, tbc: u32, tlbpc: u32, frame: Option<u16>) -> Trb {
        let start = match frame {
            Some(f) => ((f & 0x7FF) as u32) << 20,
            None => SIA,
        };
        let control = (self.control & !(0x3F << 10 | 3 << 7 | 0xF << 16 | 0x7FF << 20 | SIA))
            | (ty::ISOCH as u32) << 10 | (tbc & 3) << 7 | (tlbpc & 0xF) << 16 | start;
        Trb { control, ..self }
    }
    /// A Transfer No-Op without IOC (padding up to a Link TRB; the controller passes it silently).
    pub fn transfer_noop() -> Trb {
        Trb::with_type(ty::TRANSFER_NOOP)
    }
    /// A Link TRB to `target`; `toggle` on the last segment's link.
    pub fn link(target: u64, toggle: bool) -> Trb {
        let mut t = Trb::with_type(ty::LINK);
        t.param = target;
        if toggle {
            t.control |= LINK_TC;
        }
        t
    }
}

/// How a transfer ended, as the controller reports it in a Transfer Event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferStatus {
    Complete,
    /// Fewer bytes than asked; `residual` bytes were not transferred.
    Short { residual: u32 },
    Error(u8),
}

/// Classifies a Transfer Event. **AMD quirk** (V1 `xhcievent.asm:146-176`, Linux `XHCI_TRUST_TX_LENGTH`): a short
/// data stage arrives either as Short Packet or as Success with a non-zero residual; both mean short.
pub fn classify_transfer(ev: &Trb) -> TransferStatus {
    let residual = ev.status & 0x00FF_FFFF;
    match ev.code() {
        cc::SUCCESS if residual != 0 => TransferStatus::Short { residual },
        cc::SUCCESS => TransferStatus::Complete,
        cc::SHORT_PACKET => TransferStatus::Short { residual },
        c => TransferStatus::Error(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: u8, residual: u32) -> Trb {
        Trb { param: 0x1000, status: (code as u32) << 24 | residual, control: (ty::TRANSFER_EVENT as u32) << 10 | 1 }
    }

    #[test]
    fn amd_residue_quirk() {
        assert_eq!(classify_transfer(&ev(cc::SUCCESS, 0)), TransferStatus::Complete);
        assert_eq!(classify_transfer(&ev(cc::SUCCESS, 46)), TransferStatus::Short { residual: 46 }, "AMD: Success with residue");
        assert_eq!(classify_transfer(&ev(cc::SHORT_PACKET, 46)), TransferStatus::Short { residual: 46 });
        assert_eq!(classify_transfer(&ev(cc::STALL, 0)), TransferStatus::Error(cc::STALL));
    }

    #[test]
    fn isoch_trb_packing() {
        // Full speed audio packet of 192 bytes, OUT, one TRB: type 5, IOC, TLBPC 0 (one packet), SIA, no CH/ISP.
        let t = Trb::normal(0x12_3440, 192, 0, true, false).into_isoch(0, 0, None);
        assert_eq!(t.trb_type(), ty::ISOCH);
        assert_eq!(t.control, (ty::ISOCH as u32) << 10 | IOC | SIA);
        assert_eq!((t.param, t.status), (0x12_3440, 192));
        // Frame ID 0x7FF at bits 30:20, no SIA; TBC 2 and TLBPC 15 in their fields; IN keeps ISP and CH.
        let t = Trb::normal(0x5000, 3072, 4, false, true).into_isoch(2, 15, Some(0xFFFF));
        assert_eq!(t.control, (ty::ISOCH as u32) << 10 | CHAIN | ISP | 2 << 7 | 15 << 16 | 0x7FF << 20);
        assert_eq!(t.status, 3072 | 4 << 17, "TD Size untouched");
        assert_eq!(t.control & CYCLE, 0, "the cycle bit is the ring's to set");
    }

    #[test]
    fn fields_and_constructors() {
        let t = Trb { param: 7 << 24, status: 1 << 24, control: 3 << 24 | (ty::PORT_STATUS_CHANGE as u32) << 10 | 5 << 16 | 1 };
        assert_eq!((t.trb_type(), t.code(), t.slot(), t.port(), t.endpoint(), t.cycle()), (34, 1, 3, 7, 5, true));
        assert_eq!(Trb::noop_command().trb_type(), ty::NOOP_COMMAND);
        let l = Trb::link(0x2000, true);
        assert_eq!((l.trb_type(), l.param, l.control & LINK_TC), (ty::LINK, 0x2000, LINK_TC));
        assert_eq!(Trb::enable_slot(3).control >> 16 & 0x1F, 3);
        assert_eq!(Trb::disable_slot(9).slot(), 9);
        let s = Trb::setup_stage(0x0008_0000_0100_0680, TRT_IN);
        assert_eq!((s.trb_type(), s.status, s.control & (IDT | TRT_IN)), (ty::SETUP, 8, IDT | TRT_IN));
        let d = Trb::data_stage(0x5000, 18, true);
        assert_eq!((d.trb_type(), d.status, d.control & (DIR_IN | ISP)), (ty::DATA, 18, DIR_IN | ISP));
        assert_eq!(Trb::status_stage(false).control & (DIR_IN | IOC), IOC);
        let q = Trb::set_tr_dequeue(3, 1, 0x7010, true);
        assert_eq!((q.param, q.slot(), q.control >> 16 & 0x1F), (0x7011, 3, 1));
        assert_eq!(Trb::reset_endpoint(4, 3).control >> 16 & 0x1F, 3);
        assert_eq!(Trb::address_device(0x9000, 2, false).param, 0x9000);
        assert_eq!(Trb::address_device(0x9000, 2, false).control & ADDRESS_BSR, 0);
        assert_eq!(Trb::address_device(0x9000, 2, true).control, (ty::ADDRESS_DEVICE as u32) << 10 | 2 << 24 | 1 << 9);
    }
}
