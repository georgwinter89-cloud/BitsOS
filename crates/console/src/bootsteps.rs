//! DS-4: the boot screen's step list, fed by the kernel log (the console's step sink gets every rendered line,
//! `[    1.234567] I net link.up ifname=igc0 ...`). Each step is a real part of the start with its real state and
//! the time of the event that set it; nothing waits for the screen. Pure (host-tested); the drawing is `bootdraw`,
//! the runtime `bootscreen`. `/proc/bitsos/boot` is this model as text (DS-5 continues the list from it).

use alloc::format;
use crate::stepdetail::Detail;
use alloc::string::String;
use alloc::vec::Vec;

/// The steps in the order of the start (and of the list on screen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Kernel,
    Memory,
    Cpus,
    Storage,
    Network,
    Sound,
    Gpu,
    Services,
    Mcp,
    Desktop,
}

pub const KEYS: [Key; 10] = [Key::Kernel, Key::Memory, Key::Cpus, Key::Storage, Key::Network, Key::Sound, Key::Gpu, Key::Services, Key::Mcp, Key::Desktop];

impl Key {
    /// The word on screen (the design's lower-case log list).
    pub fn label(self) -> &'static str {
        match self {
            Key::Kernel => "kern",
            Key::Memory => "speicher",
            Key::Cpus => "prozessoren",
            Key::Storage => "laufwerke",
            Key::Network => "netzwerk",
            Key::Sound => "ton",
            Key::Gpu => "grafik",
            Key::Services => "dienste",
            Key::Mcp => "mcp",
            Key::Desktop => "arbeitsfläche",
        }
    }
    /// The id in `/proc/bitsos/boot`.
    pub fn id(self) -> &'static str {
        match self {
            Key::Kernel => "kernel",
            Key::Memory => "memory",
            Key::Cpus => "cpus",
            Key::Storage => "storage",
            Key::Network => "network",
            Key::Sound => "sound",
            Key::Gpu => "gpu",
            Key::Services => "services",
            Key::Mcp => "mcp",
            Key::Desktop => "desktop",
        }
    }
    fn index(self) -> usize {
        KEYS.iter().position(|k| *k == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not reached yet.
    Waiting,
    /// läuft
    Running,
    /// fertig
    Done,
    /// Fehler (the detail says why)
    Failed,
    /// Not there or switched off (no sound card, `init=off`, `desktop=off`).
    Skipped,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Skipped => "skipped",
        }
    }
    fn parse(s: &str) -> Option<State> {
        Some(match s {
            "waiting" => State::Waiting,
            "running" => State::Running,
            "done" => State::Done,
            "failed" => State::Failed,
            "skipped" => State::Skipped,
            _ => return None,
        })
    }
    pub fn is_final(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Skipped)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub state: State,
    pub detail: Detail,
    /// Milliseconds since boot of the event that set the state.
    pub t_ms: u64,
}

/// What became of init (the desktop start waits for it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Init {
    #[default]
    Unknown,
    Running,
    Off,
    GivenUp,
}

/// `Copy` and heap-free (IN-9): a reader copies it out under the lock and works outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Steps {
    pub steps: [Step; KEYS.len()],
    /// The boot generation from `kernel start` (0 in QEMU and before the line came).
    pub generation: u64,
    pub kernel_ready: bool,
    pub init: Init,
    /// GX-8c: the chip's bring-up at boot runs (`gx boot.start` until its end).
    pub gpu_boot: bool,
    /// Changes so far (the screen draws when it moved; `/proc/bitsos/boot` shows it).
    pub seq: u64,
}

impl Default for Steps {
    fn default() -> Self {
        Self::new()
    }
}

/// One rendered klog line taken apart: time, level letter, subsystem, name, fields.
pub struct Line<'a> {
    pub t_ms: u64,
    pub level: char,
    pub subsystem: &'a str,
    pub name: &'a str,
    rest: &'a str,
}

impl<'a> Line<'a> {
    pub fn parse(line: &'a str) -> Option<Line<'a>> {
        let (l, subsystem, name) = crate::logfilter::head(line)?;
        let stamp = line.strip_prefix('[')?.split(']').next()?.trim();
        let (secs, micros) = stamp.split_once('.')?;
        let t_ms = secs.trim().parse::<u64>().ok()? * 1000 + micros.parse::<u64>().ok()? / 1000;
        let body = &line[line.find("] ")? + 2..];
        let rest = body.splitn(4, ' ').nth(3).unwrap_or("");
        let rest = rest.split(" | ").next().unwrap_or("");
        Some(Line { t_ms, level: l, subsystem, name, rest })
    }
    /// The value of field `key` (fields are `key=value` without spaces in the value's first word).
    pub fn field(&self, key: &str) -> Option<&'a str> {
        self.rest.split_whitespace().find_map(|w| w.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
    }
}

enum Update {
    Run(Detail),
    Done(Detail),
    Fail(Detail),
    Skip(Detail),
}

/// `format!` into a `Detail` (the step model never allocates, see `stepdetail`).
macro_rules! d {
    ($($t:tt)+) => {
        Detail::of(format_args!($($t)+))
    };
}

/// The MCP row when the server did not start: with why, if the line says (`reason=`, or `error=` as the kernel's own
/// `mcp.not_started` writes it), and without empty parentheses if it does not. klog quotes a value with spaces, so it is
/// read whole (`klog::fields::value_of`); a value with nothing the boot screen can draw counts as none (`Detail` drops
/// control characters, the atlas what it lacks).
fn not_started(l: &Line<'_>) -> Detail {
    let why = |key| klog::fields::value_of(l.rest, key).map(|v| v.trim_matches('"')).filter(|v| v.chars().any(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\\')));
    match why("reason").or_else(|| why("error")) {
        Some(why) => d!("nicht gestartet ({why})"),
        None => Detail::from("nicht gestartet"),
    }
}

fn mb(v: Option<&str>) -> Detail {
    let n: u64 = v.and_then(|s| s.parse().ok()).unwrap_or(0);
    if n >= 10 * 1024 {
        let tenths = n * 10 / 1024;
        d!("{},{} GB", tenths / 10, tenths % 10)
    } else {
        d!("{n} MB")
    }
}

impl Steps {
    pub const fn new() -> Steps {
        Steps {
            steps: [const { Step { state: State::Waiting, detail: Detail::new(), t_ms: 0 } }; KEYS.len()],
            generation: 0,
            kernel_ready: false,
            init: Init::Unknown,
            gpu_boot: false,
            seq: 0,
        }
    }

    pub fn get(&self, k: Key) -> &Step {
        &self.steps[k.index()]
    }

    /// Takes one log line; true when the list changed.
    pub fn feed(&mut self, line: &str) -> bool {
        let Some(l) = Line::parse(line) else { return false };
        let before = self.seq;
        if let Some((k, u)) = self.map(&l) {
            self.apply(k, u, l.t_ms);
        }
        self.seq != before
    }

    fn map(&mut self, l: &Line<'_>) -> Option<(Key, Update)> {
        let err = l.level == 'E';
        let f = |k: &str| l.field(k).unwrap_or("");
        Some(match (l.subsystem, l.name) {
            ("kernel", "start") => {
                self.generation = l.field("generation").and_then(|g| g.parse().ok()).unwrap_or(0);
                (Key::Kernel, Update::Done(d!("version {}", f("version"))))
            }
            ("kernel", "ready") => {
                self.kernel_ready = true;
                self.resolve_at_ready(l.t_ms);
                (Key::Services, Update::Run(Detail::from("init startet")))
            }
            ("mm", "frames") => (Key::Memory, Update::Done(d!("{} frei von {}", mb(l.field("free_mb")), mb(l.field("total_mb"))))),
            ("mm", n) if err => (Key::Memory, Update::Fail(Detail::from(n))),
            ("smp", "prepared") => (Key::Cpus, Update::Run(Detail::from("kerne starten"))),
            ("smp", "done") => {
                let failed: u64 = l.field("failed").and_then(|v| v.parse().ok()).unwrap_or(0);
                if failed > 0 {
                    (Key::Cpus, Update::Fail(d!("{} laufen, {failed} antworten nicht", f("cpus"))))
                } else {
                    (Key::Cpus, Update::Done(d!("{} kerne bereit", f("cpus"))))
                }
            }
            ("smp", "not_started") => (Key::Cpus, Update::Done(d!("1 kern ({})", f("reason")))),
            ("smp", n) | ("cpu", n) if err => (Key::Cpus, Update::Fail(Detail::from(n))),
            ("usb", "xhci.up") => (Key::Storage, Update::Run(d!("usb {} bereit", f("ctrl")))),
            ("nvme", "ctrl.ready") => (Key::Storage, Update::Run(d!("nvme {} bereit", f("ctrl")))),
            ("block", "disk.stick") => (Key::Storage, Update::Run(d!("stick {} gefunden", f("disk")))),
            ("fat", "stick.mounted") => (Key::Storage, Update::Done(d!("stick {} eingehängt", f("disk")))),
            ("usb", n) | ("nvme", n) | ("vfs", n) if err => (Key::Storage, Update::Fail(d!("{} {n}", l.subsystem))),
            ("net", "igc.up") | ("net", "virtio.up") => (Key::Network, Update::Run(d!("{} bereit", f("ifname")))),
            ("wlan", "netdev.registered") => (Key::Network, Update::Run(d!("wlan {} bereit", f("ifname")))),
            ("net", "link.up") | ("net", "iface.link_up") => (Key::Network, Update::Run(d!("{} verbunden", f("ifname")))),
            ("net", "dhcp.lease") | ("net", "addr.static") => (Key::Network, Update::Done(d!("{} {}", f("ifname"), f("ip")))),
            ("net", n) | ("wlan", n) if err => (Key::Network, Update::Fail(Detail::from(n))),
            ("snd", "hda.probe") => (Key::Sound, Update::Run(Detail::from("hda gefunden"))),
            ("snd", "hda.output") => (Key::Sound, Update::Done(Detail::from("hda ausgang bereit"))),
            ("snd", n) if err => (Key::Sound, Update::Fail(Detail::from(n))),
            ("dcn", "attached") => (Key::Gpu, Update::Run(Detail::from("anzeige gefunden"))),
            ("gx", "state.classified") => (Key::Gpu, Update::Run(Detail::from("gpu wird geprüft"))),
            // GX-8c: CONFIG gpu=on. The row is settled by the end of the whole run, not by GC's boot on the way.
            ("gx", "boot.start") => {
                self.gpu_boot = true;
                (Key::Gpu, Update::Run(Detail::from("gpu startet")))
            }
            ("gx", "boot.no_chip") => (Key::Gpu, Update::Skip(Detail::from("keine amd-gpu"))),
            ("gx", "boot.up") => {
                self.gpu_boot = false;
                (Key::Gpu, Update::Done(d!("gpu bereit, renderD{}", f("minor"))))
            }
            ("gx", "boot.failed") => {
                self.gpu_boot = false;
                if f("stop") == "not_needed" {
                    (Key::Gpu, Update::Fail(d!("nicht gestartet ({}), ohne gpu", f("stage"))))
                } else {
                    (Key::Gpu, Update::Fail(d!("gestoppt bei {}, ohne gpu", f("stage"))))
                }
            }
            ("gx", "gfx.booted") if self.gpu_boot => (Key::Gpu, Update::Run(Detail::from("gpu gestartet"))),
            ("gx", "gfx.booted") => (Key::Gpu, Update::Done(Detail::from("gpu gestartet"))),
            ("gx", n) | ("dcn", n) if err => (Key::Gpu, Update::Fail(Detail::from(n))),
            ("mcp", "server.start") => (Key::Mcp, Update::Done(d!("port {}", f("port")))),
            // CONFIG mcp=service: the kernel keeps the gate, the user-space service bitsmcp (started by init) serves.
            ("mcp", "server.not_started") => (Key::Mcp, Update::Done(Detail::from("über den dienst bitsmcp"))),
            ("mcp", "server.start_failed") | ("kernel", "mcp.not_started") => (Key::Mcp, Update::Fail(not_started(l))),
            ("ul", "init.start") => {
                self.init = Init::Running;
                (Key::Services, Update::Done(d!("init läuft (pid {})", f("pid"))))
            }
            ("ul", "init.off") => {
                self.init = Init::Off;
                (Key::Services, Update::Skip(Detail::from("init=off")))
            }
            ("ul", "init.died") => (Key::Services, Update::Fail(d!("init endete (code {})", f("code")))),
            ("ul", "init.given_up") => {
                self.init = Init::GivenUp;
                (Key::Services, Update::Fail(Detail::from("init aufgegeben")))
            }
            ("console", "desktop.off") => (Key::Desktop, Update::Skip(Detail::from("desktop=off"))),
            ("console", "desktop.wait") => (Key::Desktop, Update::Run(Detail::from("wartet auf init"))),
            ("console", "desktop.held") => (Key::Desktop, Update::Run(Detail::from("wartet auf die gpu"))),
            ("console", "desktop.start") => (Key::Desktop, Update::Run(d!("{} startet (versuch {})", f("program"), f("attempt")))),
            ("console", "desktop.failed") => (Key::Desktop, Update::Run(d!("versuch {}: {}", f("attempt"), why(f("reason"))))),
            ("console", "desktop.up") => (Key::Desktop, Update::Done(d!("{} läuft", f("program")))),
            ("console", "desktop.given_up") => (Key::Desktop, Update::Fail(d!("{} versuche: {}", f("attempts"), why(f("reason"))))),
            ("console", "desktop.ended") => (Key::Desktop, Update::Fail(d!("übernahm den bildschirm, dann {}", why(f("reason"))))),
            ("console", "desktop.slow") => (Key::Desktop, Update::Fail(Detail::from("läuft, zeigt aber nichts"))),
            ("console", "desktop.no_init") => (Key::Desktop, Update::Skip(Detail::from("ohne init"))),
            _ => return None,
        })
    }

    /// IN-9: the events `map` reads below the Error level, i.e. the ones a loglevel of warn or error drops before
    /// any sink. klog's watch hands exactly these to the step list whatever the level (Error lines always reach
    /// the step sink through the ring). Called for every dropped Info/Warn event: string compares only, no lock,
    /// no allocation. A new named arm in `map` belongs here too (host test `every_named_step_event_is_watched`).
    pub fn watched(subsystem: &str, name: &str) -> bool {
        match subsystem {
            "kernel" => matches!(name, "start" | "ready" | "mcp.not_started"),
            "mm" => name == "frames",
            "smp" => matches!(name, "prepared" | "done" | "not_started"),
            "usb" => name == "xhci.up",
            "nvme" => name == "ctrl.ready",
            "block" => name == "disk.stick",
            "fat" => name == "stick.mounted",
            "net" => matches!(name, "igc.up" | "virtio.up" | "link.up" | "iface.link_up" | "dhcp.lease" | "addr.static"),
            "wlan" => name == "netdev.registered",
            "snd" => matches!(name, "hda.probe" | "hda.output"),
            "dcn" => name == "attached",
            "gx" => matches!(name, "state.classified" | "gfx.booted" | "boot.start" | "boot.no_chip" | "boot.up" | "boot.failed"),
            "mcp" => matches!(name, "server.start" | "server.not_started" | "server.start_failed"),
            "ul" => matches!(name, "init.start" | "init.off" | "init.died" | "init.given_up"),
            "console" => matches!(
                name,
                "desktop.off" | "desktop.wait" | "desktop.held" | "desktop.start" | "desktop.failed" | "desktop.up" | "desktop.given_up"
                    | "desktop.ended" | "desktop.slow" | "desktop.no_init"
            ),
            // The ktest `in.boot.watch` checks the path without touching the live list (`map` ignores it).
            "ktest" => name == "in.boot.watch",
            _ => false,
        }
    }

    /// At `kernel ready` every driver had its probe: what never said anything is not there.
    fn resolve_at_ready(&mut self, t_ms: u64) {
        for (k, what) in [
            (Key::Cpus, "1 kern"),
            (Key::Storage, "keine gefunden"),
            (Key::Network, "keine karte"),
            (Key::Sound, "keine karte"),
            (Key::Gpu, "nur gop-bild"),
            (Key::Mcp, "nicht gestartet"),
        ] {
            if self.get(k).state == State::Waiting {
                self.apply(k, Update::Skip(Detail::from(what)), t_ms);
            }
        }
    }

    fn apply(&mut self, k: Key, u: Update, t_ms: u64) {
        let s = &mut self.steps[k.index()];
        let (state, detail) = match u {
            Update::Run(d) => (State::Running, d),
            Update::Done(d) => (State::Done, d),
            Update::Fail(d) => (State::Failed, d),
            Update::Skip(d) => (State::Skipped, d),
        };
        let allowed = match state {
            // A later success wins over an earlier failure (WLAN failed, the cable got an address; init restarted).
            State::Done => true,
            // A failure never hides a success of the same step.
            State::Failed => s.state != State::Done,
            _ => matches!(s.state, State::Waiting | State::Running),
        };
        // `Detail` is already clean: a detail with control characters must not count as a change every time.
        if allowed && (s.state != state || s.detail != detail) {
            s.state = state;
            s.detail = detail;
            s.t_ms = t_ms;
            self.seq += 1;
        }
    }

    /// Steps in a final state, of all.
    pub fn progress(&self) -> (usize, usize) {
        (self.steps.iter().filter(|s| s.state.is_final()).count(), KEYS.len())
    }

    /// The line under the title: what runs now, or how the start ended.
    pub fn status(&self) -> String {
        if let Some(k) = KEYS.iter().find(|k| self.get(**k).state == State::Running) {
            return format!("Startet: {}", k.label());
        }
        let failed: Vec<&str> = KEYS.iter().filter(|k| self.get(**k).state == State::Failed).map(|k| k.label()).collect();
        let last = self.steps.iter().map(|s| s.t_ms).max().unwrap_or(0);
        let (done, all) = self.progress();
        if done < all {
            return String::from("Startet …");
        }
        if failed.is_empty() { format!("Bereit nach {}.", seconds(last)) } else { format!("Bereit nach {}, Fehler bei {}.", seconds(last), failed.join(", ")) }
    }

    /// `/proc/bitsos/boot` (DS-5's interface, see plan/notes/in-DS-4.md): `screen` is the boot screen's state,
    /// `drawn` the `seq` it last drew.
    pub fn proc_text(&self, screen: &str, drawn: u64) -> String {
        let mut s = format!("bitsos-boot 1\nseq {}\ndrawn {drawn}\nscreen {screen}\ngeneration {}\n", self.seq, self.generation);
        for k in KEYS {
            let st = self.get(k);
            s.push_str(&format!("step {} {} {} {}\n", k.id(), st.state.name(), st.t_ms, st.detail));
        }
        s
    }

    /// The model back from `proc_text` (host tests draw the picture QEMU should show; DS-5 reads the same text).
    pub fn from_proc(text: &str) -> Option<Steps> {
        let mut st = Steps::new();
        for line in text.lines() {
            let mut w = line.splitn(5, ' ');
            match w.next()? {
                "seq" => st.seq = w.next()?.parse().ok()?,
                "generation" => st.generation = w.next()?.parse().ok()?,
                "step" => {
                    let id = w.next()?;
                    let k = KEYS.iter().copied().find(|k| k.id() == id)?;
                    let state = State::parse(w.next()?)?;
                    let t_ms = w.next()?.parse().ok()?;
                    let detail = Detail::from(w.next().unwrap_or(""));
                    st.steps[k.index()] = Step { state, detail, t_ms };
                }
                _ => {}
            }
        }
        Some(st)
    }
}

/// The desktop start's reason word (console-cmds' `desktop.failed`/`desktop.given_up`) in words.
pub fn why(reason: &str) -> &str {
    match reason {
        "not_found" => "nicht gefunden",
        "not_executable" => "kein programm",
        "no_memory" => "kein speicher",
        "refused" => "nicht erlaubt",
        "crashed" => "abgestürzt",
        "ended" => "beendet",
        "exit_code" => "mit fehler beendet",
        "no_init" => "init nicht bereit",
        "spawn" => "start fehlgeschlagen",
        other => other,
    }
}

/// `1234` ms as "1,23 s" (German decimal comma).
pub fn seconds(ms: u64) -> String {
    format!("{},{:02} s", ms / 1000, ms % 1000 / 10)
}
