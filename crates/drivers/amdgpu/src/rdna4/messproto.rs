//! The measurement run's protocol (`messung`): one fact per line, a fixed key first so a script reads it
//! (`host/gaming-pc/gx-messung-eval.py`), German headings (`## ...`) Georg can follow. This file only words
//! lines; writing them is the run's.
//!
//! | key | what |
//! |---|---|
//! | `KOPF` | the run's head facts: format, kernel, card, log level, the order of stages, the limits, the stack budget, the card's revision (STRAP0, `ausfuehrung`) |
//! | `REGEL` | what the run does and when it stops, in plain German |
//! | `PRUEF` | one preflight check: `PRUEF <name> ok|FEHLER <detail>` |
//! | `FIRMWARE` | one firmware file: `FIRMWARE <file> ok|FEHLT|DEFEKT stage=<first stage> ...` |
//! | `GPU` | the read-only `gpu` snapshot: `GPU vorher <line>`; after the run only what is read anew, `GPU nachher <line>` |
//! | `START` | written and flushed before a stage runs: `START <stage> nr=<n> t_ms=<since the J>` |
//! | `ANTWORT` | the stage's answer as `gx_bringup` gives it, line by line |
//! | `ZEIT`, `STAPEL` | the stage's duration (us) and how deep below the run's entry it reached the card (bytes) |
//! | `VERGLEICH` | the stage's `linuxseq` line, typed: `section=<s> said=<kind> answers=<n>` |
//! | `REGELN` | a section's declared differences (once per section): `REGELN <section> <kind> <names> ...` |
//! | `SPURINFO`, `SPUR` | the stage's register accesses as Linux's trace shows them (the fixtures' line forms) |
//! | `IH` | the card's messages counted since the stage before |
//! | `THERMO` | wave 44: the thermal taker's alerts since the stage before; after `pmfw` and at the end the card's limits, one read of its temperatures and the run's judgement (`messtherm`) |
//! | `BLICK` | a look that only reads: `BLICK <look> nach=<stage> START` on the disk before it reads, then its lines |
//! | `LOG` | the kernel's own log lines of the run |
//! | `WEITER`, `STOPP` | the stop rule's decision after a stage |
//! | `ENDE` | the last line |

use super::chainseq::{self, Chosen, Verdict};
use super::ihring::Reader;
use super::linuxseq::fold::{self, Bus, Did};
use super::linuxseq::rules::Rule;
use super::linuxseq::{SECTIONS, What, parse};
use super::messcheck::Facts;
use super::regs14::Regs14;
use super::session::Session;
use super::stage::Stage;
use crate::gpucore::psp::{Bound, Cond};
use crate::gpucore::trail::Access;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Linux's names of the card's registers, from the kernel's sections, made once per run (a lookup per line would
/// parse every section again).
pub struct Names(BTreeMap<u32, &'static str>);

impl Names {
    pub fn new() -> Names {
        let mut m = BTreeMap::new();
        for s in SECTIONS.iter().filter_map(|(n, t)| parse(n, t).ok()) {
            for l in &s.lines {
                match l.what {
                    What::Write { name, dword, .. } | What::Read { name, dword, .. } | What::Wait { name, dword, .. } => {
                        m.entry(dword).or_insert(name);
                    }
                    What::Hdp { mem_name, mem, .. } => {
                        m.entry(mem).or_insert(mem_name);
                    }
                    _ => {}
                }
            }
        }
        Names(m)
    }

    fn of(&self, regs: &Regs14, dword: u32) -> &'static str {
        self.0.get(&dword).copied().or_else(|| regs.reg_at(dword * 4).map(|r| r.name)).unwrap_or("?")
    }
}

impl Default for Names {
    fn default() -> Self {
        Names::new()
    }
}

fn v(x: Option<u32>) -> String {
    x.map_or(String::from("-"), |x| format!("{x:#010x}"))
}

fn cond(c: Cond) -> String {
    match c {
        Cond::Masked { mask, value } => format!("mask={mask:#010x} is={value:#010x}"),
        Cond::Changed { from } => format!("changed from={from:#010x}"),
    }
}

fn bound(b: Bound) -> String {
    match b {
        Bound::Reads(n) => format!("max={n}"),
        Bound::Time { us, step_us } => format!("max=time:{us}us/{step_us}us"),
    }
}

/// A stage's trail in the fixtures' line forms (`R`, `W`, `WAIT`, `HDP`, `SLEEP`; equal reads in a row as one
/// `R ... xN`), folded as Linux's trace shows the same bus accesses (`linuxseq::fold`). Each wait keeps its time.
pub fn trail(trail: &[Access], regs: &Regs14, names: &Names) -> Result<Vec<String>, String> {
    let did = fold::fold(trail, &Bus::of(regs))?;
    let mut us = trail.iter().filter_map(|a| if let Access::Wait { us, .. } = a { Some(*us) } else { None });
    let mut out: Vec<String> = Vec::new();
    let mut repeat: Option<(u32, Option<u32>, u32)> = None;
    let flush = |out: &mut Vec<String>, r: &mut Option<(u32, Option<u32>, u32)>| {
        if let Some((dword, value, n)) = r.take() {
            let times = if n > 1 { format!(" x{n}") } else { String::new() };
            out.push(format!("R {} {dword:#x} {}{times}", names.of(regs, dword), v(value)));
        }
    };
    for d in did {
        if let Did::Read { dword, value } = d {
            match &mut repeat {
                Some((dw, val, n)) if *dw == dword && *val == value => *n += 1,
                _ => {
                    flush(&mut out, &mut repeat);
                    repeat = Some((dword, value, 1));
                }
            }
            continue;
        }
        flush(&mut out, &mut repeat);
        out.push(match d {
            Did::Write { dword, value, reached } => format!("W {} {dword:#x} {value:#010x}{}", names.of(regs, dword), if reached { "" } else { " refused" }),
            Did::Wait { dword, cond: c, bound: b, reads, last } => {
                format!("WAIT {} {dword:#x} {} {} last={} reads={reads} us={}", names.of(regs, dword), cond(c), bound(b), v(last), us.next().unwrap_or(0))
            }
            Did::Hdp { flush, mem, value } => format!("HDP {flush:#x} {} {mem:#x} {}", names.of(regs, mem), v(value)),
            Did::Sleep { us } => format!("SLEEP us={us}"),
            Did::Read { .. } => continue,
        });
    }
    flush(&mut out, &mut repeat);
    Ok(out)
}

/// The value the card answered at its first read of `byte` in a trail (a look's answer, read as the stage saw it).
pub fn first_read(trail: &[Access], byte: u32) -> Option<Option<u32>> {
    trail.iter().find_map(|a| match *a {
        Access::Read { byte: b, value } if b == byte => Some(value),
        Access::Wait { byte: b, last, .. } if b == byte => Some(last),
        _ => None,
    })
}

/// A section's declared differences as the evaluator needs them: the kind, the registers it speaks for, and
/// what decides its lines (`chainseq::rules`, with what this boot chose).
pub fn rules(section: &str, s: &Session, vram_bytes: u64) -> Vec<String> {
    let told = s.nb.gart.as_ref().map(super::gcstage::Told::of);
    let chosen = Chosen { l: s.layout.as_ref(), taken: s.bump.as_ref(), told: told.as_ref(), irq: &[], mes: s.mes.kiq, sdma: s.sdma.bound };
    let rules = match chainseq::rules(section, vram_bytes, chosen) {
        Ok(r) => r,
        Err(why) => return alloc::vec![format!("REGELN {section} none why={why}")],
    };
    if rules.is_empty() {
        return alloc::vec![format!("REGELN {section} none")];
    }
    rules
        .iter()
        .map(|r| {
            let names: Vec<&str> = r.names().collect();
            let names = names.join(",");
            match r {
                Rule::Address(a) => format!("REGELN {section} address {names} shift={} keep={:#x}", a.shift, a.keep),
                Rule::Argument(a) => format!("REGELN {section} argument {names} hi={:#x} lo={:#x} at={:#x}", a.hi, a.lo, a.at),
                Rule::HdpBefore { .. } => format!("REGELN {section} hdp-before {names}"),
                Rule::BindBefore { .. } => format!("REGELN {section} bind-before {names}"),
                Rule::BindBothBefore { .. } => format!("REGELN {section} bind-both-before {names}"),
                Rule::BindMesBefore { .. } => format!("REGELN {section} bind-mes-before {names}"),
                Rule::Untraced { byte, .. } => format!("REGELN {section} untraced {names} byte={byte:#x}"),
                Rule::PairPoll(p) => format!("REGELN {section} pair-poll {names} max={} first_is={:#x}:{:#x} second_is={:#x}:{:#x}", p.max, p.first_is.0, p.first_is.1, p.second_is.0, p.second_is.1),
            }
        })
        .collect()
}

/// The typed `linuxseq` line of a stage call.
pub fn verdict(stage: Stage, v: Option<&Verdict>) -> String {
    match v {
        Some(v) => format!("VERGLEICH {} section={} said={} answers={}", stage.name(), v.section, v.said.word(), v.answers),
        None => format!("VERGLEICH {} section=- said=none answers=0", stage.name()),
    }
}

/// What the IH reader counted since `before` (both of this boot), as one line.
pub fn ih(stage: &str, before: Option<&Reader>, now: Option<&Reader>) -> String {
    let Some(now) = now else { return format!("IH {stage} off") };
    let zero = Reader::default();
    let b = before.unwrap_or(&zero);
    let d = |a: u64, x: u64| a.saturating_sub(x);
    let mut out = format!(
        "IH {stage} ring0=+{} ring1=+{} self=+{} overflows=+{} failed=+{} faults_gfxhub=+{} faults_mmhub=+{} faults_unread=+{}",
        d(now.entries[0], b.entries[0]),
        d(now.entries[1], b.entries[1]),
        d(now.self_irqs, b.self_irqs),
        d(now.overflows, b.overflows),
        d(now.bad_reads, b.bad_reads),
        d(now.faults.count[0], b.faults.count[0]),
        d(now.faults.count[1], b.faults.count[1]),
        d(now.faults.unread, b.faults.unread),
    );
    let unclaimed: Vec<String> = now.unclaimed.iter().filter_map(|(k, n)| Some((k, d(*n, b.unclaimed.get(k).copied().unwrap_or(0)))).filter(|(_, x)| *x > 0)).map(|((c, s), x)| format!("{c}:{s}:+{x}")).collect();
    let was: BTreeMap<(u8, u8, u8), u64> = b.taken_all().map(|(_, k, n)| (k, n)).collect();
    let taken: Vec<String> = now.taken_all().filter_map(|(who, k, n)| Some((who, k, d(n, was.get(&k).copied().unwrap_or(0)))).filter(|(_, _, x)| *x > 0)).map(|(who, (c, s, r), x)| format!("{who}:{c}:{s}:{r}:+{x}")).collect();
    let was: BTreeMap<(u8, u8, u8), u64> = b.refused_all().collect();
    let refused: Vec<String> = now.refused_all().filter_map(|(k, n)| Some((k, d(n, was.get(&k).copied().unwrap_or(0)))).filter(|(_, x)| *x > 0)).map(|((c, s, r), x)| format!("{c}:{s}:{r}:+{x}")).collect();
    let list = |v: Vec<String>| if v.is_empty() { String::from("-") } else { v.join(",") };
    out.push_str(&format!(" unclaimed={} taken={} refused={}", list(unclaimed), list(taken), list(refused)));
    out
}

/// Every line of `text` under `key` and the stage's name.
pub fn lines(key: &str, who: &str, text: &str) -> String {
    let mut out = String::new();
    for l in text.lines() {
        out.push_str(&format!("{key} {who} {l}\n"));
    }
    out
}

/// The protocol's head: what the run is, the rules it follows, the facts of the machine.
pub fn head(f: &Facts, build: &str, levels: &str) -> String {
    let order: Vec<&str> = super::messung::RUN.iter().map(|s| s.name()).collect();
    let mut h = String::new();
    h.push_str("## BitsOS: Messprotokoll der Grafikkarte RX 9070 XT (gpu-messung)\n");
    h.push_str(&format!("KOPF format={}\nKOPF kernel={build}\nKOPF karte={} {:04x}:{:04x} rev {:#04x}\n", super::messung::FORMAT, f.key, f.vendor, f.device, f.revision));
    h.push_str(&format!("KOPF loglevel={levels}\nKOPF stufen={}\nKOPF grenze_s={}\nKOPF aus_nach_s={}\n", order.join(","), super::messung::LIMIT_US / 1_000_000, super::messung::OFF_IN_S));
    h.push_str(&format!("KOPF stapel_budget={}\n", super::fakemodel::STAGE_STACK));
    h.push_str(&format!("KOPF ausfuehrung={}\n", f.rev_id.map_or(String::from("ungelesen"), |r| format!("{r}"))));
    h.push_str("## Was die Messung tut\n");
    for l in RULES {
        h.push_str("REGEL ");
        h.push_str(l);
        h.push('\n');
    }
    h
}

/// The head's rules, in plain German.
pub const RULES: [&str; 13] = [
    "Die Stufen laufen der Reihe nach, jede auf demselben Weg wie gx_bringup; jede Stufe steht ganz in dieser Datei, die nach jeder Stufe auf die Platte geschrieben wird.",
    "Stopp-Regel: hält eine Stufe an, wird sie nicht gestartet, wollte sie etwas schreiben, das ihre Liste nicht erlaubt, sagt ihre linuxseq-Zeile, dass sie anders zugreift als Linux (ein Zugriff zu viel oder zu wenig, eine andere Reihenfolge, ein anderer geschriebener Wert, eine andere Wartebedingung) oder dass sie nicht verglichen werden konnte, oder ist ihre Mitschrift unvollständig, dann endet die Messung dort: keine spätere Stufe berührt die Karte.",
    "Antwortet die Karte auf ein Lesen anders als in Linux' Mitschrift, wird das mit der Antwort festgehalten, und die Messung geht nur weiter, wenn die Stufe selbst bestanden hat.",
    "Lässt sich diese Datei nicht weiterschreiben, endet die Messung dort: keine weitere Stufe berührt die Karte, denn was sie täte, stünde nirgends.",
    "Die Messung ist kurz und belastet die Karte nicht (nur die Prüfsprossen der Kopiereinheit), weil die Temperaturwarnung der Karte noch keinen Empfänger hat (Welle 44).",
    "Die Zeitgrenze wird vor jeder Stufe geprüft; eine laufende Stufe wird nicht unterbrochen, sie wartet höchstens so lange, wie Linux wartet.",
    "Zwischen zwei Stufen liest der Blick state nur. Der Blick smu läuft nicht: er schickt Fragen, die Linux beim Start nicht schickt.",
    "nbif und ih ändern vor ihrer Stufe den PCI-Konfigurationsraum der Karte selbst (Device Control 2: AtomicOps; Befehlsregister; MSI), außerhalb der Schreibliste der Stufe; das bleibt bis zum Ausschalten.",
    "Nach der letzten Zeile schaltet sich der PC selbst aus; dabei halten die Abschalt-Haken des Treibers die Kopiereinheiten an, schalten die Speicherverwaltung des Grafikteils und die Meldungen der Karte ab (sie schreiben auf die Karte, nach dieser Datei, auch nach einem Stopp; ihre Zugriffe stehen in keiner Zeile).",
    "Eine Temperatur liest die Messung nicht (dafür bräuchte es eine Nachricht an die Energieverwaltung, die die Kette nicht schickt); die Lüfter hört Georg.",
    "GPU nachher zeigt nur, was neu gelesen wird; Leitung, Link und SCPM sind die Angaben vom Anschluss beim Start (GPU vorher) und werden nicht neu gelesen.",
    "Vor jeder Stufe und vor jedem Blick steht seine START-Zeile auf der Platte: hängt etwas, ist die letzte START-Zeile ohne Abschluss die Stelle; dann hilft nur der Einschaltknopf.",
    "Wo innerhalb einer Stufe sie hängt, steht nicht in der Datei: ihre Zugriffe werden geschrieben, wenn die Stufe zurückkehrt (ein Schreiben auf die Platte mitten in der Stufe hielte die Karte fest, während der Datenträger arbeitet, und schöbe Wartezeiten zwischen Linux' Zugriffe).",
];

/// The snapshot after the run: only its lines that are read anew (what the stages left, the attach's write
/// count, the firmware files), the attach's facts named as not read again.
pub fn after(snap: &str) -> String {
    let mut out = String::from("GPU nachher nicht neu gelesen: Anschluss, Leitung, Link und SCPM (die Angaben vom Anschluss beim Start, oben unter GPU vorher)\n");
    for l in snap.lines().filter(|l| ["stages: ", "writes: ", "dma: ", "firmware "].iter().any(|k| l.starts_with(k))) {
        out.push_str(&format!("GPU nachher {l}\n"));
    }
    out
}
