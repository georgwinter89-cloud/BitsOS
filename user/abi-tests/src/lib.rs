//! Transcript helper for the abi-* programs and the comparison rules shared with the checks.
//!
//! A transcript is one line per check, `<id>: <result>`, written at once (a crash leaves the lines so far),
//! between `wave X begin` and `wave X end (n checks)`. Results never contain values that differ between two
//! correct kernels (pids, addresses, times, port numbers); those are reduced to `ok`, `true`/`false` or a
//! relation. Linux is the oracle: `expected/wave-*.txt` are CachyOS's transcripts, and the checks (host test
//! `linux_truth`, the kernel's `ul.abi.*` ktests) compare against them with [`compare`].
#![forbid(unsafe_code)]

extern crate alloc;

pub mod compare;
pub use compare::{Diff, compare, parse, parse_known};

use std::fmt::Display;
use usys::SysResult;

pub struct T {
    wave: char,
    count: u32,
}

impl T {
    pub fn begin(wave: char) -> T {
        // Same umask on both machines, whatever the starter had.
        let _ = usys::umask(0o022);
        let t = T { wave, count: 0 };
        t.raw(&format!("wave {wave} begin\n"));
        t
    }

    fn raw(&self, s: &str) {
        let mut b = s.as_bytes();
        let mut tries = 0;
        while !b.is_empty() && tries < 100 {
            match usys::write(1, b) {
                Ok(n) if n > 0 => b = &b[n.min(b.len())..],
                _ => tries += 1,
            }
        }
    }

    pub fn say(&mut self, id: &str, res: impl Display) {
        self.count += 1;
        self.raw(&format!("{id}: {res}\n"));
    }
    /// The kernel's answer with its value (`= n` / `-ENAME`).
    pub fn sys(&mut self, id: &str, r: SysResult) {
        self.say(id, usys::show(r));
    }
    /// Only ok / -ENAME.
    pub fn kind<V>(&mut self, id: &str, r: &SysResult<V>) {
        self.say(id, usys::kind(r));
    }
    pub fn check(&mut self, id: &str, b: bool) {
        self.say(id, b);
    }
    /// A check that depends on an earlier one that failed.
    pub fn skip(&mut self, id: &str, why: &str) {
        self.say(id, format!("skipped ({why})"));
    }

    pub fn end(self) -> ! {
        self.raw(&format!("wave {} end ({} checks)\n", self.wave, self.count));
        usys::exit_group(0)
    }
}

/// A scratch directory under /tmp named after the wave and pid (the name is never printed).
pub fn scratch(wave: char) -> String {
    format!("/tmp/ul-abi-{}-{}", wave.to_ascii_lowercase(), usys::getpid().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_rules() {
        let exp = "wave A begin\na.x: true\na.y: = 3\na.z: -ENOENT\nwave A end (3 checks)\n";
        assert!(compare(exp, exp, "").ok());
        let got = "wave A begin\na.x: true\na.y: = 4\na.z: -ENOSYS\nwave A end (3 checks)\n";
        let d = compare(exp, got, "# comment\na.z: needs FS\n");
        assert_eq!(d.new, vec![("a.y".into(), "= 3".into(), "= 4".into())]);
        assert_eq!(d.known.len(), 1);
        assert!(!d.ok());
        let d = compare(exp, exp, "a.z: needs FS\n");
        assert_eq!(d.fixed, vec!["a.z".to_string()]);
        assert!(d.ok());
        // Cut short: missing lines are new differences.
        let d = compare(exp, "wave A begin\na.x: true\n", "");
        assert_eq!(d.new.len(), 3);
        let longer = "wave A begin\na.x: true\na.y: = 3\na.z: -ENOENT\na.w: 1\nwave A end (4 checks)\n";
        let d = compare(exp, longer, "");
        assert_eq!(d.extra, vec!["a.w".to_string()]);
        assert_eq!(d.new, vec![("wave A end".into(), "3 checks".into(), "4 checks".into())]);
        // A known entry covers the count line and an extra line alike.
        let d = compare(exp, longer, "wave A end: AB-3\na.w: AB-3\n");
        assert!(d.ok());
        assert_eq!(d.known.len(), 1);
    }
}
