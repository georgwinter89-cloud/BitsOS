//! The session (WY-5, Start.dc.html "Letzte Sitzung wiederherstellen"): the workspaces with their tiles and the
//! programs to restart, plus the recently used addresses. Saved 2 s after a change, as a small text file:
//! `<stick>/BITSOS2/home/.local/state/bitscomp/session.txt` (XDG_STATE_HOME in user 1000's home on the stick, which
//! fs-fat gives to 1000:1000) when settingsd stores on the stick and that home exists (persistent: the session
//! survives a reboot), else, and after a failed save there (a read-only stick), `$XDG_RUNTIME_DIR/bitscomp-session.txt`
//! (survives a restart of bitscomp). File work runs on a
//! thread of its own (B4: a hung stick never stalls the frame loop); its answers come back through a calloop
//! channel. Nothing is saved while the start page still offers the last session, so starting does not overwrite it;
//! the first window that opens (by any way) ends the offer, and from then on this session is saved.
//! Only bitscomp's own programs are stored with their arguments (the argv it started them with); any other client
//! is stored by its program name alone, so no argument (a password on a command line) reaches the file, which on
//! the stick (FAT32) has no permissions. Restore starts only programs the user could start anyway (in PATH or an
//! absolute path to an executable).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::state::App;

pub const SAVE_AFTER: Duration = Duration::from_secs(2);
/// Recently used entries older than this are dropped (a retention time, not a count).
const RECENT_KEEP_SECS: i64 = 30 * 24 * 3600;
const MAGIC: &str = "bitscomp-session 2";
/// The last line: a file without it was cut short (power loss on the stick) and counts as no session.
const END: &str = "end";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub name: String,
    /// Per tile its stack, each page the program's argv.
    pub tiles: Vec<Vec<Vec<String>>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    pub when: i64,
    pub active: usize,
    pub spaces: Vec<Space>,
}

impl Saved {
    pub fn counts(&self) -> (usize, usize) {
        let tiles = self.spaces.iter().map(|s| s.tiles.len()).sum();
        (tiles, self.spaces.iter().flat_map(|s| s.tiles.iter()).map(Vec::len).sum())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub when: i64,
    pub addr: String,
}

/// The "kind" column of the start page's recent list.
pub fn kind(addr: &str) -> &'static str {
    match addr.split('/').next() {
        Some("programm") => "PROGRAMM",
        Some("einstellungen") => "EINSTELLUNG",
        _ => "ADRESSE",
    }
}

pub fn encode(s: &Saved, recent: &[Recent]) -> String {
    let clean = |v: &str| v.replace(['\n', '\t', '\x1f'], " ");
    let mut out = format!("{MAGIC}\nsaved {}\nactive {}\n", s.when, s.active);
    for r in recent {
        out.push_str(&format!("recent {}\t{}\n", r.when, clean(&r.addr)));
    }
    for sp in &s.spaces {
        out.push_str(&format!("ws {}\n", clean(&sp.name)));
        for t in &sp.tiles {
            out.push_str("tile\n");
            for argv in t {
                let a: Vec<String> = argv.iter().map(|x| clean(x)).collect();
                out.push_str(&format!("prog {}\n", a.join("\x1f")));
            }
        }
    }
    out.push_str(END);
    out.push('\n');
    out
}

/// Reads a session file; malformed lines are skipped, another format or a file cut short gives `None`.
pub fn decode(text: &str) -> Option<(Saved, Vec<Recent>)> {
    let mut lines = text.lines();
    if lines.next()? != MAGIC || text.lines().next_back() != Some(END) {
        return None;
    }
    let mut s = Saved::default();
    let mut recent = Vec::new();
    for l in lines {
        let (k, v) = l.split_once(' ').unwrap_or((l, ""));
        match k {
            "saved" => s.when = v.parse().unwrap_or(0),
            "active" => s.active = v.parse().unwrap_or(0),
            "recent" => {
                if let Some((w, a)) = v.split_once('\t')
                    && let Ok(when) = w.parse()
                {
                    recent.push(Recent { when, addr: a.into() });
                }
            }
            "ws" => s.spaces.push(Space { name: v.into(), tiles: Vec::new() }),
            "tile" => {
                if let Some(sp) = s.spaces.last_mut() {
                    sp.tiles.push(Vec::new());
                }
            }
            "prog" => {
                // Empty arguments inside are kept (`foot -T ""`); an empty program name is not a program.
                let argv: Vec<String> = v.split('\x1f').map(String::from).collect();
                if let Some(t) = s.spaces.last_mut().and_then(|sp| sp.tiles.last_mut())
                    && !argv[0].is_empty()
                {
                    t.push(argv);
                }
            }
            _ => {}
        }
    }
    s.spaces.iter_mut().for_each(|sp| sp.tiles.retain(|t| !t.is_empty()));
    s.active = s.active.min(s.spaces.len().saturating_sub(1));
    Some((s, recent))
}

enum Job {
    Load(PathBuf),
    Save(PathBuf, String),
}

pub enum Done {
    Loaded(PathBuf, Option<String>),
    Failed(String),
}

#[derive(Default)]
pub struct Session {
    pub path: PathBuf,
    /// `$XDG_RUNTIME_DIR/bitscomp-session.txt`: used when the stick's home is missing or refuses the save.
    fallback: PathBuf,
    /// What the file held at start, offered on the start page until used or dismissed.
    pub saved: Option<Saved>,
    pub offered: bool,
    pub recent: Vec<Recent>,
    pub due: Option<Duration>,
    /// A window has opened or the offer was used: a session read later is not offered any more.
    taken: bool,
    last_written: String,
    jobs: Option<mpsc::Sender<Job>>,
}

impl Session {
    /// A window opened, or the offer was used or declined: the last session is no longer offered, this one is
    /// saved from now on (a session file read later, e.g. the stick's, does not bring the offer back).
    pub fn take_up(&mut self) {
        self.offered = false;
        self.saved = None;
        self.taken = true;
    }

    /// A session read from its file: offered on the start page unless a window has opened already. Only a session
    /// not yet taken up replaces the offer (the stick's, read later, is newer than the run dir's).
    pub fn loaded(&mut self, saved: Saved) {
        let has = saved.spaces.iter().any(|s| !s.tiles.is_empty());
        if !self.taken && (self.offered || self.saved.is_none()) {
            self.offered = has;
            self.saved = has.then_some(saved);
        }
    }

    /// Whether the session may be written (not while the last one is still offered).
    pub fn may_save(&self) -> bool {
        !self.offered
    }

    pub fn recent_addresses(&self) -> Vec<String> {
        self.recent.iter().map(|r| r.addr.clone()).collect()
    }
}

pub fn start(handle: &LoopHandle<'static, App>, app: &mut App, rundir: &Path) -> Result<(), String> {
    let (tx, rx) = channel::channel::<Done>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(d) = ev {
                app.session_done(d);
            }
        })
        .map_err(|e| e.to_string())?;
    let (jtx, jrx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("bitscomp-session".into())
        .spawn(move || store(&jrx, &tx))
        .map_err(|e| format!("session thread: {e}"))?;
    app.session.path = rundir.join("bitscomp-session.txt");
    app.session.fallback = app.session.path.clone();
    // Test mode (`--fixed-status`): no earlier session, so screenshots compare.
    if !app.status.fixed {
        let _ = jtx.send(Job::Load(app.session.path.clone()));
    }
    app.session.jobs = Some(jtx);
    Ok(())
}

fn store(jobs: &mpsc::Receiver<Job>, tx: &Sender<Done>) {
    while let Ok(first) = jobs.recv() {
        let mut batch = vec![first];
        batch.extend(jobs.try_iter());
        let last_save = batch.iter().rposition(|j| matches!(j, Job::Save(..)));
        for (i, job) in batch.into_iter().enumerate() {
            let done = match job {
                Job::Load(p) => {
                    let text = std::fs::read_to_string(&p).ok();
                    Done::Loaded(p, text)
                }
                // Only the newest save of a batch is written.
                Job::Save(..) if Some(i) != last_save => continue,
                Job::Save(p, text) => match write(&p, &text) {
                    Ok(()) => continue,
                    Err(e) => Done::Failed(format!("{}: {e}", p.display())),
                },
            };
            if tx.send(done).is_err() {
                return;
            }
        }
    }
}

/// Written beside, flushed to the medium, and renamed over, so a crash or an unplugged stick never leaves half a
/// file in place of the good one (a cut-short file has no END line and is ignored anyway). One writer thread, so
/// the fixed temp name cannot collide; a stale one from a crash is overwritten by the next save.
fn write(p: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, p) {
        // A file system without rename over an existing file (rename(2), linux-6.6/fs/namei.c): remove, then rename.
        if !p.exists() {
            return Err(e);
        }
        eprintln!("bitscomp: session rename over {} failed ({e}), replacing it", p.display());
        std::fs::remove_file(p)?;
        std::fs::rename(&tmp, p)?;
    }
    // The rename itself on the medium; a directory that cannot be synced (some file systems) is no error.
    if let Some(d) = p.parent()
        && let Ok(dir) = std::fs::File::open(d)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Whether `cmd` names a program the user could start anyway: an absolute path to an executable file, or a bare
/// name found executable in `path` (PATH). A relative path is not restored (whose directory would it be?).
pub fn startable(cmd: &str, path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let exe = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if cmd.starts_with('/') {
        return exe(Path::new(cmd));
    }
    !cmd.is_empty() && !cmd.contains('/') && path.split(':').filter(|d| !d.is_empty()).any(|d| exe(&Path::new(d).join(cmd)))
}

impl App {
    pub fn session_dirty(&mut self) {
        if self.session.due.is_none() {
            self.session.due = Some(crate::pacer::now() + SAVE_AFTER);
        }
    }

    /// settingsd stores on the stick: the session moves next to its directory (`dir` is <stick>/settings).
    pub fn session_place(&mut self, settings_dir: Option<String>) {
        let Some(d) = settings_dir.filter(|_| !self.status.fixed) else { return };
        let home = Path::new(&d).parent().unwrap_or(Path::new(&d)).join("home");
        if !std::fs::metadata(&home).is_ok_and(|m| m.is_dir()) {
            return;
        }
        let p = home.join(".local/state/bitscomp/session.txt");
        if p != self.session.path {
            self.session.path = p.clone();
            if let Some(j) = &self.session.jobs {
                let _ = j.send(Job::Load(p));
            }
        }
    }

    fn session_done(&mut self, d: Done) {
        match d {
            Done::Loaded(p, text) => {
                if p != self.session.path {
                    return;
                }
                if let Some((saved, recent)) = text.as_deref().and_then(decode) {
                    eprintln!("bitscomp: session {}: {} workspaces, {} recent", p.display(), saved.spaces.len(), recent.len());
                    self.session.loaded(saved);
                    for r in recent {
                        if !self.session.recent.iter().any(|x| x.addr == r.addr) {
                            self.session.recent.push(r);
                        }
                    }
                    self.chrome_dirty();
                }
            }
            Done::Failed(e) => {
                eprintln!("bitscomp: session not saved: {e}");
                // The stick's home refused it (read-only stick): from now on the runtime directory, saved again.
                if self.session.path != self.session.fallback && !self.session.fallback.as_os_str().is_empty() {
                    self.session.path = self.session.fallback.clone();
                    self.session.last_written.clear();
                    eprintln!("bitscomp: session now in {}", self.session.path.display());
                    self.session_dirty();
                }
            }
        }
    }

    /// What is open now, as a session.
    pub fn session_now(&self) -> Saved {
        let mut spaces = Vec::new();
        for i in 0..self.ws_count() {
            let ws = if i == self.desk.active { &self.ws } else { &self.desk.parked[i] };
            let argv = |w: &smithay::desktop::Window| self.restart_argv(self.pid_of(w));
            let mut tiles: Vec<Vec<Vec<String>>> =
                ws.tiles.iter().map(|t| t.stack.iter().filter_map(argv).collect()).collect();
            tiles.extend(ws.floating.iter().filter_map(|f| argv(&f.win)).map(|a| vec![a]));
            tiles.retain(|t: &Vec<Vec<String>>| !t.is_empty());
            spaces.push(Space { name: self.desk.names[i].clone(), tiles });
        }
        Saved { when: crate::status::epoch_secs(), active: self.desk.active, spaces }
    }

    /// What restarts the program of `pid`: the argv bitscomp started it with, else only its program name (another
    /// client's arguments are not ours to store).
    fn restart_argv(&self, pid: i32) -> Option<Vec<String>> {
        if let Some(a) = u32::try_from(pid).ok().and_then(|p| self.spawned.get(&p)) {
            return Some(a.clone());
        }
        crate::ending::cmdline(pid).and_then(|v| v.into_iter().next()).filter(|c| !c.is_empty()).map(|c| vec![c])
    }

    /// Saves when due (and something changed since the last write); returns the next deadline.
    pub fn session_tick(&mut self, now: Duration) -> Option<Duration> {
        let due = self.session.due?;
        if now < due {
            return Some(due);
        }
        self.session.due = None;
        if !self.session.may_save() || self.status.fixed {
            return None;
        }
        let cut = crate::status::epoch_secs() - RECENT_KEEP_SECS;
        self.session.recent.retain(|r| r.when >= cut);
        let mut now_s = self.session_now();
        let text_wo_time = encode(&Saved { when: 0, ..now_s.clone() }, &self.session.recent);
        if text_wo_time != self.session.last_written {
            self.session.last_written = text_wo_time;
            now_s.when = crate::status::epoch_secs();
            if let Some(j) = &self.session.jobs {
                let _ = j.send(Job::Save(self.session.path.clone(), encode(&now_s, &self.session.recent)));
            }
        }
        None
    }

    /// An address was used: to the front of the recent list.
    pub fn recent_add(&mut self, addr: &str) {
        self.session.recent.retain(|r| r.addr != addr);
        let when = if self.status.fixed { crate::status::FIXED_SECS } else { crate::status::epoch_secs() };
        self.session.recent.insert(0, Recent { when, addr: addr.into() });
        self.session_dirty();
        self.chrome_dirty();
    }

    /// "Letzte Sitzung wiederherstellen": the saved workspaces again, their programs restarted.
    pub fn session_restore(&mut self) {
        let Some(saved) = self.session.saved.take() else { return };
        self.session.take_up();
        let path = self.spawn_env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).or_else(|| std::env::var("PATH").ok());
        let path = path.unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
        let base = if self.ws_counts(self.desk.active) == (0, 0) && self.ws_count() == 1 { 0 } else { self.ws_count() };
        for (i, sp) in saved.spaces.iter().enumerate() {
            let at = base + i;
            while self.ws_count() <= at {
                self.desk.names.push(crate::desk::name(self.desk.next_no));
                self.desk.next_no += 1;
                self.desk.parked.push(Default::default());
            }
            self.desk.names[at] = sp.name.clone();
            for tile in &sp.tiles {
                let group = self.next_group;
                self.next_group += 1;
                for argv in tile {
                    if !startable(&argv[0], &path) {
                        eprintln!("bitscomp: session: {:?} is not a program in PATH, not restarted", argv[0]);
                        continue;
                    }
                    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                    if let Some(pid) = self.spawn(&refs) {
                        self.restoring.push(crate::place::Restore { pid, ws: at, group });
                    }
                }
            }
        }
        self.ws_switch(base + saved.active.min(saved.spaces.len().saturating_sub(1)));
        self.chrome_dirty();
        self.session_dirty();
    }

    /// "Neue Sitzung": the saved one is no longer offered (it is replaced as this one goes); a terminal to begin with.
    pub fn session_new(&mut self) {
        self.session.take_up();
        self.open_program(crate::terminal::TERMINAL.iter().map(|s| s.to_string()).collect(), crate::address::Where::Tile, None);
        self.chrome_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_survives_its_file() {
        let s = Saved {
            when: 1_792_593_120,
            active: 1,
            spaces: vec![
                Space { name: "Arbeitsraum 1".into(), tiles: vec![vec![vec!["foot".into(), "--font=Noto Sans Mono:size=11".into()]]] },
                Space { name: "Steuer 2026".into(), tiles: vec![vec![vec!["a".into()], vec!["b".into(), "x y".into()]], vec![vec!["c".into()]]] },
            ],
        };
        let recent = vec![Recent { when: 5, addr: "programm/terminal".into() }];
        let (back, r) = decode(&encode(&s, &recent)).unwrap();
        assert_eq!(back, s);
        assert_eq!(r, recent);
        assert_eq!(s.counts(), (3, 4));
        assert!(decode("something else").is_none());
        let (odd, _) = decode(&format!("{MAGIC}\nprog lost\nws A\ntile\nactive 9\nrecent bad\nprog \n{END}\n")).unwrap();
        assert_eq!(odd.spaces, [Space { name: "A".into(), tiles: vec![] }]);
        assert_eq!(odd.active, 0);
        assert_eq!(kind("programm/terminal"), "PROGRAMM");
        assert_eq!(kind("youtube.com"), "ADRESSE");
    }

    #[test]
    fn empty_arguments_survive_and_a_cut_file_is_no_session() {
        let s = Saved {
            when: 1,
            active: 0,
            spaces: vec![Space { name: "A".into(), tiles: vec![vec![vec!["foot".into(), "-T".into(), "".into(), "x".into()]]] }],
        };
        let text = encode(&s, &[]);
        assert_eq!(decode(&text).unwrap().0, s, "the empty title argument stays");
        for cut in [text.len() - 4, text.len() / 2, MAGIC.len() + 1] {
            assert!(decode(&text[..cut]).is_none(), "a file cut at {cut} is no session");
        }
    }

    fn one_tile() -> Saved {
        Saved { when: 1, active: 0, spaces: vec![Space { name: "A".into(), tiles: vec![vec![vec!["foot".into()]]] }] }
    }

    #[test]
    fn an_opened_window_ends_the_offer_and_the_session_is_saved_again() {
        let mut s = Session::default();
        s.loaded(one_tile());
        assert!(s.offered && !s.may_save(), "offered at start: not overwritten");
        // A window opens another way (a favourite, the address bar, a client of its own).
        s.take_up();
        assert!(!s.offered && s.saved.is_none() && s.may_save());
        // The stick's session read only now (settingsd answered late) does not come back.
        s.loaded(one_tile());
        assert!(!s.offered && s.saved.is_none() && s.may_save());
        // Before anything opened, the later (stick) one replaces the earlier offer.
        let mut s = Session::default();
        s.loaded(Saved { spaces: vec![], ..one_tile() });
        assert!(!s.offered);
        s.loaded(one_tile());
        assert!(s.offered);
    }

    #[test]
    fn only_startable_programs_are_restored() {
        assert!(startable("sh", "/nonexistent:/bin:/usr/bin"));
        assert!(startable("/bin/sh", ""));
        assert!(!startable("sh", "/nonexistent"));
        assert!(!startable("./sh", "/bin"));
        assert!(!startable("bin/sh", "/"));
        assert!(!startable("", "/bin"));
        assert!(!startable("/etc/hostname-not-a-program", "/bin"));
        assert!(!startable("/etc", "/bin"), "a directory is no program");
    }
}
