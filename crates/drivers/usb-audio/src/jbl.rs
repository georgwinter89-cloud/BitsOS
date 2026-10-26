//! Georg's JBL Quantum 800 dongle (0ecb:203e), its configuration descriptor byte for byte as
//! plan/hw/jbl-quantum-800-lsusb.txt prints it (wTotalLength 0x0147 = 327, checked by the tests).

#[allow(dead_code)] // the device descriptor, for a fake device later
pub const DEVICE: [u8; 18] = [18, 1, 0x10, 0x01, 0, 0, 0, 64, 0xCB, 0x0E, 0x3E, 0x20, 0x00, 0x01, 4, 5, 0, 1];

pub fn config() -> std::vec::Vec<u8> {
    let parts: &[&[u8]] = &[
        &[9, 2, 0x47, 0x01, 6, 1, 0, 0x80, 50],
        // Interface 0: AudioControl "Game": IT1 USB streaming 2ch -> FU2 (mute) -> OT5 speaker.
        &[9, 4, 0, 0, 0, 1, 1, 0, 1],
        &[9, 0x24, 1, 0x00, 0x01, 0x28, 0x00, 1, 1],
        &[12, 0x24, 2, 1, 0x01, 0x01, 0, 2, 0x03, 0x00, 0, 0],
        &[10, 0x24, 6, 2, 1, 1, 0x01, 0x00, 0x00, 0],
        &[9, 0x24, 3, 5, 0x01, 0x03, 0, 2, 0],
        // Interface 1: game playback; alt 1 without endpoints, alt 2 EP 0x02 OUT 288 B synchronous.
        &[9, 4, 1, 0, 0, 1, 2, 0, 0],
        &[9, 4, 1, 1, 0, 1, 2, 0, 0],
        &[9, 4, 1, 2, 1, 1, 2, 0, 0],
        &[7, 0x24, 1, 1, 0, 0x01, 0x00],
        &[11, 0x24, 2, 1, 2, 2, 16, 1, 0x80, 0xBB, 0x00],
        &[9, 5, 0x02, 0x0D, 0x20, 0x01, 1, 0, 0],
        &[7, 0x25, 1, 0x01, 0, 0x00, 0x00],
        // Interface 2: AudioControl "Chat": IT6 headset mic -> FU3 (mute+volume) -> OT7 USB streaming;
        // IT96 USB streaming 1ch -> FU4 (mute) -> OT80 headset.
        &[9, 4, 2, 0, 0, 1, 1, 0, 2],
        &[10, 0x24, 1, 0x00, 0x01, 0x46, 0x00, 2, 3, 4],
        &[12, 0x24, 2, 6, 0x02, 0x04, 80, 1, 0x00, 0x00, 0, 0],
        &[9, 0x24, 6, 3, 6, 1, 0x03, 0x00, 0],
        &[9, 0x24, 3, 7, 0x01, 0x01, 0, 3, 0],
        &[12, 0x24, 2, 96, 0x01, 0x01, 0, 1, 0x00, 0x00, 0, 0],
        &[9, 0x24, 6, 4, 96, 1, 0x01, 0x00, 0],
        &[9, 0x24, 3, 80, 0x02, 0x04, 6, 4, 0],
        // Interface 3: mic capture, alt 1 EP 0x81 IN 144 B.
        &[9, 4, 3, 0, 0, 1, 2, 0, 0],
        &[9, 4, 3, 1, 1, 1, 2, 0, 0],
        &[7, 0x24, 1, 7, 0, 0x01, 0x00],
        &[11, 0x24, 2, 1, 1, 2, 16, 1, 0x80, 0xBB, 0x00],
        &[9, 5, 0x81, 0x0D, 0x90, 0x00, 1, 0, 0],
        &[7, 0x25, 1, 0x01, 0, 0x00, 0x00],
        // Interface 4: chat playback, alt 1 EP 0x06 OUT 144 B.
        &[9, 4, 4, 0, 0, 1, 2, 0, 0],
        &[9, 4, 4, 1, 1, 1, 2, 0, 0],
        &[7, 0x24, 1, 96, 0, 0x01, 0x00],
        &[11, 0x24, 2, 1, 1, 2, 16, 1, 0x80, 0xBB, 0x00],
        &[9, 5, 0x06, 0x0D, 0x90, 0x00, 1, 0, 0],
        &[7, 0x25, 1, 0x01, 0, 0x00, 0x00],
        // Interface 5: HID (usb-hid's).
        &[9, 4, 5, 0, 1, 3, 0, 0, 3],
        &[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 0xDC, 0x03],
        &[7, 5, 0x83, 3, 0x20, 0x00, 1],
    ];
    parts.concat()
}
