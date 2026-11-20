//! PK-7a: the "Programme" page's model (plan/spec/program-library.md section 4): what bitspkgd said last, kept for the page, which
//! draws only from it (B4). Pure: requests come out as `Job`s (pkg_client sends them), answers go in with `on`;
//! every request has a deadline, and one without an answer by then is "Programmverwaltung antwortet nicht".
//! The plan card's states follow the progress lines of `do` (download, verify, unpack, triggers, switch).
//! German texts of the card and the numbers: `programme_text`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mcpreg::Json;

use crate::pkg_client::{Job, Msg};
use crate::programme_text as text;

/// The page's own deadline for a read (bitspkgd answers reads within 30 s; the page says so sooner).
pub const READ_WAIT: Duration = Duration::from_secs(10);
/// A run: bitspkgd's own limit and a little; between two progress lines at most `DO_IDLE`.
pub const DO_WAIT: Duration = Duration::from_secs(3660);
pub const DO_IDLE: Duration = Duration::from_secs(300);
/// "Jetzt prüfen" (bitspkgd's `refresh`, its own limit 1800 s, and a little).
pub const REFRESH_WAIT: Duration = Duration::from_secs(1860);
/// Sources older than this many days get a line on the page (no network means nothing new is found).
pub const STALE_DAYS: i64 = 7;
pub const LATE: &str = "Programmverwaltung antwortet nicht";
/// The first-start offer (plan §5): installed from the mirrors on Georg's click.
pub const SUGGESTIONS: [(&str, &str, &str); 2] =
    [("brave-bin", "Brave", "Webbrowser mit eingebautem Werbeblocker"), ("libreoffice-fresh", "LibreOffice", "Texte, Tabellen, Präsentationen und Zeichnungen")];

/// A program name the page passes on (from an address or a list): what pacman allows (`[a-z0-9@._+-]`, not
/// starting with `.` or `-`). Anything else (`..`, `/etc`) never becomes a path or a request.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && !n.starts_with(['.', '-'])
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "@._+-".contains(c))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Install,
    Remove,
    Update,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Remove => "remove",
            Action::Update => "update",
        }
    }
}

/// What a request was for.
#[derive(Clone, Debug, PartialEq)]
pub enum What {
    Status,
    /// The installed list, asked at change generation `era` (an answer of an older generation is stale).
    List(u64),
    Sources,
    /// "Jetzt prüfen": bitspkgd loads the sources again (streams progress lines).
    Refresh,
    Search(u64),
    Plan(Action, String),
    Do(Action, String),
    Rollback(String),
    Space(String),
    Info(String),
    Facts(String),
    Clean(String),
    /// `plan update` of one installed program (the Aktualisierungen list), at change generation `era`.
    Check(String, u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub id: u64,
    pub what: What,
    pub due: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub desc: String,
    pub rust: bool,
    /// Bytes on disk once installed (`isize`), where bitspkgd names it.
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Installed {
    pub name: String,
    pub current: Option<String>,
    pub previous: Option<String>,
    pub versions: Vec<String>,
}

/// Per program: info, space and the folder's facts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detail {
    pub repo: Option<String>,
    pub signer: Option<String>,
    pub key: Option<String>,
    pub permissions: Vec<String>,
    /// (version, unix seconds of its folder).
    pub dates: Vec<(String, i64)>,
    pub own: Option<u64>,
    pub shared: Option<u64>,
    pub data: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The plan is asked for.
    Asking,
    /// The plan can run ("Jetzt installieren").
    Ready,
    /// bitspkgd refused the plan (space, a part, a signature) or the page cannot do what was ticked.
    Refused,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub action: Action,
    pub name: String,
    pub phase: Phase,
    pub plan_id: Option<String>,
    pub summary: String,
    pub version: String,
    /// `repo/name version` of what comes along.
    pub packages: Vec<String>,
    pub download: u64,
    pub installed: u64,
    /// (free, limit, used, fits) of D4.
    pub space: Option<(u64, u64, u64, bool)>,
    pub permissions: Vec<String>,
    pub refused: Vec<String>,
    /// "Zusätze": (dep, reason, ticked).
    pub extras: Vec<(String, String, bool)>,
    pub show_list: bool,
    pub keep_data: bool,
    /// Removal: what goes and what stays.
    pub data: u64,
    pub data_removed: bool,
    /// Progress: the phase reached (`progress::rank`), whether it is done, download bytes.
    pub rank: Option<usize>,
    pub step_done: bool,
    pub bytes: (u64, u64),
    pub report: Vec<String>,
    /// The result or error line (German, with the reason).
    pub line: String,
    /// The confirm dialog for a removal was opened (once per plan).
    pub asked: bool,
    /// "Alle aktualisieren": how many are left after this one.
    pub queue: Vec<String>,
}

impl Card {
    pub(crate) fn new(action: Action, name: &str) -> Card {
        Card {
            action,
            name: name.into(),
            phase: Phase::Asking,
            plan_id: None,
            summary: String::new(),
            version: String::new(),
            packages: Vec::new(),
            download: 0,
            installed: 0,
            space: None,
            permissions: Vec::new(),
            refused: Vec::new(),
            extras: Vec::new(),
            show_list: false,
            keep_data: false,
            data: 0,
            data_removed: false,
            rank: None,
            step_done: false,
            bytes: (0, 0),
            report: Vec::new(),
            line: String::new(),
            asked: false,
            queue: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Update {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub parts: usize,
    pub plan_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    next: u64,
    pub pending: Vec<Pending>,
    /// bitspkgd answered the last request (None: nothing asked yet); the reason when not.
    pub live: Option<bool>,
    pub down: String,
    pub installed: Vec<Installed>,
    pub listed: bool,
    pub query: String,
    seq: u64,
    pub hits: Vec<Hit>,
    pub more: usize,
    /// The search's state line ("Suche …", an error); empty when there are hits or nothing was searched.
    pub search_line: String,
    pub card: Option<Card>,
    pub last_check: Option<i64>,
    pub updates_found: Option<i64>,
    pub updates: Vec<Update>,
    pub details: BTreeMap<String, Detail>,
    /// The page's answer line (text, is an error).
    pub note: Option<(String, bool)>,
    /// Event lines for the log (B6), taken by the App.
    pub events: Vec<String>,
    /// Where program folders are (`/apps`; the scenario's fake folder).
    pub apps: PathBuf,
    /// The library's "Entfernen" opened a new page: it asks this removal once it is there.
    pub remove_next: Option<String>,
    /// Counts what changed the installed programs (a run, a rollback, "Jetzt prüfen"): list and check answers
    /// asked before the last change are stale and dropped; requests of the same generation are not asked twice.
    era: u64,
    /// What bitspkgd says it can beyond today's protocol (`status.features`: `keep_data`, `with`, `clean`);
    /// None until status answered. The page greys what is not there instead of a click that only refuses.
    pub features: Option<Vec<String>>,
    /// Sources not loaded or old (from `sources`): the line the page shows instead of "Nichts gefunden".
    pub sources_line: Option<String>,
    /// The search's `notes`: sources not searched because they are not loaded.
    pub search_note: String,
    /// Update checks that failed or came late: (program, German reason); the list is then incomplete.
    pub check_failed: Vec<(String, String)>,
    /// When "Jetzt prüfen" last loaded the sources (unix seconds).
    pub checked_at: Option<i64>,
}

fn s(j: &Json, k: &str) -> String {
    j.get(k).and_then(Json::as_str).unwrap_or("").to_string()
}

fn n(j: &Json, k: &str) -> u64 {
    j.get(k).and_then(Json::as_u64).unwrap_or(0)
}

fn strs(j: &Json, k: &str) -> Vec<String> {
    j.get(k).and_then(Json::as_array).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect()
}

fn arr<'a>(j: &'a Json, k: &str) -> &'a [Json] {
    j.get(k).and_then(Json::as_array).unwrap_or(&[])
}

fn line(op: &str, more: Vec<(&str, Json)>) -> String {
    let mut v = vec![("op", Json::str(op))];
    v.extend(more);
    Json::obj(v).to_string()
}

impl Model {
    pub fn new(apps: PathBuf) -> Model {
        Model { apps, ..Model::default() }
    }

    pub fn can(&self, feature: &str) -> bool {
        self.features.as_ref().is_some_and(|f| f.iter().any(|x| x == feature))
    }

    fn asking(&self, f: impl Fn(&What) -> bool) -> bool {
        self.pending.iter().any(|p| f(&p.what))
    }

    fn job(&mut self, what: What, line: String, now: Instant) -> Job {
        self.next += 1;
        let stream = matches!(what, What::Do(..) | What::Refresh);
        let (wait, idle) = match what {
            What::Do(..) => (DO_WAIT, DO_IDLE),
            What::Refresh => (REFRESH_WAIT, DO_IDLE),
            _ => (READ_WAIT, READ_WAIT),
        };
        let local = match &what {
            What::Facts(p) if valid_name(p) => Some(self.apps.join(p)),
            _ => None,
        };
        self.pending.push(Pending { id: self.next, what, due: now + wait });
        Job { id: self.next, line, stream, wait, idle, local }
    }

    /// Something changes or the page opens: status, the installed list (and so the update checks), the sources.
    /// What is still asked at the same generation is not asked again (tab changes do not pile up requests).
    pub fn refresh(&mut self, now: Instant) -> Vec<Job> {
        let era = self.era;
        let mut v = Vec::new();
        if !self.asking(|w| *w == What::Status) {
            v.push(self.job(What::Status, line("status", vec![]), now));
        }
        if !self.asking(|w| *w == What::List(era)) {
            v.push(self.job(What::List(era), line("list", vec![]), now));
        }
        if !self.asking(|w| *w == What::Sources) {
            v.push(self.job(What::Sources, line("sources", vec![]), now));
        }
        v
    }

    /// "Jetzt prüfen": bitspkgd loads the sources again; then the lists and the update checks are asked anew.
    pub fn check_now(&mut self, now: Instant) -> Option<Job> {
        if self.asking(|w| *w == What::Refresh) {
            return None;
        }
        self.say("Paketquellen werden geladen …", false);
        Some(self.job(What::Refresh, line("refresh", vec![]), now))
    }

    pub fn refreshing(&self) -> bool {
        self.asking(|w| *w == What::Refresh)
    }

    pub fn checking(&self) -> bool {
        self.asking(|w| matches!(w, What::Check(..) | What::List(_)))
    }

    /// The search field changed: a search for its text (the answer of an older one is dropped).
    pub fn search(&mut self, query: &str, now: Instant) -> Option<Job> {
        self.query = query.to_string();
        self.seq += 1;
        if query.trim().is_empty() {
            self.hits.clear();
            self.more = 0;
            self.search_line.clear();
            return None;
        }
        self.search_line = "Suche …".into();
        let l = line("search", vec![("query", Json::str(query.trim())), ("limit", Json::Num(50.0))]);
        Some(self.job(What::Search(self.seq), l, now))
    }

    /// "Installieren" (or "Entfernen" with the box `keep_data`): the plan card, its plan asked for. `with`: the
    /// ticked "Zusätze" (a new plan when a box changes).
    pub fn plan(&mut self, action: Action, name: &str, with: &[String], keep_data: bool, now: Instant) -> Job {
        let mut card = Card::new(action, name);
        card.keep_data = keep_data;
        if let Some(old) = self.card.as_ref().filter(|c| c.name == name && c.action == action) {
            card.show_list = old.show_list;
            card.extras = old.extras.iter().map(|(d, r, _)| (d.clone(), r.clone(), with.contains(d))).collect();
        }
        self.card = Some(card);
        let mut more = vec![("action", Json::str(action.word())), ("name", Json::str(name))];
        if !with.is_empty() {
            more.push(("with", Json::Arr(with.iter().map(|w| Json::str(w)).collect())));
        }
        if keep_data {
            more.push(("keep_data", Json::Bool(true)));
        }
        self.job(What::Plan(action, name.to_string()), line("plan", more), now)
    }

    /// "Jetzt installieren" (or the removal's confirmation): the plan runs.
    pub fn go(&mut self, now: Instant) -> Option<Job> {
        let c = self.card.as_mut().filter(|c| c.phase == Phase::Ready)?;
        let id = c.plan_id.clone()?;
        c.phase = Phase::Running;
        c.line = text::running(c.action, &c.name);
        let (action, name) = (c.action, c.name.clone());
        self.events.push(format!("programme: {} {name} started (plan {id})", action.word()));
        Some(self.job(What::Do(action, name), line("do", vec![("plan_id", Json::str(&id))]), now))
    }

    /// "Aktualisieren" of one program, or "Alle aktualisieren" (`names` in order; each runs after the one before).
    pub fn update(&mut self, names: &[String], now: Instant) -> Option<Job> {
        let (first, rest) = names.split_first()?;
        let u = self.updates.iter().find(|u| u.name == *first)?.clone();
        let mut c = Card::new(Action::Update, first);
        c.summary = u.summary.clone();
        c.version = u.version.clone();
        c.queue = rest.to_vec();
        match u.plan_id {
            Some(id) => {
                c.plan_id = Some(id);
                c.phase = Phase::Ready;
                self.card = Some(c);
                self.go(now)
            }
            None => {
                c.phase = Phase::Refused;
                c.line = format!("{first}: diese Aktualisierung kann nur root oder der Compositor ausführen");
                self.card = Some(c);
                None
            }
        }
    }

    pub fn rollback(&mut self, name: &str, now: Instant) -> Job {
        self.job(What::Rollback(name.into()), line("rollback", vec![("name", Json::str(name))]), now)
    }

    /// The per-program view: info, space, the folder's facts (nothing for a name that is no program name).
    pub fn detail(&mut self, name: &str, now: Instant) -> Vec<Job> {
        if !valid_name(name) {
            return Vec::new();
        }
        vec![
            self.job(What::Info(name.into()), line("info", vec![("name", Json::str(name))]), now),
            self.job(What::Space(name.into()), line("space", vec![("name", Json::str(name))]), now),
            self.job(What::Facts(name.into()), String::new(), now),
        ]
    }

    pub fn clean(&mut self, name: &str, now: Instant) -> Job {
        self.job(What::Clean(name.into()), line("clean", vec![("name", Json::str(name))]), now)
    }

    pub fn close_card(&mut self) {
        self.card = None;
    }

    pub fn busy(&self) -> bool {
        self.card.as_ref().is_some_and(|c| c.phase == Phase::Running)
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.installed.iter().any(|i| i.name == name && i.current.is_some())
    }

    fn take(&mut self, id: u64) -> Option<What> {
        let i = self.pending.iter().position(|p| p.id == id)?;
        Some(self.pending.remove(i).what)
    }

    fn say(&mut self, t: impl Into<String>, error: bool) {
        self.note = Some((t.into(), error));
    }

    /// A request past its deadline (the main loop's timer): late, as if its thread said so.
    pub fn expire(&mut self, id: u64, now: Instant) -> Vec<Job> {
        if self.pending.iter().any(|p| p.id == id && p.due <= now) { self.on(Msg::Late(id), now) } else { Vec::new() }
    }

    /// An answer; what it starts next (after a run: the lists again).
    pub fn on(&mut self, m: Msg, now: Instant) -> Vec<Job> {
        let id = match &m {
            Msg::Line(id, _) | Msg::End(id, _) | Msg::Failed(id, _) | Msg::Late(id) => *id,
        };
        if let Msg::Line(_, j) = &m {
            let what = self.pending.iter().find(|p| p.id == id).map(|p| p.what.clone());
            if let Some(What::Do(..) | What::Refresh) = what {
                if let (Some(What::Do(..)), Some(c)) = (&what, self.card.as_mut().filter(|c| c.phase == Phase::Running)) {
                    text::progress(c, j);
                }
                // The deadline is between lines: the next one is due within DO_IDLE.
                if let Some(p) = self.pending.iter_mut().find(|p| p.id == id) {
                    p.due = now + DO_IDLE;
                }
            }
            return Vec::new();
        }
        let Some(what) = self.take(id) else { return Vec::new() };
        let reply = match m {
            Msg::End(_, j) => {
                self.live = Some(true);
                j
            }
            // The folder's facts are read here, not by bitspkgd: a missing folder says nothing about the service.
            Msg::Failed(_, why) if matches!(what, What::Facts(_)) => {
                self.events.push(format!("programme: folder facts: {why}"));
                return Vec::new();
            }
            Msg::Failed(_, why) => {
                self.lost(&what, why.clone(), &why);
                return Vec::new();
            }
            Msg::Late(_) => {
                let secs = if matches!(what, What::Do(..)) { DO_IDLE.as_secs() } else { READ_WAIT.as_secs() };
                self.lost(&what, LATE.into(), &format!("{LATE} (nach {secs} s)"));
                return Vec::new();
            }
            Msg::Line(..) => return Vec::new(),
        };
        let ok = reply.get("ok").and_then(Json::as_bool) == Some(true);
        let why = text::reason(&s(&reply, "error"), &s(&reply, "message"));
        if !ok {
            // B6: bitspkgd's own words in the log; the page shows the German reason.
            self.events.push(format!("programme: bitspkgd refused {what:?}: {} {}", s(&reply, "error"), s(&reply, "message")));
        }
        match what {
            What::Status => {
                self.last_check = reply.get("last_check").and_then(Json::as_i64);
                self.updates_found = reply.get("updates_found").and_then(Json::as_i64);
                self.features = ok.then(|| strs(&reply, "features"));
            }
            What::List(g) | What::Check(_, g) if g != self.era => {}
            What::List(_) if ok => {
                self.listed = true;
                self.installed = arr(&reply, "programs")
                    .iter()
                    .map(|p| Installed {
                        name: s(p, "name"),
                        current: p.get("current").and_then(Json::as_str).map(String::from),
                        previous: p.get("previous").and_then(Json::as_str).map(String::from),
                        versions: strs(p, "versions"),
                    })
                    .collect();
                self.updates.retain(|u| self.installed.iter().any(|i| i.name == u.name));
                self.check_failed.retain(|(n, _)| self.installed.iter().any(|i| i.name == *n));
                let era = self.era;
                let names: Vec<String> = self
                    .installed
                    .iter()
                    .filter(|i| i.current.is_some() && valid_name(&i.name))
                    .map(|i| i.name.clone())
                    .filter(|n| !self.pending.iter().any(|p| p.what == What::Check(n.clone(), era)))
                    .collect();
                self.check_failed.retain(|(n, _)| !names.contains(n));
                return names.iter().map(|n| self.job(What::Check(n.clone(), era), line("plan", vec![("action", Json::str("update")), ("name", Json::str(n))]), now)).collect();
            }
            What::List(_) => self.say(format!("Liste der Programme: {why}"), true),
            What::Sources => self.sources_line = if ok { text::sources_line(arr(&reply, "sources")) } else { None },
            What::Refresh => {
                if ok {
                    self.era += 1;
                    self.checked_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64);
                    let bad = n(&reply, "refused");
                    let t = if bad > 0 { format!("Paketquellen geladen; {bad} abgelehnt (Signatur oder Spiegel): siehe „Quellen und Platz“.") } else { "Paketquellen geladen.".to_string() };
                    self.events.push(format!("programme: sources refreshed ({bad} refused)"));
                    self.say(t, bad > 0);
                    return self.refresh(now);
                }
                self.say(format!("Prüfung fehlgeschlagen: {why}"), true);
            }
            What::Search(seq) if seq != self.seq => {}
            What::Search(_) if ok => {
                self.hits = arr(&reply, "hits")
                    .iter()
                    .map(|h| Hit {
                        repo: s(h, "repo"),
                        name: s(h, "name"),
                        version: s(h, "version"),
                        desc: s(h, "desc"),
                        rust: h.get("rust").and_then(Json::as_bool) == Some(true),
                        size: h.get("isize").and_then(Json::as_u64),
                    })
                    .collect();
                self.more = n(&reply, "more") as usize;
                let notes = strs(&reply, "notes");
                self.search_note = if notes.is_empty() { String::new() } else { format!("Nicht durchsucht, weil noch nicht geladen: {}.", notes.join(", ")) };
                self.search_line = match (&self.sources_line, self.hits.is_empty()) {
                    (_, false) => String::new(),
                    (Some(l), true) => l.clone(),
                    (None, true) => format!("Nichts gefunden für „{}“.", self.query.trim()),
                };
            }
            What::Search(_) => {
                self.hits.clear();
                self.search_note.clear();
                self.search_line = if s(&reply, "message").contains("keine Quelle geladen") {
                    "Noch keine Paketquelle geladen: „Jetzt prüfen“ unter Aktualisierungen lädt sie (braucht Netz).".into()
                } else {
                    format!("Suche: {why}")
                };
            }
            What::Plan(action, name) => self.planned(action, &name, &reply, ok, &why),
            What::Do(action, name) => return self.done(action, &name, &reply, ok, &why, now),
            What::Check(name, _) => {
                self.updates.retain(|u| u.name != name);
                if !ok {
                    self.check_failed.push((name, why));
                    return Vec::new();
                }
                let changed = arr(&reply, "changed").len();
                if ok && changed > 0 && reply.get("installable").and_then(Json::as_bool) != Some(false) {
                    self.updates.push(Update {
                        name,
                        version: s(&reply, "version"),
                        summary: s(&reply, "summary"),
                        parts: changed,
                        plan_id: reply.get("plan_id").and_then(Json::as_str).map(String::from),
                    });
                    self.updates.sort_by(|a, b| a.name.cmp(&b.name));
                }
            }
            What::Rollback(name) => {
                if ok {
                    self.era += 1;
                    let report = strs(&reply, "report");
                    let t = format!("{name}: zurück auf {} (vorher {}).", s(&reply, "to"), s(&reply, "from"));
                    self.events.push(format!("programme: rollback {name} from {} to {}", s(&reply, "from"), s(&reply, "to")));
                    self.say(report.first().map_or(t.clone(), |r| format!("{t} {r}")), false);
                    let mut jobs = self.refresh(now);
                    jobs.extend(self.detail(&name, now));
                    return jobs;
                }
                self.events.push(format!("programme: rollback {name} refused: {why}"));
                self.say(format!("{name}: nicht zurückgestellt: {why}"), true);
            }
            What::Info(name) if ok => {
                let d = self.details.entry(name).or_default();
                if let Some(r) = reply.get("repo") {
                    d.repo = Some(s(r, "repo")).filter(|x| !x.is_empty()).or(d.repo.take());
                }
                if let Some(i) = reply.get("installed") {
                    let o = i.get("origin");
                    d.signer = o.map(|o| s(o, "signer")).filter(|x| !x.is_empty()).or(d.signer.take());
                    d.repo = o.map(|o| s(o, "repo")).filter(|x| !x.is_empty()).or(d.repo.take());
                    let p = strs(i, "permissions");
                    if !p.is_empty() {
                        d.permissions = p;
                    }
                }
            }
            What::Space(name) if ok => {
                if let Some(p) = arr(&reply, "programs").iter().find(|p| s(p, "name") == name) {
                    let d = self.details.entry(name).or_default();
                    d.own = p.get("own").and_then(Json::as_u64);
                    d.shared = p.get("shared").and_then(Json::as_u64);
                    d.data = p.get("data").and_then(Json::as_u64);
                }
            }
            What::Facts(name) if ok => {
                let d = self.details.entry(name).or_default();
                d.dates = arr(&reply, "versions").iter().map(|v| (s(v, "version"), v.get("time").and_then(Json::as_i64).unwrap_or(0))).collect();
                if let Some(o) = arr(&reply, "origins").first() {
                    d.repo = d.repo.take().or(Some(s(o, "repo")).filter(|x| !x.is_empty()));
                    d.key = Some(s(o, "key")).filter(|x| !x.is_empty());
                    d.signer = d.signer.take().or(Some(s(o, "packager")).filter(|x| !x.is_empty()));
                }
                let p = strs(&reply, "permissions");
                if d.permissions.is_empty() {
                    d.permissions = p;
                }
            }
            What::Info(_) | What::Space(_) | What::Facts(_) => self.events.push(format!("programme: details: {why}")),
            What::Clean(name) => {
                if ok {
                    self.say(format!("{name}: Cache geleert ({} frei).", text::bytes(n(&reply, "freed"))), false);
                    return vec![self.job(What::Space(name.clone()), line("space", vec![("name", Json::str(&name))]), now)];
                }
                let t = if s(&reply, "error") == "bad_request" { "das kann die Programmverwaltung noch nicht".to_string() } else { why };
                self.say(format!("{name}: Cache nicht geleert: {t}"), true);
            }
        }
        Vec::new()
    }

    /// No answer at all: every view says so; a run's card fails (what it did meanwhile is in bitspkgd's journal).
    fn lost(&mut self, what: &What, short: String, long: &str) {
        self.live = Some(false);
        self.down = short;
        self.events.push(format!("programme: bitspkgd: {long} ({what:?})"));
        match what {
            What::Search(seq) if *seq == self.seq => {
                self.hits.clear();
                self.search_line = long.to_string();
            }
            What::Plan(..) | What::Do(..) => {
                if let Some(c) = self.card.as_mut() {
                    c.phase = Phase::Failed;
                    c.line = if matches!(what, What::Do(..)) { format!("{long}: der Lauf geht dort vielleicht weiter; nach dem Neuladen steht hier, was geschah") } else { long.to_string() };
                }
            }
            What::Rollback(_) | What::Clean(_) => self.say(long.to_string(), true),
            What::Refresh => self.say(format!("Prüfung fehlgeschlagen: {long}"), true),
            What::Check(name, g) if *g == self.era => self.check_failed.push((name.clone(), long.to_string())),
            _ => {}
        }
    }

    fn planned(&mut self, action: Action, name: &str, j: &Json, ok: bool, why: &str) {
        let Some(c) = self.card.as_mut().filter(|c| c.name == name && c.action == action && c.phase == Phase::Asking) else { return };
        if !ok {
            c.phase = Phase::Refused;
            c.line = format!("Kein Plan: {why}");
            return;
        }
        c.summary = s(j, "summary");
        c.version = s(j, "version");
        c.packages = arr(j, "packages").iter().map(|p| format!("{}/{} {}", s(p, "repo"), s(p, "name"), s(p, "version"))).collect();
        c.download = n(j, "download");
        c.installed = n(j, "installed");
        c.space = j.get("space").filter(|sp| sp.get("free").is_some()).map(|sp| (n(sp, "free"), n(sp, "limit"), n(sp, "used"), sp.get("fits").and_then(Json::as_bool) != Some(false)));
        c.permissions = strs(j, "permissions");
        c.refused = arr(j, "refused").iter().map(|r| format!("{}: {}", s(r, "name"), s(r, "reason"))).collect();
        let ticked: Vec<String> = c.extras.iter().filter(|e| e.2).map(|e| e.0.clone()).collect();
        let echoed = strs(j, "with");
        c.extras = arr(j, "optional").iter().map(|o| (s(o, "dep"), s(o, "reason"), echoed.contains(&s(o, "dep")))).collect();
        c.data = n(j, "data");
        c.data_removed = j.get("data_removed").and_then(Json::as_bool) == Some(true);
        c.plan_id = j.get("plan_id").and_then(Json::as_str).map(String::from);
        let installable = j.get("installable").and_then(Json::as_bool) != Some(false);
        c.phase = Phase::Ready;
        if !ticked.is_empty() && ticked.iter().any(|t| !echoed.contains(t)) {
            c.phase = Phase::Refused;
            c.line = "Zusätze kann die Programmverwaltung noch nicht mitnehmen: die Haken sind wieder aus, so lässt es sich installieren.".into();
            c.plan_id = None;
        } else if action == Action::Remove && c.keep_data && j.get("keep_data").and_then(Json::as_bool) != Some(true) {
            c.phase = Phase::Refused;
            c.line = "Daten behalten kann die Programmverwaltung noch nicht: nichts entfernt (ohne den Haken gehen die Daten mit).".into();
            c.plan_id = None;
        } else if !installable {
            c.phase = Phase::Refused;
            c.line = text::refusal(c);
        } else if c.plan_id.is_none() {
            c.phase = Phase::Refused;
            c.line = "Nur lesen: ausführen dürfen nur root und der Compositor.".into();
        }
        let phase = c.phase;
        self.events.push(format!("programme: plan {} {name}: {phase:?} ({})", action.word(), j.get("summary").and_then(Json::as_str).unwrap_or("")));
    }

    fn done(&mut self, action: Action, name: &str, j: &Json, ok: bool, why: &str, now: Instant) -> Vec<Job> {
        let report = strs(j, "report");
        let mut next = None;
        if let Some(c) = self.card.as_mut().filter(|c| c.name == name && c.action == action) {
            c.report.extend(report);
            if ok {
                c.phase = Phase::Done;
                c.rank = Some(4);
                c.step_done = true;
                c.line = text::finished(action, name, j);
            } else {
                c.phase = Phase::Failed;
                c.line = text::failed(action, name, why);
            }
            if ok && !c.queue.is_empty() {
                next = Some(c.queue.clone());
            }
        }
        self.events.push(format!("programme: {} {name}: {}", action.word(), if ok { "done".to_string() } else { format!("refused: {why}") }));
        // A run (also a failed one) may have changed the programs: what was asked before is stale.
        self.era += 1;
        let mut jobs = self.refresh(now);
        if action != Action::Remove && (ok || self.is_installed(name)) {
            jobs.extend(self.detail(name, now));
        } else if ok {
            self.details.remove(name);
        }
        if let Some(q) = next {
            self.updates.retain(|u| u.name != name);
            jobs.extend(self.update(&q, now));
        }
        jobs
    }
}

#[cfg(test)]
#[path = "programme_tests.rs"]
mod tests;
