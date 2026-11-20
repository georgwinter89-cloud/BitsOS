//! The measurement run's recorder (`messung::go` drives it): the preflight, a look, the record of one stage call,
//! the end, each written to the protocol (`messproto` words the lines). Every write is on the disk when `put`
//! returns `Ok`; one that fails stops the run before the next access of the card, since that access's evidence
//! would be lost (`messung`, module head).

use super::depth;
use super::ihring::Reader;
use super::messcheck::{self, Facts};
use super::messproto::{self as proto, Names};
use super::messung::{Card, Disk, Env, Outcome, RUN, Took, OFF_IN_S};
use super::regs14::Regs14;
use super::stage::Stage;
use crate::gpucore::trail::Access;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub struct Run<'a> {
    disk: &'a mut dyn Disk,
    pub env: &'a mut dyn Env,
    top: usize,
    t0: u64,
    names: Names,
    regs: Option<(Regs14, u64)>,
    /// The IH reader as the last record saw it.
    ih: Option<Reader>,
    /// Sections whose declared rules are in the protocol already.
    sections: BTreeSet<&'static str>,
    /// A write failed: said on the screen once.
    told: bool,
}

/// A preflight that failed: the check's name (`messung::screen`) and the protocol's detail.
pub type Halt = (&'static str, String);

impl<'a> Run<'a> {
    pub fn new(disk: &'a mut dyn Disk, env: &'a mut dyn Env, top: usize, t0: u64) -> Run<'a> {
        Run { disk, env, top, t0, names: Names::new(), regs: None, ih: None, sections: BTreeSet::new(), told: false }
    }

    pub fn since_ms(&mut self) -> u64 {
        self.env.now_us().saturating_sub(self.t0) / 1000
    }

    /// Appends, with the kernel's log lines since the last write; `Err` when the text is not on the disk.
    pub fn put(&mut self, text: &str) -> Result<(), String> {
        let mut all = String::from(text);
        for l in self.env.log() {
            all.push_str("LOG ");
            all.push_str(&l);
            all.push('\n');
        }
        let r = self.disk.put(&all);
        if let Err(e) = &r
            && !self.told
        {
            self.told = true;
            self.env.say(&format!("Das Protokoll ließ sich nicht weiterschreiben ({e}); die Messung hält an."));
        }
        r
    }

    /// The preflight; `Err` stops the run before any write to the card.
    pub fn preflight(&mut self, card: &mut dyn Card, facts: &Facts) -> Result<(), Halt> {
        let mut text = String::from("## Vorprüfung (nichts wird auf die Karte geschrieben)\n");
        let checks = messcheck::judge(facts);
        let mut failed: Option<Halt> = None;
        for (name, r) in &checks {
            match r {
                Ok(what) => text.push_str(&format!("PRUEF {name} ok {what}\n")),
                Err(why) => {
                    text.push_str(&format!("PRUEF {name} FEHLER {why}\n"));
                    failed = failed.or_else(|| Some((name, format!("{name}: {why}"))));
                }
            }
        }
        for f in &facts.files {
            let (word, what) = match &f.state {
                Ok(w) => ("ok", w.clone()),
                Err(w) => (if w.starts_with("fehlt") { "FEHLT" } else { "DEFEKT" }, w.clone()),
            };
            let also: Vec<&str> = f.also.iter().map(|s| s.name()).collect();
            text.push_str(&format!("FIRMWARE {} {word} stage={} also={} measured={} {what}\n", f.file, f.stage.name(), if also.is_empty() { String::from("-") } else { also.join(",") }, f.measured));
        }
        if let Some((name, why)) = failed {
            text.push_str(&format!("STOPP vorpruefung {why} | preflight\n"));
            let _ = self.put(&text);
            return Err((name, why));
        }
        self.regs = card.regs();
        let snap = card.snapshot();
        text.push_str(&proto::lines("GPU", "vorher", &snap));
        self.put(&text).map_err(|e| (super::messung::PROTOCOL, e))?;
        // The card cold as Linux found it: no secure OS. The look's first read of C2PMSG_81 says it.
        let sol = self.look(card, Stage::Alive, "vorher", String::new()).map_err(|e| (super::messung::PROTOCOL, e))?;
        let cold = match (&self.regs, &sol) {
            (Some((r, _)), Some(trail)) => proto::first_read(trail, r.psp.c2pmsg_81).flatten(),
            _ => None,
        };
        match cold {
            Some(0) => {
                self.put("PRUEF kalt ok C2PMSG_81 0x00000000: kein sicheres Betriebssystem läuft, die Karte ist so kalt, wie Linux sie fand\n").map_err(|e| (super::messung::PROTOCOL, e))?;
            }
            other => {
                let (check, why) = match other {
                    Some(v) => ("kalt", format!("C2PMSG_81 ist {v:#010x}: ein sicheres Betriebssystem läuft schon, die Karte ist nicht kalt (sie hatte seit dem letzten Einschalten schon Strom und einen Treiber, etwa nach einem Neustart statt eines Ausschaltens)")),
                    None => ("kalt-unlesbar", String::from("C2PMSG_81 ließ sich nicht lesen")),
                };
                let _ = self.put(&format!("PRUEF kalt FEHLER {why}\nSTOPP vorpruefung {why} | preflight\n"));
                return Err((check, why));
            }
        }
        for st in [Stage::Bootloader, Stage::State] {
            self.look(card, st, "vorher", String::new()).map_err(|e| (super::messung::PROTOCOL, e))?;
        }
        Ok(())
    }

    /// A look that only reads, recorded under `after`: its START line (after `carried`, in one write) is on the
    /// disk before it reads, so a hang in it is told apart from one in the stage before. Its trail for the
    /// caller; `Err` when the protocol could not be written (the look does not run then).
    pub fn look(&mut self, card: &mut dyn Card, st: Stage, after: &str, carried: String) -> Result<Option<Vec<Access>>, String> {
        let mut start = carried;
        start.push_str(&format!("BLICK {} nach={after} START\n", st.name()));
        self.put(&start)?;
        let r = card.stage(st);
        let (text, trail) = match &r {
            Ok(t) => {
                let mut text = String::new();
                for l in t.text.lines() {
                    text.push_str(&format!("BLICK {} nach={after} {l}\n", st.name()));
                }
                text.push_str(&format!("BLICK {} nach={after} writes={} us={}\n", st.name(), t.d.totals.writes, t.us));
                (text, Some(t.d.trail.clone()))
            }
            Err(e) => (format!("BLICK {} nach={after} refused {e}\n", st.name()), None),
        };
        self.put(&text)?;
        Ok(trail)
    }

    /// Everything of one stage call, as text.
    pub fn record(&mut self, card: &mut dyn Card, st: Stage, took: &Result<Took, String>) -> String {
        let name = st.name();
        let t = match took {
            Ok(t) => t,
            Err(e) => return format!("ANTWORT {name} refused before the stage ran: {e}\n"),
        };
        let mut text = proto::lines("ANTWORT", name, &t.text);
        text.push_str(&format!("ZEIT {name} us={}\n", t.us));
        text.push_str(&format!("STAPEL {name} bytes={}\n", depth::below(self.top, t.lowest)));
        text.push_str(&proto::verdict(st, t.d.seq.as_ref()));
        text.push('\n');
        let (mut rules, mut ih) = (Vec::new(), None);
        let section = t.d.seq.as_ref().map(|v| v.section);
        let vram = self.regs.as_ref().map_or(0, |r| r.1);
        let fresh = section.filter(|s| !self.sections.contains(s));
        card.session(&mut |s| {
            if let Some(sec) = fresh {
                rules = proto::rules(sec, s, vram);
            }
            ih = s.nb.ih.as_ref().map(|u| u.reader.clone());
        });
        if let Some(sec) = fresh {
            self.sections.insert(sec);
        }
        for l in rules {
            text.push_str(&l);
            text.push('\n');
        }
        match self.regs.as_ref().map(|(r, _)| proto::trail(&t.d.trail, r, &self.names)) {
            Some(Ok(lines)) => {
                text.push_str(&format!("SPURINFO {name} accesses={} lines={} lost={}\n", t.d.trail.len(), lines.len(), t.d.trail_lost));
                for l in lines {
                    text.push_str(&format!("SPUR {name} {l}\n"));
                }
            }
            Some(Err(e)) => text.push_str(&format!("SPURINFO {name} accesses={} lines=0 lost={} unfolded={e}\n", t.d.trail.len(), t.d.trail_lost)),
            None => text.push_str(&format!("SPURINFO {name} accesses={} lines=0 lost={} unfolded=no register table\n", t.d.trail.len(), t.d.trail_lost)),
        }
        text.push_str(&proto::ih(name, self.ih.as_ref(), ih.as_ref()));
        text.push('\n');
        self.ih = ih;
        text
    }

    /// The looks once more, the snapshot after, the last line. Every write is tried: the disk may be back.
    pub fn end(&mut self, card: &mut dyn Card, out: &mut Outcome) {
        let after = out.ran.last().map_or("vorpruefung", |s| s.name());
        let touched = !out.ran.is_empty();
        if touched {
            let mut carried = String::from("## Am Ende (nur Lesen)\n");
            for st in [Stage::Alive, Stage::State] {
                let _ = self.look(card, st, "ende", core::mem::take(&mut carried));
            }
            let mut ih = None;
            card.session(&mut |s| ih = s.nb.ih.as_ref().map(|u| u.reader.clone()));
            let line = proto::ih("ende", self.ih.as_ref(), ih.as_ref());
            let snap = card.snapshot();
            let _ = self.put(&format!("{line}\n{}", proto::after(&snap)));
        }
        let how = match &out.stopped {
            None => format!("alle {} Stufen bestanden", RUN.len()),
            Some((at, why)) => format!("angehalten bei {at}: {why}"),
        };
        let ms = self.since_ms();
        let _ = self.put(&format!("ENDE nach={after} t_ms={ms} {how}; der PC schaltet sich in {OFF_IN_S} Sekunden aus\n"));
    }
}
