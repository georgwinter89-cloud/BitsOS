//! GX-7c: the Linux-diff harness. Our bring-up's register writes (a fake's or the bring-up owner's log) against
//! Linux's (`regtrace` of the 27.10. capture), phase by phase (PSP, SMU, IMU/RLC, MES, SDMA): each write Linux
//! makes that we do not (missing), each we make that Linux does not (extra) and each we make in another place
//! (reordered). Pure and self-contained (only `alloc`): the host tool `regtrace diff` includes this file too.
//!
//! A phase picks writes by register name (the names come from the caller: the generated tables in host tests and
//! the tool, so no table is compiled in here) and optionally by a window between two named writes. Values compare
//! by a rule per register: addresses and counters differ by design between two drivers (`Rule::Any`). Known,
//! reasoned differences are `Allow` entries: they stay in the report, marked, and do not fail.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Register names at a BAR5 byte offset (several when tables alias).
pub trait Names {
    fn names(&self, byte: u32) -> &[String];
}

impl Names for BTreeMap<u32, Vec<String>> {
    fn names(&self, byte: u32) -> &[String] {
        self.get(&byte).map_or(&[], |v| v.as_slice())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    Exact,
    /// Any value (addresses, write pointers): only that the register is written counts.
    Any,
    /// Only these bits.
    Mask(u32),
}

pub struct PhaseDef {
    pub name: &'static str,
    /// A write belongs to the phase when one of its names equals or starts with one of these.
    pub prefixes: &'static [&'static str],
    /// Only writes from the first write to `.0` up to and including the next write to `.1`.
    pub window: Option<(&'static str, &'static str)>,
    /// Value rules by register name prefix (first match wins; default `Exact`).
    pub rules: &'static [(&'static str, Rule)],
}

/// The bring-up phases of GC 11.0.1 / PSP 13.0.4 / SMU 13.0.4 (linux-6.6 amdgpu_device_ip_init order: PSP's
/// firmware load, SMU hw_init, GFX hw_init with the RLC autoload wait, MES-KIQ, SDMA). Names are linux-6.6's
/// asic_reg ones (MP1's in mp_13_0_4).
pub const PHASES: &[PhaseDef] = &[
    PhaseDef {
        name: "psp",
        prefixes: &["MP0_SMN_C2PMSG_"],
        window: None,
        // 69/70 hold the ring's address, 67 the write pointer (one write per command).
        rules: &[("MP0_SMN_C2PMSG_69", Rule::Any), ("MP0_SMN_C2PMSG_70", Rule::Any), ("MP0_SMN_C2PMSG_67", Rule::Any)],
    },
    PhaseDef {
        name: "smu",
        // The driver's mailbox only: 67/83/91 are the display's (DC) and never ours.
        prefixes: &["MP1_SMN_C2PMSG_66", "MP1_SMN_C2PMSG_82", "MP1_SMN_C2PMSG_90"],
        window: None,
        rules: &[],
    },
    PhaseDef {
        name: "mes",
        prefixes: &["CP_MES_", "RLC_CP_SCHEDULERS", "CP_HQD_", "CP_MQD_", "GRBM_GFX_CNTL"],
        window: Some(("CP_MES_CNTL", "CP_HQD_ACTIVE")),
        rules: &[("CP_MQD_BASE_ADDR", Rule::Any), ("CP_HQD_PQ_BASE", Rule::Any), ("CP_HQD_PQ_RPTR_REPORT_ADDR", Rule::Any), ("CP_HQD_PQ_WPTR_POLL_ADDR", Rule::Any)],
    },
    PhaseDef {
        name: "imu_rlc",
        prefixes: &["RLC_", "GFX_IMU_"],
        window: None,
        rules: &[("RLC_CSIB_ADDR", Rule::Any)],
    },
    // GX-7i: sdma_v6_0_start (F32 unhalt) through the IB enable; the trap and clock-gating writes after it come
    // with the IH and power gating (later items).
    PhaseDef {
        name: "sdma",
        prefixes: &["SDMA0_"],
        window: Some(("SDMA0_F32_CNTL", "SDMA0_QUEUE0_IB_CNTL")),
        rules: &[("SDMA0_QUEUE0_RB_BASE", Rule::Any), ("SDMA0_QUEUE0_RB_RPTR_ADDR", Rule::Any), ("SDMA0_QUEUE0_RB_WPTR_POLL_ADDR", Rule::Any)],
    },
    // GX-7h: MMHUB's gart_enable (Linux's GMC hw_init, early) through its first VMID 0 flush, then GFXHUB's (with
    // GFX hw_init) through its flush. The table and the two default pages are placed by each driver.
    PhaseDef {
        name: "gmc_mmhub",
        prefixes: &["MMVM_", "MMMC_VM_"],
        window: Some(("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", "MMVM_INVALIDATE_ENG17_SEM")),
        rules: &[("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR", Rule::Any), ("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR", Rule::Any), ("MMVM_L2_PROTECTION_FAULT_DEFAULT_ADDR", Rule::Any)],
    },
    PhaseDef {
        name: "gmc_gfxhub",
        prefixes: &["GCVM_", "GCMC_VM_", "CP_DEBUG"],
        window: Some(("GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", "GCVM_INVALIDATE_ENG17_REQ")),
        rules: &[("GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR", Rule::Any), ("GCMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR", Rule::Any), ("GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR", Rule::Any)],
    },
    // GX-7j: GFX hw_init after gfxhub_enable: the golden registers, constants_init and the RLC resume of the PSP
    // path (gfx_v11_0.c:4308-4330), from the first golden write through SRM's enable. CGTT_WD_CLK_CTRL and TCP_CNTL
    // have no name in gc_11_0_0's tables, so neither side selects them (tests::gfxring checks them by byte).
    PhaseDef {
        name: "gfx_tail",
        prefixes: &[
            "CGTT_GS_NGG_CLK_CTRL",
            "CPF_GCR_CNTL",
            "GL2C_CTRL3",
            "PA_CL_ENHANCE",
            "PA_SC_ENHANCE_3",
            "PA_SC_VRS_SURFACE_CNTL_1",
            "TA_CNTL_AUX",
            "TCP_CNTL2",
            "GRBM_CNTL",
            "GRBM_GFX_INDEX",
            "GRBM_GFX_CNTL",
            "SH_MEM_",
            "SPI_GDBG_PER_VMID_CNTL",
            "GDS_",
            "RLC_CSIB_",
            "RLC_SRM_CNTL",
        ],
        window: Some(("CGTT_GS_NGG_CLK_CTRL", "RLC_SRM_CNTL")),
        rules: &[("RLC_CSIB_ADDR", Rule::Any)],
    },
    // GX-7j: the CP: cp_set_doorbell_range through cp_gfx_start's CP_DEVICE_ID. Linux 7.1 (the capture) maps the
    // GFX queue through MES in between (the `mes` phase); the direct path's CP_RB0/CP_RB_ writes have no capture
    // and are extra by design (linux-6.6 cp_gfx_resume). The CP's unhalt (CP_ME_CNTL) is compared by value apart.
    PhaseDef {
        name: "gfx_ring",
        prefixes: &["CP_RB_DOORBELL_RANGE_", "CP_MEC_DOORBELL_RANGE_", "CP_RB0_", "CP_RB_", "CP_MAX_CONTEXT", "CP_DEVICE_ID"],
        window: Some(("CP_RB_DOORBELL_RANGE_LOWER", "CP_DEVICE_ID")),
        rules: &[("CP_RB0_BASE", Rule::Any), ("CP_RB0_RPTR_ADDR", Rule::Any), ("CP_RB_WPTR_POLL_ADDR", Rule::Any)],
    },
];

/// The display registers outside DCN's own segments, by name (the scanout watcher's list, `watch.rs` has their
/// addresses; a host test keeps both the same): MMHUB fields the display fetch uses, the display's PMFW mailbox.
pub const DISPLAY_NAMES: &[&str] = &[
    "MMMC_VM_FB_OFFSET",
    "MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_LSB",
    "MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_MSB",
    "MMMC_VM_LOCAL_SYSMEM_ADDRESS_START",
    "MMMC_VM_LOCAL_SYSMEM_ADDRESS_END",
    "MMMC_VM_LOCAL_FB_ADDRESS_START",
    "MMMC_VM_LOCAL_FB_ADDRESS_END",
    "MMMC_VM_FB_LOCATION_BASE",
    "MMMC_VM_FB_LOCATION_TOP",
    "MMMC_VM_AGP_TOP",
    "MMMC_VM_AGP_BOT",
    "MMMC_VM_AGP_BASE",
    "MMMC_VM_SYSTEM_APERTURE_LOW_ADDR",
    "MMMC_VM_SYSTEM_APERTURE_HIGH_ADDR",
    "MMMC_VM_MX_L1_TLB_CNTL",
    "MMVM_L2_CNTL",
    "MMVM_L2_CNTL2",
    "MMVM_L2_CNTL3",
    "MMVM_L2_CNTL4",
    "MMVM_L2_CNTL5",
    "MMVM_CONTEXT0_CNTL",
    "MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32",
    "MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32",
    "MP1_SMN_C2PMSG_67",
    "MP1_SMN_C2PMSG_83",
    "MP1_SMN_C2PMSG_91",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    /// Linux writes it, we do not.
    Missing,
    /// We write it, Linux does not.
    Extra,
    /// Both write it, in another order.
    Reordered,
}

/// A known difference: up to `count` diffs of `kind` (any kind when `None`) on the register `name` (prefix).
pub struct Allow {
    pub phase: &'static str,
    pub name: &'static str,
    pub kind: Option<DiffKind>,
    pub count: usize,
    pub why: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub kind: DiffKind,
    pub name: String,
    pub byte: u32,
    /// Linux's value (missing, reordered) or ours (extra).
    pub value: u32,
    /// Position in the phase's own Linux / our sequence.
    pub linux_at: Option<usize>,
    pub ours_at: Option<usize>,
    /// The `Allow` reason when this difference is known.
    pub allowed: Option<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseReport {
    pub name: &'static str,
    pub linux: usize,
    pub ours: usize,
    pub matched: usize,
    pub diffs: Vec<Diff>,
    /// The phase was too long for the exact diff (`LCS_CELLS`): the match is greedy, not the longest.
    pub approximate: bool,
}

impl PhaseReport {
    /// The differences no `Allow` explains.
    pub fn failures(&self) -> impl Iterator<Item = &Diff> {
        self.diffs.iter().filter(|d| d.allowed.is_none())
    }

    pub fn count(&self, kind: DiffKind) -> usize {
        self.diffs.iter().filter(|d| d.kind == kind).count()
    }
}

fn matches_name(names: &[String], p: &str) -> bool {
    names.iter().any(|n| n.starts_with(p))
}

fn label(names: &[String], byte: u32) -> String {
    if names.is_empty() {
        alloc::format!("?{byte:#x}")
    } else {
        names.join("|")
    }
}

/// The phase's writes of `seq` as (position in `seq`, byte, value).
pub fn select(ph: &PhaseDef, seq: &[(u32, u32)], n: &dyn Names) -> Vec<(usize, u32, u32)> {
    let (mut from, mut to) = (0usize, seq.len());
    if let Some((a, b)) = ph.window {
        let start = seq.iter().position(|&(byte, _)| matches_name(n.names(byte), a));
        match start {
            None => return Vec::new(),
            Some(s) => {
                from = s;
                to = seq[s..].iter().position(|&(byte, _)| matches_name(n.names(byte), b)).map_or(seq.len(), |e| s + e + 1);
            }
        }
    }
    seq[from..to]
        .iter()
        .enumerate()
        .filter(|(_, (byte, _))| ph.prefixes.iter().any(|p| matches_name(n.names(*byte), p)))
        .map(|(i, &(b, v))| (from + i, b, v))
        .collect()
}

fn rule(ph: &PhaseDef, names: &[String]) -> Rule {
    ph.rules.iter().find(|(p, _)| matches_name(names, p)).map_or(Rule::Exact, |&(_, r)| r)
}

fn key(ph: &PhaseDef, n: &dyn Names, byte: u32, value: u32) -> (u32, u32) {
    let v = match rule(ph, n.names(byte)) {
        Rule::Exact => value,
        Rule::Any => 0,
        Rule::Mask(m) => value & m,
    };
    (byte, v)
}

/// The most table cells `lcs` allocates (4 bytes each: 64 MiB). The real phases are a few hundred writes; a
/// longer middle (after the common prefix and suffix) falls back to an in-order greedy match.
pub const LCS_CELLS: usize = 16 << 20;

/// Matched pairs (i in a, j in b), increasing in both, and whether they are a longest common subsequence
/// (`false`: the middle was over `cells` and the match is greedy, so some reordered writes may show as
/// missing plus extra).
pub(crate) fn lcs(a: &[(u32, u32)], b: &[(u32, u32)], cells: usize) -> (Vec<(usize, usize)>, bool) {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<(usize, usize)> = (0..pre).map(|i| (i, i)).collect();
    let (n, m) = (ma.len(), mb.len());
    let w = m + 1;
    let fits = (n + 1).checked_mul(w).filter(|&c| c <= cells);
    let exact = n == 0 || m == 0 || fits.is_some();
    if n > 0 && m > 0 {
        match fits {
            Some(c) => {
                let mut t = vec![0u32; c];
                for i in (0..n).rev() {
                    for j in (0..m).rev() {
                        t[i * w + j] = if ma[i] == mb[j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
                    }
                }
                let (mut i, mut j) = (0, 0);
                while i < n && j < m {
                    if ma[i] == mb[j] {
                        out.push((pre + i, pre + j));
                        i += 1;
                        j += 1;
                    } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
                        i += 1;
                    } else {
                        j += 1;
                    }
                }
            }
            None => {
                let mut j0 = 0;
                for (i, x) in ma.iter().enumerate() {
                    if let Some(k) = mb[j0..].iter().position(|y| y == x) {
                        out.push((pre + i, pre + j0 + k));
                        j0 += k + 1;
                    }
                }
            }
        }
    }
    out.extend((0..suf).map(|k| (a.len() - suf + k, b.len() - suf + k)));
    (out, exact)
}

/// One phase: Linux's writes against ours.
pub fn diff_phase(ph: &PhaseDef, linux: &[(u32, u32)], ours: &[(u32, u32)], n: &dyn Names, allow: &[Allow]) -> PhaseReport {
    let l = select(ph, linux, n);
    let o = select(ph, ours, n);
    let lk: Vec<(u32, u32)> = l.iter().map(|&(_, b, v)| key(ph, n, b, v)).collect();
    let ok: Vec<(u32, u32)> = o.iter().map(|&(_, b, v)| key(ph, n, b, v)).collect();
    // No klog here (the host tool includes this file): `approximate` carries it into the report.
    let (pairs, exact) = lcs(&lk, &ok, LCS_CELLS);
    let mut lm = vec![false; l.len()];
    let mut om = vec![false; o.len()];
    for &(i, j) in &pairs {
        lm[i] = true;
        om[j] = true;
    }
    let mut diffs = Vec::new();
    let mut extras: Vec<usize> = (0..o.len()).filter(|&j| !om[j]).collect();
    for i in (0..l.len()).filter(|&i| !lm[i]) {
        let (_, b, v) = l[i];
        let name = label(n.names(b), b);
        // The same write elsewhere in ours: reordered, not missing plus extra.
        if let Some(k) = extras.iter().position(|&j| ok[j] == lk[i]) {
            let j = extras.remove(k);
            diffs.push(Diff { kind: DiffKind::Reordered, name, byte: b, value: v, linux_at: Some(i), ours_at: Some(j), allowed: None });
        } else {
            diffs.push(Diff { kind: DiffKind::Missing, name, byte: b, value: v, linux_at: Some(i), ours_at: None, allowed: None });
        }
    }
    for j in extras {
        let (_, b, v) = o[j];
        diffs.push(Diff { kind: DiffKind::Extra, name: label(n.names(b), b), byte: b, value: v, linux_at: None, ours_at: Some(j), allowed: None });
    }
    let mut left: Vec<usize> = allow.iter().map(|a| a.count).collect();
    for d in &mut diffs {
        for (k, a) in allow.iter().enumerate() {
            if a.phase == ph.name && left[k] > 0 && a.kind.is_none_or(|x| x == d.kind) && d.name.split('|').any(|x| x.starts_with(a.name)) {
                left[k] -= 1;
                d.allowed = Some(a.why);
                break;
            }
        }
    }
    PhaseReport { name: ph.name, linux: l.len(), ours: o.len(), matched: pairs.len(), diffs, approximate: !exact }
}

/// Every phase of `phases`.
pub fn diff(phases: &[PhaseDef], linux: &[(u32, u32)], ours: &[(u32, u32)], n: &dyn Names, allow: &[Allow]) -> Vec<PhaseReport> {
    phases.iter().map(|ph| diff_phase(ph, linux, ours, n, allow)).collect()
}

/// The report as text, one line per phase and one per difference.
pub fn render(reports: &[PhaseReport]) -> String {
    use core::fmt::Write as _;
    let mut s = String::new();
    for r in reports {
        let fails = r.failures().count();
        let _ = writeln!(
            s,
            "phase {}: linux {} ours {} matched {} missing {} extra {} reordered {} unexplained {fails}{}",
            r.name,
            r.linux,
            r.ours,
            r.matched,
            r.count(DiffKind::Missing),
            r.count(DiffKind::Extra),
            r.count(DiffKind::Reordered),
            if r.approximate { " (approximate: too long for the exact diff)" } else { "" }
        );
        for d in &r.diffs {
            let at = |x: Option<usize>| x.map_or(String::from("-"), |i| alloc::format!("{i}"));
            let _ = writeln!(
                s,
                "  {:?} {} @{:#07x} = {:#010x} linux#{} ours#{}{}",
                d.kind,
                d.name,
                d.byte,
                d.value,
                at(d.linux_at),
                at(d.ours_at),
                d.allowed.map_or(String::new(), |w| alloc::format!("  (allowed: {w})"))
            );
        }
    }
    s
}
