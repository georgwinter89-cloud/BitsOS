//! The machine around the run: the kernel's clock and log, the console, the power-off.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu::rdna4::messung::Env;

/// The kernel's side of `messung::Env`.
pub struct KernelEnv {
    warn: u64,
    card: u64,
}

impl KernelEnv {
    pub fn new() -> KernelEnv {
        let now = klog::next_seq();
        KernelEnv { warn: now, card: now }
    }
}

impl Env for KernelEnv {
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }

    /// Warn and above of every subsystem, and the card's own (`gx`, the rdna4 events) from info on, once each.
    fn log(&mut self) -> Vec<String> {
        let mut got: BTreeMap<u64, String> = BTreeMap::new();
        for (since, f) in [(&mut self.warn, klog::Filter { level: Some(klog::Level::Warn), subsystem: None, name: None }), (&mut self.card, klog::Filter { level: Some(klog::Level::Info), subsystem: Some("gx"), name: None })] {
            let page = klog::read(*since, 512, &f);
            for r in &page.records {
                got.insert(r.seq, format!("{} {}", r.seq, r.render()));
            }
            if page.missed() > 0 {
                got.insert(page.next, format!("{} ({} lines of the log were lost before they were read)", page.next, page.missed()));
            }
            *since = page.next;
        }
        got.into_values().collect()
    }

    fn levels(&mut self) -> String {
        format!("{} (gx stored from {})", klog::levels(), klog::level_for("gx").name())
    }

    fn build(&mut self) -> String {
        format!("{} BitsOS {}", env!("CARGO_PKG_VERSION"), bitsver::NUMBER)
    }

    fn say(&mut self, line: &str) {
        console::vt::write(line, console::screen::FG_INDEX);
        console::vt::write("\n", console::screen::FG_INDEX);
    }

    fn wait_s(&mut self, s: u64) {
        sched::sleep_ns(s.saturating_mul(1_000_000_000));
    }

    fn poweroff(&mut self) -> String {
        console::reset::poweroff()
    }
}
