//! One-time changes of stored values when a default changes meaning. The marker file `settings/MIGRATED` on
//! the stick lists the migrations done (one name per line), so a value Georg sets afterwards stays as he set it.

use crate::change::Who;
use crate::events::{Level, q};
use crate::registry::Registry;
use crate::store::{self, Job, Place};
use crate::value::Value;
use std::time::{Duration, Instant};

/// How often a refused marker write is tried again.
const MARKER_RETRY: Duration = Duration::from_secs(2);

/// FS-13b: kernel.preload.max_mb's default became `all`; a stored 1024 was the old default, so it goes.
const PRELOAD_ALL: &str = "preload_all";

impl Registry {
    /// Runs the migrations not done yet. Only on the stick (the marker lives there); on the fallback they wait
    /// for a later start.
    pub(crate) fn migrate(&mut self) {
        if !matches!(self.place, Place::Stick) {
            return;
        }
        let marker = self.paths.dir(&Place::Stick).join("MIGRATED");
        let m2 = marker.clone();
        let done = match self.writer.call(move || store::read_text(&m2)) {
            Ok(Ok(t)) => t,
            Ok(Err(_)) => String::new(),
            Err(e) => {
                self.ev.emit(Level::Warn, "migrate.unreadable", &format!("error={}", q(&e)));
                return;
            }
        };
        if done.lines().any(|l| l.trim() == PRELOAD_ALL) {
            return;
        }
        let key = "kernel.preload.max_mb";
        if let Some(d) = self.defs.get(key).cloned()
            && self.values.get(key) == Some(&Value::Int(1024))
        {
            let who = Who { source: "settingsd-migrate", confirmed: true };
            match self.change(&d, None, &who, "migrate", None) {
                Ok(_) => self.ev.emit(
                    Level::Info,
                    "migrated",
                    &format!("key={key} from=1024 to={}", d.default.show()),
                ),
                Err(e) => {
                    self.ev.emit(Level::Warn, "migrate.failed", &format!("key={key} error={}", q(&e.msg)));
                    return;
                }
            }
        }
        let mut text = done;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(PRELOAD_ALL);
        text.push('\n');
        self.marker_due = Some(Marker { path: marker, text: text.into_bytes(), at: Instant::now(), tries: 0 });
        self.write_marker();
    }

    /// Writes a due migration marker. At boot settingsd starts before the stick is writable (`fat stick.decided`
    /// about a second later): a refused write is tried again from `tick`, 2 s later, doubling up to a minute.
    pub(crate) fn write_marker(&mut self) {
        let Some(mut m) = self.marker_due.take() else { return };
        if Instant::now() < m.at {
            self.marker_due = Some(m);
            return;
        }
        match self.writer.run(vec![Job::Write(m.path.clone(), m.text.clone())]) {
            Ok(()) => self.ev.emit(Level::Info, "migrate.marked", &format!("file={} tries={}", m.path.display(), m.tries + 1)),
            Err(e) => {
                if e.starts_with(store::SLOW) {
                    self.replace_writer();
                }
                if m.tries == 0 {
                    self.ev.emit(Level::Info, "migrate.marker_later", &format!("error={}", q(&e)));
                }
                let gap = MARKER_RETRY.saturating_mul(1 << m.tries.min(5)).min(Duration::from_secs(60));
                m.tries += 1;
                m.at = Instant::now() + gap;
                self.marker_due = Some(m);
            }
        }
    }
}

/// A migration marker still to be written.
pub(crate) struct Marker {
    pub(crate) path: std::path::PathBuf,
    pub(crate) text: Vec<u8>,
    pub(crate) at: Instant,
    pub(crate) tries: u32,
}
