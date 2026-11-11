//! WY-21: the GPU desktop notices a lost device and never stays a frozen picture (desk-INT-W31 open point 1).
//!
//! Four signs, any of them is enough:
//! - a new open of the render node answers ENODEV (or the node is gone): what V2's amdgpu does once GX-9a's ladder found
//!   no rung that works (`hw/mod.rs` `open`), and the fake with `drm_fake_hang=lose`. A look every `LOOK_EVERY`, on a
//!   thread of its own: an open that never answers holds that thread, nothing else.
//! - the GL context reports a reset (`render_gl`: a robust context, so Mesa reports instead of aborting the process);
//! - the GPU has not finished a frame for `LATE` (`late`);
//! - a present (or the wait for the last flip before a frame) has not returned for `STALL`: stuck in the driver. Nothing in
//!   this process can draw any more, so the stall thread ends it with `LOST_EXIT`, and the rescue build that handed
//!   over and waits (`flavour`) takes the screen with the session. Only the waits on the display count: a slow first
//!   frame (shaders on a cold boot), a new renderer or a burst of imports is work, not a hang; a loop turn as a whole
//!   gets `TURN_STALL`, the bound for a hang anywhere else.
//!
//! The first three switch the renderer in this process (`main.rs` `leave_gpu`), so every client that survives the loss
//! keeps its window. The looks cost one open and close of the node a second and one atomic store per loop turn.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The gpu build's exit status when it gave the GPU up and cannot draw on: the waiting rescue build takes over.
pub const LOST_EXIT: i32 = 75;
/// How often the node is looked at.
pub const LOOK_EVERY: Duration = Duration::from_secs(1);
/// A present longer than this is stuck: a flip lands within a refresh, and V2's pan gives up after two
/// (gopfb `fbdev/flip.rs`).
pub const STALL: Duration = Duration::from_secs(6);
/// A loop turn longer than this, outside the presents, is a hang elsewhere (a driver call that never returns). Far above
/// a cold first frame or a new renderer.
pub const TURN_STALL: Duration = Duration::from_secs(30);
/// The GPU's work on a frame unfinished this long: the device does not come back by itself. Above slow GL work (a
/// software rasteriser's first blur), the ladder's own timeouts are GX-9a's.
pub const LATE: Duration = Duration::from_secs(10);

/// What one look at the node says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    Opens,
    /// The device is lost or the node gone.
    Gone(String),
    /// Anything else (EMFILE, EACCES, EBUSY): nothing about the device.
    Other,
}

pub fn look(node: &str) -> Look {
    match std::fs::OpenOptions::new().read(true).write(true).open(node) {
        Ok(_) => Look::Opens,
        Err(e) => match e.raw_os_error() {
            Some(libc::ENODEV | libc::ENXIO | libc::ENOENT) => Look::Gone(format!("a new open of {node} answers {e}")),
            _ => Look::Other,
        },
    }
}

/// `look` bounded: an open that does not answer within `limit` says nothing either way.
pub fn look_within(node: &str, limit: Duration) -> Look {
    let (tx, rx) = std::sync::mpsc::channel();
    let n = node.to_string();
    if std::thread::Builder::new().name("bitscomp-gpu-look".into()).spawn(move || drop(tx.send(look(&n)))).is_err() {
        return Look::Other;
    }
    rx.recv_timeout(limit).unwrap_or(Look::Other)
}

/// Why the loop is stuck, if it is: `waiting` is when the present under way began (None: none is), `last_turn` when the
/// loop last came round (None: not yet), `now` the clock.
pub fn stalled(waiting: Option<Duration>, last_turn: Option<Duration>, now: Duration) -> Option<&'static str> {
    if waiting.is_some_and(|t| now.saturating_sub(t) > STALL) {
        return Some("a present has not returned");
    }
    last_turn.is_some_and(|t| now.saturating_sub(t) > TURN_STALL).then_some("the main loop has not come round")
}

/// Whether the GPU's work is late past `LATE`: `since` is when the frames began to be late (None: they are not).
pub fn late(since: Option<Duration>, now: Duration) -> bool {
    since.is_some_and(|t| now.saturating_sub(t) > LATE)
}

struct Shared {
    lost: Mutex<Option<String>>,
    /// The loop's last turn, ns on the monotonic clock; 0 before the first.
    turn_ns: AtomicU64,
    /// When the present under way began; 0: none is.
    wait_ns: AtomicU64,
    stop: AtomicBool,
}

pub struct Watch {
    shared: Arc<Shared>,
}

impl Watch {
    /// Both threads for a renderer on `node`; None when a thread does not start (said once: the desktop runs unwatched).
    pub fn start(node: &str) -> Option<Watch> {
        let shared = Arc::new(Shared { lost: Mutex::new(None), turn_ns: AtomicU64::new(0), wait_ns: AtomicU64::new(0), stop: AtomicBool::new(false) });
        let (s, n) = (shared.clone(), node.to_string());
        let looks = std::thread::Builder::new().name("bitscomp-gpu-watch".into()).spawn(move || {
            while !s.stop.load(Ordering::Acquire) {
                std::thread::sleep(LOOK_EVERY);
                if let Look::Gone(why) = look(&n) {
                    *s.lost.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                    return;
                }
            }
        });
        let s = shared.clone();
        let stall = std::thread::Builder::new().name("bitscomp-gpu-stall".into()).spawn(move || {
            while !s.stop.load(Ordering::Acquire) {
                std::thread::sleep(LOOK_EVERY);
                let at = |a: &AtomicU64| Some(a.load(Ordering::Acquire)).filter(|&t| t != 0).map(Duration::from_nanos);
                let (wait, turn) = (at(&s.wait_ns), at(&s.turn_ns));
                let now = crate::pacer::now();
                if let Some(what) = stalled(wait, turn, now).filter(|_| !s.stop.load(Ordering::Acquire)) {
                    let since = wait.filter(|_| what.starts_with("a present")).or(turn).unwrap_or(now);
                    end_stuck(what, now.saturating_sub(since));
                }
            }
        });
        if looks.is_err() || stall.is_err() {
            eprintln!("bitscomp: gpu: no thread to watch the render node: a lost GPU is seen only by GL");
            shared.stop.store(true, Ordering::Release);
            return None;
        }
        eprintln!("bitscomp: gpu: watching {node} (a look every {} s, a stuck loop after {} s)", LOOK_EVERY.as_secs(), STALL.as_secs());
        Some(Watch { shared })
    }

    /// The loop came round.
    pub fn turn(&self, now: Duration) {
        self.shared.turn_ns.store(now.as_nanos().max(1) as u64, Ordering::Release);
    }

    /// A wait on the display begins (`Some(now)`) or ended (`None`).
    pub fn waiting(&self, since: Option<Duration>) {
        self.shared.wait_ns.store(since.map_or(0, |t| t.as_nanos().max(1) as u64), Ordering::Release);
    }

    /// Why the device is lost, once.
    pub fn lost(&self) -> Option<String> {
        self.shared.lost.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The stuck process ends. Its line is written with write(2) from a buffer on this stack: the stuck thread may hold
/// stderr's lock or the allocator's, and nothing it holds may be waited for here.
fn end_stuck(what: &str, held: Duration) -> ! {
    use std::io::Write as _;
    let mut buf = [0u8; 256];
    let mut out = std::io::Cursor::new(&mut buf[..]);
    let _ = writeln!(
        out,
        "bitscomp: gpu: {what} for {} ms (stuck in the driver): ending with {LOST_EXIT}, the rescue build takes over",
        held.as_millis()
    );
    let n = out.position() as usize;
    // SAFETY: write(2) of our own stack buffer to fd 2, then _exit: nothing of this process runs after it.
    unsafe {
        libc::write(2, buf.as_ptr().cast(), n);
        libc::_exit(LOST_EXIT)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_present_past_the_bound_is_a_stall() {
        let s = Duration::from_secs;
        assert_eq!(stalled(None, None, s(100)), None, "before the first turn nothing is stuck");
        assert_eq!(stalled(Some(s(100)), Some(s(100)), s(100) + STALL), None);
        assert!(stalled(Some(s(100)), Some(s(100)), s(101) + STALL).unwrap().starts_with("a present"));
        // A slow first frame, a new renderer, a burst of imports: a long turn with no present under way is work.
        assert_eq!(stalled(None, Some(s(100)), s(101) + STALL), None, "a long turn outside a present is not a stuck present");
        assert_eq!(stalled(None, Some(s(100)), s(100) + TURN_STALL), None);
        assert!(stalled(None, Some(s(100)), s(101) + TURN_STALL).unwrap().starts_with("the main loop"));
        assert_eq!(stalled(Some(s(100)), Some(s(100)), s(50)), None, "a clock read before the turn");
        assert!(!late(None, s(100)));
        assert!(!late(Some(s(10)), s(10) + LATE));
        assert!(late(Some(s(10)), s(11) + LATE));
    }

    #[test]
    fn only_a_missing_device_is_a_loss() {
        let d = std::env::temp_dir().join(format!("bitscomp-wy21-look-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("node");
        std::fs::write(&f, b"").unwrap();
        let p = f.to_string_lossy().into_owned();
        assert_eq!(look(&p), Look::Opens);
        assert_eq!(look_within(&p, Duration::from_secs(5)), Look::Opens);
        // A directory opened for writing: EISDIR, not a lost device.
        assert_eq!(look(&d.to_string_lossy()), Look::Other);
        std::fs::remove_file(&f).unwrap();
        assert!(matches!(look(&p), Look::Gone(w) if w.contains("answers")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_watch_hears_the_node_go() {
        let d = std::env::temp_dir().join(format!("bitscomp-wy21-watch-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("node");
        std::fs::write(&f, b"").unwrap();
        let w = Watch::start(&f.to_string_lossy()).expect("threads");
        w.turn(crate::pacer::now());
        std::thread::sleep(LOOK_EVERY + Duration::from_millis(300));
        assert_eq!(w.lost(), None);
        std::fs::remove_file(&f).unwrap();
        let end = std::time::Instant::now() + LOOK_EVERY * 5;
        let why = loop {
            w.turn(crate::pacer::now());
            if let Some(why) = w.lost() {
                break why;
            }
            assert!(std::time::Instant::now() < end, "the watch did not see the node go");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(why.contains("node"), "{why}");
        assert_eq!(w.lost(), None, "said once");
        drop(w);
        let _ = std::fs::remove_dir_all(&d);
    }
}
