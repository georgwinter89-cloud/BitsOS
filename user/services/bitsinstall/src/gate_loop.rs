//! The installer's loop at the gate (MC-9a, MC-10 fixer). One thread owns the gate session: it fetches the
//! requests (`next`, short waits), hands each to the decider thread, sends the decisions back (`decide`) and
//! pushes the full-access table to the kernel (`grants`) whenever it changed. The decider waits for Georg's
//! answers (up to the dialog time); the gate thread never does, so a revoke at the keyboard reaches the
//! kernel's table within one `NEXT_WAIT_MS` even while a dialog is open (a client cannot hold its grant by
//! keeping the installer in a dialog of its own).

use crate::access::Access;
use crate::decide::{Confirmer, State};
use crate::link::{self, Link};
use crate::log;
use mcp_wire::installer::{Asked, Decision};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long one `next` waits in the kernel: the bound on how late a changed table reaches it.
pub const NEXT_WAIT_MS: u64 = 250;
/// How often the grants are swept when no request comes.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// Hands the kernel the full-access table when it changed (again at the next round when it failed).
pub fn push_access(gate_link: &mut dyn Link, access: &Access) {
    if !access.take_dirty() {
        return;
    }
    match link::grants(gate_link, &access.table()) {
        Ok(answer) if answer.contains("\"refused\":[]") => {}
        Ok(answer) => log::warn(&format!("the kernel did not take every full-access grant: {}", answer.trim())),
        Err(e) => {
            log::warn(&format!("the kernel did not take the full-access table (errno {e}); trying again"));
            access.mark_dirty();
        }
    }
}

/// Runs until the gate fails (its errno) or `stop` is set (tests: Ok).
pub fn run<C>(gate_link: &mut dyn Link, state: Arc<State>, access: Arc<Access>, confirmer: Arc<C>, now: fn() -> i64, stop: &AtomicBool) -> Result<(), i32>
where
    C: Confirmer + Send + Sync + 'static,
{
    let (ask_tx, ask_rx) = mpsc::channel::<Asked>();
    let (dec_tx, dec_rx) = mpsc::channel::<Decision>();
    let (st, c) = (state.clone(), confirmer.clone());
    // One decider: the dialogs come one after the other, as before.
    std::thread::Builder::new()
        .name(String::from("decider"))
        .spawn(move || {
            for a in ask_rx {
                let d = st.decide(&*c, &a, now());
                if dec_tx.send(d).is_err() {
                    return;
                }
            }
        })
        .map_err(|_| link::EIO)?;
    let mut swept = Instant::now();
    while !stop.load(Ordering::Acquire) {
        push_access(gate_link, &access);
        while let Ok(d) = dec_rx.try_recv() {
            // A yes for full access: the kernel's table first, so the asking client finds it at once.
            push_access(gate_link, &access);
            if let Err(e) = link::decide(gate_link, &d) {
                log::warn(&format!("request {}: the kernel did not take the decision (errno {e}); its caller is gone", d.id));
            }
        }
        match link::next(gate_link, NEXT_WAIT_MS)? {
            Some(a) => {
                if ask_tx.send(a).is_err() {
                    return Err(link::EIO);
                }
            }
            None => {}
        }
        if swept.elapsed() >= SWEEP_EVERY {
            swept = Instant::now();
            state.sweep(now());
            if access.sweep() {
                confirmer.access_changed();
            }
        }
    }
    Ok(())
}
