//! The strides against every generated instance, and V1's hex constants (dcnregs.asm) against the header.

use crate::layout as l;
use dcn314_regs::lookup;

fn at(name: &str) -> u32 {
    lookup(name).unwrap_or_else(|| panic!("{name} not in the header")).byte
}

#[test]
fn strides_match_every_instance() {
    for p in 0..l::PIPES {
        assert_eq!(l::hubp(p, l::HUBP_CNTL), at(&alloc::format!("HUBP{p}_DCHUBP_CNTL")));
        assert_eq!(l::hubp(p, l::HUBP_ADDR_HI), at(&alloc::format!("HUBPREQ{p}_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH")));
        assert_eq!(l::hubp(p, l::HUBP_INUSE_HI), at(&alloc::format!("HUBPREQ{p}_DCSURF_SURFACE_EARLIEST_INUSE_HIGH")));
        assert_eq!(l::hubp(p, l::HUBP_VIEWPORT), at(&alloc::format!("HUBP{p}_DCSURF_PRI_VIEWPORT_DIMENSION")));
        assert_eq!(l::pg_status(p), at(&alloc::format!("DOMAIN{p}_PG_STATUS")));
        // GX-5a: the cursor blocks in the HUBP, DPP and MPCC strides, the cursor lock per OPP.
        assert_eq!(l::hubp(p, l::CUR_CONTROL), at(&alloc::format!("CURSOR0_{p}_CURSOR_CONTROL")));
        assert_eq!(l::hubp(p, l::CUR_DST_OFFSET), at(&alloc::format!("CURSOR0_{p}_CURSOR_DST_OFFSET")));
        assert_eq!(l::hubp(p, l::CUR_SETTINGS), at(&alloc::format!("HUBPREQ{p}_CURSOR_SETTINGS")));
        assert_eq!(l::hubp(p, l::REF_FREQ_TO_PIX_FREQ), at(&alloc::format!("HUBPREQ{p}_REF_FREQ_TO_PIX_FREQ")));
        assert_eq!(l::dpp(p, l::CNVC_CUR_CONTROL), at(&alloc::format!("CNVC_CUR{p}_CURSOR0_CONTROL")));
        assert_eq!(l::dpp(p, l::CNVC_CUR_FP_SCALE_BIAS), at(&alloc::format!("CNVC_CUR{p}_CURSOR0_FP_SCALE_BIAS")));
        assert_eq!(l::mpcc(p, l::MPCC_OPP_ID), at(&alloc::format!("MPCC{p}_MPCC_OPP_ID")));
        assert_eq!(l::cur_lock(p), at(&alloc::format!("CUR_VUPDATE_LOCK_SET{p}")));
    }
    for k in 0..l::OTGS {
        assert_eq!(l::otg(k, l::OTG_CONTROL), at(&alloc::format!("OTG{k}_OTG_CONTROL")));
        assert_eq!(l::otg(k, l::OTG_LOCK), at(&alloc::format!("OTG{k}_OTG_MASTER_UPDATE_LOCK")));
        assert_eq!(l::otg(k, l::OTG_V_BLANK), at(&alloc::format!("OTG{k}_OTG_V_BLANK_START_END")));
        assert_eq!(l::dto_cntl(k), at(&alloc::format!("OTG{k}_PIXEL_RATE_CNTL")));
        assert_eq!(l::dto_phase(k), at(&alloc::format!("DP_DTO{k}_PHASE")));
        assert_eq!(l::dto_modulo(k), at(&alloc::format!("DP_DTO{k}_MODULO")));
    }
    for d in 0..l::DIGS {
        assert_eq!(l::dig(d, l::DIG_BE_CNTL), at(&alloc::format!("DIG{d}_DIG_BE_CNTL")));
        assert_eq!(l::dig(d, l::DIG_BE_EN_CNTL), at(&alloc::format!("DIG{d}_DIG_BE_EN_CNTL")));
        assert_eq!(l::dp(d, l::DP_VID_STREAM_CNTL), at(&alloc::format!("DP{d}_DP_VID_STREAM_CNTL")));
    }
}

#[test]
fn v1_constants() {
    const S1: u32 = 0xC0;
    const S2: u32 = 0x34C0;
    assert_eq!((l::MEMSIZE, l::APER_SIZE), (0xDE3 * 4, 0xDE7 * 4));
    assert_eq!((l::FB_BASE, l::FB_TOP), ((S2 + 0x475) * 4, (S2 + 0x476) * 4));
    assert_eq!(l::pg_status(0), (S2 + 0x81) * 4);
    assert_eq!(l::HUBP_STRIDE, 0xDC * 4);
    for (r, d) in [
        (l::HUBP_CNTL, 0x5F3),
        (l::HUBP_VMID, 0x609),
        (l::HUBP_ADDR, 0x60A),
        (l::HUBP_ADDR_HI, 0x60B),
        (l::HUBP_SURF, 0x61A),
        (l::HUBP_FLIP, 0x61B),
        (l::HUBP_FLIP2, 0x61C),
        (l::HUBP_INUSE, 0x625),
        (l::HUBP_INUSE_HI, 0x626),
    ] {
        assert_eq!(r, (S2 + d) * 4);
    }
    assert_eq!((l::OTG_STRIDE, l::OTG_CONTROL, l::OTG_LOCK), (0x80 * 4, (S2 + 0x1B41) * 4, (S2 + 0x1B8A) * 4));
    assert_eq!((l::dto_cntl(0), l::DTO_STRIDE), ((S1 + 0x80) * 4, 16));
    assert_eq!(l::DMCUB_CNTL, (S2 + 0x1F6) * 4);
    for (n, w) in l::DMCUB_WINDOWS.iter().enumerate() {
        if n < 8 {
            let n = n as u32;
            assert_eq!(
                (w.top, w.base, w.offset),
                ((S2 + 0x1AD + n) * 4, Some((S2 + 0x1A5 + n) * 4), (S2 + 0x1B5 + 2 * n) * 4),
                "{}",
                w.name
            );
        } else {
            let r: u32 = w.name[6..].parse().unwrap();
            assert_eq!(
                (w.top, w.base, w.offset),
                ((S2 + 0x19E + r - u32::from(r > 3)) * 4, None, (S2 + 0x18E + 2 * r) * 4),
                "{}",
                w.name
            );
        }
        assert_eq!(w.offset_high, w.offset + 4, "{}", w.name);
    }
    assert_eq!(
        (l::BLANKED, l::FLIP_TYPE, l::FLIP_PENDING, l::DMCUB_ENABLE, l::PG_GATED),
        (1, 2, 0x100, 0x10000, 0xC000_0000)
    );
}
