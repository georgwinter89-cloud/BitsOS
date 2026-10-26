//! Metric sampler (RB-7b, B6): every `PERIOD_NS` the `metrics` thread reads the kernel's numbers
//! (`sources.rs`), turns running totals into per-period values (`sampler.rs`) and adds one round to the
//! series store (`store.rs`), which the MCP tool `metrics` (`tool.rs`) shows and RB-8's journal will drain
//! (`with_store`, then `Store::rounds`/`time`/`series`). The store's history is sized from RAM (R1):
//! RAM/4096 bytes (Mini-PC 32 GB: 8 MiB, hours of history), CONFIG `metrics_buf=<bytes>` wins (at least
//! 64 KiB); CONFIG `metrics=off` turns the sampler off.

pub mod sampler;
pub(crate) mod sources;
pub mod store;
pub mod temp;
pub mod tool;

use alloc::string::String;
use ksync::Mutex;
use kreg::Level;
use store::Store;

pub const PERIOD_NS: u64 = 2_000_000_000;
const MIN_BUDGET: usize = 64 * 1024;

/// `None` until the sampler starts (and for good with `metrics=off`).
pub(crate) static STORE: Mutex<Option<Store>> = Mutex::named("rb.metrics", None);

/// Runs `f` on the store, waiting at most `timeout_ns` for it; `None` when off or busy.
pub fn with_store<R>(timeout_ns: u64, f: impl FnOnce(&Store) -> R) -> Option<R> {
    STORE.lock_timeout(timeout_ns)?.as_ref().map(f)
}

pub fn budget(ram: u64, config: Option<&str>) -> usize {
    let from_ram = (ram / 4096) as usize;
    config.and_then(|c| c.trim().parse::<usize>().ok()).unwrap_or(from_ram).max(MIN_BUDGET)
}

fn sensors(src: &sources::Sources) -> String {
    match &src.zen {
        sources::Sensors::Ready(z) => z.names().collect::<alloc::vec::Vec<_>>().join(","),
        sources::Sensors::Pending(_) => String::from("after the PCI scan"),
        sources::Sensors::Absent(why) => String::from(*why),
    }
}

fn run(mut src: sources::Sources) {
    let mut sampler = sampler::Sampler::default();
    let mut cost = 0;
    let mut next = klog::now();
    loop {
        let t0 = klog::now();
        if src.find_sensors() {
            klog::event!(Info, "klog", "metrics.sensors", temps = sensors(&src));
        }
        let reading = src.read(t0, cost);
        if let Some(store) = STORE.lock().as_mut() {
            sampler.step(&reading, store);
        }
        let now = klog::now();
        cost = now.saturating_sub(t0);
        // Fixed rhythm; a round that came late (a loaded machine) is not made up for.
        next += PERIOD_NS;
        if next <= now {
            next = now + PERIOD_NS;
        }
        sched::sleep_ms((next - now).div_ceil(1_000_000));
    }
}

fn start() -> Result<(), &'static str> {
    if kexec::boot::config("metrics").as_deref() == Some("off") {
        klog::event!(Info, "klog", "metrics.off"; "CONFIG metrics=off: no metric sampler");
        return Ok(());
    }
    let ram = mm_core::frames().map(|f| f.stats().total_bytes).unwrap_or(0);
    let budget = budget(ram, kexec::boot::config("metrics_buf").as_deref());
    *STORE.lock() = Some(Store::new(budget));
    let src = sources::Sources::detect();
    let temps = sensors(&src);
    let rapl = match src.esu {
        Ok(esu) => alloc::format!("energy unit 2^-{esu} J"),
        Err(why) => String::from(why),
    };
    klog::event!(Info, "klog", "metrics.start", period_s = PERIOD_NS / 1_000_000_000, budget_bytes = budget, temps = temps, rapl = rapl);
    // Normal like klogd: Background would starve under load, exactly when the numbers matter.
    match sched::spawn("metrics", sched::Class::Normal, move || run(src)) {
        Ok(_) => Ok(()),
        Err(e) => {
            klog::event!(Error, "klog", "metrics.spawn_failed", error = alloc::format!("{e:?}"));
            Err("metrics thread")
        }
    }
}
// Service: after PCI (temperatures), the network cards and the disks are registered; each round reads what
// exists then, so later cards and disks appear by themselves.
kreg::initcall!(Level::Service, "klog.metrics", start);

#[cfg(test)]
mod tests {
    #[test]
    fn the_budget_comes_from_ram_unless_config_says() {
        assert_eq!(super::budget(32 << 30, None), 8 << 20);
        assert_eq!(super::budget(128 << 20, None), 64 * 1024, "at least 64 KiB");
        assert_eq!(super::budget(32 << 30, Some("1000000")), 1_000_000);
        assert_eq!(super::budget(32 << 30, Some("x")), 8 << 20, "a bad value falls back to the RAM size");
    }
}
