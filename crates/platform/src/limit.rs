//! CONFIG `smp=` and `cpus=` (CP-4b; pure, host-tested): how many CPUs the bring-up may have online.
//!
//! - `smp=off` keeps the APs offline (one CPU), e.g. so RB-5a's single-CPU Reload works until RB-5b parks APs;
//!   `smp=on` (the default) starts every CPU the MADT lists. A number is no `smp=` value (`smp=1` could mean "on"
//!   or "one CPU"): it is not understood, so one CPU runs and the warning points to `cpus=N`.
//! - `cpus=N` starts at most N CPUs, the BSP included (a debugging limit, like Linux `maxcpus=`); `cpus=0`
//!   means the BSP alone.
//!
//! A value that does not parse never means "all" (spec/core.md §2.6, V1 README): an unknown `smp=` value or a
//! `cpus=` that is not a number gives one CPU, and the bring-up logs why. The possible-CPU count stays the
//! MADT's, so per-CPU containers keep their size (Linux keeps `cpu_possible_mask` under `maxcpus=` too).

use alloc::vec::Vec;

/// What the keys ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limit {
    /// At most this many CPUs online, the BSP included (`None`: every CPU the MADT lists). Never 0.
    pub max_cpus: Option<usize>,
    /// Which key decided `max_cpus` (names the reason in `smp not_started`).
    pub by: By,
}

/// Which key decided the limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum By {
    /// Neither key limits: every CPU the MADT lists.
    #[default]
    Nothing,
    /// `smp=off`.
    SmpOff,
    /// An `smp=` value that is neither on nor off: one CPU.
    SmpBad,
    /// `cpus=N`.
    Cpus,
    /// A `cpus=` value that is not a usable count (not a number, or 0): one CPU.
    CpusBad,
    /// The kernel never handed the CONFIG values over (`acpi::cpu_keys` empty): one CPU.
    KeysMissing,
}

/// A key value that was not understood (logged as a warning by the bring-up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// `smp=` is neither on nor off: one CPU.
    SmpUnknown,
    /// `cpus=` is not a number: one CPU.
    CpusNotANumber,
    /// `cpus=0`: the BSP always runs, so one CPU.
    CpusZero,
}

impl Problem {
    pub fn name(self) -> &'static str {
        match self {
            Problem::SmpUnknown => "smp_unknown",
            Problem::CpusNotANumber => "cpus_not_a_number",
            Problem::CpusZero => "cpus_zero",
        }
    }

    /// What the user probably meant (logged with the warning).
    pub fn hint(self) -> &'static str {
        match self {
            Problem::SmpUnknown => "smp= takes on or off; use cpus=N for a count",
            Problem::CpusNotANumber => "cpus= takes a CPU count such as cpus=4",
            Problem::CpusZero => "the BSP always runs; cpus=1 is the smallest count",
        }
    }
}

/// Reads the two values as CONFIG gives them (`None` = the key is absent; an empty value counts as absent).
pub fn parse(smp: Option<&str>, cpus: Option<&str>) -> (Limit, Vec<Problem>) {
    let mut problems = Vec::new();
    let smp = match smp.map(str::trim).filter(|v| !v.is_empty()) {
        None => None,
        Some(v) if is_any(v, &["on", "yes", "true"]) => None,
        Some(v) if is_any(v, &["off", "no", "false"]) => Some(By::SmpOff),
        Some(_) => {
            problems.push(Problem::SmpUnknown);
            Some(By::SmpBad)
        }
    };
    let from_cpus = match cpus.map(str::trim).filter(|v| !v.is_empty()) {
        None => None,
        Some(v) => match v.parse::<usize>() {
            Ok(0) => {
                problems.push(Problem::CpusZero);
                Some((1, By::CpusBad))
            }
            Ok(n) => Some((n, By::Cpus)),
            Err(_) => {
                problems.push(Problem::CpusNotANumber);
                Some((1, By::CpusBad))
            }
        },
    };
    // smp= wins over cpus=.
    let lim = match (smp, from_cpus) {
        (Some(by), _) => Limit { max_cpus: Some(1), by },
        (None, Some((n, by))) => Limit { max_cpus: Some(n), by },
        (None, None) => Limit::default(),
    };
    (lim, problems)
}

fn is_any(v: &str, words: &[&str]) -> bool {
    words.iter().any(|w| v.eq_ignore_ascii_case(w))
}

impl Limit {
    /// Only the BSP may run: the bring-up does not even prepare the trampoline.
    pub fn bsp_only(&self) -> bool {
        self.max_cpus == Some(1)
    }

    /// The reason text for `smp not_started` when `bsp_only`.
    pub fn reason(&self) -> &'static str {
        match self.by {
            By::SmpOff => "CONFIG smp=off",
            By::SmpBad => "CONFIG smp= not understood: one CPU",
            By::Cpus | By::Nothing => "CONFIG cpus=1",
            By::CpusBad => "CONFIG cpus= not usable: one CPU",
            By::KeysMissing => "CONFIG smp=/cpus= not handed over: one CPU",
        }
    }

    /// The safe answer when the kernel never handed the keys over: one CPU (never "all"), with its own reason.
    pub fn keys_missing() -> Self {
        Limit { max_cpus: Some(1), by: By::KeysMissing }
    }

    /// Whether one more AP may start while `online` CPUs run.
    pub fn allows_another(&self, online: usize) -> bool {
        self.max_cpus.is_none_or(|m| online < m)
    }

    /// The CPUs expected online with `possible` CPUs listed and no AP failing.
    pub fn expected_online(&self, possible: usize) -> usize {
        let p = possible.max(1);
        self.max_cpus.map_or(p, |m| m.min(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn defaults() {
        assert_eq!(parse(None, None), (Limit::default(), vec![]));
        assert_eq!(parse(Some(""), Some("  ")), (Limit::default(), vec![]), "empty values count as absent");
        assert_eq!(parse(Some("on"), None).0.max_cpus, None);
        assert_eq!(parse(Some(" ON "), None).0.max_cpus, None);
        assert!(!Limit::default().bsp_only());
    }

    #[test]
    fn smp_off() {
        for v in ["off", "OFF", " off", "no", "false"] {
            let (l, p) = parse(Some(v), None);
            assert_eq!(l, Limit { max_cpus: Some(1), by: By::SmpOff }, "{v}");
            assert!(p.is_empty());
            assert!(l.bsp_only());
            assert_eq!(l.reason(), "CONFIG smp=off");
        }
        // smp=off wins over cpus=N, and a bad cpus= value does not change the reason.
        assert_eq!(parse(Some("off"), Some("8")).0.max_cpus, Some(1));
        let (l, p) = parse(Some("off"), Some("x"));
        assert_eq!((l.reason(), p), ("CONFIG smp=off", vec![Problem::CpusNotANumber]));
    }

    #[test]
    fn cpus_limit() {
        let (l, p) = parse(None, Some("4"));
        assert_eq!(l, Limit { max_cpus: Some(4), by: By::Cpus });
        assert!(p.is_empty());
        assert!(!l.bsp_only());
        assert_eq!(l.expected_online(16), 4);
        assert_eq!(l.expected_online(2), 2, "a limit above the MADT's count changes nothing");
        assert!(l.allows_another(3));
        assert!(!l.allows_another(4));
        let (l, _) = parse(Some("on"), Some("1"));
        assert!(l.bsp_only());
        assert_eq!(l.reason(), "CONFIG cpus=1");
        assert_eq!(parse(None, Some("100000")).0.max_cpus, Some(100_000), "no maximum (R1)");
    }

    #[test]
    fn bad_values_mean_one_cpu() {
        let bad_smp = Limit { max_cpus: Some(1), by: By::SmpBad };
        assert_eq!(parse(Some("of"), None), (bad_smp, vec![Problem::SmpUnknown]));
        assert_eq!(bad_smp.reason(), "CONFIG smp= not understood: one CPU");
        // A number is no smp= value: smp=1 could mean "on" or "one CPU", so it is the safe one CPU plus a hint.
        for v in ["0", "1", "2", "16"] {
            assert_eq!(parse(Some(v), None), (bad_smp, vec![Problem::SmpUnknown]), "smp={v}");
        }
        assert!(Problem::SmpUnknown.hint().contains("cpus=N"));
        let bad_cpus = Limit { max_cpus: Some(1), by: By::CpusBad };
        assert_eq!(parse(None, Some("all")), (bad_cpus, vec![Problem::CpusNotANumber]));
        assert_eq!(bad_cpus.reason(), "CONFIG cpus= not usable: one CPU");
        assert_eq!(parse(None, Some("-3")).1, vec![Problem::CpusNotANumber]);
        assert_eq!(parse(None, Some("4x")).0.max_cpus, Some(1));
        assert_eq!(parse(None, Some("0")), (bad_cpus, vec![Problem::CpusZero]));
        let (l, p) = parse(Some("maybe"), Some("x"));
        assert_eq!(p, vec![Problem::SmpUnknown, Problem::CpusNotANumber]);
        assert_eq!(l, bad_smp, "smp= decides, so its problem names the reason");
        assert_eq!(parse(Some("of"), Some("0")).0, bad_smp);
        assert_eq!(Problem::CpusZero.name(), "cpus_zero");
    }

    #[test]
    fn no_limit() {
        let l = Limit::default();
        assert!(l.allows_another(1_000));
        assert_eq!(l.expected_online(16), 16);
        assert_eq!(l.expected_online(0), 1);
        let m = Limit::keys_missing();
        assert!(m.bsp_only() && m.reason() == "CONFIG smp=/cpus= not handed over: one CPU");
    }
}
