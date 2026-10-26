//! TLB shootdown, PCID and lazy TLB (MM-6; spec/core.md §8.3, YK-1 3.7; Linux `arch/x86/mm/tlb.c`).
//!
//! The model is Linux's generation scheme:
//! - Every user root is an 8 KiB block: the PML4, then its **context** (`Ctx`): a unique id, a generation
//!   `generation` that every change needing a flush raises, the range of the latest change, a `dying` flag and the
//!   **CPU mask** (the CPUs that have the root loaded, lazily too). Bit 52 of the root's PML4 entry 256 marks
//!   such roots (the CPU ignores bits 52-62 of a PML4 entry), so `activate` finds the context without a lock.
//! - The kernel half has one global generation with its own range.
//! - Per CPU: the context it is loading (`entering`) and has loaded (`loaded`), the generation that context's
//!   translations on this CPU reflect (`synced`), the kernel generation it flushed up to, and with PCID a
//!   small cache of PCID slots, each remembering a context id and the generation its TLB entries reflect.
//! - An **initiator** changes the tables, publishes the range and raises the generation, brings its own CPU
//!   up to date, then sends `VECTOR` to every CPU of the mask that is behind and waits until each answered
//!   (`tlb_plan::user_acked`). A target answers in the IPI handler (`irq` -> `service`), a halted target at
//!   once (idle is `sti; hlt`). After `RESEND_NS` without an answer it sends again, then at doubling intervals
//!   up to `RESEND_MAX_NS` (MM-tlb: a resend is a safety net, a fixed IPI is not lost); after `GIVE_UP_NS` it
//!   marks the CPU stuck and reports it (`Outcome::unanswered`): the caller keeps the frames and tables of that
//!   change out of use. A stuck CPU that misses the next request too costs only `STUCK_GIVE_UP_NS`. Every
//!   round is counted with its wait (`TlbStats::rounds`, `wait_hist`, `slow`, `resent_once`, `several`), the
//!   slowest wait with the CPU that answered last (`take_slowest`), and per CPU the longest send-to-answer
//!   time (`take_answer_max`), so `mm`'s report can tell a slow waker from a CPU with interrupts off.
//! - A CPU that **loads** a context compares its slot's generation with the context's: behind or a new slot
//!   means the CR3 load flushes that PCID, in sync means the no-flush bit. Without PCID every load flushes.
//! - **Lazy TLB**: `activate_lazy_kernel` keeps a user root loaded while a kernel thread runs. A flush that
//!   frees no page tables does not ask such a CPU (MM-tlb3, Linux `tlb_is_not_lazy`): when it comes back to the
//!   root (`switch_root_as`, same root) it clears the flag and catches up before user code runs. A flush that
//!   freed tables asks it too, and a request reaching a lazy CPU that is behind switches it to the kernel root
//!   instead of flushing (Linux's `leave_mm`). Its stale entries must never be used by kernel code either: a
//!   copy of a root's user memory by a thread that does not own the root (`owns_here` false: a kernel thread,
//!   e.g. MCP reading /proc/<pid>/mem) runs page by page in `without_lazy`, which first switches a CPU that
//!   holds that root lazily to the kernel root (as the skipped request would have), with interrupts off until
//!   the copy decided on its path, so `usercopy`'s CR3 check sends it to the table walk (mm `AddressSpace`).
//! - A dying root (`leave`) makes every CPU that still has it loaded switch to the kernel root; only then may
//!   its tables and the block be freed. PCID slots of a dead context are never matched again (ids are unique)
//!   and are flushed when they are given to another context.
//! - Kernel-half mappings are global (`map_4k` sets G there), so `invlpg` drops them in every PCID; a full
//!   kernel flush (more than `RANGE_PAGES` pages, or page tables freed) drops everything (INVPCID type 2, or
//!   CR4.PGE off and on).
//!
//! Ordering (why the shared fields are SeqCst): a loading CPU sets its mask bit and `entering`, then reads
//! `generation`; an initiator raises `generation`, then reads the mask and the CPU's fields. Either the loader sees the new
//! generation (and flushes) or the initiator sees it entering (and waits for it). Likewise a lazy CPU clears
//! `lazy`, then reads `generation`; an initiator raises `generation`, then reads `lazy`: either it saw the CPU
//! lazy and the CPU catches up, or it waits for the CPU. A CPU publishes `loaded` only
//! after CR3 holds the new root, and clears the old context's mask bit before that, which is its last access
//! to the old context: a context frame stays allocated until `leave` saw no CPU entering or holding it.
//!
//! Contexts: the IPI handler runs in hard-IRQ context (atomics only, no heap, no klog). Initiators wait with
//! interrupts off and serve requests to their own CPU meanwhile (`service`), so CPUs shooting down at each
//! other never deadlock; a caller must not hold a lock that another CPU spins for with interrupts off and
//! without calling `poll`.

use super::tlb_plan::{self, Ask, Plan, Step};
use crate::apic::{IpiDest, IpiKind};
use crate::cpu::IrqGuard;
use crate::percpu::{self, PerCpu};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed, Ordering::SeqCst};

pub use super::tlb_plan::{RANGE_PAGES, WAIT_BOUNDS_NS, WAIT_BUCKETS, ticks_to_ns};

/// The system vector of the shootdown IPI (CP-3's system range; 0xF0 timer, 0xF1 SMP_CALL).
pub const VECTOR: u8 = 0xF2;
/// A CPU that has not answered after this long gets the IPI again; each further resend waits twice as long as
/// the one before, up to `RESEND_MAX_NS` (MM-tlb; spec §8.3 said 1 ms, which real wake-ups from deep C-states
/// and SMIs exceed now and then: see `tlb_plan::Limits`).
pub const RESEND_NS: u64 = 2_000_000;
/// The longest interval between two resends.
pub const RESEND_MAX_NS: u64 = 64_000_000;
/// A shootdown that waited this long or more counts as slow (Info event), even without a resend.
pub const SLOW_NS: u64 = 1_000_000;
/// The initiator stops waiting for a CPU after this long (B4: no unbounded wait); see the module comment.
pub const GIVE_UP_NS: u64 = 500_000_000;
/// The same for a CPU that missed an earlier request and has not served one since.
pub const STUCK_GIVE_UP_NS: u64 = 2 * RESEND_NS;
/// A round still waiting this long samples the other CPUs by NMI (RB-2's `nmi::sample_others`) and keeps
/// where the late CPU was (`take_late_sample`): at most once per `SAMPLE_EVERY_NS`, only when RB prepared NMIs.
pub const SAMPLE_AFTER_NS: u64 = 8_000_000;
pub const SAMPLE_EVERY_NS: u64 = 10_000_000_000;
/// How long the sample waits for the NMI answers (the initiator has interrupts off meanwhile).
const SAMPLE_WAIT_NS: u64 = 2_000_000;
const LIMITS: tlb_plan::Limits =
    tlb_plan::Limits { first_ns: RESEND_NS, max_ns: RESEND_MAX_NS, limit_ns: GIVE_UP_NS, stuck_limit_ns: STUCK_GIVE_UP_NS };
/// PCID slots per CPU: a cache of recently loaded contexts (Linux keeps 6), not a limit on address spaces.
pub const PCID_SLOTS: usize = 6;

const CR3_NOFLUSH: u64 = 1 << 63;
/// Bit 52 of PML4 entry `MARK_ENTRY`: "this root carries a context in the next frame".
pub(super) const USER_ROOT_MARK: u64 = 1 << 52;
pub(super) const MARK_ENTRY: usize = 256;
const CTX_MAGIC: u64 = 0x5442_4C43_5458_3656;

/// The range published with a generation; (0, 0) means the whole context.
#[repr(C)]
struct Info {
    generation: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
}

impl Info {
    const fn new() -> Self {
        Info { generation: AtomicU64::new(0), start: AtomicU64::new(0), end: AtomicU64::new(0) }
    }
    /// Only the holder of the context's `busy` flag writes.
    fn publish(&self, generation: u64, (start, end): (u64, u64)) {
        self.generation.store(0, SeqCst);
        self.start.store(start, SeqCst);
        self.end.store(end, SeqCst);
        self.generation.store(generation, SeqCst);
    }
    /// The range of `generation`, or None when another generation's range is there (or being written).
    fn read(&self, generation: u64) -> Option<(u64, u64)> {
        if self.generation.load(SeqCst) != generation {
            return None;
        }
        let r = (self.start.load(SeqCst), self.end.load(SeqCst));
        (self.generation.load(SeqCst) == generation).then_some(r)
    }
}

const HEADER_WORDS: usize = 11;
const MASK_WORDS: usize = 512 - HEADER_WORDS;
/// CPUs the mask has bits for; a CPU with a higher number sets `overflow` and initiators scan every CPU above.
pub const MASK_CPUS: usize = MASK_WORDS * 64;

/// A user root's context: the frame after the PML4.
#[repr(C)]
struct Ctx {
    magic: AtomicU64,
    id: AtomicU64,
    generation: AtomicU64,
    dying: AtomicU64,
    busy: AtomicU64,
    overflow: AtomicU64,
    root: AtomicU64,
    /// MM-tlb2 fixer: 1 = rights-only changes may skip the shootdown round in this space although the kernel's
    /// copy fault path is not known to retry spurious faults (`allow_grants`: a test space no fast kernel copy
    /// writes into).
    grants_ok: AtomicU64,
    info: Info,
    mask: [AtomicU64; MASK_WORDS],
}

const _: () = assert!(core::mem::size_of::<Ctx>() == 4096);

impl Ctx {
    fn mask_set(&self, cpu: usize) {
        if cpu < MASK_CPUS {
            let (w, b) = tlb_plan::mask_pos(cpu);
            self.mask[w].fetch_or(b, SeqCst);
        } else {
            self.overflow.store(1, SeqCst);
        }
    }
    fn mask_clear(&self, cpu: usize) {
        if cpu < MASK_CPUS {
            let (w, b) = tlb_plan::mask_pos(cpu);
            self.mask[w].fetch_and(!b, SeqCst);
        }
    }
    fn for_each_cpu(&self, f: &mut dyn FnMut(usize)) {
        let n = percpu::cpu_count();
        for (w, word) in self.mask.iter().enumerate().take(n.div_ceil(64)) {
            let mut bits = word.load(SeqCst);
            while bits != 0 {
                f(w * 64 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
        if self.overflow.load(SeqCst) != 0 {
            (MASK_CPUS..n).for_each(f);
        }
    }
}

struct Kernel {
    generation: AtomicU64,
    busy: AtomicU64,
    info: Info,
}

static KERNEL: Kernel = Kernel { generation: AtomicU64::new(1), busy: AtomicU64::new(0), info: Info::new() };

struct CpuTlb {
    entering: AtomicU64,
    loaded: AtomicU64,
    synced: AtomicU64,
    lazy: AtomicBool,
    kernel_synced: AtomicU64,
    pcid: AtomicBool,
    /// `enable_pcid` refused once on this CPU (counted then).
    pcid_tried: AtomicBool,
    cur: AtomicUsize,
    slot_id: [AtomicU64; PCID_SLOTS],
    slot_gen: [AtomicU64; PCID_SLOTS],
    victim: AtomicUsize,
    stuck: AtomicBool,
    irqs: AtomicU64,
    /// TSC when an initiator first sent this CPU an IPI that it has not answered yet (0: none): the oldest
    /// unanswered send, taken back by the next `service` (MM-tlb diagnostics; TSC because the IPI handler
    /// must not read `time`'s sequence-locked clock).
    sent_tsc: AtomicU64,
    /// The longest send-to-answer time in TSC ticks since `take_answer_max` last read it.
    answer_max: AtomicU64,
}

impl CpuTlb {
    fn new() -> Self {
        CpuTlb {
            entering: AtomicU64::new(0),
            loaded: AtomicU64::new(0),
            synced: AtomicU64::new(0),
            lazy: AtomicBool::new(false),
            // 0: behind every kernel generation, so the CPU's first `service` flushes everything. A CPU whose
            // state is made after it started (no `prepare_cpus` before the SIPIs) may have cached kernel
            // translations that an earlier shootdown did not reach; this catches those up too.
            kernel_synced: AtomicU64::new(0),
            pcid: AtomicBool::new(false),
            pcid_tried: AtomicBool::new(false),
            cur: AtomicUsize::new(0),
            slot_id: [const { AtomicU64::new(0) }; PCID_SLOTS],
            slot_gen: [const { AtomicU64::new(0) }; PCID_SLOTS],
            victim: AtomicUsize::new(0),
            stuck: AtomicBool::new(false),
            irqs: AtomicU64::new(0),
            sent_tsc: AtomicU64::new(0),
            answer_max: AtomicU64::new(0),
        }
    }
}

static CPUS: PerCpu<CpuTlb> = PerCpu::new(|_| CpuTlb::new());
static READY: AtomicBool = AtomicBool::new(false);
static INIT: AtomicBool = AtomicBool::new(false);
static PCID_OK: AtomicBool = AtomicBool::new(false);
static INVPCID_OK: AtomicBool = AtomicBool::new(false);
static CLOCK: AtomicUsize = AtomicUsize::new(0);
static BOOT_ROOT: AtomicU64 = AtomicU64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Counters {
    user: AtomicU64,
    kernel: AtomicU64,
    leaves: AtomicU64,
    ipis: AtomicU64,
    resends: AtomicU64,
    give_ups: AtomicU64,
    unanswered: AtomicU64,
    not_ready: AtomicU64,
    send_failed: AtomicU64,
    page_flushes: AtomicU64,
    full_flushes: AtomicU64,
    everything: AtomicU64,
    switches: AtomicU64,
    pcid_kept: AtomicU64,
    pcid_flushed: AtomicU64,
    pcid_evictions: AtomicU64,
    pcid_cpus: AtomicU64,
    pcid_no_pge: AtomicU64,
    pcid_cr3_bits: AtomicU64,
    grants: AtomicU64,
    grant_rounds: AtomicU64,
    lazy_entries: AtomicU64,
    lazy_leaves: AtomicU64,
    lazy_skips: AtomicU64,
    lazy_catchups: AtomicU64,
    lazy_drops: AtomicU64,
    dying_leaves: AtomicU64,
    foreign_refused: AtomicU64,
    kept_pages: AtomicU64,
    kept_tables: AtomicU64,
    rounds: AtomicU64,
    slow: AtomicU64,
    resent_once: AtomicU64,
    several: AtomicU64,
    wait_hist: [AtomicU64; tlb_plan::WAIT_BUCKETS],
}

static C: Counters = Counters {
    user: AtomicU64::new(0),
    kernel: AtomicU64::new(0),
    leaves: AtomicU64::new(0),
    ipis: AtomicU64::new(0),
    resends: AtomicU64::new(0),
    give_ups: AtomicU64::new(0),
    unanswered: AtomicU64::new(0),
    not_ready: AtomicU64::new(0),
    send_failed: AtomicU64::new(0),
    page_flushes: AtomicU64::new(0),
    full_flushes: AtomicU64::new(0),
    everything: AtomicU64::new(0),
    switches: AtomicU64::new(0),
    pcid_kept: AtomicU64::new(0),
    pcid_flushed: AtomicU64::new(0),
    pcid_evictions: AtomicU64::new(0),
    pcid_cpus: AtomicU64::new(0),
    pcid_no_pge: AtomicU64::new(0),
    pcid_cr3_bits: AtomicU64::new(0),
    grants: AtomicU64::new(0),
    grant_rounds: AtomicU64::new(0),
    lazy_entries: AtomicU64::new(0),
    lazy_leaves: AtomicU64::new(0),
    lazy_skips: AtomicU64::new(0),
    lazy_catchups: AtomicU64::new(0),
    lazy_drops: AtomicU64::new(0),
    dying_leaves: AtomicU64::new(0),
    foreign_refused: AtomicU64::new(0),
    kept_pages: AtomicU64::new(0),
    kept_tables: AtomicU64::new(0),
    rounds: AtomicU64::new(0),
    slow: AtomicU64::new(0),
    resent_once: AtomicU64::new(0),
    several: AtomicU64::new(0),
    wait_hist: [const { AtomicU64::new(0) }; tlb_plan::WAIT_BUCKETS],
};

/// The longest wait of a shootdown since `take_slowest` last read it, and the CPU that answered last in it
/// (usize::MAX: none).
static SLOWEST_NS: AtomicU64 = AtomicU64::new(0);
static SLOWEST_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);

fn inc(c: &AtomicU64) {
    c.fetch_add(1, Relaxed);
}

/// Totals since boot (statistics for `mm`'s report and the ktests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlbStats {
    /// The IPI vector is registered (shootdowns possible).
    pub ready: bool,
    pub pcid: bool,
    pub invpcid: bool,
    /// CPUs that run with CR4.PCIDE.
    pub pcid_cpus: u64,
    /// MM-tlb2: CPUs that stayed without PCID although CPUID offers it: CR4.PGE off (kernel `invlpg` would
    /// not reach other PCIDs), or CR3 had low bits set when CR4.PCIDE was to be turned on (SDM: #GP).
    pub pcid_no_pge: u64,
    pub pcid_cr3_bits: u64,
    /// MM-tlb2: user leaves whose rights were only widened (write fault on a read-only page that may be
    /// written, `grant_user`): flushed on this CPU alone, no shootdown round.
    pub grants: u64,
    /// MM-tlb2 fixer: rights-only leaves that still took a round, because the kernel's copy fault path does
    /// not retry spurious faults yet (`set_spurious_copy_retry`), and whether it does.
    pub grant_rounds: u64,
    pub spurious_copy_retry: bool,
    /// Shootdowns of user ranges, of kernel ranges, and leaves of dying roots.
    pub user: u64,
    pub kernel: u64,
    pub leaves: u64,
    pub ipis: u64,
    /// Rounds of re-sent IPIs (a CPU did not answer within `RESEND_NS`).
    pub resends: u64,
    /// Shootdowns that stopped waiting (`GIVE_UP_NS`), and the CPUs they gave up on.
    pub give_ups: u64,
    pub unanswered: u64,
    /// Shootdowns that had other CPUs to ask before the vector was ready.
    pub not_ready: u64,
    /// IPIs that could not be sent (the target had no APIC id or the APIC refused): a target that cannot be
    /// reached, not a slow one.
    pub send_failed: u64,
    /// Flushes done (initiators and targets): page by page, whole context, everything (kernel full).
    pub page_flushes: u64,
    pub full_flushes: u64,
    pub everything: u64,
    /// CR3 loads through `activate`; with PCID: loads that kept the PCID's entries, loads that flushed it,
    /// slots given to another context.
    pub switches: u64,
    pub pcid_kept: u64,
    pub pcid_flushed: u64,
    pub pcid_evictions: u64,
    /// Kernel threads that kept a user root loaded, CPUs that left such a root on a request, CPUs that left
    /// a dying root.
    pub lazy_entries: u64,
    pub lazy_leaves: u64,
    /// MM-tlb3: CPUs a flush did not ask because they held the root only lazily (no tables freed), and lazy
    /// CPUs that caught up with such a flush when they came back to the root.
    pub lazy_skips: u64,
    pub lazy_catchups: u64,
    /// Copies of a root's user memory by a thread that held the root only lazily (a kernel thread): the CPU
    /// left the root first, so the copy walked the tables (`without_lazy`).
    pub lazy_drops: u64,
    pub dying_leaves: u64,
    /// `activate` calls for a root that is neither a live user root nor a kernel root (a user root whose
    /// space has ended, e.g. one read from CR3 while this CPU held it lazily): the kernel root was loaded.
    pub foreign_refused: u64,
    /// Pages and page tables kept out of use because a shootdown was not confirmed.
    pub kept_pages: u64,
    pub kept_tables: u64,
    /// CPUs marked stuck now.
    pub stuck_cpus: u64,
    /// Shootdown IPIs taken, summed over the CPUs.
    pub irqs: u64,
    /// MM-tlb: shootdowns that had to ask other CPUs (rounds), and how they were answered: slow (waited
    /// `SLOW_NS` or more without a resend), after exactly one resend, after several resends.
    pub rounds: u64,
    pub slow: u64,
    pub resent_once: u64,
    pub several: u64,
    /// Rounds by wait (`tlb_plan::WAIT_BOUNDS_NS`: below 10 µs, 100 µs, 1 ms, 10 ms, and above).
    pub wait_hist: [u64; tlb_plan::WAIT_BUCKETS],
}

pub fn stats() -> TlbStats {
    let l = |a: &AtomicU64| a.load(Relaxed);
    let (mut stuck, mut irqs) = (0, 0);
    for c in 0..percpu::cpu_count() {
        if let Some(t) = cpu_tlb(c) {
            stuck += t.stuck.load(Relaxed) as u64;
            irqs += t.irqs.load(Relaxed);
        }
    }
    TlbStats {
        ready: READY.load(Relaxed),
        pcid: PCID_OK.load(Relaxed),
        invpcid: INVPCID_OK.load(Relaxed),
        pcid_cpus: l(&C.pcid_cpus),
        pcid_no_pge: l(&C.pcid_no_pge),
        pcid_cr3_bits: l(&C.pcid_cr3_bits),
        grants: l(&C.grants),
        grant_rounds: l(&C.grant_rounds),
        spurious_copy_retry: SPURIOUS_COPY_RETRY.load(SeqCst),
        user: l(&C.user),
        kernel: l(&C.kernel),
        leaves: l(&C.leaves),
        ipis: l(&C.ipis),
        resends: l(&C.resends),
        give_ups: l(&C.give_ups),
        unanswered: l(&C.unanswered),
        not_ready: l(&C.not_ready),
        send_failed: l(&C.send_failed),
        page_flushes: l(&C.page_flushes),
        full_flushes: l(&C.full_flushes),
        everything: l(&C.everything),
        switches: l(&C.switches),
        pcid_kept: l(&C.pcid_kept),
        pcid_flushed: l(&C.pcid_flushed),
        pcid_evictions: l(&C.pcid_evictions),
        lazy_entries: l(&C.lazy_entries),
        lazy_leaves: l(&C.lazy_leaves),
        lazy_skips: l(&C.lazy_skips),
        lazy_catchups: l(&C.lazy_catchups),
        lazy_drops: l(&C.lazy_drops),
        dying_leaves: l(&C.dying_leaves),
        foreign_refused: l(&C.foreign_refused),
        kept_pages: l(&C.kept_pages),
        kept_tables: l(&C.kept_tables),
        stuck_cpus: stuck,
        irqs,
        rounds: l(&C.rounds),
        slow: l(&C.slow),
        resent_once: l(&C.resent_once),
        several: l(&C.several),
        wait_hist: core::array::from_fn(|i| l(&C.wait_hist[i])),
    }
}

/// The longest shootdown wait (ns) since the last call and the CPU that answered last in it; None when no
/// round waited for another CPU since. For `mm`'s report.
pub fn take_slowest() -> Option<(u64, usize)> {
    let ns = SLOWEST_NS.swap(0, Relaxed);
    let cpu = SLOWEST_CPU.swap(usize::MAX, Relaxed);
    (ns > 0 && cpu != usize::MAX).then_some((ns, cpu))
}

/// Return addresses kept per late sample (the start of the backtrace).
pub const LATE_TRACE: usize = 6;

/// Where a CPU was that had not answered a shootdown for `SAMPLE_AFTER_NS` (its NMI answer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LateSample {
    pub cpu: usize,
    /// How long the round had waited when it sampled.
    pub waited_ns: u64,
    pub rip: u64,
    /// RFLAGS.IF of the interrupted code: false = the CPU ran with interrupts off; true = it could have taken
    /// the IPI (then it was asleep, in SMM, or in a long instruction, e.g. an MMIO read that timed out).
    pub interrupts_on: bool,
    pub user: bool,
    pub trace: [u64; LATE_TRACE],
    pub ntrace: usize,
}

// One late sample, written by the round that won `LAST_SAMPLE` and read by `mm`'s report.
const LATE_WORDS: usize = 6 + LATE_TRACE;
static LATE: [AtomicU64; LATE_WORDS] = [const { AtomicU64::new(0) }; LATE_WORDS];
static LATE_FULL: AtomicBool = AtomicBool::new(false);
static LAST_SAMPLE: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// The last late sample, if `mm`'s report has not taken it yet.
pub fn late_sample() -> Option<LateSample> {
    if !LATE_FULL.load(SeqCst) {
        return None;
    }
    let w: [u64; LATE_WORDS] = core::array::from_fn(|i| LATE[i].load(Relaxed));
    let ntrace = (w[5] as usize).min(LATE_TRACE);
    Some(LateSample {
        cpu: w[0] as usize,
        waited_ns: w[1],
        rip: w[2],
        interrupts_on: w[3] & 1 != 0,
        user: w[3] & 2 != 0,
        trace: core::array::from_fn(|i| w[6 + i]),
        ntrace,
    })
}

/// `late_sample`, and frees the place for the next one.
pub fn take_late_sample() -> Option<LateSample> {
    let s = late_sample();
    LATE_FULL.store(false, SeqCst);
    s
}

/// Late samples taken since boot.
pub fn late_samples() -> u64 {
    SAMPLES.load(Relaxed)
}

/// Lets the next slow round sample at once (ktests; the limit only keeps NMIs rare on a sick machine).
pub fn allow_sample_now() {
    LAST_SAMPLE.store(0, SeqCst);
}

/// From a waiting round (interrupts off): samples every other CPU by NMI and keeps where `cpu` was.
/// The 10 s budget (`LAST_SAMPLE`) is spent only by a round whose NMIs went out, not by one that found the NMI
/// path unprepared or busy (the watchdog's sample, a panic stop). A round whose NMIs went out but that `cpu`
/// did not answer keeps it spent: the budget limits NMIs on a sick machine, and those were sent.
fn sample_late(cpu: usize, waited_ns: u64, t: u64) {
    let last = LAST_SAMPLE.load(SeqCst);
    if (last != 0 && t.saturating_sub(last) < SAMPLE_EVERY_NS) || LATE_FULL.load(SeqCst) || !crate::nmi::prepared() {
        return;
    }
    let mine = t.max(1);
    if LAST_SAMPLE.compare_exchange(last, mine, SeqCst, Relaxed).is_err() {
        return;
    }
    let before = crate::nmi::answers(cpu);
    if crate::nmi::sample_others(now, SAMPLE_WAIT_NS).is_err() {
        // Nothing was sent: give the budget back (unless another round took it meanwhile).
        let _ = LAST_SAMPLE.compare_exchange(mine, last, SeqCst, Relaxed);
        return;
    }
    if crate::nmi::answers(cpu) == before {
        return;
    }
    let Some(r) = crate::nmi::regs(cpu) else { return };
    let mut trace = [0u64; LATE_TRACE];
    let mut n = 0;
    for s in r.steps.iter().take(r.nsteps) {
        let a = match *s {
            crate::panic::Step::Call(a) | crate::panic::Step::Guess(a) => a,
            crate::panic::Step::Trap { rip, .. } => rip,
            _ => continue,
        };
        if n < LATE_TRACE {
            trace[n] = a;
            n += 1;
        }
    }
    const IF: u64 = 1 << 9;
    let flags = (r.frame.rflags & IF != 0) as u64 | (r.user as u64) << 1;
    for (i, v) in [cpu as u64, waited_ns, r.frame.rip, flags, 0, n as u64].into_iter().chain(trace).enumerate() {
        LATE[i].store(v, Relaxed);
    }
    SAMPLES.fetch_add(1, Relaxed);
    LATE_FULL.store(true, SeqCst);
}

/// The longest time (TSC ticks) `cpu` took from an IPI's send to its answer since the last call (0: none).
pub fn take_answer_max(cpu: usize) -> u64 {
    cpu_tlb(cpu).map_or(0, |t| t.answer_max.swap(0, Relaxed))
}

pub(super) fn count_grants(pages: u64) {
    C.grants.fetch_add(pages, Relaxed);
}

pub(super) fn count_grant_rounds(pages: u64) {
    C.grant_rounds.fetch_add(pages, Relaxed);
}

/// Whether a kernel copy that faults on an access its page table allows is retried (the fault itself drops
/// the stale entry on x86, SDM 4.10.4.1). Off until the copy fault resolver says so.
static SPURIOUS_COPY_RETRY: AtomicBool = AtomicBool::new(false);

/// MM-tlb2 fixer: the copy fault resolver (`hal::usercopy::set_fault_resolver`, proc's `resolve_copy_fault`)
/// retries a fault on an allowed access as a spurious one (Linux handle_pte_fault, linux-6.6/mm/memory.c:5016;
/// `flush_tlb_fix_spurious_fault` is empty on x86, arch/x86/include/asm/pgtable.h:1325). Only then may
/// `grant_user` leave a narrower (read-only) translation on other CPUs: before, a `read()` into a page another
/// thread had just made writable through a reused copy-on-write or dirty-marked page would fail with EFAULT on
/// a CPU that still holds the read-only entry.
pub fn set_spurious_copy_retry(on: bool) {
    SPURIOUS_COPY_RETRY.store(on, SeqCst);
}

/// Lets `root`'s rights-only changes skip the round even without `set_spurious_copy_retry`: for a space no
/// fast kernel copy writes into (the ktests' in-kernel spaces). Reset when the root is made again.
pub fn allow_grants(root: u64) {
    if let Some(c) = ctx_of(root) {
        c.grants_ok.store(1, SeqCst);
    }
}

/// Whether `grant_user` may skip the round for `root`.
pub(super) fn grants_allowed(root: u64) -> bool {
    SPURIOUS_COPY_RETRY.load(SeqCst) || ctx_of(root).is_some_and(|c| c.grants_ok.load(SeqCst) != 0)
}

pub(super) fn count_kept(pages: u64, tables: u64) {
    C.kept_pages.fetch_add(pages, Relaxed);
    C.kept_tables.fetch_add(tables, Relaxed);
}

/// What one shootdown found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Other CPUs that had to answer when the request went out.
    pub targets: u32,
    pub ipis: u32,
    /// Rounds of re-sent IPIs.
    pub resends: u32,
    /// CPUs that did not answer (marked stuck): the caller keeps what the change freed out of use.
    pub unanswered: u32,
    /// Other CPUs had to answer but the IPI vector was not registered (no `init`).
    pub not_ready: bool,
}

impl Outcome {
    /// Every CPU that could hold the old translations has dropped them.
    pub fn confirmed(&self) -> bool {
        self.unanswered == 0
    }
    /// Adds another shootdown's numbers (callers that flush in batches).
    pub fn add(&mut self, o: Outcome) {
        self.targets = self.targets.max(o.targets);
        self.ipis += o.ipis;
        self.resends += o.resends;
        self.unanswered = self.unanswered.max(o.unanswered);
        self.not_ready |= o.not_ready;
    }
}

fn cpu_tlb(cpu: usize) -> Option<&'static CpuTlb> {
    CPUS.get_for(cpu)
}

/// Creates every possible CPU's state now, so the IPI handler never allocates (`PerCpu` grows only when a
/// CPU number is not covered yet; once covered this costs one load).
fn presize() {
    let n = percpu::possible_cpus().max(percpu::cpu_count());
    if n > 0 {
        let _ = CPUS.get_for(n - 1);
    }
}

/// Creates the state of every possible CPU (thread context, interrupts on, no lock held). Call once the
/// possible-CPU count is known (after `acpi::init`) and before the APs start, so no AP ever grows the per-CPU
/// table from its IPI handler. `init` runs earlier (one CPU known), so MM calls this again from its Core
/// initcall; the bring-up should call it right before the SIPIs.
pub fn prepare_cpus() {
    if crate::BARE_METAL {
        presize();
    }
}

/// Registers the IPI vector, records the boot root (where CPUs go when they leave a root before the kernel
/// root exists) and the CPU's PCID/INVPCID support. Call once, on the BSP, before the APs start; later calls
/// only update the clock. `clock` gives monotonic nanoseconds (until it is set: TSC ticks, which only makes
/// the waits shorter).
pub fn init(clock: Option<fn() -> u64>) -> Result<(), crate::vectors::VectorError> {
    if let Some(f) = clock {
        CLOCK.store(f as usize, SeqCst);
    }
    if INIT.swap(true, SeqCst) {
        return Ok(());
    }
    BOOT_ROOT.store(super::current_root(), SeqCst);
    if crate::BARE_METAL {
        let f = crate::cpu::detect();
        PCID_OK.store(f.pcid, SeqCst);
        INVPCID_OK.store(f.invpcid, SeqCst);
    }
    presize();
    crate::vectors::register_system(VECTOR, irq)?;
    READY.store(true, SeqCst);
    Ok(())
}

/// Sets the clock the waits use (`time::now_ns` once it is calibrated).
pub fn set_clock(clock: fn() -> u64) {
    CLOCK.store(clock as usize, SeqCst);
}

pub(super) fn now() -> u64 {
    match CLOCK.load(Relaxed) {
        0 => crate::cpu::rdtsc(),
        f => {
            // SAFETY: only `init`/`set_clock` store into CLOCK, always a `fn() -> u64`.
            let f = unsafe { core::mem::transmute::<usize, fn() -> u64>(f) };
            match f() {
                0 => crate::cpu::rdtsc(),
                t => t,
            }
        }
    }
}

fn leave_root() -> u64 {
    super::kernel_root().unwrap_or_else(|| BOOT_ROOT.load(Relaxed))
}

/// Whether `root` (no user context) may be loaded as it is: the kernel root, the loader's root while it is
/// the kernel's, the root `switch_kernel_root` is about to install, or the one hal's switch code resolves
/// `KERNEL_CR3` to. Before the boot root is known (no `init` yet) nothing else exists to tell apart.
fn is_kernel_root(root: u64) -> bool {
    let home = leave_root();
    home == 0 || root == home || root == super::kernel::candidate() || root == crate::switch::kernel_root() & super::ADDR_MASK
}

/// The root a CPU loads when it leaves a user root: the kernel root (the loader's before MM installed its own).
/// What kernel code means by "the kernel root"; CR3 may hold a user root lazily (MM-6), so it is not that.
pub fn home_root() -> u64 {
    leave_root()
}

// ---------------------------------------------------------------------------------------------------------
// Contexts

/// The context frame at direct-map address `addr`.
///
/// # Safety
/// `addr` is the direct-map address of the second frame of a user root's 8 KiB block (a context frame) that
/// stays allocated for as long as the returned reference is used. Every field is an atomic, so any content
/// is a valid value and shared references are fine.
unsafe fn ctx_at(addr: u64) -> &'static Ctx {
    // SAFETY: the caller's contract (above): a live, 4 KiB-aligned direct-mapped frame of the right size.
    unsafe { &*(addr as *const Ctx) }
}

fn addr_of(c: &Ctx) -> u64 {
    c as *const Ctx as u64
}

/// The context of a marked user root (None for the kernel root and any other root).
fn ctx_of(root: u64) -> Option<&'static Ctx> {
    if !crate::BARE_METAL || root == 0 || root & 0x1FFF != 0 || super::read_entry(root, MARK_ENTRY) & USER_ROOT_MARK == 0 {
        return None;
    }
    // SAFETY: the root is marked, so it was made by `new_user_root` and its next frame is its context; the
    // caller holds the root (it may not be freed while the caller uses it), and the magic is checked below.
    let c = unsafe { ctx_at(crate::phys_to_virt(root + 4096)) };
    (c.magic.load(SeqCst) == CTX_MAGIC ^ root).then_some(c)
}

/// Sets up the context frame of a new user root (`root` 8 KiB aligned, the next frame owned with it).
pub(super) fn init_ctx(root: u64) {
    if !crate::BARE_METAL {
        return;
    }
    // SAFETY: `new_user_root` passes a fresh 8 KiB block it owns; the context frame is its second half.
    let c = unsafe { ctx_at(crate::phys_to_virt(root + 4096)) };
    for w in c.mask.iter() {
        w.store(0, Relaxed);
    }
    c.id.store(NEXT_ID.fetch_add(1, Relaxed), Relaxed);
    c.generation.store(1, Relaxed);
    c.dying.store(0, Relaxed);
    c.busy.store(0, Relaxed);
    c.overflow.store(0, Relaxed);
    c.root.store(root, Relaxed);
    c.grants_ok.store(0, Relaxed);
    c.info.publish(0, (0, 0));
    c.magic.store(CTX_MAGIC ^ root, SeqCst);
}

/// Unmarks a root whose `leave` was confirmed, before its block is freed.
pub(super) fn retire_ctx(root: u64) {
    if let Some(c) = ctx_of(root) {
        c.magic.store(0, SeqCst);
    }
}

/// Whether `root` carries a context (a user root made by `new_user_root` and not retired).
pub fn has_context(root: u64) -> bool {
    ctx_of(root).is_some()
}

/// `root`'s generation (every change that needed a shootdown round raised it; None: no user context): tests count
/// the rounds of one space with it, unaffected by other programs' rounds (MM-tlb2).
pub fn generation(root: u64) -> Option<u64> {
    ctx_of(root).map(|c| c.generation.load(SeqCst))
}

/// Calls `f` for every CPU in `root`'s mask (CPUs that have it loaded or are loading it).
pub fn cpus_with(root: u64, f: &mut dyn FnMut(usize)) {
    if let Some(c) = ctx_of(root) {
        c.for_each_cpu(f);
    }
}

// ---------------------------------------------------------------------------------------------------------
// Loading roots (every CR3 write except the AP trampoline, Reload and CP's MTRR reload goes through here)

/// Loads `root` on this CPU (interrupts off inside). A user root already loaded here (also lazily) stays.
pub(super) fn switch_root(root: u64) {
    switch_root_as(root, false)
}

/// A root a thread saved at its last switch (hal::switch): loaded only while it is still a live user root; a
/// root whose space ended meanwhile (no context any more, or dying) is never loaded, the CPU takes the kernel
/// root instead. One `ctx_of` decides, so a root retired between a check and the load cannot slip through.
pub(super) fn switch_saved_root(root: u64) {
    switch_root_as(root, true)
}

fn switch_root_as(root: u64, saved: bool) {
    if !crate::BARE_METAL {
        return;
    }
    let _irq = IrqGuard::new();
    let c = CPUS.get();
    let cpu = percpu::this_cpu();
    let prev = c.loaded.load(Relaxed);
    let next = ctx_of(root);
    let next_addr = next.map_or(0, addr_of);
    if next_addr != 0 && next_addr == prev && super::current_root() == root {
        // MM-tlb3: a CPU that held the root lazily was not asked by flushes that freed no tables, so it catches
        // up now, before the root's user code runs again. The flag is cleared first (SeqCst) and the generation
        // read after it: an initiator that still saw the flag set raised the generation before (Linux
        // switch_mm_irqs_off's same-mm path, arch/x86/mm/tlb.c:575).
        if c.lazy.swap(false, SeqCst) {
            let before = c.synced.load(SeqCst);
            service(c);
            if c.loaded.load(SeqCst) == next_addr && c.synced.load(SeqCst) > before {
                inc(&C.lazy_catchups);
            }
        }
        return;
    }
    match next {
        Some(n) => {
            if !load_ctx(c, cpu, n, root) {
                // The root is dying (a kernel thread's saved root whose space ends right now): its `leave` may
                // not wait for this CPU, so it is never loaded; this CPU goes to the kernel root instead.
                let k = leave_root();
                if k != 0 {
                    imp::write_cr3(k);
                }
                c.synced.store(0, SeqCst);
                c.lazy.store(false, SeqCst);
                if prev != 0 && prev != next_addr {
                    // SAFETY: as below.
                    unsafe { ctx_at(prev) }.mask_clear(cpu);
                }
                c.loaded.store(0, SeqCst);
                c.entering.store(0, SeqCst);
                inc(&C.dying_leaves);
                return;
            }
        }
        None => {
            // Without a context only a kernel root is ever loaded. Anything else is a user root whose space
            // ended (its block may be free and reused already): no CPU's mask holds a load of it, so its `leave`
            // could not wait for this CPU, and the CPU would walk freed tables (INT-A2a2).
            let r = if saved || is_kernel_root(root) {
                if saved { leave_root() } else { root }
            } else {
                inc(&C.foreign_refused);
                leave_root()
            };
            if r != 0 {
                imp::write_cr3(r);
            }
            c.synced.store(0, SeqCst);
        }
    }
    c.lazy.store(false, SeqCst);
    if prev != 0 && prev != next_addr {
        // SAFETY: this CPU's own `loaded` value: the context stays allocated until `leave` saw this CPU give it
        // up, which is the store of `loaded` below (module comment, Ordering).
        unsafe { ctx_at(prev) }.mask_clear(cpu);
    }
    c.loaded.store(next_addr, SeqCst);
    c.entering.store(0, SeqCst);
    inc(&C.switches);
}

/// False (nothing loaded, mask bit and `entering` taken back) when the context is dying: `leave` raises the
/// generation after `dying`, so a loader that set its mask bit too late for the leaver to see it reads `dying`
/// here (SeqCst), and one the leaver saw is waited for and leaves at its next `service`.
fn load_ctx(c: &CpuTlb, cpu: usize, n: &Ctx, root: u64) -> bool {
    let pcid = enable_pcid(c);
    n.mask_set(cpu);
    c.entering.store(addr_of(n), SeqCst);
    if n.dying.load(SeqCst) != 0 {
        n.mask_clear(cpu);
        c.entering.store(0, SeqCst);
        return false;
    }
    let g = n.generation.load(SeqCst);
    if pcid {
        let ids: [u64; PCID_SLOTS] = core::array::from_fn(|i| c.slot_id[i].load(Relaxed));
        let id = n.id.load(Relaxed);
        let (slot, fresh) = tlb_plan::pick_slot(&ids, id, c.victim.load(Relaxed));
        if fresh {
            c.victim.fetch_add(1, Relaxed);
            if ids[slot] != 0 {
                inc(&C.pcid_evictions);
            }
            c.slot_id[slot].store(id, Relaxed);
        }
        let flush = fresh || c.slot_gen[slot].load(Relaxed) < g;
        inc(if flush { &C.pcid_flushed } else { &C.pcid_kept });
        imp::write_cr3(root | (slot as u64 + 1) | if flush { 0 } else { CR3_NOFLUSH });
        c.slot_gen[slot].store(g, Relaxed);
        c.cur.store(slot, Relaxed);
    } else {
        imp::write_cr3(root);
    }
    c.synced.store(g, SeqCst);
    true
}

/// Turns CR4.PCIDE on for this CPU the first time it loads a user root (CR3's low bits are 0 then: no PCID
/// was ever loaded here). CR4.PCIDE is MM's bit (cpu.rs `CR4_KEEP`).
fn enable_pcid(c: &CpuTlb) -> bool {
    if c.pcid.load(Relaxed) {
        return true;
    }
    if !PCID_OK.load(Relaxed) {
        return false;
    }
    let cr4 = crate::cpu::read_cr4();
    // Kernel-half shootdowns flush page by page with `invlpg`, which drops other PCIDs' entries only for
    // global pages; without CR4.PGE the G bit is ignored, so this CPU stays without PCID (every load flushes).
    // Counted once per CPU (`pcid_tried`), so `mm` can say why PCID stayed off (MM-tlb2).
    if cr4 & crate::cpu::CR4_PGE == 0 {
        if !c.pcid_tried.swap(true, Relaxed) {
            inc(&C.pcid_no_pge);
        }
        return false;
    }
    if cr4 & crate::cpu::CR4_PCIDE == 0 {
        if crate::cpu::read_cr3() & 0xFFF != 0 {
            if !c.pcid_tried.swap(true, Relaxed) {
                inc(&C.pcid_cr3_bits);
            }
            return false;
        }
        imp::write_cr4(cr4 | crate::cpu::CR4_PCIDE);
    }
    c.pcid.store(true, Relaxed);
    inc(&C.pcid_cpus);
    true
}

/// Reload's point of no return (`kexec_stub::jump`): loads the bridge root as it is. It has no user context and
/// is no kernel root, so `switch_root` would refuse it and load the kernel root, which lacks the identity-mapped
/// stub page: the jump then faulted on the stub's address (Mini-PC 20.10., after INT-A2a2's refusal). Safe to
/// load here because every other CPU is stopped and interrupts are off: no `leave` can free it under this CPU,
/// and this kernel's Rust code never runs again.
pub(crate) fn load_for_jump(root: u64) {
    if crate::BARE_METAL {
        imp::write_cr3(root);
    }
}

/// For a kernel thread: keeps the user root this CPU has loaded (lazy TLB) or loads the kernel root.
pub fn activate_lazy_kernel() {
    if !crate::BARE_METAL {
        return;
    }
    let _irq = IrqGuard::new();
    let c = CPUS.get();
    if c.loaded.load(Relaxed) != 0 {
        if !c.lazy.swap(true, SeqCst) {
            inc(&C.lazy_entries);
        }
        return;
    }
    let k = leave_root();
    if k != 0 && super::current_root() != k {
        switch_root(k);
    }
}

/// The CR3 value of the running thread's own choice: the kernel root while this CPU holds a user root only
/// lazily, CR3 otherwise. The switch code saves this, not CR3, so a kernel thread never keeps a user root.
pub fn thread_root() -> u64 {
    if !crate::BARE_METAL {
        return crate::cpu::read_cr3();
    }
    let _irq = IrqGuard::new();
    if CPUS.get().lazy.load(Relaxed) { leave_root() } else { crate::cpu::read_cr3() }
}

/// Whether this CPU holds a user root lazily now.
pub fn lazy_here() -> bool {
    crate::BARE_METAL && {
        let _irq = IrqGuard::new();
        CPUS.get().lazy.load(Relaxed)
    }
}

/// Whether the running thread owns `root` here: CR3 holds it and not only lazily. Stays true across a thread
/// switch (the switch code reloads the thread's own root, `thread_root`), so a copy may go through this CPU's
/// TLB. False for a kernel thread: it may be resumed on a CPU that holds `root` lazily with stale entries.
pub fn owns_here(root: u64) -> bool {
    if !crate::BARE_METAL {
        return true;
    }
    let _irq = IrqGuard::new();
    !CPUS.get().lazy.load(SeqCst) && super::current_root() == root
}

/// Runs `f` (a short access to `root`'s user memory by a thread that does not own `root`) with interrupts off,
/// after this CPU stopped holding `root` lazily: it switches to the kernel root, as the request a flush skipped
/// would have made it (`lazy_drops`). With interrupts off the thread cannot move to another CPU that holds
/// `root` lazily before `f` looked at CR3, so `usercopy` takes the table walk, never a stale entry. `f` must
/// not block or fault (the table walk does neither).
pub fn without_lazy<T>(root: u64, f: impl FnOnce() -> T) -> T {
    let _irq = IrqGuard::new();
    if crate::BARE_METAL {
        let c = CPUS.get();
        if c.lazy.load(SeqCst) && super::current_root() == root {
            inc(&C.lazy_drops);
            leave_here(c);
        }
    }
    f()
}

/// Whether this CPU runs with PCID.
pub fn pcid_here() -> bool {
    crate::BARE_METAL && {
        let _irq = IrqGuard::new();
        CPUS.get().pcid.load(Relaxed)
    }
}

/// Shootdown IPIs `cpu` took.
pub fn cpu_irqs(cpu: usize) -> u64 {
    cpu_tlb(cpu).map_or(0, |t| t.irqs.load(Relaxed))
}

// ---------------------------------------------------------------------------------------------------------
// Serving requests (IPI handler, waiting initiators, `poll`)

fn irq(_v: u8) {
    let c = CPUS.get();
    c.irqs.fetch_add(1, Relaxed);
    service(c);
}

/// Serves the requests to this CPU now: for code that spins with interrupts off (another CPU may be waiting
/// for this one).
pub fn poll() {
    if !crate::BARE_METAL {
        return;
    }
    let _irq = IrqGuard::new();
    service(CPUS.get());
}

/// Brings this CPU's TLB up to the kernel generation and its loaded context's generation; leaves a dying
/// context, and a lazily held one that changed. Interrupts are off.
fn service(c: &CpuTlb) {
    c.stuck.store(false, Relaxed);
    // Taken back before the generations are read: a send stamped after this swap is answered by the next
    // `service` (its IPI is pending), one stamped before it published its generation first (SeqCst both sides).
    let sent = c.sent_tsc.swap(0, SeqCst);
    if sent != 0 {
        let d = crate::cpu::rdtsc().wrapping_sub(sent);
        // TSCs of different CPUs may differ slightly: a "negative" delay is no delay.
        if d < 1 << 62 {
            c.answer_max.fetch_max(d, Relaxed);
        }
    }
    let kg = KERNEL.generation.load(SeqCst);
    let ks = c.kernel_synced.load(SeqCst);
    if kg > ks {
        apply_kernel(tlb_plan::plan(ks, kg, KERNEL.info.read(kg)));
        c.kernel_synced.store(kg, SeqCst);
    }
    let l = c.loaded.load(SeqCst);
    if l == 0 {
        return;
    }
    // SAFETY: this CPU's own `loaded` value, kept allocated until this CPU publishes another (as in switch_root).
    let x = unsafe { ctx_at(l) };
    let g = x.generation.load(SeqCst);
    let s = c.synced.load(SeqCst);
    let dying = x.dying.load(SeqCst) != 0;
    if dying || (g > s && c.lazy.load(SeqCst)) {
        inc(if dying { &C.dying_leaves } else { &C.lazy_leaves });
        leave_here(c);
        return;
    }
    if g > s {
        apply_user(tlb_plan::plan(s, g, x.info.read(g)));
        if c.pcid.load(Relaxed) {
            c.slot_gen[c.cur.load(Relaxed)].store(g, Relaxed);
        }
        c.synced.store(g, SeqCst);
    }
}

/// Switches this CPU from its loaded user root to the kernel root.
fn leave_here(c: &CpuTlb) {
    let k = leave_root();
    if k == 0 {
        return;
    }
    let prev = c.loaded.load(Relaxed);
    imp::write_cr3(k);
    c.synced.store(0, SeqCst);
    c.lazy.store(false, SeqCst);
    if prev != 0 {
        // SAFETY: this CPU's own `loaded` value, as in switch_root.
        unsafe { ctx_at(prev) }.mask_clear(percpu::this_cpu());
    }
    c.loaded.store(0, SeqCst);
}

fn apply_kernel(p: Plan) {
    match p {
        Plan::Nothing => {}
        Plan::Pages { start, pages } => {
            (0..pages).for_each(|i| super::flush_page(start + i * 4096));
            inc(&C.page_flushes);
        }
        Plan::Full => flush_everything(),
    }
}

fn apply_user(p: Plan) {
    match p {
        Plan::Nothing => {}
        Plan::Pages { start, pages } => {
            (0..pages).for_each(|i| super::flush_page(start + i * 4096));
            inc(&C.page_flushes);
        }
        Plan::Full => {
            // The same root and PCID without the no-flush bit: drops that PCID's entries (all non-global ones
            // without PCID) and its paging-structure caches.
            imp::write_cr3(crate::cpu::read_cr3() & !CR3_NOFLUSH);
            inc(&C.full_flushes);
        }
    }
}

/// Drops every TLB entry of this CPU, global ones and every PCID included.
pub fn flush_everything() {
    let _irq = IrqGuard::new();
    inc(&C.everything);
    if INVPCID_OK.load(Relaxed) {
        imp::invpcid_all();
        return;
    }
    let cr4 = crate::cpu::read_cr4();
    if cr4 & crate::cpu::CR4_PGE != 0 {
        imp::toggle_pge(cr4);
    } else {
        // No global pages: a CR3 reload drops the current PCID; the other PCIDs' slots are forgotten so the
        // next load of each flushes it.
        imp::write_cr3(crate::cpu::read_cr3() & !CR3_NOFLUSH);
        if crate::BARE_METAL {
            let c = CPUS.get();
            let cur = c.cur.load(Relaxed);
            for (i, s) in c.slot_id.iter().enumerate() {
                if i != cur || c.loaded.load(Relaxed) == 0 {
                    s.store(0, Relaxed);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Initiators

fn acquire(busy: &AtomicU64, c: &CpuTlb) {
    while busy.compare_exchange(0, 1, SeqCst, SeqCst).is_err() {
        service(c);
        core::hint::spin_loop();
    }
}

fn send(cpu: usize) -> bool {
    if let Some(t) = cpu_tlb(cpu) {
        // Only the oldest unanswered send counts (a resend keeps the first stamp).
        let _ = t.sent_tsc.compare_exchange(0, crate::cpu::rdtsc().max(1), SeqCst, Relaxed);
    }
    let ok = match crate::smp::apic_id(cpu) {
        Some(a) => crate::apic::send_ipi(IpiDest::Physical(a), IpiKind::Fixed(VECTOR)).is_ok(),
        None => false,
    };
    if !ok {
        inc(&C.send_failed);
    }
    ok
}

/// Waits until every CPU `scan` names (other than this one) is `acked`; see the module comment.
/// Calls its argument for every CPU a request goes to.
type Scan<'a> = &'a dyn Fn(&mut dyn FnMut(usize));

/// `acked(cpu, first)`: `first` is true in the first look only (the one that decides whom to send to).
fn wait_for(c: &CpuTlb, scan: Scan<'_>, acked: &dyn Fn(usize, bool) -> bool) -> Outcome {
    let me = percpu::this_cpu();
    let mut out = Outcome::default();
    let ready = READY.load(SeqCst);
    // (missing, stuck among them, one missing CPU, one missing CPU that is not stuck (usize::MAX: none))
    let pass = |send_now: bool, first: bool, out: &mut Outcome| {
        let (mut missing, mut stuck, mut one, mut late) = (0u32, 0u32, usize::MAX, usize::MAX);
        scan(&mut |cpu| {
            if cpu == me || acked(cpu, first) {
                return;
            }
            missing += 1;
            one = cpu;
            if cpu_tlb(cpu).is_some_and(|t| t.stuck.load(Relaxed)) {
                stuck += 1;
            } else {
                late = cpu;
            }
            if send_now && ready && send(cpu) {
                out.ipis += 1;
            }
        });
        (missing, stuck, one, late)
    };
    let start = now();
    let (first, _, mut last_missing, _) = pass(true, true, &mut out);
    out.targets = first;
    if first == 0 {
        return finish(out);
    }
    if !ready {
        out.unanswered = first;
        out.not_ready = true;
        return finish(out);
    }
    let mut w = tlb_plan::Waiting::new(start);
    let mut sampled = false;
    let done = loop {
        service(c);
        for _ in 0..64 {
            core::hint::spin_loop();
        }
        // The time first, then the look: a resend or give-up is decided only by a look made after the interval
        // ran out (MM-tlb: an initiator delayed between its look and the clock read resent needlessly).
        let t = now();
        let (missing, stuck, one, late) = pass(false, false, &mut out);
        if missing == 0 {
            break t;
        }
        last_missing = one;
        // The sample is about a CPU that is newly late, not one stuck since an earlier round.
        if !sampled && t.saturating_sub(start) >= SAMPLE_AFTER_NS && late != usize::MAX {
            sampled = true;
            sample_late(late, t.saturating_sub(start), t);
        }
        let s = tlb_plan::step(&w, t, &LIMITS, stuck == missing);
        tlb_plan::looked(&mut w, t, &LIMITS);
        match s {
            Step::Wait => {}
            Step::Resend => {
                pass(true, false, &mut out);
                out.resends += 1;
                w.resends += 1;
                w.last_send = t;
            }
            Step::GiveUp => {
                scan(&mut |cpu| {
                    if cpu != me && !acked(cpu, false) {
                        out.unanswered += 1;
                        if let Some(tl) = cpu_tlb(cpu) {
                            tl.stuck.store(true, Relaxed);
                        }
                    }
                });
                break t;
            }
        }
    };
    record_round(done.saturating_sub(start), out.resends, last_missing);
    finish(out)
}

/// Counts an answered (or given-up) round that asked other CPUs: its verdict, its wait's bucket, and the
/// slowest wait with the CPU that answered last.
fn record_round(wait_ns: u64, resends: u32, last_cpu: usize) {
    inc(&C.rounds);
    inc(&C.wait_hist[tlb_plan::wait_bucket(wait_ns)]);
    match tlb_plan::verdict(wait_ns, resends, SLOW_NS) {
        tlb_plan::Verdict::Quick => {}
        tlb_plan::Verdict::Slow => inc(&C.slow),
        tlb_plan::Verdict::ResentOnce => inc(&C.resent_once),
        tlb_plan::Verdict::Several => inc(&C.several),
    }
    if wait_ns > SLOWEST_NS.fetch_max(wait_ns, Relaxed) {
        // Racy with another initiator's record, which only mixes up the CPU of two near-equal waits.
        SLOWEST_CPU.store(last_cpu, Relaxed);
    }
}

fn finish(out: Outcome) -> Outcome {
    C.ipis.fetch_add(out.ipis as u64, Relaxed);
    C.resends.fetch_add(out.resends as u64, Relaxed);
    if out.unanswered > 0 {
        C.unanswered.fetch_add(out.unanswered as u64, Relaxed);
        inc(if out.not_ready { &C.not_ready } else { &C.give_ups });
    }
    out
}

/// After a change of `root`'s user half in [start, end) (`full`: or anything else): every CPU that has `root`
/// loaded drops the old translations before this returns, unless the outcome says otherwise. A CPU that holds
/// `root` only lazily is not asked (MM-tlb3, Linux `tlb_is_not_lazy`): it catches up before it runs `root`'s user
/// code again (`switch_root_as`), and a kernel thread copying `root`'s memory leaves it first (`without_lazy`). With
/// `tables` (page tables of `root` were freed) lazy CPUs are asked too and leave the root, as in Linux: a freed
/// table must not stay reachable by a speculative walk. A root without a context is flushed on this CPU only (no
/// other CPU can hold it).
pub fn flush_user(root: u64, start: u64, end: u64, full: bool, tables: bool) -> Outcome {
    if !crate::BARE_METAL {
        return Outcome::default();
    }
    let Some(x) = ctx_of(root) else {
        if super::current_root() == root {
            let _irq = IrqGuard::new();
            apply_user(tlb_plan::plan(0, 1, Some(tlb_plan::published_range(start, end, full))));
        }
        return Outcome::default();
    };
    let _irq = IrqGuard::new();
    presize();
    let c = CPUS.get();
    acquire(&x.busy, c);
    let g = x.generation.load(SeqCst) + 1;
    x.info.publish(g, tlb_plan::published_range(start, end, full));
    x.generation.store(g, SeqCst);
    inc(&C.user);
    service(c);
    let xa = addr_of(x);
    let ask = if tables { Ask::FlushTables } else { Ask::Flush };
    // The lazy flag is read after the generation was raised (SeqCst): a CPU that clears it later reads the new
    // generation afterwards (`switch_root_as`) and flushes before it runs user code.
    // Skips are counted where they are decided: in the first look, a CPU answered only because it is lazy.
    let skips = core::cell::Cell::new(0u64);
    let acked = |t: &CpuTlb, first: bool| {
        let (e, l, s) = (t.entering.load(SeqCst), t.loaded.load(SeqCst), t.synced.load(SeqCst));
        let lazy = t.lazy.load(SeqCst);
        let a = tlb_plan::user_acked(xa, e, l, s, lazy, g, ask);
        if a && first && lazy && !tlb_plan::user_acked(xa, e, l, s, false, g, ask) {
            skips.set(skips.get() + 1);
        }
        a
    };
    let out = wait_for(c, &|f| x.for_each_cpu(f), &|cpu, first| cpu_tlb(cpu).is_none_or(|t| acked(t, first)));
    x.busy.store(0, SeqCst);
    if skips.get() > 0 {
        C.lazy_skips.fetch_add(skips.get(), Relaxed);
    }
    out
}

/// Before `root` is freed: every CPU that has it loaded (lazily) switches to the kernel root. Confirmed, no
/// CPU can reach the root's tables any more; unconfirmed, the caller must keep the root and its tables.
pub fn leave(root: u64) -> Outcome {
    if !crate::BARE_METAL {
        return Outcome::default();
    }
    let Some(x) = ctx_of(root) else { return Outcome::default() };
    let _irq = IrqGuard::new();
    presize();
    let c = CPUS.get();
    acquire(&x.busy, c);
    x.dying.store(1, SeqCst);
    let g = x.generation.load(SeqCst) + 1;
    x.info.publish(g, (0, 0));
    x.generation.store(g, SeqCst);
    inc(&C.leaves);
    service(c);
    let xa = addr_of(x);
    let out = wait_for(c, &|f| x.for_each_cpu(f), &|cpu, _| {
        cpu_tlb(cpu).is_none_or(|t| tlb_plan::user_acked(xa, t.entering.load(SeqCst), t.loaded.load(SeqCst), 0, false, g, Ask::Leave))
    });
    x.busy.store(0, SeqCst);
    out
}

/// After a change of kernel-half mappings in [start, end) (`full`: or page tables were freed): every online
/// CPU drops the old translations before this returns, unless the outcome says otherwise.
pub fn flush_kernel(start: u64, end: u64, full: bool) -> Outcome {
    if !crate::BARE_METAL {
        return Outcome::default();
    }
    let _irq = IrqGuard::new();
    presize();
    let c = CPUS.get();
    acquire(&KERNEL.busy, c);
    let g = KERNEL.generation.load(SeqCst) + 1;
    KERNEL.info.publish(g, tlb_plan::published_range(start, end, full));
    KERNEL.generation.store(g, SeqCst);
    inc(&C.kernel);
    service(c);
    let out = wait_for(
        c,
        &|f| (0..percpu::cpu_count()).filter(|&cpu| crate::smp::is_online(cpu)).for_each(&mut *f),
        &|cpu, _| cpu_tlb(cpu).is_none_or(|t| t.kernel_synced.load(SeqCst) >= g),
    );
    KERNEL.busy.store(0, SeqCst);
    out
}

/// `flush_kernel` of everything (memory-type changes of the direct map).
pub fn flush_kernel_all() -> Outcome {
    flush_kernel(0, 0, true)
}

// ---------------------------------------------------------------------------------------------------------
// The instructions

#[cfg(target_os = "none")]
mod imp {
    pub fn write_cr3(v: u64) {
        // SAFETY: every caller passes a root whose kernel half maps the running code, stack, IDT and GDT to
        // the same frames (a user root copies the kernel root's half; the kernel and boot roots map the kernel)
        // with a PCID this module chose while CR4.PCIDE is on, or none. Only the TLB and CR3 change.
        unsafe { core::arch::asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags)) };
    }
    pub fn write_cr4(v: u64) {
        // SAFETY: only `enable_pcid` calls it, with the CR4 it just read plus PCIDE while CR3[11:0] is 0 (SDM:
        // otherwise #GP) and interrupts off; no other bit changes.
        unsafe { core::arch::asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags)) };
    }
    pub fn toggle_pge(cr4: u64) {
        // SAFETY: CR4 just read, written without PGE and then unchanged, with interrupts off (the caller's
        // IrqGuard): drops every TLB entry, global ones and all PCIDs included (SDM 4.10.4.1).
        unsafe {
            core::arch::asm!("mov cr4, {off}", "mov cr4, {on}", off = in(reg) cr4 & !crate::cpu::CR4_PGE, on = in(reg) cr4,
                options(nostack, preserves_flags))
        };
    }
    pub fn invpcid_all() {
        let desc = [0u64; 2];
        // SAFETY: INVPCID type 2 (all contexts, global entries included) with a valid 16-byte descriptor on the
        // stack; only called when CPUID reports INVPCID. Drops TLB entries only.
        unsafe { core::arch::asm!("invpcid {t}, [{d}]", t = in(reg) 2u64, d = in(reg) desc.as_ptr(), options(nostack, preserves_flags)) };
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn write_cr3(_v: u64) {}
    pub fn write_cr4(_v: u64) {}
    pub fn toggle_pge(_cr4: u64) {}
    pub fn invpcid_all() {}
}
