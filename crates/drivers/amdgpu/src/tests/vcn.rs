//! GX-8v: the VCN stage's ktests run on the host (the same functions), and the pure parts.

use crate::gpucore::fakechip::discovery_780m;
use crate::gpucore::vcn::{self, VcnAddrs};
use crate::ipmod::vcn4::{self, R};
use crate::ktests_vcn as k;

#[test]
fn stage_passes_on_the_fakes() {
    k::check_starts(&discovery_780m()).unwrap();
}

#[test]
fn video_messages_stay_on_their_list() {
    k::check_lists(&discovery_780m()).unwrap();
}

#[test]
fn failures_end_powered_down() {
    k::check_failures(&discovery_780m()).unwrap();
}

#[test]
fn stop_start_and_quiesce() {
    k::check_again(&discovery_780m()).unwrap();
}

#[test]
fn addresses_are_the_780m_capture() {
    let a = VcnAddrs::from_discovery(&discovery_780m()).unwrap();
    // The capture's dwords: UVD_POWER_STATUS 0x7e02, UVD_RB_BASE_LO 0x7eea, VCN_RB1_DB_CTRL 0x7e72, UVD_STATUS 0x7f43.
    assert_eq!(a.byte(R::PowerStatus), Some(0x7e02 * 4));
    assert_eq!(a.byte(R::RbBaseLo), Some(0x7eea * 4));
    assert_eq!(a.byte(R::Rb1DbCtrl), Some(0x7e72 * 4));
    assert_eq!(a.byte(R::Status), Some(0x7f43 * 4));
    assert_eq!(vcn4::rb1_db_ctrl(vcn4::DOORBELL_DW), 0x4000_0c48);
    assert_eq!(vcn4::doorbell_range(0, vcn4::DOORBELL_RANGE_START_DW, 8), 0x0008_0c40);
    // Nothing of VCN lies in DCN's segments or MMHUB's display fields.
    let g = crate::gpucore::watch::DisplayGuard::from_discovery(&discovery_780m());
    assert!(a.readable().iter().all(|&b| g.class(b).is_none()));
    // The SRAM list's offsets are the VCPU's (inside the video range).
    let l = vcn::Layout::new(0x80_0010_0000);
    let s = vcn::sram_list(&a, &l, 0x242).unwrap();
    assert_eq!(s.0.len(), 54);
    assert_eq!(s.0[0], 0xd6);
}

#[test]
fn a_program_decodes_through_the_job_half() {
    crate::ktests_vcn_jobs::check_jobs().unwrap();
}

#[test]
fn a_hang_ends_every_job() {
    crate::ktests_vcn_jobs::check_hang().unwrap();
}

#[test]
fn the_tool_has_no_vcn_stage() {
    // VCN starts with until=backend at every boot; the tool has no stage of its own that would run it twice.
    use crate::psplive::parse_args;
    assert!(parse_args(&mcp::Json::parse(r#"{"until":"vcn"}"#).unwrap()).is_err());
    frame_checker();
}

/// The frame checker takes MMHUB's context bases, engine 0 and LUT_MM only, and the request's and semaphore's values.
fn frame_checker() {
    use crate::gpucore::vcnvm::{self, ring_ok};
    use crate::ipmod::gmc11::{self, R};
    use amdgpu_fmt::vcn::reg_write;
    let d = discovery_780m();
    let mm = crate::gpucore::gmc::HubAddrs::from_discovery(&d, gmc11::Hub::Mm).unwrap();
    let lut = VcnAddrs::from_discovery(&d).unwrap().lut_mm;
    // osssys_6_0_0_offset.h: IH_VMID_0_LUT_MM is dword 0x10 of OSSSYS segment 0, 16 after IH_VMID_0_LUT.
    assert_eq!(lut, d.reg(crate::gpucore::hwid::OSSSYS, 0, 0, 0x10).map(|x| x * 4));
    let ok = vcnvm::ring_writable(&mm, lut);
    let mut f = vcnvm::flush(&mm, 3, 0x80_0000_1001).unwrap();
    // The semaphore's wait, base low and high, the request, its ack, the semaphore's release, the base's wait:
    // 4 + 3 + 3 + 3 + 4 + 3 + 4 dwords (a register wait is 4, a write 3).
    assert!(ring_ok(&f, &ok) && f.len() == 24);
    let pm = vcnvm::pasid_map(lut, 3, 0x8001);
    assert_eq!(pm.len(), 3);
    f.extend(pm);
    assert!(ring_ok(&f, &ok), "the PASID entry");
    assert!(vcnvm::pasid_map(lut, 0, 1).is_empty() && vcnvm::pasid_map(None, 3, 1).is_empty());
    let with = |reg: u32, v: u32| {
        let mut b = f.clone();
        reg_write(&mut b, reg, v);
        ring_ok(&b, &ok)
    };
    assert!(!with(0x700 + 0x1A000, 1), "MMVM_L2_CNTL from the ring");
    let (req, sem) = (mm.byte(R::Req(0)).unwrap() / 4, mm.byte(R::Sem(0)).unwrap() / 4);
    assert!(with(req, gmc11::invalidate_req(7, 0)) && with(sem, 0));
    assert!(!with(req, gmc11::invalidate_req(0, 0)), "a flush of VMID 0 (the kernel's context)");
    assert!(!with(req, gmc11::invalidate_req(3, 0) | gmc11::invalidate_req(0, 0)), "VMID 0 beside 3");
    assert!(!with(req, gmc11::invalidate_req(3, 0) | gmc11::invalidate_req(4, 0)), "two VMIDs");
    assert!(!with(req, gmc11::invalidate_req(9, 0)) && !with(req, 0), "VMID 9, none");
    assert!(!with(sem, 1), "the semaphore taken by a write");
    // Another engine's request is not on the list at all.
    assert!(!with(mm.byte(R::Req(1)).unwrap() / 4, gmc11::invalidate_req(3, 0)));
    assert!(vcnvm::flush(&mm, 0, 0).is_none() && vcnvm::flush(&mm, 8, 0).is_none());
    // None of the CPU's context and range registers is a display field; engine 0's range is among them.
    let g = crate::gpucore::watch::DisplayGuard::from_discovery(&d);
    let w = vcnvm::writable(&mm);
    assert!(w.iter().all(|&b| g.class(b).is_none()) && w.len() == 7 * 5 + 2);
    assert!([R::RangeLo(0), R::RangeHi(0)].iter().all(|&r| w.contains(&mm.byte(r).unwrap())));
    // The bytes plan/guides/vcn-780m.md names for the desk's read (mmhub_3_0_1_offset.h 0x787/0x788, segment 1).
    assert_eq!((mm.byte(R::RangeLo(0)), mm.byte(R::RangeHi(0))), (Some(0x69e1c), Some(0x69e20)));
}

/// The stage's VCN register writes (start, then the stop) are the capture's (tests/data/linux-780m-vcn-writes.txt).
#[test]
fn cpu_writes_are_the_capture() {
    let d = discovery_780m();
    let mut kp = k::kept(&d, 512 << 20, &|_| {}).unwrap();
    let (r, seq0) = k::stage(&d, &mut kp, &|_| {}).unwrap();
    assert!(r.passed(), "{:?}", r.error);
    let a = VcnAddrs::from_discovery(&d).unwrap();
    let smu = crate::gpucore::pspbringup::GfxInputs::from_discovery(&d).unwrap().smu;
    let s = vcn::stop(&mut kp.owner, &a, r.layout.as_ref(), &smu, crate::gpucore::smu::SmuTimeouts::LINUX, vcn4::LINUX_TIMEOUT_US);
    assert!(s.ok());
    let base = [a.byte(R::RbBaseLo), a.byte(R::RbBaseHi)];
    let ours: std::vec::Vec<(u32, u32)> = kp.owner.log().since(seq0).filter(|e| a.name_at(e.byte).is_some()).map(|e| (e.byte, if base.contains(&Some(e.byte)) { 0 } else { e.value })).collect();
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-vcn-writes.txt");
    let linux: std::vec::Vec<(u32, u32)> = std::fs::read_to_string(&p).unwrap().lines().filter_map(crate::ktests_bringup::w_line).collect();
    assert_eq!(ours, linux);
}
