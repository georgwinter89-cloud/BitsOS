//! PK-2: what a package brings beyond its files and data folder lives in other services: settings keys
//! (settingsd), permission rules (permd, PM-3) and action registrations (MC-7). Install hands the new current
//! version to every hook, removal hands it the manifests of every version that goes. Hooks are idempotent: a
//! removal a cut interrupted runs them again from the manifests in the trash (`Store::recover`), and an entry
//! stays in the trash until every hook has succeeded, so nothing a package registered is forgotten.

use crate::cut::step;
use crate::manifest::Manifest;
use crate::settings::{DEFAULT_SOCKET, Settings};
use std::path::{Path, PathBuf};

pub trait Hook {
    fn what(&self) -> &'static str;
    /// Whether these manifests give this hook anything to do.
    fn needed(&self, ms: &[Manifest]) -> bool;
    /// Before a removal commits: refuse it when the hook could not finish (nothing has changed yet).
    fn ready(&self, _ms: &[Manifest]) -> Result<(), String> {
        Ok(())
    }
    fn install(&self, m: &Manifest) -> Result<(), String>;
    /// `cur` became current (install, upgrade, switch, rollback, `register`): what it brings is registered, what
    /// only `others` (versions still on disk) or `gone` (versions this command removed) brought is taken back.
    fn current(&self, cur: &Manifest, _others: &[Manifest], _gone: &[Manifest]) -> Result<(), String> {
        self.install(cur)
    }
    /// `keep` = `--keep-data`: what is Georg's or the program's data (values, rules) may stay.
    fn remove(&self, name: &str, ms: &[Manifest], keep: bool) -> Result<(), String>;
}

/// A service that does not exist yet (permd is PM-3, the action registry MC-7): what the package wants is logged
/// once per command as an event, nothing is registered, so a removal has nothing to take back.
pub struct Absent {
    pub what: &'static str,
    pub why: &'static str,
    pub has: fn(&Manifest) -> bool,
}

impl Hook for Absent {
    fn what(&self) -> &'static str {
        self.what
    }
    fn needed(&self, ms: &[Manifest]) -> bool {
        ms.iter().any(self.has)
    }
    fn install(&self, m: &Manifest) -> Result<(), String> {
        if (self.has)(m) {
            eprintln!("bitspkg: event=hook.absent what={} package={} reason=\"{}\"", self.what, m.name, self.why);
        }
        Ok(())
    }
    /// Nothing was registered, so nothing is taken back; said once so the desk sees what stays (B6).
    fn remove(&self, name: &str, ms: &[Manifest], _: bool) -> Result<(), String> {
        if self.needed(ms) {
            eprintln!("bitspkg: event=hook.absent what={} package={name} reason=\"{}: nothing to take back\"", self.what, self.why);
        }
        Ok(())
    }
}

pub struct Hooks {
    pub list: Vec<Box<dyn Hook>>,
}

impl Hooks {
    /// settingsd at `settings` (None: no settings registration, logged), permd and MC-7 not there yet.
    pub fn standard(settings: Option<PathBuf>) -> Hooks {
        let mut list: Vec<Box<dyn Hook>> = Vec::new();
        match settings {
            Some(socket) => list.push(Box::new(Settings { socket })),
            None => list.push(Box::new(Absent { what: "settings", why: "no settingsd for this root (--settings)", has: |m| !m.settings.is_empty() })),
        }
        list.push(Box::new(Absent { what: "rules", why: "permd does not exist yet (PM-3)", has: |m| !m.permissions.is_empty() }));
        list.push(Box::new(Absent { what: "actions", why: "the action registry does not exist yet (MC-7)", has: |m| !m.actions.is_empty() }));
        Hooks { list }
    }

    /// The default for a root: `BITSPKG_SETTINGS` (a socket, or `none`), else settingsd's socket for `/` only (a
    /// scratch root is a test or a data image being built on the Gaming-PC: its keys are registered on BitsOS).
    pub fn for_root(root: &Path) -> Hooks {
        let sock = match std::env::var("BITSPKG_SETTINGS") {
            Ok(s) if s == "none" => None,
            Ok(s) if !s.is_empty() => Some(PathBuf::from(s)),
            _ => (root == Path::new("/")).then(|| PathBuf::from(DEFAULT_SOCKET)),
        };
        Hooks::standard(sock)
    }

    /// After an install committed: a failure is an event, the install stands (`bitspkg register` repeats it).
    /// `others`: the package's other versions still on disk, `gone`: those this install removed.
    pub fn install(&self, m: &Manifest, others: &[Manifest], gone: &[Manifest]) -> usize {
        self.current(m, others, gone, true)
    }

    /// After a switch, a rollback or for `bitspkg register`. No cut steps: a cut after a rollback's swap cannot be
    /// finished by running the rollback again (it would swap back); `bitspkg register` repeats this part.
    pub fn sync(&self, m: &Manifest, others: &[Manifest]) -> usize {
        self.current(m, others, &[], false)
    }

    fn current(&self, m: &Manifest, others: &[Manifest], gone: &[Manifest], stepped: bool) -> usize {
        let mut failed = 0;
        for h in &self.list {
            if stepped {
                step("register");
            }
            if let Err(e) = h.current(m, others, gone) {
                eprintln!("bitspkg: event=hook.failed what={} package={} reason=\"{e}\" (run bitspkg register {})", h.what(), m.name, m.name);
                failed += 1;
            }
        }
        failed
    }

    pub fn ready(&self, ms: &[Manifest]) -> Result<(), String> {
        self.list.iter().try_for_each(|h| h.ready(ms))
    }

    /// Every hook, even after one failed; the first error is returned.
    pub fn remove(&self, name: &str, ms: &[Manifest], keep: bool) -> Result<(), String> {
        let mut first = None;
        for h in &self.list {
            step("unregister");
            if let Err(e) = h.remove(name, ms, keep) {
                eprintln!("bitspkg: event=hook.failed what={} package={name} reason=\"{e}\"", h.what());
                first.get_or_insert(format!("{}: {e}", h.what()));
            }
        }
        first.map_or(Ok(()), Err)
    }
}
