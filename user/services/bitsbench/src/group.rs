//! The programs a run starts (Brave, the game, Steam, the profiler), each in a process group of its own. A
//! `Group` ends its group when dropped, so a run that returns early or unwinds leaves nothing running. A run
//! that is killed outright cannot drop anything: every group is also noted in `run.children` (pgid, the
//! leader's start time, the boot) and the next start of bitsbench ends what is still there (`reap`).

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Grace of a group ended by a drop (the regular paths end theirs with their own grace first).
const DROP_GRACE: Duration = Duration::from_secs(2);
/// A noted boot within this many seconds of this one is this boot (the clock and uptime round apart).
const SAME_BOOT_S: i64 = 30;

/// Where a run notes its groups (None: nowhere, the CachyOS guide's one-off runs may do without).
#[derive(Clone, Debug, Default)]
pub struct Kids(pub Option<PathBuf>);

impl Kids {
    pub fn at(root: &Path) -> Kids {
        Kids(Some(root.join("run.children")))
    }

    fn note(&self, pid: u32) {
        let Some(p) = &self.0 else { return };
        let Some(start) = start_ticks(pid) else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{pid} {start} {}", boot_s().unwrap_or(0));
        }
    }

    /// The run is over and every group it started has been ended.
    pub fn clear(&self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub struct Group(Option<Child>);

impl Group {
    pub fn spawn(c: &mut Command, kids: &Kids) -> std::io::Result<Group> {
        let child = c.process_group(0).spawn()?;
        kids.note(child.id());
        Ok(Group(Some(child)))
    }

    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("a group is only emptied by end or drop")
    }

    pub fn id(&self) -> u32 {
        self.0.as_ref().map_or(0, Child::id)
    }

    /// Ends the group (TERM, KILL after `grace`).
    pub fn end(mut self, grace: Duration) {
        if let Some(mut c) = self.0.take() {
            crate::bench::end_group(&mut c, grace);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            crate::bench::end_group(&mut c, DROP_GRACE);
        }
    }
}

/// When this boot began (UTC s).
pub fn boot_s() -> Option<i64> {
    Some(crate::time::now_s() - crate::probe::uptime_s()? as i64)
}

/// Field 22 of /proc/<pid>/stat: the process's start in clock ticks after boot.
pub fn start_ticks(pid: u32) -> Option<u64> {
    stat_start(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

pub fn stat_start(stat: &str) -> Option<u64> {
    // The name in parentheses may hold spaces and parentheses; the fields after the last ')' start at field 3.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// One noted group: (pgid, the leader's start ticks, the boot it was noted in).
pub fn parse(text: &str) -> Vec<(u32, u64, i64)> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some((w.next()?.parse().ok()?, w.next()?.parse().ok()?, w.next()?.parse().ok()?))
        })
        .filter(|(pid, _, _)| *pid > 1)
        .take(256)
        .collect()
}

/// Whether the noted group may be signalled: noted in this boot, and its number not taken by another process
/// since. While any member of a group lives, Linux keeps its number from being handed out again; a leader that
/// is still there must be the noted one (same start time).
pub fn ours(noted: (u32, u64, i64), boot: Option<i64>, start_now: Option<u64>) -> bool {
    let (_, start, at) = noted;
    let same_boot = boot.is_some_and(|b| (b - at).abs() <= SAME_BOOT_S);
    same_boot && start_now.is_none_or(|s| s == start)
}

/// Ends every group a run before this one left behind (TERM, KILL after `grace`) and forgets the list. Returns
/// how many were signalled.
pub fn reap(file: &Path, grace: Duration) -> usize {
    let Ok(text) = std::fs::read_to_string(file) else { return 0 };
    let boot = boot_s();
    let groups: Vec<u32> = parse(&text).into_iter().filter(|n| ours(*n, boot, start_ticks(n.0))).map(|n| n.0).collect();
    let mut hit = 0;
    for g in &groups {
        if usys::kill(-(*g as isize), 15).is_ok() {
            hit += 1;
        }
    }
    let end = Instant::now() + grace;
    while hit > 0 && Instant::now() < end && groups.iter().any(|g| usys::kill(-(*g as isize), 0).is_ok()) {
        std::thread::sleep(Duration::from_millis(100));
    }
    for g in &groups {
        let _ = usys::kill(-(*g as isize), 9);
    }
    let _ = std::fs::remove_file(file);
    hit
}
