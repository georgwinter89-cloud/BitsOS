//! `C: Watchdog` (back end fch-wdt, RB-3). Arm with a timeout in seconds, kick while alive.

use crate::DevError;

pub trait Watchdog: Send + Sync {
    fn name(&self) -> &str;
    fn arm(&self, seconds: u32) -> Result<(), DevError>;
    fn kick(&self);
    fn stop(&self);
    /// Did the previous start end by this watchdog?
    fn fired_last_boot(&self) -> bool;
}
