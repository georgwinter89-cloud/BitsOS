//! Wave 43: SDMA 7.0.1's arithmetic against what the trace of Linux's own start shows on this card
//! (plan/hw/gaming-pc-gpu-2026-11-18, init.txt lines 9604-9671): every register lies at the dword the trace names,
//! and every value a function here computes from what the card answered is the value Linux wrote.

use crate::ipfacts::TABLE;
use crate::regmap::{dword, reach_of, Reach};
use crate::sdma7::{self as s, f};

#[test]
fn both_engines_registers_lie_where_the_trace_names_them() {
    // (register, engine 0's dword, engine 1's dword), init.txt lines 9604-9671.
    for (r, d0, d1) in [
        (s::MCU_CNTL, 0xF88E, 0xF8AE),
        (s::RB_CNTL, 0x12E0, 0x18E0),
        (s::RB_BASE, 0x12E1, 0x18E1),
        (s::RB_BASE_HI, 0x12E2, 0x18E2),
        (s::RB_RPTR, 0x12E3, 0x18E3),
        (s::RB_RPTR_HI, 0x12E4, 0x18E4),
        (s::RB_WPTR, 0x12E5, 0x18E5),
        (s::RB_WPTR_HI, 0x12E6, 0x18E6),
        (s::RB_RPTR_ADDR_LO, 0x12E7, 0x18E7),
        (s::RB_RPTR_ADDR_HI, 0x12E8, 0x18E8),
        (s::IB_CNTL, 0x12E9, 0x18E9),
        (s::DOORBELL, 0x12EF, 0x18EF),
        (s::DOORBELL_OFFSET, 0x12F1, 0x18F1),
        (s::RB_WPTR_POLL_ADDR_LO, 0x12F8, 0x18F8),
        (s::RB_WPTR_POLL_ADDR_HI, 0x12F9, 0x18F9),
        (s::MINOR_PTR_UPDATE, 0x12FB, 0x18FB),
        (s::WATCHDOG_CNTL, 0x128B, 0x188B),
        (s::UTCL1_CNTL, 0x1295, 0x1895),
        (s::UTCL1_PAGE, 0x1298, 0x1898),
        // Late init's trap enable (init.txt lines 9823-9826).
        (s::CNTL, 0x126D, 0x186D),
    ] {
        assert_eq!((dword(&TABLE, s::instance(r, 0)), dword(&TABLE, s::instance(r, 1))), (Some(d0), Some(d1)), "{}", r.name);
    }
    assert_eq!(dword(&TABLE, s::DOORBELL_ENTRY_2), Some(0xEED), "init.txt line 9627");
    // The in-ring HDP flush's registers: NBIF base 2 (0xD20) + 0x106 / 0x107.
    assert_eq!((dword(&TABLE, s::GPU_HDP_FLUSH_REQ), dword(&TABLE, s::GPU_HDP_FLUSH_DONE)), (Some(0xE26), Some(0xE27)));
    // Everything of both engines lies inside BAR5: no index pair.
    for r in s::ALL {
        for i in 0..s::INSTANCES {
            let dw = dword(&TABLE, s::instance(r, i)).expect("the table places GC");
            assert!(matches!(reach_of(dw), Reach::Bar5(_)), "{} of engine {i}", r.name);
        }
    }
}

#[test]
fn only_the_hypervisor_decode_range_moves_by_0x20() {
    assert_eq!(s::instance(s::MCU_CNTL, 1).offset - s::MCU_CNTL.offset, 0x20);
    assert_eq!(s::instance(s::RB_CNTL, 1).offset - s::RB_CNTL.offset, 0x600);
    assert_eq!(s::instance(s::RB_CNTL, 0), s::RB_CNTL);
    // The range's ends are inside it (sdma_v7_0.c:130-131).
    let at = |offset| crate::regmap::Reg { offset, ..s::MCU_CNTL };
    assert_eq!(s::instance(at(s::HYP_DEC_REG_START), 1).offset, s::HYP_DEC_REG_START + 0x20);
    assert_eq!(s::instance(at(s::HYP_DEC_REG_END), 1).offset, s::HYP_DEC_REG_END + 0x20);
    assert_eq!(s::instance(at(s::HYP_DEC_REG_END + 1), 1).offset, s::HYP_DEC_REG_END + 1 + 0x600);
}

#[test]
fn the_values_are_the_ones_linux_wrote_for_what_the_card_answered() {
    // RB_CNTL: read 0x00040800 (9608), written 0x00840816 (9609), then 0x00841817 with the enable (9638).
    let sized = s::rb_cntl_sized(0x0004_0800);
    assert_eq!(sized, 0x0084_0816);
    assert_eq!(s::rb_cntl_run(sized, false), 0x0084_1816);
    assert_eq!(s::rb_cntl_run(sized, true), 0x0084_1817);
    assert_eq!((f::RB_SIZE.get(sized), 4u32 << f::RB_SIZE.get(sized)), (11, s::RING_BYTES), "the ring's size as Linux computes it");
    // A driver's own poll bit left on by someone is taken off, the MCU's stays (:520-526).
    assert_eq!(s::rb_cntl_run(sized | f::WPTR_POLL_ENABLE.mask, false), 0x0084_1816);
    // The doorbell and its offset: read 0 (9623-9624), written 0x10000000 and 0x800 / 0x850 (9625-9626, 9658-9659).
    assert_eq!(s::doorbell(0), 0x1000_0000);
    assert_eq!((s::doorbell_dw(0), s::doorbell_dw(1)), (0x200, 0x214));
    assert_eq!((s::doorbell_offset(0, s::doorbell_dw(0)), s::doorbell_offset(0, s::doorbell_dw(1))), (0x800, 0x850));
    // NBIF's entry: read 0, written 0x3051001d (9627-9628).
    assert_eq!(s::doorbell_range(0), 0x3051_001D);
    assert_eq!((s::PORT2_RANGE_OFFSET.get(0x3051_001D), s::PORT2_RANGE_SIZE.get(0x3051_001D)), (0x200, 40));
    // Watchdog 0 -> 1 (9630-9631), UTCL1_CNTL 0x3b000288 -> 0x3b000689 (9632-9633), UTCL1_PAGE unchanged (9634-9635).
    assert_eq!(s::watchdog(0), 1);
    assert_eq!(s::utcl1_cntl(0x3B00_0288), 0x3B00_0689);
    assert_eq!(s::utcl1_page(0x000C_EC20), 0x000C_EC20);
    assert_eq!(s::utcl1_page(0x010C_0C20), 0x000C_EC20, "the policies are set, the bits above the register's 24 dropped");
    // MCU_CNTL 0 -> 0 (9604-9607, 9636-9637), IB_CNTL 0x100 -> 0x101 (9639-9640).
    assert_eq!((s::mcu_halt(0, false), s::mcu_unhalt(0), s::mcu_unhalt(3)), (0, 0, 0));
    assert_eq!(s::ib_cntl(0x100, true), 0x101);
}

#[test]
fn a_stop_is_linuxs_stop() {
    // sdma_v7_0_gfx_stop and sdma_v7_0_enable(false): the ring and the IBs off, then the halt.
    assert_eq!(s::rb_cntl_stopped(0x0084_1817), 0x0084_1816);
    assert_eq!(s::ib_cntl(0x101, false), 0x100);
    assert_eq!(s::mcu_halt(0, true), 1);
}

#[test]
fn a_trap_names_its_engine_as_the_load_trace_shows() {
    // trace-load.txt: client 10, source 49, ring 0 (SDMA0, 1095 entries) and ring 16 (SDMA1, 99 entries).
    assert_eq!((s::CLIENT_GFX, s::SRC_TRAP), (10, 49));
    assert_eq!((s::trap_ring_id(0), s::trap_ring_id(1)), (0, 16));
    assert_eq!((s::trap_of(0), s::trap_of(16), s::trap_of(0x11)), (Some((0, 0)), Some((1, 0)), Some((1, 1))));
    assert_eq!(s::trap_of(0x20), None, "an engine this card does not have");
    assert_eq!((s::hdp_flush_bit(0), s::hdp_flush_bit(1)), (0x400, 0x800));
}
