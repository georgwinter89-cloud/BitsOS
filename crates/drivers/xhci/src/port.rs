//! Root ports (spec usb.md §3.2; V1 `xhciport.asm`, `xhcilink.asm`; xHCI 1.2 §4.19, §5.4.8). PORTSC is written
//! with the bits it keeps (`portsc::KEEP`: power, indicator, wake enables) plus only the RW1C change bits meant
//! to be cleared or the one action bit; **PED is never written** (a 1 disables the port).
//! USB 2 port: PR, PRC within 500 ms, PED required, changes cleared, 50 ms recovery. USB 3 port: a link in
//! SS.Inactive or Compliance gets a warm reset (WPR) at once; otherwise PED with link in U0 within 500 ms, else
//! the warm reset; then PRC, U0 again.

use crate::hc::{Controller, Env, MS};
use crate::regs::portsc::{self, CCS, CHANGES, CSC, KEEP, OCA, OCC, PEC, PED, PLC, PLS_MASK, PLS_SHIFT, PP, PR, PRC, WPR, WRC, CEC};
use alloc::string::String;
use usb_core::{Place, PortStatus, Ports, Speed, UsbError};

/// A port's reset, or a USB 3 link coming up (V1 `XH_PORT_MS`).
pub const PORT_NS: u64 = 500 * MS;
/// After a USB 2 port reset, before the address: 10 ms TRSTRCY plus Linux's margin (V1 `XH_RECOVERY`).
pub const RECOVERY_NS: u64 = 50 * MS;
const POLL_STEP_NS: u64 = MS;

klog::counter!(PORT_RESETS, "usb.xhci.port_resets");
klog::counter!(WARM_RESETS, "usb.xhci.warm_resets");

/// The status a PORTSC value shows.
pub fn decode(v: u32) -> PortStatus {
    PortStatus {
        connected: v & CCS != 0,
        enabled: v & PED != 0,
        powered: v & PP != 0,
        over_current: v & OCA != 0,
        speed: Speed::from_xhci_psiv(((v >> portsc::SPEED_SHIFT) & 0xF) as u8),
        connect_change: v & CSC != 0,
        enable_change: v & PEC != 0,
        reset_change: v & (PRC | WRC) != 0,
        over_current_change: v & OCC != 0,
        link_change: v & (PLC | CEC) != 0,
        raw: v,
    }
}

pub(crate) struct RootPorts<'a, E: Env> {
    pub(crate) c: &'a Controller<E>,
}

impl<E: Env> RootPorts<'_, E> {
    fn off(&self, port: u8) -> usize {
        self.c.caps.portsc(port)
    }
    fn read(&self, port: u8) -> Result<u32, UsbError> {
        match self.c.rd(self.off(port)) {
            u32::MAX => Err(UsbError::NotRunning),
            v => Ok(v),
        }
    }
    /// Writes the keep bits plus `set` (an action bit or change bits to clear).
    fn write(&self, port: u8, set: u32) -> Result<(), UsbError> {
        let v = self.read(port)?;
        self.c.wr(self.off(port), (v & KEEP) | set);
        Ok(())
    }
    fn clear_all_changes(&self, port: u8) -> Result<u32, UsbError> {
        let v = self.read(port)?;
        self.c.wr(self.off(port), (v & KEEP) | (v & CHANGES));
        Ok(v)
    }
    fn wait(&self, port: u8, mask: u32, want: u32) -> Result<(), UsbError> {
        self.c.poll_sleep(self.off(port), mask, want, PORT_NS, POLL_STEP_NS).map_err(|w| match w {
            crate::hc::Wait::Gone => UsbError::NotRunning,
            crate::hc::Wait::Timeout => UsbError::Timeout,
        })
    }
    fn usb3(&self, port: u8) -> bool {
        self.c.protocol_of(port).is_some_and(|p| p.major == 3)
    }

    fn reset_usb2(&self, port: u8) -> Result<u32, UsbError> {
        self.write(port, PR)?;
        self.wait(port, PRC, PRC)?;
        let v = self.clear_all_changes(port)?;
        if v & PED == 0 {
            return Err(UsbError::Invalid("root port not enabled after its reset"));
        }
        self.c.env.sleep_ns(RECOVERY_NS);
        self.read(port)
    }

    fn link_usb3(&self, port: u8) -> Result<u32, UsbError> {
        let pls = (self.read(port)? & PLS_MASK) >> PLS_SHIFT;
        let stuck = matches!(pls, portsc::PLS_INACTIVE | portsc::PLS_COMPLIANCE);
        if stuck || self.wait(port, PED | PLS_MASK, PED).is_err() {
            WARM_RESETS.inc();
            klog::event!(Info, "usb", "xhci.warm_reset", ctrl = self.c.name.as_str(), port = port, link_state = pls,
                portsc = klog::Hex(self.read(port)? as u64),
                why = if stuck { "link inactive or in compliance" } else { "USB 3 link not in U0 within 500 ms" });
            self.write(port, WPR)?;
            self.wait(port, PRC, PRC)?;
            self.clear_all_changes(port)?;
            self.wait(port, PED | PLS_MASK, PED)?;
        }
        self.clear_all_changes(port)?;
        self.read(port)
    }
}

impl<E: Env> Ports for RootPorts<'_, E> {
    fn label(&self) -> String {
        self.c.name.clone()
    }
    fn count(&self) -> u8 {
        self.c.caps.max_ports
    }
    fn usable(&self, port: u8) -> bool {
        self.c.protocol_of(port).is_some()
    }
    fn status(&self, port: u8) -> Result<PortStatus, UsbError> {
        self.read(port).map(decode)
    }
    fn clear_changes(&self, port: u8, st: &PortStatus) {
        let _ = self.write(port, st.raw & CHANGES);
    }
    fn reset(&self, port: u8) -> Result<Speed, UsbError> {
        PORT_RESETS.inc();
        let v = if self.usb3(port) { self.link_usb3(port)? } else { self.reset_usb2(port)? };
        let psiv = ((v >> portsc::SPEED_SHIFT) & 0xF) as u8;
        let speed = self.c.protocol_of(port).and_then(|p| p.speed(psiv));
        if speed.is_none() {
            klog::event!(Warn, "usb", "xhci.unknown_speed", ctrl = self.c.name.as_str(), port = port, psiv = psiv,
                portsc = klog::Hex(v as u64));
        }
        speed.ok_or(UsbError::Invalid("root port reports no known speed"))
    }
    fn disable(&self, port: u8) {
        klog::event!(Debug, "usb", "xhci.port_left", ctrl = self.c.name.as_str(), port = port;
            "root port left as it is (PED is never written)");
    }
    fn place(&self, port: u8, speed: Speed) -> Result<Place, UsbError> {
        Ok(Place::root(port, speed))
    }
}
