//! PK-6c: the closure of one program over the indexes (plan/PACKAGES.md 6.6), with pacman 7.1's
//! semantics (pacman/lib/libalpm/deps.c:788 _alpm_resolvedeps, :636 resolvedep, sync.c:367 _alpm_sync_prepare,
//! conflict.c:206 _alpm_innerconflicts, trans.c:152 _alpm_sortbydeps) and BitsOS's rules on top:
//! - D8: every program is its own closure, so only conflicts inside it count. pacman's target-vs-installed check
//!   (conflict.c:219 _alpm_outerconflicts) has nothing to compare with: no program shares a /usr with another.
//! - D3: only the configured sources exist (multilib, testing and the AUR are refused in the source order): a
//!   dependency only they could satisfy is refused with its name.
//! - D2: a package whose %ARCH% the CPU level does not run never counts as a satisfier.
//! - Providers: the table (data/providers.txt), then the previous version's provider (pacman returns an
//!   installed provider early, deps.c:719), then the first in repository order (pacman --noconfirm).
//! - System packages and parts that are no program are refused (policy.rs); %OPTDEPENDS% are listed, never pulled.
//!
//! `Closure::pacman` is the same walk without BitsOS's rules and table: it must equal `pacman -Sp`.

pub mod dep;
pub mod order;
pub mod plan;
pub mod policy;

use super::index::Index;
use super::refresh::event;
use super::sources::{self, Level};
use super::syncdb::{self, Pkg};
use dep::Dep;
pub use plan::{Choice, Optional, Plan, Planned, Rule};
use policy::{Prefs, Want};
use std::collections::{HashMap, HashSet};

pub struct Entry<'a> {
    pub repo: usize,
    pub pkg: &'a Pkg,
    pub depends: Vec<Dep>,
    pub provides: Vec<Dep>,
    pub conflicts: Vec<Dep>,
    pub replaces: Vec<Dep>,
    pub optdepends: Vec<Dep>,
    /// A field bitspkg cannot read: the package is refused when a closure needs it.
    pub broken: Option<String>,
}

/// The packages of the loaded sources a closure may use, in pacman's order: source order, then name
/// (be_sync.c:494 sorts each database's cache by name, as the index's BTreeMap is).
pub struct Universe<'a> {
    pub repos: Vec<String>,
    pub level: Level,
    pub entries: Vec<Entry<'a>>,
    by_name: HashMap<&'a str, Vec<usize>>,
    by_provide: HashMap<String, Vec<usize>>,
    other_arch: HashMap<&'a str, (usize, &'a str)>,
}

fn deps(v: &[String], what: &str, broken: &mut Option<String>) -> Vec<Dep> {
    v.iter()
        .filter_map(|s| match dep::parse(s) {
            Ok(d) => Some(d),
            Err(e) => {
                broken.get_or_insert(format!("{what}: {e}"));
                None
            }
        })
        .collect()
}

impl<'a> Universe<'a> {
    pub fn new(indexes: &'a [(String, Index)], level: Level) -> Universe<'a> {
        let arches = sources::arches(level);
        let mut u = Universe { repos: indexes.iter().map(|(r, _)| r.clone()).collect(), level, entries: Vec::new(), by_name: HashMap::new(), by_provide: HashMap::new(), other_arch: HashMap::new() };
        for (ri, (_, idx)) in indexes.iter().enumerate() {
            for p in idx.pkgs.values() {
                if !arches.contains(&p.arch.as_str()) {
                    u.other_arch.entry(p.name.as_str()).or_insert((ri, p.arch.as_str()));
                    continue;
                }
                let mut broken = None;
                let depends = deps(&p.depends, "%DEPENDS%", &mut broken);
                let mut provides = deps(&p.provides, "%PROVIDES%", &mut broken);
                // Sorted by name (stable: the order within one name stays) so a lookup is a binary search: a
                // hostile package with 10^5 provides must not make every dependency check linear.
                provides.sort_by(|a, b| a.name.cmp(&b.name));
                let conflicts = deps(&p.conflicts, "%CONFLICTS%", &mut broken);
                let replaces = deps(&p.replaces, "%REPLACES%", &mut broken);
                let mut opt_broken = None;
                let optdepends = deps(&p.optdepends, "%OPTDEPENDS%", &mut opt_broken);
                let i = u.entries.len();
                u.by_name.entry(p.name.as_str()).or_default().push(i);
                u.entries.push(Entry { repo: ri, pkg: p, depends, provides, conflicts, replaces, optdepends, broken });
            }
        }
        for (i, e) in u.entries.iter().enumerate() {
            let mut seen = HashSet::new();
            for pr in &e.provides {
                if seen.insert(pr.name.as_str()) {
                    u.by_provide.entry(pr.name.clone()).or_default().push(i);
                }
            }
        }
        u
    }

    /// The entry of `name` in source `repo`.
    pub fn entry(&self, repo: &str, name: &str) -> Option<usize> {
        self.by_name.get(name)?.iter().copied().find(|&e| self.repo(e) == repo)
    }
    pub fn name(&self, e: usize) -> &'a str {
        self.entries[e].pkg.name.as_str()
    }
    pub fn repo(&self, e: usize) -> &str {
        &self.repos[self.entries[e].repo]
    }
    /// deps.c:452 _alpm_depcmp.
    pub fn sat(&self, e: usize, d: &Dep) -> bool {
        let x = &self.entries[e];
        dep::literal(&x.pkg.name, &x.pkg.version, d) || self.prov(e, d)
    }
    /// deps.c:428 _alpm_depcmp_provides over the entry's provides of `d`'s name only.
    pub fn prov(&self, e: usize, d: &Dep) -> bool {
        let p = &self.entries[e].provides;
        let lo = p.partition_point(|x| x.name.as_str() < d.name.as_str());
        let n = p[lo..].partition_point(|x| x.name == d.name);
        dep::provides(d, &p[lo..lo + n])
    }

    /// Why nothing satisfies `d`, in German.
    fn missing(&self, d: &Dep) -> String {
        let n = d.name.as_str();
        if let Some(v) = self.by_name.get(n) {
            let have: Vec<String> = v.iter().map(|&e| format!("{} {}", self.repo(e), self.entries[e].pkg.version)).collect();
            return format!("{} braucht eine andere Version als die vorhandene ({})", d.text(), have.join(", "));
        }
        if let Some(v) = self.by_provide.get(n) {
            let have: Vec<String> = v.iter().map(|&e| self.name(e).to_string()).collect();
            return format!("{} wird nur in einer anderen Version angeboten (von {})", d.text(), have.join(", "));
        }
        if let Some((r, a)) = self.other_arch.get(n) {
            return format!("{n} gibt es nur für {a} (in {}); diese CPU-Stufe ({}) nimmt {}", self.repos[*r], self.level.name(), sources::arches(self.level).join(", "));
        }
        if n.starts_with("lib32-") {
            return format!("{n} ist ein 32-Bit-Teil aus multilib, und multilib ist aus");
        }
        format!("{n} steht in keiner eingeschalteten Quelle ({}); multilib, testing und das AUR sind aus", self.repos.join(" "))
    }

    /// The members of group `g` (%GROUPS%), in source order: `pacman -S GROUP` would take them all.
    fn group(&self, g: &str) -> Vec<&'a str> {
        let mut v: Vec<&'a str> = Vec::new();
        for e in &self.entries {
            if e.pkg.groups.iter().any(|x| x == g) && !v.contains(&e.pkg.name.as_str()) {
                v.push(e.pkg.name.as_str());
            }
        }
        v
    }
}

/// How many group members the refusal names; the count says how many there are.
const GROUP_SHOWN: usize = 8;

/// A program name as asked for (`REPO/NAME`): real ones are under 100 characters.
const NAME_MAX: usize = 256;

pub struct Closure;

impl Closure {
    /// One program's plan (plan 1.5). `what` is NAME or REPO/NAME; `previous` names the packages of the
    /// program's previous version (its provider choices are kept).
    pub fn resolve(what: &str, u: &Universe, prefs: &Prefs, previous: &[String]) -> Plan {
        let p = Run::new(u, prefs, previous, true).plan(what);
        event("pkg.resolve.done", &[("target", what), ("packages", &p.packages.len().to_string()), ("download", &p.download.to_string()), ("refused", &p.refused.len().to_string())]);
        p
    }
    /// pacman's closure alone: no table, no system or program rules (the comparison with `pacman -Sp`).
    pub fn pacman(what: &str, u: &Universe) -> Plan {
        Run::new(u, &Prefs::none(), &[], false).plan(what)
    }
}

struct Run<'u, 'a> {
    u: &'u Universe<'a>,
    prefs: &'u Prefs,
    previous: HashSet<&'u str>,
    policy: bool,
    set: Vec<usize>,
    names: HashMap<&'a str, usize>,
    provs: HashMap<&'u str, Vec<usize>>,
    why: HashMap<usize, String>,
    plan: Plan,
}

impl<'u, 'a> Run<'u, 'a> {
    fn new(u: &'u Universe<'a>, prefs: &'u Prefs, previous: &'u [String], policy: bool) -> Self {
        Run { u, prefs, previous: previous.iter().map(String::as_str).collect(), policy, set: Vec::new(), names: HashMap::new(), provs: HashMap::new(), why: HashMap::new(), plan: Plan::default() }
    }

    fn refuse(&mut self, name: &str, reason: String) {
        let name = dep::short(name);
        event("pkg.resolve.refused", &[("name", &name), ("reason", &reason)]);
        self.plan.refused.push((name, reason));
    }

    fn add(&mut self, e: usize) {
        self.set.push(e);
        self.names.insert(self.u.name(e), e);
        for p in &self.u.entries[e].provides {
            self.provs.entry(p.name.as_str()).or_default().push(e);
        }
    }

    /// deps.c:86 find_dep_satisfier over the closure so far.
    fn have(&self, d: &Dep) -> bool {
        self.names.get(d.name.as_str()).is_some_and(|&e| self.u.sat(e, d)) || self.provs.get(d.name.as_str()).is_some_and(|v| v.iter().any(|&e| self.u.prov(e, d)))
    }

    /// deps.c:636 resolvedep: a literal in any source first (source order), then the providers of all sources.
    fn resolvedep(&mut self, d: &Dep, needed_by: &str) -> Option<usize> {
        let u = self.u;
        if let Some(v) = u.by_name.get(d.name.as_str()) {
            if let Some(&e) = v.iter().find(|&&e| dep::literal(u.name(e), &u.entries[e].pkg.version, d) && !self.names.contains_key(u.name(e))) {
                return Some(e);
            }
        }
        let cands: Vec<usize> = u.by_provide.get(d.name.as_str()).map(|v| v.iter().copied().filter(|&e| u.name(e) != d.name && u.prov(e, d) && !self.names.contains_key(u.name(e))).collect()).unwrap_or_default();
        match cands.len() {
            0 => None,
            1 => Some(cands[0]),
            _ => Some(self.choose(d, &cands, needed_by)),
        }
    }

    fn choose(&mut self, d: &Dep, cands: &[usize], needed_by: &str) -> usize {
        let u = self.u;
        let by_table = self.prefs.rule(&d.name).and_then(|w| match w {
            Want::Name(n) => cands.iter().copied().find(|&e| u.name(e) == n),
            Want::Newest(g) => {
                let ver = |e: usize| u.entries[e].provides.iter().find(|p| p.name == d.name && p.m == dep::Mod::Eq).map(|p| p.version.clone()).unwrap_or_default();
                let mut best: Option<usize> = None;
                for &e in cands.iter().filter(|&&e| policy::glob(g, u.name(e))) {
                    if best.is_none_or(|b| super::vercmp::vercmp(&ver(e), &ver(b)) > 0) {
                        best = Some(e);
                    }
                }
                best
            }
        });
        let (pick, rule) = if let Some(e) = by_table {
            (e, Rule::Table)
        } else if let Some(&e) = cands.iter().find(|&&e| self.previous.contains(u.name(e))) {
            (e, Rule::Previous)
        } else {
            (cands[0], Rule::First)
        };
        let others = cands.iter().filter(|&&e| e != pick).map(|&e| format!("{}/{}", u.repo(e), u.name(e))).collect();
        let first = format!("{}/{}", u.repo(cands[0]), u.name(cands[0]));
        let c = Choice { dep: d.text(), needed_by: needed_by.to_string(), chosen: u.name(pick).to_string(), repo: u.repo(pick).to_string(), rule, others, first };
        event("pkg.resolve.choice", &[("dep", &c.dep), ("chosen", &c.chosen), ("rule", rule.name()), ("needed_by", needed_by)]);
        self.plan.choices.push(c);
        pick
    }

    fn plan(mut self, what: &str) -> Plan {
        self.plan.target = what.to_string();
        let rules = policy::system_rules();
        let (repo, name) = match what.split_once('/') {
            Some((r, n)) => (Some(r), n),
            None => (None, what),
        };
        if what.len() > NAME_MAX {
            self.refuse(what, format!("Name länger als {NAME_MAX} Zeichen"));
            return self.plan;
        }
        if let Err(e) = syncdb::check_name(name) {
            self.refuse(what, e);
            return self.plan;
        }
        if self.policy
            && let Some(r) = policy::system(&rules, name, true)
        {
            self.refuse(name, format!("Das ist ein Systemteil, kein Programm: {r}"));
            return self.plan;
        }
        let u = self.u;
        let top = match repo {
            Some(r) => match u.repos.iter().position(|x| x == r) {
                None => {
                    self.refuse(what, format!("die Quelle {r} ist nicht eingeschaltet ({})", u.repos.join(" ")));
                    return self.plan;
                }
                Some(ri) => u.by_name.get(name).and_then(|v| v.iter().copied().find(|&e| u.entries[e].repo == ri)),
            },
            None => self.resolvedep(&Dep::any(name), "Auswahl"),
        };
        let Some(top) = top else {
            let members = u.group(name);
            let m = if members.is_empty() {
                u.missing(&Dep::any(name))
            } else {
                let more = if members.len() > GROUP_SHOWN { format!(" und {} weitere", members.len() - GROUP_SHOWN) } else { String::new() };
                format!("{name} ist eine Paketgruppe, kein Programm ({} Pakete: {}{more}); bitte ein Programm daraus wählen", members.len(), members[..members.len().min(GROUP_SHOWN)].join(", "))
            };
            self.refuse(name, m);
            return self.plan;
        };
        let tname = u.name(top);
        // walk() checks the packages it pulls in; the program's own fields are checked here (pacman never drops
        // a dependency silently, and deps() could not read this one).
        if let Some(b) = &u.entries[top].broken {
            self.refuse(tname, format!("Das Paket hat ein unlesbares Feld: {b}"));
            return self.plan;
        }
        if self.policy {
            if let Some(r) = policy::system(&rules, tname, true) {
                self.refuse(tname, format!("Das ist ein Systemteil, kein Programm: {r}"));
                return self.plan;
            }
            if let Some(kind) = policy::not_program(u.entries[top].pkg) {
                self.refuse(tname, format!("Das ist kein Programm, nur ein Teil davon ({kind})"));
                return self.plan;
            }
            if policy::own_part(tname) {
                self.plan.notes.push(format!("{tname} ist ein Teil, den bitspkg selbst braucht (Schlüssel oder Spiegelliste)"));
            } else {
                self.plan.notes.push("Ob es ein Programm ist (Datei in usr/bin oder Eintrag in usr/share/applications), prüft bitspkg nach dem Laden an der Dateiliste".into());
            }
        }
        self.why.insert(top, "Programm".into());
        self.walk(top);
        if !self.plan.refused.is_empty() {
            return self.finish_refused();
        }
        self.check_parts(&rules);
        let removed = self.conflicts();
        if !self.plan.refused.is_empty() {
            return self.finish_refused();
        }
        if removed.contains(&top) {
            self.refuse(tname, "fällt durch einen Konflikt innerhalb des Programms weg".into());
            return self.finish_refused();
        }
        self.set.retain(|e| !removed.contains(e));
        if !removed.is_empty() {
            self.recheck();
        }
        if !self.plan.refused.is_empty() {
            return self.finish_refused();
        }
        self.finish(top)
    }

    /// deps.c:788 _alpm_resolvedeps, as a loop (no recursion: a hostile database's chain cannot overflow the
    /// stack). A package enters the closure before its dependencies, as in pacman's `resolved` list.
    fn walk(&mut self, top: usize) {
        let u = self.u;
        self.add(top);
        let mut stack: Vec<(usize, usize)> = vec![(top, 0)];
        while let Some(&mut (e, ref mut i)) = stack.last_mut() {
            let ds = &u.entries[e].depends;
            if *i >= ds.len() {
                stack.pop();
                continue;
            }
            let d = &ds[*i];
            *i += 1;
            // alpm_checkdeps with the package itself as the upgrade list, then the closure so far.
            if u.sat(e, d) || self.have(d) {
                continue;
            }
            let by = u.name(e);
            match self.resolvedep(d, by) {
                Some(s) => {
                    if let Some(b) = &u.entries[s].broken {
                        let r = format!("gebraucht von {by}, aber unlesbar: {b}");
                        self.refuse(u.name(s), r);
                    }
                    self.why.insert(s, format!("gebraucht von {by}"));
                    self.add(s);
                    stack.push((s, 0));
                }
                None => {
                    let r = format!("gebraucht von {by}: {}", u.missing(d));
                    self.refuse(&d.text(), r);
                }
            }
        }
    }

    /// System parts inside the closure (scope `all`); systemd stays as a library holder, said in a note.
    fn check_parts(&mut self, rules: &[policy::SystemRule]) {
        if !self.policy {
            return;
        }
        let u = self.u;
        for &e in self.set.clone().iter().skip(1) {
            let n = u.name(e);
            if let Some(r) = policy::system(rules, n, false) {
                let w = self.why.get(&e).cloned().unwrap_or_default();
                self.refuse(n, format!("{w}; {r}"));
            } else if n == "systemd" {
                self.plan.notes.push("systemd kommt als Bibliotheksträger in den Programmbaum mit; es läuft dort nie als Init".into());
            }
        }
    }

    /// sync.c:529-571: conflicts inside the closure (conflict.c:206). A package that provides the other's name takes
    /// its place; otherwise the program is refused, as pacman refuses. %REPLACES% removes nothing: pacman applies it
    /// only to installed packages (sync.c:124 check_replacers), and a closure starts empty; it is only said.
    fn conflicts(&mut self) -> HashSet<usize> {
        let u = self.u;
        let pos: HashMap<usize, usize> = self.set.iter().enumerate().map(|(i, &e)| (e, i)).collect();
        let mut found: Vec<(usize, usize, String, bool)> = Vec::new();
        let mut seen = HashSet::new();
        for &a in &self.set {
            for (c, replace) in u.entries[a].conflicts.iter().map(|c| (c, false)).chain(u.entries[a].replaces.iter().map(|r| (r, true))) {
                let mut hits: Vec<usize> = self.names.get(c.name.as_str()).copied().into_iter().chain(self.provs.get(c.name.as_str()).cloned().unwrap_or_default()).filter(|&b| b != a && u.name(b) != u.name(a)).filter(|&b| if replace { dep::literal(u.name(b), &u.entries[b].pkg.version, c) } else { u.sat(b, c) }).collect();
                hits.sort_by_key(|b| pos.get(b).copied().unwrap_or(usize::MAX));
                hits.dedup();
                for b in hits {
                    if seen.insert((a, b)) {
                        found.push((a, b, c.text(), replace));
                    }
                }
            }
        }
        let mut removed = HashSet::new();
        let mut replaced = Vec::new();
        for (a, b, why, replace) in found {
            if replace {
                replaced.push((a, b));
                continue;
            }
            if removed.contains(&a) || removed.contains(&b) {
                continue;
            }
            let (na, nb) = (u.name(a), u.name(b));
            if u.sat(a, &Dep::any(nb)) {
                removed.insert(b);
                self.plan.notes.push(format!("{nb} fällt weg: {na} bietet es selbst und verträgt sich nicht damit ({why})"));
                event("pkg.resolve.conflict", &[("kept", na), ("dropped", nb), ("by", &why)]);
            } else if u.sat(b, &Dep::any(na)) {
                removed.insert(a);
                self.plan.notes.push(format!("{na} fällt weg: {nb} bietet es selbst und verträgt sich nicht damit ({why})"));
                event("pkg.resolve.conflict", &[("kept", nb), ("dropped", na), ("by", &why)]);
            } else {
                self.refuse(na, format!("verträgt sich nicht mit {nb} ({why}); beide gehören zu diesem Programm"));
            }
        }
        for (a, b) in replaced {
            if !removed.contains(&a) && !removed.contains(&b) {
                self.plan.notes.push(format!("{} ersetzt {}, beide werden gebraucht und bleiben", u.name(a), u.name(b)));
            }
        }
        removed
    }

    /// sync.c:651 alpm_checkdeps after conflicts removed a package: every dependency still met inside.
    fn recheck(&mut self) {
        let u = self.u;
        let set = self.set.clone();
        let inside: HashSet<usize> = set.iter().copied().collect();
        for &e in &set {
            for d in &u.entries[e].depends {
                let n = d.name.as_str();
                let met = self.names.get(n).is_some_and(|s| inside.contains(s) && u.sat(*s, d)) || self.provs.get(n).is_some_and(|v| v.iter().any(|s| inside.contains(s) && u.prov(*s, d)));
                if !met {
                    let r = format!("nach einem Konflikt fehlt {} für {}", d.text(), u.name(e));
                    self.refuse(&d.text(), r);
                }
            }
        }
    }

    fn finish_refused(mut self) -> Plan {
        self.plan.packages.clear();
        self.plan
    }

    fn finish(mut self, top: usize) -> Plan {
        let u = self.u;
        let mut by_name: HashMap<&str, usize> = HashMap::new();
        let mut by_prov: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, &e) in self.set.iter().enumerate() {
            by_name.insert(u.name(e), i);
            for p in &u.entries[e].provides {
                by_prov.entry(p.name.as_str()).or_default().push(i);
            }
        }
        let children: Vec<Vec<usize>> = self
            .set
            .iter()
            .map(|&e| {
                let mut c: Vec<usize> = Vec::new();
                for d in &u.entries[e].depends {
                    let cand = by_name.get(d.name.as_str()).copied().into_iter().chain(by_prov.get(d.name.as_str()).cloned().unwrap_or_default());
                    c.extend(cand.filter(|&j| u.sat(self.set[j], d)));
                }
                c.sort_unstable();
                c.dedup();
                c
            })
            .collect();
        let (ord, _cycles) = order::sort(&children);
        for i in ord {
            let e = self.set[i];
            let p = u.entries[e].pkg;
            // A lying database may claim sizes near 2^64: the sum is refused, never wrapped or a panic.
            let (Some(down), Some(inst)) = (self.plan.download.checked_add(p.csize), self.plan.installed.checked_add(p.isize)) else {
                self.refuse(&p.name, format!("unplausible Größenangabe in der Quelle {} (%CSIZE% {}, %ISIZE% {}): zusammen mehr als 2^64 Bytes", u.repo(e), p.csize, p.isize));
                return self.finish_refused();
            };
            self.plan.download = down;
            self.plan.installed = inst;
            self.plan.packages.push(Planned { repo: u.repo(e).to_string(), name: p.name.clone(), version: p.version.clone(), csize: p.csize, isize: p.isize, why: self.why.get(&e).cloned().unwrap_or_default(), rust: p.rust() });
        }
        for d in &u.entries[top].optdepends {
            let present = self.have(d);
            self.plan.optional.push(Optional { dep: d.text(), reason: d.desc.clone().unwrap_or_default(), present });
        }
        let kept: HashSet<&str> = self.set.iter().map(|&e| u.name(e)).collect();
        for old in &self.previous {
            if !kept.contains(old) && let Some(&e) = self.set.iter().find(|&&e| u.entries[e].replaces.iter().any(|r| r.name == *old)) {
                self.plan.notes.push(format!("{} ersetzt {old} aus der vorigen Version", u.name(e)));
            }
        }
        self.plan
    }
}
