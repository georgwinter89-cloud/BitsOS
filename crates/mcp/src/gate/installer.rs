//! MC-9a: the kernel's side of the installer (protocol: `mcp_wire::installer`). bitsmcp's `install` becomes a
//! pending request here; the session that holds the installer's role (`claim`, judged by `identity`) takes it
//! with `next` and answers it with `decide`; the waiting `install` then runs the tool or answers the refusal.
//! Waits poll in short slices on the server's clock (`Env`): a human decision takes seconds, so 25 ms of extra
//! latency is nothing, and every wait has its bound (B4).

use crate::server::Env;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use mcp_wire::gate::Request;
use mcp_wire::installer::{Asked, Decision, Kind};
use uapi::errno::*;
use vfs::Errno;

const SLICE_NS: u64 = 25_000_000;

pub(crate) struct Pending {
    pub id: u64,
    pub req: Request,
    pub kind: Kind,
    taken: AtomicBool,
    decision: SpinLock<Option<Decision>>,
}

pub(crate) struct Broker {
    /// The session holding the installer's role.
    holder: SpinLock<Option<u64>>,
    /// Requests waiting for a decision, oldest first (a few at most: each holds one of bitsmcp's sessions).
    queue: SpinLock<Vec<Arc<Pending>>>,
    next_id: AtomicU64,
}

impl Broker {
    pub fn new() -> Broker {
        Broker { holder: SpinLock::named("mcp.installer.holder", None), queue: SpinLock::named("mcp.installer.queue", Vec::new()), next_id: AtomicU64::new(1) }
    }

    pub fn holds(&self, seat: u64) -> bool {
        *self.holder.lock() == Some(seat)
    }

    /// The role for `seat`; EBUSY while another seat holds it (a second claim by the holder is fine).
    pub fn claim(&self, seat: u64) -> Result<(), Errno> {
        let mut h = self.holder.lock();
        match *h {
            Some(other) if other != seat => Err(EBUSY),
            _ => {
                *h = Some(seat);
                Ok(())
            }
        }
    }

    /// `seat` ended: if it held the role, every waiting request is refused (true: it held it).
    pub fn release(&self, seat: u64) -> bool {
        {
            let mut h = self.holder.lock();
            if *h != Some(seat) {
                return false;
            }
            *h = None;
        }
        let waiting: Vec<Arc<Pending>> = self.queue.lock().clone();
        for p in waiting {
            let mut d = p.decision.lock();
            if d.is_none() {
                *d = Some(Decision { id: p.id, yes: false, by: String::from("kernel"), reason: String::from("the installer ended before it decided") });
            }
        }
        true
    }

    /// Queues `req` for the installer; None when no installer holds the role. The holder lock is kept across
    /// the push so `release` either sees this request in its snapshot or this call sees no holder.
    pub fn ask(&self, req: Request) -> Option<Arc<Pending>> {
        let h = self.holder.lock();
        h.as_ref()?;
        let kind = Kind::of(&req.tool);
        let p = Arc::new(Pending { id: self.next_id.fetch_add(1, Ordering::Relaxed), req, kind, taken: AtomicBool::new(false), decision: SpinLock::named("mcp.installer.decision", None) });
        self.queue.lock().push(p.clone());
        drop(h);
        Some(p)
    }

    /// Waits up to `timeout_ns` for `p`'s decision; `p` leaves the queue either way. When none came (or `abort`
    /// said the caller is gone) the kernel's own no goes into the slot under its lock, so a `decide` arriving
    /// after that is told ENOENT instead of believing its yes counted.
    pub fn wait(&self, env: &dyn Env, p: &Arc<Pending>, timeout_ns: u64, abort: &dyn Fn() -> bool) -> Decision {
        let end = env.now_ns().saturating_add(timeout_ns);
        let mut aborted = false;
        loop {
            if p.decision.lock().is_some() {
                break;
            }
            if abort() {
                aborted = true;
                break;
            }
            if env.now_ns() >= end {
                break;
            }
            env.sleep_ns(SLICE_NS);
        }
        let d = p
            .decision
            .lock()
            .get_or_insert_with(|| {
                let reason = if aborted { String::from("the request ended before a decision") } else { alloc::format!("no decision within {} s", timeout_ns / 1_000_000_000) };
                Decision { id: p.id, yes: false, by: String::from("kernel"), reason }
            })
            .clone();
        self.queue.lock().retain(|q| !Arc::ptr_eq(q, p));
        d
    }

    /// The oldest request not handed out yet, waiting up to `wait_ns` for one.
    pub fn next(&self, env: &dyn Env, wait_ns: u64, abort: &dyn Fn() -> bool) -> Option<Asked> {
        let end = env.now_ns().saturating_add(wait_ns);
        loop {
            let found = self.queue.lock().iter().find(|p| !p.taken.swap(true, Ordering::AcqRel)).cloned();
            if let Some(p) = found {
                return Some(Asked { id: p.id, tool: p.req.tool.clone(), kind: p.kind, args: p.req.args.clone(), timeout_ms: p.req.timeout_ms, peer: p.req.peer.clone(), client: p.req.client.clone() });
            }
            if abort() || env.now_ns() >= end {
                return None;
            }
            env.sleep_ns(SLICE_NS);
        }
    }

    /// The installer's word; ENOENT when `d.id` waits no more (or was decided already). Returns the request.
    pub fn decide(&self, d: Decision) -> Result<Arc<Pending>, Errno> {
        // Only a request the installer was handed (it showed the person exactly this one).
        let p = self.queue.lock().iter().find(|p| p.id == d.id && p.taken.load(Ordering::Acquire)).cloned().ok_or(ENOENT)?;
        let mut slot = p.decision.lock();
        if slot.is_some() {
            return Err(ENOENT);
        }
        *slot = Some(d);
        drop(slot);
        Ok(p)
    }

    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.queue.lock().len()
    }
}
