//! Host tests: the verifier against hostile programs, the interpreter's arithmetic, the stack's precedence,
//! the action cache against a full run, and the thread state's rules.

use crate::bpf::{Insn, Op, Program, Refused};
use crate::consts::*;
use crate::filter::{Data, Filter};
use crate::task::{AttachError, TaskSeccomp, Verdict, change};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const LD_W_ABS: u16 = 0x20;
const LD_IMM: u16 = 0x00;
const LDX_IMM: u16 = 0x01;
const RET_K: u16 = 0x06;
const RET_A: u16 = 0x16;
const JA: u16 = 0x05;
const JEQ_K: u16 = 0x15;
const JGE_K: u16 = 0x35;
const JSET_K: u16 = 0x45;
const ST: u16 = 0x02;
const LD_MEM: u16 = 0x60;
const DIV_K: u16 = 0x34;
const DIV_X: u16 = 0x3c;
const LSH_K: u16 = 0x64;
const LSH_X: u16 = 0x6c;
const RSH_X: u16 = 0x7c;
const MOD_K: u16 = 0x94;
const NEG: u16 = 0x84;
const ADD_K: u16 = 0x04;
const MUL_X: u16 = 0x2c;
const TAX: u16 = 0x07;
const TXA: u16 = 0x87;

fn s(code: u16, k: u32) -> Insn {
    Insn::new(code, 0, 0, k)
}
fn j(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn::new(code, jt, jf, k)
}
fn load(p: &[Insn]) -> Result<Program, Refused> {
    Program::load(p)
}
fn data(nr: u32, args: [u64; 6]) -> Data {
    Data { nr, arch: AUDIT_ARCH_X86_64, ip: 0x1234_5678_9abc, args }
}
fn run(p: &[Insn], d: &Data) -> u32 {
    load(p).expect("valid").run(&d.words())
}

/// The usual shape: kill other arches, `ret_for(nr)` for listed numbers, ALLOW for the rest.
fn policy(rules: &[(u32, u32)]) -> Vec<Insn> {
    let mut p = vec![s(LD_W_ABS, 4), j(JEQ_K, AUDIT_ARCH_X86_64, 1, 0), s(RET_K, RET_KILL_PROCESS), s(LD_W_ABS, 0)];
    for &(nr, ret) in rules {
        p.push(j(JEQ_K, nr, 0, 1));
        p.push(s(RET_K, ret));
    }
    p.push(s(RET_K, RET_ALLOW));
    p
}

#[test]
fn lengths() {
    assert_eq!(load(&[]).err(), Some(Refused::Length));
    let mut p = vec![s(LD_IMM, 1); MAX_INSNS - 1];
    p.push(s(RET_K, RET_ALLOW));
    assert!(load(&p).is_ok(), "4096 instructions are allowed");
    p.insert(0, s(LD_IMM, 1));
    assert_eq!(load(&p).err(), Some(Refused::Length), "4097 are not");
    assert_eq!(crate::load(&[0u8; 7]).err(), Some(Refused::Length));
}

#[test]
fn jumps_forward_and_inside() {
    // JA to the last instruction is fine, one further is out.
    assert!(load(&[s(JA, 1), s(RET_K, 0), s(RET_K, RET_ALLOW)]).is_ok());
    assert_eq!(load(&[s(JA, 2), s(RET_K, 0), s(RET_K, RET_ALLOW)]).err(), Some(Refused::Jump(0)));
    assert_eq!(load(&[s(JA, 0xffff_ffff), s(RET_K, 0)]).err(), Some(Refused::Jump(0)));
    // A JA as the last instruction can go nowhere (and is no return).
    assert_eq!(load(&[s(RET_K, 0), s(JA, 0)]).err(), Some(Refused::Jump(1)));
    assert_eq!(load(&[j(JEQ_K, 1, 2, 0), s(RET_K, 0), s(RET_K, 1)]).err(), Some(Refused::Jump(0)));
    assert_eq!(load(&[j(JEQ_K, 1, 0, 2), s(RET_K, 0), s(RET_K, 1)]).err(), Some(Refused::Jump(0)));
    assert!(load(&[j(JEQ_K, 1, 1, 0), s(RET_K, 0), s(RET_K, 1)]).is_ok());
    assert_eq!(load(&[s(RET_K, 0), j(JEQ_K, 0, 255, 255)]).err(), Some(Refused::Jump(1)));
}

#[test]
fn loads_of_seccomp_data_only() {
    for off in [0u32, 4, 8, 12, 16, 60] {
        assert!(load(&[s(LD_W_ABS, off), s(RET_A, 0)]).is_ok(), "offset {off}");
    }
    for off in [64u32, 2, 61, 0xffff_f000, 0xffff_ffff] {
        assert_eq!(load(&[s(LD_W_ABS, off), s(RET_A, 0)]).err(), Some(Refused::Load(0)), "offset {off}");
    }
    // Half-word, byte, indirect and MSH loads are socket-filter only.
    for code in [0x28u16, 0x30, 0x40, 0x48, 0x50, 0xb1] {
        assert_eq!(load(&[s(code, 0), s(RET_A, 0)]).err(), Some(Refused::Opcode(0)), "code {code:#x}");
    }
    // The length loads read sizeof(seccomp_data).
    assert_eq!(run(&[s(0x80, 0), s(RET_A, 0)], &data(0, [0; 6])), 64);
    assert_eq!(run(&[s(0x81, 0), s(TXA, 0), s(RET_A, 0)], &data(0, [0; 6])), 64);
}

#[test]
fn arithmetic_rules() {
    assert_eq!(load(&[s(DIV_K, 0), s(RET_A, 0)]).err(), Some(Refused::DivByZero(0)));
    assert_eq!(load(&[s(MOD_K, 3), s(RET_A, 0)]).err(), Some(Refused::Opcode(0)), "MOD is not for seccomp");
    assert_eq!(load(&[s(0x9c, 0), s(RET_A, 0)]).err(), Some(Refused::Opcode(0)));
    assert_eq!(load(&[s(LSH_K, 32), s(RET_A, 0)]).err(), Some(Refused::Shift(0)));
    assert_eq!(load(&[s(0x74, 32), s(RET_A, 0)]).err(), Some(Refused::Shift(0)));
    assert_eq!(load(&[s(0x8c, 0), s(RET_A, 0)]).err(), Some(Refused::Opcode(0)), "NEG has no X form");
    let d = data(7, [0; 6]);
    // Division by X = 0 ends the program with 0 (KILL_THREAD).
    assert_eq!(run(&[s(LD_IMM, 10), s(LDX_IMM, 0), s(DIV_X, 0), s(RET_K, RET_ALLOW)], &d), 0);
    assert_eq!(run(&[s(LD_IMM, 10), s(LDX_IMM, 3), s(DIV_X, 0), s(RET_A, 0)], &d), 3);
    // A shift by X uses its low five bits.
    assert_eq!(run(&[s(LD_IMM, 1), s(LDX_IMM, 33), s(LSH_X, 0), s(RET_A, 0)], &d), 2);
    assert_eq!(run(&[s(LD_IMM, 0x8000_0000), s(LDX_IMM, 63), s(RSH_X, 0), s(RET_A, 0)], &d), 1);
    assert_eq!(run(&[s(LD_IMM, 5), s(NEG, 0), s(RET_A, 0)], &d), 5u32.wrapping_neg());
    assert_eq!(run(&[s(LD_IMM, 0xffff_ffff), s(ADD_K, 2), s(RET_A, 0)], &d), 1);
    assert_eq!(run(&[s(LD_IMM, 0x1_0001), s(TAX, 0), s(MUL_X, 0), s(RET_A, 0)], &d), 0x2_0001);
}

#[test]
fn opcodes_outside_the_list() {
    for code in [0xffffu16, 0x0e, 0x0d, 0x55, 0x08, 0x100, 0x0f, 0x26] {
        assert_eq!(load(&[s(code, 0), s(RET_K, 0)]).err(), Some(Refused::Opcode(0)), "code {code:#x}");
    }
    assert_eq!(load(&[s(LD_IMM, 0)]).err(), Some(Refused::NoReturn));
}

#[test]
fn scratch_cells() {
    assert_eq!(load(&[s(LD_MEM, 0), s(RET_A, 0)]).err(), Some(Refused::UnsetCell(0)));
    assert_eq!(load(&[s(ST, 16), s(RET_A, 0)]).err(), Some(Refused::Cell(0)));
    assert_eq!(load(&[s(LD_MEM, 16), s(RET_A, 0)]).err(), Some(Refused::Cell(0)));
    assert_eq!(run(&[s(LD_IMM, 9), s(ST, 3), s(LD_IMM, 0), s(LD_MEM, 3), s(RET_A, 0)], &data(0, [0; 6])), 9);
    // Written on one branch only: refused after the join.
    let one = [s(LD_W_ABS, 0), j(JEQ_K, 1, 0, 1), s(ST, 2), s(LD_MEM, 2), s(RET_A, 0)];
    assert_eq!(load(&one).err(), Some(Refused::UnsetCell(3)));
    // Written before the branch: fine on both paths.
    let both = [s(ST, 2), s(LD_W_ABS, 0), j(JEQ_K, 1, 0, 1), s(LD_IMM, 0), s(LD_MEM, 2), s(RET_A, 0)];
    assert!(load(&both).is_ok());
    // A JA over the store does not count it.
    let skip = [s(JA, 1), s(ST, 1), s(LD_MEM, 1), s(RET_A, 0)];
    assert_eq!(load(&skip).err(), Some(Refused::UnsetCell(2)));
}

#[test]
fn data_layout() {
    let d = Data { nr: 39, arch: AUDIT_ARCH_X86_64, ip: 0x1122_3344_5566_7788, args: [0xaaaa_bbbb_cccc_dddd, 1, 2, 3, 4, 0xffff_0000_0000_0001] };
    let w = d.words();
    assert_eq!(w[0], 39);
    assert_eq!(w[1], 0xC000_003E);
    assert_eq!((w[2], w[3]), (0x5566_7788, 0x1122_3344));
    assert_eq!((w[4], w[5]), (0xcccc_dddd, 0xaaaa_bbbb));
    assert_eq!((w[14], w[15]), (1, 0xffff_0000));
    assert_eq!(run(&[s(LD_W_ABS, 60), s(RET_A, 0)], &d), 0xffff_0000);
}

#[test]
fn a_policy_and_x32() {
    let p = policy(&[(39, RET_ERRNO | 1), (110, RET_TRAP | 7)]);
    assert_eq!(run(&p, &data(39, [0; 6])), RET_ERRNO | 1);
    assert_eq!(run(&p, &data(110, [0; 6])), RET_TRAP | 7);
    assert_eq!(run(&p, &data(0, [0; 6])), RET_ALLOW);
    // An x32 number is a different number to the filter.
    assert_eq!(run(&p, &data(39 | X32_SYSCALL_BIT, [0; 6])), RET_ALLOW);
    let i386 = Data { arch: 0x4000_0003, ..data(39, [0; 6]) };
    assert_eq!(run(&p, &i386), RET_KILL_PROCESS);
}

fn filter(p: &[Insn], prev: Option<Arc<Filter>>) -> Arc<Filter> {
    Arc::new(Filter::new(load(p).expect("valid"), false, prev))
}

#[test]
fn precedence() {
    let f1 = filter(&policy(&[(39, RET_ERRNO | 1), (63, RET_ERRNO | 5), (102, RET_LOG)]), None);
    let f2 = filter(&policy(&[(39, RET_ERRNO | 13), (63, RET_ALLOW), (102, RET_TRAP | 2)]), Some(f1.clone()));
    let f3 = filter(&policy(&[(39, RET_KILL_PROCESS), (104, RET_ALLOW | 9)]), Some(f2.clone()));
    // Equal actions: the newest filter's data.
    assert_eq!(f2.run(&data(39, [0; 6])).0, RET_ERRNO | 13);
    // ERRNO beats ALLOW whatever the order.
    assert_eq!(f2.run(&data(63, [0; 6])).0, RET_ERRNO | 5);
    // TRAP beats LOG.
    assert_eq!(f2.run(&data(102, [0; 6])).0, RET_TRAP | 2);
    // KILL_PROCESS (negative) beats everything.
    assert_eq!(f3.run(&data(39, [0; 6])).0, RET_KILL_PROCESS);
    // ALLOW with data is ALLOW: the starting value stays.
    assert_eq!(f3.run(&data(104, [0; 6])).0, RET_ALLOW);
    assert_eq!((f1.depth(), f2.depth(), f3.depth()), (1, 2, 3));
    assert!(f3.has(&f1) && !f1.has(&f3));
    let order = [RET_KILL_PROCESS, RET_KILL_THREAD, RET_TRAP, RET_ERRNO, RET_USER_NOTIF, RET_TRACE, RET_LOG, RET_ALLOW];
    for w in order.windows(2) {
        assert!(crate::task::wins(w[0], w[1]), "{:#x} before {:#x}", w[0], w[1]);
    }
}

#[test]
fn cache_matches_a_full_run() {
    // Calls 0..20 decided by number only; 39 by its first argument; 60 through arithmetic on nr.
    let mut p = vec![s(LD_W_ABS, 4), j(JEQ_K, AUDIT_ARCH_X86_64, 1, 0), s(RET_K, RET_KILL_PROCESS), s(LD_W_ABS, 0)];
    p.extend([j(JGE_K, 20, 1, 0), s(RET_K, RET_ALLOW)]);
    p.extend([j(JEQ_K, 39, 0, 4), s(LD_W_ABS, 16), j(JSET_K, 1, 0, 1), s(RET_K, RET_ERRNO | 1), s(RET_K, RET_ALLOW)]);
    p.extend([s(ADD_K, 0), j(JEQ_K, 60, 0, 1), s(RET_K, RET_ALLOW), s(RET_K, RET_ERRNO | 2)]);
    let f = filter(&p, None);
    assert!(f.cached_allow(&data(5, [0; 6])));
    assert!(!f.cached_allow(&data(39, [0; 6])), "depends on an argument");
    assert!(!f.cached_allow(&data(60, [0; 6])), "ADD is not in the emulation");
    assert!(!f.cached_allow(&data(100, [0; 6])), "not allowed at all");
    let prog = load(&p).unwrap();
    for nr in 0..(crate::filter::CACHE_NRS as u32 + 8) {
        for a0 in [0u64, 1, 3, u64::MAX] {
            let d = data(nr, [a0, 2, 3, 4, 5, 6]);
            let full = prog.run(&d.words());
            if f.cached_allow(&d) {
                assert_eq!(full, RET_ALLOW, "nr {nr} a0 {a0}: the cache allowed what the filter does not");
            }
            assert_eq!(f.run(&d).0, full, "nr {nr} a0 {a0}");
        }
    }
    // A second filter narrows the cache.
    let g = filter(&policy(&[(5, RET_ERRNO | 9)]), Some(f.clone()));
    assert!(!g.cached_allow(&data(5, [0; 6])) && g.cached_allow(&data(6, [0; 6])));
    assert_eq!(g.run(&data(5, [0; 6])).0, RET_ERRNO | 9);
}

#[test]
fn const_allow_reads_only_nr_and_arch() {
    let p = load(&[s(LD_W_ABS, 8), j(JEQ_K, 0, 0, 1), s(RET_K, RET_ALLOW), s(RET_K, RET_ALLOW)]).unwrap();
    assert!(!p.const_allow(1, AUDIT_ARCH_X86_64), "an ip load is not constant");
    let q = load(&[s(LD_W_ABS, 0), s(0x54, 0xff), j(JEQ_K, 3, 0, 1), s(RET_K, RET_ALLOW), s(RET_K, RET_ERRNO)]).unwrap();
    assert!(q.const_allow(0x103, AUDIT_ARCH_X86_64) && !q.const_allow(4, AUDIT_ARCH_X86_64));
    assert!(matches!(q.ops()[1], Op::AluK(crate::bpf::AluOp::And, 0xff)));
}

#[test]
fn runtime_lengths_as_linux_converts() {
    assert_eq!(insns(4096).runtime_len(), 4100);
    // The arch check: a negative constant takes a move first.
    let p = load(&[s(LD_W_ABS, 4), j(JEQ_K, AUDIT_ARCH_X86_64, 1, 0), s(RET_K, 0), s(LD_IMM, 0), s(DIV_X, 0), s(RET_A, 0)]).unwrap();
    assert_eq!(p.runtime_len(), 3 + 1 + 2 + 2 + 1 + 5 + 1);
    let q = load(&[j(JEQ_K, 1, 0, 1), j(JSET_K, 1, 0, 1), j(JGE_K, 1, 1, 1), s(RET_K, 0), s(RET_K, 1)]).unwrap();
    assert_eq!(q.runtime_len(), 3 + 1 + 2 + 2 + 2 + 2);
}

#[test]
fn deep_stack_drops_without_recursion() {
    let p = [s(RET_K, RET_ALLOW)];
    let mut top: Option<Arc<Filter>> = None;
    let mut n = 0;
    loop {
        let prog = load(&p).unwrap();
        if !Filter::fits(&prog, top.as_deref()) {
            break;
        }
        top = Some(Arc::new(Filter::new(prog, false, top)));
        n += 1;
    }
    assert_eq!(n, (MAX_INSNS_PER_PATH - 5) / 9 + 1, "RET K is 3 + 2 runtime instructions, + 4 for each filter below");
    drop(top);
}

fn insns(n: usize) -> Program {
    let mut p = vec![s(LD_IMM, 0); n - 1];
    p.push(s(RET_K, RET_ALLOW));
    load(&p).unwrap()
}

#[test]
fn task_modes_and_inheritance() {
    let t = TaskSeccomp::new();
    assert_eq!(t.mode(), MODE_DISABLED);
    assert_eq!(t.check(&data(1000, [0; 6])), Verdict::Allow { log: false });
    let g = change();
    t.set_strict(&g).unwrap();
    assert_eq!(t.check(&data(1, [0; 6])), Verdict::Allow { log: false });
    assert_eq!(t.check(&data(15, [0; 6])), Verdict::Allow { log: false });
    assert_eq!(t.check(&data(39, [0; 6])), Verdict::StrictKill);
    assert_eq!(t.attach(&g, insns(1), false).err(), Some(AttachError::Mode), "strict then filter");
    assert!(t.set_strict(&g).is_ok(), "strict again is fine");

    let f = TaskSeccomp::new();
    assert_eq!(f.attach(&g, load(&policy(&[(39, RET_ERRNO | 1)])).unwrap(), false), Ok(1));
    assert_eq!(f.set_strict(&g).err(), Some(AttachError::Mode), "filter then strict");
    assert_eq!(f.attach(&g, load(&policy(&[(40, RET_TRAP | 3), (41, RET_KILL_THREAD | 4), (42, 0x0001_0000), (43, RET_TRACE), (44, RET_USER_NOTIF), (45, RET_ERRNO | 0xffff)])).unwrap(), true), Ok(2));
    assert_eq!(f.mode(), MODE_FILTER);
    assert_eq!(f.filter_count(), 2);
    assert_eq!(f.check(&data(39, [0; 6])), Verdict::Errno { errno: 1, action: RET_ERRNO, log: false });
    assert_eq!(f.check(&data(40, [0; 6])), Verdict::Trap { data: 3, log: true });
    assert_eq!(f.check(&data(41, [0; 6])), Verdict::KillThread { data: 4 });
    assert_eq!(f.check(&data(42, [0; 6])), Verdict::KillProcess { data: 0 }, "an unknown action kills the process");
    assert_eq!(f.check(&data(43, [0; 6])), Verdict::Errno { errno: 38, action: RET_TRACE, log: true });
    assert_eq!(f.check(&data(44, [0; 6])), Verdict::Errno { errno: 38, action: RET_USER_NOTIF, log: true });
    assert_eq!(f.check(&data(45, [0; 6])), Verdict::Errno { errno: MAX_ERRNO, action: RET_ERRNO, log: true });

    let child = TaskSeccomp::new();
    child.inherit(&f);
    assert_eq!((child.mode(), child.filter_count()), (MODE_FILTER, 2));
    assert_eq!(child.check(&data(39, [0; 6])), f.check(&data(39, [0; 6])));
    // The child's own filter does not reach the parent.
    child.attach(&g, insns(1), false).unwrap();
    assert_eq!((child.filter_count(), f.filter_count()), (3, 2));
    f.set_dead();
    assert_eq!(f.check(&data(0, [0; 6])), Verdict::StrictKill);
}

#[test]
fn tsync_rules() {
    let g = change();
    let caller = TaskSeccomp::new();
    let plain = TaskSeccomp::new();
    let strict = TaskSeccomp::new();
    strict.set_strict(&g).unwrap();
    caller.attach(&g, insns(2), false).unwrap();
    let older = TaskSeccomp::new();
    older.inherit(&caller);
    let own = TaskSeccomp::new();
    own.inherit(&caller);
    own.attach(&g, insns(3), false).unwrap();
    caller.attach(&g, insns(4), false).unwrap();
    assert!(plain.can_sync_from(&caller), "no mode");
    assert!(older.can_sync_from(&caller), "its stack is below the caller's");
    assert!(!own.can_sync_from(&caller), "a filter of its own");
    assert!(!strict.can_sync_from(&caller), "strict mode");
    assert!(!older.synced_with(&caller));
    older.sync_from(&g, &caller);
    plain.sync_from(&g, &caller);
    assert!(older.synced_with(&caller) && plain.synced_with(&caller));
    assert_eq!((plain.mode(), plain.filter_count()), (MODE_FILTER, 2));
}

// The clone/TSYNC race (fixer, CR1): a thread that inherited before its creator was synced takes the
// synced stack when it enters the thread list; a second resync changes nothing.
#[test]
fn resync_after_tsync() {
    let g = change();
    let caller = TaskSeccomp::new();
    let creator = TaskSeccomp::new();
    let fresh = TaskSeccomp::new();
    fresh.inherit(&creator);
    assert!(!fresh.resync(&creator), "nothing to take");
    caller.attach(&g, insns(2), false).unwrap();
    creator.sync_from(&g, &caller);
    assert_eq!(fresh.mode(), MODE_DISABLED, "inherited before the sync");
    assert!(fresh.resync(&creator));
    assert!(fresh.synced_with(&caller));
    assert_eq!((fresh.mode(), fresh.filter_count()), (MODE_FILTER, 1));
    assert!(!fresh.resync(&creator), "the same stack again");
}

#[test]
fn path_bound() {
    let g = change();
    let t = TaskSeccomp::new();
    let mut n = 0;
    loop {
        match t.attach(&g, insns(MAX_INSNS), false) {
            Ok(_) => n += 1,
            Err(e) => {
                assert_eq!(e, AttachError::TooLong);
                break;
            }
        }
    }
    assert_eq!(n, 7, "7 x (4100 + 4) runtime instructions fit in 32768, the 8th does not");
    // 32768 - 7 x 4104 = 4040 runtime instructions: a program of n instructions is n + 4.
    assert!(t.can_attach(&g, &insns(4036)).is_ok() && t.can_attach(&g, &insns(4037)).is_err());
}
