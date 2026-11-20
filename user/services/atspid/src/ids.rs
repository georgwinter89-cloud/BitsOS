//! Stable element ids: `<window>/<n>` keeps its n while the object (its path in its program) exists. A number is
//! never given out twice under one window id: an id someone still holds (bitscomp, an agent, a step waiting for
//! Georg's yes) then names nothing instead of another object.

use std::collections::HashMap;

/// Beyond this many remembered objects a window forgets those the last read did not see (a long-running page that
/// made and dropped that many); what is on screen keeps its ids.
const MAX_REMEMBERED: usize = 50_000;

#[derive(Default)]
pub struct WinIds {
    /// The program's bus name: another program under the same window id forgets the old one's objects.
    pub app: String,
    /// The process the window was asked for: `act` must name the same one.
    pub pid: u32,
    /// The window's own object (its frame) as last read: where `act` checks which field has the keyboard.
    pub frame: String,
    /// That window's own name at the last read (a desktop title equal to it is no news).
    pub title: String,
    next: u32,
    /// Path -> (n, the read that last saw it).
    by_path: HashMap<String, (u32, u64)>,
    by_n: HashMap<u32, String>,
    parent: HashMap<String, String>,
    read: u64,
    limit: Option<usize>,
}

impl WinIds {
    /// The start of a read of `app`'s window for process `pid`.
    pub fn for_app(&mut self, app: &str, pid: u32) {
        if self.app != app {
            self.forget(|_| false);
            self.app = app.to_string();
        } else if self.by_path.len() > self.limit.unwrap_or(MAX_REMEMBERED) {
            let last = self.read;
            self.forget(|seen| seen == last);
        }
        self.pid = pid;
        self.read += 1;
    }

    /// Drops every object whose last read does not pass `keep`; `next` stays, so no number comes back.
    fn forget(&mut self, keep: impl Fn(u64) -> bool) {
        self.by_path.retain(|_, (_, seen)| keep(*seen));
        let by_path = &self.by_path;
        self.by_n.retain(|_, p| by_path.contains_key(p));
        self.parent.retain(|p, _| by_path.contains_key(p));
    }

    pub fn id(&mut self, path: &str) -> u32 {
        if let Some((n, seen)) = self.by_path.get_mut(path) {
            *seen = self.read;
            return *n;
        }
        self.next += 1;
        self.by_path.insert(path.to_string(), (self.next, self.read));
        self.by_n.insert(self.next, path.to_string());
        self.next
    }

    pub fn path(&self, n: u32) -> Option<&str> {
        self.by_n.get(&n).map(String::as_str)
    }

    pub fn parent_of(&mut self, path: &str, parent: &str) {
        if self.parent.get(path).map(String::as_str) != Some(parent) {
            self.parent.insert(path.to_string(), parent.to_string());
        }
    }

    pub fn parent(&self, path: &str) -> Option<&str> {
        self.parent.get(path).map(String::as_str)
    }
}

/// `<window>/<n>` split at its last slash (window ids may hold slashes).
pub fn split(id: &str) -> Option<(&str, u32)> {
    let (w, n) = id.rsplit_once('/')?;
    Some((w, n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_stay_while_the_object_exists_and_are_never_given_again() {
        let mut t = WinIds::default();
        t.for_app(":1.5", 7);
        let a = t.id("/a");
        let b = t.id("/b");
        assert_ne!(a, b);
        assert_eq!(t.id("/a"), a);
        assert_eq!(t.path(b), Some("/b"));
        t.for_app(":1.5", 7);
        assert_eq!(t.id("/a"), a, "same program: same id");
        t.for_app(":1.9", 8);
        assert_eq!((t.path(a), t.path(b)), (None, None), "another program: the old objects are forgotten");
        let c = t.id("/c");
        assert!(c > b, "a new object never gets an old number ({c} after {b})");
        assert_eq!((t.app.as_str(), t.pid), (":1.9", 8));
    }

    #[test]
    fn past_the_limit_only_what_the_last_read_saw_is_kept() {
        let mut t = WinIds { limit: Some(3), ..WinIds::default() };
        t.for_app(":1.5", 7);
        let ids: Vec<u32> = ["/a", "/b", "/c", "/d"].iter().map(|p| t.id(p)).collect();
        t.for_app(":1.5", 7);
        let a = t.id("/a");
        assert_eq!(a, ids[0]);
        t.for_app(":1.5", 7);
        assert_eq!(t.path(ids[0]), Some("/a"), "seen in the last read: kept");
        assert_eq!(t.path(ids[1]), None, "not seen since: forgotten");
        let e = t.id("/b");
        assert!(e > ids[3], "a forgotten object that comes back gets a new number, never an old one");
        assert_eq!(t.parent("/c"), None);
    }

    #[test]
    fn split_takes_the_last_slash() {
        assert_eq!(split("w/3/17"), Some(("w/3", 17)));
        assert_eq!(split("w"), None);
        assert_eq!(split("w/x"), None);
    }
}
