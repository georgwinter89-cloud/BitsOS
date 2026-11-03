//! MC-10: the kernel's side of full access (protocol and clock rule: `mcp_wire::access`). The installer
//! (the boot's measured /bin/bitsinstall, the only session that may send `grants`) hands the kernel the table
//! of live grants it keeps on /data; the gate then relays every kernel tool to a client whose key holds a grant
//! (`Gate::granted`). The kernel judges each grant by the clock rule itself, at every command, and ends what the
//! rule ends (event `mcp access.ended`), so a grant the installer forgot to drop does not outlive its time.
//! A grant with Georg's NVMe tick opens the block layer's run-time NVMe gate until the grant's end
//! (`block::policy::open_nvme_until`) and closes it when no such grant is left.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;
use mcp_wire::access::{self as wire, Ended, Grant, Seen};

const NS: u64 = 1_000_000_000;

struct Live {
    g: Grant,
    seen: Seen,
    /// The monotonic ns when it was first seen (for the NVMe gate's end).
    seen_ns: u64,
}

impl Live {
    /// The grant's end on the monotonic clock (what the NVMe gate is told).
    fn end_ns(&self) -> u64 {
        let left = self.g.until.saturating_sub(self.seen.wall).max(0) as u64;
        self.seen_ns.saturating_add(left.saturating_mul(NS))
    }
}

/// Where the NVMe tick goes: the block layer's run-time opener (tests: a recorder of their own, so a ktest on
/// the Mini-PC never opens the real NVMe gate).
#[derive(Clone, Copy)]
pub(crate) struct NvmeSink {
    pub open: fn(u64, &str),
    pub close: fn(&str),
}

pub(crate) const BLOCK: NvmeSink = NvmeSink { open: block::policy::open_nvme_until, close: block::policy::close_nvme };

pub(crate) struct Table {
    live: SpinLock<Vec<Live>>,
    /// The NVMe gate's end as last told to the block layer (0: closed by us). Held across reading `live` and
    /// telling the sink, so two callers cannot apply their results out of order (lock order: nvme_ns, then live).
    nvme_ns: SpinLock<u64>,
    sink: NvmeSink,
    /// MC-11: the installer has sent a table since this kernel started; before that "no grant" is "not known yet".
    loaded: AtomicBool,
}

/// What `set` took and refused.
pub(crate) struct Taken {
    pub taken: usize,
    pub refused: Vec<(String, Ended)>,
}

fn short(c: &str) -> &str {
    c.get(..16).unwrap_or(c)
}

impl Table {
    pub fn new() -> Table {
        Table::with_sink(BLOCK)
    }

    pub fn with_sink(sink: NvmeSink) -> Table {
        Table { live: SpinLock::named("mcp.access.live", Vec::new()), nvme_ns: SpinLock::named("mcp.access.nvme", 0), sink, loaded: AtomicBool::new(false) }
    }

    /// The end last told to the NVMe sink (0: closed).
    #[allow(dead_code)]
    pub fn nvme_told(&self) -> u64 {
        *self.nvme_ns.lock()
    }

    /// The installer's new table. A grant already held keeps the moment it was first seen (the clock rule
    /// measures from there); one the rule refuses is not taken. Without a wall clock no grant is taken.
    pub fn set(&self, list: Vec<Grant>, wall: Option<i64>, mono_ns: u64) -> Taken {
        let mono = (mono_ns / NS) as i64;
        let mut refused = Vec::new();
        let mut fresh = Vec::new();
        let mut next: Vec<Live> = Vec::new();
        {
            let old = self.live.lock();
            for g in list {
                let Some(now) = wall else {
                    refused.push((g.client, Ended::ClockBack));
                    continue;
                };
                let kept = old.iter().find(|l| l.g == g).map(|l| (l.seen, l.seen_ns));
                let (seen, seen_ns) = kept.unwrap_or((Seen { wall: now, mono }, mono_ns));
                match wire::judge(&g, now, seen, mono) {
                    Ok(()) => {
                        if kept.is_none() {
                            fresh.push((g.client.clone(), g.until, g.nvme));
                        }
                        next.push(Live { g, seen, seen_ns })
                    }
                    Err(why) => refused.push((g.client, why)),
                }
            }
        }
        for (c, why) in &refused {
            klog::event!(Warn, "mcp", "access.refused", client = short(c), rule = why.name(); "{}", why.text());
        }
        let taken = next.len();
        let gone: Vec<Live> = {
            let mut cur = self.live.lock();
            let old = core::mem::replace(&mut *cur, next);
            old.into_iter().filter(|o| !cur.iter().any(|n| n.g.client == o.g.client)).collect()
        };
        for o in &gone {
            klog::event!(Info, "mcp", "access.ended", client = short(&o.g.client), why = "revoked"; "the installer's table no longer holds it");
        }
        for (c, until, nvme) in &fresh {
            klog::event!(Warn, "mcp", "access.live", client = short(c), until = *until, nvme = *nvme; "full access for this client: every kernel tool through the gate");
        }
        self.sync_nvme();
        if !self.loaded.swap(true, Ordering::AcqRel) {
            klog::event!(Info, "mcp", "access.loaded", taken = taken; "the installer's first full-access table");
        }
        Taken { taken, refused }
    }

    /// Ends what the clock rule ends by now.
    pub fn sweep(&self, wall: Option<i64>, mono_ns: u64) {
        let mono = (mono_ns / NS) as i64;
        let ended: Vec<(String, Ended)> = {
            let mut l = self.live.lock();
            let mut out = Vec::new();
            l.retain(|x| {
                let r = match wall {
                    Some(now) => wire::judge(&x.g, now, x.seen, mono),
                    None => Err(Ended::ClockBack),
                };
                if let Err(why) = r {
                    out.push((x.g.client.clone(), why));
                }
                r.is_ok()
            });
            out
        };
        if ended.is_empty() {
            return;
        }
        for (c, why) in &ended {
            klog::event!(Info, "mcp", "access.ended", client = short(c), rule = why.name(); "{}", why.text());
        }
        self.sync_nvme();
    }

    /// Whether `client` holds a grant (sweep first).
    pub fn live(&self, client: &str) -> bool {
        !client.is_empty() && self.live.lock().iter().any(|l| l.g.client == client)
    }

    /// `client`'s state for `access_status` (sweep first).
    pub fn status(&self, client: &str, wall: Option<i64>, mono_ns: u64, offset_min: i32) -> wire::Status {
        let mono = (mono_ns / NS) as i64;
        let l = self.live.lock();
        match (l.iter().find(|x| !client.is_empty() && x.g.client == client), wall) {
            (Some(x), Some(now)) => {
                let left = wire::left_s(&x.g, now, x.seen, mono);
                // The end the client can count on: the earlier of the two clocks' ends.
                let until = x.g.until.min(now.saturating_add(left));
                wire::Status { granted: left > 0, until, left_s: left, nvme: x.g.nvme, local: wire::local_text(until, offset_min), loaded: true }
            }
            _ => wire::Status { granted: false, until: 0, left_s: 0, nvme: false, local: String::new(), loaded: self.loaded.load(Ordering::Acquire) },
        }
    }


    /// The NVMe gate follows the live grants with Georg's tick: open until the latest end, else closed. The
    /// table is read and the sink told under one lock: a caller holding an older view cannot apply it after a
    /// newer one (a sweep's stale "open" after a revoke's "closed").
    fn sync_nvme(&self) {
        let mut had = self.nvme_ns.lock();
        let want = self.live.lock().iter().filter(|l| l.g.nvme).map(Live::end_ns).max().unwrap_or(0);
        if *had == want {
            return;
        }
        *had = want;
        if want == 0 {
            (self.sink.close)("full-access grant ended");
        } else {
            (self.sink.open)(want, "full-access grant with the NVMe tick");
        }
    }
}
