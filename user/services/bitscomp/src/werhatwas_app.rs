//! DB2 in bitscomp: the page "Wer hat was gemacht" gets bitsaccess's `summary` on a thread of its own (every 10 s
//! while such a page is open, and at once when it opens or its time filter changes), turns it into rows
//! (`werhatwas::build`), takes its clicks (the filter, the switch `access.enabled` through settingsd) and answers
//! `ctl werhatwas` (the rows as text, for scenarios and the Mini-PC checks).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
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
use crate::settings_write::Req;
use crate::state::App;
use crate::werhatwas::{self, Data, RANGE_KEY, SWITCH_KEY};

pub const SOCKET: &str = "/run/bitsos/access.sock";
const RELOAD: Duration = Duration::from_secs(10);
const NO_ANSWER: &str = "Der Dienst für „Wer hat was gemacht“ antwortet nicht. Er startet mit BitsOS; ist er gerade erst gestartet, kommt die Liste gleich.";

pub struct WerHatWas {
    pub data: Data,
    /// SYS-A: only this program (`einstellungen/werhatwas/<name>`, the task manager's link).
    pub only: Option<String>,
    pub range: String,
    pub built: Built,
    open: Arc<AtomicBool>,
    ask: Option<mpsc::Sender<String>>,
}

impl Default for WerHatWas {
    fn default() -> Self {
        WerHatWas { data: Data::default(), only: None, range: werhatwas::RANGES[0].0.into(), built: Built::default(), open: Arc::new(AtomicBool::new(false)), ask: None }
    }
}

fn socket() -> String {
    SOCKET.into()
}

/// (now in unix ms, the local offset): the host tests' fixed clock (`Status::fixed`, UTC), else the real one.
pub fn clock(fixed: bool) -> (u64, i32) {
    if fixed { (crate::status::FIXED_SECS as u64 * 1000, 0) } else { (crate::status::epoch_secs().max(0) as u64 * 1000, crate::status::offset_min()) }
}

/// bitsaccess's summary for `range`.
pub fn fetch(sock: &str, range: &str, fixed: bool) -> Data {
    let (now, off) = clock(fixed);
    let since = werhatwas::since_s(range, now, off);
    let ask = || -> Result<Json, String> {
        let s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let mut w = s.try_clone().map_err(|e| e.to_string())?;
        writeln!(w, "{{\"id\":1,\"op\":\"summary\",\"args\":{{\"since_s\":{since}}}}}").map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
        let j = Json::parse(line.trim())?;
        match j.get("result") {
            Some(r) => Ok(r.clone()),
            None => Err(j.get("error").and_then(Json::as_str).unwrap_or("keine Antwort").to_string()),
        }
    };
    match ask() {
        Ok(r) => werhatwas::parse(&r),
        Err(e) => {
            eprintln!("bitscomp: werhatwas: {sock}: {e}");
            Data { loaded: true, error: Some(NO_ANSWER.into()), ..Data::default() }
        }
    }
}

impl App {
    pub fn wer_start(&mut self, handle: &LoopHandle<'static, App>) {
        let (tx, rx) = channel::channel::<(String, Data)>();
        let (atx, arx) = mpsc::channel::<String>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg((range, d)) = ev
                && range == app.wer.range
            {
                app.wer.data = d;
                app.wer_refresh();
            }
        }) {
            eprintln!("bitscomp: werhatwas: no channel ({e}); the page stays empty");
            return;
        }
        let open = self.wer.open.clone();
        let sock = socket();
        let fixed = self.status.fixed;
        let spawned = std::thread::Builder::new().name("bitscomp-werhatwas".into()).spawn(move || {
            let mut range: Option<String> = None;
            loop {
                match arx.recv_timeout(RELOAD) {
                    Ok(r) => range = Some(r),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                if let Some(r) = range.as_ref().filter(|_| open.load(Ordering::Relaxed))
                    && tx.send((r.clone(), fetch(&sock, r, fixed))).is_err()
                {
                    return;
                }
            }
        });
        match spawned {
            Ok(_) => self.wer.ask = Some(atx),
            Err(e) => eprintln!("bitscomp: werhatwas: no thread ({e}); the page stays empty"),
        }
        self.wer_refresh();
    }

    fn wer_enabled(&self) -> bool {
        self.settings.flag(SWITCH_KEY).unwrap_or(true)
    }

    pub fn wer_refresh(&mut self) {
        let (now, off) = clock(self.status.fixed);
        let d = crate::sys_wer::only(&self.wer.data, self.wer.only.as_deref());
        let b = werhatwas::build(&d, &self.wer.range, self.wer_enabled(), now, off);
        if b != self.wer.built {
            self.wer.built = b;
            self.needs_frame = true;
        }
    }

    fn wer_load(&mut self) {
        if let Some(a) = &self.wer.ask {
            let _ = a.send(self.wer.range.clone());
        }
    }

    /// A page of this kind opened: load now.
    pub fn wer_opened(&mut self) {
        self.wer.open.store(true, Ordering::Relaxed);
        self.wer_load();
    }

    /// After pages went away: stop loading when none of this kind is left.
    pub fn wer_pages_gone(&mut self) {
        let any = self.pages.by_pid.values().any(|s| s.kind == PageKind::WerHatWas);
        self.wer.open.store(any, Ordering::Relaxed);
    }

    /// The rows as the page shows them now: the switch as settingsd says this moment.
    pub fn wer_now(&self) -> Built {
        let mut b = self.wer.built.clone();
        werhatwas::switch_now(&mut b, self.wer_enabled());
        b
    }

    pub fn wer_act(&mut self, pid: u32, part: Part) {
        match part {
            Part::DialogCancel => self.page_dialog_close(pid, false),
            Part::DialogOk => self.page_dialog_close(pid, true),
            Part::Undo => self.page_send(pid, Req::UNDO),
            Part::Pill(r, o) => {
                let Some(row) = self.wer.built.model.rows.get(r) else { return };
                if let (true, Kind::Pills { opts, .. }) = (row.key == RANGE_KEY, &row.kind)
                    && let Some((v, _)) = opts.get(o)
                    && *v != self.wer.range
                {
                    self.wer.range = v.clone();
                    self.wer.data = Data::default();
                    self.wer_refresh();
                    self.wer_load();
                }
            }
            Part::Switch(r) if self.wer.built.model.rows.get(r).is_some_and(|row| row.key == SWITCH_KEY) => {
                let on = self.wer_enabled();
                self.page_send(pid, Req::set(SWITCH_KEY, Json::Bool(!on)));
            }
            _ => {}
        }
    }

    pub fn wer_report(&self) -> String {
        format!(" werhatwas range={} loaded={} programs={}", self.wer.range, self.wer.data.loaded, self.wer.data.programs.len())
    }
}

/// `ctl werhatwas [reload|heute|7|30]`: the page's rows as text (`label | value | when | line`).
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first().copied()? != "werhatwas" {
        return None;
    }
    match words.get(1).copied() {
        Some(r) if werhatwas::RANGES.iter().any(|x| x.0 == r) => {
            app.wer.range = r.to_string();
            app.wer.data = Data::default();
            app.wer_refresh();
            app.wer_load();
        }
        Some("reload") => app.wer_load(),
        Some(w) => return Some(Err(format!("werhatwas: unknown word {w} (reload, heute, 7, 30)"))),
        None => {}
    }
    let on = app.wer_enabled();
    Some(Ok(format!("werhatwas range={} loaded={} programs={} on={on}\n{}end", app.wer.range, app.wer.data.loaded, app.wer.data.programs.len(), werhatwas::rows_text(&app.wer_now()))))
}
