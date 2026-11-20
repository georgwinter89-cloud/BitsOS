//! What a helper started, as the kernel's process table shows it. Ending the helper's process group is
//! not enough: a program it starts may leave the group (a shell command in a session of its own), and once
//! its parents are gone nothing leads back to the helper any more. So the family is written down while it
//! still can be, and whatever of it is still there at the end is ended too.

use std::collections::BTreeSet;

/// A process: its number and when it started (field 22 of `/proc/<pid>/stat`), so that a number the
/// kernel gave out again is never taken for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Proc {
    pub pid: u32,
    started: u64,
}

/// (parent, start time) of `pid`, or None when it is gone or already ended (a zombie runs nothing).
fn stat(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The second field is the program's name in brackets and may hold anything: count from the last one.
    let mut rest = stat.rsplit_once(')')?.1.split_whitespace();
    let state = rest.next()?;
    if state == "Z" || state == "X" {
        return None;
    }
    let parent = rest.next()?.parse().ok()?;
    // state is field 3; start time is field 22.
    let started = rest.nth(17)?.parse().ok()?;
    Some((parent, started))
}

/// Every running process that descends from `head`, `head` not counted.
pub fn family(head: u32) -> Vec<Proc> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    let all: Vec<(u32, u32, u64)> = dir
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| stat(pid).map(|(parent, started)| (pid, parent, started)))
        .collect();
    let mut inside = BTreeSet::from([head]);
    let mut found = Vec::new();
    // A child is listed before its parent sometimes: go over the table until nothing new is found.
    loop {
        let new: Vec<_> = all.iter().filter(|(pid, parent, _)| inside.contains(parent) && !inside.contains(pid)).copied().collect();
        if new.is_empty() {
            return found;
        }
        for (pid, _, started) in new {
            inside.insert(pid);
            found.push(Proc { pid, started });
        }
    }
}

/// True while `p` runs (the same process, not a later one with its number).
pub fn runs(p: &Proc) -> bool {
    stat(p.pid).is_some_and(|(_, started)| started == p.started)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_is_found_and_known_again() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("sleep");
        let me = std::process::id();
        let mine = family(me);
        let p = *mine.iter().find(|p| p.pid == child.id()).expect("the child is in the family");
        assert!(runs(&p) && !runs(&Proc { started: p.started + 1, ..p }));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!runs(&p) && family(me).iter().all(|q| q.pid != p.pid));
    }
}
