//! VS-3, "the whole system as at 15:00" (B1, V2-PLAN §1a planner's choices): the system generation that ran at a
//! time (RB-9's `system::at_time`: the newest confirmed generation installed by then) and the PIN that brings it
//! back (RB-9's MCP tool `system_pin`, called as a tool so it takes RB's `Busy` and lock itself; rb-RB-9.md
//! decision 9). /data's part is `schedule_rollback`; `restore_at` puts the two together for `bitssnap restore-system`, the
//! MCP tool `restore` (kind system) and the console's `restore_at`.

use crate::Target;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use kexec::install::{system, system_stick};

/// How long the stick listing may take (it reads every generation's files once).
const LIST_WAIT_NS: u64 = 20_000_000_000;
/// The PIN write and its lock wait (system_pin's own default is 120 s; a stick write takes milliseconds).
const PIN_WAIT_NS: u64 = 60_000_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct At {
    /// The generation that ran at the time (None: no confirmed generation was installed by then).
    pub generation: Option<u32>,
    pub running: Option<u32>,
    /// What bitsboot starts next with the stick as it is.
    pub next: Option<u32>,
    pub pin: Option<u32>,
    pub writable: bool,
}

/// The stick's generations seen from `utc_s` (UTC seconds).
pub fn at(utc_s: i64) -> Result<At, String> {
    let l = system_stick::listing(LIST_WAIT_NS)?;
    let dirs: Vec<_> = l.gens.iter().map(|g| g.dir.clone()).collect();
    Ok(At {
        generation: system::at_time(&l.gens, utc_s),
        running: l.running.map(|g| g.number),
        next: system::next_start(&dirs, l.pin).ok().map(|(r, _, _)| r.number),
        pin: l.pin,
        writable: l.writable,
    })
}

/// `system_pin {"n": n}` (or "off"), never `now`: the switch happens at the next start or Reload, so /data's
/// scheduled rollback and the generation change together. `Ok(text)`: done; `Err(text)`: refused or failed
/// (nothing on the stick changed then, system_pin's own rule).
pub fn pin(n: Option<u32>) -> Result<String, String> {
    let t = mcp::find("system_pin").ok_or_else(|| String::from("system_pin is not in this kernel"))?;
    let args = match n {
        Some(n) => format!("{{\"n\":{n},\"timeout_ms\":{}}}", PIN_WAIT_NS / 1_000_000),
        None => format!("{{\"n\":\"off\",\"timeout_ms\":{}}}", PIN_WAIT_NS / 1_000_000),
    };
    let a = mcp::Json::parse(&args).map_err(|_| String::from("system_pin arguments"))?;
    let ctx = mcp::ToolContext::detached(time::now_ns().saturating_add(PIN_WAIT_NS));
    let text = |out: Vec<mcp::ToolOutput>| {
        out.into_iter()
            .filter_map(|o| match o {
                mcp::ToolOutput::Text(t) => Some(t),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    match (t.handler)(&ctx, &a) {
        Ok(out) => {
            let t = text(out);
            klog::event!(Info, "snap", "system.pinned", n = n.unwrap_or(0));
            Ok(t)
        }
        Err(mcp::ToolError::BadArgument(e) | mcp::ToolError::Refused(e) | mcp::ToolError::Failed(e)) => {
            klog::event!(Warn, "snap", "system.pin_refused", n = n.unwrap_or(0), why = e.as_str());
            Err(e)
        }
    }
}

/// One line without newlines (an answer line of /dev/bitssnap).
pub fn one_line(s: &str) -> String {
    s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

/// What `restore_at` did (or, dry, would do).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// /data's snapshot of that time (the newest taken at or before it).
    pub snapshot: u64,
    pub snapshot_ns: u64,
    /// The generation that ran then and gets pinned (None: the generation stays, `note` says why).
    pub pin: Option<u32>,
    pub at: At,
    /// A rollback scheduled before this one (replaced).
    pub replaced: Option<u64>,
    pub note: String,
}

/// The newest snapshot of `snaps` (id, created ns) taken at or before `utc_s`; one without a time (0) never.
pub fn snapshot_at(snaps: &[(u64, u64)], utc_s: i64) -> Option<(u64, u64)> {
    let limit = u64::try_from(utc_s).ok()?.saturating_mul(1_000_000_000).saturating_add(999_999_999);
    snaps.iter().copied().filter(|&(_, ns)| ns != 0 && ns <= limit).max_by_key(|&(id, ns)| (ns, id))
}

/// The generation to pin for a time: None when none was confirmed by then or the one of then starts next anyway.
pub fn pin_for(a: &At) -> (Option<u32>, String) {
    match a.generation {
        None => (None, String::from("no confirmed system generation was installed by then: the generation stays as it is")),
        Some(g) if a.next == Some(g) => (None, format!("generation {g} ran then and is the one the next start takes anyway: no pin")),
        Some(g) => (Some(g), format!("generation {g} ran then: pinned for the next start")),
    }
}

/// What this start's last `restore_at` changed, so that its undo takes back exactly that and nothing of
/// Georg's (an own PIN or rollback set before or after it stays). Kernel memory on purpose: the undo is "before
/// the next start"; after a reboot or Reload the switch has happened and the undo is the "before-rollback"
/// snapshot plus `system_pin off`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Done {
    pool: String,
    vol: u64,
    /// The rollback it scheduled and the one it replaced (None once undone).
    rollback: Option<(u64, Option<u64>)>,
    /// The PIN it wrote and the PIN before it (None: it wrote none, or that part is undone).
    pin: Option<(u32, Option<u32>)>,
}

static DONE: ksync::SpinLock<Option<Done>> = ksync::SpinLock::new(None);
/// One restore-at or undo at a time (each reads the stick and calls system_pin; never waited for: B4).
static ONE: ksync::Mutex<()> = ksync::Mutex::named("snap.restore_at", ());

fn one() -> Result<ksync::MutexGuard<'static, ()>, String> {
    ONE.try_lock().ok_or_else(|| String::from("another restore-at or its undo runs right now: try again in a minute"))
}

/// "Everything as at `utc_s`" (B1): /data back to its snapshot of then and the system generation of then, both
/// at the next start or Reload (`system_pin {"n":N,"now":true}` does both at once, `reboot` too). The /data
/// rollback is scheduled first; a refused PIN takes it back, so it is all or nothing. Undo before the start:
/// `undo_restore_at`; after it: the "before-rollback" snapshot the switch made plus `system_pin off`.
pub fn restore_at(t: &Target, utc_s: i64, dry: bool) -> Result<Plan, String> {
    let _one = one()?;
    let snaps = crate::list(t, false).map_err(|e| format!("the snapshots of /data cannot be listed (errno {e})"))?;
    let pairs: Vec<(u64, u64)> = snaps.iter().map(|s| (s.id, s.created_ns)).collect();
    let (snapshot, snapshot_ns) = snapshot_at(&pairs, utc_s).ok_or_else(|| String::from("no snapshot of /data from that time or before it (snapshots without a wall-clock time do not count): nothing changed"))?;
    let at = at(utc_s)?;
    let (pin, note) = pin_for(&at);
    if pin.is_some() && !at.writable {
        return Err(String::from("the boot stick is read-only: no PIN can be written, nothing changed"));
    }
    let mut plan = Plan { snapshot, snapshot_ns, pin, at, replaced: None, note };
    if dry {
        return Ok(plan);
    }
    plan.replaced = crate::schedule_rollback(t, snapshot).map_err(|e| format!("/data's rollback cannot be scheduled (errno {e}): nothing changed"))?;
    if let Some(g) = pin
        && let Err(e) = self::pin(Some(g))
    {
        let back = match plan.replaced {
            Some(o) => crate::schedule_rollback(t, o).map(|_| ()),
            None => crate::cancel_rollback(t).map(|_| ()),
        };
        return Err(match back {
            Ok(()) => format!("system_pin {g} refused: {e}; /data's rollback taken back: nothing changed"),
            Err(b) => format!("system_pin {g} refused: {e}; taking /data's rollback back failed too (errno {b}): bitssnap rollback cancel"),
        });
    }
    klog::event!(Info, "snap", "restore_at", utc_s = utc_s, snapshot = snapshot, pin = pin.unwrap_or(0));
    *DONE.lock() = Some(Done {
        pool: t.pool_name.clone(),
        vol: t.vol,
        rollback: Some((snapshot, plan.replaced)),
        pin: pin.map(|g| (g, plan.at.pin)),
    });
    Ok(plan)
}

/// The undo of this start's last `restore_at` before the next start: its /data rollback taken back (the one it
/// replaced is scheduled again) and its PIN replaced by the PIN before it, each only while it is still the one
/// restore_at set. `Ok((false, why))`: nothing of a restore-at to undo, nothing changed (a stray or repeated
/// undo never removes a PIN or rollback Georg set himself). `Err`: a part failed; what is undone stays undone
/// and a second undo retries the rest.
pub fn undo_restore_at(t: &Target) -> Result<(bool, String), String> {
    let _one = one()?;
    let Some(mut d) = DONE.lock().clone().filter(|d| d.pool == t.pool_name && d.vol == t.vol) else {
        return Ok((false, String::from("no restore-at of this start to undo: the PIN and /data's rollback stay as they are")));
    };
    let mut said = Vec::new();
    if let Some((to, replaced)) = d.rollback {
        match crate::pending_rollback(t).map_err(|e| format!("/data's rollback cannot be read (errno {e}): nothing changed"))? {
            Some(p) if p == to => {
                let r = match replaced {
                    Some(o) => crate::schedule_rollback(t, o).map(|_| format!("/data's rollback to {to} taken back (the rollback to {o} before it is scheduled again)")),
                    None => crate::cancel_rollback(t).map(|_| format!("/data's rollback to {to} cancelled")),
                };
                said.push(r.map_err(|e| format!("/data's rollback to {to} cannot be taken back (errno {e}): nothing changed"))?);
            }
            p => said.push(format!("/data's rollback was changed since (now: {}): left as it is", p.map_or_else(|| String::from("none"), |p| format!("to {p}")))),
        }
        d.rollback = None;
        *DONE.lock() = Some(d.clone());
    }
    match d.pin {
        Some((g, before)) => {
            let now = system_stick::listing(LIST_WAIT_NS).map(|l| l.pin);
            match now {
                Ok(Some(p)) if p == g => match pin(before) {
                    Ok(_) => said.push(before.map_or_else(|| format!("PIN {g} removed"), |b| format!("PIN {g} replaced by the PIN {b} from before"))),
                    Err(e) => {
                        klog::event!(Warn, "snap", "restore_at.undo_pin_failed", pin = g, why = e.as_str());
                        return Err(format!("{}; PIN {g} stays (system_pin refused: {e}); undo again to retry", said.join("; ")));
                    }
                },
                Ok(p) => said.push(format!("the PIN was changed since (now: {}): left as it is", p.map_or_else(|| String::from("none"), |p| format!("{p}")))),
                Err(e) => return Err(format!("{}; the stick cannot be read ({e}): PIN {g} stays; undo again to retry", said.join("; "))),
            }
        }
        None => said.push(String::from("restore-at wrote no PIN: the PIN stays")),
    }
    *DONE.lock() = None;
    klog::event!(Info, "snap", "restore_at.undone", pool = t.pool_name.as_str());
    Ok((true, said.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_of_a_time() {
        const S: u64 = 1_000_000_000;
        let l = [(2, 100 * S), (3, 0), (4, 200 * S + 5), (5, 300 * S)];
        assert_eq!(snapshot_at(&l, 99), None);
        assert_eq!(snapshot_at(&l, 100), Some((2, 100 * S)));
        assert_eq!(snapshot_at(&l, 200), Some((4, 200 * S + 5)), "the whole second counts");
        assert_eq!(snapshot_at(&l, 299), Some((4, 200 * S + 5)));
        assert_eq!(snapshot_at(&l, i64::MAX), Some((5, 300 * S)));
        assert_eq!(snapshot_at(&l, -1), None);
        assert_eq!(snapshot_at(&[(3, 0)], 1_000), None, "a snapshot without a wall-clock time never");
    }

    #[test]
    fn the_generation_of_a_time() {
        let a = At { generation: Some(1), running: Some(2), next: Some(2), pin: None, writable: true };
        assert_eq!(pin_for(&a).0, Some(1));
        assert_eq!(pin_for(&At { generation: Some(2), ..a }).0, None, "2 starts next anyway");
        assert_eq!(pin_for(&At { generation: Some(2), next: Some(1), pin: Some(1), ..a }).0, Some(2), "a pin to 1 is replaced");
        let (p, why) = pin_for(&At { generation: None, ..a });
        assert!(p.is_none() && why.contains("stays"));
        assert_eq!(one_line("a\nb\rc"), "a b c");
    }
}
