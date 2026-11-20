//! The class requests a clock source takes, and the order Linux starts a UAC2 stream in. As values: nothing here sends.
//! `Setup` has the same fields as `usb_core::Setup`, which this crate does not depend on.

use crate::desc::ClockSource;

pub const CS_CUR: u8 = 0x01; // V:279
pub const CS_RANGE: u8 = 0x02; // V:280
pub const CONTROL_SAM_FREQ: u8 = 0x01; // V:301
pub const CONTROL_CLOCK_VALID: u8 = 0x02; // V:302

const IF_OUT: u8 = 0x21; // class, interface, host to device
const IF_IN: u8 = 0xA1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

/// C:465-469: the control in the high byte, and the clock's id above the interface's number.
const fn request(request_type: u8, request: u8, control: u8, iface: u8, clock: u8, length: u16) -> Setup {
    Setup { request_type, request, value: (control as u16) << 8, index: ((clock as u16) << 8) | iface as u16, length }
}

/// F:540: two bytes first, to learn the count.
pub const fn range_head(iface: u8, clock: u8) -> Setup {
    request(IF_IN, CS_RANGE, CONTROL_SAM_FREQ, iface, clock, 2)
}

/// F:575, with the length from `range::full_length`.
pub const fn range_full(iface: u8, clock: u8, length: u16) -> Setup {
    request(IF_IN, CS_RANGE, CONTROL_SAM_FREQ, iface, clock, length)
}

/// C:465: four bytes, unlike UAC1's three.
pub const fn get_rate(iface: u8, clock: u8) -> Setup {
    request(IF_IN, CS_CUR, CONTROL_SAM_FREQ, iface, clock, 4)
}

/// C:513-518.
pub const fn set_rate(iface: u8, clock: u8, rate: u32) -> (Setup, [u8; 4]) {
    (request(IF_OUT, CS_CUR, CONTROL_SAM_FREQ, iface, clock, 4), rate.to_le_bytes())
}

/// C:235-238.
pub const fn get_valid(iface: u8, clock: u8) -> Setup {
    request(IF_IN, CS_CUR, CONTROL_CLOCK_VALID, iface, clock, 1)
}

pub fn decode_rate(reply: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(reply.get(..4)?.try_into().ok()?))
}

/// C:231-248: a clock that cannot say whether it is valid counts as valid.
pub fn decode_valid(clock: &ClockSource, reply: Option<&[u8]>) -> bool {
    if !clock.validity_readable() {
        return true;
    }
    matches!(reply, Some([value, ..]) if *value != 0)
}

/// C:554: a clock already at this rate is left alone.
pub const fn needs_set(current: u32, wanted: u32) -> bool {
    current != wanted
}

/// One step of starting a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// SET_INTERFACE.
    Interface {
        iface: u8,
        alt: u8,
    },
    ReadRate(Setup),
    /// Only if `needs_set` says so.
    WriteRateIfDifferent(Setup, [u8; 4]),
    ReadValid(Setup),
}

/// E:1471-1503 with C:553-590: deselect, read the clock, set it, check it, and only then select the alternate setting.
/// UAC1 does it the other way round (`iface_first`, E:1477), which is why `usb-audio`'s order does not carry over.
pub const fn start_order(control_iface: u8, clock: u8, rate: u32, iface: u8, alt: u8) -> [Step; 5] {
    let (write, data) = set_rate(control_iface, clock, rate);
    [
        Step::Interface { iface, alt: 0 },
        Step::ReadRate(get_rate(control_iface, clock)),
        Step::WriteRateIfDifferent(write, data),
        Step::ReadValid(get_valid(control_iface, clock)),
        Step::Interface { iface, alt },
    ]
}
