//! GP-GPU2 mutation proof (as GP-NET did): Linux's sequence with each mandatory step left out and each neighbouring
//! pair swapped, at three levels, is refused every time:
//! - the stages (`stage::run` refuses before writing anything);
//! - the commands on the ring (the model refuses, the driver's own order checks bypassed: `Ring14` driven by hand),
//!   all 31 of Linux's init, and each command with one of its arguments, its bytes or its buffer changed;
//! - the register writes of ring create and of an SMU question (the model refuses).
//! A sequence whose last step is left out is refused in the sense that the card never gets there (no ring, no
//! autoload, no message): that is asserted instead.

use super::rdna4fix::*;
use crate::gpucore::psp::{self, BoAlloc, Hw, Timeouts, cmd};
use crate::rdna4::fakecmd::Want;
use crate::rdna4::fakemodel::{MC_BASE, Model};
use crate::rdna4::guard::{HdpFlush, Mc};
use crate::rdna4::ring14::{Pri, Ring14};
use crate::rdna4::stage::Stage;
use crate::rdna4::vram::{self, Bump, Layout};
use std::vec::Vec;

/// The card's chain without the stages: MC addresses and Linux's HDP flush after each frame.
fn hdp<'a>(m: &'a mut Mc<Model>, flush: u32, memsize: u32) -> HdpFlush<'a> {
    HdpFlush { hw: m, flush, memsize }
}

fn variants<T: Clone>(seq: &[T]) -> Vec<(std::string::String, Vec<T>)> {
    let mut v = Vec::new();
    for i in 0..seq.len() {
        let mut s = seq.to_vec();
        s.remove(i);
        v.push((std::format!("without step {i}"), s));
    }
    for i in 0..seq.len() - 1 {
        let mut s = seq.to_vec();
        s.swap(i, i + 1);
        v.push((std::format!("steps {i} and {} swapped", i + 1), s));
    }
    v
}

#[test]
fn the_stages_out_of_order_are_refused_before_any_write() {
    let rig = Rig::new();
    for (what, seq) in variants(&ORDER) {
        let (mut s, mut m) = rig.up_to(&[]);
        let mut refused_clean = false;
        for st in &seq {
            let (d, back) = rig.run(&mut s, *st, m, &mut Stick::full());
            m = back;
            if !d.pass {
                assert_eq!(d.totals.writes, 0, "{what}: {st:?} wrote before refusing");
                refused_clean = true;
                break;
            }
        }
        assert!(m.refused.is_empty(), "{what}: the model saw {:?}", m.refused);
        assert!(refused_clean || m.next().is_some(), "{what}: passed through");
        if seq.len() == ORDER.len() || seq.last() != Some(&Stage::FwRest) {
            assert!(refused_clean, "{what}: no stage refused");
        }
    }
}

fn layout() -> Layout {
    vram::place(16304 << 20, MC_BASE, Rig::new().facts.bar0.unwrap(), Some(crate::attach::Gop { phys: rdna4_core::card::CARD_A.bar0_base, bytes: PICTURE })).unwrap()
}

/// A ring made by hand on a card whose secure OS runs: the model, the ring, the firmware buffer and the window.
fn by_hand() -> (Mc<Model>, Ring14, Pri, Bump, Layout) {
    let mut m = Mc { hw: warm(), base: MC_BASE, bytes: 16304 << 20 };
    let l = layout();
    let mut bump = Bump::new(&l);
    let regs = m.hw.regs.clone();
    let pri = Pri::new(&mut bump, &l).unwrap();
    let mut ring = Ring14::new(&mut bump, &l, regs.psp, Timeouts::LINUX).unwrap();
    ring.create(&mut hdp(&mut m, regs.hdp_flush, regs.memsize)).unwrap();
    (m, ring, pri, bump, l)
}

/// Linux's commands after ring create, then each variant, on a fresh model through `Ring14` without the stages.
#[test]
fn the_model_refuses_every_command_sequence_but_linuxs() {
    let e = expect();
    // Linux's 31, by hand (psp_load_fw, amdgpu_psp.c:3595-3660); the model's own script must say the same.
    let mut seq = std::vec![Want::Reserv(0x50), Want::Reserv(0x51), Want::Toc];
    seq.extend((0..20).map(Want::Image));
    seq.push(Want::Autoload);
    seq.extend([Want::Image(20), Want::Image(21), Want::Asd, Want::RegList, Want::Ta(0), Want::Ta(1), Want::Ta(2)]);
    assert_eq!((seq.len(), &seq), (31, &model().script()));
    let run = |steps: &[Want]| -> Model {
        let (mut m, mut ring, mut pri, mut bump, l) = by_hand();
        let regs = m.hw.regs.clone();
        let (_, fwr) = bump.alloc(1 << 20, 4096).unwrap();
        let (_, shr) = bump.alloc(0x4000, 4096).unwrap();
        let (at, shared, mc) = (l.mc(fwr.start), l.mc(shr.start), pri.buf.mc);
        let (lo, hi) = (mc as u32, (mc >> 32) as u32);
        for st in steps {
            let hw = &mut hdp(&mut m, regs.hdp_flush, regs.memsize);
            let mut from_pri = |hw: &mut HdpFlush, bytes: &[u8]| pri.load(hw, bytes).unwrap();
            let r = match *st {
                Want::Reserv(id) => ring.submit(hw, id, &[], &[psp::PSP_ERR_UNKNOWN_COMMAND]),
                Want::Toc => {
                    from_pri(hw, &e.toc);
                    ring.submit(hw, cmd::LOAD_TOC, &[lo, hi, e.toc.len() as u32], &[])
                }
                Want::Image(i) => {
                    let (ty, b) = &e.images[i];
                    hw.mem_write(at, b);
                    ring.submit(hw, cmd::LOAD_IP_FW, &[at as u32, (at >> 32) as u32, b.len() as u32, *ty], &[])
                }
                Want::Autoload => ring.submit(hw, cmd::AUTOLOAD_RLC, &[], &[]),
                Want::Asd => {
                    from_pri(hw, &e.asd);
                    ring.submit(hw, 0x04, &[lo, hi, e.asd.len() as u32, 0, 0, 0], &[])
                }
                Want::RegList => {
                    from_pri(hw, &e.reg_list);
                    ring.submit(hw, cmd::LOAD_IP_FW, &[lo, hi, e.reg_list.len() as u32, 67], &[])
                }
                Want::Ta(n) => {
                    hw.mem_write(shared, &[0; 0x4000]);
                    if n == 0 {
                        hw.mem_write(shared + 16, &e.ras_flags);
                    }
                    from_pri(hw, &e.tas[n]);
                    ring.submit(hw, cmd::LOAD_TA, &[lo, hi, e.tas[n].len() as u32, shared as u32, (shared >> 32) as u32, 0x4000], &[])
                }
            };
            if r.is_err() {
                break;
            }
        }
        m.hw
    };
    let ok = run(&seq);
    assert!(ok.refused.is_empty() && ok.next().is_none() && ok.commands.len() == 31, "{:?}", ok.refused);
    for (what, s) in variants(&seq) {
        let m = run(&s);
        // Two stacks of MEC and the two MES pipes share their bytes but not their type: a swap of them is refused
        // too.
        assert!(!m.refused.is_empty() || m.next().is_some(), "{what}: accepted");
        if s.last() == Some(&Want::Ta(2)) {
            assert!(!m.refused.is_empty(), "{what}: not refused");
        }
    }
}

/// What a command must carry besides its place in the order: the model refuses each of these single changes.
#[test]
fn the_model_refuses_a_command_with_a_wrong_type_bytes_or_buffer() {
    let owned = expect();
    let e = &owned;
    // The ring up to (not including) the command at `upto` of Linux's sequence, sent right; then `last`.
    let run = |upto: usize, last: &dyn Fn(&mut HdpFlush, &mut Ring14, &mut Pri, u64, u64)| -> Model {
        let (mut m, mut ring, mut pri, mut bump, l) = by_hand();
        let regs = m.hw.regs.clone();
        let (_, fwr) = bump.alloc(1 << 20, 4096).unwrap();
        let (_, shr) = bump.alloc(0x4000, 4096).unwrap();
        let (at, shared, mc) = (l.mc(fwr.start), l.mc(shr.start), pri.buf.mc);
        let hw = &mut hdp(&mut m, regs.hdp_flush, regs.memsize);
        for st in &model().script()[..upto] {
            match *st {
                Want::Reserv(id) => ring.submit(hw, id, &[], &[]),
                Want::Toc => {
                    pri.load(hw, &e.toc).unwrap();
                    ring.submit(hw, cmd::LOAD_TOC, &[mc as u32, (mc >> 32) as u32, e.toc.len() as u32], &[])
                }
                Want::Image(i) => {
                    hw.mem_write(at, &e.images[i].1);
                    ring.submit(hw, cmd::LOAD_IP_FW, &[at as u32, (at >> 32) as u32, e.images[i].1.len() as u32, e.images[i].0], &[])
                }
                Want::Autoload => ring.submit(hw, cmd::AUTOLOAD_RLC, &[], &[]),
                Want::Asd => {
                    pri.load(hw, &e.asd).unwrap();
                    ring.submit(hw, 0x04, &[mc as u32, (mc >> 32) as u32, e.asd.len() as u32, 0, 0, 0], &[])
                }
                Want::RegList => {
                    pri.load(hw, &e.reg_list).unwrap();
                    ring.submit(hw, cmd::LOAD_IP_FW, &[mc as u32, (mc >> 32) as u32, e.reg_list.len() as u32, 67], &[])
                }
                Want::Ta(_) => unreachable!("the cases end at the first application"),
            }
            .unwrap();
        }
        last(hw, &mut ring, &mut pri, at, shared);
        m.hw
    };
    let refused = |what: &str, m: Model, why: &str| {
        assert!(m.refused.len() == 1 && m.refused[0].contains(why), "{what}: {:?}", m.refused);
    };
    let words = |mc: u64, len: usize| [mc as u32, (mc >> 32) as u32, len as u32];
    // The TOC: other bytes, a byte left behind it, not from the megabyte's start, the wrong length.
    refused("toc bytes", run(2, &|hw, ring, pri, _, _| {
        pri.load(hw, &e.asd[..2304]).unwrap();
        let _ = ring.submit(hw, cmd::LOAD_TOC, &words(pri.buf.mc, 2304), &[]);
    }), "does not hold those bytes");
    refused("toc with a tail", run(2, &|hw, ring, pri, _, _| {
        pri.load(hw, &e.toc).unwrap();
        hw.mem_write(pri.buf.mc + 0x8_0000, &[7]);
        let _ = ring.submit(hw, cmd::LOAD_TOC, &words(pri.buf.mc, 2304), &[]);
    }), "does not hold those bytes");
    refused("toc off the megabyte", run(2, &|hw, ring, pri, _, _| {
        hw.mem_write(pri.buf.mc + 0x1000, &e.toc);
        let _ = ring.submit(hw, cmd::LOAD_TOC, &words(pri.buf.mc + 0x1000, 2304), &[]);
    }), "not the 1 MiB firmware buffer");
    refused("toc length", run(2, &|hw, ring, pri, _, _| {
        pri.load(hw, &e.toc).unwrap();
        let _ = ring.submit(hw, cmd::LOAD_TOC, &words(pri.buf.mc, 2300), &[]);
    }), "2300 bytes where Linux hands over 2304");
    // An image: the wrong type, the wrong bytes, the wrong size, outside VRAM.
    let image = |ty: u32, bytes: Vec<u8>, len: Option<u32>, addr: Option<u64>| {
        run(3, &move |hw, ring, _, at, _| {
            hw.mem_write(at, &bytes);
            let a = addr.unwrap_or(at);
            let _ = ring.submit(hw, cmd::LOAD_IP_FW, &[a as u32, (a >> 32) as u32, len.unwrap_or(bytes.len() as u32), ty], &[]);
        })
    };
    let smc = e.images[0].1.clone();
    refused("image type", image(71, smc.clone(), None, None), "type 71 where Linux loads type 18");
    refused("image bytes", image(18, e.images[1].1.clone(), None, None), "not that image's bytes");
    refused("image size", image(18, smc.clone(), Some(4092), None), "4092 bytes at");
    refused("image outside VRAM", image(18, smc.clone(), None, Some(0x30_0000)), "4096 bytes at 0x300000");
    // The ASD with a shared buffer; the register list as another type; an application without its shared buffer,
    // with one in the picture, with one of another size, and RAS without Linux's init flags.
    refused("asd shared", run(26, &|hw, ring, pri, _, shared| {
        pri.load(hw, &e.asd).unwrap();
        let w = words(pri.buf.mc, e.asd.len());
        let _ = ring.submit(hw, 0x04, &[w[0], w[1], w[2], shared as u32, (shared >> 32) as u32, 0x4000], &[]);
    }), "LOAD_ASD with a shared buffer");
    refused("reg list type", run(27, &|hw, ring, pri, _, _| {
        pri.load(hw, &e.reg_list).unwrap();
        let w = words(pri.buf.mc, e.reg_list.len());
        let _ = ring.submit(hw, cmd::LOAD_IP_FW, &[w[0], w[1], w[2], 8], &[]);
    }), "where Linux loads the register list (67)");
    let ras = |shared_of: &'static dyn Fn(u64) -> (u64, u32), flags: bool| {
        run(28, &move |hw, ring, pri, _, shared| {
            hw.mem_write(shared, &[0; 0x4000]);
            if flags {
                hw.mem_write(shared + 16, &e.ras_flags);
            }
            pri.load(hw, &e.tas[0]).unwrap();
            let (w, (sh, len)) = (words(pri.buf.mc, e.tas[0].len()), shared_of(shared));
            let _ = ring.submit_any(hw, cmd::LOAD_TA, &[w[0], w[1], w[2], sh as u32, (sh >> 32) as u32, len]);
        })
    };
    refused("ras without a shared buffer", ras(&|_| (0, 0), true), "not 16 KiB on a page of free VRAM");
    refused("ras shared in the picture", ras(&|_| (MC_BASE + 0x10_0000, 0x4000), true), "not 16 KiB on a page of free VRAM");
    refused("ras shared size", ras(&|s| (s, 0x1000), true), "not 16 KiB on a page of free VRAM");
    refused("ras shared unaligned", ras(&|s| (s + 0x10, 0x4000), true), "not 16 KiB on a page of free VRAM");
    refused("ras flags", ras(&|s| (s, 0x4000), false), "does not hold Linux's init flags");
    assert!(ras(&|s| (s, 0x4000), true).refused.is_empty());
}

#[test]
fn the_model_refuses_what_linux_never_sends() {
    let (mut m, mut ring, _, _, l) = by_hand();
    let regs = m.hw.regs.clone();
    // A second ring.
    assert!(!m.reg_write(regs.psp.c2pmsg_64, rdna4_core::refuse::GFX_CTRL_INIT_GPCOM_RING));
    // The driver's allow list keeps them off the ring before the command page is written; past it (`submit_raw`)
    // the model refuses them too.
    let before = (m.hw.writes.len(), m.hw.commands.len());
    for (id, words) in [(cmd::SETUP_TMR, [0u32; 4]), (cmd::DESTROY_TMR, [0; 4]), (0x02, [0; 4]), (0x03, [0; 4]), (0x22, [0; 4]), (cmd::LOAD_IP_FW, [0, 0, 4096, 11])] {
        let r = ring.submit(&mut hdp(&mut m, regs.hdp_flush, regs.memsize), id, &words, &[]);
        assert!(matches!(r, Err(psp::PspError::CmdRefused { .. })), "{id:#x}: {r:?}");
        let r = ring.submit_any(&mut hdp(&mut m, regs.hdp_flush, regs.memsize), id, &words);
        assert!(matches!(r, Err(psp::PspError::CmdRefused { .. })), "{id:#x}: {r:?}");
    }
    assert_eq!((m.hw.writes.len(), m.hw.commands.len()), before, "nothing reached the card");
    for (id, words) in [(cmd::SETUP_TMR, [0u32; 4]), (cmd::DESTROY_TMR, [0; 4]), (0x02, [0; 4]), (0x22, [0; 4])] {
        let a = ring.submit_raw(&mut hdp(&mut m, regs.hdp_flush, regs.memsize), id, &words).unwrap();
        assert_ne!(a.resp.status, 0, "{id:#x}");
    }
    assert!(m.hw.refused.iter().any(|r| r.contains("boot-time TMR")), "{:?}", m.hw.refused);
    // Mode1 reset, the bootloader, a never-message, the SMU's debug mailbox, an index outside the flags.
    let n = m.hw.refused.len();
    let find = |name: &str| regs.named.iter().find(|(_, r)| r.name == name).unwrap().0;
    assert!(!m.reg_write(regs.psp.c2pmsg_64, rdna4_core::refuse::GFX_CTRL_MODE1_RESET));
    assert!(!m.reg_write(regs.psp.c2pmsg_35, 0x2_0000));
    assert!(!m.reg_write(find("MP1_SMN_C2PMSG_75"), 0x1));
    assert!(!m.reg_write(regs.rsmu_index, 0x0301_0028));
    m.reg_write(regs.smu_answer, 0);
    m.reg_write(regs.smu_argument, 0);
    for never in rdna4_core::refuse::SMU_NEVER {
        assert!(!m.reg_write(regs.smu_message, u32::from(never.0)), "{}", never.1);
    }
    assert_eq!(m.hw.refused.len(), n + 4 + rdna4_core::refuse::SMU_NEVER.len());
    assert!(m.hw.messages.is_empty());
    // A frame before any ring exists, on a card with a secure OS.
    let mut bare = warm();
    assert!(!bare.reg_write(regs.psp.c2pmsg_67, 16) && bare.refused[0].contains("a frame without a ring"));
    // A frame whose memory was not flushed through HDP (the chain without `HdpFlush`).
    let mut m2 = Mc { hw: warm(), base: MC_BASE, bytes: 16304 << 20 };
    let mut bump = Bump::new(&l);
    let mut r2 = Ring14::new(&mut bump, &l, regs.psp, Timeouts::LINUX).unwrap();
    r2.create(&mut hdp(&mut m2, regs.hdp_flush, regs.memsize)).unwrap();
    assert!(r2.submit(&mut m2, 0x50, &[], &[]).is_err());
    assert!(m2.hw.refused.iter().any(|r| r.contains("not flushed through HDP")), "{:?}", m2.hw.refused);
    // The first frame of a ring whose write pointer does not read 0: nothing is written.
    let mut m3 = Mc { hw: warm(), base: MC_BASE, bytes: 16304 << 20 };
    let mut bump = Bump::new(&l);
    let mut r3 = Ring14::new(&mut bump, &l, regs.psp, Timeouts::LINUX).unwrap();
    r3.create(&mut hdp(&mut m3, regs.hdp_flush, regs.memsize)).unwrap();
    m3.hw.c67 = 0x40;
    let writes = m3.hw.writes.len();
    assert_eq!(r3.submit(&mut hdp(&mut m3, regs.hdp_flush, regs.memsize), 0x50, &[], &[]), Err(psp::PspError::RingNotReset { wptr: 0x40 }));
    assert_eq!(m3.hw.writes.len(), writes);
    // A destroy, then a destroy without a ring.
    r2.destroy(&mut hdp(&mut m2, regs.hdp_flush, regs.memsize)).unwrap();
    assert!(!m2.reg_write(regs.psp.c2pmsg_64, rdna4_core::refuse::GFX_CTRL_DESTROY_RINGS));
}

/// Ring create's register sequence (Linux: read C2PMSG_64 until ready, write 69, 70, 71, then 64).
#[test]
fn ring_create_out_of_order_is_refused() {
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum R {
        Ready,
        W69,
        W70,
        W71,
        Init,
    }
    let seq = [R::Ready, R::W69, R::W70, R::W71, R::Init];
    let mc = MC_BASE + (32 << 20);
    let run = |s: &[R]| {
        let mut m = warm();
        let p = m.regs.psp;
        for r in s {
            match r {
                R::Ready => {
                    m.reg_read(p.c2pmsg_64);
                }
                R::W69 => {
                    m.reg_write(p.c2pmsg_69, mc as u32);
                }
                R::W70 => {
                    m.reg_write(p.c2pmsg_70, (mc >> 32) as u32);
                }
                R::W71 => {
                    m.reg_write(p.c2pmsg_71, 0x1000);
                }
                R::Init => {
                    m.reg_write(p.c2pmsg_64, rdna4_core::refuse::GFX_CTRL_INIT_GPCOM_RING);
                }
            }
        }
        m
    };
    let ok = run(&seq);
    assert!(ok.refused.is_empty() && ok.ring == Some((mc, 0x1000)), "{:?}", ok.refused);
    for (what, s) in variants(&seq) {
        let m = run(&s);
        assert!(m.ring.is_none(), "{what}: a ring was made");
        if s.last() == Some(&R::Init) {
            assert!(!m.refused.is_empty(), "{what}: not refused");
        }
    }
}

/// An SMU question's register sequence (Linux: answer 0, argument, message).
#[test]
fn an_smu_question_out_of_order_is_refused() {
    let seq = [0usize, 1, 2];
    let run = |s: &[usize]| {
        let mut m = model();
        let r = m.regs.clone();
        for i in s {
            let (b, v) = [(r.smu_answer, 0), (r.smu_argument, 0), (r.smu_message, 0x02)][*i];
            m.reg_write(b, v);
        }
        m
    };
    let ok = run(&seq);
    assert!(ok.refused.is_empty() && ok.messages == [(0x02, 0)], "{:?}", ok.refused);
    for (what, s) in variants(&seq) {
        let m = run(&s);
        assert!(m.messages.is_empty(), "{what}: the message went out");
        if s.last() == Some(&2) {
            assert!(!m.refused.is_empty(), "{what}: not refused");
        }
    }
}
