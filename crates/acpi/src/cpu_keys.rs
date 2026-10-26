//! CP's CONFIG keys that CP's initcalls read later (CP-4b: `smp=`, `cpus=` for `platform`'s `cpu.smp`). The
//! kernel hands CP its CONFIG getter once, at `irq::init` (before the initcalls); an initcall has no getter of its
//! own, so the raw values are kept here. Meaning and checks live with the user (`platform::limit`).

use alloc::string::String;
use ksync::Once;

/// The raw values (`None`: the key is absent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuKeys {
    pub smp: Option<String>,
    pub cpus: Option<String>,
    /// CP-5's QEMU scenario (`host/tests/cp/60-power.sh`): `poweroff` or `reboot` makes the QemuOnly ktest
    /// `cp.power.qemu_action` do it. Absent everywhere else.
    pub power_test: Option<String>,
}

static KEYS: Once<CpuKeys> = Once::new();

/// Keeps the values (first call wins; `irq::init` calls it).
pub(crate) fn capture(config: &dyn Fn(&str) -> Option<String>) {
    KEYS.get_or_init(|| CpuKeys { smp: config("smp"), cpus: config("cpus"), power_test: config("cp_power_test") });
}

/// The kept values (`None` before `irq::init` ran).
pub fn cpu_keys() -> Option<&'static CpuKeys> {
    KEYS.get()
}
