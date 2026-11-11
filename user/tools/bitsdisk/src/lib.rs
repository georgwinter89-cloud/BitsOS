//! `bitsdisk`'s pure part (host-tested): the kernel's `list` answer (crates/drivers/nvme setup.rs) and the words
//! Georg reads. Everything that decides lives in the kernel; this tool only shows and asks.

#![forbid(unsafe_code)]

use std::io;

/// One disk of the list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Disk {
    pub name: String,
    pub model: String,
    pub serial: String,
    pub blocks: u64,
    pub block_size: u32,
    pub parts: Vec<Part>,
}

/// One partition of a disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    pub number: u32,
    pub first: u64,
    pub blocks: u64,
    pub kind: String,
    pub guid: String,
    /// What is on it: `-` nothing the kernel recognises.
    pub signature: String,
    /// `free`, `this_pool`, or the refusal in words.
    pub verdict: String,
}

impl Part {
    pub fn pickable(&self) -> bool {
        self.verdict == "free" || self.verdict == "this_pool"
    }
}

/// The parsed `list` answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// CONFIG `nvme_write=on` at this boot.
    pub write_on: bool,
    /// The stick's `nvme_data=` value, if it has one.
    pub key: Option<String>,
    pub disks: Vec<Disk>,
}

/// None: not an `ok` answer of the expected form.
pub fn parse_list(answer: &str) -> Option<Listing> {
    let mut lines = answer.lines();
    if lines.next()? != "ok" {
        return None;
    }
    let mut l = Listing::default();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "nvme_write" => l.write_on = *f.get(1)? == "on",
            "key" => l.key = f.get(1).filter(|v| **v != "-").map(|v| v.to_string()),
            "disk" => l.disks.push(Disk {
                name: f.get(1)?.to_string(),
                model: f.get(2)?.to_string(),
                serial: f.get(3)?.to_string(),
                blocks: f.get(4)?.parse().ok()?,
                block_size: f.get(5)?.parse().ok()?,
                parts: Vec::new(),
            }),
            "part" => l.disks.last_mut()?.parts.push(Part {
                number: f.get(1)?.parse().ok()?,
                first: f.get(2)?.parse().ok()?,
                blocks: f.get(3)?.parse().ok()?,
                kind: f.get(4)?.to_string(),
                guid: f.get(5)?.to_string(),
                signature: f.get(6)?.to_string(),
                verdict: f.get(7)?.to_string(),
            }),
            _ => return None,
        }
    }
    Some(l)
}

/// `n` bytes for a person.
pub fn size_text(n: u64) -> String {
    const G: u64 = 1 << 30;
    if n >= 1 << 40 {
        format!("{:.2} TiB", n as f64 / (1u64 << 40) as f64)
    } else if n >= G {
        format!("{:.1} GiB", n as f64 / G as f64)
    } else {
        format!("{} MiB", n >> 20)
    }
}

/// The list as Georg reads it, with a running number per pickable partition; the same order as `picks`.
pub fn render(l: &Listing) -> String {
    let mut out = String::new();
    let mut n = 0;
    for d in &l.disks {
        out.push_str(&format!("{}  {}  Seriennummer {}  {}\n", d.name, d.model, d.serial, size_text(d.blocks * d.block_size as u64)));
        if d.parts.is_empty() {
            out.push_str("    keine Partitionen, oder keine GPT\n");
        }
        for p in &d.parts {
            let mark = if p.pickable() {
                n += 1;
                format!("[{n}]")
            } else {
                String::from(" - ")
            };
            let on = if p.signature == "-" { String::from("leer") } else { p.signature.clone() };
            out.push_str(&format!("  {mark} Partition {}  {}  {}  {on}\n", p.number, size_text(p.blocks * d.block_size as u64), p.kind));
            if !p.pickable() {
                out.push_str(&format!("        nicht waehlbar: {}\n", p.verdict));
            } else if p.verdict == "this_pool" {
                out.push_str("        das ist schon der BitsOS-Bereich\n");
            }
        }
    }
    out
}

/// The pickable partitions in the list's order: (disk, partition).
pub fn picks(l: &Listing) -> Vec<(&Disk, &Part)> {
    l.disks.iter().flat_map(|d| d.parts.iter().filter(|p| p.pickable()).map(move |p| (d, p))).collect()
}

/// A kernel answer in Georg's words.
pub fn refusal_text(word: &str, text: &str) -> String {
    match word {
        "no_nvme_write" => String::from(
            "In CONFIG.TXT fehlt nvme_write=on. Das traegst du selbst ein (am Stick auf dem anderen Rechner: die Zeile nvme_write=on in \\BITSOS2\\CONFIG.TXT), \
             startest neu und rufst bitsdisk wieder auf.",
        ),
        "window_busy" => format!("Gerade ist schon eine Partition in Benutzung, nichts wurde geschrieben: {text}"),
        "unclear" => format!("Der BitsOS-Bereich auf dieser Partition laesst sich nicht beurteilen, also wird er nicht geloescht. Es wurde nichts geschrieben: {text}"),
        "changed" => String::from("Die Partition ist nicht mehr so wie bei der Frage. Es wurde nichts geschrieben, bitte noch einmal."),
        "wrong_code" => String::from("Die Zeichen stimmen nicht. Es wurde nichts geschrieben."),
        "injected" => String::from("Die Tasten kamen nicht von der Tastatur an diesem Rechner. Es wurde nichts geschrieben."),
        "no_enter" | "too_old" | "stale" => String::from("Die Eingabe wurde nicht rechtzeitig erkannt. Es wurde nichts geschrieben, bitte noch einmal."),
        _ => format!("Abgelehnt ({word}): {text}"),
    }
}

/// The kernel's answer to `prepare`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prepared {
    /// Nothing to do; the kernel's words.
    Already(String),
    /// The code to type; `remake`: a half-made pool on the partition would be erased.
    Type { code: String, remake: bool },
    Refused { word: String, text: String },
}

pub fn parse_prepare(answer: &str) -> Prepared {
    let a = answer.trim();
    if let Some(t) = a.strip_prefix("ok already") {
        return Prepared::Already(t.trim().to_string());
    }
    if let Some(code) = a.strip_prefix("ok type ") {
        return Prepared::Type { code: code.to_string(), remake: false };
    }
    if let Some(code) = a.strip_prefix("ok remake ") {
        return Prepared::Type { code: code.to_string(), remake: true };
    }
    parse_refused(a)
}

/// A `refused <word> <text>` answer, or whatever else stands there.
fn parse_refused(a: &str) -> Prepared {
    let mut w = a.trim().splitn(3, ' ');
    let _ = w.next();
    Prepared::Refused { word: w.next().unwrap_or("").to_string(), text: w.next().unwrap_or("").to_string() }
}

/// What the dialog needs of the machine (the tests give it a script).
pub trait Dialog {
    /// One command to the kernel, its answer.
    fn ask(&mut self, cmd: &str) -> io::Result<String>;
    /// One line typed; None: the input ended.
    fn line(&mut self) -> Option<String>;
    fn say(&mut self, text: &str);
}

fn refused(d: &mut impl Dialog, word: &str, text: &str) -> bool {
    d.say(&format!("{}\n", refusal_text(word, text)));
    false
}

/// The dialog: pick, type, done. True: the partition is set up (now or before). Nothing reaches the kernel but what is
/// asked for here, and `confirm` only after a line was typed.
pub fn setup(d: &mut impl Dialog, l: &Listing) -> io::Result<bool> {
    d.say(&render(l));
    if !l.write_on {
        d.say(&format!("\n{}\n", refusal_text("no_nvme_write", "")));
        return Ok(false);
    }
    let choices = picks(l);
    if choices.is_empty() {
        d.say("\nKeine Partition ist waehlbar. Es wurde nichts geschrieben.\n");
        return Ok(false);
    }
    d.say("\nWelche Nummer soll BitsOS bekommen? (leer: abbrechen) ");
    let Some(pick) = d.line().filter(|s| !s.is_empty()) else {
        d.say("Abgebrochen.\n");
        return Ok(false);
    };
    let Some((disk, p)) = pick.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| choices.get(n)) else {
        d.say("Diese Nummer gibt es nicht. Es wurde nichts geschrieben.\n");
        return Ok(false);
    };
    let (code, remake) = match parse_prepare(&d.ask(&format!("prepare {} {}\n", disk.name, p.number))?) {
        Prepared::Already(t) => {
            d.say(&format!("Schon eingerichtet: {t}\n"));
            return Ok(true);
        }
        Prepared::Refused { word, text } => return Ok(refused(d, &word, &text)),
        Prepared::Type { code, remake } => (code, remake),
    };
    d.say("\nDas wird geschrieben, nur innerhalb dieser Partition:\n");
    d.say(&format!("  {} Partition {} ({}), Seriennummer {}\n", disk.name, p.number, size_text(p.blocks * disk.block_size as u64), disk.serial));
    if remake {
        d.say("  Darauf liegt ein angefangener BitsOS-Bereich ohne Daten. Er wird GELOESCHT und neu gemacht.\n");
    }
    d.say("  Alles darauf geht verloren. Alles andere auf der Platte bleibt unberuehrt.\n");
    d.say(&format!("Zum Bestaetigen tippe auf der Tastatur an diesem Rechner: {code} und dann Enter.\n> "));
    if d.line().is_none() {
        d.ask("cancel\n")?;
        d.say("Abgebrochen.\n");
        return Ok(false);
    }
    d.say("Einen Moment ...\n");
    let a = d.ask("confirm\n")?;
    if a.trim() != "ok done" {
        let Prepared::Refused { word, text } = parse_refused(&a) else { return Ok(false) };
        return Ok(refused(d, &word, &text));
    }
    d.say("Fertig. CONFIG.TXT hat jetzt die Zeile nvme_data=, die Partition ist vorbereitet.\n");
    d.say("Starte den Rechner neu (ganz, nicht nur den Kern): dann liegt /data auf dieser Partition.\n");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "ok\nnvme_write\ton\nkey\t-\n\
        disk\tnvme0n1\tLexar SSD NM790 2TB\tLX123456\t3907029168\t512\ttrue\n\
        part\t1\t34\t32768\tE3C9E316-0B5C-4DB8-817D-F92DF00215AE\tGUID1\t-\ttype: the partition's type\n\
        part\t2\t32802\t3000000000\tMicrosoft basic data\tGUID2\tNTFS boot sector\tsignature: the partition holds NTFS boot sector\n\
        part\t3\t3000032802\t900000000\tMicrosoft basic data\tGUID3\t-\tfree\n\
        disk\tnvme1n1\tOther\tOT1\t1000\t512\tfalse\n";

    #[test]
    fn the_list() {
        let l = parse_list(LIST).unwrap();
        assert!(l.write_on);
        assert_eq!(l.key, None);
        assert_eq!(l.disks.len(), 2);
        assert_eq!(l.disks[0].parts.len(), 3);
        assert_eq!(l.disks[0].serial, "LX123456");
        let p = picks(&l);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].0.name.as_str(), p[0].1.number), ("nvme0n1", 3));
        let t = render(&l);
        assert!(t.contains("[1] Partition 3") && t.contains("nicht waehlbar") && t.contains("Seriennummer LX123456"), "{t}");
        assert!(!t.contains("[2]"), "{t}");
    }

    #[test]
    fn bad_answers() {
        assert!(parse_list("refused x y\n").is_none());
        assert!(parse_list("ok\npart\t1\t2\t3\tx\ty\t-\tfree\n").is_none(), "a partition without a disk");
        assert!(parse_list("ok\nother\n").is_none());
    }

    /// A script: the lines typed and the kernel's answers, in order; what was asked and said is kept.
    #[derive(Default)]
    struct Script {
        lines: Vec<&'static str>,
        answers: Vec<&'static str>,
        asked: Vec<String>,
        said: String,
    }

    impl Dialog for Script {
        fn ask(&mut self, cmd: &str) -> io::Result<String> {
            self.asked.push(cmd.trim().to_string());
            Ok(if self.answers.is_empty() { String::from("ok\n") } else { self.answers.remove(0).to_string() })
        }
        fn line(&mut self) -> Option<String> {
            (!self.lines.is_empty()).then(|| self.lines.remove(0).to_string())
        }
        fn say(&mut self, text: &str) {
            self.said.push_str(text);
        }
    }

    fn run(lines: Vec<&'static str>, answers: Vec<&'static str>, list: &str) -> (bool, Script) {
        let mut s = Script { lines, answers, ..Script::default() };
        let r = setup(&mut s, &parse_list(list).unwrap()).unwrap();
        (r, s)
    }

    #[test]
    fn no_nvme_write_says_so_and_starts_nothing() {
        let off = LIST.replace("nvme_write\ton", "nvme_write\toff");
        let mut s = Script { lines: vec!["1", "2345AB"], ..Script::default() };
        assert!(!setup(&mut s, &parse_list(&off).unwrap()).unwrap());
        assert!(s.asked.is_empty(), "nothing reached the kernel: {:?}", s.asked);
        assert_eq!(s.lines.len(), 2, "and nothing was asked of Georg");
        assert!(s.said.contains("In CONFIG.TXT fehlt nvme_write=on"), "{}", s.said);
    }

    #[test]
    fn a_wrong_code_is_the_kernels_no_and_nothing_more() {
        for code in ["WRONG1", "2345A", ""] {
            let (r, s) = run(vec!["1", code], vec!["ok type 2345AB\n", "refused wrong_code the typed code does not count: nothing was written\n"], LIST);
            assert!(!r);
            assert_eq!(s.asked, ["prepare nvme0n1 3", "confirm"]);
            assert!(s.said.contains("Die Zeichen stimmen nicht") && !s.said.contains("Fertig"), "{}", s.said);
        }
    }

    #[test]
    fn the_good_way_and_its_exits() {
        let (r, s) = run(vec!["1", "3456AB"], vec!["ok type 3456AB\n", "ok done\n"], LIST);
        assert!(r);
        assert!(s.said.contains("tippe auf der Tastatur an diesem Rechner: 3456AB") && s.said.contains("Fertig") && !s.said.contains("GELOESCHT"), "{}", s.said);
        // Input over at the code: cancelled, no confirm.
        let (r, s) = run(vec!["1"], vec!["ok type 3456AB\n"], LIST);
        assert!(!r);
        assert_eq!(s.asked, ["prepare nvme0n1 3", "cancel"]);
        // No pick, a pick that is not offered.
        for pick in [vec![], vec![""], vec!["2"], vec!["0"], vec!["x"]] {
            let (r, s) = run(pick, vec![], LIST);
            assert!(!r);
            assert!(s.asked.is_empty(), "{:?}", s.asked);
        }
        // A half-made pool is called what it is.
        let (r, s) = run(vec!["1", "3456AB"], vec!["ok remake 3456AB\n", "ok done\n"], LIST);
        assert!(r);
        assert!(s.said.contains("GELOESCHT"), "{}", s.said);
        // The kernel refuses at `prepare`: no code is asked for.
        let (r, s) = run(vec!["1", "3456AB"], vec!["refused window_busy /data lies on nvme0n1p3\n"], LIST);
        assert!(!r);
        assert_eq!(s.asked, ["prepare nvme0n1 3"]);
        assert_eq!(s.lines.len(), 1);
        assert!(s.said.contains("/data lies on nvme0n1p3"), "{}", s.said);
        let (r, s) = run(vec!["1"], vec!["ok already the pool is there\n"], LIST);
        assert!(r);
        assert!(s.said.contains("Schon eingerichtet: the pool is there"), "{}", s.said);
    }

    #[test]
    fn prepare_answers() {
        assert_eq!(parse_prepare("ok type AB\n"), Prepared::Type { code: "AB".into(), remake: false });
        assert_eq!(parse_prepare("ok remake AB\n"), Prepared::Type { code: "AB".into(), remake: true });
        assert_eq!(parse_prepare("refused unclear why not\n"), Prepared::Refused { word: "unclear".into(), text: "why not".into() });
        assert!(matches!(parse_prepare("???"), Prepared::Refused { .. }));
    }

    #[test]
    fn sizes() {
        assert_eq!(size_text(1 << 41), "2.00 TiB");
        assert_eq!(size_text(5 << 30), "5.0 GiB");
        assert_eq!(size_text(16 << 20), "16 MiB");
    }
}
