//! PLAN K5: whether a process belongs to the assistant's process tree. The kernel keeps the tag (wave EINS-C); this
//! crate is the one place user space asks it, so bits and bitscomp change together when the kernel's interface is
//! settled.
//!
//! The kernel's answer is `/proc/<pid>/bitsos_tree` (`EINS_C_FILE`, eins-c.md §1): `none`, or
//! `tree=<n> root=<pid> since=<ns> chat=<label|->`. It is absent on a kernel without the tag (Linux, the host tests)
//! and then the process is outside. A second marker only ever adds the tag: the environment variable
//! `BITSOS_FAKE_TREE=1` of the process (read from `/proc/<pid>/environ`), inherited over fork and exec as the tag
//! is. The host scenarios use it to put a `bits` into the tree. It cannot take anyone out of the tree, so it gives
//! nobody more than the kernel does.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// The kernel's file per process (the brief to wave EINS-C names it; their eins-c.md is the contract).
pub const EINS_C_FILE: &str = "bitsos_tree";
/// The marker of the host scenarios (see the crate's text).
pub const FAKE_ENV: &str = "BITSOS_FAKE_TREE";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tree {
    /// Outside the tree: Georg's own terminal, a service, a script he started.
    Out,
    /// In the assistant's tree; the chat it belongs to when the service named one, and the tree's first process
    /// (the service itself; unknown for the host marker or a peer that is gone).
    In { chat: Option<String>, root: Option<u32> },
}

impl Tree {
    pub fn inside(&self) -> bool {
        matches!(self, Tree::In { .. })
    }
}

/// Whether this kernel keeps the tag at all (BitsOS with EINS-C); on another one nobody is in a tree but by the
/// host marker.
pub fn kernel_knows() -> bool {
    Path::new("/proc/self").join(EINS_C_FILE).exists()
}

/// The process `pid` (as SO_PEERCRED named it, or this process's own).
pub fn of_pid(pid: u32) -> Tree {
    of_proc(&PathBuf::from(format!("/proc/{pid}")))
}

/// A socket peer (SO_PEERCRED's pid, possibly gone by now): like `of_pid`, but a peer that cannot be seen any more
/// (ended, a zombie, no pid at all) counts as inside. The answer is asked after the line arrived; a process in the
/// tree that writes a line and exits at once must not pass as Georg's own.
pub fn of_peer(pid: Option<u32>) -> Tree {
    match pid.filter(|p| *p > 0) {
        Some(pid) => of_live(&PathBuf::from(format!("/proc/{pid}"))),
        None => Tree::In { chat: None, root: None },
    }
}

/// `of_proc`, then whether the process still lives: the tag read from a folder that went away meanwhile proves
/// nothing.
fn of_live(dir: &Path) -> Tree {
    match of_proc(dir) {
        Tree::Out if !alive(dir) => Tree::In { chat: None, root: None },
        t => t,
    }
}

/// `stat`'s third field (`pid (comm) S ...`; comm may hold spaces and parentheses, so after the last `)`).
fn alive(dir: &Path) -> bool {
    let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else { return false };
    let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next());
    !matches!(state, None | Some("Z" | "X" | "x"))
}

pub fn of_self() -> Tree {
    let fake = std::env::var(FAKE_ENV).ok().filter(|v| v == "1").is_some();
    match of_proc(Path::new("/proc/self")) {
        Tree::Out if fake => Tree::In { chat: None, root: None },
        t => t,
    }
}

/// `dir` is a process's folder in /proc (tests give another one).
pub fn of_proc(dir: &Path) -> Tree {
    if let Some(t @ Tree::In { .. }) = std::fs::read_to_string(dir.join(EINS_C_FILE)).ok().and_then(|t| parse(&t)) {
        return t;
    }
    let fake = std::fs::read(dir.join("environ")).is_ok_and(|env| env.split(|b| *b == 0).any(|kv| kv == format!("{FAKE_ENV}=1").as_bytes()));
    if fake { Tree::In { chat: None, root: None } } else { Tree::Out }
}

/// The kernel file's text (`none`, or `tree=<n> root=<pid> since=<ns> chat=<label|->`). Anything else is not
/// understood (None). Outside or not understood, the marker may still add the tag.
pub fn parse(text: &str) -> Option<Tree> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let field = |k: &str| words.iter().find_map(|w| w.strip_prefix(k));
    match words.first()? {
        &"none" => Some(Tree::Out),
        w if w.strip_prefix("tree=").and_then(|n| n.parse::<u64>().ok()).is_some_and(|n| n > 0) => Some(Tree::In {
            chat: field("chat=").filter(|c| !c.is_empty() && *c != "-").map(str::to_string),
            root: field("root=").and_then(|r| r.parse().ok()),
        }),
        _ => None,
    }
}

/// Whether `pid` is the first process of its tree: the assistant's service itself, which judges its own tools by
/// the level (the kernel tags `/bin/assist` at its start by the compositor; a process inside a tree can never start
/// another, so no peer of the service becomes a root).
pub fn is_root(pid: u32) -> bool {
    matches!(of_pid(pid), Tree::In { root: Some(r), .. } if r == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitstree-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_kernel_file_decides_and_names_the_chat() {
        assert_eq!(parse("none\n"), Some(Tree::Out));
        assert_eq!(parse("tree=1 root=412 since=8812 chat=-\n"), Some(Tree::In { chat: None, root: Some(412) }));
        assert_eq!(parse("tree=3 root=9 since=1 chat=c7\n"), Some(Tree::In { chat: Some("c7".into()), root: Some(9) }));
        assert_eq!(parse("tree=0 root=9"), None, "tree 0 is no tree");
        assert_eq!(parse("ja"), None);
        assert_eq!(parse("1"), None, "an older guess of the shape is not understood");
    }

    #[test]
    fn a_peer_that_cannot_be_seen_is_inside() {
        assert!(of_peer(None).inside(), "no peer credentials");
        assert!(of_peer(Some(0)).inside(), "pid 0: not in our pid namespace");
        assert_eq!(of_peer(Some(std::process::id())), Tree::Out, "this test runs outside the tree");
        let d = dir("gone");
        assert!(of_live(&d).inside(), "no stat: the process ended");
        std::fs::write(d.join("stat"), "7 (a b) c) Z 1 7\n").unwrap();
        assert!(of_live(&d).inside(), "a zombie");
        std::fs::write(d.join("stat"), "7 (a b) c) S 1 7\n").unwrap();
        assert_eq!(of_live(&d), Tree::Out, "alive and untagged");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_real_child_that_ended_is_inside() {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        // Not reaped yet: a zombie (its folder stays until wait).
        let t0 = std::time::Instant::now();
        while alive(&PathBuf::from(format!("/proc/{pid}"))) && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(of_peer(Some(pid)).inside(), "the zombie");
        c.wait().unwrap();
        assert!(of_peer(Some(pid)).inside(), "reaped: its folder is gone");
    }

    #[test]
    fn the_marker_only_adds_the_tag() {
        let d = dir("marker");
        assert_eq!(of_proc(&d), Tree::Out, "nothing there: outside");
        std::fs::write(d.join("environ"), b"HOME=/x\0BITSOS_FAKE_TREE=1\0").unwrap();
        assert!(of_proc(&d).inside());
        std::fs::write(d.join(EINS_C_FILE), "tree=2 root=5 since=1 chat=a\n").unwrap();
        assert_eq!(of_proc(&d), Tree::In { chat: Some("a".into()), root: Some(5) });
        std::fs::write(d.join(EINS_C_FILE), "none\n").unwrap();
        assert!(of_proc(&d).inside(), "the marker adds the tag to a process the kernel has outside");
        std::fs::write(d.join("environ"), b"BITSOS_FAKE_TREE=0\0").unwrap();
        std::fs::write(d.join(EINS_C_FILE), "tree=2 root=5 since=1 chat=-\n").unwrap();
        assert!(of_proc(&d).inside(), "no marker takes a process out");
        let _ = std::fs::remove_dir_all(&d);
    }
}
