//! SYS-K: the hwmon chip `nvme` per controller (linux-7.3.1/drivers/nvme/host/hwmon.c): `temp1_input` the SMART log's
//! composite temperature in millidegrees (`kelvin_to_millicelsius`), `temp1_label` "Composite". Linux reads the log
//! at every read; here the thread `nvme-hwmon` reads it (one Get Log Page, read-only) every 10 s while somebody read
//! in the last 60 s, and a read that finds no fresh value wakes it and waits at most 300 ms. No reading: ENODATA.

use crate::Nvme;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::errno::ENODATA;
use vfs::hwmon::{Attr, Chip};

const PERIOD_MS: u64 = 10_000;
const ACTIVE_NS: u64 = 60_000_000_000;
const FRESH_NS: u64 = 15_000_000_000;
const WAIT_NS: u64 = 300_000_000;
const IDLE_WAIT_NS: u64 = 30_000_000_000;

struct Watched {
    ctrl: Weak<Nvme>,
    name: String,
    hwmon: u32,
    /// (when, Kelvin) of the last reading.
    temp: Option<(u64, u16)>,
}

static WATCHED: SpinLock<Vec<Watched>> = SpinLock::named("nvme.hwmon", Vec::new());
static LAST_READ: AtomicU64 = AtomicU64::new(0);
static WANT: AtomicBool = AtomicBool::new(false);
static GEN: AtomicU64 = AtomicU64::new(0);
static WAKE: WaitQueue = WaitQueue::new();
static DONE: WaitQueue = WaitQueue::new();
static STARTED: ksync::Once<()> = ksync::Once::new();

fn now() -> u64 {
    time::now_ns()
}

/// `kelvin_to_millicelsius` (include/linux/units.h): K * 1000 - 273150.
pub fn millicelsius(kelvin: u16) -> i64 {
    kelvin as i64 * 1000 - 273_150
}

fn refresh() {
    let ctrls: Vec<(String, Option<Arc<Nvme>>)> = WATCHED.lock().iter().map(|w| (w.name.clone(), w.ctrl.upgrade())).collect();
    for (name, c) in ctrls {
        // The admin command runs without the list's lock held.
        let t = c.and_then(|c| c.smart().ok()).map(|s| (now(), s.temperature_k));
        if let Some(w) = WATCHED.lock().iter_mut().find(|w| w.name == name) {
            w.temp = t.or(w.temp);
        }
    }
    GEN.fetch_add(1, Ordering::AcqRel);
    DONE.wake_all();
}

/// Somebody read in the last `ACTIVE_NS` (0: nobody ever did).
fn active() -> bool {
    let last = LAST_READ.load(Ordering::Acquire);
    last != 0 && now().saturating_sub(last) < ACTIVE_NS
}

fn run() {
    loop {
        if WANT.swap(false, Ordering::AcqRel) || active() {
            refresh();
            // A reader that finds no fresh value (a device that just came) cuts the pause short.
            WAKE.wait_until_timeout(|| WANT.load(Ordering::Acquire), PERIOD_MS * 1_000_000);
        } else {
            WAKE.wait_until_timeout(|| WANT.load(Ordering::Acquire), IDLE_WAIT_NS);
        }
    }
}

fn fresh(name: &str, t: u64) -> Option<u16> {
    WATCHED.lock().iter().find(|w| w.name == name)?.temp.filter(|(at, _)| t.saturating_sub(*at) <= FRESH_NS).map(|(_, k)| k)
}

fn temp(name: &str) -> Option<u16> {
    let t = now();
    LAST_READ.store(t, Ordering::Release);
    if let Some(k) = fresh(name, t) {
        return Some(k);
    }
    let g0 = GEN.load(Ordering::Acquire);
    WANT.store(true, Ordering::Release);
    WAKE.wake_all();
    DONE.wait_until_timeout(|| GEN.load(Ordering::Acquire) != g0, WAIT_NS);
    fresh(name, t)
}

/// A controller is up: its chip (`device` is its PCI function, `0000:01:00.0`).
pub fn added(c: &Arc<Nvme>, function: &str) {
    STARTED.get_or_init(|| {
        if let Err(e) = sched::spawn("nvme-hwmon", sched::Class::Normal, run) {
            klog::event!(Warn, "nvme", "hwmon.spawn_failed", error = format!("{e:?}"); "temp1_input answers ENODATA");
        }
    });
    let name = c.name.clone();
    let n2 = name.clone();
    let attrs = alloc::vec![Attr::new("temp1_input", move || temp(&n2).map(|k| format!("{}\n", millicelsius(k))).ok_or(ENODATA)), Attr::label("temp1_label", "Composite")];
    let hwmon = vfs::hwmon::register(Chip { name: String::from("nvme"), device: Some(format!("bus/pci/devices/{function}")), attrs });
    WATCHED.lock().push(Watched { ctrl: Arc::downgrade(c), name, hwmon, temp: None });
}

/// A controller went: its chip goes.
pub fn removed(c: &Arc<Nvme>) {
    let gone: Vec<u32> = {
        let mut l = WATCHED.lock();
        let g = l.iter().filter(|w| w.name == c.name).map(|w| w.hwmon).collect();
        l.retain(|w| w.name != c.name);
        g
    };
    for n in gone {
        vfs::hwmon::unregister(n);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn kelvin_to_millidegrees_as_linux() {
        assert_eq!(super::millicelsius(310), 36_850);
        assert_eq!(super::millicelsius(0), -273_150);
    }
}

/// SYS-K ktest: one `nvme` chip per controller that came up, each with a plausible composite temperature read
/// through the class as a tool reads it (QEMU plain has no NVMe: no chip). `real`: the machine has an NVMe disk
/// (the Mini-PC's Lexar), so at least one chip must be there.
fn chips_ktest(real: bool) -> ktest::TestResult {
    use ktest::kassert;
    let up = crate::controllers().iter().filter(|c| c.state() == crate::State::Ready).count();
    kassert!(!real || up > 0, "no NVMe controller came up on this machine");
    let chips: Vec<u32> = vfs::hwmon::chips().iter().filter(|(_, c)| c.name == "nvme").map(|(n, _)| *n).collect();
    kassert!(chips.len() == up, "{} nvme chips for {up} controllers up", chips.len());
    for n in chips {
        let d = format!("/sys/class/hwmon/hwmon{n}");
        let label = vfs::root_ns().read_file(&format!("{d}/temp1_label"), 64).map_err(|e| format!("{d}/temp1_label: errno {e}"))?;
        kassert!(label == b"Composite\n", "{d}: label {label:?}");
        let t = vfs::root_ns().read_file(&format!("{d}/temp1_input"), 64).map_err(|e| format!("{d}/temp1_input: errno {e}"))?;
        let v: i64 = core::str::from_utf8(&t).ok().and_then(|s| s.trim_end().parse().ok()).ok_or(format!("{d}: {t:?}"))?;
        kassert!((0..=95_000).contains(&v), "{d}: composite {v} m°C");
    }
    Ok(())
}
fn chips_ktest_any() -> ktest::TestResult {
    chips_ktest(false)
}
ktest::ktest!("so.sysk.nvme_hwmon", Safe, chips_ktest_any);
fn chips_ktest_real() -> ktest::TestResult {
    chips_ktest(true)
}
ktest::ktest!("so.sysk.nvme_hwmon_real", Hardware, chips_ktest_real);
