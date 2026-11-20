//! PE-1: program errors visible. Every program started from the desktop (`open_program`: a tile, the library,
//! the address bar, the "Programme" page, the sofa, the assistant; a restored session) writes its stdout and
//! stderr into one pipe of its own instead of bitscomp's stderr (the kernel log, mixed and nameless). bitscomp
//! keeps the last `prog_end::KEEP` bytes per start and, when the start ends as `prog_end::verdict` says, shows a
//! notice card with the reason and those lines, and writes the same into the journal with the program's name.
//!
//! The pipe is read for the program's whole life, never closed early: a reader gone means SIGPIPE/EPIPE for the
//! program's next write (linux-7.3.1/fs/pipe.c:536-539). Helpers in the program's group may keep the write end
//! after it ended, so the end is the wait status (`reap_children`), not EOF; the source goes at EOF. What they write
//! after the leader's end is kept as well and goes into the journal at EOF. A start that shows no window within
//! `launch::LIMIT` writes its lines so far into the journal once (a hang leaves a trace), and `ctl prog-starts`
//! shows every kept line while it runs. `ctl spawn` keeps bitscomp's stderr as before (tests that read their
//! program's lines from the log).

use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};

use crate::prog_end::{self, End, Facts, Ring, Verdict};
use crate::state::App;

/// One start whose end is still to come.
pub struct Start {
    id: u64,
    pub pid: u32,
    pub name: String,
    at: Duration,
    pub window: bool,
    terminal: bool,
    /// A shell or sandbox passes the program's signal on as 128 + it (`prog_end::relays`).
    relay: bool,
    /// When bitscomp asked it to end.
    asked: Option<Duration>,
    /// Its lines so far went into the journal because no window came within `launch::LIMIT`.
    waited: bool,
    pub ring: Ring,
    /// A second handle on the read end, to take what the program wrote right before its end.
    reader: File,
}

/// A start whose leader ended while its helpers still hold the pipe: what they write, journalled at EOF.
pub struct Left {
    id: u64,
    pub pid: u32,
    pub name: String,
    pub ring: Ring,
}

/// A start's pipe before the start: its watch and a second handle on the read end.
pub struct Pipe {
    id: u64,
    token: RegistrationToken,
    reader: File,
}

/// A notice card (newest last; the newest is shown).
pub struct Note {
    pub seq: u64,
    pub name: String,
    pub title: String,
    pub reason: String,
    pub lines: Vec<String>,
}

#[derive(Default)]
pub struct Progs {
    handle: Option<LoopHandle<'static, App>>,
    next: u64,
    pub starts: Vec<Start>,
    pub left: Vec<Left>,
    pub notes: Vec<Note>,
    pub hover: bool,
    pub image: Option<(crate::prog_note_draw::Look, MemoryRenderBuffer)>,
    buf: Vec<u8>,
}

/// A pipe for a start's stdout and stderr: (read end, non-blocking; write end, blocking as a program expects).
/// Both close on exec: the child gets the write end as its fd 1 and 2 only (dup2 clears the flag).
fn pipe() -> std::io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes two descriptors into `fds`, which lives for the call.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just made by pipe2 and are owned by nobody else.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: fcntl on a descriptor we own; only the file status flags change.
    unsafe {
        let fl = libc::fcntl(fds[0], libc::F_GETFL);
        if fl < 0 || libc::fcntl(fds[0], libc::F_SETFL, fl | libc::O_NONBLOCK) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((File::from(r), File::from(w)))
}

/// The kernel log, opened while bitscomp is still root (uid 1000 may not open /dev/kmsg): the program lines reach
/// MCP `log` and the journal on a machine without COM1. Without it (a start that was never root) they
/// go to bitscomp's stderr like its other lines.
static KMSG: std::sync::OnceLock<File> = std::sync::OnceLock::new();

/// Called before bitscomp gives up root.
pub fn open_journal() {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(f) = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_CLOEXEC).open("/dev/kmsg") {
        let _ = KMSG.set(f);
    }
}

/// One journal line in one write (Linux takes each write to /dev/kmsg as one record).
fn journal(line: &str) {
    use std::io::Write;
    let sent = KMSG.get().is_some_and(|f| (&*f).write_all(prog_end::kmsg_record(line).as_bytes()).is_ok());
    if !sent {
        crate::db_app::journal(line);
    }
}

/// The kept lines, one journal line each, behind `head: `.
fn journal_lines(head: &str, lines: &[String]) {
    for l in lines {
        journal(&prog_end::cut(&format!("{head}: {l}"), prog_end::JOURNAL_LINE));
    }
}

/// Whether the program file starts with "#!" (a script: its shell passes a signal on as 128 + it). A bare name is
/// looked up in PATH as execvp would.
fn is_script(prog: &str) -> bool {
    use std::io::Read;
    let head = |p: &std::path::Path| {
        let mut b = [0u8; 2];
        File::open(p).and_then(|mut f| f.read_exact(&mut b)).is_ok() && &b == b"#!"
    };
    if prog.contains('/') {
        return head(prog.as_ref());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|d| d.join(prog)).find(|p| p.is_file()).is_some_and(|p| head(&p))
}

/// Reads what is there (at most `max` reads, so a program that writes without pause does not hold the loop) into
/// `ring` when given. True at EOF or a read error: the source goes.
fn read_some(f: &File, buf: &mut Vec<u8>, mut ring: Option<&mut Ring>, max: usize) -> bool {
    use std::io::Read;
    buf.resize(64 * 1024, 0);
    for _ in 0..max {
        match (&*f).read(buf) {
            Ok(0) => return true,
            Ok(n) => {
                if let Some(r) = ring.as_deref_mut() {
                    r.push(&buf[..n]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return true,
        }
    }
    false
}

impl App {
    pub fn prog_start_loop(&mut self, handle: &LoopHandle<'static, App>) {
        self.runs.handle = Some(handle.clone());
    }

    /// The pipe for a desktop start, its read end already watched; None (bitscomp's stderr, as before) without a
    /// loop or when no pipe or watch can be had.
    pub fn prog_pipe(&mut self) -> Option<(Pipe, File)> {
        let handle = self.runs.handle.clone()?;
        let (read, write) = pipe().map_err(|e| eprintln!("bitscomp: program output: no pipe ({e}), the program writes to the journal")).ok()?;
        let reader = read.try_clone().ok()?;
        let id = self.runs.next;
        self.runs.next += 1;
        let token = handle
            .insert_source(Generic::new(read, Interest::READ, Mode::Level), move |_, f, app| {
                let mut buf = std::mem::take(&mut app.runs.buf);
                let r = &mut app.runs;
                let ring = r.starts.iter_mut().find(|s| s.id == id).map(|s| &mut s.ring).or_else(|| r.left.iter_mut().find(|l| l.id == id).map(|l| &mut l.ring));
                let done = read_some(f, &mut buf, ring, 16);
                app.runs.buf = buf;
                if done {
                    app.prog_pipe_closed(id);
                }
                Ok(if done { PostAction::Remove } else { PostAction::Continue })
            })
            .map_err(|e| eprintln!("bitscomp: program output: not watched ({}), the program writes to the journal", e.error))
            .ok()?;
        Some((Pipe { id, token, reader }, write))
    }

    /// The start for which `p` was made did not happen.
    pub fn prog_unused(&mut self, p: Pipe) {
        if let Some(h) = &self.runs.handle {
            h.remove(p.token);
        }
    }

    /// A desktop start went out with `p` as its output.
    pub fn prog_started(&mut self, p: Pipe, pid: u32, argv: &[&str], terminal: bool) {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        let name = self.library.entries.iter().find(|e| e.argv() == argv).map_or_else(|| prog_end::name_of(&argv), |e| e.name.clone());
        let relay = prog_end::relays(&argv, prog_end::program_of(&argv).is_some_and(is_script));
        let Pipe { id, reader, .. } = p;
        let at = crate::pacer::now();
        self.runs.starts.push(Start { id, pid, name, at, window: false, terminal, relay, asked: None, waited: false, ring: Ring::default(), reader });
    }

    /// The pipe of start `id` is at EOF: the helpers that outlived its leader are done too.
    fn prog_pipe_closed(&mut self, id: u64) {
        let Some(i) = self.runs.left.iter().position(|l| l.id == id) else { return };
        let l = self.runs.left.remove(i);
        let lines = l.ring.lines();
        if !lines.is_empty() {
            let head = format!("bitscomp: program: {} (pid {})", l.name, l.pid);
            journal(&format!("{head}: its helpers wrote {} lines after its end", lines.len()));
            journal_lines(&format!("{head} wrote after its end"), &lines);
        }
    }

    /// Once per start, `launch::LIMIT` after it and still without a window: its lines so far into the journal (a
    /// hanging start, Brave on the fallback desktop, leaves a trace; the end adds the rest).
    pub fn prog_tick(&mut self) {
        let now = crate::pacer::now();
        for s in self.runs.starts.iter_mut().filter(|s| !s.window && !s.waited && now.saturating_sub(s.at) >= crate::launch::LIMIT) {
            s.waited = true;
            let lines = s.ring.lines();
            let head = format!("bitscomp: program: {} (pid {})", s.name, s.pid);
            journal(&format!("{head} shows no window {} ms after its start and still runs; {} lines so far", now.saturating_sub(s.at).as_millis(), lines.len()));
            journal_lines(&format!("{head} wrote"), &lines);
        }
    }

    /// A window of `pid` drew its first frame: the start it belongs to (its pid, its group, or an ancestor: a
    /// launcher whose program is its child) has shown a window.
    pub fn prog_mapped(&mut self, pid: u32) {
        if self.runs.starts.iter().all(|s| s.window) {
            return;
        }
        let mut line = vec![pid, Self::group_of(pid)];
        let mut p = pid;
        while let Some(pp) = std::fs::read_to_string(format!("/proc/{p}/stat")).ok().as_deref().and_then(prog_end::parse_ppid) {
            if pp <= 1 || line.contains(&pp) {
                break;
            }
            line.push(pp);
            p = pp;
        }
        for s in self.runs.starts.iter_mut().filter(|s| !s.window && line.contains(&s.pid)) {
            s.window = true;
        }
    }

    /// bitscomp is about to end `pid` or the group `pid` (Beenden, Spiel beenden, window_kill): no notice for it.
    pub fn prog_asked(&mut self, pid: u32) {
        let g = Self::group_of(pid);
        let now = crate::pacer::now();
        for s in self.runs.starts.iter_mut().filter(|s| s.pid == pid || s.pid == g) {
            s.asked = Some(now);
        }
    }

    /// A child ended (`reap_children`, before the launch marks are updated).
    pub fn prog_ended(&mut self, pid: u32, st: std::process::ExitStatus) {
        let lib_name = self.lib.starts.iter().position(|(p, _)| *p == pid).map(|k| self.lib.starts.remove(k).1);
        let Some(i) = self.runs.starts.iter().position(|s| s.pid == pid) else { return };
        let mut s = self.runs.starts.remove(i);
        let mut buf = std::mem::take(&mut self.runs.buf);
        // Everything it wrote is in the pipe now; helpers that still write are cut off after a few reads.
        let eof = read_some(&s.reader, &mut buf, Some(&mut s.ring), 16);
        self.runs.buf = buf;
        let now = crate::pacer::now();
        let by_desktop = prog_end::by_desktop(s.asked, now);
        let name = lib_name.unwrap_or(s.name);
        if !eof {
            self.runs.left.push(Left { id: s.id, pid, name: name.clone(), ring: Ring::default() });
        }
        let ran = now.saturating_sub(s.at);
        let f = Facts { end: End::of(st).relayed(s.relay), window: s.window, ran, terminal: s.terminal, by_desktop };
        let v = prog_end::verdict(&f);
        if v == Verdict::Quiet {
            return;
        }
        let lines = s.ring.lines();
        let shown = matches!(v, Verdict::Notice(_));
        let head = format!("bitscomp: program: {name} (pid {pid})");
        journal(&format!(
            "{head} ended with {} after {} ms, {} its window{}; {} lines kept{}",
            prog_end::journal_end(f.end),
            ran.as_millis(),
            if f.window { "after" } else { "before" },
            if by_desktop { ", ended by the desktop" } else { "" },
            lines.len(),
            if shown { "; notice shown" } else { "" },
        ));
        journal_lines(&format!("{head} wrote"), &lines);
        if let Verdict::Notice(k) = v {
            let title = prog_end::title(&name, k);
            self.prog_note(name, title, prog_end::reason(f.end), prog_end::card_lines(&lines));
        }
    }

    fn prog_note(&mut self, name: String, title: String, reason: String, lines: Vec<String>) {
        if self.sofa.listens() {
            // GM-6b: the sofa screens show the short form; the card waits on the desktop.
            self.sofa.body.note = Some(format!("{title} {reason}"));
        } else if self.lib.open {
            self.lib.note = Some(title.clone());
        }
        self.runs.notes.retain(|n| n.name != name);
        let seq = self.runs.next;
        self.runs.next += 1;
        self.runs.notes.push(Note { seq, name, title, reason, lines });
        self.runs.hover = false;
        self.needs_frame = true;
    }

    /// The shown card goes; the next one (if any) comes.
    pub fn prog_note_close(&mut self) -> bool {
        let Some(n) = self.runs.notes.pop() else { return false };
        eprintln!("bitscomp: program: notice about {} closed", n.name);
        self.runs.hover = false;
        self.needs_frame = true;
        true
    }

    /// Whether the card is on the screen (not under a sofa screen, which is worked by the pad).
    pub fn prog_note_shown(&self) -> Option<&Note> {
        (self.sofa.show() == crate::sofa::Show::None).then(|| self.runs.notes.last()).flatten()
    }

    /// A press: on "Schließen" the card goes; anywhere on the card it is the card's (true: the press was ours).
    pub fn prog_note_press(&mut self) -> bool {
        let Some(b) = self.prog_note_boxes() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        if b.close.contains(x, y) {
            return self.prog_note_close();
        }
        b.card.contains(x, y)
    }

    pub fn prog_note_motion(&mut self) {
        let on = self.prog_note_boxes().is_some_and(|b| b.close.contains(self.pointer.x, self.pointer.y));
        if self.runs.hover != on {
            self.runs.hover = on;
            self.needs_frame = true;
        }
    }

    /// `ctl hot`'s line for the card.
    pub fn prog_note_hot(&self) -> String {
        let (Some(n), Some(b)) = (self.prog_note_shown(), self.prog_note_boxes()) else { return String::new() };
        let s = self.theme.scale;
        let pt = |r: crate::layout::Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        format!("prognote name={:?} card={} close={}\n", n.name, pt(b.card), pt(b.close))
    }
}

/// `ctl prog-note [close]`, `ctl prog-starts`, `ctl open-program ARGV...` (the desktop's start, as the address
/// bar starts a program).
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    match *words.first()? {
        "prog-note" if words.get(1) == Some(&"close") => Some(if app.prog_note_close() { Ok("closed".into()) } else { Err("no notice".into()) }),
        "prog-note" => {
            let sofa = if app.sofa.listens() { app.sofa.body.note.clone() } else { None };
            let mut out = format!("notes={} shown={} sofa_note={:?}\n", app.runs.notes.len(), app.prog_note_shown().is_some(), sofa.unwrap_or_default());
            if let Some(n) = app.runs.notes.last() {
                let shown = app.prog_note_boxes().map_or(0, |b| b.lines);
                out.push_str(&format!("note name={:?} title={:?} reason={:?} lines={} shown_lines={shown}\n", n.name, n.title, n.reason, n.lines.len()));
                for l in &n.lines {
                    out.push_str(&format!("line {l:?}\n"));
                }
            }
            out.push_str("end");
            Some(Ok(out))
        }
        "prog-starts" => {
            let mut out = String::new();
            for s in &app.runs.starts {
                out.push_str(&format!("start pid={} name={:?} window={} bytes={}\n", s.pid, s.name, s.window, s.ring.len()));
                for l in s.ring.lines() {
                    out.push_str(&format!("line pid={} {l:?}\n", s.pid));
                }
            }
            for s in &app.runs.left {
                out.push_str(&format!("after-end pid={} name={:?} bytes={}\n", s.pid, s.name, s.ring.len()));
                for l in s.ring.lines() {
                    out.push_str(&format!("line pid={} {l:?}\n", s.pid));
                }
            }
            out.push_str("end");
            Some(Ok(out))
        }
        "open-program" => {
            let argv: Vec<String> = words[1..].iter().map(|w| w.to_string()).collect();
            if argv.is_empty() {
                return Some(Err("usage: open-program PROGRAM [ARG]...".into()));
            }
            Some(app.open_program(argv, crate::address::Where::Tile, None).map(|pid| format!("pid {pid}")).ok_or_else(|| "start failed".into()))
        }
        _ => None,
    }
}
