//! HID class pieces without hardware: the HID descriptor (HID 1.11 §6.2.1), the class requests (§7.2) and
//! names (Linux `hidinput_allocate`, linux-6.6/drivers/hid/hid-input.c:2007-2075).

use alloc::format;
use alloc::string::String;
use hidparse::{ReportDescriptor, ReportKind};
use usb_core::{Setup, Speed};

pub const DT_HID: u8 = 0x21;
pub const DT_REPORT: u8 = 0x22;
pub const REQ_GET_REPORT: u8 = 0x01;
pub const REQ_SET_REPORT: u8 = 0x09;
pub const REQ_SET_IDLE: u8 = 0x0A;
pub const REQ_SET_PROTOCOL: u8 = 0x0B;
pub const PROTOCOL_BOOT: u16 = 0;
pub const PROTOCOL_REPORT: u16 = 1;
/// Report types in wValue's high byte of GET/SET_REPORT.
pub const REPORT_OUTPUT: u16 = 2;
pub const REPORT_FEATURE: u16 = 3;
/// Interface subclass 1 = supports the boot protocol; protocol 1 keyboard, 2 mouse.
pub const SUBCLASS_BOOT: u8 = 1;
pub const PROTOCOL_KEYBOARD: u8 = 1;
pub const PROTOCOL_MOUSE: u8 = 2;
/// Linux `HID_MAX_BUFFER_SIZE` (include/linux/hid.h): the longest report a device may send; a bound on
/// hostile input, not a count of things.
pub const MAX_REPORT_BYTES: usize = 16384;
/// Memory one interface's report state may take (`state_bytes`): a bound on hostile descriptors. A real
/// keyboard needs a few hundred bytes, a gamepad a few KiB.
pub const MAX_STATE_BYTES: u64 = 4 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidDescriptor {
    pub version: u16,
    pub country: u8,
    /// wDescriptorLength of the (last) report descriptor entry, as Linux `usbhid_parse` takes it.
    pub report_len: u16,
}

/// Linux `HID_MAX_DESCRIPTOR_SIZE` (include/uapi/linux/hid.h:78): longer report descriptors are refused.
pub const MAX_DESCRIPTOR_BYTES: u16 = 4096;

/// The HID descriptor among an interface's class-specific descriptors; None if there is none or it names no
/// report descriptor or one of 0 or more than 4096 bytes (Linux refuses the interface then, usbhid_parse,
/// linux-6.6/drivers/hid/usbhid/hid-core.c:1027-1030; a boot interface falls back to the boot protocol).
pub fn hid_descriptor(extra: &[u8]) -> Option<HidDescriptor> {
    let mut i = 0usize;
    while i + 2 <= extra.len() {
        let len = extra[i] as usize;
        if len < 2 || i + len > extra.len() {
            return None;
        }
        let d = &extra[i..i + len];
        if d[1] == DT_HID && len >= 9 {
            let n = (d[5] as usize).min((len - 6) / 3);
            let report_len = (0..n)
                .filter(|k| d[6 + 3 * k] == DT_REPORT)
                .map(|k| u16::from_le_bytes([d[7 + 3 * k], d[8 + 3 * k]]))
                .next_back()
                .filter(|&l| l > 0 && l <= MAX_DESCRIPTOR_BYTES)?;
            return Some(HidDescriptor { version: u16::from_le_bytes([d[2], d[3]]), country: d[4], report_len });
        }
        i += len;
    }
    None
}

pub fn get_report_descriptor(iface: u8, len: u16) -> Setup {
    Setup::new(0x81, 6, (DT_REPORT as u16) << 8, iface as u16, len)
}
/// SET_IDLE: `duration` in 4 ms units (0 = report only on change), for every report (ID 0).
pub fn set_idle(iface: u8, duration: u8) -> Setup {
    Setup::new(0x21, REQ_SET_IDLE, (duration as u16) << 8, iface as u16, 0)
}
pub fn set_protocol(iface: u8, protocol: u16) -> Setup {
    Setup::new(0x21, REQ_SET_PROTOCOL, protocol, iface as u16, 0)
}
pub fn set_report_output(iface: u8, id: u8, len: u16) -> Setup {
    Setup::new(0x21, REQ_SET_REPORT, REPORT_OUTPUT << 8 | id as u16, iface as u16, len)
}
/// GET_REPORT of feature report `id`, `len` bytes (Linux `usbhid_get_raw_report`, hid-core.c: wValue =
/// (type + 1) << 8 | id with HID_FEATURE_REPORT = 2).
pub fn get_report_feature(iface: u8, id: u8, len: u16) -> Setup {
    Setup::new(0xA1, REQ_GET_REPORT, REPORT_FEATURE << 8 | id as u16, iface as u16, len)
}

/// Sixaxis-family pads that send no report before the host reads feature reports 0xF2 (17 bytes) and 0xF5
/// (8 bytes): Linux hid-sony `sixaxis_set_operational_usb` (linux-6.6/drivers/hid/hid-sony.c:1133-1180, sizes
/// :465-466) for SIXAXIS_CONTROLLER_USB and NAVIGATION_CONTROLLER_USB (device table :2222-2257).
pub const SIXAXIS_USB: [(u16, u16); 3] = [(0x054C, 0x0268), (0x054C, 0x042F), (0x1345, 0x3008)];
pub const SIXAXIS_WAKE: [(u8, u16); 2] = [(0xF2, 17), (0xF5, 8)];

pub fn needs_sixaxis_wake(vendor: u16, product: u16) -> bool {
    SIXAXIS_USB.contains(&(vendor, product))
}

/// Joystick and Gamepad applications (Generic Desktop 4 and 5): the devices polled at 1 ms.
pub fn is_pad(application: u32) -> bool {
    matches!(application, 0x0001_0004 | 0x0001_0005)
}

/// The bInterval a gamepad's interrupt IN endpoint is polled with instead of its descriptor's (V1
/// `usb/hidpadbind.asm:165-190` `pad_fast`, spec usb.md §2 "Polling"): 1 ms at low and full speed, 4 (2^3
/// microframes = 1 ms) at high and super speed; None when the descriptor already asks for 1 ms or less. Low
/// speed devices are polled at 1 ms too (USB 2.0 says 10 ms at least, but xHCI does it and V1's DragonRise, a
/// low speed pad, gave 897-902 reports/s on the Mini-PC).
pub fn fast_interval(speed: Speed, b_interval: u8) -> Option<u8> {
    match speed {
        Speed::Low | Speed::Full => (b_interval > 1).then_some(1),
        Speed::High | Speed::Super | Speed::SuperPlus => (b_interval > 4).then_some(4),
    }
}

/// The interval wish for an interface: `fast_interval` for a pad, None for any other interface and when
/// CONFIG `usbhid_pad_binterval=on` (`keep_descriptor`) keeps the descriptor's bInterval.
pub fn pad_poll(pad: bool, keep_descriptor: bool, speed: Speed, b_interval: u8) -> Option<u8> {
    if pad && !keep_descriptor { fast_interval(speed, b_interval) } else { None }
}

/// Milliseconds (or fractions) the controller waits between polls for `b_interval` at `speed`, as xHCI
/// rounds it (`xhci::ctx::interval`: 2^n x 125 us), in microseconds: what the MCP tool shows.
pub fn poll_us(speed: Speed, b_interval: u8) -> u32 {
    let exp = match speed {
        Speed::High | Speed::Super | Speed::SuperPlus => b_interval.clamp(1, 16) - 1,
        Speed::Low | Speed::Full => ((31 - ((b_interval.max(1) as u32) * 8).leading_zeros()) as u8).clamp(3, 10),
    };
    125 << exp
}

/// The wished poll period in microseconds as the MCP tool and `hid.bound` show it, `none` without a wish. Only a
/// wish until usb-core's PipeOpts carries an interval (plan/notes/in-IN-2b.md, change request 1).
pub fn poll_want_us(speed: Speed, want: Option<u8>) -> String {
    want.map(|b| format!("{}", poll_us(speed, b))).unwrap_or_else(|| String::from("none"))
}

/// Bytes the longest input report takes on the wire (ID byte included), at least 1.
pub fn max_input_report(desc: &ReportDescriptor) -> usize {
    let numbered = desc.numbered(ReportKind::Input);
    let longest = desc.reports_of(ReportKind::Input).map(|r| r.len_bytes(numbered)).max().unwrap_or(1);
    usize::try_from(longest).unwrap_or(usize::MAX).max(1)
}

/// What `hidparse::InputState::new(desc, map, max_bytes)` will hold, in bytes (two i32 per value slot that can
/// lie inside `max_bytes`), so a hostile descriptor is refused before anything is allocated.
pub fn state_bytes(desc: &ReportDescriptor, max_bytes: usize) -> u64 {
    let numbered = desc.numbered(ReportKind::Input);
    let bits = (max_bytes.saturating_sub(numbered as usize) as u64).saturating_mul(8);
    desc.reports_of(ReportKind::Input)
        .flat_map(|r| r.fields.iter())
        .filter(|f| f.size > 0 && f.bit_offset < bits)
        .map(|f| ((bits - f.bit_offset).div_ceil(f.size as u64)).min(f.count as u64))
        .fold(0u64, |a, n| a.saturating_add(n.saturating_mul(8)))
}

/// Linux's name suffix for an application when a device has several (hidinput_allocate).
pub fn suffix(application: u32) -> Option<&'static str> {
    Some(match application {
        0x0001_0006 => "Keyboard",
        0x0001_0007 => "Keypad",
        0x0001_0002 => "Mouse",
        0x000D_0002 => "Stylus",
        0x000D_0020 => "Pen",
        0x000D_0004 => "Touchscreen",
        0x000D_0005 => "Touchpad",
        0x0001_0080 => "System Control",
        0x000C_0001 => "Consumer Control",
        0x0001_000C => "Wireless Radio Control",
        0x0001_000E => "System Multi Axis",
        _ => return None,
    })
}

/// "USB HID 046d:c52b" plus the application's suffix when the interface has several applications. No string
/// descriptors are read (spec usb.md §2: some pads answer unnamed indices with garbage).
pub fn device_name(vendor: u16, product: u16, application: u32, several: bool) -> String {
    let base = format!("USB HID {vendor:04x}:{product:04x}");
    match suffix(application).filter(|_| several) {
        Some(s) => format!("{base} {s}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_descriptor_from_extra_bytes() {
        // QEMU usb-kbd: HID 1.11, country 0, one report descriptor of 63 bytes.
        let kbd = [0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x3F, 0x00];
        assert_eq!(hid_descriptor(&kbd), Some(HidDescriptor { version: 0x0111, country: 0, report_len: 63 }));
        // Behind another class descriptor; two entries (a physical descriptor first).
        let two = [0x04, 0x24, 0x00, 0x00, 0x0C, 0x21, 0x10, 0x01, 0x09, 0x02, 0x23, 0x10, 0x00, 0x22, 0x80, 0x01];
        assert_eq!(hid_descriptor(&two).map(|h| (h.country, h.report_len)), Some((9, 0x180)));
        // bNumDescriptors larger than the descriptor holds: only what fits is read (Linux min_t).
        let lying = [0x09, 0x21, 0x11, 0x01, 0x00, 0x05, 0x22, 0x10, 0x00];
        assert_eq!(hid_descriptor(&lying).map(|h| h.report_len), Some(16));
        // Linux's size bound: 4096 bytes pass, 4097 and 65535 do not.
        assert_eq!(hid_descriptor(&[0x09, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x00, 0x10]).map(|h| h.report_len), Some(4096));
        assert_eq!(hid_descriptor(&[0x09, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x01, 0x10]), None);
        assert_eq!(hid_descriptor(&[0x09, 0x21, 0x11, 0x01, 0, 1, 0x22, 0xFF, 0xFF]), None);
        for bad in [&[][..], &[0x09, 0x21, 0x11][..], &[0x00, 0x21][..], &[0x09, 0x21, 0x11, 0x01, 0, 1, 0x22, 0, 0][..],
            &[0x09, 0x21, 0x11, 0x01, 0, 1, 0x23, 9, 0][..], &[0x06, 0x21, 0x11, 0x01, 0, 0][..]] {
            assert_eq!(hid_descriptor(bad), None, "{bad:x?}");
        }
    }

    #[test]
    fn class_requests_as_on_the_wire() {
        assert_eq!(get_report_descriptor(1, 63).to_u64(), 0x003F_0001_2200_0681);
        assert_eq!(set_idle(0, 0).to_u64(), 0x0000_0000_0000_0A21);
        assert_eq!(set_protocol(2, PROTOCOL_BOOT).to_u64(), 0x0000_0002_0000_0B21);
        assert_eq!(set_report_output(0, 0, 1).to_u64(), 0x0001_0000_0200_0921);
        assert_eq!(set_report_output(1, 5, 2).value, 0x0205);
    }

    #[test]
    fn sixaxis_wake_requests_as_linux_sends_them() {
        // usbhid_get_raw_report: USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE, HID_REQ_GET_REPORT,
        // wValue 0x03F2, wIndex = interface, wLength 17; V1 `pad_wake` sends the same 0x0011_0000_03F2_01A1.
        assert_eq!(get_report_feature(0, 0xF2, 17).to_u64(), 0x0011_0000_03F2_01A1);
        assert_eq!(get_report_feature(2, 0xF5, 8).to_u64(), 0x0008_0002_03F5_01A1);
        assert!(needs_sixaxis_wake(0x054C, 0x0268) && needs_sixaxis_wake(0x054C, 0x042F) && needs_sixaxis_wake(0x1345, 0x3008));
        // DualShock 4 (054C:05C4), the motion controller (054C:03D5, hid-sony asks it nothing on USB) and
        // Georg's DragonRise: no wake.
        assert!(!needs_sixaxis_wake(0x054C, 0x05C4) && !needs_sixaxis_wake(0x054C, 0x03D5) && !needs_sixaxis_wake(0x0079, 6));
    }

    #[test]
    fn pads_are_polled_every_millisecond() {
        // Georg's DragonRise: low speed, bInterval 10 -> polled every 8 ms (125 reports/s); forced to 1 ms.
        assert_eq!(poll_us(Speed::Low, 10), 8000);
        assert_eq!(fast_interval(Speed::Low, 10), Some(1));
        assert_eq!(poll_us(Speed::Low, 1), 1000);
        assert_eq!((fast_interval(Speed::Full, 1), fast_interval(Speed::Full, 0)), (None, None), "1 ms already (0 reads as 1)");
        assert_eq!(fast_interval(Speed::Full, 255), Some(1));
        // High speed: bInterval 4 = 2^3 microframes = 1 ms; shorter ones are kept.
        assert_eq!(fast_interval(Speed::High, 7), Some(4));
        assert_eq!(poll_us(Speed::High, 4), 1000);
        assert_eq!((fast_interval(Speed::High, 4), fast_interval(Speed::High, 1)), (None, None));
        assert_eq!(fast_interval(Speed::Super, 16), Some(4));
        assert_eq!(poll_us(Speed::Super, 1), 125);
        // CONFIG usbhid_pad_binterval=on keeps the descriptor's; a keyboard or mouse never gets a wish.
        assert_eq!(pad_poll(true, false, Speed::Low, 10), Some(1));
        assert_eq!(pad_poll(true, true, Speed::Low, 10), None);
        assert_eq!(pad_poll(true, true, Speed::High, 7), None);
        assert_eq!(pad_poll(false, false, Speed::Low, 10), None);
        assert_eq!((poll_want_us(Speed::Low, Some(1)), poll_want_us(Speed::Low, None)), (String::from("1000"), String::from("none")));
        assert!(is_pad(0x10004) && is_pad(0x10005) && !is_pad(0x10002) && !is_pad(0x10006) && !is_pad(0x10008));
    }

    #[test]
    fn report_sizes_and_state_bound() {
        let kbd = hidparse::boot::boot_keyboard();
        assert_eq!(max_input_report(&kbd), 8);
        assert!(state_bytes(&kbd, 8) <= 8 * 64);
        // One field declaring 2^31 values of 1 bit: the state only counts what the report length can reach.
        let huge = hidparse::parse(&[0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0, 0x29, 1, 0x75, 1,
            0x97, 0, 0, 0, 0x80, 0x81, 0x02, 0xC0]).unwrap();
        assert!(max_input_report(&huge) > MAX_REPORT_BYTES);
        assert_eq!(state_bytes(&huge, MAX_REPORT_BYTES), MAX_REPORT_BYTES as u64 * 8 * 8);
        assert!(state_bytes(&huge, MAX_REPORT_BYTES) <= MAX_STATE_BYTES);
        assert_eq!(state_bytes(&hidparse::ReportDescriptor::default(), 64), 0);
    }

    #[test]
    fn names_follow_linux() {
        assert_eq!(device_name(0x046d, 0xc52b, 0x10006, true), "USB HID 046d:c52b Keyboard");
        assert_eq!(device_name(0x046d, 0xc52b, 0x10006, false), "USB HID 046d:c52b");
        assert_eq!(device_name(0x0627, 1, 0xFF000001, true), "USB HID 0627:0001");
    }
}
