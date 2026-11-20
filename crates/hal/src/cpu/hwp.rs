//! Hardware P-states on Intel (CP-265K): the sequence of Linux's intel_pstate in active mode
//! (linux-7.3.1/drivers/cpufreq/intel_pstate.c:2001-2015 `intel_pstate_hwp_enable`, :1223-1283 `intel_pstate_hwp_set`), run by
//! each CPU on itself. Without it a CPU whose firmware leaves IA32_PM_ENABLE clear sits at the firmware's clock; with
//! it the core picks its clock between the two bounds and turbos by itself.
//!
//! Only on GenuineIntel with CPUID `hwp` and `hwp_epp`, never under a hypervisor: AMD and every guest refuse before
//! the first MSR access (`msr::read` and `msr::write` have no fault recovery). Pure over `Msrs`, host-tested with a
//! fake that faults like the CPU; `IA32_PM_ENABLE` is one-way until reset, so a Reload finds it set and writes the
//! same request again.

use super::CpuidView;
use crate::msr::{IA32_HWP_CAPABILITIES, IA32_HWP_INTERRUPT, IA32_HWP_REQUEST, IA32_PM_ENABLE};

/// IA32_HWP_REQUEST energy-performance preference: Linux's generic `balance_performance` (HWP_EPP_BALANCE_PERFORMANCE,
/// linux-7.3.1/arch/x86/include/asm/msr-index.h:593), and `performance` (:592). Models of `epp_values` differ.
pub const EPP_BALANCE_PERFORMANCE: u8 = 0x80;
pub const EPP_PERFORMANCE: u8 = 0x00;

/// GP-CPU2: the user's energy preference (settings key `kernel.cpu_policy`, handed to the kernel as CONFIG
/// `cpu_policy=`): which of Linux's EPP policies every CPU asks for. `balanced` is Linux's default
/// (`balance_performance`), `performance` its sysfs `performance` (energy_performance_preference).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    #[default]
    Balanced,
    Performance,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Policy::Balanced => "balanced",
            Policy::Performance => "performance",
        }
    }
}

/// CONFIG `cpu_policy=` (pure): `balanced` (or missing) and `performance`. Anything else keeps `balanced` and says so
/// (`true`): both values are safe, so a typo costs nothing.
pub fn parse_policy(v: Option<&str>) -> (Policy, bool) {
    match v.map(str::trim) {
        None | Some("") | Some("balanced") => (Policy::Balanced, false),
        Some("performance") => (Policy::Performance, false),
        Some(_) => (Policy::Balanced, true),
    }
}

/// Linux's EPP of the two policies on a model: `intel_epp_default` (linux-7.3.1/drivers/cpufreq/intel_pstate.c:3730-3746,
/// applied at :3888-3899 and :1990-1998), else the generic values. The 265K (Arrow Lake, 6/0xC6): balance_performance
/// 64, performance 16. Linux keeps a firmware's EPP instead only when the firmware switched HWP on itself and left a
/// value in 1..=0x80 (:1980-1985, "HWP enabled by BIOS", :3807-3809); this machine's Linux prints only "HWP enabled",
/// so it runs 64. BitsOS always writes the table's value, whatever the firmware left (CP-265K decision 1).
pub fn epp_values(family: u32, model: u32) -> (u8, u8) {
    match (family, model) {
        // Alder Lake L: "the max suggested EPP which can result in one core turbo frequency".
        (6, 0x9A) => (102, EPP_PERFORMANCE),
        // Sapphire, Emerald and Granite Rapids.
        (6, 0x8F | 0xCF | 0xAD | 0xAE) => (32, EPP_PERFORMANCE),
        // Meteor Lake L and Arrow Lake: HWP_SET_EPP_VALUES(HWP_EPP_POWERSAVE, 179, 64, 16).
        (6, 0xAA | 0xC6) => (64, 16),
        _ => (EPP_BALANCE_PERFORMANCE, EPP_PERFORMANCE),
    }
}

/// The EPP the CPU `v` describes gets under `policy` (pure).
pub fn epp_for(v: &CpuidView, policy: Policy) -> u8 {
    let (family, model, _) = super::decode_fms(v.get(1, 0).eax);
    let (balanced, performance) = epp_values(family, model);
    match policy {
        Policy::Balanced => balanced,
        Policy::Performance => performance,
    }
}

/// The bits of IA32_HWP_REQUEST this keeps from what it finds: the activity window (41:32; 0 = the CPU's choice).
/// Package control (42) and the valid bits (63:59) go, so this CPU's own request is the one that counts.
const REQUEST_KEEP: u64 = 0x3FF << 32;

/// Why a CPU does not get the sequence (no MSR was touched).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotIntel,
    Hypervisor,
    NoHwp,
    NoEpp,
    /// The bring-up has not run the sequence on this CPU yet.
    NotRun,
}

impl Refusal {
    pub fn name(self) -> &'static str {
        match self {
            Refusal::NotIntel => "not_intel",
            Refusal::Hypervisor => "hypervisor",
            Refusal::NoHwp => "no_hwp",
            Refusal::NoEpp => "no_hwp_epp",
            Refusal::NotRun => "not_run",
        }
    }
}

/// Why the sequence stopped. The clocks stay as they are: no request was written, or the CPU did not keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// IA32_PM_ENABLE bit 0 did not stick; the other HWP MSRs would #GP.
    NotEnabled,
    /// IA32_HWP_CAPABILITIES has no usable range (raw value).
    BadCapabilities(u64),
    /// IA32_HWP_REQUEST reads back another value (written, read).
    RequestDiffers(u64, u64),
    /// An access raised #GP (the register): the CPU lacks it or refused the value.
    MsrFault(u32),
}

impl Failure {
    pub fn name(self) -> &'static str {
        match self {
            Failure::NotEnabled => "not_enabled",
            Failure::BadCapabilities(_) => "bad_capabilities",
            Failure::RequestDiffers(..) => "request_differs",
            Failure::MsrFault(_) => "msr_fault",
        }
    }
}

/// What the sequence did on one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Refused(Refusal),
    On {
        /// The firmware (or a Reload's earlier kernel) had switched HWP on already.
        was_on: bool,
        lowest: u8,
        highest: u8,
        request: u64,
    },
    Failed(Failure),
}

impl Default for Outcome {
    fn default() -> Self {
        Outcome::Refused(Refusal::NotRun)
    }
}

impl Outcome {
    pub fn is_on(self) -> bool {
        matches!(self, Outcome::On { .. })
    }

    pub fn name(self) -> &'static str {
        match self {
            Outcome::Refused(r) => r.name(),
            Outcome::On { .. } => "on",
            Outcome::Failed(f) => f.name(),
        }
    }
}

/// MSR access of the CPU the sequence runs on.
pub trait Msrs {
    fn read(&mut self, msr: u32) -> u64;
    fn write(&mut self, msr: u32, value: u64);
}

/// The running CPU's MSRs, fault-safe (GP-CPU): the sequence has never run on a real CPU, and it runs on every CPU
/// during bring-up, where a #GP would end the boot. After the first fault nothing more is touched: reads answer 0,
/// writes do nothing, so the sequence ends without a request (`Stopping`), and `enable_this_cpu` reports the
/// register as `Failure::MsrFault`.
#[derive(Default)]
pub struct LiveMsrs(pub Stopping);

impl Msrs for LiveMsrs {
    fn read(&mut self, msr: u32) -> u64 {
        self.0.read(msr, crate::msr::try_read)
    }
    fn write(&mut self, msr: u32, value: u64) {
        self.0.write(msr, value, crate::msr::try_write)
    }
}

/// The rule of `LiveMsrs` over any fallible access (pure; host-tested): the first fault is kept and ends all access.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stopping {
    pub faulted: Option<u32>,
}

impl Stopping {
    pub fn read(&mut self, msr: u32, f: impl FnOnce(u32) -> Option<u64>) -> u64 {
        if self.faulted.is_some() {
            return 0;
        }
        f(msr).unwrap_or_else(|| {
            self.faulted = Some(msr);
            0
        })
    }
    pub fn write(&mut self, msr: u32, value: u64, f: impl FnOnce(u32, u64) -> bool) {
        if self.faulted.is_none() && !f(msr, value) {
            self.faulted = Some(msr);
        }
    }
}

/// Whether the CPU `v` describes gets the sequence (pure; asks no MSR).
pub fn gate(v: &CpuidView) -> Option<Refusal> {
    let vendor = v.get(0, 0);
    if vendor.ebx != u32::from_le_bytes(*b"Genu") || vendor.ecx != u32::from_le_bytes(*b"ntel") {
        Some(Refusal::NotIntel)
    } else if v.has("hypervisor") {
        Some(Refusal::Hypervisor)
    } else if !v.has("hwp") {
        Some(Refusal::NoHwp)
    } else if !v.has("hwp_epp") {
        Some(Refusal::NoEpp)
    } else {
        None
    }
}

/// IA32_HWP_REQUEST for the range: minimum `lowest`, maximum `highest`, desired 0 (the CPU decides), `epp`
/// (linux-7.3.1/arch/x86/include/asm/msr-index.h:588-597), the activity window from `old`.
pub fn request_value(old: u64, lowest: u8, highest: u8, epp: u8) -> u64 {
    (old & REQUEST_KEEP) | u64::from(lowest) | u64::from(highest) << 8 | u64::from(epp) << 24
}

/// The sequence on the CPU `v` describes and `m` reaches (allocation-free: APs run it before they may take the
/// heap's lock). The order is Linux's: notifications off (IA32_HWP_INTERRUPT may be written with HWP off, SDM Vol. 3B
/// 15.4.2), PM_ENABLE, then the capabilities and the request, which #GP before it.
pub fn enable(v: &CpuidView, m: &mut dyn Msrs) -> Outcome {
    enable_with(v, Policy::Balanced, m)
}

/// `enable` with the user's EPP policy (GP-CPU2).
pub fn enable_with(v: &CpuidView, policy: Policy, m: &mut dyn Msrs) -> Outcome {
    if let Some(r) = gate(v) {
        return Outcome::Refused(r);
    }
    if v.has("hwp_notify") {
        m.write(IA32_HWP_INTERRUPT, 0);
    }
    let was_on = m.read(IA32_PM_ENABLE) & 1 != 0;
    if !was_on {
        m.write(IA32_PM_ENABLE, 1);
        if m.read(IA32_PM_ENABLE) & 1 == 0 {
            return Outcome::Failed(Failure::NotEnabled);
        }
    }
    let caps = m.read(IA32_HWP_CAPABILITIES);
    let (highest, lowest) = (caps as u8, (caps >> 24) as u8);
    if highest == 0 || lowest > highest {
        return Outcome::Failed(Failure::BadCapabilities(caps));
    }
    let request = request_value(m.read(IA32_HWP_REQUEST), lowest, highest, epp_for(v, policy));
    m.write(IA32_HWP_REQUEST, request);
    let back = m.read(IA32_HWP_REQUEST);
    if back != request {
        return Outcome::Failed(Failure::RequestDiffers(request, back));
    }
    Outcome::On { was_on, lowest, highest, request }
}

/// The sequence on the CPU this runs on.
pub fn enable_this_cpu(policy: Policy) -> Outcome {
    let mut m = LiveMsrs::default();
    after_faults(enable_with(&super::live(), policy, &mut m), m.0.faulted)
}

/// What a run means once the accessor saw a fault (pure; host-tested): whatever the sequence concluded from the
/// zeros it was given, the outcome is the fault. A run without one stays as it is.
pub fn after_faults(outcome: Outcome, faulted: Option<u32>) -> Outcome {
    match faulted {
        Some(msr) => Outcome::Failed(Failure::MsrFault(msr)),
        None => outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::super::real_265k;
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A CPU's HWP MSRs: faults (panics) where the real one raises #GP, and records every access.
    #[derive(Default)]
    struct Fake {
        regs: BTreeMap<u32, u64>,
        log: Vec<(char, u32, u64)>,
        /// PM_ENABLE ignores writes.
        stuck_off: bool,
        /// IA32_HWP_REQUEST keeps only these bits.
        request_mask: u64,
    }

    impl Fake {
        fn new(caps: u64) -> Fake {
            let regs = BTreeMap::from([(IA32_PM_ENABLE, 0), (IA32_HWP_CAPABILITIES, caps), (IA32_HWP_REQUEST, 0), (IA32_HWP_INTERRUPT, 0x5)]);
            Fake { regs, request_mask: !0, ..Fake::default() }
        }
        fn check(&self, msr: u32) {
            let on = self.regs.get(&IA32_PM_ENABLE).is_some_and(|x| x & 1 != 0);
            assert!(on || msr == IA32_PM_ENABLE || msr == IA32_HWP_INTERRUPT, "#GP: {msr:#x} before PM_ENABLE");
            assert!(self.regs.contains_key(&msr), "#GP: no MSR {msr:#x}");
        }
        fn writes(&self) -> Vec<(u32, u64)> {
            self.log.iter().filter(|e| e.0 == 'w').map(|e| (e.1, e.2)).collect()
        }
    }

    impl Msrs for Fake {
        fn read(&mut self, msr: u32) -> u64 {
            self.check(msr);
            let x = self.regs[&msr];
            self.log.push(('r', msr, x));
            x
        }
        fn write(&mut self, msr: u32, value: u64) {
            self.check(msr);
            self.log.push(('w', msr, value));
            match msr {
                IA32_PM_ENABLE if self.stuck_off => {}
                IA32_HWP_REQUEST => {
                    self.regs.insert(msr, value & self.request_mask);
                }
                _ => {
                    self.regs.insert(msr, value);
                }
            }
        }
    }

    /// The capabilities of a P core: highest 87, guaranteed 62, efficient and lowest as a 265K P core could report.
    const P_CAPS: u64 = 0x0C_20_3E_57;
    const E_CAPS: u64 = 0x0A_1E_2E_45;

    #[test]
    fn the_order_on_the_real_cpuid() {
        for (table, caps) in [(real_265k::cpu0(), P_CAPS), (real_265k::cpu3(), P_CAPS), (real_265k::cpu8(), E_CAPS), (real_265k::cpu19(), E_CAPS)] {
            let g = table.getter();
            let v = CpuidView::new(&g);
            assert_eq!(gate(&v), None);
            let mut m = Fake::new(caps);
            let out = enable(&v, &mut m);
            let (highest, lowest) = (caps as u8, (caps >> 24) as u8);
            // GP-CPU2: Linux 7.3.1's balance_performance on Arrow Lake is 64 (intel_pstate.c:3743-3744), not 0x80.
            let request = u64::from(lowest) | u64::from(highest) << 8 | 0x40 << 24;
            assert_eq!(out, Outcome::On { was_on: false, lowest, highest, request });
            assert_eq!(m.writes(), vec![(IA32_HWP_INTERRUPT, 0), (IA32_PM_ENABLE, 1), (IA32_HWP_REQUEST, request)], "Linux's order");
            let enable_at = m.log.iter().position(|e| e.0 == 'w' && e.1 == IA32_PM_ENABLE).unwrap();
            let first_caps = m.log.iter().position(|e| e.1 == IA32_HWP_CAPABILITIES).unwrap();
            assert!(enable_at < first_caps, "nothing but PM_ENABLE and the interrupt before the enable");
            assert_eq!(request >> 16 & 0xFF, 0, "desired 0: the CPU decides");
        }
    }

    #[test]
    fn refusals_touch_no_msr() {
        // A fake with no registers at all panics on the first access.
        let refused = |v: &CpuidView| enable(v, &mut Fake::default());
        let mut guest = real_265k::cpu0();
        guest.patch(1, 0, |r| r.ecx |= 1 << 31);
        let g = guest.getter();
        assert_eq!(refused(&CpuidView::new(&g)), Outcome::Refused(Refusal::Hypervisor), "never in a guest, even if it shows hwp");

        let mut no_hwp = real_265k::cpu0();
        no_hwp.patch(6, 0, |r| r.eax &= !(1 << 7));
        let g = no_hwp.getter();
        assert_eq!(refused(&CpuidView::new(&g)), Outcome::Refused(Refusal::NoHwp));

        let mut no_epp = real_265k::cpu0();
        no_epp.patch(6, 0, |r| r.eax &= !(1 << 10));
        let g = no_epp.getter();
        assert_eq!(refused(&CpuidView::new(&g)), Outcome::Refused(Refusal::NoEpp));

        // AMD with every bit set the Intel path looks at: nothing happens.
        let mut amd = real_265k::cpu0();
        amd.patch(0, 0, |r| {
            r.ebx = u32::from_le_bytes(*b"Auth");
            r.edx = u32::from_le_bytes(*b"enti");
            r.ecx = u32::from_le_bytes(*b"cAMD");
        });
        let g = amd.getter();
        assert_eq!(refused(&CpuidView::new(&g)), Outcome::Refused(Refusal::NotIntel));
    }

    #[test]
    fn firmware_left_on_and_a_second_run() {
        let t = real_265k::cpu0();
        let g = t.getter();
        let v = CpuidView::new(&g);
        let mut m = Fake::new(P_CAPS);
        m.regs.insert(IA32_PM_ENABLE, 1);
        // The window stays; package control and the valid bits go; the firmware's old bounds and EPP go.
        m.regs.insert(IA32_HWP_REQUEST, 0xF800_0000_0000_0000 | 1 << 42 | 0x7 << 32 | 0xFF_05_30_01);
        let out = enable(&v, &mut m);
        // GP-CPU2: EPP 0x40, Arrow Lake's balance_performance in Linux 7.3.1.
        let want = 0x7 << 32 | 0x40_00_57_0C;
        assert_eq!(out, Outcome::On { was_on: true, lowest: 0x0C, highest: 0x57, request: want });
        assert!(!m.writes().iter().any(|e| e.0 == IA32_PM_ENABLE), "already on: no second enable");
        // A Reload: the same again, the same answer.
        let again = enable(&v, &mut m);
        assert_eq!(again, out);
    }

    #[test]
    fn a_failure_leaves_the_clocks() {
        let t = real_265k::cpu8();
        let g = t.getter();
        let v = CpuidView::new(&g);

        let mut stuck = Fake::new(E_CAPS);
        stuck.stuck_off = true;
        assert_eq!(enable(&v, &mut stuck), Outcome::Failed(Failure::NotEnabled));
        assert!(!stuck.log.iter().any(|e| e.1 == IA32_HWP_CAPABILITIES || e.1 == IA32_HWP_REQUEST), "the fake would fault");

        for caps in [0u64, 0x60_00_00_10] {
            let mut bad = Fake::new(caps);
            assert_eq!(enable(&v, &mut bad), Outcome::Failed(Failure::BadCapabilities(caps)));
            assert!(!bad.writes().iter().any(|e| e.0 == IA32_HWP_REQUEST), "no request from a bad range");
        }

        let mut ignoring = Fake::new(E_CAPS);
        ignoring.request_mask = 0;
        assert!(matches!(enable(&v, &mut ignoring), Outcome::Failed(Failure::RequestDiffers(_, 0))));
    }

    #[test]
    fn not_run_until_the_bring_up() {
        assert_eq!(Outcome::default().name(), "not_run");
    }
    /// A CPU on which `absent` raises #GP (and every other access works as on `Fake`), reached through `Stopping` as
    /// the live accessor is. Any access after the first fault is a test failure: nothing more may be touched.
    struct Faulty {
        inner: Fake,
        absent: u32,
        stop: Stopping,
        touched_after_fault: bool,
    }

    impl Faulty {
        fn new(absent: u32) -> Faulty {
            Faulty { inner: Fake::new(P_CAPS), absent, stop: Stopping::default(), touched_after_fault: false }
        }
    }

    impl Msrs for Faulty {
        fn read(&mut self, msr: u32) -> u64 {
            let (inner, absent, hit) = (&mut self.inner, self.absent, &mut self.touched_after_fault);
            let was = self.stop.faulted.is_some();
            self.stop.read(msr, |m| {
                *hit |= was;
                (m != absent).then(|| inner.read(m))
            })
        }
        fn write(&mut self, msr: u32, value: u64) {
            let (inner, absent, hit) = (&mut self.inner, self.absent, &mut self.touched_after_fault);
            let was = self.stop.faulted.is_some();
            self.stop.write(msr, value, |m, v| {
                *hit |= was;
                if m == absent {
                    return false;
                }
                inner.write(m, v);
                true
            })
        }
    }

    /// GP-CPU: a register that raises #GP anywhere in the sequence ends it as "failed" with that register named,
    /// nothing is touched after the fault, and no request is left behind.
    #[test]
    fn a_fault_anywhere_fails_cleanly() {
        let d = real_265k::cpu0();
        let g = d.getter();
        let v = CpuidView::new(&g);
        for absent in [IA32_HWP_INTERRUPT, IA32_PM_ENABLE, IA32_HWP_CAPABILITIES, IA32_HWP_REQUEST] {
            let mut m = Faulty::new(absent);
            let o = enable(&v, &mut m);
            let o = after_faults(o, m.stop.faulted);
            assert_eq!(o, Outcome::Failed(Failure::MsrFault(absent)), "absent {absent:#x}");
            assert_eq!(o.name(), "msr_fault");
            assert!(!m.touched_after_fault, "an MSR was touched after {absent:#x} faulted");
            assert!(!m.inner.writes().iter().any(|w| w.0 == IA32_HWP_REQUEST), "a request was written although {absent:#x} faulted");
        }
        // No fault: the outcome is the sequence's own.
        let mut m = Faulty::new(0xFFFF_FFFF);
        let o = enable(&v, &mut m);
        assert!(after_faults(o, m.stop.faulted).is_on());
    }

    #[test]
    fn stopping_keeps_the_first_fault() {
        let mut s = Stopping::default();
        assert_eq!(s.read(1, |_| Some(7)), 7);
        s.write(2, 9, |_, _| true);
        assert_eq!(s.faulted, None);
        assert_eq!(s.read(3, |_| None), 0);
        assert_eq!(s.faulted, Some(3));
        assert_eq!(s.read(4, |_| panic!("read after a fault")), 0);
        s.write(5, 1, |_, _| panic!("write after a fault"));
        assert_eq!(s.faulted, Some(3));
        let mut s = Stopping::default();
        s.write(6, 1, |_, _| false);
        assert_eq!(s.faulted, Some(6));
    }

    /// GP-CPU2: the EPP Linux 7.3.1 writes for each policy, on the 265K's own CPUID (P and E alike), and the generic
    /// values elsewhere; `performance` reaches the request.
    #[test]
    fn epp_by_model_and_policy() {
        for t in [real_265k::cpu0(), real_265k::cpu8()] {
            let g = t.getter();
            let v = CpuidView::new(&g);
            assert_eq!((epp_for(&v, Policy::Balanced), epp_for(&v, Policy::Performance)), (64, 16));
            let mut m = Fake::new(P_CAPS);
            let Outcome::On { request, .. } = enable_with(&v, Policy::Performance, &mut m) else { panic!() };
            assert_eq!(request >> 24 & 0xFF, 16);
        }
        assert_eq!(epp_values(6, 0x97), (0x80, 0), "Alder Lake S: Linux's generic values");
        assert_eq!(epp_values(6, 0x9A), (102, 0));
        assert_eq!(epp_values(6, 0xAA), (64, 16));
        assert_eq!(epp_values(0x19, 0x75), (0x80, 0), "never reached on AMD (gate), and nothing special there either");
    }

    #[test]
    fn policy_config() {
        assert_eq!(parse_policy(None), (Policy::Balanced, false));
        assert_eq!(parse_policy(Some(" performance ")), (Policy::Performance, false));
        assert_eq!(parse_policy(Some("balanced")), (Policy::Balanced, false));
        assert_eq!(parse_policy(Some("turbo")), (Policy::Balanced, true));
    }
}
