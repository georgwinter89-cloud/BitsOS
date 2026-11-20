//! PE-1: what a program started from the desktop wrote last, and whether its end is worth a notice. Pure, so the
//! rule and the words are host-tested; `prog_note` does the pipes and the window, `prog_note_draw` the look.
//!
//! The rule (decided 17.11., plan/spec/bitscomp.md §1):
//! - a clean end (exit code 0) is never shown: a second Brave hands its address to the first and ends with 0, and
//!   so do launchers that start something in the background (before a window it goes into the journal, with the
//!   lines: what such a launcher said is the desk's only trace of it);
//! - an end bitscomp asked for itself (Beenden, Spiel beenden, `window_kill`) within ASKED_FOR is never shown;
//! - an error end before the program showed any window is shown, however long it ran (nothing else tells Georg);
//! - an error end within EARLY after the start is shown even after a window (a crash at the start: libraries,
//!   sandbox, graphics); later it is the program's own end after use and only goes into the journal;
//! - a terminal's error end after its window is the shell's last exit code, never shown.

use std::time::Duration;

/// The output kept per start: the end of what it wrote is what explains its end. 4 KiB holds a Chromium FATAL
/// line with its context or a Rust panic message, and a crash loop does not flood the journal.
pub const KEEP: usize = 4096;
/// "Soon after the start": a crash this early comes from the start itself, not from what Georg did with it.
pub const EARLY: Duration = Duration::from_secs(10);
/// Linux refuses a /dev/kmsg write over 1024 bytes as a whole (linux-7.3.1/kernel/printk/printk.c:749,
/// PRINTKRB_RECORD_MAX in kernel/printk/internal.h:51): a journal line is cut below that, with its prefix.
pub const JOURNAL_LINE: usize = 900;
/// An end within this after bitscomp asked for it (SIGTERM, then SIGKILL after `ending::KILL_AFTER`; a stick
/// launcher waits up to 10 s for Brave to save) is the asked-for end. A later one is the program's own: it ignored
/// the request and ended by itself.
pub const ASKED_FOR: Duration = Duration::from_secs(30);
/// A line our own programs (launchers, bitsrun) write for the person at the screen starts with this. When a start
/// wrote such lines, the card shows only those (the mark taken off): the others are the desk's, in English with
/// paths and codes, and stay in the journal.
pub const MARK: &str = "» ";

/// The last KEEP bytes a program wrote to stdout and stderr (one pipe, in the order written).
#[derive(Default, Debug)]
pub struct Ring {
    buf: Vec<u8>,
    /// Older output was dropped: the first line in `buf` may be the end of a longer one.
    cut: bool,
}

impl Ring {
    pub fn push(&mut self, b: &[u8]) {
        if b.len() >= KEEP {
            self.buf.clear();
            self.buf.extend_from_slice(&b[b.len() - KEEP..]);
            self.cut = true;
            return;
        }
        self.buf.extend_from_slice(b);
        if self.buf.len() > KEEP {
            let n = self.buf.len() - KEEP;
            self.buf.drain(..n);
            self.cut = true;
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// The whole lines kept, as text: any bytes (invalid UTF-8 is replaced), no control characters or colour
    /// codes, no empty lines. A line cut by the bound at the front is left out unless it is the only one.
    pub fn lines(&self) -> Vec<String> {
        let mut b = &self.buf[..];
        if self.cut
            && let Some(i) = b.iter().position(|&c| c == b'\n')
            && b[i + 1..].iter().any(|c| !c.is_ascii_whitespace())
        {
            b = &b[i + 1..];
        }
        b.split(|&c| c == b'\n').map(clean).filter(|l| !l.is_empty()).collect()
    }
}

/// One line as the screen and the journal show it: ANSI escape sequences out, tabs as spaces, other control
/// characters out, trailing blanks off.
fn clean(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => {
                // CSI (ESC [ ... final byte 0x40-0x7e) or a two-character escape.
                if it.peek() == Some(&'[') {
                    it.next();
                    for d in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&d) {
                            break;
                        }
                    }
                } else {
                    it.next();
                }
            }
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim_end().to_string()
}

/// How a program ended (the wait status: linux-7.3.1/kernel/exit.c:1297-1303, low 7 bits the signal, else the
/// code in the next byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Code(i32),
    Signal(i32),
    /// Exit code 128 + this signal from a shell or sandbox between bitscomp and the program (a stick launcher,
    /// bitsrun's bwrap): the program's own end was the signal.
    Passed(i32),
}

impl End {
    pub fn of(st: std::process::ExitStatus) -> End {
        use std::os::unix::process::ExitStatusExt;
        match (st.code(), st.signal()) {
            (Some(c), _) => End::Code(c),
            (None, Some(s)) => End::Signal(s),
            // Neither (a stopped child is never reaped as ended): count it as an error of unknown kind.
            _ => End::Code(-1),
        }
    }

    /// The end read through a relay (`relays`): 129..=192 is the program's signal. A program's own exit(130) is a
    /// code like any other.
    pub fn relayed(self, relay: bool) -> End {
        match self {
            End::Code(c) if relay && (129..=192).contains(&c) => End::Passed(c - 128),
            e => e,
        }
    }
}

/// Whether bitscomp asked for the end (`asked`: when) and it came within ASKED_FOR.
pub fn by_desktop(asked: Option<Duration>, now: Duration) -> bool {
    asked.is_some_and(|t| now.saturating_sub(t) < ASKED_FOR)
}

/// What is known about a start when it ends.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub end: End,
    /// A window of the program (its group or a descendant) drew its first frame.
    pub window: bool,
    pub ran: Duration,
    pub terminal: bool,
    /// bitscomp itself ended it (Beenden, Spiel beenden, window_kill).
    pub by_desktop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Ended before any window; `early`: within EARLY.
    NoWindow { early: bool },
    /// Ended within EARLY after its window came.
    SoonAfterWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A clean end: nothing.
    Quiet,
    /// One journal line with the reason and the last lines, no notice.
    Journal,
    Notice(Kind),
}

pub fn verdict(f: &Facts) -> Verdict {
    if f.end == End::Code(0) {
        return if f.window { Verdict::Quiet } else { Verdict::Journal };
    }
    if f.by_desktop || (f.terminal && f.window) {
        return Verdict::Journal;
    }
    let early = f.ran < EARLY;
    match (f.window, early) {
        (false, _) => Verdict::Notice(Kind::NoWindow { early }),
        (true, true) => Verdict::Notice(Kind::SoonAfterWindow),
        (true, false) => Verdict::Journal,
    }
}

/// The notice's first line.
pub fn title(name: &str, k: Kind) -> String {
    match k {
        Kind::NoWindow { early: true } => format!("„{name}“ hat sich gleich nach dem Start beendet."),
        Kind::NoWindow { early: false } => format!("„{name}“ hat sich beendet, ohne ein Fenster zu zeigen."),
        Kind::SoonAfterWindow => format!("„{name}“ hat sich kurz nach dem Start beendet."),
    }
}

/// The exit reason in Georg's words (no signal names; the journal has them).
pub fn reason(end: End) -> String {
    match end {
        End::Code(127) => "Die Programmdatei fehlt oder lässt sich nicht starten.".into(),
        End::Code(126) => "Die Programmdatei lässt sich nicht ausführen.".into(),
        End::Code(c) if c < 0 => "Das Programm endete aus einem unbekannten Grund.".into(),
        End::Code(c) => format!("Das Programm meldete einen Fehler (Fehlercode {c})."),
        End::Passed(s) => reason(End::Signal(s)),
        End::Signal(s) => match s {
            libc::SIGSEGV => "Das Programm ist abgestürzt: Es griff auf Speicher zu, der ihm nicht gehört.".into(),
            libc::SIGBUS => "Das Programm ist abgestürzt: Ein Speicherzugriff ging schief.".into(),
            libc::SIGILL => "Das Programm ist abgestürzt: Es enthielt einen Befehl, den der Prozessor nicht kennt.".into(),
            libc::SIGFPE => "Das Programm ist abgestürzt: Eine Rechnung ging nicht, zum Beispiel durch null geteilt.".into(),
            libc::SIGABRT => "Das Programm hat einen inneren Fehler bemerkt und sich selbst abgebrochen.".into(),
            libc::SIGTRAP => "Das Programm hat eine innere Prüfung nicht bestanden und angehalten.".into(),
            libc::SIGSYS => "Das Programm wollte etwas tun, das BitsOS nicht erlaubt oder noch nicht kann.".into(),
            libc::SIGKILL => "Das Programm wurde zwangsweise beendet.".into(),
            libc::SIGTERM => "Das Programm wurde von außen aufgefordert, sich zu beenden.".into(),
            libc::SIGINT => "Das Programm wurde abgebrochen.".into(),
            libc::SIGHUP => "Das Programm hat seine Verbindung verloren.".into(),
            libc::SIGPIPE => "Das Programm schrieb in eine Verbindung, die schon geschlossen war.".into(),
            libc::SIGXCPU => "Das Programm hat mehr Rechenzeit gebraucht, als es durfte.".into(),
            libc::SIGXFSZ => "Das Programm wollte eine größere Datei schreiben, als es durfte.".into(),
            s => format!("Das Programm wurde unerwartet beendet (Grund Nummer {s})."),
        },
    }
}

/// The heading over the program's lines, or the sentence when it wrote nothing.
pub const LINES_HEAD: &str = "Was das Programm zuletzt geschrieben hat:";
pub const NO_LINES: &str = "Das Programm hat vorher nichts geschrieben.";
pub const CLOSE: &str = "Schließen";

/// "Noch 2 weitere Meldungen" under the shown notice.
pub fn more(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("Noch 1 weitere Meldung".into()),
        n => Some(format!("Noch {n} weitere Meldungen")),
    }
}

/// The end in the journal's (the desk's) words.
pub fn journal_end(end: End) -> String {
    match end {
        End::Passed(s) => format!("exit code {} (128 + signal {s}, {})", 128 + s, signal_name(s)),
        End::Code(c) => format!("exit code {c}"),
        End::Signal(s) => format!("signal {s} ({})", signal_name(s)),
    }
}

fn signal_name(s: i32) -> &'static str {
    match s {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGSYS => "SIGSYS",
        _ => "other",
    }
}

/// `s` cut to at most `max` bytes on a character boundary (an ellipsis marks the cut).
pub fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// BitsOS's machine lines (`bitsrun: event=NAME k=v`, `lib.rs` of bitsrun): the journal's, never the card's.
fn is_event_line(l: &str) -> bool {
    let word = |w: &str, more: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || more.contains(c));
    l.split_once(": event=").is_some_and(|(tool, rest)| word(tool, "-_") && word(rest.split(' ').next().unwrap_or(""), "-_."))
}

/// The lines the card shows out of the kept ones: the marked lines (MARK off) when there are any, else every line
/// but the machine lines. The journal keeps them all.
pub fn card_lines(lines: &[String]) -> Vec<String> {
    let marked: Vec<String> = lines.iter().filter_map(|l| l.strip_prefix(MARK)).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if !marked.is_empty() {
        return marked;
    }
    lines.iter().filter(|l| !is_event_line(l)).cloned().collect()
}

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Where in `argv` the program is: past `busybox` and `env`'s settings.
fn program_at(argv: &[String]) -> Option<usize> {
    let mut i = 0;
    while let Some(w) = argv.get(i) {
        match base(w) {
            "busybox" => i += 1,
            "env" => {
                i += 1;
                while let Some(a) = argv.get(i).filter(|a| a.starts_with('-') || a.contains('=')) {
                    // `-u NAME` and `-C DIR` take the next word along.
                    i += if matches!(a.as_str(), "-u" | "--unset" | "-C" | "--chdir") { 2 } else { 1 };
                }
            }
            _ => return Some(i),
        }
    }
    None
}

/// The word of `argv` that is the program.
pub fn program_of(argv: &[String]) -> Option<&str> {
    program_at(argv).map(|i| argv[i].as_str())
}

/// Whether the program is a relay that passes a signal end on as exit code 128 + the signal: a shell (sh SCRIPT,
/// a launcher script), bitsrun or bwrap. `script`: the program file starts with "#!" (its shell does it).
pub fn relays(argv: &[String], script: bool) -> bool {
    script
        || program_of(argv).is_some_and(|p| matches!(base(p), "sh" | "bash" | "dash" | "ash" | "bitsrun" | "bwrap"))
}

/// The parent pid in a /proc/PID/stat line (the field after the state, behind the name's last ')').
pub fn parse_ppid(stat: &str) -> Option<u32> {
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// The name a start is shown by when no library entry names it: the program's file name, the package for
/// `bitsrun`, the script for `sh SCRIPT`, the program after `env`'s settings.
pub fn name_of(argv: &[String]) -> String {
    let Some(i) = program_at(argv) else { return "Programm".into() };
    let (w, rest) = (base(&argv[i]), &argv[i + 1..]);
    match w {
        // `sh SCRIPT` is the script; `sh -c TEXT` says nothing better than the shell.
        "sh" | "bash" | "dash" => rest.first().filter(|a| !a.starts_with('-')).map_or(w, |a| base(a)).to_string(),
        "bitsrun" => {
            let mut r = rest.iter().filter(|a| !a.starts_with('-'));
            // `bitsrun --exec PROG NAME`: the package's name.
            let first = r.next();
            r.next().or(first).map_or("bitsrun", |a| base(a)).to_string()
        }
        b => b.to_string(),
    }
}

/// A journal line as one /dev/kmsg record at priority 3 (err): Georg's machines keep `kernel.loglevel=error`, where
/// an Info line never reaches the log; a program that failed is what the journal is for (`<3>`, as Linux's
/// devkmsg_write reads it, linux-7.3.1/kernel/printk/printk.c:772; crates/vfs-sys/src/kmsg.rs maps it to Error).
pub fn kmsg_record(line: &str) -> String {
    format!("<3>{line}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_journal_line_is_one_err_record() {
        assert_eq!(kmsg_record("bitscomp: program: x (pid 1) ended"), "<3>bitscomp: program: x (pid 1) ended\n");
    }

    const S: Duration = Duration::from_secs(1);

    fn facts(end: End, window: bool, ran: Duration) -> Facts {
        Facts { end, window, ran, terminal: false, by_desktop: false }
    }

    #[test]
    fn the_rule_shows_error_ends_before_a_window_or_at_the_start() {
        use Verdict::*;
        assert_eq!(verdict(&facts(End::Code(1), false, S)), Notice(Kind::NoWindow { early: true }), "Brave's case");
        assert_eq!(verdict(&facts(End::Signal(libc::SIGSEGV), false, S * 3600)), Notice(Kind::NoWindow { early: false }));
        assert_eq!(verdict(&facts(End::Signal(libc::SIGABRT), true, S * 2)), Notice(Kind::SoonAfterWindow));
        assert_eq!(verdict(&facts(End::Code(1), true, EARLY)), Journal, "an error end after use");
        assert_eq!(verdict(&facts(End::Signal(libc::SIGSEGV), true, S * 60)), Journal);
        for ran in [S, S * 600] {
            assert_eq!(verdict(&facts(End::Code(0), true, ran)), Quiet, "a clean end after use: nothing ({ran:?})");
            assert_eq!(verdict(&facts(End::Code(0), false, ran)), Journal, "a clean end without a window: the journal only ({ran:?})");
        }
        let mut f = facts(End::Signal(libc::SIGTERM), true, S);
        f.by_desktop = true;
        assert_eq!(verdict(&f), Journal, "Beenden from the desktop");
        let mut t = facts(End::Code(2), true, S);
        t.terminal = true;
        assert_eq!(verdict(&t), Journal, "a shell's last exit code");
        t.window = false;
        assert_eq!(verdict(&t), Notice(Kind::NoWindow { early: true }), "a terminal that never opened");
    }

    #[test]
    fn the_ring_keeps_the_last_bytes_and_whole_lines() {
        let mut r = Ring::default();
        r.push(b"one\ntwo\n");
        r.push(b"three");
        assert_eq!(r.lines(), ["one", "two", "three"]);
        let mut r = Ring::default();
        for i in 0..2000 {
            r.push(format!("line {i}\n").as_bytes());
        }
        assert_eq!(r.len(), KEEP, "bounded by bytes");
        let l = r.lines();
        assert_eq!(l.last().map(String::as_str), Some("line 1999"));
        assert!(l.iter().all(|x| x.starts_with("line ")), "the cut first line is left out: {:?}", &l[..2]);
        let first: usize = l[0][5..].parse().unwrap();
        assert_eq!(l.len(), 2000 - first, "every line after the first kept one");
        // One huge write, one endless line without a newline.
        let mut r = Ring::default();
        r.push(&vec![b'x'; KEEP * 10]);
        assert_eq!(r.len(), KEEP);
        assert_eq!(r.lines(), [String::from_utf8(vec![b'x'; KEEP]).unwrap()], "the only line stays, cut");
    }

    #[test]
    fn any_bytes_become_readable_lines() {
        let mut r = Ring::default();
        r.push(b"\x1b[31mrot\x1b[0m und \xff\xfe kaputt\r\n\tTab\x07\n\n   \n");
        assert_eq!(r.lines(), ["rot und \u{fffd}\u{fffd} kaputt", " Tab"]);
        // A UTF-8 character split across two writes stays whole.
        let mut r = Ring::default();
        r.push("Größe ü".as_bytes().split_at(9).0);
        r.push("Größe ü".as_bytes().split_at(9).1);
        assert_eq!(r.lines(), ["Größe ü"]);
    }

    #[test]
    fn the_words_are_plain_german_and_have_glyphs() {
        assert_eq!(title("Brave", Kind::NoWindow { early: true }), "„Brave“ hat sich gleich nach dem Start beendet.");
        assert_eq!(reason(End::Code(1)), "Das Programm meldete einen Fehler (Fehlercode 1).");
        assert!(reason(End::Code(127)).contains("fehlt"));
        let mut all: Vec<String> = [Kind::NoWindow { early: true }, Kind::NoWindow { early: false }, Kind::SoonAfterWindow]
            .iter()
            .map(|k| title("Brave", *k))
            .collect();
        for s in 1..=64 {
            let r = reason(End::Signal(s));
            assert!(!r.contains("SIG") && !r.contains("Signal"), "{r}");
            all.push(r);
        }
        all.extend([-1, 1, 126, 127, 255].map(|c| reason(End::Code(c))));
        all.extend([LINES_HEAD, NO_LINES, CLOSE].map(String::from));
        all.extend([1, 5].map(|n| more(n).unwrap()));
        assert_eq!(more(0), None);
        for t in &all {
            assert!(t.chars().all(crate::text::Font::has), "{t}");
        }
        assert_eq!(journal_end(End::Signal(libc::SIGSEGV)), "signal 11 (SIGSEGV)");
        assert_eq!(journal_end(End::Code(3)), "exit code 3");
        // Through a shell or bwrap: 128 + the signal; a program's own exit(130) is its code.
        assert_eq!(reason(End::Code(139).relayed(true)), reason(End::Signal(libc::SIGSEGV)));
        assert_eq!(reason(End::Code(133).relayed(true)), "Das Programm hat eine innere Prüfung nicht bestanden und angehalten.");
        assert_eq!(journal_end(End::Code(134).relayed(true)), "exit code 134 (128 + signal 6, SIGABRT)");
        assert_eq!(End::Code(128).relayed(true), End::Code(128));
        assert_eq!(End::Code(193).relayed(true), End::Code(193));
        assert_eq!(reason(End::Code(130).relayed(false)), "Das Programm meldete einen Fehler (Fehlercode 130).");
        assert_eq!(journal_end(End::Code(130).relayed(false)), "exit code 130");
    }

    #[test]
    fn relays_are_shells_and_sandboxes() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        for a in ["/bin/sh /boot/BITSOS2/bin/brave", "/bin/busybox sh /tmp/x.sh", "/bin/bitsrun brave-bin", "env A=1 bash x", "bwrap --x y"] {
            assert!(relays(&v(a), false), "{a}");
        }
        for a in ["/usr/bin/python3 -c x", "/bin/busybox sleep 9", "env -u SH /opt/game", "vkquake"] {
            assert!(!relays(&v(a), false), "{a}");
            assert!(relays(&v(a), true), "a script's shell relays: {a}");
        }
        assert_eq!(program_of(&v("env -u A -C /tmp B=2 /opt/x/game -f")), Some("/opt/x/game"));
        assert_eq!(program_of(&v("busybox")), None);
    }

    #[test]
    fn an_asked_end_counts_only_soon_after_the_ask() {
        assert!(!by_desktop(None, S * 100));
        assert!(by_desktop(Some(S * 100), S * 102), "SIGKILL after 2 s");
        assert!(!by_desktop(Some(S * 100), S * 100 + ASKED_FOR), "the program ignored it and ended later by itself");
    }

    #[test]
    fn the_card_shows_lines_for_georg() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let raw = v(&["bitsrun: event=pkg.run.start name=brave-bin", "brave: libraries: 87 found, none missing", "[1:2:FATAL] No usable sandbox!"]);
        assert_eq!(card_lines(&raw), v(&["brave: libraries: 87 found, none missing", "[1:2:FATAL] No usable sandbox!"]), "machine lines off");
        let mut ours = raw.clone();
        ours.extend(v(&["» Brave startet nicht: Der Schutzraum fehlt.", "brave: Brave ended (code 133) after 2 s", "»   "]));
        assert_eq!(card_lines(&ours), v(&["Brave startet nicht: Der Schutzraum fehlt."]), "only the marked lines");
        assert_eq!(card_lines(&v(&["Fehler: event=x kaputt", "a: event=b"])), v(&["Fehler: event=x kaputt"]), "only a tool's event line is one");
        assert!(card_lines(&[]).is_empty());
    }

    #[test]
    fn names_cuts_and_parents() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(name_of(&v("/usr/bin/vkquake -game x")), "vkquake");
        assert_eq!(name_of(&v("/bin/sh /boot/BITSOS2/bin/brave --x")), "brave");
        assert_eq!(name_of(&v("/bin/busybox sh /tmp/pe1/fail.sh")), "fail.sh");
        assert_eq!(name_of(&["sh".into(), "-c".into(), "echo a/b; exit 3".into()]), "sh");
        assert_eq!(name_of(&v("bitsrun --exec /usr/bin/brave brave-bin")), "brave-bin");
        assert_eq!(name_of(&v("/bin/bitsrun steam")), "steam");
        assert_eq!(name_of(&v("env A=1 -u B /opt/x/game")), "game");
        assert_eq!(name_of(&[]), "Programm");
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcdef", 4), "a…");
        assert!(cut(&"ü".repeat(600), JOURNAL_LINE).len() <= JOURNAL_LINE);
        assert_eq!(parse_ppid("4242 (my (odd) prog) S 17 4242 4242 0"), Some(17));
        assert_eq!(parse_ppid("garbage"), None);
    }
}
