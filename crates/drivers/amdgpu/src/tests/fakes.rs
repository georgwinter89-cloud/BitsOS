//! GX-7c: the grown fakes as host tests (the ktests' checks, plus what needs the real discovery blob): the fake's
//! 780M table equals the real one, the classifier reads the fake at the real addresses, the scanout watcher's
//! ranges cover every DCN register the display side knows, and the watcher's name list is the diff tool's.

use crate::gpucore::classify;
use crate::gpucore::disc::{self, Discovery};
use crate::gpucore::fakechip;
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource};
use crate::gpucore::regdiff;
use crate::gpucore::watch::{self, Class, DisplayGuard};
use crate::ktests_fakes as k;
use std::vec::Vec;

fn blob() -> Option<Discovery> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/gx/fixtures/minipc-780m-discovery.bin");
    let Ok(buf) = std::fs::read(&p) else {
        std::eprintln!("skip: {} missing", p.display());
        return None;
    };
    Some(disc::parse(&buf).unwrap())
}

#[test]
fn smu_fake_answers_linux_like_the_780m() {
    k::check_smu().unwrap();
}

#[test]
fn gc_fake_gfxoff_imu_and_autoload() {
    k::check_gc().unwrap();
}

#[test]
fn every_start_state_classified_and_met() {
    k::check_stale(&fakechip::discovery_780m()).unwrap();
    if let Some(d) = blob() {
        k::check_stale(&d).unwrap();
    }
}

#[test]
fn scanout_watcher() {
    let hubp = dcn314_regs::lookup("HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS").unwrap().byte;
    k::check_watch(&fakechip::discovery_780m(), hubp).unwrap();
    if let Some(d) = blob() {
        k::check_watch(&d, hubp).unwrap();
    }
}

#[test]
fn the_fakes_table_is_the_real_one() {
    let Some(real) = blob() else { return };
    let fake = fakechip::discovery_780m();
    for ip in &fake.ips {
        let r = real.find(ip.hw_id, 0).unwrap_or_else(|| panic!("hw {} not in the real table", ip.hw_id));
        assert_eq!((r.version, &r.bases), (ip.version, &ip.bases), "hw {}", ip.hw_id);
    }
    assert_eq!(classify::addresses(&fake), classify::addresses(&real));
}

#[test]
fn watcher_covers_every_dcn_register_and_matches_the_tool() {
    let g = DisplayGuard::from_discovery(&fakechip::discovery_780m());
    // Segment 0 aliases NBIO (dcn314-regs leaves it out too); every other DCN register is watched.
    let mut n = 0;
    for r in dcn314_regs::REGISTERS.iter().filter(|r| r.seg != 0) {
        assert_eq!(g.class(r.byte), Some(Class::Dcn), "{} @{:#x}", r.name, r.byte);
        n += 1;
    }
    assert!(n > 5000, "{n}");
    let ours: Vec<&str> = watch::MMHUB_DISPLAY.iter().chain(watch::DCN_MAILBOX).map(|(n, _)| *n).collect();
    assert_eq!(ours, regdiff::DISPLAY_NAMES);
    // Our own mailboxes and the PSP's are not display registers.
    let f = FakeGpu::new(Hostile::None);
    for b in [f.a.c2pmsg_64, f.a.c2pmsg_67, f.smu.drv.msg_at, f.smu.drv.arg_at, f.smu.drv.resp_at] {
        assert_eq!(g.class(b), None, "{b:#x}");
    }
}

/// The PSP fake's memory rules: the TMR is the PSP's, a reserved range refuses the TMR, and Linux's order.
#[test]
fn psp_fake_memory_rules() {
    let src = || MapSource(pspfake::synthetic_780m());
    let mut f = FakeGpu::new(Hostile::None);
    let run = pspfake::run(&mut f, &mut src()).unwrap();
    let (tmc, tsize) = f.tmr.unwrap();
    use crate::gpucore::psp::Hw;
    assert!(!f.mem_write(tmc + 0x1000, &[1, 2, 3, 4]), "the CPU may not write the PSP's TMR");
    assert_eq!(f.tmr_writes, [(tmc + 0x1000, 4)]);
    assert!(f.mem_write(tmc + u64::from(tsize), &[1]), "just past it is fine");
    drop(run);

    // A reserved range where the BO manager put the TMR: SETUP_TMR answers non-zero (GX-7d: fatal, undone).
    let mut r = FakeGpu::new(Hostile::None);
    r.reserved.push((tmc, tmc + 1));
    let e = pspfake::run(&mut r, &mut src()).err().unwrap();
    let want = crate::gpucore::psp::PspError::CmdStatus { cmd: crate::gpucore::psp::cmd::SETUP_TMR, status: pspfake::ST_BAD_PARAM, ucode: None };
    assert!(matches!(&e, pspfake::RunError::Psp(x) if *x == want), "{e:?}");
    assert!(r.ring.is_none() && r.tmr.is_none());

    // Out of Linux's order: two images swapped, AUTOLOAD_RLC not right after RLC_G.
    let mut o = FakeGpu::new(Hostile::None);
    let opts = crate::gpucore::ucode::ListOpts::PSP13_0_4;
    let (fw, _) = crate::gpucore::fwload::load(&pspfake::plan_780m().files(), &mut src(), &opts).unwrap();
    let mut list = fw.load_list(&opts).unwrap();
    list.swap(2, 3);
    let n = list.len();
    list.swap(n - 1, n - 2); // RLC_G before RLC_P (GX-7d: no VCN, RLC_G is last)
    let mut bo = crate::bo::BoManager::new(o.fb_size, Vec::new());
    let carve = crate::gpucore::psp::Carve { mc_base: o.fb_mc, phys_base: o.fb_phys };
    let mut p = crate::gpucore::psp::Psp::new(&mut bo, crate::ipmod::psp13::Mailbox13 { addrs: o.a }, carve, crate::gpucore::psp::Timeouts::LINUX).unwrap();
    p.start(&mut o, &mut bo, &fw, &list, opts.autoload).unwrap();
    assert!(o.order_errors.len() >= 2, "{:?}", o.order_errors);
    // The C2PMSG_64 the real chip shows: extra bits beside the response flag, status 0 (the driver masks).
    assert_eq!(o.reg(o.a.c2pmsg_64) & 0xFFFF, 0);
    assert_eq!(o.reg(o.a.c2pmsg_64) & 0xFFFF_0000, pspfake::CTRL_COLD | crate::gpucore::psp::CTRL_INIT_GPCOM_RING);
}
