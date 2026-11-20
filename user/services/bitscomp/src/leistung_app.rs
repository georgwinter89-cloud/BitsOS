//! SELFOPT-1 in bitscomp: whether anybody uses the machine (`ctl idle`, read by bitsbench before and during a
//! night run: physical input only, from keyboards, mice and pads; MCP's and the control socket's input never
//! counts), the morning line shown at Georg's first input after a night run (BitsOS has no lock screen: his
//! first touch is the "unlock"), and the data of the page "Leistung", read on a thread of its own every 30 s.

use std::sync::mpsc;
use std::time::Duration;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Event};

use crate::leistung::{self, Data};
use crate::state::App;

/// How long the morning line stays (a notice's usual 5 s is too short to read it over a coffee).
pub const MORNING_FOR: Duration = Duration::from_secs(20);
const RELOAD: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Leistung {
    pub data: Data,
    /// Physical input events since bitscomp started, and the last one's time (pacer clock).
    pub inputs: u64,
    pub last_input: Option<Duration>,
    reload: Option<mpsc::Sender<()>>,
}

impl App {
    pub fn leistung_start(&mut self, handle: &LoopHandle<'static, App>) {
        let (tx, rx) = channel::channel::<Data>();
        let (rtx, rrx) = mpsc::channel::<()>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg(d) = ev {
                app.leistung_loaded(d);
            }
        }) {
            eprintln!("bitscomp: leistung: no channel ({e}); the page shows no history");
            return;
        }
        let root = leistung::dir();
        let spawned = std::thread::Builder::new().name("bitscomp-leistung".into()).spawn(move || {
            loop {
                if tx.send(leistung::load(&root)).is_err() {
                    return;
                }
                match rrx.recv_timeout(RELOAD) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        match spawned {
            Ok(_) => self.leistung.reload = Some(rtx),
            Err(e) => eprintln!("bitscomp: leistung: no thread ({e}); the page shows no history"),
        }
    }

    fn leistung_loaded(&mut self, d: Data) {
        // A line shown meanwhile stays shown, whatever the file said when it was read.
        let shown = self.leistung.data.shown.clone().filter(|s| d.report.as_ref().is_some_and(|r| r.0 == *s));
        let d = Data { shown: shown.or(d.shown), ..d };
        if d != self.leistung.data {
            self.leistung.data = d;
            self.needs_frame = true;
        }
    }

    /// Every physical input event: counts for `idle`, and brings the morning line once.
    pub fn leistung_input(&mut self) {
        self.leistung.inputs += 1;
        self.leistung.last_input = Some(crate::pacer::now());
        if let Some((night, line)) = leistung::pending(&self.leistung.data) {
            self.leistung.data.shown = Some(night.clone());
            self.notice_for(line.clone(), MORNING_FOR);
            eprintln!("bitscomp: leistung: morning line shown night={night} text={line:?}");
            leistung::mark_shown(leistung::dir(), night);
        }
    }

    /// Windows of games: a library entry that is a game owns their app id, or Steam named them `steam_app_*`.
    fn leistung_games(&self) -> usize {
        self.all_windows()
            .iter()
            .filter(|w| {
                let id = crate::ctl::names(w).0;
                id.starts_with("steam_app_") || self.library.entries.iter().any(|e| e.owns(&id) && crate::sofa_model::is_game(e))
            })
            .count()
    }

    /// The lines under the page's own rows (other rows keep their address there); empty without such rows.
    pub fn leistung_subs(&self, rows: &[crate::page::Row]) -> Vec<Option<(String, crate::page_draw::Tone)>> {
        if !rows.iter().any(|r| leistung::is_own(&r.key)) {
            return Vec::new();
        }
        let origin = leistung::origin();
        rows.iter()
            .map(|r| leistung::is_own(&r.key).then(|| (leistung::sub(&self.leistung.data, &r.key, &origin), crate::page_draw::Tone::Plain)))
            .collect()
    }

    pub fn leistung_idle(&self) -> String {
        let now = crate::pacer::now();
        let since = now.saturating_sub(self.leistung.last_input.unwrap_or(Duration::ZERO));
        format!(
            "idle input_s={} inputs={} fullscreen={} games={} offset_min={}",
            since.as_secs(),
            self.leistung.inputs,
            u8::from(self.ws.fullscreen_shown().is_some()),
            self.leistung_games(),
            crate::status::offset_min()
        )
    }
}

/// `ctl idle`, `ctl leistung [reload]`.
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    match words.first().copied()? {
        "idle" => Some(Ok(app.leistung_idle())),
        "leistung" => {
            if words.get(1) == Some(&"reload") {
                if let Some(r) = &app.leistung.reload {
                    let _ = r.send(());
                }
            }
            let d = &app.leistung.data;
            let charted = leistung::BENCHES.iter().filter(|b| d.history.iter().any(|n| n.medians.contains_key(b.0))).count();
            Some(Ok(format!(
                "leistung report={} shown={} line={:?} nights={} charted={} cachyos={} origin={:?}",
                d.report.as_ref().map_or("-", |r| r.0.as_str()),
                d.shown.as_deref().unwrap_or("-"),
                d.report.as_ref().map_or("", |r| r.1.as_str()),
                d.history.len(),
                charted,
                d.cachyos.as_ref().map_or("-", |c| c.night.as_str()),
                leistung::origin(),
            )))
        }
        _ => None,
    }
}
