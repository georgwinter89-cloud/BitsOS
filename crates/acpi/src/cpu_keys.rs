//! CP's CONFIG values that CP's initcalls read later (CP-5's QEMU power test, GP-CPU2's energy preference for
//! `platform`'s `cpu.smp`). The kernel hands CP its CONFIG getter once, at `irq::init` (before the initcalls); an
//! initcall has no getter of its own, so the raw values are kept here.

use alloc::string::String;
use ksync::Once;

/// The raw values (`None`: the key is absent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuKeys {
    /// CP-5's QEMU scenario (`host/tests/cp/60-power.sh`): `poweroff` or `reboot` makes the QemuOnly ktest
    /// `cp.power.qemu_action` do it. Absent everywhere else.
    pub power_test: Option<String>,
    /// GP-CPU2: `cpu_policy=balanced|performance`, the settings key `kernel.cpu_policy` as settingsd hands it over
    /// (meaning and checks in `hal::cpu::hwp::parse_policy`).
    pub cpu_policy: Option<String>,
}

static KEYS: Once<CpuKeys> = Once::new();

/// Keeps the values (first call wins; `irq::init` calls it).
pub(crate) fn capture(config: &dyn Fn(&str) -> Option<String>) {
    KEYS.get_or_init(|| CpuKeys { power_test: config("cp_power_test"), cpu_policy: config("cpu_policy") });
}

/// The kept values (`None` before `irq::init` ran).
pub fn cpu_keys() -> Option<&'static CpuKeys> {
    KEYS.get()
}
