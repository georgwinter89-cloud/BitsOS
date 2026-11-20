//! UPDATE in bitscomp: the page "Aktualisieren" talks to bitspkgd's update ops (pkg_client's line protocol) on a
//! thread of its own: the status and a check for a new version when the page opens, the install with its progress
//! lines when Georg presses "Aktualisieren", "Zurück", the token. Every changing press needs Georg's own input
//! (`pages.real`, as the page "KI"): an injected click or key changes nothing and the page says why; bitspkgd
//! then judges this compositor by fb0's grant (its peer rule) before it does anything.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Event};

use crate::ki::Built;
use crate::page::{Kind, Part};
use crate::page_kind::PageKind;
use crate::pkg_client::{Job, Msg};
use crate::state::App;
use crate::update_page::{self as up, Data};

pub const REAL_INPUT: &str = crate::ki_ask::REAL_INPUT;

#[derive(Clone, Debug, PartialEq)]
enum Cmd {
    Op(&'static str, String),
    Reboot,
}

#[derive(Clone, Debug)]
enum Back {
    Answer(&'static str, Json),
    Progress(String),
    Failed(String),
    Power(String, bool),
}

#[derive(Default)]
pub struct Upd {
    pub data: Data,
    pub built: Built,
    open: Arc<AtomicBool>,
    ask: Option<mpsc::Sender<Cmd>>,
}

fn line(op: &str, extra: Vec<(&str, Json)>) -> String {
    let mut v = vec![("op", Json::str(op))];
    v.extend(extra);
    let mut s = String::new();
    Json::obj(v).write_to(&mut s);
    s
}

/// One op on its own connection; progress lines go to `say`.
fn run(socket: &std::path::Path, op: &'static str, req: &str, say: &mut dyn FnMut(Back)) -> Back {
    let stream = op == "update_do";
    let wait = if stream { Duration::from_secs(3600) } else { Duration::from_secs(300) };
    let job = Job { id: 0, line: req.to_string(), stream, wait, idle: Duration::from_secs(600), local: None };
    let m = crate::pkg_client::run(socket, &job, &mut |m| {
        if let Msg::Line(_, j) = m {
            say(Back::Progress(j.get("progress").and_then(Json::as_str).unwrap_or("").to_string()));
        }
        true
    });
    match m {
        Some(Msg::End(_, j)) => Back::Answer(op, j),
        Some(Msg::Failed(_, why)) => Back::Failed(why),
        Some(Msg::Late(_)) => Back::Failed(String::from("Die Programmverwaltung antwortet nicht.")),
        _ => Back::Failed(String::from("Keine Antwort.")),
    }
}

impl App {
    pub fn upd_start(&mut self, handle: &LoopHandle<'static, App>, socket: String) {
        let (tx, rx) = channel::channel::<Back>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg(b) = ev {
                app.upd_back(b);
            }
        }) {
            eprintln!("bitscomp: aktualisieren: no channel ({e}); the page stays empty");
            return;
        }
        let (atx, arx) = mpsc::channel::<Cmd>();
        let sock = std::path::PathBuf::from(socket);
        let spawned = std::thread::Builder::new().name("bitscomp-update".into()).spawn(move || {
            while let Ok(c) = arx.recv() {
                let b = match c {
                    Cmd::Op(op, req) => run(&sock, op, &req, &mut |b| {
                        let _ = tx.send(b);
                    }),
                    Cmd::Reboot => {
                        let (t, down) = crate::sofa_io::power_request(std::path::Path::new(crate::sofa_io::POWER_SOCKET), "reboot", true);
                        Back::Power(t, down)
                    }
                };
                if tx.send(b).is_err() {
                    return;
                }
            }
        });
        match spawned {
            Ok(_) => self.upd.ask = Some(atx),
            Err(e) => eprintln!("bitscomp: aktualisieren: no thread ({e}); the page stays empty"),
        }
        self.upd_refresh();
    }

    fn upd_send(&mut self, op: &'static str, extra: Vec<(&str, Json)>) {
        let req = line(op, extra);
        match &self.upd.ask {
            Some(a) if a.send(Cmd::Op(op, req)).is_ok() => {}
            _ => self.upd.data.error = Some(String::from("Die Programmverwaltung ist nicht verbunden.")),
        }
    }

    fn upd_back(&mut self, b: Back) {
        let d = &mut self.upd.data;
        match b {
            Back::Answer("update_status", j) => up::status(d, &j),
            Back::Answer("update_check", j) => up::checked(d, &j),
            Back::Answer("update_do", j) => {
                up::done(d, &j);
                self.upd_send("update_status", vec![]);
            }
            Back::Answer(_, j) => {
                d.busy = false;
                if j.get("ok").and_then(Json::as_bool) != Some(true) {
                    d.said = Some((j.get("message").and_then(Json::as_str).unwrap_or("Nicht geklappt.").to_string(), true));
                } else {
                    d.said = None;
                }
                self.upd_send("update_status", vec![]);
            }
            Back::Progress(p) => d.progress = Some(p),
            Back::Failed(why) => {
                d.busy = false;
                d.progress = None;
                d.loaded = true;
                d.error = Some(why);
            }
            Back::Power(t, down) => d.said = Some((t, !down)),
        }
        self.upd_refresh();
    }

    pub fn upd_refresh(&mut self) {
        let b = up::build(&self.upd.data);
        if b != self.upd.built {
            self.upd.built = b;
            self.needs_frame = true;
        }
    }

    /// A page of this kind opened: the status, and a look for a new version (the only network use without a press).
    pub fn upd_opened(&mut self) {
        self.upd.open.store(true, Ordering::Relaxed);
        self.upd.data.busy = true;
        self.upd_send("update_status", vec![]);
        self.upd_send("update_check", vec![]);
        self.upd_refresh();
    }

    pub fn upd_pages_gone(&mut self) {
        let any = self.pages.by_pid.values().any(|s| s.kind == PageKind::Update);
        self.upd.open.store(any, Ordering::Relaxed);
    }

    fn upd_real(&mut self, pid: u32, what: &str) -> bool {
        if self.pages.real {
            return true;
        }
        eprintln!("bitscomp: aktualisieren: {what} by injected input refused");
        self.page_say(pid, REAL_INPUT, true);
        false
    }

    pub fn upd_act(&mut self, pid: u32, part: Part) {
        let rows = &self.upd.built.model.rows;
        match part {
            Part::DialogCancel => self.page_dialog_close(pid, false),
            Part::DialogOk => self.page_dialog_close(pid, true),
            Part::Field(r) if rows.get(r).is_some_and(|row| matches!(row.kind, Kind::Secret { .. })) => {
                if self.upd_real(pid, "typing the token") {
                    self.page_edit_begin(pid, part, String::new());
                }
            }
            Part::Button(r, o) => {
                let Some(row) = rows.get(r).cloned() else { return };
                let name = match &row.kind {
                    Kind::Buttons(v) => v.get(o).map(|b| b.0.clone()),
                    Kind::Secret { clear: true } => Some(String::from("loeschen")),
                    _ => None,
                };
                let Some(name) = name else { return };
                if !self.upd_real(pid, &name) {
                    return;
                }
                let d = &mut self.upd.data;
                d.said = None;
                match name.as_str() {
                    "aktualisieren" => {
                        d.busy = true;
                        d.progress = Some(String::from("Einen Moment …"));
                        self.upd_send("update_do", vec![]);
                    }
                    "pruefen" => {
                        d.busy = true;
                        self.upd_send("update_check", vec![]);
                    }
                    "zurueck" => {
                        d.busy = true;
                        self.upd_send("update_back", vec![]);
                    }
                    "vor" => {
                        d.busy = true;
                        self.upd_send("update_forward", vec![]);
                    }
                    "loeschen" => self.upd_send("update_token", vec![("clear", Json::Bool(true))]),
                    "neustart" => {
                        if let Some(a) = &self.upd.ask {
                            let _ = a.send(Cmd::Reboot);
                        }
                    }
                    "spaeter" => {
                        d.restart = false;
                        d.said = Some((String::from("Die neue Version startet beim nächsten Neustart."), false));
                    }
                    _ => {}
                }
                self.upd_refresh();
            }
            _ => {}
        }
    }

    /// A key for the token's field: an injected one ends the edit, nothing is stored.
    pub(crate) fn upd_key_guard(&mut self, pid: u32) -> bool {
        if self.pages.real {
            return false;
        }
        eprintln!("bitscomp: aktualisieren: an injected key reached the token's field: edit ended, nothing stored");
        self.page_edit_end();
        self.page_say(pid, REAL_INPUT, true);
        true
    }

    /// Enter in the token's field.
    pub(crate) fn upd_commit(&mut self, pid: u32, r: usize, text: String) {
        let secret = self.upd.built.model.rows.get(r).is_some_and(|row| row.key == up::TOKEN_KEY);
        if secret && self.upd_real(pid, "storing the token") && !text.trim().is_empty() {
            self.upd_send("update_token", vec![("token", Json::str(text.trim()))]);
        }
        crate::settings_write::wipe(text);
    }

    pub fn upd_report(&self) -> String {
        let d = &self.upd.data;
        format!(" aktualisieren loaded={} busy={} offer={} restart={}", d.loaded, d.busy, d.offer.as_ref().map_or(0, |o| o.0), d.restart)
    }
}

/// `ctl aktualisieren [status|pruefen]`: the page's rows as text (`label | value [buttons] | line`).
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first().copied()? != "aktualisieren" {
        return None;
    }
    match words.get(1).copied() {
        Some("status") => app.upd_send("update_status", vec![]),
        Some("pruefen") => app.upd_send("update_check", vec![]),
        Some(w) => return Some(Err(format!("aktualisieren: unknown word {w} (status, pruefen)"))),
        None => {}
    }
    Some(Ok(format!("aktualisieren{}\n{}end", app.upd_report(), up::rows_text(&app.upd.built))))
}
