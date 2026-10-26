//! The rule table: one set per program, a default set for programs without one, and the fast path.
//!
//! Fast path: `MASK` has a bit per access class that is set while any set (the default included) can decide
//! something other than an unconditional allow for that class. With the bit clear (today, and whenever only
//! other classes have rules) a check is one atomic load: no lock, no lookup, and vfs need not build the path
//! (`allows_any`). With the bit set the program's set is looked up under a read lock; a process may keep a
//! `Cache` of its program's unconditional decisions, valid while the table generation is unchanged.

use crate::rules::{Decision, Learned, RuleSet};
use crate::{AccessClass, ProgramId, CLASSES};
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use ksync::RwSpinLock;

struct Table {
    sets: BTreeMap<ProgramId, RuleSet>,
    /// For foreign programs without a set of their own; None = allow all (B3 default).
    default: Option<RuleSet>,
    /// "Always" answers per program, kept apart from the set so a program without a set of its own keeps
    /// following the default (a copy would freeze it). A matching learned rule decides before the set (Georg's
    /// latest word on exactly that access). `set_rules` of the program replaces them too (PM-3 keeps learned
    /// answers as settings and hands them back with the set); `set_default_rules` leaves them.
    learned: BTreeMap<ProgramId, RuleSet>,
}

static TABLE: RwSpinLock<Table> = RwSpinLock::named("policy.table", Table { sets: BTreeMap::new(), default: None, learned: BTreeMap::new() });
static GEN: AtomicU64 = AtomicU64::new(1);
static MASK: AtomicU8 = AtomicU8::new(0);

pub fn generation() -> u64 {
    GEN.load(Ordering::Acquire)
}

/// True: every access of class `c` by every program is allowed, whatever its details.
#[inline]
pub(crate) fn all_allow(c: AccessClass) -> bool {
    MASK.load(Ordering::Acquire) & (1 << c as u8) == 0
}

fn conditional_mask(s: &RuleSet) -> u8 {
    let mut m = 0;
    for c in crate::ALL_CLASSES {
        if s.unconditional(c) != Some(Decision::Allow) {
            m |= 1 << c as u8;
        }
    }
    m
}

/// Classes in which learned rules exist (their defaults never decide).
fn learned_mask(s: &RuleSet) -> u8 {
    let mut m = 0;
    for c in crate::ALL_CLASSES {
        if s.unconditional(c).is_none() {
            m |= 1 << c as u8;
        }
    }
    m
}

/// Called with the write lock held: recompute the mask and publish a new generation.
fn republish(t: &Table) {
    let mut m = t.default.as_ref().map_or(0, conditional_mask);
    for s in t.sets.values() {
        m |= conditional_mask(s);
    }
    for s in t.learned.values() {
        m |= learned_mask(s);
    }
    MASK.store(m, Ordering::Release);
    GEN.fetch_add(1, Ordering::AcqRel);
}

/// The set that applies to `program`: its own, else (BitsOS itself: allow all) the default.
fn set_of(t: &Table, program: ProgramId) -> Option<&RuleSet> {
    match t.sets.get(&program) {
        Some(s) => Some(s),
        None if program == ProgramId::SYSTEM => None,
        None => t.default.as_ref(),
    }
}

pub fn rules_of(program: ProgramId) -> RuleSet {
    set_of(&TABLE.read(), program).cloned().unwrap_or_default()
}

/// The program's "always" answers (only the rule lists are meaningful), for PM-3 to show and store.
pub fn learned_rules(program: ProgramId) -> RuleSet {
    TABLE.read().learned.get(&program).cloned().unwrap_or_default()
}

/// Unconditional decision of class `c` for `program` (None: depends on the access), learned rules included.
fn unconditional_in(t: &Table, program: ProgramId, c: AccessClass) -> Option<Decision> {
    if t.learned.get(&program).is_some_and(|l| l.unconditional(c).is_none()) {
        return None;
    }
    set_of(t, program).map_or(Some(Decision::Allow), |s| s.unconditional(c))
}

/// A decision with what an ask becomes at its deadline, and that deadline.
pub(crate) struct Verdict {
    pub decision: Decision,
    pub on_timeout: Decision,
    pub timeout_ns: Option<u64>,
}

/// Decided under the read lock (no Arc copied, nothing allocated); an ask waits after the lock is gone.
pub(crate) fn decide(program: ProgramId, a: &crate::Access<'_>) -> Verdict {
    let t = TABLE.read();
    let mut v = match set_of(&t, program) {
        Some(s) => Verdict { decision: s.decide(a), on_timeout: s.on_timeout(a.class()), timeout_ns: s.ask_timeout_ns },
        None => Verdict { decision: Decision::Allow, on_timeout: Decision::Allow, timeout_ns: None },
    };
    if let Some(d) = t.learned.get(&program).and_then(|l| l.matched(a)) {
        v.decision = d;
    }
    v
}

pub(crate) fn unconditional(program: ProgramId, c: AccessClass) -> Option<Decision> {
    unconditional_in(&TABLE.read(), program, c)
}

/// Installs (Some) or removes (None: back to the default) the rules of one program.
pub fn set_rules(program: ProgramId, rules: Option<RuleSet>) {
    let generation_now = {
        let mut t = TABLE.write();
        match rules {
            Some(r) => t.sets.insert(program, r),
            None => t.sets.remove(&program),
        };
        t.learned.remove(&program);
        republish(&t);
        generation()
    };
    klog::event!(Info, "policy", "rules.set", program = program.0, generation = generation_now; "rules of a program changed");
}

/// The rules for foreign programs without rules of their own (None: allow all).
pub fn set_default_rules(rules: Option<RuleSet>) {
    let generation_now = {
        let mut t = TABLE.write();
        t.default = rules;
        republish(&t);
        generation()
    };
    klog::event!(Info, "policy", "rules.default", generation = generation_now; "default rules changed");
}

/// An "always" answer: the learned rule joins the program's learned rules (an identical one is replaced, so
/// two waiters answered together add it once).
pub(crate) fn learn(program: ProgramId, l: Learned) {
    let mut t = TABLE.write();
    l.add_to(t.learned.entry(program).or_default());
    republish(&t);
}

// ---------------------------------------------------------------------------------------------------------

/// A process's cache of its program's unconditional decisions (no lock on a hit). Embed one per process and
/// pass it to `check_cached`; a rule change (new generation) makes it look the set up again once.
///
/// One atomic word holds generation tag, program and decisions, so threads of one process refilling at the
/// same time can only leave a word that is consistent in itself; an older one fails the generation compare and
/// is refilled. The generation is read under the table's read lock, which rule changes exclude.
pub struct Cache {
    /// Bits 0..12: 2 bits per class (0 conditional, 1 allow, 2 ask, 3 deny); 12..44 program; 44..64 the
    /// generation tag (0: never filled).
    word: AtomicU64,
}

const GEN_SHIFT: u32 = 44;

/// Generation tag in 1..2^20. A stale word could only match again after 2^20 rule changes between one
/// thread's refill and its store (each change takes the write lock), which is not a real interleaving.
fn tag(g: u64) -> u64 {
    (g & 0xF_FFFF).max(1)
}

impl Default for Cache {
    fn default() -> Self {
        Cache::new()
    }
}

impl Cache {
    pub const fn new() -> Self {
        Cache { word: AtomicU64::new(0) }
    }

    /// Some(decision) when it holds for every access of class `c` by `program`.
    pub fn unconditional(&self, program: ProgramId, c: AccessClass) -> Option<Decision> {
        let mut w = self.word.load(Ordering::Acquire);
        if w >> GEN_SHIFT != tag(generation()) || (w >> 12) as u32 != program.0 {
            w = self.refill(program);
        }
        match (w >> (2 * c as u32)) & 3 {
            1 => Some(Decision::Allow),
            2 => Some(Decision::Ask),
            3 => Some(Decision::Deny),
            _ => None,
        }
    }

    fn refill(&self, program: ProgramId) -> u64 {
        let mut packed = 0u64;
        let t = TABLE.read();
        let g = generation();
        for c in crate::ALL_CLASSES {
            let v = match unconditional_in(&t, program, c) {
                None => 0,
                Some(Decision::Allow) => 1,
                Some(Decision::Ask) => 2,
                Some(Decision::Deny) => 3,
            };
            packed |= v << (2 * c as u32);
        }
        drop(t);
        let w = tag(g) << GEN_SHIFT | (program.0 as u64) << 12 | packed;
        self.word.store(w, Ordering::Release);
        w
    }

    /// The generation tag the cache was filled at (0: never filled); compare with `generation_tag`.
    pub fn generation(&self) -> u64 {
        self.word.load(Ordering::Relaxed) >> GEN_SHIFT
    }
}

/// The table generation as a `Cache` keeps it.
pub fn generation_tag() -> u64 {
    tag(generation())
}

const _: () = assert!(CLASSES * 2 <= 12);
