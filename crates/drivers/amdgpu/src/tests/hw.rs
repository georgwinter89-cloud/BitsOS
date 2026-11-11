//! GX-8a: the hardware backend's memory half on the host (the ktests' checks, `ktests_hw`) and the tool's argument.

use crate::gpucore::fakechip;
use crate::ktests_hw as k;
use crate::psplive::{Action, parse_args};

#[test]
fn info_from_the_chip() {
    k::check_info(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn harvest_and_another_chip() {
    k::check_harvest(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn firmware_table() {
    k::check_firmware().unwrap();
}

#[test]
fn vram_allocator() {
    k::check_vram().unwrap();
}

#[test]
fn an_object_over_two_slabs() {
    k::check_two_slabs().unwrap();
}

#[test]
fn gtt_pool() {
    k::check_gtt().unwrap();
}

#[test]
fn tables_walk_through_the_uapi() {
    k::check_tables(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn vmids_per_job() {
    k::check_vmid().unwrap();
}

#[test]
fn page_table_memory_outlives_its_jobs() {
    k::check_pt_memory().unwrap();
}

#[test]
fn updates_keep_their_order() {
    k::check_order().unwrap();
}

#[test]
fn failed_stores_and_foreign_memory() {
    k::check_store_failure().unwrap();
}

#[test]
fn the_proof_of_a_bring_up() {
    k::check_proof(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn hostile_gem_va_answers_as_linux() {
    k::check_va_hostile().unwrap();
}

#[test]
fn table_reserve_runs_out_cleanly() {
    k::check_table_reserve().unwrap();
}

#[test]
fn free_slabs_go_back_window_first() {
    k::check_slabs_back().unwrap();
}

#[test]
fn wipes_are_drained_and_in_chunks() {
    k::check_wipe().unwrap();
}

#[test]
fn entries_hold_their_frames_over_a_revalidation() {
    k::check_revalidate().unwrap();
}

#[test]
fn until_backend_parses() {
    let a = parse_args(&mcp::Json::parse(r#"{"until":"backend"}"#).unwrap()).unwrap();
    assert_eq!(a.action, Action::Backend);
    let a = parse_args(&mcp::Json::parse(r#"{"until":"jobs"}"#).unwrap()).unwrap();
    assert_eq!(a.action, Action::Jobs);
    assert!(parse_args(&mcp::Json::parse(r#"{"until":"nothing"}"#).unwrap()).unwrap_err().contains("backend"));
}

// ---- GX-8b: the job half, inline (`Jobs::kick` does a round), on the fake chain

mod jobs {
    use crate::ktests_hw_jobs as k;

    #[test]
    fn proof() {
        k::proof().unwrap();
    }
    #[test]
    fn many_jobs_wrap_the_rings() {
        k::many().unwrap();
    }
    #[test]
    fn user_fence() {
        k::user_fence().unwrap();
    }
    #[test]
    fn dependencies() {
        k::deps().unwrap();
    }
    #[test]
    fn frames_on_the_rings() {
        k::frames().unwrap();
    }
    #[test]
    fn flush_rule() {
        k::flush_rule().unwrap();
    }
    #[test]
    fn more_vms_than_vmids() {
        k::vmids().unwrap();
    }
    #[test]
    fn refused() {
        k::refused().unwrap();
    }
    #[test]
    fn fault_goes_on() {
        k::fault_goes_on().unwrap();
    }
    #[test]
    fn fault_stops_only_that_job() {
        k::fault_stops_job().unwrap();
    }
    #[test]
    fn hang_gfx() {
        k::hang_gfx().unwrap();
    }
    #[test]
    fn hang_compute() {
        k::hang_compute().unwrap();
    }
    #[test]
    fn idle_and_stop() {
        k::idle_and_stop().unwrap();
    }
    #[test]
    fn gang_keeps_a_lost_flush() {
        crate::ktests_hw_lost::gang_flush().unwrap();
    }
    #[test]
    fn a_frame_that_may_run_keeps_its_memory() {
        crate::ktests_hw_lost::doorbell_refused().unwrap();
    }
    #[test]
    fn ladder_soft() {
        crate::ktests_hw_ladder::soft().unwrap();
    }
    #[test]
    fn ladder_queue_gfx() {
        crate::ktests_hw_ladder::queue_gfx().unwrap();
    }
    #[test]
    fn ladder_queue_compute() {
        crate::ktests_hw_ladder::queue_compute().unwrap();
    }
    #[test]
    fn ladder_queue_sdma() {
        crate::ktests_hw_ladder::queue_sdma().unwrap();
    }
    #[test]
    fn ladder_gfx_hqd() {
        crate::ktests_hw_ladder::gfx_hqd().unwrap();
    }
    #[test]
    fn ladder_queue_dead() {
        crate::ktests_hw_ladder::queue_dead().unwrap();
    }
    #[test]
    fn ladder_unmap_wedged() {
        crate::ktests_hw_ladder::unmap_wedged().unwrap();
    }
    #[test]
    fn ladder_gc() {
        crate::ktests_hw_ladder::gc().unwrap();
    }
    #[test]
    fn ladder_gc_limit() {
        crate::ktests_hw_ladder::gc_limit().unwrap();
    }
    #[test]
    fn ladder_mode2_unproven() {
        crate::ktests_hw_ladder::mode2_unproven().unwrap();
    }
    #[test]
    fn ladder_halt_proof() {
        crate::ktests_hw_ladder::halt_proof().unwrap();
    }
    #[test]
    fn ladder_gc_fails() {
        crate::ktests_hw_ladder::gc_fails().unwrap();
    }
    #[test]
    fn ladder_mode2_closed() {
        crate::ktests_hw_ladder::mode2_closed().unwrap();
    }
    #[test]
    fn ladder_mode2_open() {
        crate::ktests_hw_ladder::mode2_open().unwrap();
    }
    #[test]
    fn ladder_mode2_no_answer() {
        crate::ktests_hw_ladder::mode2_no_answer().unwrap();
    }
    #[test]
    fn ladder_picture_moved() {
        crate::ktests_hw_ladder::picture_moved().unwrap();
    }
    #[test]
    fn ladder_vram_lost() {
        crate::ktests_hw_ladder::vram_lost().unwrap();
    }
    #[test]
    fn ladder_tool() {
        crate::ktests_hw_ladder::tool_word().unwrap();
    }
    #[test]
    fn ladder_config() {
        crate::ktests_hw_ladder::config().unwrap();
    }
    #[test]
    fn a_hang_the_ladder_refuses_still_ends() {
        crate::ktests_hw_lost::ladder_refuses().unwrap();
    }
    #[test]
    fn another_clients_fault_is_no_stop() {
        crate::ktests_hw_lost::other_client_fault().unwrap();
    }
    #[test]
    fn a_hang_with_two_vms_in_flight() {
        crate::ktests_hw_lost::hang_two_vms().unwrap();
    }
    #[test]
    fn held_jobs_after_a_stop() {
        crate::ktests_hw_lost::held_after_stop().unwrap();
    }
    #[test]
    fn many_queued_stay_in_order_per_entity() {
        crate::ktests_hw_lost::queue_order().unwrap();
    }
    #[test]
    fn a_stall_counts_only_while_the_queue_stands() {
        use crate::hw::rings::Stall;
        use amdgpu_fmt::watch::Sample;
        let a = Sample { rptr_hw: 0x100, fence: 7 };
        let mut st = Stall { at_us: 0, addr: 0, write: true, vmid: 1, seq: 8, seen: Some(a) };
        assert!(!st.stopped(4_000, a, 5_000));
        // The queue went on: the count starts again.
        let b = Sample { rptr_hw: 0x140, fence: 7 };
        assert!(!st.stopped(6_000, b, 5_000));
        assert!(!st.stopped(10_000, b, 5_000));
        assert!(st.stopped(11_000, b, 5_000));
        // A fence that moved counts as well, and a fault seen with no sample starts at the first one.
        let c = Sample { rptr_hw: 0x140, fence: 8 };
        assert!(!st.stopped(12_000, c, 5_000));
        let mut none = Stall { at_us: 0, addr: 0, write: true, vmid: 1, seq: 8, seen: None };
        assert!(!none.stopped(100_000, a, 5_000));
        assert!(none.stopped(105_000, a, 5_000));
    }
}

// ---- GX-8b: a job's frame, as amdgpu_ib_schedule orders it

mod frames {
    use crate::gpucore::disc;
    use crate::gpucore::fakechip;
    use crate::gpucore::vmring::VmAddrs;
    use crate::hw::frame::{Frame, RingType, check_ibs};
    use alloc::vec::Vec;
    use amdgpu_fmt::hang::decode_stream;
    use amdgpu_fmt::pm4;
    use amdgpu_fmt::ring::{Kind, Ring};
    use amdgpu_fmt::sdma;
    use amdgpu_fmt::sim::Isa;
    use drm::job::JobIb;
    use drm::uapi::*;

    const SLOT: u64 = 0x80_0006_a000;
    const PD: u64 = 0x80_0000_1001;

    fn addrs() -> VmAddrs {
        let d: disc::Discovery = fakechip::discovery_780m();
        VmAddrs::from_discovery(&d).expect("addresses")
    }

    fn words(f: &Frame, kind: Kind) -> Vec<u32> {
        let head = f.head().expect("head");
        let mut g = Ring::new(kind, 2048, 0).expect("ring");
        g.begin(f.bound(&head), 0).expect("room");
        f.emit(&head, &mut g).expect("emit");
        let mut out = alloc::vec![0u32; 2048];
        let end = g.commit(&mut out).expect("commit");
        out.truncate(end as usize);
        out
    }

    /// The opcodes of the stream in order, the padding NOPs left out.
    fn ops(w: &[u32]) -> Vec<u8> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < w.len() {
            let h = w[i];
            if h == pm4::PAD {
                i += 1;
                continue;
            }
            v.push(((h >> 8) & 0xff) as u8);
            i += ((h >> 16) & 0x3fff) as usize + 2;
        }
        v
    }

    fn frame<'a>(ty: RingType, a: &'a VmAddrs, ibs: &'a [JobIb], flush: bool, sync: bool, switch: bool) -> Frame<'a> {
        Frame { ty, addrs: a, ibs, vmid: 3, flush: flush.then_some((PD, 0x8001)), sync: sync.then_some((SLOT, 41)), ctx_switch: switch, fence: (SLOT, 42), max_wave_id: 383, int: false }
    }

    #[test]
    fn gfx_frame_in_linux_order() {
        use pm4::op::*;
        let a = addrs();
        let ibs = [JobIb { va: 0x1_0000_0000, len_dw: 16, flags: 0 }, JobIb { va: 0x1_0000_1000, len_dw: 8, flags: AMDGPU_IB_FLAG_PREAMBLE }];
        let w = words(&frame(RingType::Gfx, &a, &ibs, true, true, true), Kind::Gfx);
        assert_eq!(
            ops(&w),
            [WAIT_REG_MEM, WRITE_DATA, WRITE_DATA, WAIT_REG_MEM, PFP_SYNC_ME, WRITE_DATA, CONTEXT_CONTROL, FRAME_CONTROL, INDIRECT_BUFFER, INDIRECT_BUFFER, FRAME_CONTROL, RELEASE_MEM]
        );
        // The sync waits on the fence slot, from the PFP, for the last number (gfx_v11_0.c:5395).
        assert_eq!(&w[0..7], &[pm4::packet3(WAIT_REG_MEM, 5), 0x13 | 1 << 8, SLOT as u32, (SLOT >> 32) as u32, 41, u32::MAX, 4]);
        // gfx_v11_0_ring_emit_cntxcntl with a context switch; no VALID bit on GFX's IB, the VMID in its control.
        assert!(w.windows(2).any(|x| x[0] == pm4::packet3(CONTEXT_CONTROL, 1) && x[1] == 0x8101_8003));
        assert!(w.windows(4).any(|x| x[0] == pm4::packet3(INDIRECT_BUFFER, 2) && x[3] == (16 | 3 << 24)));
        // The 64-bit fence with Linux's cache bits, the frame padded to 256 dwords.
        assert!(w.windows(5).any(|x| x[0] == pm4::packet3(RELEASE_MEM, 6) && x[1] == pm4::rm::GFX11_FENCE && x[2] == 2 << 29 && x[3] == SLOT as u32));
        assert_eq!(w.len() % 256, 0);
    }

    #[test]
    fn no_flush_no_sync_no_switch() {
        use pm4::op::*;
        let a = addrs();
        let ibs = [JobIb { va: 0x1_0000_0000, len_dw: 16, flags: 0 }];
        let w = words(&frame(RingType::Gfx, &a, &ibs, false, false, false), Kind::Gfx);
        assert_eq!(ops(&w), [CONTEXT_CONTROL, FRAME_CONTROL, INDIRECT_BUFFER, FRAME_CONTROL, RELEASE_MEM]);
        assert!(w.windows(2).any(|x| x[0] == pm4::packet3(CONTEXT_CONTROL, 1) && x[1] == 0x8000_0000));
    }

    #[test]
    fn compute_frame_has_no_gfx_parts() {
        use pm4::op::*;
        let a = addrs();
        let ibs = [JobIb { va: 0x1_0000_0000, len_dw: 16, flags: AMDGPU_IB_FLAG_RESET_GDS_MAX_WAVE_ID | AMDGPU_IB_FLAG_EMIT_MEM_SYNC }];
        let w = words(&frame(RingType::Compute, &a, &ibs, true, true, true), Kind::Gfx);
        assert_eq!(ops(&w), [ACQUIRE_MEM, WAIT_REG_MEM, WRITE_DATA, WRITE_DATA, WAIT_REG_MEM, WRITE_DATA, SET_CONFIG_REG, INDIRECT_BUFFER, RELEASE_MEM]);
        assert!(w.windows(4).any(|x| x[0] == pm4::packet3(INDIRECT_BUFFER, 2) && x[3] == (16 | pm4::IB_VALID | 3 << 24)));
        assert!(w.windows(3).any(|x| x[0] == pm4::packet3(SET_CONFIG_REG, 1) && x[1] == 0x20e8 && x[2] == 383));
    }

    #[test]
    fn compute_ib_in_the_upper_half_goes_out_sign_extended() {
        // RADV's IBs live above the hole. Without the upper bits MEC drops the job with SUA_ERROR (measured, 4.11.).
        let a = addrs();
        let ibs = [JobIb { va: 0x8001_000a_9000, len_dw: 56, flags: 0 }];
        let w = words(&frame(RingType::Compute, &a, &ibs, false, false, false), Kind::Gfx);
        let at = w.iter().position(|&x| x == pm4::packet3(pm4::op::INDIRECT_BUFFER, 2)).expect("indirect");
        assert_eq!((w[at + 1], w[at + 2]), (0x000a_9000, 0xffff_8001));
    }

    #[test]
    fn sdma_and_gfx_ibs_in_the_upper_half_go_out_sign_extended() {
        // As compute's: SDMA stops inside such an IB for good, no fault and no fence (measured, 5.11.: every RADV copy hung).
        let a = addrs();
        let ibs = [JobIb { va: 0x8001_000a_9000, len_dw: 32, flags: 0 }];
        for (ty, kind) in [(RingType::Sdma, Kind::Sdma { burst_nop: false }), (RingType::Gfx, Kind::Gfx)] {
            let w = words(&frame(ty, &a, &ibs, false, false, false), kind);
            assert!(w.windows(2).any(|x| x == [0x000a_9000, 0xffff_8001]), "{ty:?}: {w:x?}");
        }
    }

    #[test]
    fn sdma_frame_decodes_and_ends_on_its_boundary() {
        let a = addrs();
        let ibs = [JobIb { va: 0x1_0000_0004, len_dw: 16, flags: AMDGPU_IB_FLAG_EMIT_MEM_SYNC }];
        let kind = Kind::Sdma { burst_nop: false };
        let w = words(&frame(RingType::Sdma, &a, &ibs, true, true, false), kind);
        let lines = decode_stream(Isa::Sdma, &w, 0);
        let names: Vec<alloc::string::String> = lines.iter().map(|l| alloc::format!("{l}")).collect();
        let text = names.join("\n");
        assert!(text.find("GCR_REQ").unwrap() < text.find("POLL_REGMEM").unwrap(), "{text}");
        assert!(text.find("POLL_REGMEM").unwrap() < text.find("SRBM_WRITE").unwrap(), "{text}");
        assert!(text.find("SRBM_WRITE").unwrap() < text.find("INDIRECT").unwrap(), "{text}");
        assert!(text.rfind("FENCE").unwrap() > text.find("INDIRECT").unwrap(), "{text}");
        // The IB's low bits masked (sdma_v6_0.c:267), its packet ending on an 8-dword boundary.
        let at = w.iter().position(|&x| x == sdma::header(sdma::op::INDIRECT, 0) | 3 << 16).expect("indirect");
        assert_eq!(w[at + 1], 0);
        assert_eq!((at + 6) % 8, 0);
        // Two FENCE packets: 64 bits.
        assert_eq!(w.iter().filter(|&&x| x == sdma::header(sdma::op::FENCE, 0) | 3 << 16).count(), 2);
    }

    #[test]
    fn ibs_a_ring_cannot_run_are_the_programs_error() {
        let bad = [JobIb { va: 0x1_0000_0002, len_dw: 16, flags: 0 }];
        assert!(check_ibs(RingType::Gfx, &bad).is_err() && check_ibs(RingType::Compute, &bad).is_err());
        assert!(check_ibs(RingType::Sdma, &bad).is_ok());
        let far = [JobIb { va: 1 << 48, len_dw: 16, flags: 0 }];
        assert!(RingType::ALL.iter().all(|t| check_ibs(*t, &far).is_err()));
        let long = [JobIb { va: 0x1_0000_0000, len_dw: 1 << 20, flags: 0 }];
        assert!(RingType::ALL.iter().all(|t| check_ibs(*t, &long).is_err()));
        assert_eq!(RingType::of_ip(AMDGPU_HW_IP_VCN_ENC), None);
    }
}
