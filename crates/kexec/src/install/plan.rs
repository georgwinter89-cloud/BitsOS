//! The generation arithmetic of `install_kernel` and of the kernel's own boot confirmation (pure; host-tested):
//! which number a new generation gets, which unconfirmed generation it replaces, what pruning removes, which
//! markers are tidied, and what bitsboot will start next (its own `choose`, from `bitsboot_pure`).
//!
//! Rules (V2-PLAN §7, B1; the stick tool BT-3 prunes the same way):
//! - The newest generation without `OK` (NEW, TRY, both, or no marker: an interrupted write) is replaced: it is
//!   removed before the new one is written. The new one always gets a number above every existing folder, never
//!   the replaced one's (decision, RB-6 notes: a kernel waiting to confirm generation n must never confirm
//!   different files that later took the number n).
//! - Pruning keeps the newest generation, the newest `sys_keep` confirmed ones and the pinned one; every other
//!   numbered generation goes (an older unconfirmed one is never a fallback).
//! - `OK` generations lose stale `TRY`/`NEW` markers (a confirmation cut between its steps).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bitsboot_pure::gens::{self, Choice, GenDir, Pin};

/// The largest generation number the stick's rules allow (`fs_fat::sysapi`).
pub const NUMBER_MAX: u32 = 99_999_999;

/// One generation as the stick lists it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gen {
    pub n: u32,
    pub new: bool,
    pub tried: bool,
    pub ok: bool,
    pub kernel: bool,
    pub initrd: bool,
    /// Bytes its files take (for the space estimate).
    pub bytes: u64,
}

impl Gen {
    pub fn dir(&self) -> GenDir {
        GenDir { number: self.n, kernel: self.kernel, initrd: self.initrd, new: self.new, tried: self.tried, ok: self.ok }
    }
    pub fn markers(&self) -> String {
        self.dir().markers()
    }
}

/// A marker to clear on a confirmed generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    Try,
    New,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallPlan {
    /// The new generation's number.
    pub number: u32,
    /// The unconfirmed newest generation removed first.
    pub replaced: Option<u32>,
    /// Removed by pruning (`OK` cleared first), oldest first.
    pub prune: Vec<u32>,
    /// Stale markers on kept confirmed generations.
    pub tidy: Vec<(u32, Stale)>,
    /// The newest confirmed generation that stays: where bitsboot falls back to (`KERNEL.OLD`).
    pub fallback: Option<u32>,
    /// A valid `PIN` that will keep starting instead of the new generation (the install was not asked to unpin).
    pub pinned_instead: Option<u32>,
    /// Bytes freed by the replacement and the pruning (their files).
    pub freed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// Every number up to `NUMBER_MAX` is taken (a folder of that number exists).
    NoNumberLeft,
    /// No confirmed generation would stay as a fallback (the caller can allow it).
    NoFallback,
}

impl PlanError {
    pub fn describe(&self) -> String {
        match self {
            PlanError::NoNumberLeft => format!("no generation number left: a folder \\BITSOS2\\SYS\\{NUMBER_MAX} exists"),
            PlanError::NoFallback => String::from(
                "no confirmed (OK) generation on the stick would stay as the fallback: if the new kernel failed, bitsboot \
                 would start it again and again, and only a plug-over could help. allow_no_fallback=true installs anyway",
            ),
        }
    }
}

/// BT-3's pruning rule: everything except the newest, the newest `keep` confirmed and the pinned generation.
pub fn prune(gens: &[Gen], keep: u32, pin: Option<u32>) -> Vec<u32> {
    let keep = keep.max(1) as usize;
    let newest = gens.iter().map(|g| g.n).max();
    let mut oks: Vec<u32> = gens.iter().filter(|g| g.ok).map(|g| g.n).collect();
    oks.sort_unstable_by(|a, b| b.cmp(a));
    let kept: Vec<u32> = oks.into_iter().take(keep).chain(newest).chain(pin).collect();
    let mut out: Vec<u32> = gens.iter().map(|g| g.n).filter(|n| !kept.contains(n)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Stale `TRY`/`NEW` on confirmed generations that stay.
pub fn tidy(gens: &[Gen], gone: &[u32]) -> Vec<(u32, Stale)> {
    let mut out = Vec::new();
    for g in gens.iter().filter(|g| g.ok && !gone.contains(&g.n)) {
        if g.tried {
            out.push((g.n, Stale::Try));
        }
        if g.new {
            out.push((g.n, Stale::New));
        }
    }
    out
}

/// A `PIN` bitsboot honours: it names a confirmed generation with both files.
pub fn valid_pin(gens: &[Gen], pin: Option<u32>) -> Option<u32> {
    let p = pin?;
    gens.iter().any(|g| g.n == p && g.ok && g.kernel && g.initrd).then_some(p)
}

/// The plan for one install. `unpin`: the caller removes the pin with this install.
pub fn install(gens: &[Gen], pin: Option<u32>, keep: u32, unpin: bool, allow_no_fallback: bool) -> Result<InstallPlan, PlanError> {
    let newest = gens.iter().max_by_key(|g| g.n);
    let replaced = newest.filter(|g| !g.ok).map(|g| g.n);
    let top = newest.map_or(0, |g| g.n);
    if top >= NUMBER_MAX {
        return Err(PlanError::NoNumberLeft);
    }
    let number = top + 1;
    // The stick after the replacement, with the new generation (unconfirmed, newest).
    let mut after: Vec<Gen> = gens.iter().filter(|g| Some(g.n) != replaced).copied().collect();
    after.push(Gen { n: number, new: true, kernel: true, initrd: true, ..Gen::default() });
    let kept_pin = if unpin { None } else { pin };
    let gone = prune(&after, keep, kept_pin);
    let fallback = after.iter().filter(|g| g.ok && g.kernel && g.initrd && !gone.contains(&g.n)).map(|g| g.n).max();
    if fallback.is_none() && !allow_no_fallback {
        return Err(PlanError::NoFallback);
    }
    let freed = gens.iter().filter(|g| Some(g.n) == replaced || gone.contains(&g.n)).map(|g| g.bytes).sum();
    let pinned_instead = if unpin { None } else { valid_pin(gens, pin) };
    Ok(InstallPlan { number, replaced, tidy: tidy(&after, &gone), prune: gone, fallback, pinned_instead, freed })
}

/// What the kernel's own confirmation of generation `n` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmPlan {
    /// The folder is gone (replaced by an install in this boot): nothing to confirm.
    Missing,
    /// `OK` is there already (a confirmation cut before its last step, or confirmed elsewhere): tidy and prune.
    AlreadyOk { prune: Vec<u32>, tidy: Vec<(u32, Stale)> },
    /// Write `OK`, then clear `TRY` and `NEW` (OK beats TRY beats NEW, so every cut is safe), tidy, prune.
    Confirm { prune: Vec<u32>, tidy: Vec<(u32, Stale)> },
}

pub fn confirm(gens: &[Gen], n: u32, pin: Option<u32>, keep: u32) -> ConfirmPlan {
    let Some(g) = gens.iter().find(|g| g.n == n) else { return ConfirmPlan::Missing };
    let was_ok = g.ok;
    let after: Vec<Gen> = gens.iter().map(|x| if x.n == n { Gen { ok: true, new: false, tried: false, ..*x } } else { *x }).collect();
    let gone = prune(&after, keep, pin);
    let tidy = tidy(&after, &gone);
    if was_ok { ConfirmPlan::AlreadyOk { prune: gone, tidy } } else { ConfirmPlan::Confirm { prune: gone, tidy } }
}

/// What bitsboot starts at the next boot (files assumed good when present): `(number, reason, why)`, or the
/// refusal text.
pub fn next_boot(gens: &[Gen], pin: Option<u32>) -> Result<(u32, &'static str, String), String> {
    let dirs: Vec<GenDir> = gens.iter().map(Gen::dir).collect();
    let pin = pin.map_or(Pin::None, Pin::Number);
    match gens::choose(&dirs, pin, &[], &[]) {
        Choice::Start { record, why, .. } => Ok((record.number, gens::reason_name(record.reason), why)),
        Choice::Refuse(t) => Err(t),
    }
}
