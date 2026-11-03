//! PM4 and SDMA encoders against Linux's values, decoder round trips and refusals.

use std::vec::Vec;

use crate::encode::EncodeError;
use crate::pm4::{self, Packet as P};
use crate::sdma::{self, Packet as S};

#[test]
fn pm4_headers_match_nvd_h() {
    assert_eq!(pm4::PAD, 0xffff1000);
    assert_eq!(pm4::packet3(pm4::op::RELEASE_MEM, 6), 0xc0064900);
    assert_eq!(pm4::packet3(pm4::op::INDIRECT_BUFFER, 2), 0xc0023f00);
    assert_eq!(pm4::packet3(pm4::op::WRITE_DATA, 3), 0xc0033700);
    assert_eq!(pm4::packet_len(&[pm4::PAD]), Ok(1));
    assert_eq!(pm4::packet_len(&[pm4::TYPE2]), Ok(1));
    assert_eq!(pm4::packet_len(&[0xc0011000]), Ok(3));
    assert_eq!(pm4::packet_len(&[0x4000_0000]), Err(pm4::DecodeError::BadType { header: 0x4000_0000 }));
    assert_eq!(pm4::packet_len(&[]), Err(pm4::DecodeError::Truncated { need: 1, have: 0 }));
}

#[test]
fn pm4_round_trips() {
    let mut v = Vec::new();
    pm4::write_data(&mut v, pm4::dst_sel::MEM, 0x1234_5670, pm4::WR_CONFIRM, &[1, 2, 3]).unwrap();
    pm4::indirect_buffer(&mut v, 0x8000_1000, 77, 5, false, pm4::IB_VALID).unwrap();
    pm4::cond_exec(&mut v, 0x4000, 9).unwrap();
    let w = pm4::WaitRegMem {
        function: 3,
        mem: true,
        operation: 0,
        engine: 1,
        addr0: 0x2000,
        addr1: 0x1,
        reference: 7,
        mask: 0xff,
        interval: 4,
    };
    pm4::wait_reg_mem(&mut v, &w).unwrap();
    pm4::set_reg(&mut v, pm4::RegSpace::Uconfig, 0xc040, &[0xCAFE_DEAD]).unwrap();
    let mut i = 0;
    let mut got = Vec::new();
    while i < v.len() {
        let (p, n) = pm4::decode(&v[i..]).unwrap();
        got.push(std::format!("{p}"));
        match p {
            P::WriteData { dst_sel, addr, data, control } => {
                assert_eq!((dst_sel, addr, data, control & pm4::WR_CONFIRM != 0), (5, 0x1234_5670, &[1, 2, 3][..], true));
            }
            P::IndirectBuffer(ib) => {
                assert_eq!((ib.addr, ib.len_dw, ib.vmid, ib.cnst, ib.control & pm4::IB_VALID != 0), (0x8000_1000, 77, 5, false, true));
            }
            P::CondExec { addr, skip } => assert_eq!((addr, skip), (0x4000, 9)),
            P::WaitRegMem(d) => assert_eq!(d, w),
            P::SetReg { space, reg, values } => assert_eq!((space, reg, values), (pm4::RegSpace::Uconfig, 0xc040, &[0xCAFE_DEAD][..])),
            other => panic!("unexpected {other:?}"),
        }
        i += n;
    }
    assert_eq!(i, v.len());
    assert!(got[0].starts_with("WRITE_DATA dst_sel=5 addr=0x12345670 n=3 confirm"));
}

#[test]
fn pm4_fence_32_and_interrupt() {
    let mut v = Vec::new();
    pm4::fence(&mut v, 0x1004, 0xAAAA_BBBB_CCCC_DDDD, false, true).unwrap();
    let (p, n) = pm4::decode(&v).unwrap();
    assert_eq!(n, 8);
    let P::ReleaseMem(r) = p else { panic!() };
    assert_eq!((r.data_sel, r.int_sel, r.addr, r.data), (1, 2, 0x1004, 0xCCCC_DDDD));
    assert_eq!((r.event_type(), r.event_index()), (0x14, 5));
}

#[test]
fn pm4_encoders_refuse_and_append_nothing() {
    let mut v = Vec::new();
    assert_eq!(pm4::fence(&mut v, 0x1004, 1, true, false), Err(EncodeError::Misaligned { addr: 0x1004, align: 8 }));
    assert!(matches!(pm4::fence(&mut v, 0x1002, 1, false, false), Err(EncodeError::Misaligned { .. })));
    assert!(pm4::indirect_buffer(&mut v, 0x1002, 1, 0, false, 0).is_err());
    assert!(pm4::indirect_buffer(&mut v, 1 << 48, 1, 0, false, 0).is_err());
    assert!(pm4::indirect_buffer(&mut v, 0x1000, 1 << 20, 0, false, 0).is_err());
    assert!(pm4::indirect_buffer(&mut v, 0x1000, 1, 16, false, 0).is_err());
    assert!(pm4::nop(&mut v, &[]).is_err());
    assert!(pm4::nop(&mut v, &std::vec![0; 0x4000]).is_err());
    assert!(pm4::write_data(&mut v, 3, 0, 0, &[1]).is_err());
    assert!(pm4::write_data(&mut v, pm4::dst_sel::MEM, 2, 0, &[1]).is_err());
    assert!(pm4::write_data(&mut v, pm4::dst_sel::MEM, 0, 0, &[]).is_err());
    assert!(pm4::set_reg(&mut v, pm4::RegSpace::Sh, 0x10, &[1]).is_err());
    assert!(v.is_empty());
    assert_eq!(EncodeError::Misaligned { addr: 0, align: 4 }.errno(), crate::errno::EINVAL);
    // The longest NOP body that is not the one-dword filler.
    pm4::nop(&mut v, &std::vec![0; 0x3FFF]).unwrap();
    assert_eq!(pm4::packet_len(&v), Ok(0x4000));
}

#[test]
fn pm4_bad_counts_are_refused() {
    // RELEASE_MEM with count 5, INDIRECT_BUFFER with count 1, WRITE_DATA without data.
    for h in [pm4::packet3(pm4::op::RELEASE_MEM, 5), pm4::packet3(pm4::op::INDIRECT_BUFFER, 1), pm4::packet3(pm4::op::WRITE_DATA, 2)] {
        let w = std::vec![h, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(pm4::decode(&w), Err(pm4::DecodeError::BadCount { header: h }));
    }
}

#[test]
fn sdma_values_match_linux() {
    let mut v = Vec::new();
    sdma::emit_fence(&mut v, 0x2000, 0x1_0000_0002, true, true).unwrap();
    // FENCE | MTYPE(3), addr, seq lo; FENCE at +4 with seq hi; TRAP, ctx 0 (sdma_v6_0.c:338-370).
    assert_eq!(v, [0x0003_0005, 0x2000, 0, 2, 0x0003_0005, 0x2004, 0, 1, 6, 0]);
    v.clear();
    sdma::indirect(&mut v, 4, 0x1_0000_0040, 32, 0x5000).unwrap();
    assert_eq!(v, [0x0004_0004, 0x40, 1, 32, 0x5000, 0]);
    v.clear();
    sdma::copy_linear(&mut v, 0x1000, 0x2000, 4096, false).unwrap();
    assert_eq!(v, [1, 4095, 0, 0x1000, 0, 0x2000, 0]);
    v.clear();
    sdma::const_fill(&mut v, 0x3000, 0xAB, 4096, sdma::FILL_BYTE).unwrap();
    assert_eq!(v, [11, 0x3000, 0, 0xAB, 4095]);
    v.clear();
    sdma::const_fill(&mut v, 0x3000, 0xAB, 4096, sdma::FILL_DWORD).unwrap();
    assert_eq!(v[0], 11 | (2 << 30));
    v.clear();
    sdma::nops(&mut v, 5, true).unwrap();
    assert_eq!(v, [4 << 16, 0, 0, 0, 0]);
    assert_eq!(sdma::packet_len(&v), Ok(5));
    v.clear();
    sdma::srbm_write(&mut v, 0x1234, 9).unwrap();
    assert_eq!(v, [0xF000_000E, 0x1234, 9]);
}

#[test]
fn sdma_round_trips() {
    let mut v = Vec::new();
    sdma::write_linear(&mut v, 0x40, &[7; 9]).unwrap();
    sdma::copy_linear(&mut v, 0x11, 0x23, 5, false).unwrap();
    sdma::const_fill(&mut v, 0x100, 0x55, 8, sdma::FILL_DWORD).unwrap();
    sdma::trap(&mut v, 0x0ABC_DEF0).unwrap();
    sdma::cond_exe(&mut v, 0x80, 1, 12).unwrap();
    let p = sdma::PollRegMem {
        hdp_flush: false,
        mem: true,
        function: 5,
        addr_lo: 0x200,
        addr_hi: 0,
        reference: 3,
        mask: 0xF,
        interval: 10,
        retry: 0xFFF,
    };
    sdma::poll_regmem(&mut v, &p).unwrap();
    let mut i = 0;
    let mut seen = 0;
    while i < v.len() {
        let (pk, n) = sdma::decode(&v[i..]).unwrap();
        match pk {
            S::WriteLinear { addr, data } => assert_eq!((addr, data.len()), (0x40, 9)),
            S::CopyLinear { src, dst, bytes, .. } => assert_eq!((src, dst, bytes), (0x11, 0x23, 5)),
            S::ConstFill { dst, data, bytes, fillsize } => assert_eq!((dst, data, bytes, fillsize), (0x100, 0x55, 8, 2)),
            S::Trap { ctx } => assert_eq!(ctx, 0x0ABC_DEF0),
            S::CondExe { addr, reference, count } => assert_eq!((addr, reference, count), (0x80, 1, 12)),
            S::PollRegMem(d) => assert_eq!(d, p),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!std::format!("{pk}").is_empty());
        seen += 1;
        i += n;
    }
    assert_eq!((i, seen), (v.len(), 6));
}

#[test]
fn sdma_encoders_refuse_and_append_nothing() {
    let mut v = Vec::new();
    assert!(sdma::write_linear(&mut v, 2, &[1]).is_err());
    assert!(sdma::write_linear(&mut v, 0, &[]).is_err());
    assert!(sdma::write_linear(&mut v, 0, &std::vec![0; 0x10_0001]).is_err());
    assert!(sdma::copy_linear(&mut v, 0, 0, 0, false).is_err());
    assert!(sdma::copy_linear(&mut v, 0, 0, (1 << 30) + 1, false).is_err());
    assert!(sdma::const_fill(&mut v, 2, 0, 4, sdma::FILL_DWORD).is_err());
    assert!(sdma::const_fill(&mut v, 0, 0, 6, sdma::FILL_DWORD).is_err());
    assert!(sdma::const_fill(&mut v, 0, 0, 4, 1).is_err());
    assert!(sdma::fence(&mut v, 6, 0).is_err());
    assert!(sdma::emit_fence(&mut v, 2, 0, true, false).is_err());
    assert!(sdma::indirect(&mut v, 0, 0x10, 8, 0).is_err());
    assert!(sdma::indirect(&mut v, 16, 0, 8, 0).is_err());
    assert!(sdma::trap(&mut v, 1 << 28).is_err());
    assert!(sdma::nops(&mut v, 0x4001, true).is_err());
    assert!(v.is_empty());
    sdma::nops(&mut v, 0x4000, true).unwrap();
    assert_eq!(sdma::packet_len(&v), Ok(0x4000));
}

#[test]
fn sdma_unknown_and_truncated() {
    assert_eq!(sdma::packet_len(&[0x0000_0003]), Err(sdma::DecodeError::Unknown { header: 3 }));
    assert_eq!(sdma::packet_len(&[0x0000_0101]), Err(sdma::DecodeError::Unknown { header: 0x101 }), "tiled copy");
    assert_eq!(sdma::packet_len(&[2, 0]), Err(sdma::DecodeError::Truncated { need: 4, have: 2 }));
    assert_eq!(sdma::decode(&[2, 0, 0, 3, 1]), Err(sdma::DecodeError::Truncated { need: 8, have: 5 }));
    // PTEPDE decodes (for dumps) although nothing executes it.
    let w = [12, 0x1000, 0, 0xff, 0, 0x2000, 0, 0x1000, 0, 3];
    let (p, n) = sdma::decode(&w).unwrap();
    assert_eq!(n, 10);
    assert_eq!(p, S::PtePde { pe: 0x1000, mask: 0xff, value: 0x2000, incr: 0x1000, count: 4 });
}
