//! GX-8v: VCN 4's words against Linux 7.3.1 and the 27.10. capture of the 780M.

use crate::ring::{Kind, Ring};
use crate::vcn::{self, Packet, Sram, cmd, shared};
use std::vec;
use std::vec::Vec;

#[test]
fn ring_rules_are_the_unified_rings() {
    assert_eq!(Kind::Vcn.ring_align_dw(), 64);
    assert_eq!(Kind::Vcn.hw_ptr(0x40), 0x40);
    // The capture's ring test: one END, the commit pads to 64 (UVD_RB_RPTR went 0 -> 0x40).
    let mut r = Ring::new(Kind::Vcn, 1024, 0).unwrap();
    let mut mem = vec![0xdead_beefu32; 1024];
    r.begin(16, 0).unwrap();
    r.write(&[cmd::END]).unwrap();
    assert_eq!(r.commit(&mut mem[..]).unwrap(), 0x40);
    assert_eq!(mem[0], cmd::END);
    assert!(mem[1..64].iter().all(|&w| w == cmd::NO_OP));
}

#[test]
fn ib_and_fence_packets() {
    let mut v = Vec::new();
    vcn::ib(&mut v, 1, 0x7fff_0026_1000, 24).unwrap();
    vcn::fence(&mut v, 0x7fff_0000_16c0, 0x1_0000_0005).unwrap();
    assert_eq!(v, [cmd::IB, 1, 0x0026_1000, 0x7fff, 24, cmd::FENCE, 0x0000_16c0, 0x7fff, 5, cmd::TRAP]);
    assert!(vcn::ib(&mut v, 0, 2, 4).is_err() && vcn::ib(&mut v, 0, 4, 0).is_err() && vcn::ib(&mut v, 16, 4, 4).is_err());
    let mut r = Ring::new(Kind::Vcn, 1024, 0).unwrap();
    r.begin(16, 0).unwrap();
    assert!(r.emit_fence(0x1000, 1, true, false).is_err(), "VCN fences are 32-bit");
    r.emit_fence(0x1000, 1, false, false).unwrap();
    assert_eq!(Packet::decode(&[cmd::FENCE, 0x1000, 0, 1]), Some((Packet::Fence { addr: 0x1000, seq: 1 }, 4)));
    let mut w = Vec::new();
    vcn::reg_write(&mut w, 0x7ab + 2, 5);
    vcn::reg_wait(&mut w, 0x775, 4, 4);
    assert_eq!(Packet::decode(&w), Some((Packet::RegWrite { reg_dw: 0x7ad, value: 5 }, 3)));
    assert_eq!(Packet::decode(&w[3..]), Some((Packet::RegWait { reg_dw: 0x775, mask: 4, value: 4 }, 4)));
}

#[test]
fn unified_ibs_carry_linux_checksums() {
    let e = vcn::enc_session_ib(1, 0x1234_5000, vcn::ENC_OP_INIT);
    assert_eq!(e.len(), 24);
    assert_eq!(&e[..8], &[0x10, 0x3000_0002, e[2], 0x11, 0x10, 0x3000_0001, 2, 0x44]);
    assert_eq!(e[2], e[4..21].iter().fold(0u32, |s, &x| s.wrapping_add(x)));
    let u = vcn::parse_unified(&e).unwrap();
    assert!(u.checksum_ok && u.engine == vcn::ENGINE_ENCODE);
    assert_eq!(&u.body[..6], &[0x18, 1, 1, 0, 0x1234_5000, 0]);
    let d = vcn::dec_sw_ib(0x80_0012_3000);
    assert_eq!(d.len(), 72);
    let u = vcn::parse_unified(&d).unwrap();
    assert!(u.checksum_ok && u.engine == vcn::ENGINE_DECODE && u.pack_dw == 39);
    assert_eq!(vcn::dec_msg_addr(&u), Some(0x80_0012_3000));
    let mut bad = d.clone();
    bad[20] ^= 1;
    assert!(!vcn::parse_unified(&bad).unwrap().checksum_ok);
    assert!(vcn::parse_unified(&[0; 4]).is_none());
    assert_eq!(vcn::dec_msg_type(&vcn::dec_create_msg(1)), Some(0));
    assert_eq!(vcn::dec_msg_type(&vcn::dec_destroy_msg(1)), Some(2));
    assert_eq!(vcn::dec_create_msg(7)[4], 7);
}

#[test]
fn dpg_offsets_follow_soc15_dpg_mode_offset() {
    // VCN 4.0.2's segment 1 base 0x7E00 (the 780M's discovery): UVD_VCPU_CNTL 0x196 is in the video range.
    let byte = |dw: u32| (0x7E00 + dw) * 4;
    assert_eq!(vcn::dpg_offset(byte(0x196)), (byte(0x196) - 0x1fb00) >> 2);
    assert_eq!(vcn::dpg_offset(byte(0x196)), 0xd6);
    // UVD_GFX10_ADDR_CONFIG (0x42) lies below the video range: AON.
    assert_eq!(vcn::dpg_offset(byte(0x42)), (byte(0x42) - 0x1f800 + 0x30000) >> 2);
    // Outside every range: the address itself.
    assert_eq!(vcn::dpg_offset(0x100), 0x40);
    let mut s = Sram::default();
    s.push(byte(0x196), 0x0ff0_0200);
    s.push(byte(0xe1), 0);
    assert_eq!(s.bytes(), 16);
    assert_eq!(Sram::pairs(&s.to_bytes()), [(0xd6, 0x0ff0_0200), (vcn::dpg_offset(byte(0xe1)), 0)]);
}

#[test]
fn shared_page_as_vcn_v4_0_fw_shared_init() {
    let b = vcn::fw_shared(true);
    let flags = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    assert_eq!(flags, (1 << 2) | (1 << 4) | (1 << 11));
    assert_eq!((b[shared::SQ_IS_ENABLED], b[shared::SMU_INTERFACE_TYPE], b[shared::DRM_KEY_WA_METHOD]), (1, 1, 2));
    let plain = vcn::fw_shared(false);
    assert_eq!(u32::from_le_bytes([plain[0], plain[1], plain[2], plain[3]]), (1 << 2) | (1 << 11));
    assert_eq!(plain[shared::DRM_KEY_WA_METHOD], 0);
}

#[test]
fn hang_lines_decode_vcn_packets() {
    let mut w = Vec::new();
    vcn::ib(&mut w, 1, 0x1000, 24).unwrap();
    w.extend([0x77, cmd::FENCE, 1]);
    let l = vcn::decode_stream(&w, 0x40);
    assert_eq!(l.len(), 3);
    assert!(l[0].text.starts_with("Ib") && l[1].text.contains("opcode 0x77") && l[2].text.contains("TRUNCATED"));
    assert_eq!(crate::watch::TimeoutClass::Video.linux_us(), 2_000_000);
}
