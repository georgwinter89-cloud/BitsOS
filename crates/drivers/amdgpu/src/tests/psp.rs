//! GX-7a PSP bring-up against the fake: the good run in Linux's order (the ktest's check), the mailbox sequence,
//! the frame and command formats, and every hostile answer ending in its own error within bounded waits.

use super::blob::{Blob, build, minipc_ips};
use crate::bo::{BoError, BoManager};
use crate::gpucore::psp::{self, Carve, Hw, Mailbox, Psp, PspError, Timeouts, cmd};
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource, RunError};
use crate::ipmod::psp13::{Addrs, Mailbox13};
use std::vec::Vec;

fn run(h: Hostile) -> (FakeGpu, Result<pspfake::Run, RunError>) {
    let mut fake = FakeGpu::new(h);
    let r = pspfake::run(&mut fake, &mut MapSource(pspfake::synthetic_780m()));
    (fake, r)
}

fn psp_err(h: Hostile) -> (FakeGpu, PspError) {
    match run(h) {
        (f, Err(RunError::Psp(e))) => (f, e),
        (_, Err(e)) => panic!("{h:?}: {e:?}"),
        (_, Ok(_)) => panic!("{h:?}: passed"),
    }
}

/// The fakes prove `Until::Backend` as one run, the live tool runs `BACKEND_STAGES` one by one: the two must stand for
/// the same work, or `gx.hw.*` proves another chain than the chip gets.
#[test]
fn fake_backend_chain_is_the_live_tools_stages() {
    use psp::Until;
    assert_eq!(Until::BACKEND_STAGES, [Until::Sdma, Until::GfxMes]);
    let what: [(&str, fn(Until) -> bool); 6] = [
        ("loads_fw", Until::loads_fw),
        ("runs_gc", Until::runs_gc),
        ("runs_gmc", Until::runs_gmc),
        ("after_gart", Until::after_gart),
        ("sdma", Until::fake_chain_sdma),
        ("mes", Until::fake_chain_mes),
    ];
    for (name, p) in what {
        assert_eq!(p(Until::Backend), Until::BACKEND_STAGES.iter().any(|&s| p(s)), "{name}");
    }
    assert!(Until::Backend.fake_chain_sdma() && Until::Sdma.fake_chain_sdma() && !Until::GfxMes.fake_chain_sdma());
    assert!(Until::Backend.fake_chain_mes() && Until::GfxMes.fake_chain_mes() && !Until::Sdma.fake_chain_mes());
    assert!(Until::Backend.maps_queues() && Until::GfxMes.maps_queues(), "the live stage and the fake chain both map the queues");
}

#[test]
fn good_run_in_linux_order() {
    crate::ktests_psp::check_run(&mut FakeGpu::new(Hostile::None), &mut MapSource(pspfake::synthetic_780m())).unwrap();
}

#[test]
fn mailbox_sequence_and_formats() {
    let (fake, r) = run(Hostile::None);
    let r = r.unwrap();
    let a = fake.a;
    let ring = r.psp.ring.buf.mc;
    // psp_v13_0_4_ring_create: address low, high, size, then KM << 16 into C2PMSG_64.
    assert_eq!(fake.writes[..4], [(a.c2pmsg_69, ring as u32), (a.c2pmsg_70, (ring >> 32) as u32), (a.c2pmsg_71, 0x1000), (a.c2pmsg_64, 0x2_0000)]);
    // Then only write pointers, one frame (16 dwords) further each time, wrapping at the ring's 1024 dwords.
    let wptrs: Vec<u32> = fake.writes[4..].iter().map(|&(b, v)| { assert_eq!(b, a.c2pmsg_67); v }).collect();
    // LOAD_TOC, SETUP_TMR, 17 images (GX-7d: no VCN; GX-7k: MES's KIQ pipe too), AUTOLOAD_RLC.
    assert_eq!(wptrs.len(), 20);
    assert!(wptrs.iter().enumerate().all(|(i, &w)| w == (i as u32 + 1) * 16 % 1024));
    assert_eq!(r.report.steps[0], psp::Step::SosAlive(pspfake::SOS_SIGN));
    assert_eq!(r.report.stale_ring_size, 0);
    // Buffers in the carve-out at MC addresses, 4 KiB ring/fence/command, 1 MiB private, TMR 1 MiB aligned.
    for b in [r.psp.ring.buf, r.psp.fence, r.psp.cmd] {
        assert!(b.mc >= pspfake::FB_MC && b.mc % 4096 == 0 && b.size == 4096);
    }
    assert_eq!(r.psp.pri.mc % 0x10_0000, 0);
    let tmr = r.psp.tmr.unwrap();
    assert_eq!((tmr.mc % psp::TMR_ALIGN, tmr.size), (0, u64::from(pspfake::TOC_TMR)));
    // The PSP's answers: each microcode's place in the TMR, in order.
    let addrs: Vec<u64> = r.report.loaded.iter().map(|l| l.tmr_addr).collect();
    assert!(addrs[0] == tmr.mc && addrs.windows(2).all(|w| w[0] < w[1]));
    // Formats: a frame, a command.
    let f = psp::frame(0x80_1234_5000, 0x80_0000_1000, 7);
    assert_eq!(&f[0..8], &[0x00, 0x50, 0x34, 0x12, 0x80, 0, 0, 0]);
    assert_eq!(&f[8..12], &[0; 4]);
    assert_eq!(&f[12..24], &[0x00, 0x10, 0, 0, 0x80, 0, 0, 0, 7, 0, 0, 0]);
    assert!(f[24..].iter().all(|&x| x == 0));
    let c = psp::encode(cmd::LOAD_IP_FW, &[1, 2, 3, 4]);
    assert_eq!(c.len(), 1024);
    assert_eq!(&c[8..12], &6u32.to_le_bytes());
    assert_eq!(&c[28..44], &[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0]);
}

#[test]
fn mp0_addresses_from_discovery() {
    let d = crate::gpucore::disc::parse(&build(&Blob::new(minipc_ips())).0).unwrap();
    let a = Addrs::from_discovery(&d).unwrap();
    assert_eq!(Some(a), Addrs::from_segment1(pspfake::MP0_SEG1));
    assert_eq!((a.c2pmsg_64, a.c2pmsg_81, a.c2pmsg_67, a.c2pmsg_35), (0x58200, 0x58244, 0x5820C, 0x5818C));
    assert_eq!(a.c2pmsg_81, dcn314_regs::bar5::MP0_C2PMSG_81);
    assert_eq!(Addrs::from_segment1(u32::MAX), None);
}

#[test]
fn every_hostile_mode_in_the_table() {
    crate::ktests_psp::check_hostile().unwrap();
}

#[test]
fn hostile_answers() {
    let t = Timeouts::LINUX;
    let (_, e) = psp_err(Hostile::SosDead);
    assert_eq!(e, PspError::SosNotRunning);
    // Not ready: a 100 ms wait of fake time, then -ETIME as Linux returns it.
    let (f, e) = psp_err(Hostile::NotReady);
    assert!(matches!(e, PspError::Timeout { what: "ring.ready", last: 0 }));
    assert_eq!(e.errno(), psp::ETIME);
    assert!(f.now >= t.reg_us && f.now <= t.reg_us + 2 * t.poll_us, "waited {}", f.now);
    assert!(f.writes.is_empty(), "wrote before the PSP was ready: {:?}", f.writes);
    let (_, e) = psp_err(Hostile::Silent);
    assert!(matches!(e, PspError::Timeout { what: "ring.create", .. }));
    let (_, e) = psp_err(Hostile::RingStatus(0x22));
    assert_eq!(e, PspError::RingStatus { status: 0x22 });
    let (_, e) = psp_err(Hostile::AllOnes);
    assert_eq!(e.errno(), psp::ENODEV);
    let (f, e) = psp_err(Hostile::RefuseWrite(Addrs::from_segment1(pspfake::MP0_SEG1).unwrap().c2pmsg_69));
    assert_eq!(e, PspError::WriteRefused { byte: f.a.c2pmsg_69 });
    let (_, e) = psp_err(Hostile::Wptr(5));
    assert_eq!(e, PspError::RingBounds { wptr: 5 });
    let (_, e) = psp_err(Hostile::Wptr(1024 + 16));
    assert_eq!(e, PspError::RingBounds { wptr: 1040 });
    // GX-7d: a write pointer that survives create: nothing is sent, the ring is stopped again.
    let (f, e) = psp_err(Hostile::WptrKept(0x40));
    assert_eq!(e, PspError::RingNotReset { wptr: 0x40 });
    assert!(f.cmds.is_empty() && f.ring.is_none(), "{:?}", f.cmds);
    assert_eq!(e.errno(), psp::EINVAL);
    let (_, e) = psp_err(Hostile::TocTmr(0));
    assert_eq!(e, PspError::TmrSize(0));
    let (_, e) = psp_err(Hostile::TocTmr(0x4000_0000));
    assert_eq!(e, PspError::Bo(BoError::NoSpace));
    assert_eq!(e.errno(), psp::ENOMEM);
    let (_, e) = psp_err(Hostile::MemFail);
    assert!(matches!(e, PspError::Mem { .. }));
    // A fence that never comes, or comes wrong: -ETIME after the fence wait, ring wedged.
    for h in [Hostile::NoFence, Hostile::WrongFence] {
        let (f, e) = psp_err(h);
        assert!(matches!(e, PspError::Timeout { what: "fence", .. }), "{h:?}: {e:?}");
        assert!(f.now <= t.reg_us + t.settle_us + t.fence_us + 1000, "{h:?}: waited {}", f.now);
    }
}

/// GX-7d inverts GX-7a (Linux only warns, amdgpu_psp.c:681-699): a non-zero status stops the run, the undo
/// destroys what the PSP accepted (DESTROY_TMR only after an accepted SETUP_TMR), and the report keeps the steps.
#[test]
fn status_answers_are_fatal() {
    for (id, tmr_destroyed) in [(cmd::LOAD_TOC, false), (cmd::SETUP_TMR, false), (cmd::LOAD_IP_FW, true), (cmd::AUTOLOAD_RLC, true)] {
        let mut fake = FakeGpu::new(Hostile::CmdStatus { id, status: 9 });
        let opts = crate::gpucore::ucode::ListOpts::PSP13_0_4;
        let (fw, _) = crate::gpucore::fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).unwrap();
        let list = fw.load_list(&opts).unwrap();
        let mut bo = BoManager::new(fake.fb_size, Vec::new());
        let carve = Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys };
        let mut p = Psp::new(&mut bo, Mailbox13 { addrs: fake.a }, carve, Timeouts::LINUX).unwrap();
        let e = p.start(&mut fake, &mut bo, &fw, &list, true).unwrap_err();
        assert!(matches!(e, PspError::CmdStatus { cmd: c, status: 9, .. } if c == id), "{e:?}");
        assert_eq!(e.errno(), psp::EINVAL);
        assert_eq!(p.last.error.as_ref(), Some(&e));
        assert!(p.last.steps.contains(&psp::Step::RingCreated { mc: p.ring.buf.mc }));
        assert_eq!(fake.cmds.contains(&cmd::DESTROY_TMR), tmr_destroyed, "{id:#x}: {:?}", fake.cmds);
        assert!(fake.ring.is_none() && fake.tmr.is_none() && !p.running(), "{id:#x}: not undone");
        assert!(fake.loaded.is_empty() || id == cmd::AUTOLOAD_RLC);
        p.free(&mut bo);
        assert_eq!(bo.count(), 0);
    }
}

#[test]
fn wedged_after_a_fence_timeout() {
    let mut fake = FakeGpu::new(Hostile::None);
    let mut bo = BoManager::new(fake.fb_size, Vec::new());
    let carve = Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys };
    let mut p = Psp::new(&mut bo, Mailbox13 { addrs: fake.a }, carve, Timeouts::LINUX).unwrap();
    assert_eq!(p.submit(&mut fake, &psp::encode(cmd::LOAD_TOC, &[])), Err(PspError::NoRing));
    // No files at all: sOS, ring, and a TMR of Linux's default 4 MiB (no TOC to ask).
    let fw = crate::gpucore::ucode::FwSet::default();
    // Autoload without RLC_G in the list is refused before the device is touched.
    assert_eq!(p.start(&mut fake, &mut bo, &fw, &[], true), Err(PspError::NoRlcG));
    assert!(fake.writes.is_empty());
    let rep = p.start(&mut fake, &mut bo, &fw, &[], false).unwrap();
    assert_eq!(rep.steps.len(), 3, "{rep:?}");
    assert_eq!(p.start(&mut fake, &mut bo, &fw, &[], false), Err(PspError::Started));
    assert_eq!(p.tmr.unwrap().size, psp::TMR_DEFAULT);
    fake.hostile = Hostile::NoFence;
    assert!(matches!(p.submit(&mut fake, &psp::encode(cmd::AUTOLOAD_RLC, &[])), Err(PspError::Timeout { .. })));
    assert!(p.wedged());
    let n = fake.frames;
    assert_eq!(p.submit(&mut fake, &psp::encode(cmd::AUTOLOAD_RLC, &[])), Err(PspError::Wedged));
    assert_eq!(fake.frames, n, "a wedged ring sent a frame");
    // The undo after a timeout sends no command, only stops the ring.
    fake.hostile = Hostile::None;
    let cmds = fake.cmds.len();
    p.stop(&mut fake).unwrap();
    assert_eq!(fake.cmds.len(), cmds);
    assert!(fake.ring.is_none());
    // GX-7d fixer: the lost frame and the TMR (DESTROY_TMR was not sent) may still be the PSP's: the `Psp` is not
    // quiet, a new start is refused before any write, and its buffers must stay allocated (the caller parks it).
    assert!(!p.quiet());
    let held = bo.count();
    let writes = fake.writes.len();
    assert_eq!(p.start(&mut fake, &mut bo, &fw, &[], false), Err(PspError::Wedged));
    assert_eq!((fake.writes.len(), bo.count()), (writes, held));
    assert!(p.buffers().all(|b| bo.get(b.handle).is_some()));
}

/// A good run, stopped, started again on the same `Psp`: the earlier TMR and firmware buffers go back, the
/// second run loads everything again, and the end leaves no buffer.
#[test]
fn restart_after_stop() {
    let mut fake = FakeGpu::new(Hostile::None);
    let pspfake::Run { mut psp, mut bo, fw, list, .. } = pspfake::run(&mut fake, &mut MapSource(pspfake::synthetic_780m())).unwrap();
    let held = bo.count();
    psp.stop(&mut fake).unwrap();
    let n = fake.loaded.len();
    let rep = psp.start(&mut fake, &mut bo, &fw, &list, true).unwrap();
    assert_eq!(rep.loaded.len(), list.len());
    assert_eq!(fake.loaded.len(), 2 * n);
    assert_eq!(bo.count(), held, "the first run's TMR/firmware buffers were not given back");
    psp.stop(&mut fake).unwrap();
    psp.free(&mut bo);
    assert_eq!((bo.count(), bo.free_bytes()), (0, fake.fb_size));
}

/// Refuses the n-th write to one register, everything else goes to the fake.
struct RefuseNth<'a> {
    fake: &'a mut FakeGpu,
    byte: u32,
    n: u32,
}

impl Hw for RefuseNth<'_> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.fake.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        if byte == self.byte {
            self.n = self.n.wrapping_sub(1);
            if self.n == 0 {
                return false;
            }
        }
        self.fake.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.fake.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.fake.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.fake.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.fake.delay_us(us)
    }
}

/// SETUP_TMR fails without a fence timeout (its write pointer is refused): start undoes itself with the ring
/// stop only, no DESTROY_TMR for a TMR the PSP never accepted.
#[test]
fn failed_setup_tmr_sends_no_destroy_tmr() {
    let mut fake = FakeGpu::new(Hostile::None);
    let opts = crate::gpucore::ucode::ListOpts::PSP13_0_4;
    let (fw, _) = crate::gpucore::fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).unwrap();
    let list = fw.load_list(&opts).unwrap();
    let mut bo = BoManager::new(fake.fb_size, Vec::new());
    let carve = Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys };
    let mut p = Psp::new(&mut bo, Mailbox13 { addrs: fake.a }, carve, Timeouts::LINUX).unwrap();
    let wp = fake.a.c2pmsg_67;
    // Write pointer 1 is LOAD_TOC's, 2 is SETUP_TMR's.
    let e = p.start(&mut RefuseNth { fake: &mut fake, byte: wp, n: 2 }, &mut bo, &fw, &list, true).unwrap_err();
    assert_eq!(e, PspError::WriteRefused { byte: wp });
    assert!(!p.wedged());
    assert_eq!(fake.cmds, [cmd::LOAD_TOC], "{:?}", fake.cmds);
    assert!(fake.ring.is_none(), "start's undo did not stop the ring");
    assert_eq!(p.stop(&mut fake), Ok(()));
    p.free(&mut bo);
    assert_eq!(bo.count(), 0);
}

#[test]
fn km_ring_size_refusals() {
    let buf = psp::Buf { handle: 1, offset: 0, size: 0x1000, mc: 0x1000 };
    for bytes in [0, 4, psp::FRAME_BYTES - 1, psp::FRAME_BYTES + 4, 0x2000] {
        assert!(psp::KmRing::new(buf, bytes).is_none(), "{bytes}");
    }
    assert_eq!(psp::KmRing::new(buf, psp::FRAME_BYTES).map(|r| r.frames()), Some(1));
    assert_eq!(psp::KmRing::new(buf, 0x1000).map(|r| r.frames()), Some(64));
}

#[test]
fn ring_full_with_a_holding_psp() {
    let mut fake = FakeGpu::new(Hostile::Hold);
    let mut bo = BoManager::new(fake.fb_size, Vec::new());
    let carve = Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys };
    let mut p = Psp::new(&mut bo, Mailbox13 { addrs: fake.a }, carve, Timeouts::LINUX).unwrap();
    let mb = Mailbox13 { addrs: fake.a };
    mb.ring_create(&mut fake, &p.t, p.ring.buf.mc, psp::RING_BYTES).unwrap();
    let cmd_page = |i: u64| p.cmd.mc + i * 0x1000;
    let frames = p.ring.frames();
    assert_eq!(frames, 64);
    for i in 1..frames {
        p.ring.push(&mut fake, &mb, cmd_page(0), p.fence.mc, i).unwrap();
    }
    assert_eq!(p.ring.in_flight(), 63);
    assert_eq!(p.ring.push(&mut fake, &mb, cmd_page(0), p.fence.mc, 64), Err(PspError::RingFull));
    fake.release();
    assert_eq!(fake.frames, 63);
    let mut b = [0u8; 4];
    assert!(fake.mem_read(p.fence.mc, &mut b));
    let seen = u32::from_le_bytes(b);
    assert_eq!(seen, 63);
    p.ring.complete(seen);
    assert_eq!(p.ring.push(&mut fake, &mb, cmd_page(0), p.fence.mc, 64), Ok(63));
    p.free(&mut bo);
}

#[test]
fn buffers_returned_when_the_carve_out_is_too_small() {
    let mut bo = BoManager::new(0x8_0000, Vec::new());
    let fake = FakeGpu::new(Hostile::None);
    let carve = Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys };
    let r = Psp::new(&mut bo, Mailbox13 { addrs: fake.a }, carve, Timeouts::LINUX);
    assert!(matches!(r, Err(PspError::Bo(BoError::NoSpace))));
    assert_eq!((bo.count(), bo.free_bytes()), (0, 0x8_0000));
}

#[test]
fn probe_reads_the_psp_state_registers() {
    let d = crate::gpucore::disc::parse(&build(&Blob::new(minipc_ips())).0).unwrap();
    let regs = crate::probe::reg_reads(&d, |b| Some(b));
    let names: Vec<_> = regs.iter().map(|r| (r.name, r.byte)).collect();
    assert_eq!(names[3], ("MP0_C2PMSG_35 (PSP bootloader ready: bit 31)", Some(0x5818C)));
    assert_eq!(names[4], ("MP0_C2PMSG_67 (PSP KM ring write pointer)", Some(0x5820C)));
    // Every one of them is a register the snapshot may read (no index/data window, inside BAR5's display range).
    assert!(regs.iter().all(|r| r.byte.is_some_and(crate::snapshot::readable)));
}

#[test]
fn firmware_list_matches_the_modules() {
    let list: Vec<&str> = include_str!("../../firmware.list").lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let mut want: Vec<std::string::String> = Vec::new();
    for f in pspfake::plan_780m().files() {
        want.push(f.name);
        want.extend(f.fallback);
    }
    let mut a: Vec<_> = list.iter().map(|s| std::string::String::from(*s)).collect();
    a.sort();
    want.sort();
    assert_eq!(a, want);
}
