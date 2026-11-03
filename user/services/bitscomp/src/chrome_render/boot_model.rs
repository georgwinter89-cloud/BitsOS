//! DS-5: the step list of `/proc/bitsos/boot` (DS-4's interface, plan/notes/in-DS-4.md) and bitscomp's own steps
//! that continue it. Pure: parsing, the merge and the picture's `View` are host-tested.

use super::boot_draw::{Art, Row, State, View};

/// One step as the file says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub state: State,
    pub t_ms: u64,
    pub detail: String,
}

/// The file's content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Steps {
    pub seq: u64,
    pub drawn: u64,
    pub screen: String,
    pub generation: u64,
    pub steps: Vec<Step>,
}

/// The word on screen for a step id (crates/console/src/bootsteps.rs `Key::label`); an unknown id shows itself.
pub fn label(id: &str) -> &str {
    match id {
        "kernel" => "kern",
        "memory" => "speicher",
        "cpus" => "prozessoren",
        "storage" => "laufwerke",
        "network" => "netzwerk",
        "sound" => "ton",
        "gpu" => "grafik",
        "services" => "dienste",
        "mcp" => "mcp",
        "desktop" => "arbeitsfläche",
        other => other,
    }
}

/// `1234` ms as "1,23 s" (German decimal comma).
pub fn seconds(ms: u64) -> String {
    format!("{},{:02} s", ms / 1000, ms % 1000 / 10)
}

impl Steps {
    /// None for a file that is not format 1 or has no step (an older kernel, a test's empty file).
    pub fn parse(text: &str) -> Option<Steps> {
        let mut lines = text.lines();
        if lines.next()?.trim() != "bitsos-boot 1" {
            return None;
        }
        let mut st = Steps::default();
        for line in lines {
            let mut w = line.splitn(5, ' ');
            match w.next() {
                Some("seq") => st.seq = w.next()?.parse().ok()?,
                Some("drawn") => st.drawn = w.next()?.parse().ok()?,
                Some("screen") => st.screen = w.next()?.to_string(),
                Some("generation") => st.generation = w.next()?.parse().ok()?,
                Some("step") => {
                    let id = w.next()?.to_string();
                    let state = State::parse(w.next()?)?;
                    let t_ms = w.next()?.parse().ok()?;
                    let detail = w.next().unwrap_or("").chars().filter(|c| !c.is_control()).take(80).collect();
                    st.steps.push(Step { id, state, t_ms, detail });
                }
                _ => {}
            }
        }
        (!st.steps.is_empty()).then_some(st)
    }

    pub fn get(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// The list with the desktop row replaced by bitscomp's own step (`own`), when there is one.
    pub fn with_desktop(&self, own: Option<&Step>) -> Steps {
        let mut s = self.clone();
        if let Some(o) = own {
            match s.steps.iter_mut().find(|x| x.id == "desktop") {
                Some(d) => *d = o.clone(),
                None => s.steps.push(o.clone()),
            }
        }
        s
    }

    /// The line under the title (the kernel's `Steps::status`).
    pub fn status(&self) -> String {
        if let Some(s) = self.steps.iter().find(|s| s.state == State::Running) {
            return format!("Startet: {}", label(&s.id));
        }
        let failed: Vec<&str> = self.steps.iter().filter(|s| s.state == State::Failed).map(|s| label(&s.id)).collect();
        let last = self.steps.iter().map(|s| s.t_ms).max().unwrap_or(0);
        let (done, all) = self.progress();
        if done < all {
            return String::from("Startet …");
        }
        if failed.is_empty() { format!("Bereit nach {}.", seconds(last)) } else { format!("Bereit nach {}, Fehler bei {}.", seconds(last), failed.join(", ")) }
    }

    pub fn progress(&self) -> (usize, usize) {
        (self.steps.iter().filter(|s| s.state.is_final()).count(), self.steps.len())
    }

    /// The picture's content (the kernel's `View::of`).
    pub fn view(&self, art: &Art) -> View {
        let rows = self
            .steps
            .iter()
            .map(|st| {
                let d = if st.state == State::Waiting { "wartet" } else { st.detail.as_str() };
                let time = if st.state == State::Waiting { String::new() } else { seconds(st.t_ms) };
                let (cut, more) = art.mono.fit(d, art.detail_max(&time), 0);
                let detail = if more { format!("{cut}…") } else { cut.to_string() };
                Row { state: st.state, label: label(&st.id).to_string(), detail, time }
            })
            .collect();
        let warn = self.steps.iter().any(|s| s.state == State::Failed) && !self.steps.iter().any(|s| s.state == State::Running);
        let header = if self.generation > 0 { format!("GENERATION {}", self.generation) } else { String::from("BITSOS V2") };
        View { rows, status: self.status(), status_warn: warn, progress: self.progress(), header }
    }
}

/// bitscomp's own part of the start, shown in the desktop row: its start, the shell and status services (the
/// user's settings and the status values), the first program's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Own {
    Services,
    Window,
    Ready,
}

impl Own {
    pub fn detail(self, program: &str) -> String {
        match self {
            Own::Services => String::from("bitscomp: einstellungen, status"),
            Own::Window if program.is_empty() => String::from("bitscomp: erstes fenster"),
            Own::Window => format!("bitscomp: erstes fenster ({program})"),
            Own::Ready => String::from("bitscomp bereit"),
        }
    }

    pub fn step(self, program: &str, t_ms: u64) -> Step {
        let state = if self == Own::Ready { State::Done } else { State::Running };
        Step { id: String::from("desktop"), state, t_ms, detail: self.detail(program) }
    }
}

/// What the desktop still waits for. `settings`: settingsd answered, or bitscomp has no settings connection, or
/// the wait for it ran out; `status`: the clock has its first value; `window`: no program was started, or one has
/// a window.
pub fn own(settings: bool, status: bool, window: bool) -> Own {
    if !(settings && status) {
        Own::Services
    } else if !window {
        Own::Window
    } else {
        Own::Ready
    }
}
