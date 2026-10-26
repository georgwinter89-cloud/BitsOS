//! User page faults (pure part): what the #PF error code asks for and whether the VMA allows it. The address
//! space fills the page when it does; otherwise the reason goes into the V2 fault message (R3: program,
//! thread, address, reason).

use crate::vma::Prot;

pub const PF_PRESENT: u64 = 1 << 0;
pub const PF_WRITE: u64 = 1 << 1;
pub const PF_USER: u64 = 1 << 2;
pub const PF_RSVD: u64 = 1 << 3;
pub const PF_FETCH: u64 = 1 << 4;
pub const PF_PK: u64 = 1 << 5;
pub const PF_SS: u64 = 1 << 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Fetch,
}

impl Access {
    pub fn from_error(error: u64) -> Access {
        if error & PF_FETCH != 0 {
            Access::Fetch
        } else if error & PF_WRITE != 0 {
            Access::Write
        } else {
            Access::Read
        }
    }
    pub fn allowed_by(self, p: Prot) -> bool {
        match self {
            Access::Read => p.accessible(),
            Access::Write => p.write,
            Access::Fetch => p.exec,
        }
    }
}

/// Why a fault ends the program (the text goes into the V2 fault message).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultReason {
    /// No mapping at the address.
    NotMapped,
    /// Mapped with PROT_NONE (a guard page or a reservation).
    ProtNone,
    ReadOnly,
    NoExec,
    /// The address is not in user space.
    KernelAddress,
    /// A reserved bit, protection key or shadow-stack fault: never set by MM, so a kernel bug or a CPU feature
    /// BitsOS does not use yet.
    Unexpected,
    /// No frame for the page (no commit accounting before MM-5b; then this cannot happen, R2).
    NoMemory,
    /// The same thread faults on the same valid page over and over, a local flush each time (a kernel bug;
    /// ending the program beats a loop; `SpuriousRun`).
    Repeated,
    /// A file mapping's page at or past the end of the file, or one its file system could not read (FS-5;
    /// Linux sends SIGBUS).
    Bus,
}

impl FaultReason {
    pub fn text(self) -> &'static str {
        match self {
            FaultReason::NotMapped => "not mapped",
            FaultReason::ProtNone => "PROT_NONE",
            FaultReason::ReadOnly => "write to read-only memory",
            FaultReason::NoExec => "execute of non-executable memory",
            FaultReason::KernelAddress => "kernel address",
            FaultReason::Unexpected => "reserved bit or protection key",
            FaultReason::NoMemory => "out of memory",
            FaultReason::Repeated => "fault repeats on a fixed page",
            FaultReason::Bus => "beyond the end of the mapped file",
        }
    }
}

/// Whether a fault with `error` at a user address inside a VMA with `prot` (`None`: outside every VMA) may
/// be served by filling or re-syncing the page.
pub fn check(error: u64, prot: Option<Prot>) -> Result<Access, FaultReason> {
    if error & (PF_RSVD | PF_PK | PF_SS) != 0 {
        return Err(FaultReason::Unexpected);
    }
    let prot = prot.ok_or(FaultReason::NotMapped)?;
    let access = Access::from_error(error);
    if !prot.accessible() {
        return Err(FaultReason::ProtNone);
    }
    if !access.allowed_by(prot) {
        return Err(if access == Access::Write { FaultReason::ReadOnly } else { FaultReason::NoExec });
    }
    Ok(access)
}

/// Spurious faults in a row on one page that one thread may take before the fault ends the program.
pub const SPURIOUS_REPEATS: u32 = 16;
/// Two spurious faults further apart than this belong to different runs: a looping fault comes back within
/// microseconds, while a page another thread upgrades now and then may legitimately fault again much later.
pub const SPURIOUS_GAP_NS: u64 = 50_000_000;

/// One thread's run of spurious faults: faults on a page whose entry already allows the access. Either another
/// thread filled the page while this one waited for the address space's lock (several threads touching fresh
/// library text at once, the usual case), or this CPU still held a TLB entry from before an upgrade made on
/// another CPU (x86 may keep one; the fault handler drops it locally). Counted per thread, never per address
/// space: N threads racing for one page are N single spurious faults, not a run of N.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpuriousRun {
    root: u64,
    page: u64,
    n: u32,
    last_ns: u64,
}

impl SpuriousRun {
    /// Notes a spurious fault on `page` of the space `root` at `now_ns`; returns how many in a row this is, or
    /// `Err(Repeated)` once the run is longer than `SPURIOUS_REPEATS`.
    pub fn note(&mut self, root: u64, page: u64, now_ns: u64) -> Result<u32, FaultReason> {
        let same = self.n > 0 && self.root == root && self.page == page && now_ns.saturating_sub(self.last_ns) <= SPURIOUS_GAP_NS;
        let n = if same { self.n + 1 } else { 1 };
        *self = SpuriousRun { root, page, n, last_ns: now_ns };
        if n > SPURIOUS_REPEATS { Err(FaultReason::Repeated) } else { Ok(n) }
    }
    /// A fault that changed something (a fill, a copy, a dirty mark) ends the run.
    pub fn reset(&mut self) {
        *self = SpuriousRun::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RO: Prot = Prot { read: true, write: false, exec: false };
    const RX: Prot = Prot { read: true, write: false, exec: true };

    #[test]
    fn decisions() {
        let u = PF_USER;
        assert_eq!(check(u, None), Err(FaultReason::NotMapped));
        assert_eq!(check(u | PF_WRITE, Some(Prot::NONE)), Err(FaultReason::ProtNone));
        assert_eq!(check(u | PF_PRESENT | PF_WRITE, Some(RO)), Err(FaultReason::ReadOnly));
        assert_eq!(check(u | PF_FETCH, Some(Prot::rw())), Err(FaultReason::NoExec));
        assert_eq!(check(u | PF_RSVD | PF_PRESENT, Some(Prot::rw())), Err(FaultReason::Unexpected));
        assert_eq!(check(u, Some(RO)), Ok(Access::Read));
        assert_eq!(check(u | PF_WRITE, Some(Prot::rw())), Ok(Access::Write));
        assert_eq!(check(u | PF_FETCH, Some(RX)), Ok(Access::Fetch));
        let wo = Prot { read: false, write: true, exec: false };
        assert_eq!(check(u, Some(wo)), Ok(Access::Read), "write-only is readable on x86");
    }

    #[test]
    fn racing_threads_are_single_spurious_faults() {
        // ab.ab8.threads on the Mini-PC: 8 threads fault on one fresh text page; one fills, the rest find it
        // valid. Each thread has its own run, so none of them is a repeat.
        let mut runs = [SpuriousRun::default(); 16];
        for r in &mut runs {
            assert_eq!(r.note(0x1000, 0x7eff_0000, 5), Ok(1));
        }
    }

    #[test]
    fn a_loop_on_one_page_ends() {
        let mut r = SpuriousRun::default();
        for i in 1..=SPURIOUS_REPEATS {
            assert_eq!(r.note(0x1000, 0x5000, u64::from(i) * 1000), Ok(i));
        }
        assert_eq!(r.note(0x1000, 0x5000, 99_000), Err(FaultReason::Repeated));
    }

    #[test]
    fn runs_break_on_other_page_space_gap_or_reset() {
        let mut r = SpuriousRun::default();
        let t = 1_000_000;
        assert_eq!(r.note(1, 0x5000, t), Ok(1));
        assert_eq!(r.note(1, 0x5000, t + 1), Ok(2));
        assert_eq!(r.note(1, 0x6000, t + 2), Ok(1), "another page");
        assert_eq!(r.note(2, 0x6000, t + 3), Ok(1), "another space (after execve)");
        assert_eq!(r.note(2, 0x6000, t + 3 + SPURIOUS_GAP_NS), Ok(2), "within the gap");
        assert_eq!(r.note(2, 0x6000, t + 4 + 2 * SPURIOUS_GAP_NS), Ok(1), "a later episode");
        r.reset();
        assert_eq!(r.note(2, 0x6000, t + 5 + 2 * SPURIOUS_GAP_NS), Ok(1), "after a fill");
    }
}
