//! Classic BPF for seccomp: the verifier and the interpreter.
//!
//! The verifier applies Linux's two passes in one walk (linux-6.6/net/core/filter.c `bpf_check_classic` and
//! `check_load_and_stores`, kernel/seccomp.c `seccomp_check_filter`): only the opcodes seccomp allows, jumps
//! forward and inside the program, no division by a constant zero, shifts by a constant below 32, scratch cells
//! 0..16 and read only after a write on every path, 32-bit loads of `struct seccomp_data` (aligned, inside its
//! 64 bytes), and a return at the end. Every refusal is EINVAL, as in Linux.
//!
//! The interpreter runs the checked program over the 16 words of `seccomp_data` with Linux's arithmetic
//! (32-bit, wrapping; a division by X = 0 ends the program with 0, a shift by X uses its low 5 bits).

use alloc::vec::Vec;

// Opcode parts (bpf_common.h).
const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;
const W: u16 = 0x00;
const IMM: u16 = 0x00;
const ABS: u16 = 0x20;
const MEM: u16 = 0x60;
const LEN: u16 = 0x80;
const K: u16 = 0x00;
const X: u16 = 0x08;
const RET_A: u16 = 0x10;
const TAX: u16 = 0x00;
const TXA: u16 = 0x80;
/// `BPF_MEMWORDS`.
const MEMWORDS: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    Mul,
    Div,
    Or,
    And,
    Lsh,
    Rsh,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Gt,
    Ge,
    Set,
}

/// One checked instruction, decoded once at load time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// A = the 32-bit word `n` of seccomp_data (byte offset 4n).
    LdData(u8),
    LdImm(u32),
    LdxImm(u32),
    LdMem(u8),
    LdxMem(u8),
    St(u8),
    Stx(u8),
    AluK(AluOp, u32),
    AluX(AluOp),
    Neg,
    Tax,
    Txa,
    Ja(u32),
    JmpK(Cond, u32, u8, u8),
    JmpX(Cond, u8, u8),
    RetK(u32),
    RetA,
}

/// A checked program.
#[derive(Debug)]
pub struct Program {
    ops: Vec<Op>,
}

/// `struct sock_filter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

impl Insn {
    pub const fn new(code: u16, jt: u8, jf: u8, k: u32) -> Insn {
        Insn { code, jt, jf, k }
    }
    /// From the 8 bytes of a user `sock_filter` (little endian).
    pub fn from_bytes(b: &[u8; 8]) -> Insn {
        Insn { code: u16::from_le_bytes([b[0], b[1]]), jt: b[2], jf: b[3], k: u32::from_le_bytes([b[4], b[5], b[6], b[7]]) }
    }
}

fn alu_op(code: u16) -> Option<AluOp> {
    Some(match code & 0xf0 {
        0x00 => AluOp::Add,
        0x10 => AluOp::Sub,
        0x20 => AluOp::Mul,
        0x30 => AluOp::Div,
        0x40 => AluOp::Or,
        0x50 => AluOp::And,
        0x60 => AluOp::Lsh,
        0x70 => AluOp::Rsh,
        0xa0 => AluOp::Xor,
        // MOD (0x90) passes bpf_check_classic but seccomp_check_filter refuses it; NEG is handled apart.
        _ => return None,
    })
}

fn cond(code: u16) -> Option<Cond> {
    Some(match code & 0xf0 {
        0x10 => Cond::Eq,
        0x20 => Cond::Gt,
        0x30 => Cond::Ge,
        0x40 => Cond::Set,
        _ => return None,
    })
}

/// Why a program was refused (all EINVAL towards the program; the reason goes to the log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Length,
    Opcode(usize),
    DivByZero(usize),
    Shift(usize),
    Cell(usize),
    Jump(usize),
    Load(usize),
    NoReturn,
    UnsetCell(usize),
}

impl Refused {
    pub fn what(&self) -> &'static str {
        match self {
            Refused::Length => "length",
            Refused::Opcode(_) => "opcode",
            Refused::DivByZero(_) => "division by zero",
            Refused::Shift(_) => "shift >= 32",
            Refused::Cell(_) => "scratch cell >= 16",
            Refused::Jump(_) => "jump out of the program",
            Refused::Load(_) => "load outside seccomp_data",
            Refused::NoReturn => "last instruction is no return",
            Refused::UnsetCell(_) => "scratch cell read before a write",
        }
    }
    pub fn at(&self) -> Option<usize> {
        match *self {
            Refused::Opcode(p) | Refused::DivByZero(p) | Refused::Shift(p) | Refused::Cell(p) | Refused::Jump(p) | Refused::Load(p) | Refused::UnsetCell(p) => Some(p),
            Refused::Length | Refused::NoReturn => None,
        }
    }
}

/// Decodes and checks one instruction at `pc` of a program of `len`.
fn decode(i: Insn, pc: usize, len: usize) -> Result<Op, Refused> {
    let k = i.k;
    let cell = |op: fn(u8) -> Op| if k < MEMWORDS { Ok(op(k as u8)) } else { Err(Refused::Cell(pc)) };
    let target_ok = |off: usize| pc + off + 1 < len;
    let op = match i.code {
        c if c == LD | W | ABS => {
            if k >= crate::consts::DATA_SIZE || k & 3 != 0 {
                return Err(Refused::Load(pc));
            }
            Op::LdData((k / 4) as u8)
        }
        // `seccomp_check_filter` turns the length loads into immediates of sizeof(seccomp_data).
        c if c == LD | W | LEN => Op::LdImm(crate::consts::DATA_SIZE),
        c if c == LDX | W | LEN => Op::LdxImm(crate::consts::DATA_SIZE),
        c if c == LD | IMM => Op::LdImm(k),
        c if c == LDX | IMM => Op::LdxImm(k),
        c if c == LD | MEM => cell(Op::LdMem)?,
        c if c == LDX | MEM => cell(Op::LdxMem)?,
        c if c == ST => cell(Op::St)?,
        c if c == STX => cell(Op::Stx)?,
        c if c == ALU | 0x80 => Op::Neg,
        c if c & 0x07 == ALU && c & !0xff == 0 => {
            let op = alu_op(c).ok_or(Refused::Opcode(pc))?;
            if c & X != 0 {
                if c & !0xf8 != ALU {
                    return Err(Refused::Opcode(pc));
                }
                Op::AluX(op)
            } else {
                if c & !0xf0 != ALU {
                    return Err(Refused::Opcode(pc));
                }
                if op == AluOp::Div && k == 0 {
                    return Err(Refused::DivByZero(pc));
                }
                if matches!(op, AluOp::Lsh | AluOp::Rsh) && k >= 32 {
                    return Err(Refused::Shift(pc));
                }
                Op::AluK(op, k)
            }
        }
        c if c == JMP => {
            // `k >= flen - pc - 1` is refused: the target pc + 1 + k must lie inside the program.
            if (k as usize) >= len - pc - 1 {
                return Err(Refused::Jump(pc));
            }
            Op::Ja(k)
        }
        c if c & 0x07 == JMP && c & !0xf8 == JMP => {
            let cd = cond(c).ok_or(Refused::Opcode(pc))?;
            if !target_ok(i.jt as usize) || !target_ok(i.jf as usize) {
                return Err(Refused::Jump(pc));
            }
            if c & X != 0 { Op::JmpX(cd, i.jt, i.jf) } else { Op::JmpK(cd, k, i.jt, i.jf) }
        }
        c if c == RET | K => Op::RetK(k),
        c if c == RET | RET_A => Op::RetA,
        c if c == MISC | TAX => Op::Tax,
        c if c == MISC | TXA => Op::Txa,
        _ => return Err(Refused::Opcode(pc)),
    };
    Ok(op)
}

impl Program {
    /// Checks a program as `bpf_check_classic` + `seccomp_check_filter` do. The length bound (1..=4096) is the
    /// caller's first check (it comes before the privilege check in Linux); it is repeated here.
    pub fn load(insns: &[Insn]) -> Result<Program, Refused> {
        let len = insns.len();
        if len == 0 || len > crate::consts::MAX_INSNS {
            return Err(Refused::Length);
        }
        let mut ops = Vec::new();
        ops.try_reserve_exact(len).map_err(|_| Refused::Length)?;
        for (pc, i) in insns.iter().enumerate() {
            ops.push(decode(*i, pc, len)?);
        }
        if !matches!(ops[len - 1], Op::RetK(_) | Op::RetA) {
            return Err(Refused::NoReturn);
        }
        check_cells(&ops)?;
        Ok(Program { ops })
    }
    pub fn len(&self) -> usize {
        self.ops.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// The length Linux's stack bound counts: the program as `bpf_convert_filter` (linux-6.6/net/core/filter.c)
    /// rewrites it for its runtime. Three prologue instructions; RET K takes two; a division by X takes five
    /// (the zero check); a conditional jump takes one more when its constant is negative as a signed number,
    /// and two instead of one unless its false branch is the next instruction (or its true branch is, for
    /// JEQ/JGT/JGE, which turn into JNE/JLE/JLT). `seccomp_attach_filter` checks this length, so the same stacks
    /// fit as on Linux.
    pub fn runtime_len(&self) -> usize {
        3 + self
            .ops
            .iter()
            .map(|op| match *op {
                Op::RetK(_) => 2,
                Op::AluX(AluOp::Div) => 5,
                Op::JmpK(c, k, jt, jf) => ((k as i32) < 0) as usize + jump_len(c, jt, jf),
                Op::JmpX(c, jt, jf) => jump_len(c, jt, jf),
                _ => 1,
            })
            .sum::<usize>()
    }

    /// Runs the program over `data` (the 16 words of seccomp_data) and returns its value.
    pub fn run(&self, data: &[u32; 16]) -> u32 {
        let (mut a, mut x) = (0u32, 0u32);
        let mut m = [0u32; MEMWORDS as usize];
        let mut pc = 0usize;
        loop {
            // A checked program ends in a return and jumps only forward inside itself; running off the end
            // anyway fails closed.
            let Some(&op) = self.ops.get(pc) else { return crate::consts::RET_KILL_PROCESS };
            pc += 1;
            match op {
                Op::LdData(n) => a = data[(n & 15) as usize],
                Op::LdImm(k) => a = k,
                Op::LdxImm(k) => x = k,
                Op::LdMem(n) => a = m[(n & 15) as usize],
                Op::LdxMem(n) => x = m[(n & 15) as usize],
                Op::St(n) => m[(n & 15) as usize] = a,
                Op::Stx(n) => m[(n & 15) as usize] = x,
                Op::AluK(o, k) => a = alu(o, a, k),
                Op::AluX(o) => {
                    if o == AluOp::Div && x == 0 {
                        return 0;
                    }
                    a = alu(o, a, x);
                }
                Op::Neg => a = a.wrapping_neg(),
                Op::Tax => x = a,
                Op::Txa => a = x,
                Op::Ja(k) => pc += k as usize,
                Op::JmpK(c, k, jt, jf) => pc += if test(c, a, k) { jt } else { jf } as usize,
                Op::JmpX(c, jt, jf) => pc += if test(c, a, x) { jt } else { jf } as usize,
                Op::RetK(k) => return k,
                Op::RetA => return a,
            }
        }
    }

    /// Whether the program allows call `nr` of `arch` whatever its arguments and address (Linux's
    /// `seccomp_is_const_allow`): it reaches `RET ALLOW` reading only nr and arch, through AND with a constant
    /// and constant jumps. Anything else counts as "depends on more" (the action cache then runs the filter).
    pub fn const_allow(&self, nr: u32, arch: u32) -> bool {
        let mut a = 0u32;
        let mut pc = 0usize;
        while let Some(&op) = self.ops.get(pc) {
            pc += 1;
            match op {
                Op::LdData(0) => a = nr,
                Op::LdData(1) => a = arch,
                Op::RetK(k) => return k == crate::consts::RET_ALLOW,
                Op::Ja(k) => pc += k as usize,
                Op::JmpK(c, k, jt, jf) => pc += if test(c, a, k) { jt } else { jf } as usize,
                Op::AluK(AluOp::And, k) => a &= k,
                _ => return false,
            }
        }
        false
    }
}

fn jump_len(c: Cond, jt: u8, jf: u8) -> usize {
    if jf == 0 || (jt == 0 && c != Cond::Set) { 1 } else { 2 }
}

fn alu(o: AluOp, a: u32, v: u32) -> u32 {
    match o {
        AluOp::Add => a.wrapping_add(v),
        AluOp::Sub => a.wrapping_sub(v),
        AluOp::Mul => a.wrapping_mul(v),
        // A constant divisor was checked non-zero; X = 0 ended the program before.
        AluOp::Div => a.checked_div(v).unwrap_or(0),
        AluOp::Or => a | v,
        AluOp::And => a & v,
        AluOp::Lsh => a << (v & 31),
        AluOp::Rsh => a >> (v & 31),
        AluOp::Xor => a ^ v,
    }
}

fn test(c: Cond, a: u32, v: u32) -> bool {
    match c {
        Cond::Eq => a == v,
        Cond::Gt => a > v,
        Cond::Ge => a >= v,
        Cond::Set => a & v != 0,
    }
}

/// `check_load_and_stores`: a scratch cell is read only where every path to it wrote the cell. The bits known
/// written flow forward; a jump target keeps only what every jump to it knows; after a jump the next
/// instruction is reached only by jumps.
fn check_cells(ops: &[Op]) -> Result<(), Refused> {
    let mut masks: Vec<u16> = Vec::new();
    masks.try_reserve_exact(ops.len()).map_err(|_| Refused::Length)?;
    masks.resize(ops.len(), 0xffff);
    let mut valid: u16 = 0;
    for (pc, op) in ops.iter().enumerate() {
        valid &= masks[pc];
        match *op {
            Op::St(n) | Op::Stx(n) => valid |= 1 << n,
            Op::LdMem(n) | Op::LdxMem(n) => {
                if valid & (1 << n) == 0 {
                    return Err(Refused::UnsetCell(pc));
                }
            }
            Op::Ja(k) => {
                masks[pc + 1 + k as usize] &= valid;
                valid = 0xffff;
            }
            Op::JmpK(_, _, jt, jf) | Op::JmpX(_, jt, jf) => {
                masks[pc + 1 + jt as usize] &= valid;
                masks[pc + 1 + jf as usize] &= valid;
                valid = 0xffff;
            }
            _ => {}
        }
    }
    Ok(())
}
