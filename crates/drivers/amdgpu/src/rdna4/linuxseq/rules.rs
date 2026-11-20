//! GX-10: the declared differences between a stage and Linux's trace. A stage may differ from a section only where
//! a rule names the register, and each rule says why and carries its own check, so a declared value is still a
//! well-formed one. A rule that no compared line used is an error (`compare::Compare::stop`): rules cannot rot.

use super::compare::Kind;
use super::fold::Did;
use alloc::format;
use alloc::string::String;
use core::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// The register (or the low and high register together) holds an address BitsOS chooses for itself.
    Address(Address),
    /// The stage flushes the HDP once right before each write of `reg`, where Linux's trace has no flush (a buffer
    /// Linux kept in system memory and BitsOS keeps in VRAM, written through BAR0).
    HdpBefore { reg: &'static str, why: &'static str },
    /// The stage binds GART pages of its own right before the first write of `reg` and flushes the TLB for them
    /// exactly as Linux does after a bind (`linuxseq::bind_flush`, cut from `sw-init`, where Linux bound its own
    /// pages earlier). Every line of that flush is compared like any other.
    BindBefore { reg: &'static str, why: &'static str },
    /// The stage reads the register at the byte address `byte`, past BAR5 and so through the index pair, right
    /// before the section's first line (a write of `first`), where Linux's trace shows nothing: Linux makes that
    /// read too, through a path that has no tracepoint. The read must be there, once, and nothing else.
    Untraced { first: &'static str, byte: u32, why: &'static str },
    /// An address BitsOS chooses for itself, handed over in a mailbox's argument register.
    Argument(Argument),
    /// As `BindBefore`, for a bind made once the graphics block's hub is on: the flush then walks both hubs,
    /// GFXHUB first (`linuxseq::bind_flush_both`, twelve lines). The register form is BitsOS's own for the time
    /// before MES runs and equals no part of Linux's start: Linux binds nothing between GFXHUB's start and MES,
    /// and at its next bind (section sdma, after mes) it flushes through MES
    /// (`amdgpu_gmc_fw_reg_write_reg_wait`, gmc_v12_0.c:315-327). Since wave 44 it holds for MES's own bind
    /// (`messeq`), the last bind of a boot before MES runs; every bind after it is `BindMesBefore`.
    BindBothBefore { reg: &'static str, why: &'static str },
    /// As `BindBefore`, for a bind made once MES runs (wave 44, `Session::flush_regs` with MES ready): Linux's
    /// flush is then an HDP flush of its own and per hub an HDP flush and a MES command
    /// (`gmc_v12_0_flush_gpu_tlb`, gmc_v12_0.c:305-330 with `amdgpu_gmc_fw_reg_write_reg_wait`), three HDP flushes
    /// on the bus and nothing else (`linuxseq::bind_flush_mes`; the trace-load holds them so after every bind).
    BindMesBefore { reg: &'static str, why: &'static str },
    /// Linux polls two registers together in a loop of its own, which is no single register's wait: the trace
    /// shows the round that ended it as two plain reads, and a stage may need more rounds than Linux did.
    PairPoll(PairPoll),
}

/// A 64-bit address in two messages: the write of `arg` right before the message `hi` (the next write of `msg`
/// in Linux's section) carries its upper half, the one before `lo` its lower half. `arg` is written with other
/// things under every other message, so the rule speaks only for these two writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argument {
    pub arg: &'static str,
    pub msg: &'static str,
    pub hi: u32,
    pub lo: u32,
    /// The address BitsOS's VRAM manager handed out for what the two messages name, from the manager's own
    /// record: the two halves must name exactly this (two tables that changed places, or another free page,
    /// are well-formed and still not what was taken).
    pub at: u64,
    pub align: u64,
    /// Where the address and the `bytes` behind it must lie.
    pub within: Range<u64>,
    pub bytes: u64,
    pub why: &'static str,
}

impl Argument {
    /// The address the stage's two arguments name, or why it is not the one this rule declares.
    pub fn check(&self, hi: u32, lo: u32) -> Result<u64, String> {
        let addr = u64::from(hi) << 32 | u64::from(lo);
        if self.align == 0 || !addr.is_multiple_of(self.align) {
            return Err(format!("the address {addr:#x} is not aligned to {:#x}", self.align));
        }
        match addr.checked_add(self.bytes) {
            Some(end) if addr >= self.within.start && end <= self.within.end => {}
            _ => return Err(format!("the address {addr:#x} with its {:#x} bytes is not inside {:#x}..{:#x}", self.bytes, self.within.start, self.within.end)),
        }
        if addr != self.at {
            return Err(format!("the address {addr:#x} is not the one BitsOS's VRAM manager handed out for it, {:#x}", self.at));
        }
        Ok(addr)
    }
}

/// A loop over two registers: each round reads `first`, then `second`, until both show what ends the loop or
/// `max` rounds passed. The rounds before the last take no line of the fixture, and each must be exactly that
/// pair with an answer that does not end the loop; the last round is held against Linux's two lines like any
/// other read, answers included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairPoll {
    pub first: &'static str,
    pub second: &'static str,
    /// (mask, value under it) that ends the loop, per register.
    pub first_is: (u32, u32),
    pub second_is: (u32, u32),
    /// The rounds after which Linux gives up.
    pub max: u32,
    pub why: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub lo: &'static str,
    pub hi: Option<&'static str>,
    /// The address is the registers' number shifted left by this (a page number: 12; a MiB number: 20).
    pub shift: u8,
    /// Bits of the low register that are no address (a valid bit): they must be Linux's.
    pub keep: u32,
    pub align: u64,
    /// Where the address and the `bytes` behind it must lie, in the address space the register speaks (an MC
    /// range, or offsets into VRAM).
    pub within: Range<u64>,
    pub bytes: u64,
    pub why: &'static str,
}

impl Rule {
    pub fn why(&self) -> &'static str {
        match self {
            Rule::Address(a) => a.why,
            Rule::Argument(a) => a.why,
            Rule::HdpBefore { why, .. } | Rule::BindBefore { why, .. } | Rule::BindBothBefore { why, .. } | Rule::BindMesBefore { why, .. } | Rule::Untraced { why, .. } => why,
            Rule::PairPoll(p) => p.why,
        }
    }

    /// The register names this rule speaks for.
    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        let (a, b) = match self {
            Rule::Address(a) => (a.lo, a.hi),
            Rule::Argument(a) => (a.arg, Some(a.msg)),
            Rule::HdpBefore { reg, .. } | Rule::BindBefore { reg, .. } | Rule::BindBothBefore { reg, .. } | Rule::BindMesBefore { reg, .. } | Rule::Untraced { first: reg, .. } => (*reg, None),
            Rule::PairPoll(p) => (p.first, Some(p.second)),
        };
        core::iter::once(a).chain(b)
    }

    /// Two rules that would speak for the same writes: the same kind for one register; two argument rules only
    /// when they share a message.
    pub fn clashes(&self, other: &Rule) -> bool {
        match (self, other) {
            (Rule::Argument(a), Rule::Argument(b)) => a.arg == b.arg && a.msg == b.msg && [a.hi, a.lo].iter().any(|m| *m == b.hi || *m == b.lo),
            _ => core::mem::discriminant(self) == core::mem::discriminant(other) && self.names().any(|n| other.names().any(|x| x == n)),
        }
    }
}

impl Address {
    /// The address the stage's values name, or why they are not a well-formed one. `linux_lo` is what Linux
    /// wrote into the low register.
    pub fn check(&self, lo: u32, hi: u32, linux_lo: u32) -> Result<u64, String> {
        if lo & self.keep != linux_lo & self.keep {
            return Err(format!("the bits {:#x} of {} are {:#x}, Linux's are {:#x}", self.keep, self.lo, lo & self.keep, linux_lo & self.keep));
        }
        let number = u128::from(hi) << 32 | u128::from(lo & !self.keep);
        let Ok(addr) = u64::try_from(number << self.shift) else { return Err(format!("{number:#x} shifted by {} is no 64-bit address", self.shift)) };
        if self.align == 0 || !addr.is_multiple_of(self.align) {
            return Err(format!("the address {addr:#x} is not aligned to {:#x}", self.align));
        }
        match addr.checked_add(self.bytes) {
            Some(end) if addr >= self.within.start && end <= self.within.end => Ok(addr),
            _ => Err(format!("the address {addr:#x} with its {:#x} bytes is not inside {:#x}..{:#x}", self.bytes, self.within.start, self.within.end)),
        }
    }
}

impl PairPoll {
    /// Both registers show what ends Linux's loop.
    pub fn met(&self, first: u32, second: u32) -> bool {
        first & self.first_is.0 == self.first_is.1 && second & self.second_is.0 == self.second_is.1
    }

    /// How many of the accesses at the head of `did` are rounds before the loop's last one (they take no line),
    /// or how the stage's loop is not Linux's. `d0` and `d1` are the two registers as dwords.
    pub fn rounds(&self, d0: u32, d1: u32, did: &[Did]) -> Result<usize, (Kind, String)> {
        let round = |n: usize| match (did.get(2 * n), did.get(2 * n + 1)) {
            (Some(&Did::Read { dword: a, value: x }), Some(&Did::Read { dword: b, value: y })) if a == d0 && b == d1 => Some((x, y)),
            _ => None,
        };
        let until = format!("Linux reads {} and {} together until (value & {:#x}) == {:#x} and (value & {:#x}) == {:#x}", self.first, self.second, self.first_is.0, self.first_is.1, self.second_is.0, self.second_is.1);
        let mut n = 0;
        // A round with another round behind it is not the last.
        while let (Some((Some(x), Some(y))), Some(_)) = (round(n), round(n + 1)) {
            if self.met(x, y) {
                return Err((Kind::Condition, format!("{until}; the stage reads the pair again after round {} answered {x:#010x} and {y:#010x}, which ends Linux's loop", n + 1)));
            }
            n += 1;
        }
        if n as u64 >= u64::from(self.max) {
            return Err((Kind::Bound, format!("{until} and gives up after {} rounds; the stage made more", self.max)));
        }
        if let Some((Some(x), Some(y))) = round(n)
            && !self.met(x, y)
            && did.len() > 2 * n + 2
        {
            return Err((Kind::Condition, format!("{until}; the stage goes on after round {} answered {x:#010x} and {y:#010x}, which does not end Linux's loop", n + 1)));
        }
        Ok(2 * n)
    }
}
