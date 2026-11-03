//! IN-8: a program's own frames (path `program`: it read input from /dev/input, fbdev presented its next frame)
//! measure the program's drawing, which BitsOS cannot make faster (a browser's software compositor needs about
//! 24 ms a frame). They keep their numbers and counters but warn nobody: per program, a window of a minute
//! collects its frames, and a closed window with late frames becomes one Info summary (frames, late, worst,
//! median). Pure state here, so the host tests drive it with their own clock.

use super::{BUDGET_NS, STALE_NS};
use alloc::vec::Vec;

/// One summary per program at most this often.
pub const WINDOW_NS: u64 = 60_000_000_000;
/// `TASK_COMM_LEN`.
pub const COMM_LEN: usize = 16;

/// The program that read the input a frame shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reader {
    pub pid: u32,
    pub comm: [u8; COMM_LEN],
}

impl Reader {
    pub const UNKNOWN: Reader = Reader { pid: 0, comm: [0; COMM_LEN] };

    /// The comm as text (up to its NUL; a cut multi-byte character is left out).
    pub fn name(&self) -> &str {
        let n = self.comm.iter().position(|&b| b == 0).unwrap_or(COMM_LEN);
        match core::str::from_utf8(&self.comm[..n]) {
            Ok(s) => s,
            Err(e) => core::str::from_utf8(&self.comm[..e.valid_up_to()]).unwrap_or(""),
        }
    }
}

/// Log-linear buckets of microseconds, 8 per octave, so the median is within 1/16 of the truth without keeping
/// every frame; they reach the stale limit (1 s), which no counted frame passes.
pub const FINE: usize = 144;

fn fine(us: u64) -> usize {
    if us < 8 {
        return us as usize;
    }
    let o = us.ilog2() as usize;
    ((o - 2) * 8 + ((us >> (o - 3)) & 7) as usize).min(FINE - 1)
}

/// The middle (µs) of bucket `i`; buckets below 16 µs are one µs wide.
fn fine_mid(i: usize) -> u64 {
    if i < 16 {
        return i as u64;
    }
    let shift = i / 8 - 1;
    ((8 + (i % 8) as u64) << shift) + (1u64 << shift) / 2
}

/// One program's frames since `start_ns`.
#[derive(Clone, Debug)]
pub struct Window {
    pub who: Reader,
    pub start_ns: u64,
    pub frames: u64,
    pub late: u64,
    pub worst_ns: u64,
    hist: [u32; FINE],
}

impl Window {
    fn new(who: Reader, now: u64) -> Window {
        Window { who, start_ns: now, frames: 0, late: 0, worst_ns: 0, hist: [0; FINE] }
    }

    fn add(&mut self, t_ns: u64) {
        self.frames += 1;
        self.late += (t_ns > BUDGET_NS) as u64;
        self.worst_ns = self.worst_ns.max(t_ns);
        let b = &mut self.hist[fine(t_ns / 1000)];
        *b = b.saturating_add(1);
    }

    /// The median frame (lower median), from the buckets: at most the worst frame; 0 without frames.
    pub fn median_ns(&self) -> u64 {
        let want = self.frames.div_ceil(2);
        let mut seen = 0u64;
        for (i, &n) in self.hist.iter().enumerate() {
            seen += n as u64;
            if want > 0 && seen >= want {
                return (fine_mid(i) * 1000).min(self.worst_ns);
            }
        }
        self.worst_ns
    }

    pub fn due(&self, now: u64) -> bool {
        now.saturating_sub(self.start_ns) >= WINDOW_NS
    }
}

/// The open windows, one per program that drew input in the last minute.
pub struct Programs {
    windows: Vec<Window>,
}

impl Programs {
    pub const fn new() -> Programs {
        Programs { windows: Vec::new() }
    }

    /// A frame of `who` that showed its input after `t_ns`. False: a new window needs room first (`grow`,
    /// outside the lock). Stale frames are no program's reaction and stay out.
    pub fn add(&mut self, who: Reader, t_ns: u64, now: u64) -> bool {
        if t_ns > STALE_NS {
            return true;
        }
        if let Some(w) = self.windows.iter_mut().find(|w| w.who == who) {
            w.add(t_ns);
            return true;
        }
        if self.windows.len() == self.windows.capacity() {
            return false;
        }
        let mut w = Window::new(who, now);
        w.add(t_ns);
        self.windows.push(w);
        true
    }

    /// The capacity `add` wants after a false.
    pub fn room_wanted(&self) -> usize {
        (self.windows.len() + 1).saturating_mul(2)
    }

    /// Moves the windows into `more` when it is larger (no allocation here); hands back what to drop outside
    /// the lock.
    pub fn grow(&mut self, mut more: Vec<Window>) -> Vec<Window> {
        if more.capacity() <= self.windows.capacity() {
            return more;
        }
        more.extend(self.windows.drain(..));
        core::mem::replace(&mut self.windows, more)
    }

    /// Closes one window a minute old.
    pub fn take_due(&mut self, now: u64) -> Option<Window> {
        let i = self.windows.iter().position(|w| w.due(now))?;
        Some(self.windows.swap_remove(i))
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// Forgets every window; hands back the storage to drop outside the lock.
    pub fn clear(&mut self) -> Vec<Window> {
        core::mem::take(&mut self.windows)
    }
}

impl Default for Programs {
    fn default() -> Programs {
        Programs::new()
    }
}

#[cfg(test)]
pub(crate) fn fine_parts(us: u64) -> (usize, u64) {
    let i = fine(us);
    (i, fine_mid(i))
}
