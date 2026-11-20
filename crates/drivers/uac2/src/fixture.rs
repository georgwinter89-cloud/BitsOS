//! The configuration descriptor of the ASRock 26ce:0a09, built from the facts `lsusb -v` showed on 5.11. Typed from
//! those facts, field by field, with no string, no serial and no capture file: `wTotalLength` agreeing byte for byte
//! with the real device's 0x0C73 is the check that the shape is right. Its unit tree came out as 0x0144 the same way.
//! Built for this crate's tests and, with the feature `fixture`, for `usb-audio`'s; never in the kernel.

use alloc::vec::Vec;

pub struct Alt {
    pub iface: u8,
    pub alt: u8,
    pub link: u8,
    pub formats: u32,
    pub channels: u8,
    pub config: u32,
    pub subslot: u8,
    pub bits: u8,
    pub ep: u8,
    pub attributes: u8,
    pub mps: u16,
}

#[rustfmt::skip]
pub const ALTS: [Alt; 60] = [
    Alt { iface: 1, alt: 1, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x83, attributes: 5, mps: 0x007c },
    Alt { iface: 1, alt: 2, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x83, attributes: 5, mps: 0x00ba },
    Alt { iface: 1, alt: 3, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x83, attributes: 5, mps: 0x007c },
    Alt { iface: 1, alt: 4, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x83, attributes: 5, mps: 0x00ba },
    Alt { iface: 1, alt: 5, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x83, attributes: 5, mps: 0x007c },
    Alt { iface: 1, alt: 6, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x83, attributes: 5, mps: 0x00ba },
    Alt { iface: 1, alt: 7, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x83, attributes: 5, mps: 0x007c },
    Alt { iface: 1, alt: 8, link: 19, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x83, attributes: 5, mps: 0x00ba },
    Alt { iface: 2, alt: 1, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x84, attributes: 5, mps: 0x007c },
    Alt { iface: 2, alt: 2, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x84, attributes: 5, mps: 0x00ba },
    Alt { iface: 2, alt: 3, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x84, attributes: 5, mps: 0x007c },
    Alt { iface: 2, alt: 4, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x84, attributes: 5, mps: 0x00ba },
    Alt { iface: 2, alt: 5, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x84, attributes: 5, mps: 0x007c },
    Alt { iface: 2, alt: 6, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x84, attributes: 5, mps: 0x00ba },
    Alt { iface: 2, alt: 7, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x84, attributes: 5, mps: 0x007c },
    Alt { iface: 2, alt: 8, link: 20, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x84, attributes: 5, mps: 0x00ba },
    Alt { iface: 3, alt: 1, link: 13, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x05, attributes: 9, mps: 0x00f8 },
    Alt { iface: 3, alt: 2, link: 13, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x05, attributes: 9, mps: 0x0174 },
    Alt { iface: 3, alt: 3, link: 13, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x05, attributes: 9, mps: 0x01f0 },
    Alt { iface: 3, alt: 4, link: 13, formats: 0x1, channels: 4, config: 0x33, subslot: 2, bits: 16, ep: 0x05, attributes: 9, mps: 0x01f0 },
    Alt { iface: 3, alt: 5, link: 13, formats: 0x1, channels: 4, config: 0x33, subslot: 3, bits: 24, ep: 0x05, attributes: 9, mps: 0x02e8 },
    Alt { iface: 3, alt: 6, link: 13, formats: 0x1, channels: 4, config: 0x33, subslot: 4, bits: 32, ep: 0x05, attributes: 9, mps: 0x03e0 },
    Alt { iface: 3, alt: 7, link: 13, formats: 0x1, channels: 6, config: 0x3f, subslot: 2, bits: 16, ep: 0x05, attributes: 9, mps: 0x02e8 },
    Alt { iface: 3, alt: 8, link: 13, formats: 0x1, channels: 6, config: 0x3f, subslot: 3, bits: 24, ep: 0x05, attributes: 9, mps: 0x0a2e },
    Alt { iface: 3, alt: 9, link: 13, formats: 0x1, channels: 6, config: 0x3f, subslot: 4, bits: 32, ep: 0x05, attributes: 9, mps: 0x0ae8 },
    Alt { iface: 3, alt: 10, link: 13, formats: 0x1, channels: 8, config: 0x63f, subslot: 2, bits: 16, ep: 0x05, attributes: 9, mps: 0x03e0 },
    Alt { iface: 3, alt: 11, link: 13, formats: 0x1, channels: 8, config: 0x63f, subslot: 3, bits: 24, ep: 0x05, attributes: 9, mps: 0x0ae8 },
    Alt { iface: 3, alt: 12, link: 13, formats: 0x1, channels: 8, config: 0x63f, subslot: 4, bits: 32, ep: 0x05, attributes: 9, mps: 0x0be0 },
    Alt { iface: 4, alt: 1, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 2, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 3, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 4, alt: 4, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 5, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 6, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 4, alt: 7, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 8, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 9, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 4, alt: 10, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 11, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 12, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 4, alt: 13, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 14, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 15, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 4, alt: 16, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 17, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 18, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 4, alt: 19, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x06, attributes: 9, mps: 0x00f8 },
    Alt { iface: 4, alt: 20, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x06, attributes: 9, mps: 0x0174 },
    Alt { iface: 4, alt: 21, link: 14, formats: 0x1, channels: 2, config: 0x3, subslot: 4, bits: 32, ep: 0x06, attributes: 9, mps: 0x01f0 },
    Alt { iface: 5, alt: 1, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x08, attributes: 9, mps: 0x007c },
    Alt { iface: 5, alt: 2, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x08, attributes: 9, mps: 0x00ba },
    Alt { iface: 5, alt: 3, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x08, attributes: 9, mps: 0x007c },
    Alt { iface: 5, alt: 4, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x08, attributes: 9, mps: 0x00ba },
    Alt { iface: 5, alt: 5, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x08, attributes: 9, mps: 0x007c },
    Alt { iface: 5, alt: 6, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x08, attributes: 9, mps: 0x00ba },
    Alt { iface: 5, alt: 7, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x08, attributes: 9, mps: 0x007c },
    Alt { iface: 5, alt: 8, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x08, attributes: 9, mps: 0x00ba },
    Alt { iface: 5, alt: 9, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x08, attributes: 9, mps: 0x007c },
    Alt { iface: 5, alt: 10, link: 16, formats: 0x1, channels: 2, config: 0x3, subslot: 3, bits: 24, ep: 0x08, attributes: 9, mps: 0x00ba },
    Alt { iface: 5, alt: 11, link: 16, formats: 0x1381, channels: 2, config: 0x3, subslot: 2, bits: 16, ep: 0x08, attributes: 9, mps: 0x007c },
];

fn clock(out: &mut Vec<u8>, id: u8) {
    out.extend_from_slice(&[8, 0x24, 0x0A, id, 7, 0x07, 0, 0]);
}

fn input(out: &mut Vec<u8>, id: u8, ty: u16, clock: u8, channels: u8, config: u32, controls: u16) {
    out.extend_from_slice(&[17, 0x24, 2, id]);
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&[0, clock, channels]);
    out.extend_from_slice(&config.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&controls.to_le_bytes());
    out.push(0);
}

fn output(out: &mut Vec<u8>, id: u8, ty: u16, assoc: u8, source: u8, clock: u8, controls: u16) {
    out.extend_from_slice(&[12, 0x24, 3, id]);
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&[assoc, source, clock]);
    out.extend_from_slice(&controls.to_le_bytes());
    out.push(0);
}

fn feature(out: &mut Vec<u8>, id: u8, source: u8, controls: &[u32]) {
    out.extend_from_slice(&[(6 + 4 * controls.len()) as u8, 0x24, 6, id, source]);
    for c in controls {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out.push(0);
}

/// The extension unit hanging off the first microphone's feature unit, not on any path. Its wExtensionCode is 0x0BDA.
fn extension(out: &mut Vec<u8>, id: u8, source: u8) {
    out.extend_from_slice(&[16, 0x24, 9, id, 0xDA, 0x0B, 1, source, 2, 3, 0, 0, 0, 0, 0, 0]);
}

pub fn control_tree() -> Vec<u8> {
    let mut t = Vec::new();
    const STEREO: u32 = 3;
    const STEREO_VOL: [u32; 3] = [3, 0xC, 0xC];
    const STEREO_MUTE: [u32; 3] = [3, 0, 0];

    clock(&mut t, 3);
    input(&mut t, 11, 0x0202, 3, 2, STEREO, 0x0004);
    output(&mut t, 19, 0x0101, 11, 27, 3, 0);
    feature(&mut t, 27, 11, &STEREO_VOL);
    extension(&mut t, 33, 27);

    clock(&mut t, 4);
    input(&mut t, 12, 0x0201, 4, 2, STEREO, 0x0004);
    output(&mut t, 20, 0x0101, 12, 28, 4, 0);
    feature(&mut t, 28, 12, &STEREO_VOL);

    clock(&mut t, 5);
    input(&mut t, 13, 0x0101, 5, 8, 0x63F, 0);
    output(&mut t, 21, 0x0301, 13, 29, 5, 0x0004);
    feature(&mut t, 29, 13, &[3, 0xC, 0xC, 0xC, 0xC, 0xC, 0xC, 0xC, 0xC]);

    clock(&mut t, 6);
    input(&mut t, 14, 0x0101, 6, 2, STEREO, 0);
    output(&mut t, 22, 0x0302, 14, 30, 6, 0x0004);
    feature(&mut t, 30, 14, &STEREO_MUTE);

    clock(&mut t, 8);
    input(&mut t, 16, 0x0101, 8, 2, STEREO, 0);
    output(&mut t, 24, 0x0605, 16, 32, 8, 0);
    feature(&mut t, 32, 16, &STEREO_MUTE);
    t
}

fn interface(out: &mut Vec<u8>, number: u8, alt: u8, endpoints: u8, class: u8, subclass: u8, protocol: u8) {
    out.extend_from_slice(&[9, 4, number, alt, endpoints, class, subclass, protocol, 0]);
}

fn endpoint(out: &mut Vec<u8>, address: u8, attributes: u8, mps: u16, interval: u8) {
    out.extend_from_slice(&[7, 5, address, attributes]);
    out.extend_from_slice(&mps.to_le_bytes());
    out.push(interval);
}

/// Interface 5's alternate setting 11 is the one Type III format on the device (0x1381, IEC 61937 and friends).
pub fn format_type(a: &Alt) -> u8 {
    if a.formats == 0x1381 { 3 } else { 1 }
}

pub fn streaming_alt(out: &mut Vec<u8>, a: &Alt) {
    interface(out, a.iface, a.alt, 1, 1, 2, 0x20);
    out.extend_from_slice(&[16, 0x24, 1, a.link, 0, format_type(a)]);
    out.extend_from_slice(&a.formats.to_le_bytes());
    out.push(a.channels);
    out.extend_from_slice(&a.config.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&[6, 0x24, 2, format_type(a), a.subslot, a.bits]);
    endpoint(out, a.ep, a.attributes, a.mps, 1);
    out.extend_from_slice(&[8, 0x25, 1, 0, 0, 0, 0, 0]);
}

/// A configuration around these parts, with `wTotalLength` filled in.
pub fn wrap(body: &[u8], interfaces: u8) -> Vec<u8> {
    let total = (9 + body.len()) as u16;
    let mut c = alloc::vec![9, 2, 0, 0, interfaces, 1, 0, 0x80, 50];
    c[2..4].copy_from_slice(&total.to_le_bytes());
    c.extend_from_slice(body);
    c
}

/// Interface 0 with this unit tree and its header.
pub fn control_interface(tree: &[u8], declared: u16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[8, 11, 0, 6, 1, 0, 0x20, 0]);
    interface(&mut b, 0, 0, 1, 1, 1, 0x20);
    b.extend_from_slice(&[9, 0x24, 1, 0x00, 0x02, 4]);
    b.extend_from_slice(&declared.to_le_bytes());
    b.push(0);
    b.extend_from_slice(tree);
    endpoint(&mut b, 0x89, 3, 0x10, 8);
    b
}

pub fn config() -> Vec<u8> {
    let tree = control_tree();
    let mut body = control_interface(&tree, (9 + tree.len()) as u16);
    for number in 1..=5 {
        interface(&mut body, number, 0, 0, 1, 2, 0x20);
        for a in ALTS.iter().filter(|a| a.iface == number) {
            streaming_alt(&mut body, a);
        }
    }
    // Interface 6 is HID, with the HID descriptor between it and its endpoint.
    interface(&mut body, 6, 0, 1, 3, 0, 0);
    body.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 34, 0]);
    endpoint(&mut body, 0x8A, 3, 0x10, 4);
    wrap(&body, 7)
}

/// A RANGE reply listing Linux's rates for this clock as single-value triplets. Hand-built: Linux reports the rates it
/// derived, and the device's own bytes on the wire were not read, so how it splits them into triplets is unverified.
pub fn range_reply(rates: &[u32]) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend_from_slice(&(rates.len() as u16).to_le_bytes());
    for rate in rates {
        r.extend_from_slice(&rate.to_le_bytes());
        r.extend_from_slice(&rate.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
    }
    r
}

/// Rates `/proc/asound/card4/stream0` listed for the speaker on 5.11.
pub const SPEAKER_RATES: [u32; 7] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 384_000];
/// And for the microphones.
pub const MIC_RATES: [u32; 4] = [44_100, 48_000, 96_000, 192_000];
