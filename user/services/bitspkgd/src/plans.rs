//! What `plan` found, kept until `do` (as pacman shows the transaction before it commits: src/pacman/sync.c:704
//! sync_trans, :902 pacman_sync). A plan lives `TTL` and is used once; `do` checks it again before it runs, so a
//! source refreshed meanwhile refuses it ("Plan veraltet") instead of installing something else.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Install,
    Remove,
    Update,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "install" => Action::Install,
            "remove" => Action::Remove,
            "update" => Action::Update,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Remove => "remove",
            Action::Update => "update",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub action: Action,
    /// What was asked for (`[REPO/]NAME`).
    pub name: String,
    /// The BitsOS program.
    pub program: String,
    /// What it stands on: `repo/name version` per package (install, update) or the versions (remove).
    pub parts: Vec<String>,
    /// Bytes to download (install, update).
    pub download: u64,
    /// A removal that leaves the data folder (`keep_data`, CR-PK-7a-1); false for every other plan.
    pub keep_data: bool,
    pub made: Instant,
}

/// Plans are kept only for peers that may run them (root, the compositor grant holder): a reader gets the plan
/// without an id, so no reader can fill the table (one entry per asked plan of a trusted peer, gone after `TTL`).
/// `shown` is what the installer's dialog showed Georg for Claude's `program_install`/`program_remove` (D6): the
/// confirmed call must find the same parts, or it is refused.
#[derive(Default)]
pub struct Plans {
    next: u64,
    map: HashMap<String, Planned>,
    shown: HashMap<(Action, String), (Vec<String>, Instant)>,
}

impl Plans {
    pub fn add(&mut self, p: Planned) -> String {
        self.sweep(p.made);
        self.next += 1;
        let id = format!("p{}-{}", std::process::id(), self.next);
        self.map.insert(id.clone(), p);
        id
    }
    /// The plan (taken: a plan runs once); None for an unknown or expired id.
    pub fn take(&mut self, id: &str, now: Instant) -> Option<Planned> {
        self.sweep(now);
        self.map.remove(id)
    }
    /// The plan, left in the table (`do` takes it only once its run holds the service: a busy refusal keeps it).
    pub fn peek(&mut self, id: &str, now: Instant) -> Option<Planned> {
        self.sweep(now);
        self.map.get(id).cloned()
    }
    /// The dialog showed `parts` for (`action`, `name`); a newer showing replaces an older one.
    pub fn show(&mut self, action: Action, name: &str, parts: Vec<String>, now: Instant) {
        self.sweep(now);
        self.shown.insert((action, name.to_string()), (parts, now));
    }
    /// What the dialog showed for (`action`, `name`), taken (one confirmation, one run).
    pub fn take_shown(&mut self, action: Action, name: &str, now: Instant) -> Option<Vec<String>> {
        self.sweep(now);
        self.shown.remove(&(action, name.to_string())).map(|s| s.0)
    }
    fn sweep(&mut self, now: Instant) {
        self.map.retain(|_, p| now.saturating_duration_since(p.made) < TTL);
        self.shown.retain(|_, s| now.saturating_duration_since(s.1) < TTL);
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_and_not_after_ttl() {
        let t = Instant::now();
        let p = Planned { action: Action::Install, name: "jq".into(), program: "jq".into(), parts: vec![], download: 0, keep_data: false, made: t };
        let mut ps = Plans::default();
        let a = ps.add(p.clone());
        let b = ps.add(p.clone());
        assert_ne!(a, b);
        assert_eq!(ps.take(&a, t).map(|p| p.name), Some("jq".into()));
        assert!(ps.take(&a, t).is_none(), "a plan runs once");
        assert!(ps.take(&b, t + TTL).is_none(), "expired");
        assert!(ps.is_empty());
        let c = ps.add(p.clone());
        assert!(ps.peek(&c, t).is_some() && ps.peek(&c, t).is_some(), "peek leaves the plan");
        assert!(ps.take(&c, t).is_some());
        ps.show(Action::Install, "jq", vec!["fixture/jq 1".into()], t);
        ps.show(Action::Install, "jq", vec!["fixture/jq 2".into()], t);
        assert_eq!(ps.take_shown(Action::Remove, "jq", t), None);
        assert_eq!(ps.take_shown(Action::Install, "jq", t), Some(vec!["fixture/jq 2".to_string()]), "the newest showing counts");
        assert_eq!(ps.take_shown(Action::Install, "jq", t), None, "one showing, one run");
        ps.show(Action::Install, "jq", vec![], t);
        assert_eq!(ps.take_shown(Action::Install, "jq", t + TTL), None, "an old showing is gone");
        assert_eq!(Action::parse("remove").map(Action::name), Some("remove"));
        assert_eq!(Action::parse("purge"), None);
    }
}
