//! Counters as metrics (B6). A lane declares `klog::counter!(RX_DROPS, "net.rx_drops");` and bumps it;
//! RB-7's sampler reads every registered counter every 2 s into the trace/journal, MCP `metrics` lists them.

use core::sync::atomic::{AtomicU64, Ordering};

pub struct Counter {
    pub name: &'static str,
    value: AtomicU64,
}

impl Counter {
    pub const fn new(name: &'static str) -> Self {
        Counter { name, value: AtomicU64::new(0) }
    }
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }
    pub fn inc(&self) {
        self.add(1)
    }
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

pub struct CounterRef(pub &'static Counter);

kreg::declare!(pub COUNTERS: CounterRef = "metric");

/// `klog::counter!(NAME, "subsystem.metric");` declares a static counter and registers it.
#[macro_export]
macro_rules! counter {
    ($vis:vis $id:ident, $name:literal) => {
        $vis static $id: $crate::metrics::Counter = $crate::metrics::Counter::new($name);
        $crate::__kreg::register!("metric", $crate::metrics::CounterRef, $crate::metrics::CounterRef(&$id));
    };
}

/// All registered counters with their current values.
pub fn snapshot() -> impl Iterator<Item = (&'static str, u64)> {
    COUNTERS.entries().iter().map(|c| (c.0.name, c.0.get()))
}
