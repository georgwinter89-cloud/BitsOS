//! WY-7: launch feedback. A program bitscomp starts (Strg+Alt+Enter, the circle menu, the address bar, a
//! session) shows its window only after it started: foot needs 35-80 ms in QEMU, more while ten start at once.
//! Until then the press had no answer on the screen, and the kernel's `latency` tool (IN-6, path `program`)
//! counted it up to that first window (Georg's outliers of 21.10.). So the pointer carries a small "starting"
//! mark from the frame after the start until the program's first window (its process group: a launcher such as
//! Cobalt's maps the window from a child) or LIMIT, whatever comes first; a program that ends takes its mark
//! along. The mark is drawn once (no animation), so a waiting start costs no frames.

use std::time::Duration;

/// A program without a window of its own (a script) or a slow one: the mark goes after this.
pub const LIMIT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Launches {
    /// Started pid (= its process group, `spawn` makes it the leader) and the deadline (`pacer` clock).
    waiting: Vec<(u32, Duration)>,
}

impl Launches {
    pub fn started(&mut self, pid: u32, now: Duration) {
        self.waiting.retain(|(p, _)| *p != pid);
        self.waiting.push((pid, now + LIMIT));
    }

    /// A window of a process in group `pgid` drew its first frame. True when a mark went.
    pub fn mapped(&mut self, pgid: u32) -> bool {
        let n = self.waiting.len();
        self.waiting.retain(|(p, _)| *p != pgid);
        self.waiting.len() != n
    }

    /// Keeps the starts whose pid still runs. True when a mark went.
    pub fn retain_alive(&mut self, alive: impl Fn(u32) -> bool) -> bool {
        let n = self.waiting.len();
        self.waiting.retain(|(p, _)| alive(*p));
        self.waiting.len() != n
    }

    /// Drops the starts past their deadline. True when a mark went.
    pub fn tick(&mut self, now: Duration) -> bool {
        let n = self.waiting.len();
        self.waiting.retain(|(_, d)| *d > now);
        self.waiting.len() != n
    }

    /// When the next mark runs out (the loop wakes then).
    pub fn due(&self) -> Option<Duration> {
        self.waiting.iter().map(|(_, d)| *d).min()
    }

    pub fn busy(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// Starts still waiting for their window (`ctl stats`).
    pub fn count(&self) -> usize {
        self.waiting.len()
    }
}

/// The mark: a ring of eight dots (four dark, four light, so it reads on any background) in a `d` x `d` ARGB
/// buffer with a one-pixel dark outline per dot. Pure, so the shape is host-tested.
pub fn mark(d: i32) -> Vec<u8> {
    let d = d.max(8);
    let c = (d as f64 - 1.0) / 2.0;
    let ring = c * 0.68;
    let dot = (d as f64 * 0.14).max(1.5);
    let mut px = vec![0u32; (d * d) as usize];
    for i in 0..8 {
        let a = std::f64::consts::TAU * i as f64 / 8.0;
        let (cx, cy) = (c + ring * a.cos(), c + ring * a.sin());
        let body = if i < 4 { 0xff20_2020 } else { 0xffff_ffff };
        for y in 0..d {
            for x in 0..d {
                let r = ((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt();
                let p = &mut px[(y * d + x) as usize];
                if r <= dot {
                    *p = body;
                } else if r <= dot + 1.0 && *p == 0 {
                    *p = 0xff00_0000;
                }
            }
        }
    }
    px.iter().flat_map(|p| p.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_start_waits_for_its_window_or_the_limit() {
        let mut l = Launches::default();
        assert!(!l.busy() && l.due().is_none());
        l.started(10, S);
        l.started(11, S * 2);
        assert!(l.busy());
        assert_eq!(l.due(), Some(S + LIMIT));
        assert!(!l.mapped(99), "another program's window");
        assert!(l.mapped(10));
        assert!(!l.tick(S + LIMIT), "11 still has time");
        assert!(l.tick(S * 2 + LIMIT), "11 ran out");
        assert!(!l.busy());
    }

    #[test]
    fn an_ended_program_takes_its_mark_and_a_restart_counts_once() {
        let mut l = Launches::default();
        l.started(20, S);
        l.started(20, S * 3);
        assert_eq!(l.due(), Some(S * 3 + LIMIT));
        l.started(21, S);
        assert!(l.retain_alive(|p| p == 21));
        assert!(!l.retain_alive(|p| p == 21));
        assert!(l.mapped(21));
        assert!(!l.busy());
    }

    #[test]
    fn the_mark_has_dark_and_light_dots_and_a_clear_middle() {
        let d = 16;
        let b = mark(d);
        assert_eq!(b.len(), (d * d * 4) as usize);
        let px: Vec<u32> = b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        assert!(px.contains(&0xff20_2020) && px.contains(&0xffff_ffff) && px.contains(&0xff00_0000));
        assert_eq!(px[((d / 2) * d + d / 2) as usize], 0, "the middle stays clear");
        assert_eq!(px[0], 0, "the corner stays clear");
    }
}
