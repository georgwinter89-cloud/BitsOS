//! One changing run at a time (install, remove, update, switch, rollback, refresh, the timer's check), in a
//! thread of class Background (downloads and unpacking, B4), with bitspkg's progress sink pointed at the asking
//! client. The `/apps` lock inside the library stays the real guard (a root `bitspkg` command line may run at
//! the same time); this one only keeps two of the service's own runs from mixing their progress. Every wait has
//! a deadline: a second run waits at most `BUSY_WAIT`; a client stops waiting at its op's deadline while the run
//! finishes by itself (a run is never cut by the service: the cut safety is for power cuts, not for impatience).

use crate::Ctx;
use bitspkg::progress::Item;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BUSY_WAIT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct Work {
    busy: Mutex<Option<String>>,
}

impl Work {
    /// What runs now (for `status` and the busy refusal).
    pub fn busy(&self) -> Option<String> {
        self.busy.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn claim(&self, what: &str, wait: Duration) -> Result<(), String> {
        let end = Instant::now() + wait;
        loop {
            {
                let mut b = self.busy.lock().unwrap_or_else(|p| p.into_inner());
                match b.as_ref() {
                    None => {
                        *b = Some(what.to_string());
                        return Ok(());
                    }
                    Some(other) if Instant::now() >= end => {
                        return Err(format!("bitspkgd arbeitet gerade an: {other}; bitte später erneut versuchen"));
                    }
                    Some(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    fn release(&self) {
        *self.busy.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Clears the sink and the claim also when the run panics.
struct Release(Arc<Ctx>);

impl Drop for Release {
    fn drop(&mut self) {
        bitspkg::progress::set(None);
        self.0.work.release();
    }
}

/// Runs `job` as the one changing run; `on` sees its progress items in order. Err: busy, no thread, or the
/// deadline passed (the run goes on and ends by itself).
pub fn run<T: Send + 'static>(cx: &Arc<Ctx>, what: &str, deadline: Duration, wait: Duration, job: impl FnOnce(&Ctx) -> T + Send + 'static, on: &mut dyn FnMut(Item)) -> Result<T, String> {
    cx.work.claim(what, wait)?;
    let (itx, irx) = mpsc::channel();
    let (dtx, drx) = mpsc::channel();
    bitspkg::progress::set(Some(itx));
    let guard = Release(cx.clone());
    let label = what.to_string();
    let spawned = std::thread::Builder::new().name("pkg-work".into()).spawn(move || {
        let g = guard;
        // SCHED_IDLE is V2's Background class; a refusal keeps the run in the service's class, said in an event.
        if let Err(e) = usys::sched_setscheduler(0, usys::SCHED_IDLE, 0) {
            crate::event("pkg.daemon.class", &[("run", &label), ("errno", &e.to_string())]);
        }
        let r = job(&g.0);
        drop(g);
        let _ = dtx.send(r);
    });
    if let Err(e) = spawned {
        // The guard went with the closure that could not start: dropped there, so the claim is free again.
        crate::event("pkg.daemon.thread", &[("run", what), ("reason", &e.to_string())]);
        return Err(format!("kein Thread für {what}: {e}"));
    }
    let end = Instant::now() + deadline;
    loop {
        while let Ok(i) = irx.try_recv() {
            on(i);
        }
        match drx.recv_timeout(POLL) {
            Ok(r) => {
                while let Ok(i) = irx.try_recv() {
                    on(i);
                }
                return Ok(r);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                crate::event("pkg.daemon.run_lost", &[("run", what)]);
                return Err(format!("{what}: die Arbeit endete ohne Ergebnis (Fehler im Dienst, siehe Journal)"));
            }
        }
        if Instant::now() >= end {
            crate::event("pkg.op.timeout", &[("run", what), ("after_s", &deadline.as_secs().to_string())]);
            return Err(format!("{what}: keine Antwort nach {} s; die Arbeit läuft im Hintergrund zu Ende (bitspkgd status)", deadline.as_secs()));
        }
    }
}
