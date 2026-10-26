//! PC-5 on the machine: `PciFunction::aspm_l12_off` around `aspm::l12_off`, with `pci.enable` held (the
//! helper and `restore_config` both write Link Control), the per-function record and the events (B6).

use crate::aspm::{self, AspmError, L12Report, Method};
use crate::device::{self, Locked, STATE};
use crate::{Loc, PciFunction, NODES};
use alloc::format;

impl PciFunction {
    /// Switches L1.2 off on the link above this function (ASPM L1.2 and PCI-PM L1.2 in the L1 PM Substates
    /// capability of the device's function 0 and of its port; without ECAM ASPM L1 off in Link Control, V1's
    /// way). L0s, L1 and L1.1 stay as they were. Idempotent; writes only bits that are on. Refused for an
    /// untouched function or port (R8d) and for mass storage unless the NVMe driver is bound (R8a).
    /// `enable_device` does this by itself for the I226 family (`aspm::L12_OFF`). May sleep (bounded wait for
    /// `pci.enable`); never call it with a spin lock held.
    pub fn aspm_l12_off(&self) -> Result<L12Report, AspmError> {
        let f = format!("{}", self.loc());
        let _serial = device::lock_enable(&f).map_err(|_| AspmError::Busy)?;
        l12_off_locked(self.loc())
    }
}

/// `aspm_l12_off` with `pci.enable` already held (`enable_device` calls it for the quirk list).
pub(crate) fn l12_off_locked(at: Loc) -> Result<L12Report, AspmError> {
    let fname = format!("{at}");
    let may_storage = device::driver_of_loc(at) == Some(crate::optin::STORAGE_DRIVER);
    let nodes = NODES.lock().clone();
    let Some(idx) = nodes.iter().position(|n| n.loc == at) else {
        klog::event!(Warn, "pci", "aspm.refused", function = fname.as_str(), reason = "not_scanned");
        return Err(AspmError::NotScanned);
    };
    let r = match aspm::l12_off(&Locked, &nodes, idx, may_storage) {
        Ok(r) => r,
        Err(e) => {
            let reason = format!("{e}");
            match e {
                AspmError::Gone(_) => klog::event!(Error, "pci", "aspm.failed", function = fname.as_str(), reason = reason.as_str()),
                _ => klog::event!(Warn, "pci", "aspm.refused", function = fname.as_str(), reason = reason.as_str()),
            }
            return Err(e);
        }
    };
    let (dev, port) = (format!("{}", r.device), format!("{}", r.port));
    match r.method {
        Method::NotSupported => {
            klog::event!(Info, "pci", "aspm.l1_2_unsupported", function = fname.as_str(), device = dev.as_str(), port = port.as_str();
                "an end of the link has no L1 PM Substates capability: the link never enters L1.2")
        }
        Method::L1ss | Method::AspmL1Off => {
            if r.method == Method::AspmL1Off {
                klog::event!(Warn, "pci", "aspm.fallback", function = fname.as_str(), device = dev.as_str(), port = port.as_str(), method = r.method.name();
                    "the L1 PM Substates capability is out of reach (no ECAM): ASPM L1 off on the whole link instead");
            }
            for e in &r.held {
                let at = format!("{}", e.at);
                klog::event!(Debug, "pci", "aspm.l1_held", function = at.as_str(), device = fname.as_str(), offset = klog::Hex(e.offset as u64),
                    link_ctl = klog::Hex(e.before as u64); "ASPM L1 off while the L1SS enables changed, put back afterwards");
            }
            for e in &r.ends {
                let level_changed = e.changed();
                let at = format!("{}", e.at);
                if level_changed {
                    klog::event!(Info, "pci", "aspm.l1_2_off", function = at.as_str(), device = fname.as_str(), method = r.method.name(),
                        offset = klog::Hex(e.offset as u64), before = klog::Hex(e.before as u64), after = klog::Hex(e.after as u64));
                } else {
                    klog::event!(Debug, "pci", "aspm.l1_2_unchanged", function = at.as_str(), device = fname.as_str(), method = r.method.name(),
                        offset = klog::Hex(e.offset as u64), value = klog::Hex(e.after as u64));
                }
            }
            if !r.verified {
                klog::event!(Error, "pci", "aspm.not_cleared", function = fname.as_str(), device = dev.as_str(), port = port.as_str(), method = r.method.name();
                    "an enable bit reads back as on after the write");
            }
            if r.method == Method::AspmL1Off {
                // The saved copies `restore_config` writes back must not switch ASPM L1 on again.
                let mut st = STATE.lock();
                for e in r.ends.iter().filter(|e| e.changed()) {
                    if let Some(p) = st.get_mut(&e.at).and_then(|s| s.saved.as_mut()).and_then(|s| s.pcie.as_mut())
                        && p.link_ctl.is_some()
                    {
                        p.link_ctl = Some(e.after as u16);
                    }
                }
            }
        }
    }
    Ok(r)
}
