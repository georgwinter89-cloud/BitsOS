//! Wave 44: the measurement run's thermal part (`messung::go`). After each stage that passed: the thermal taker's
//! counts since the stage before (`THERMO <stage> alarme=...`, read from the session, nothing sent). After `pmfw`
//! passed and at the end: the card's limits and ONE metrics transfer through the taker's gate (`Card::thermal`,
//! outside every stage call, so in no stage's `linuxseq` line), each of its lines under `THERMO <when>`.
//!
//! The stop rule's thermal part: an alert the taker took during the run, a temperature at or above its limit
//! less `MARGIN_C`, or a read that fails ends the run before the next stage touches the card (the run puts no
//! load on the card but the rungs; what heats it is the thing to look at). It only reads: a power-off for the
//! temperature is the taker's own (`thermal`).

use super::messung::Card;
use super::stage::Stage;
use super::thermal::Look;
use alloc::format;
use alloc::string::String;

/// Degrees below each of the table's limits at which the run stops.
pub const MARGIN_C: i32 = 10;

pub struct Watch {
    seen: [u64; 3],
}

/// (plain German, the detail) of a thermal stop.
pub type Hot = (String, String);

fn judge(look: &Look) -> Result<String, String> {
    let t = look.temps.as_ref().map_err(|e| format!("the temperatures could not be read: {e}"))?;
    let Some((edge, hotspot, mem, _)) = look.limits else { return Err(String::from("the card's table named no limits")) };
    for (what, now, limit) in [("edge", t.edge, edge), ("hotspot", t.hotspot, hotspot), ("memory", t.mem, mem)] {
        if i32::from(now) >= limit - MARGIN_C {
            return Err(format!("{what} {now} C, within {MARGIN_C} C of its limit {limit} C"));
        }
    }
    Ok(format!("edge {} C, hotspot {} C, memory {} C, each more than {MARGIN_C} C below its limit", t.edge, t.hotspot, t.mem))
}

impl Watch {
    pub fn new() -> Watch {
        Watch { seen: [0; 3] }
    }

    fn read(card: &mut dyn Card, when: &str, text: &mut String) -> Option<Hot> {
        let look = card.thermal();
        for l in &look.lines {
            text.push_str(&format!("THERMO {when} {l}\n"));
        }
        match judge(&look) {
            Ok(w) => {
                text.push_str(&format!("THERMO {when} urteil ok {w}\n"));
                None
            }
            Err(e) => {
                text.push_str(&format!("THERMO {when} urteil STOPP {e}\n"));
                Some((String::from("die Karte ist zu warm oder ihre Temperatur ließ sich nicht lesen"), e))
            }
        }
    }

    /// After a stage that passed: its line, and the stop it calls for.
    pub fn after(&mut self, card: &mut dyn Card, st: Stage) -> (String, Option<Hot>) {
        let (mut now, mut armed) = ([0u64; 3], false);
        card.session(&mut |s| {
            if let Some(ih) = s.nb.ih.as_ref() {
                (now, armed) = (ih.reader.thermal.count, ih.reader.thermal.sensors.is_some());
            }
        });
        let new = [now[0].saturating_sub(self.seen[0]), now[1].saturating_sub(self.seen[1]), now[2].saturating_sub(self.seen[2])];
        self.seen = now;
        let mut text = format!("THERMO {} alarme=+{}:+{}:+{} (rose above, fell below, power controller)\n", st.name(), new[0], new[1], new[2]);
        if new.iter().any(|n| *n > 0) {
            return (text, Some((String::from("die Karte hat einen Wärmealarm gemeldet"), format!("the thermal taker took {}:{}:{} entries during {}", new[0], new[1], new[2], st.name()))));
        }
        let hot = if st == Stage::Pmfw && armed { Self::read(card, "pmfw", &mut text) } else { None };
        (text, hot)
    }

    /// At the end, after the last stage: one read more.
    pub fn end(&mut self, card: &mut dyn Card) -> String {
        let mut armed = false;
        card.session(&mut |s| armed = s.nb.ih.as_ref().is_some_and(|ih| ih.reader.thermal.sensors.is_some()));
        if !armed {
            return String::from("THERMO ende nicht gelesen: die Energieverwaltung wurde in diesem Lauf nicht gestartet\n");
        }
        let mut text = String::from("## Temperaturen am Ende (eine Abfrage der Energieverwaltung)\n");
        let _ = Self::read(card, "ende", &mut text);
        text
    }
}

impl Default for Watch {
    fn default() -> Self {
        Watch::new()
    }
}
