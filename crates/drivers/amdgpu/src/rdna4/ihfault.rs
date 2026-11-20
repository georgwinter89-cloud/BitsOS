//! Wave 43, item HB (piece 10's fault decode): a page fault of one of the card's two hubs as it arrives in the IH,
//! taken as Linux's `gmc_v12_0_process_interrupt` takes it (gmc_v12_0.c:91-157). The IH's reader hands every
//! entry of the two sources `gmc_v12_0_sw_init` registers (`rdna4_core::fault12::taker`: client VMC source 0 for
//! MMHUB, client GFX source 0 for GFXHUB) to `Faults::take`, which
//!
//! - reads the hub's L2_PROTECTION_FAULT_STATUS_LO32, once more first when the entry's `vmid_src` says GFXHUB
//!   (Linux's "dummy read", :126-127), and writes the hub's fault control back with its clear bit (:130);
//! - counts the fault per hub and keeps the last one, whatever the rate limit says;
//! - says it in plain words and in Linux's own lines, at most ten times in five seconds as Linux's
//!   `printk_ratelimit()` gates its lines (:136).
//!
//! The address is the entry's (Linux never reads the hub's fault address registers here). A fault with the retry
//! flag is counted and said like any other: `noretry` is on for this card, so `amdgpu_gmc_handle_retry_fault` is
//! not built. A status that does not read is taken as 0 (Linux then prints no status) and counted.

use super::ihring::IhIo;
use alloc::format;
use alloc::string::String;
use rdna4_core::fault12::{self, Fault, RateLimit};
use rdna4_core::hub41::Kind;
use rdna4_core::ih7::Iv;

/// The page faults of one card in this boot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// Faults taken, per hub whose registers were read: (GFXHUB, MMHUB).
    pub count: [u64; 2],
    /// The last fault, as decoded.
    pub last: Option<Fault>,
    pub rate: RateLimit,
    /// The hub's status did not read, or its fault control took no write: the fault is counted, its kind unknown.
    pub unread: u64,
}

impl Faults {
    /// One entry of a fault source.
    pub fn take(&mut self, io: &mut dyn IhIo, iv: &Iv, hub: Kind) -> Fault {
        if fault12::reads(iv).dummy_first {
            let _ = io.fault_status(hub);
        }
        let status = io.fault_status(hub).filter(|v| *v != u32::MAX);
        let cleared = io.fault_clear(hub);
        if status.is_none() || !cleared {
            self.unread += 1;
        }
        let f = Fault::new(iv, hub, status.unwrap_or(0));
        self.count[usize::from(hub == Kind::Mm)] += 1;
        self.last = Some(f);
        let (print, ended) = self.rate.allow(io.now_us());
        if let Some(n) = ended {
            klog::event!(Warn, "gx", "rdna4.fault.suppressed", faults = n; "page faults of the card that were counted but not printed in the last five seconds");
        }
        if print {
            klog::event!(
                Error, "gx", "rdna4.fault",
                hub = f.named as u8, addr = klog::Hex(f.addr), status = klog::Hex(u64::from(f.status)), vmid = f.vmid, pasid = f.pasid, retry = f.retry,
                what = format!("{f}").as_str(), linux = format!("{}", f.linux()).as_str();
                "a page fault of the card"
            );
        }
        f
    }

    pub fn describe(&self) -> String {
        if self.count == [0, 0] {
            return String::from("no page faults");
        }
        format!(
            "page faults: gfxhub {}, mmhub {} ({} not printed, {} with an unreadable status); the last: {}",
            self.count[0],
            self.count[1],
            self.rate.missed_total,
            self.unread,
            self.last.map_or(String::from("-"), |f| format!("{f}"))
        )
    }
}
