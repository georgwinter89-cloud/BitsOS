//! CRASHDISK: after a crash the desktop says so, once per crash ("BitsOS ist um 17:49 abgestürzt. Der Bericht
//! ist gesichert. …"). The kernel words the line (`/proc/bitsos/crash`, `notice=`, only while its record is new
//! at this boot); bitscomp shows it as a notice for 30 s and keeps the crash's key in its run directory, so a
//! restart of bitscomp does not say it again. The address `absturz` opens the record in a terminal.
//! The kernel reads the disk's records only once /boot is there (up to 180 s after its start, `decided=0` until
//! then), so bitscomp asks again every few seconds until it decided.

use crate::state::App;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROC: &str = "/proc/bitsos/crash";
pub const SHOW_FOR: Duration = Duration::from_secs(30);
const SHOWN: &str = "crash-shown";
pub const ASK_EVERY: Duration = Duration::from_secs(2);
/// The kernel's wait for /boot (180 s) and some room.
pub const ASK_FOR: Duration = Duration::from_secs(240);

#[derive(Debug, PartialEq, Eq)]
pub struct Note {
    /// The crash's boot id (`boot=`): the same crash keeps its key when its record's source changes.
    pub key: String,
    pub text: String,
}

/// The note in the kernel's file, when its record is new at this boot; and whether the kernel decided (a file
/// without `decided=` comes from a kernel that always has).
pub fn parse(t: &str) -> (Option<Note>, bool) {
    let mut lines = t.lines();
    let Some(head) = lines.next() else { return (None, true) };
    let decided = !head.split(' ').any(|w| w == "decided=0");
    if !head.split(' ').any(|w| w == "new=1") {
        return (None, decided);
    }
    let key = head.split(' ').find(|w| w.starts_with("boot=")).unwrap_or(head).to_string();
    let note = lines.next().and_then(|l| l.strip_prefix("notice=")).map(str::trim).filter(|t| !t.is_empty());
    (note.map(|text| Note { key, text: text.to_string() }), decided)
}

/// The asking: the run directory for the key, when to ask next, until when.
pub struct Watch {
    rundir: PathBuf,
    next: Duration,
    until: Duration,
}

/// One ask at `now`: Err(next ask) while the kernel has not decided and time is left; Ok(the note, if any).
pub fn step(file: Option<&str>, now: Duration, until: Duration) -> Result<Option<Note>, Duration> {
    let (note, decided) = file.map_or((None, true), parse);
    if !decided && now < until {
        return Err(now + ASK_EVERY);
    }
    Ok(note)
}

/// The terminal that shows the record (BusyBox `less`).
pub fn argv() -> Vec<String> {
    let mut v: Vec<String> = crate::terminal::TERMINAL.iter().map(|s| s.to_string()).collect();
    v.extend(["/bin/sh".to_string(), "-c".to_string(), format!("less {PROC} || (cat {PROC}; read x)")]);
    v
}

impl App {
    /// At the start of the desktop: the crash note, once per crash, as soon as the kernel decided.
    pub fn crash_note(&mut self, rundir: &Path) {
        let now = crate::pacer::now();
        self.crash_watch = Some(Watch { rundir: rundir.to_path_buf(), next: now, until: now + ASK_FOR });
        self.crash_tick(now);
    }

    pub fn crash_due(&self) -> Option<Duration> {
        self.crash_watch.as_ref().map(|w| w.next)
    }

    pub fn crash_tick(&mut self, mono: Duration) {
        let Some(w) = self.crash_watch.as_mut() else { return };
        if mono < w.next {
            return;
        }
        let file = std::fs::read_to_string(PROC).ok();
        let note = match step(file.as_deref(), mono, w.until) {
            Err(next) => {
                w.next = next;
                return;
            }
            Ok(n) => n,
        };
        let mark = w.rundir.join(SHOWN);
        self.crash_watch = None;
        let Some(n) = note else { return };
        if std::fs::read_to_string(&mark).is_ok_and(|k| k == n.key) {
            return;
        }
        let _ = std::fs::write(&mark, &n.key);
        eprintln!("bitscomp: crash note: {}", n.key);
        self.notice_for(n.text, SHOW_FOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_new_record_with_its_line() {
        let t = "new=1 source=disk slot=2 seq=7 wall=1 boot=ab generation=150 cut=0 hang=0 decided=1\nnotice=BitsOS ist um 17:49 abgestürzt. Der Bericht ist gesichert.\n\npanic on cpu 3\n";
        let (n, decided) = parse(t);
        assert!(decided);
        assert_eq!(n, Some(Note { key: "boot=ab".into(), text: "BitsOS ist um 17:49 abgestürzt. Der Bericht ist gesichert.".into() }));
        assert!(parse(&t.replace("new=1", "new=0")).0.is_none());
        assert_eq!(parse("new=0 source=none decided=1\n"), (None, true));
        assert_eq!(parse("new=0 source=none\n"), (None, true), "an older kernel");
        assert!(parse("new=1 source=ram decided=1\nnotice=\n").0.is_none());
        // The same crash keeps its key when the disk twin joins the RAM record.
        assert_eq!(parse(&t.replace("source=disk slot=2", "source=disk+ram slot=2")).0.unwrap().key, "boot=ab");
        for w in ["absturz", "Absturz", "crash", "absturzbericht"] {
            assert!(matches!(crate::address::resolve(w), crate::address::Open::Crash), "{w}");
        }
        let a = argv();
        assert!(a[0] == "foot" && a.last().unwrap().contains(PROC));
    }

    /// bitscomp may start before the kernel read the disk's records (it waits for /boot): it asks again until
    /// the kernel decided, then shows the note; it gives up after ASK_FOR.
    #[test]
    fn asks_again_until_the_kernel_decided() {
        let s = Duration::from_secs;
        let until = s(100) + ASK_FOR;
        let early = "new=0 source=none decided=0\n";
        let later = "new=1 source=disk slot=0 seq=1 wall=1 boot=cd generation=150 cut=0 hang=0 decided=1\nnotice=BitsOS ist um 17:49 abgestürzt. Der Bericht ist gesichert.\n";
        assert_eq!(step(Some(early), s(100), until), Err(s(100) + ASK_EVERY));
        assert_eq!(step(Some(later), s(130), until).unwrap().unwrap().key, "boot=cd");
        assert_eq!(step(Some(early), until, until), Ok(None), "gives up");
        assert_eq!(step(None, s(100), until), Ok(None), "no such file: nothing to wait for");
        let ram_early = "new=1 source=ram slot=-1 seq=0 wall=0 boot=cd generation=150 cut=0 hang=0 decided=0\nnotice=x\n";
        assert!(step(Some(ram_early), s(100), until).is_err(), "a RAM record waits for its disk twin too");
    }
}
