//! Alternate settings (USB 2.0 9.4.10 SET_INTERFACE) and isochronous pipes on them (SN-8a). Audio streaming
//! interfaces keep their isochronous endpoints in alternate setting 1 and up; alternate setting 0 has none (no
//! bandwidth while idle). Order as Linux `usb_set_interface`: the host's endpoints first (the controller may
//! refuse the bandwidth; the device then never hears of it), then the request; if the device refuses, the host
//! goes back to the old endpoints.

use crate::desc::{Endpoint, EpKind};
use crate::device::Device;
use crate::host::{Data, UsbError};
use crate::iso::{service_interval_ns, IsoOpts, IsoPipe};
use crate::setup::Setup;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicUsize;
use ksync::{Mutex, SpinLock};

/// Longest wait for another alternate-setting change of the same device (B4: no unbounded wait).
const ALT_GATE_NS: u64 = 15_000_000_000;

/// The alternate setting of each interface that was changed from its default (0), and the lock that keeps two
/// changes of one device from interleaving.
pub(crate) struct Alts {
    cur: SpinLock<AltState>,
    op: Mutex<()>,
}

#[derive(Clone, Default)]
struct AltState {
    /// (interface, alternate setting the device runs), for settings other than 0.
    set: Vec<(u8, u8)>,
    /// (interface, alternate setting whose endpoints the host has) where that differs from the device's: the
    /// device refused SET_INTERFACE and going back on the host failed too. Such an interface offers no endpoints
    /// until a `set_interface` succeeds.
    broken: Vec<(u8, u8)>,
}

impl Default for Alts {
    fn default() -> Self {
        Alts { cur: SpinLock::named("usb.alts", AltState::default()), op: Mutex::named("usb.alt_op", ()) }
    }
}

impl Device {
    /// The alternate setting interface `number` runs (0 unless `set_interface` changed it); `None` if the
    /// configuration has no such interface.
    pub fn current_alt(&self, number: u8) -> Option<u8> {
        let cfg = self.config.as_ref()?;
        cfg.interfaces.iter().any(|f| f.number == number).then_some(())?;
        Some(self.alts.cur.lock().set.iter().find(|x| x.0 == number).map_or(0, |x| x.1))
    }

    /// The endpoint descriptor of `address` among the interfaces in their current alternate settings (none of an
    /// interface whose host and device settings disagree after a failed change).
    pub fn endpoint(&self, address: u8) -> Option<Endpoint> {
        let cfg = self.config.as_ref()?;
        let cur = self.alts.cur.lock().clone();
        let alt_of = |n: u8| cur.set.iter().find(|x| x.0 == n).map_or(0, |x| x.1);
        cfg.interfaces.iter().filter(|f| f.alt == alt_of(f.number) && !cur.broken.iter().any(|b| b.0 == f.number))
            .flat_map(|f| f.endpoints.iter()).find(|e| e.address == address).copied()
    }

    /// Switches interface `number` to alternate setting `alt`: the old setting's endpoints are dropped on the
    /// host (what was queued on them ends, an isochronous pipe on them is over), the new ones set up, then
    /// SET_INTERFACE. Sleeps (commands, a control transfer): not from a completion callback.
    pub fn set_interface(&self, number: u8, alt: u8) -> Result<(), UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        let cfg = self.config.as_ref().ok_or(UsbError::Invalid("device has no configuration"))?;
        let new = cfg.interface(number, alt).ok_or(UsbError::Invalid("no such alternate setting"))?;
        // Another change of this device runs: at most two commands and a control transfer (5 s) each.
        let _g = self.alts.op.lock_timeout(ALT_GATE_NS).ok_or(UsbError::Busy)?;
        let old_alt = self.current_alt(number).unwrap_or(0);
        // The setting whose endpoints the host has: the device's, unless an earlier change left them apart.
        let host_alt = self.alts.cur.lock().broken.iter().find(|b| b.0 == number).map_or(old_alt, |b| b.1);
        let old: &[Endpoint] = cfg.interface(number, host_alt).map_or(&[], |f| &f.endpoints);
        self.host.set_interface(self.slot, old, &new.endpoints)?;
        if let Err(e) = self.control(Setup::set_interface(number, alt), Data::None) {
            if let Err(back) = self.host.set_interface(self.slot, &new.endpoints, old) {
                let mut cur = self.alts.cur.lock();
                cur.broken.retain(|b| b.0 != number);
                cur.broken.push((number, alt));
                drop(cur);
                klog::event!(Error, "usb", "device.alt_restore_failed", device = self.path(), interface = number, alt = host_alt,
                    error = back.as_str(); "host and device disagree on the alternate setting; its endpoints are unusable until set again");
            }
            klog::event!(Warn, "usb", "device.alt_refused", device = self.path(), interface = number, alt = alt, error = e.as_str());
            return Err(e);
        }
        {
            let mut cur = self.alts.cur.lock();
            cur.broken.retain(|b| b.0 != number);
            cur.set.retain(|x| x.0 != number);
            if alt != 0 {
                cur.set.push((number, alt));
            }
        }
        klog::event!(Debug, "usb", "device.alt_set", device = self.path(), interface = number, alt = alt, endpoints = new.endpoints.len());
        Ok(())
    }

    /// An isochronous pipe on endpoint `ep` of the current alternate settings, keeping at most `opts.window_ns`
    /// worth of service intervals queued (at least one packet).
    pub fn iso_pipe(self: &Arc<Self>, ep: u8, opts: IsoOpts) -> Result<IsoPipe, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        let d = self.endpoint(ep).filter(|e| e.kind() == EpKind::Isoch)
            .ok_or(UsbError::Invalid("not an isochronous endpoint of the current alternate settings"))?;
        let window = opts.window_ns.div_ceil(service_interval_ns(self.speed(), &d)).max(1) as usize;
        Ok(IsoPipe { dev: self.clone(), ep: d, window, queued: Arc::new(AtomicUsize::new(0)) })
    }
}
