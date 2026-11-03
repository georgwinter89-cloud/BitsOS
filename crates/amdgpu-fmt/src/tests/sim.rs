//! Interpreter behaviour: the SDMA rungs, ring order, IBs, polls, limits, faults, halts.

use std::vec;
use std::vec::Vec;

use super::limits;
use crate::bus::{MemFault, SimMem};
use crate::ring::{Kind, Ring};
use crate::sim::{Engine, Event, Hang, IntSrc, Isa, Limits, Stop, WaitOn};
use crate::{pm4, sdma};

const RING: u64 = 0x10_0000;
const BUF: u64 = 0x80_0000;
const SDMA: Kind = Kind::Sdma { burst_nop: true };

struct Rig {
    mem: SimMem,
    ring: Ring,
    eng: Engine,
    shadow: Vec<u32>,
}

impl Rig {
    fn new(kind: Kind, size: u64, lim: Limits) -> Rig {
        let mut mem = SimMem::new();
        mem.map(0, RING, size as usize * 4).unwrap();
        mem.map(0, BUF, 0x10_0000).unwrap();
        let isa = if kind == Kind::Gfx { Isa::Pm4 } else { Isa::Sdma };
        Rig {
            mem,
            ring: Ring::new(kind, size, 0).unwrap(),
            eng: Engine::new(isa, RING, size, 0, lim).unwrap(),
            shadow: vec![0; size as usize],
        }
    }
    /// One submission of the dwords `f` emits.
    fn submit(&mut self, f: impl FnOnce(&mut Vec<u32>)) -> u64 {
        let mut v = Vec::new();
        f(&mut v);
        self.ring.begin(v.len() as u64, self.eng.rptr()).unwrap();
        self.ring.write(&v).unwrap();
        let w = self.ring.commit(&mut self.shadow).unwrap();
        self.mem.poke_dwords(0, RING, &self.shadow).unwrap();
        w
    }
    fn run(&mut self, wptr: u64) -> crate::sim::Report {
        self.eng.run(&mut self.mem, wptr)
    }
}

#[test]
fn sdma_rungs_1_to_3() {
    let mut r = Rig::new(SDMA, 256, limits());
    let w = r.submit(|v| {
        sdma::write_linear(v, BUF, &[0xDEAD_BEEF]).unwrap();
        sdma::const_fill(v, BUF + 0x1000, 0xA5A5_5A5A, 4096, sdma::FILL_DWORD).unwrap();
        sdma::emit_fence(v, BUF + 0x40, 7, false, false).unwrap();
        sdma::copy_linear(v, BUF + 0x1000, BUF + 0x3003, 4096, false).unwrap();
        sdma::emit_fence(v, BUF + 0x48, 8, true, true).unwrap();
    });
    let rep = r.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(r.eng.rptr(), w);
    assert_eq!(r.mem.peek_u32(0, BUF), Some(0xDEAD_BEEF));
    assert_eq!(r.mem.peek_u32(0, BUF + 0x1FFC), Some(0xA5A5_5A5A));
    assert_eq!(r.mem.peek_u32(0, BUF + 0x2000), Some(0));
    let mut b = [0u8; 4];
    r.mem.peek(0, BUF + 0x3003, &mut b).unwrap();
    assert_eq!(u32::from_le_bytes(b), 0xA5A5_5A5A, "unaligned copy");
    assert_eq!(r.mem.peek_u32(0, BUF + 0x40), Some(7));
    assert_eq!(r.mem.peek_u64(0, BUF + 0x48), Some(8));
    let fences: Vec<_> = rep.events.iter().filter(|e| matches!(e, Event::Fence { .. })).collect();
    assert_eq!(fences.len(), 3);
    assert_eq!(rep.events.last(), Some(&Event::Interrupt { src: IntSrc::Trap, ctx: 0 }));
}

#[test]
fn byte_fill_and_fill_with_byte_size() {
    let mut r = Rig::new(SDMA, 64, limits());
    let w = r.submit(|v| sdma::const_fill(v, BUF + 1, 0x1234_56AB, 3, sdma::FILL_BYTE).unwrap());
    assert_eq!(r.run(w).stop, Stop::Idle);
    assert_eq!(r.mem.peek_u32(0, BUF), Some(0xABAB_AB00));
}

#[test]
fn packets_apply_in_ring_order() {
    // A later write to the same address wins; the fence is reported after every earlier write.
    let mut r = Rig::new(Kind::Gfx, 1024, limits());
    let w = r.submit(|v| {
        pm4::write_data(v, pm4::dst_sel::MEM, BUF, 0, &[1, 2]).unwrap();
        pm4::write_data(v, pm4::dst_sel::MEM, BUF + 4, pm4::WR_CONFIRM, &[3]).unwrap();
        pm4::fence(v, BUF + 0x100, 5, true, false).unwrap();
        pm4::write_data(v, pm4::dst_sel::MEM, BUF + 8, pm4::WR_ONE_ADDR, &[4, 5, 6]).unwrap();
    });
    let rep = r.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(r.mem.peek_u32(0, BUF + 4), Some(3));
    assert_eq!(r.mem.peek_u32(0, BUF + 8), Some(6), "one address: the last dword wins");
    assert_eq!(r.mem.peek_u32(0, BUF + 12), Some(0));
    let k: Vec<u8> = rep
        .events
        .iter()
        .map(|e| match e {
            Event::MemWrite { .. } => b'w',
            Event::Fence { .. } => b'f',
            _ => b'?',
        })
        .collect();
    assert_eq!(k, b"wwfw");
}

#[test]
fn release_mem_32_writes_only_low() {
    let mut r = Rig::new(Kind::Gfx, 512, limits());
    r.mem.poke_dwords(0, BUF, &[0xFFFF_FFFF, 0xFFFF_FFFF]).unwrap();
    let w = r.submit(|v| pm4::fence(v, BUF, 0x1_0000_0009, false, false).unwrap());
    r.run(w);
    assert_eq!(r.mem.peek_u64(0, BUF), Some(0xFFFF_FFFF_0000_0009));
}

#[test]
fn ring_wrap_executes() {
    let mut r = Rig::new(SDMA, 64, limits());
    // Move the pointers near the end: 3 submissions of 16.
    for _ in 0..3 {
        let w = r.submit(|v| sdma::nops(v, 1, false).unwrap());
        r.run(w);
    }
    assert_eq!(r.eng.rptr(), 48);
    let data: Vec<u32> = (0..20).collect();
    let w = r.submit(|v| sdma::write_linear(v, BUF, &data).unwrap());
    assert_eq!(r.run(w).stop, Stop::Idle);
    assert_eq!(r.mem.peek_u32(0, BUF + 19 * 4), Some(19));
}

#[test]
fn budget_resumes_where_it_stopped() {
    let mut one = Rig::new(Kind::Gfx, 4096, limits());
    let mut many = Rig::new(Kind::Gfx, 4096, Limits { max_dw: 9, ..limits() });
    for r in [&mut one, &mut many] {
        let w = r.submit(|v| {
            for i in 0..20u32 {
                pm4::write_data(v, pm4::dst_sel::MEM, BUF + 4 * i as u64, 0, &[i + 1]).unwrap();
            }
            pm4::fence(v, BUF + 0x200, 1, true, false).unwrap();
        });
        let mut runs = 0;
        loop {
            runs += 1;
            let rep = r.run(w);
            assert!(rep.fetched_dw <= r.eng.limits().max_dw);
            match rep.stop {
                Stop::Idle => break,
                Stop::Budget => assert!(runs < 1000),
                s => panic!("{s:?}"),
            }
        }
    }
    for i in 0..0x210 {
        assert_eq!(one.mem.peek_u32(0, BUF + i), many.mem.peek_u32(0, BUF + i));
    }
    assert_eq!(many.mem.peek_u64(0, BUF + 0x200), Some(1));
}

#[test]
fn byte_budget() {
    let mut r = Rig::new(SDMA, 64, Limits { max_bytes: 4096, ..limits() });
    let w = r.submit(|v| {
        sdma::const_fill(v, BUF, 1, 4096, sdma::FILL_DWORD).unwrap();
        sdma::const_fill(v, BUF + 4096, 2, 4096, sdma::FILL_DWORD).unwrap();
    });
    assert_eq!(r.run(w).stop, Stop::Budget);
    assert_eq!(r.mem.peek_u32(0, BUF + 4096), Some(0));
    assert_eq!(r.run(w).stop, Stop::Idle);
    assert_eq!(r.mem.peek_u32(0, BUF + 8188), Some(2));
    let w = r.submit(|v| sdma::const_fill(v, BUF, 1, 4100, sdma::FILL_DWORD).unwrap());
    let rep = r.run(w);
    assert!(matches!(rep.stop, Stop::Hang(Hang::TooLarge { .. })));
}

#[test]
fn polls_wait_and_resume() {
    let mut r = Rig::new(Kind::Gfx, 512, limits());
    let w = r.submit(|v| {
        let wrm = pm4::WaitRegMem {
            function: 3,
            mem: true,
            operation: 0,
            engine: 0,
            addr0: (BUF + 0x10) as u32,
            addr1: 0,
            reference: 1,
            mask: 1,
            interval: 4,
        };
        pm4::wait_reg_mem(v, &wrm).unwrap();
        pm4::fence(v, BUF + 0x20, 9, true, false).unwrap();
    });
    let rep = r.run(w);
    assert!(matches!(rep.stop, Stop::Wait(WaitOn::Mem { value: 0, reference: 1, .. })));
    assert_eq!(r.mem.peek_u64(0, BUF + 0x20), Some(0));
    assert_eq!(r.run(w).stop, rep.stop, "still waiting");
    r.mem.poke_dwords(0, BUF + 0x10, &[3]).unwrap();
    assert_eq!(r.run(w).stop, Stop::Idle);
    assert_eq!(r.mem.peek_u64(0, BUF + 0x20), Some(9));
}

#[test]
fn hdp_flush_both_engines() {
    const REQ: u32 = 0x1000;
    const DONE: u32 = 0x1001;
    // SDMA form (sdma_v6_0.c:307): done in dword 1, request in dword 2, both as byte offsets.
    let p = sdma::PollRegMem {
        hdp_flush: true,
        mem: false,
        function: 3,
        addr_lo: DONE << 2,
        addr_hi: REQ << 2,
        reference: 4,
        mask: 4,
        interval: 10,
        retry: 0xFFF,
    };
    let mut r = Rig::new(SDMA, 64, limits());
    r.mem.mirror(REQ, DONE);
    let w = r.submit(|v| sdma::poll_regmem(v, &p).unwrap());
    let rep = r.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(rep.events[0], Event::HdpFlush);
    assert_eq!(rep.events[1], Event::RegWrite { reg: REQ, value: 4, modelled: true });
    // GFX form (gfx_v11_0.c:5272): WAIT_REG_MEM write-then-wait, request first.
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    g.mem.mirror(REQ, DONE);
    let w = g.submit(|v| {
        let wrm = pm4::WaitRegMem {
            function: 3,
            mem: false,
            operation: 1,
            engine: 1,
            addr0: REQ,
            addr1: DONE,
            reference: 2,
            mask: 2,
            interval: 0x20,
        };
        pm4::wait_reg_mem(v, &wrm).unwrap();
    });
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(rep.events[0], Event::RegWrite { reg: REQ, value: 2, modelled: true });
}

fn flushes_and_writes(evs: &[Event]) -> (usize, usize) {
    let f = evs.iter().filter(|e| **e == Event::HdpFlush).count();
    let w = evs.iter().filter(|e| matches!(e, Event::RegWrite { .. })).count();
    (f, w)
}

/// A poll that waits is retried by later runs: its write half (HDP-flush request, write-wait write)
/// runs once per packet, only the compare repeats.
#[test]
fn waiting_poll_writes_once() {
    const REQ: u32 = 0x1000;
    const DONE: u32 = 0x1001;
    let p = sdma::PollRegMem {
        hdp_flush: true,
        mem: false,
        function: 3,
        addr_lo: DONE << 2,
        addr_hi: REQ << 2,
        reference: 4,
        mask: 4,
        interval: 10,
        retry: 0xFFF,
    };
    let mut r = Rig::new(SDMA, 64, limits());
    let w = r.submit(|v| {
        sdma::poll_regmem(v, &p).unwrap();
        sdma::poll_regmem(v, &p).unwrap();
    });
    let first = r.run(w);
    assert!(matches!(first.stop, Stop::Wait(WaitOn::Reg { reg: DONE, value: None, .. })));
    assert_eq!(flushes_and_writes(&first.events), (1, 1));
    r.mem.set_reg(REQ, 0);
    for _ in 0..3 {
        let again = r.run(w);
        assert_eq!(again.stop, first.stop);
        assert_eq!(flushes_and_writes(&again.events), (0, 0));
    }
    assert_eq!(r.mem.reg(REQ), Some(0), "the retry did not write the request again");
    // The flush completes; the second poll is a new packet and writes its own request.
    r.mem.set_reg(DONE, 4);
    let done = r.run(w);
    assert_eq!(done.stop, Stop::Idle);
    assert_eq!(flushes_and_writes(&done.events), (1, 1));
    // A reset forgets the armed packet: the same position writes again.
    let mut r = Rig::new(SDMA, 64, limits());
    let w = r.submit(|v| sdma::poll_regmem(v, &p).unwrap());
    assert_eq!(flushes_and_writes(&r.run(w).events), (1, 1));
    let at = r.eng.rptr();
    r.eng.reset(at);
    assert_eq!(flushes_and_writes(&r.run(w).events), (1, 1));
    assert_eq!(flushes_and_writes(&r.run(w).events), (0, 0));

    // GFX write-wait (WAIT_REG_MEM operation 1).
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| {
        let wrm = pm4::WaitRegMem {
            function: 3,
            mem: false,
            operation: 1,
            engine: 1,
            addr0: REQ,
            addr1: DONE,
            reference: 2,
            mask: 2,
            interval: 0x20,
        };
        pm4::wait_reg_mem(v, &wrm).unwrap();
    });
    let first = g.run(w);
    assert!(matches!(first.stop, Stop::Wait(_)));
    assert_eq!(flushes_and_writes(&first.events), (0, 1));
    let again = g.run(w);
    assert!(matches!(again.stop, Stop::Wait(_)));
    assert_eq!(flushes_and_writes(&again.events), (0, 0));
    g.mem.set_reg(DONE, 2);
    let done = g.run(w);
    assert_eq!(done.stop, Stop::Idle);
    assert_eq!(flushes_and_writes(&done.events), (0, 0));
}

#[test]
fn register_writes() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| {
        // Linux's GFX ring test: SCRATCH_REG0 through SET_UCONFIG_REG (gfx_v11_0.c:313).
        pm4::set_reg(v, pm4::RegSpace::Uconfig, 0xc040, &[0xDEAD_BEEF, 7]).unwrap();
        pm4::write_data(v, pm4::dst_sel::REG, 0x1234, 0, &[5]).unwrap();
    });
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(g.mem.reg(0xc040), Some(0xDEAD_BEEF));
    assert_eq!(g.mem.reg(0xc041), Some(7));
    assert_eq!(g.mem.reg(0x1234), Some(5));
    let mut s = Rig::new(SDMA, 64, limits());
    let w = s.submit(|v| sdma::srbm_write(v, 0x55, 9).unwrap());
    let rep = s.run(w);
    assert_eq!(rep.events, [Event::RegWrite { reg: 0x55, value: 9, modelled: true }]);
}

#[test]
fn cond_exec_skips() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| {
        pm4::cond_exec(v, BUF + 0x80, 6).unwrap();
        pm4::write_data(v, pm4::dst_sel::MEM, BUF, 0, &[1, 2]).unwrap(); // 6 dwords: skipped
        pm4::write_data(v, pm4::dst_sel::MEM, BUF + 8, 0, &[3]).unwrap();
    });
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(g.mem.peek_u32(0, BUF), Some(0));
    assert_eq!(g.mem.peek_u32(0, BUF + 8), Some(3));
    assert!(rep.events.contains(&Event::CondSkip { dwords: 6 }));
    // SDMA: executes only when the flag equals the reference (Linux keeps it at 1).
    let mut s = Rig::new(SDMA, 64, limits());
    s.mem.poke_dwords(0, BUF + 0x80, &[1]).unwrap();
    let w = s.submit(|v| {
        sdma::cond_exe(v, BUF + 0x80, 1, 5).unwrap();
        sdma::write_linear(v, BUF, &[7]).unwrap();
    });
    assert_eq!(s.run(w).stop, Stop::Idle);
    assert_eq!(s.mem.peek_u32(0, BUF), Some(7));
}

#[test]
fn ib_nesting_and_limits() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let ib1 = BUF + 0x1000;
    let ib2 = BUF + 0x2000;
    let mut v2 = Vec::new();
    pm4::write_data(&mut v2, pm4::dst_sel::MEM, BUF, 0, &[0x22]).unwrap();
    let mut v1 = Vec::new();
    pm4::indirect_buffer(&mut v1, ib2, v2.len() as u32, 0, false, 0).unwrap();
    pm4::write_data(&mut v1, pm4::dst_sel::MEM, BUF + 4, 0, &[0x11]).unwrap();
    g.mem.poke_dwords(0, ib1, &v1).unwrap();
    g.mem.poke_dwords(0, ib2, &v2).unwrap();
    let w = g.submit(|v| pm4::indirect_buffer(v, ib1, v1.len() as u32, 0, false, 0).unwrap());
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!((g.mem.peek_u32(0, BUF), g.mem.peek_u32(0, BUF + 4)), (Some(0x22), Some(0x11)));
    // An IB that calls itself is stopped by the depth limit.
    let mut lp = Vec::new();
    pm4::indirect_buffer(&mut lp, ib1, 4, 0, false, 0).unwrap();
    g.mem.poke_dwords(0, ib1, &lp).unwrap();
    let w = g.submit(|v| pm4::indirect_buffer(v, ib1, 4, 0, false, 0).unwrap());
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Hang(Hang::IbTooDeep { depth: 3 }));
    let at = rep.at.ib.unwrap();
    assert_eq!((at.base, at.depth, at.offset_dw), (ib1, 2, 0), "the hang points at the INDIRECT packet");
    // SDMA allows no IB below an IB when the fake says so.
    let mut s = Rig::new(SDMA, 64, Limits { max_ib_depth: 1, ..limits() });
    let mut sib = Vec::new();
    sdma::indirect(&mut sib, 0, ib1, 8, 0).unwrap();
    s.mem.poke_dwords(0, ib1, &sib).unwrap();
    let w = s.submit(|v| sdma::indirect(v, 0, ib1, 8, 0).unwrap());
    assert_eq!(s.run(w).stop, Stop::Hang(Hang::IbTooDeep { depth: 2 }));
}

#[test]
fn faults_halt_until_reset() {
    let mut s = Rig::new(SDMA, 64, limits());
    s.mem.map_read_only(0, 0x4000_0000, 4096).unwrap();
    let w = s.submit(|v| {
        sdma::write_linear(v, 0x9000_0000, &[1]).unwrap();
    });
    let rep = s.run(w);
    let f = MemFault { vmid: 0, addr: 0x9000_0000, write: true };
    assert_eq!(rep.stop, Stop::Hang(Hang::Fault(f)));
    assert_eq!(Hang::Fault(f).errno(), crate::errno::EFAULT);
    assert_eq!(rep.at.rptr, 0, "the hang points at the packet");
    assert_eq!(s.run(w).stop, rep.stop, "stays halted");
    s.eng.reset(w);
    assert_eq!(s.run(w).stop, Stop::Idle);
    let w = s.submit(|v| sdma::write_linear(v, 0x4000_0000, &[1]).unwrap());
    assert!(matches!(s.run(w).stop, Stop::Hang(Hang::Fault(MemFault { write: true, .. }))));
    s.eng.reset(w);
    let w = s.submit(|v| sdma::copy_linear(v, 0xA000_0000, BUF, 8, false).unwrap());
    assert!(matches!(s.run(w).stop, Stop::Hang(Hang::Fault(MemFault { write: false, addr: 0xA000_0000, .. }))));
}

#[test]
fn nested_ib_inherits_the_vmid() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    g.mem.map(4, 0x4000_0000, 0x1000).unwrap();
    let mut inner = Vec::new();
    // Asks for VMID 0 and the kernel's buffer: runs in VMID 4, where that address is unmapped.
    pm4::write_data(&mut inner, pm4::dst_sel::MEM, BUF, 0, &[1]).unwrap();
    let mut outer = Vec::new();
    pm4::indirect_buffer(&mut outer, 0x4000_0100, inner.len() as u32, 0, false, 0).unwrap();
    g.mem.poke_dwords(4, 0x4000_0000, &outer).unwrap();
    g.mem.poke_dwords(4, 0x4000_0100, &inner).unwrap();
    let w = g.submit(|v| pm4::indirect_buffer(v, 0x4000_0000, outer.len() as u32, 4, false, 0).unwrap());
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Hang(Hang::Fault(MemFault { vmid: 4, addr: BUF, write: true })));
    assert_eq!(g.mem.peek_u32(0, BUF), Some(0));
}

#[test]
fn ib_in_unmapped_memory_faults() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| pm4::indirect_buffer(v, 0x7000_0000, 16, 2, false, 0).unwrap());
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Hang(Hang::Fault(MemFault { vmid: 2, addr: 0x7000_0000, write: false })));
    assert_eq!(rep.events.last(), Some(&Event::Ib { vmid: 2, addr: 0x7000_0000, len_dw: 16, depth: 1 }));
}

#[test]
fn incomplete_ring_packet_waits_for_more() {
    let mut s = Rig::new(SDMA, 64, limits());
    let mut v = Vec::new();
    sdma::write_linear(&mut v, BUF, &[1, 2, 3]).unwrap();
    s.mem.poke_dwords(0, RING, &v).unwrap();
    // The driver moved wptr only past the first 5 of 7 dwords.
    let rep = s.eng.run(&mut s.mem, 5);
    assert_eq!(rep.stop, Stop::Incomplete { need: 7, have: 5 });
    assert_eq!(s.eng.rptr(), 0);
    let rep = s.eng.run(&mut s.mem, 2);
    assert_eq!(rep.stop, Stop::Incomplete { need: 4, have: 2 });
    assert_eq!(s.eng.run(&mut s.mem, 7).stop, Stop::Idle);
    assert_eq!(s.mem.peek_u32(0, BUF + 8), Some(3));
}

#[test]
fn truncated_packet_in_ib_halts() {
    let mut s = Rig::new(SDMA, 64, limits());
    let mut ib = Vec::new();
    sdma::write_linear(&mut ib, BUF, &[1, 2, 3, 4, 5]).unwrap();
    s.mem.poke_dwords(0, BUF + 0x1000, &ib).unwrap();
    let w = s.submit(|v| sdma::indirect(v, 0, BUF + 0x1000, 8, 0).unwrap());
    assert_eq!(s.run(w).stop, Stop::Hang(Hang::Truncated { need: 9, have: 8 }));
    assert_eq!(s.mem.peek_u32(0, BUF), Some(0), "nothing of a truncated packet runs");
}

#[test]
fn bad_wptr_halts() {
    let mut s = Rig::new(SDMA, 64, limits());
    assert_eq!(s.run(65).stop, Stop::Hang(Hang::BadWptr { rptr: 0, wptr: 65 }));
}

#[test]
fn unsupported_and_unknown_halt() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| v.extend_from_slice(&[pm4::packet3(0x47, 4), 0, 0, 0, 0, 0]));
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Hang(Hang::Unsupported { header: 0xc0044700 }));
    // DMA_DATA into GDS or a register is not modelled.
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| v.extend_from_slice(&[pm4::packet3(pm4::op::DMA_DATA, 5), 1 << 20 | 2 << 29, 0, 0, 0, 0, 4]));
    assert_eq!(g.run(w).stop, Stop::Hang(Hang::Unsupported { header: 0xc0055000 }));
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| v.extend_from_slice(&[pm4::packet3(pm4::op::ACQUIRE_MEM, 6), 0, 0, 0, 0, 0, 0, 0]));
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(rep.events, [Event::NoEffect { header: 0xc0065800 }]);
    let mut s = Rig::new(SDMA, 64, limits());
    let w = s.submit(|v| v.push(3));
    assert_eq!(s.run(w).stop, Stop::Hang(Hang::Sdma(sdma::DecodeError::Unknown { header: 3 })));
    let mut s = Rig::new(SDMA, 64, limits());
    let w = s.submit(|v| v.extend_from_slice(&[12, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(s.run(w).stop, Stop::Hang(Hang::Unsupported { header: 12 }), "PTEPDE: tables are written by the CPU");
}

/// Physical pages for the translated bus.
struct Phys(std::collections::BTreeMap<u64, u8>);

impl crate::bus::PhysMem for Phys {
    fn read_phys(&mut self, phys: u64, out: &mut [u8]) -> bool {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.0.get(&(phys + i as u64)).copied().unwrap_or(0);
        }
        true
    }
    fn write_phys(&mut self, phys: u64, data: &[u8]) -> bool {
        for (i, &b) in data.iter().enumerate() {
            self.0.insert(phys + i as u64, b);
        }
        true
    }
}

/// Ladder rung 4 on the fake: SDMA copies inside a user VM built by GR-1 (scattered system pages, a page
/// boundary in the middle of the copy), then touches an unmapped address and faults with the VM's VMID.
#[test]
fn sdma_in_a_user_vm_through_the_walker() {
    use crate::bus::{Translated, Translation, vm_translation};
    use crate::pte::MapFlags;
    use crate::vm::{Backing, Vm};
    let mut store = super::MemStore::new();
    let mut vm = Vm::new(&mut store, super::whole()).unwrap();
    let pages = [0x5000, 0x9_0000, 0x3000];
    vm.map(&mut store, 0x40_0000, 3 * 4096, Backing::System { pages: &pages, snooped: true }, MapFlags::RW).unwrap();
    let root = vm.root().phys;
    let mut v = Vec::new();
    sdma::write_linear(&mut v, 0x40_0ff8, &[0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444]).unwrap();
    sdma::copy_linear(&mut v, 0x40_0ff8, 0x40_2000, 16, false).unwrap();
    sdma::write_linear(&mut v, 0x50_0000, &[1]).unwrap();
    let mut bus = Translated {
        mem: Phys(std::collections::BTreeMap::new()),
        translate: |vmid: u8, va: u64, write: bool| {
            if vmid == 0 { Translation::Fault } else { vm_translation(&store, root, va, write) }
        },
    };
    // Put the packets as an IB into the user VM (through the bus, as the CPU would through its mapping).
    crate::bus::GpuBus::write(&mut bus, 5, 0x40_1000, &v.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    let mut e = Engine::new(Isa::Sdma, RING, 64, 0, limits()).unwrap();
    let rep = e.exec_ib(&mut bus, 5, 0x40_1000, v.len() as u64);
    assert_eq!(rep.stop, Stop::Hang(Hang::Fault(MemFault { vmid: 5, addr: 0x50_0000, write: true })));
    // The write crossed from page 0x5000 into page 0x9_0000; the copy landed in page 0x3000.
    assert_eq!(bus.mem.0.get(&0x5ff8), Some(&0x11));
    assert_eq!(bus.mem.0.get(&0x9_0000), Some(&0x33));
    assert_eq!(bus.mem.0.get(&0x3000), Some(&0x11));
    assert_eq!(bus.mem.0.get(&0x300c), Some(&0x44));
}

/// INT-W28 (GX-7m CR): Linux's VM flush (gfx_v11_0_ring_emit_reg_write_reg_wait) waits with reference = the whole
/// invalidate request and mask = 1 << vmid; the ack register reading 1 << vmid ends the wait.
#[test]
fn linux_vm_flush_ack_masks_the_reference() {
    const REQ: u32 = 0x1a00;
    const ACK: u32 = 0x1a12;
    let (vmid, req) = (1u32, 0x00f8_0002u32);
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    let w = g.submit(|v| {
        let wrm = pm4::WaitRegMem { function: 3, mem: false, operation: 1, engine: 1, addr0: REQ, addr1: ACK, reference: req, mask: 1 << vmid, interval: 0x20 };
        pm4::wait_reg_mem(v, &wrm).unwrap();
    });
    g.mem.set_reg(ACK, 0);
    assert!(matches!(g.run(w).stop, Stop::Wait(WaitOn::Reg { reg: ACK, .. })));
    assert_eq!(g.mem.reg(REQ), Some(req), "the request was written");
    g.mem.set_reg(ACK, 1 << vmid);
    assert_eq!(g.run(w).stop, Stop::Idle);
    // SDMA's POLL_REGMEM compares the same way.
    let mut s = Rig::new(SDMA, 64, limits());
    let p = sdma::PollRegMem { hdp_flush: false, mem: false, function: 3, addr_lo: ACK << 2, addr_hi: 0, reference: req, mask: 1 << vmid, interval: 10, retry: 0xFFF };
    let w = s.submit(|v| sdma::poll_regmem(v, &p).unwrap());
    s.mem.set_reg(ACK, 1 << vmid);
    assert_eq!(s.run(w).stop, Stop::Idle);
}

/// INT-W28 (GM-1c CR-GM1c-1): radeonsi's buffer clear (DMA_DATA with DATA to L2), a DMA_DATA copy, COPY_DATA
/// and the draw/dispatch packets as no-effect work.
#[test]
fn dma_data_copy_data_and_draws() {
    let mut g = Rig::new(Kind::Gfx, 512, limits());
    g.mem.poke_dwords(0, BUF, &[1, 2, 3, 4]).unwrap();
    let w = g.submit(|v| {
        // Fill 16 bytes at BUF + 0x100 with 0xAB (src DATA, dst L2).
        v.extend_from_slice(&[pm4::packet3(pm4::op::DMA_DATA, 5), 3 << 20 | 2 << 29, 0xAB, 0, (BUF + 0x100) as u32, 0, 16]);
        // Copy 8 bytes from BUF to BUF + 0x200 (both through L2).
        v.extend_from_slice(&[pm4::packet3(pm4::op::DMA_DATA, 5), 3 << 20 | 3 << 29, BUF as u32, 0, (BUF + 0x200) as u32, 0, 8]);
        // COPY_DATA: immediate 0x55 to memory, then memory to memory.
        v.extend_from_slice(&[pm4::packet3(pm4::op::COPY_DATA, 4), 5 | 5 << 8, 0x55, 0, (BUF + 0x300) as u32, 0]);
        v.extend_from_slice(&[pm4::packet3(pm4::op::COPY_DATA, 4), 1 | 5 << 8, (BUF + 8) as u32, 0, (BUF + 0x304) as u32, 0]);
        // radeonsi's L2 prefetch (DST_SEL nowhere) and its zero-byte CP_SYNC wait.
        v.extend_from_slice(&[pm4::packet3(pm4::op::DMA_DATA, 5), 0x6020_0000, BUF as u32, 0, BUF as u32, 0, 0x8000_0100]);
        v.extend_from_slice(&[pm4::packet3(pm4::op::DMA_DATA, 5), 0x6030_0000, 0, 0, 0, 0, 0]);
        // SET_UCONFIG_REG_INDEX with index 2 (radeonsi's form): a register write at the offset.
        v.extend_from_slice(&[pm4::packet3(pm4::op::SET_UCONFIG_REG_INDEX, 1), 0x2000_0243, 1]);
        v.extend_from_slice(&[pm4::packet3(pm4::op::DISPATCH_DIRECT, 3), 1, 1, 1, 0]);
        v.extend_from_slice(&[pm4::packet3(pm4::op::NUM_INSTANCES, 0), 1]);
        v.extend_from_slice(&[pm4::packet3(pm4::op::DRAW_INDEX_AUTO, 1), 3, 2]);
    });
    let rep = g.run(w);
    assert_eq!(rep.stop, Stop::Idle);
    let dw = |m: &crate::bus::SimMem, a: u64, n: u64| (0..n).map(|i| m.peek_u32(0, a + i * 4).unwrap()).collect::<Vec<u32>>();
    assert_eq!(dw(&g.mem, BUF + 0x100, 4), [0xAB; 4]);
    assert_eq!(dw(&g.mem, BUF + 0x200, 2), [1, 2]);
    assert_eq!(dw(&g.mem, BUF + 0x300, 2), [0x55, 3]);
    let noeffect = rep.events.iter().filter(|e| matches!(e, Event::NoEffect { .. })).count();
    assert_eq!(noeffect, 4, "{:?}", rep.events);
    assert!(rep.events.iter().any(|e| matches!(e, Event::RegWrite { reg: 0xc243, value: 1, .. })), "{:?}", rep.events);
    // A DMA_DATA bigger than the run's byte budget halts as too large.
    let lim = limits();
    let mut g = Rig::new(Kind::Gfx, 512, lim);
    let n = (lim.max_bytes + 4) as u32;
    let w = g.submit(|v| v.extend_from_slice(&[pm4::packet3(pm4::op::DMA_DATA, 5), 3 << 20 | 2 << 29, 0, 0, BUF as u32, 0, n]));
    assert!(matches!(g.run(w).stop, Stop::Hang(Hang::TooLarge { .. })));
}

/// CR-GX7m2-1: ring and IB dwords come through `GpuBus::fetch`, data through `read`/`write`. A bus that refuses
/// fetches (a page without EXECUTABLE) stops the engine with a read fault at the command's address before
/// anything runs; with the default `fetch` (a plain read) the same commands run.
#[test]
fn commands_come_through_fetch() {
    use crate::bus::GpuBus;
    struct NoExec(SimMem);
    impl GpuBus for NoExec {
        fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
            self.0.read(vmid, addr, out)
        }
        fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
            self.0.write(vmid, addr, data)
        }
        fn fetch(&mut self, vmid: u8, addr: u64, _out: &mut [u8]) -> Result<(), MemFault> {
            Err(MemFault { vmid, addr, write: false })
        }
    }
    let mut v = Vec::new();
    sdma::write_linear(&mut v, BUF + 0x1000, &[0x5a]).unwrap();
    // An IB at BUF.
    let mut r = Rig::new(SDMA, 64, limits());
    r.mem.poke_dwords(0, BUF, &v).unwrap();
    let mut bus = NoExec(r.mem);
    let rep = r.eng.exec_ib(&mut bus, 0, BUF, v.len() as u64);
    assert_eq!(rep.stop, Stop::Hang(Hang::Fault(MemFault { vmid: 0, addr: BUF, write: false })));
    assert_eq!(bus.0.peek_u32(0, BUF + 0x1000), Some(0), "nothing ran");
    // The ring too.
    let mut r = Rig::new(SDMA, 64, limits());
    let w = r.submit(|d| d.extend_from_slice(&v));
    let mut bus = NoExec(r.mem);
    let rep = r.eng.run(&mut bus, w);
    assert_eq!(rep.stop, Stop::Hang(Hang::Fault(MemFault { vmid: 0, addr: RING, write: false })));
    // The default fetch reads.
    let mut r = Rig::new(SDMA, 64, limits());
    r.mem.poke_dwords(0, BUF, &v).unwrap();
    let rep = r.eng.exec_ib(&mut r.mem, 0, BUF, v.len() as u64);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(r.mem.peek_u32(0, BUF + 0x1000), Some(0x5a));
}
