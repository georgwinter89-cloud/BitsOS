//! `reboot` and `poweroff` from the console (keyboard only, `physical_only` in the monitor): CP-5's platform
//! functions (IN-3 change request 5 / CP-5 change request 3, applied by INT-A1). Both quiesce every driver first;
//! `reboot` then walks this machine's reboot plan (FADT reset register, 0xCF9, 8042, triple fault) and never
//! returns, `poweroff` enters ACPI S5 and returns only when the machine still runs. The same reboot plan as MC's
//! `reboot` tool (crates/mcp/src/tools/reboot.rs, `acpi::power::reboot`).

use alloc::format;
use alloc::string::String;

/// Never returns: the machine resets (the triple fault is the last step of the plan).
pub fn reboot() -> ! {
    klog::event!(Warn, "console", "reboot.requested"; "typed on the console's keyboard");
    platform::power::reboot()
}

/// Returns only when the machine did not switch off (the drivers stay quiesced).
pub fn poweroff() -> String {
    klog::event!(Warn, "console", "poweroff.requested"; "typed on the console's keyboard");
    let e = platform::power::poweroff();
    klog::event!(Error, "console", "poweroff.failed", error = format!("{e:?}"); "power-off did not happen: hold the power button");
    format!("power-off did not happen ({e:?}): hold the power button; the drivers are stopped")
}
