extern crate std;

use crate::file::{messung_name_of, messung_next, messung_number, name_of, number, plan, KEEP};
use crate::page::{layout, must_keep, wrap, MAX_PAGES};
use crate::rows::{self, Clock, Cpus, FileState, McpDown, Remote, Watchdog};
use crate::{Group, Report};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

/// The console's grid on 2560x1440 (VGA font at scale 2: 16 x 32 pixels per cell).
const COLS: u32 = 160;
const ROWS: u32 = 45;

/// A report shaped like the Gaming-PC's first round (row texts from the wave notes).
fn gaming_pc() -> Report {
    let mut r = Report::new(String::from("BitsOS-Bericht für den Gaming-PC, Version 1, 47 Sekunden nach dem Start"));
    r.extend(Group::Start, ["Sicherer Modus für den Gaming-PC (am Mainboard erkannt).", "Netzwerk über Kabel: an. WLAN bleibt aus.", "Start-Stick: darf beschrieben werden (neue Versionen aus der Ferne).", "Andere USB-Datenträger: werden nur gelesen.", "Eingebaute Platten: kein Treiber, nicht gelesen."]);
    r.extend(Group::Cpu, rows::cpus(Cpus { up: 20, planned: 20, p_cores: 8, e_cores: 12, hwp_on: 20 }));
    r.extend(Group::Cpu, rows::clock(Clock { hz: 3_878_400_000, cpuid15_hz: 3_878_400_000, ppm: Some(-12), cpuid15_checked: true, hpet_left_out: true }));
    r.push(Group::Cpu, rows::madt(20, 28, 0));
    r.push(Group::Cpu, rows::msr_faults(0, &[]));
    r.push(Group::Cpu, rows::power(Some(5), true));
    r.push(Group::Guard, rows::dma("free"));
    r.push(Group::Devices, "Anschluss-Stränge: 2 (Bus 00 bis 7f und 80 bis df), 41 Geräte und Abzweige gefunden");
    for i in 0..30 {
        r.push(Group::Devices, format!("  {i:02x}:00.0 Gerät [8086:7d{i:02x}]: erkannt, ohne Treiber"));
    }
    r.push(Group::Devices, "  83:00.0 Netzwerkkarte (Kabel) [10ec:8126]: Treiber rtl8126 hat aufgegeben (map BAR2: OutOfBounds)");
    for d in ["Samsung 990 EVO Plus 2TB", "Samsung 980 PRO 1TB", "Lexar SSD NQ790 2TB"] {
        r.push(Group::Disks, format!("{d}: kein Treiber, nicht gelesen"));
    }
    r.push(Group::Picture, "Bild: 2560 x 1440, liegt im Speicher von Grafik 04:00.0 (Adresse 0xa000000000, oberhalb von 512 GiB)");
    r.push(Group::Picture, rows::picture_speed(Some(true)));
    r.push(Group::Usb, "USB-Baustein 00:0d.0: läuft, 3 Anschlüsse, 0 Geräte");
    r.push(Group::Usb, "USB-Baustein 80:14.0: läuft NICHT (reset (CNR): the controller stayed not ready for 10 s)");
    for n in 1..=8 {
        r.push(Group::Network, format!("Netzwerk {n}/8: Schritt {n} gut"));
    }
    r.push(Group::Network, rows::remote(&Remote::Reachable { addr: String::from("192.168.100.20"), port: 8765 }));
    r.push(Group::Gpu, "Grafikkarte 1 (zeigt das Bild): Lesen angehalten bei Schritt 3 von 5 (Speichergröße lesen): Speichergröße unlesbar, Karte antwortet nicht. Nichts verändert.");
    r.push(Group::Watchdog, rows::watchdog(Watchdog::Halted));
    r.push(Group::Sound, rows::sound(&[String::from("JBL Quantum 800")]));
    r
}

/// Every row of `r` that must be kept, each found whole (its wrapped rows joined) on one page.
fn kept_whole(r: &Report, pages: &[Vec<String>], cols: u32) {
    for row in r.rows.iter().filter(|x| must_keep(&x.text)) {
        let want = wrap(&format!("  {}", row.text), cols as usize);
        let on_one = pages.iter().any(|p| p.windows(want.len()).any(|w| w == want.as_slice()));
        assert!(on_one, "not whole on one page: {}", row.text);
    }
}

#[test]
fn the_gaming_pcs_report_pages_on_the_console_grid() {
    let r = gaming_pc();
    let pages = layout(&r, COLS, ROWS);
    assert!(pages.len() >= 2 && pages.len() <= MAX_PAGES, "{} pages", pages.len());
    for (i, p) in pages.iter().enumerate() {
        assert!(p.len() <= ROWS as usize, "page {} has {} rows", i + 1, p.len());
        assert!(p.iter().all(|l| l.chars().count() <= COLS as usize), "page {} has a row over {COLS} columns", i + 1);
        assert!(p[0].ends_with(&format!("Seite {} von {}", i + 1, pages.len())), "{}", p[0]);
        assert!(p.last().is_some_and(|l| l.contains("20 Sekunden")), "page {} says nothing about turning", i + 1);
    }
    kept_whole(&r, &pages, COLS);
    // Every group's title is on some page, in the page order of the groups.
    let all: Vec<&String> = pages.iter().flatten().collect();
    let mut last = 0;
    for g in Group::ALL {
        let at = all.iter().position(|l| l.as_str() == g.title() || l.starts_with(&format!("{} (weiter)", g.title())));
        let at = at.unwrap_or_else(|| panic!("no title for {g:?}"));
        assert!(at >= last, "{g:?} out of order");
        last = at;
    }
    // Nothing was left out: every row is on a page.
    for row in &r.rows {
        let first = &wrap(&format!("  {}", row.text), COLS as usize)[0];
        assert!(all.iter().any(|l| l == &first), "row missing: {}", row.text);
    }
}

#[test]
fn a_short_report_is_one_page_and_says_how_to_get_the_input() {
    let mut r = Report::new(String::from("Bericht"));
    r.push(Group::Cpu, "Prozessor: 20 Kerne laufen");
    let pages = layout(&r, COLS, ROWS);
    assert_eq!(pages.len(), 1);
    assert!(pages[0][0].ends_with("Seite 1 von 1"));
    assert!(pages[0].last().unwrap().contains("ls /boot"));
}

#[test]
fn a_huge_device_list_is_shortened_but_keeps_every_row_that_gave_up() {
    let mut r = gaming_pc();
    for i in 0..400 {
        r.push(Group::Devices, format!("  {:02x}:{:02x}.0 Gerät [1b36:000d]: läuft mit xhci", 0x90 + i / 32, i % 32));
        if i % 97 == 0 {
            r.push(Group::Devices, format!("  a{i:x}:00.0 Gerät [1b36:000d]: Treiber xhci hat aufgegeben (reset (CNR): stuck)"));
        }
    }
    let pages = layout(&r, COLS, ROWS);
    assert!(pages.len() <= MAX_PAGES, "{} pages", pages.len());
    kept_whole(&r, &pages, COLS);
    assert!(pages.iter().flatten().any(|l| l.contains("weitere Geräte ohne Auffälligkeit")), "the page does not say rows were left out");
    // Rows of other groups are never left out.
    for row in r.rows.iter().filter(|x| x.group != Group::Devices) {
        let first = &wrap(&format!("  {}", row.text), COLS as usize)[0];
        assert!(pages.iter().flatten().any(|l| l == first), "row missing: {}", row.text);
    }
}

#[test]
fn a_small_grid_still_cuts_nothing() {
    let r = gaming_pc();
    for (c, rw) in [(80, 25), (100, 30), (20, 8), (320, 90)] {
        let pages = layout(&r, c, rw);
        kept_whole(&r, &pages, c);
        for p in &pages {
            assert!(p.iter().all(|l| l.chars().count() <= c as usize), "{c}x{rw}");
        }
    }
}

#[test]
fn wrapping_keeps_every_character_and_the_indent() {
    let t = "  Speicherschutz für Geräte: EINGESCHALTET: Tastatur, Stick, Netzwerk und Festplatten bleiben aus. Bitte fotografieren und ausschalten.";
    let w = wrap(t, 40);
    assert!(w.len() > 1);
    assert!(w.iter().all(|l| l.chars().count() <= 40), "{w:?}");
    assert!(w[0].starts_with("  Speicherschutz"));
    assert!(w[1..].iter().all(|l| l.starts_with("      ")), "{w:?}");
    let back: String = w.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
    assert_eq!(back, t.trim());
    // A word longer than a row is split, not dropped.
    let long = "x".repeat(100);
    let w = wrap(&long, 30);
    assert_eq!(w.iter().map(|l| l.trim().len()).sum::<usize>(), 100);
    assert!(w.iter().all(|l| l.chars().count() <= 30));
    assert_eq!(wrap("", 30), vec![String::new()]);
}

#[test]
fn the_words_that_must_never_be_cut() {
    for t in [
        "Treiber xhci hat aufgegeben (map BAR0: OutOfBounds)",
        "USB-Baustein 80:14.0: läuft NICHT (halt (HCH): timeout)",
        "Grafikkarte 1: Lesen angehalten bei Schritt 2 von 5 (Karte bereit?): Karte meldet sich nicht bereit. Nichts verändert.",
        "Netzwerk 5/8: Kein Datenpaket nach 30 Sekunden (gesendet 3, davon fertig 0): Karte angehalten",
        &rows::dma("translation_on"),
        &rows::unguarded(4),
    ] {
        assert!(must_keep(t), "{t}");
    }
    for t in [&rows::watchdog(Watchdog::Halted), "Netzwerk 4/8: Kabelverbindung steht: 1000 Mbit/s", &rows::dma("free")] {
        assert!(!must_keep(t), "{t}");
    }
}

#[test]
fn the_file_text_holds_every_row_with_its_group() {
    let r = gaming_pc();
    let t = r.file_text();
    assert!(t.starts_with("BitsOS-Bericht"));
    assert!(t.ends_with("\r\n") && t.split('\n').filter(|l| !l.is_empty()).all(|l| l.ends_with('\r')), "every line ends in CR LF");
    for g in Group::ALL {
        assert!(t.contains(g.title()), "{g:?}");
    }
    for row in &r.rows {
        assert!(t.contains(&row.text), "{}", row.text);
    }
    let lines = r.lines();
    assert_eq!(lines.len(), t.lines().count());
}

#[test]
fn pushing_a_text_with_line_breaks_gives_one_row_each() {
    let mut r = Report::new(String::new());
    r.push(Group::Usb, "a\nb\n\n c \n");
    assert_eq!(r.group(Group::Usb).collect::<Vec<_>>(), ["a", "b", " c"]);
}

#[test]
fn report_files_keep_the_newest_three() {
    assert_eq!(number("GP000042.TXT"), Some(42));
    assert_eq!(number("gp000042.txt"), Some(42));
    for bad in ["GP42.TXT", "GP0000042.TXT", "XX000042.TXT", "GP00004a.TXT", "GP000042.LOG", "CONFIG.TXT"] {
        assert_eq!(number(bad), None, "{bad}");
    }
    assert_eq!(plan(&[]), (String::from("GP000001.TXT"), vec![]));
    assert_eq!(plan(&["GP000001.TXT"]), (String::from("GP000002.TXT"), vec![]));
    assert_eq!(plan(&["GP000002.TXT", "GP000001.TXT"]), (String::from("GP000003.TXT"), vec![]));
    assert_eq!(plan(&["GP000002.TXT", "GP000001.TXT", "GP000003.TXT"]), (String::from("GP000004.TXT"), vec![String::from("GP000001.TXT")]));
    let (n, gone) = plan(&["GP000007.TXT", "NOTES.TXT", "gp000005.txt", "GP000006.TXT", "GP000003.TXT"]);
    assert_eq!(n, "GP000008.TXT");
    assert_eq!(gone, ["GP000003.TXT", "gp000005.txt"]);
    // After the write KEEP remain, and a name that is not a report is never removed.
    let left = 4 - gone.len() + 1;
    assert_eq!(left, KEEP);
    assert_eq!(name_of(1_000_000), "GP000000.TXT");
    assert_eq!(plan(&["GP999999.TXT"]).0, "GP000001.TXT");
}

#[test]
fn every_measurement_run_gets_a_name_no_file_has_and_the_reports_never_touch_them() {
    assert_eq!(messung_number("GPUM0001.TXT"), Some(1));
    assert_eq!(messung_number("gpum0042.txt"), Some(42));
    for bad in ["GPUM001.TXT", "GPUM00001.TXT", "GPUM0000.TXT", "GPUMx001.TXT", "GP000001.TXT", "GPUM0001.LOG"] {
        assert_eq!(messung_number(bad), None, "{bad}");
    }
    assert_eq!(messung_next(&[]).as_deref(), Some("GPUM0001.TXT"));
    assert_eq!(messung_next(&["GP000003.TXT", "GPUM0001.TXT", "GPUM0007.TXT", "CONFIG.TXT"]).as_deref(), Some("GPUM0008.TXT"));
    // Past the last number the lowest free one: never a name that is there.
    assert_eq!(messung_next(&["GPUM9999.TXT", "GPUM0001.TXT", "GPUM0002.TXT"]).as_deref(), Some("GPUM0003.TXT"));
    let all: Vec<String> = (1..=9999).map(messung_name_of).collect();
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    assert_eq!(messung_next(&refs), None);
    // The report's own plan neither counts nor removes a protocol.
    assert_eq!(number("GPUM0001.TXT"), None);
    let (_, gone) = plan(&["GP000001.TXT", "GP000002.TXT", "GP000003.TXT", "GPUM0001.TXT", "GPUM0002.TXT"]);
    assert!(gone.iter().all(|g| g.starts_with("GP0")), "{gone:?}");
}

#[test]
fn the_start_question_starts_on_j_alone_and_only_once_armed() {
    use crate::ask::{KEY_JA, Start, start_key};
    assert_eq!(start_key(KEY_JA, true), Start::Ja);
    for other in [30u16, 28, 1, crate::ask::KEY_ZURUECK, 57] {
        assert_eq!(start_key(other, true), Start::Nein, "{other}");
    }
    for any in [KEY_JA, 28, 30] {
        assert_eq!(start_key(any, false), Start::Early, "{any}");
    }
}

#[test]
fn numbers_in_german() {
    assert_eq!(rows::mhz(3_878_400_000), "3 878,4 MHz");
    assert_eq!(rows::mhz(999_960_000), "1 000,0 MHz");
    assert_eq!(rows::mhz(0), "0,0 MHz");
    assert_eq!(rows::thousands(1_234_567), "1 234 567");
    assert_eq!(rows::thousands(12), "12");
}

#[test]
fn the_cpu_rows_say_good_and_bad_plainly() {
    let good = rows::cpus(Cpus { up: 20, planned: 20, p_cores: 8, e_cores: 12, hwp_on: 20 });
    assert_eq!(good, ["Prozessor: 20 Kerne laufen (8 schnelle, 12 sparsame)", "Hochtakten: eingeschaltet auf 20 Kernen"]);
    let bad = rows::cpus(Cpus { up: 12, planned: 20, p_cores: 8, e_cores: 4, hwp_on: 10 });
    assert_eq!(bad[0], "Prozessor: Nur 12 von 20 Kernen laufen (8 schnelle, 4 sparsame)");
    assert!(bad[1].starts_with("Hochtakten: auf 2 Kernen nicht eingeschaltet"));
    let c = rows::clock(Clock { hz: 3_878_400_000, cpuid15_hz: 3_878_400_000, ppm: Some(-31), cpuid15_checked: true, hpet_left_out: true });
    assert_eq!(c[0], "Uhr: 3 878,4 MHz, vom Prozessor genannt und nachgemessen (Abweichung 31 Millionstel)");
    assert!(c[1].starts_with("Zweite Uhr im Chipsatz"));
    let m = rows::clock(Clock { hz: 3_900_000_000, cpuid15_hz: 3_878_400_000, ppm: Some(5500), cpuid15_checked: false, hpet_left_out: false });
    assert_eq!(m, ["Uhr: gemessen 3 900,0 MHz, der Prozessor nennt 3 878,4 MHz: die Messung gilt"]);
    assert_eq!(rows::msr_faults(3, &[0x48, 0x10a]), "Abgelehnte Prozessor-Abfragen: 3 hat der Prozessor abgelehnt (Nummern: 0x48, 0x10a). BitsOS läuft weiter; bitte fotografieren.");
}

#[test]
fn no_row_carries_a_code_word() {
    // What Georg reads is German; the code words of the kernel's log never appear in the page's own rows.
    let texts: Vec<String> = [
        rows::dma("free"),
        rows::dma("translation_on"),
        rows::dma("unknown"),
        rows::power(None, false),
        rows::watchdog(Watchdog::Running { timeout_s: 30 }),
        rows::remote(&Remote::NoAddress),
        rows::remote(&Remote::NotStarted(McpDown::NoToken)),
        rows::remote(&Remote::NotStarted(McpDown::BadToken)),
        rows::remote(&Remote::NotStarted(McpDown::BadPort)),
        rows::remote(&Remote::NotStarted(McpDown::NetworkNotReady)),
        rows::remote(&Remote::NotStarted(McpDown::Unknown)),
        rows::file(&FileState::Waiting),
        rows::net_silent(true, true),
        rows::sound(&[]),
        rows::picture_speed(Some(false)),
        rows::file(&rows::FileState::Off),
    ]
    .into();
    for t in texts {
        // The two CONFIG lines Georg knows by their exact spelling are the only `=` a row may hold.
        let t = t.replace("gp_net=on", "").replace("gp_stick=on", "");
        for w in ["verdict", "tco", "msr", "hpet", "dmar", "bus master", "=", "_"] {
            assert!(!t.to_lowercase().contains(w), "{t} has {w}");
        }
    }
}

/// Checker finding 2: MCP's failure is German on the page (never `NoToken` or `Listen(AddrInUse)`), and every row
/// that means Claude cannot reach the PC is red and never cut.
#[test]
fn mcp_failures_are_plain_german_and_must_be_kept() {
    let cases = [
        (McpDown::NoToken, "kein Zugangsschlüssel"),
        (McpDown::BadToken, "Zugangsschlüssel auf dem Stick ist ungültig"),
        (McpDown::BadPort, "Anschlussnummer"),
        (McpDown::NetworkNotReady, "Netzwerk im System ist nicht bereit"),
        (McpDown::Unknown, "unbekannt"),
    ];
    for (d, words) in cases {
        let t = rows::remote(&Remote::NotStarted(d));
        assert!(t.contains(words), "{t}");
        assert!(must_keep(&t), "{t} must be red and kept");
        for code in ["NoToken", "BadToken", "BadPort", "NoStack", "Listen", "Spawn", "AddrInUse"] {
            assert!(!t.contains(code), "{t} has {code}");
        }
    }
    assert!(must_keep(&rows::remote(&Remote::NoAddress)));
    assert!(!must_keep(&rows::remote(&Remote::Reachable { addr: String::from("192.168.100.20"), port: 8765 })));
    assert!(!must_keep(&rows::remote(&Remote::NetOff)), "no network on purpose is not an error");
}

/// Checker finding 1: the page comes first with the row "wird geschrieben"; the file thread's answer renews only
/// that row; the stick is not waited for when it can never come.
#[test]
fn the_file_row_is_renewed_in_place_and_skipped_when_the_stick_cannot_come() {
    let mut r = gaming_pc();
    r.set_file(&FileState::Waiting);
    let before = r.clone();
    assert_eq!(r.group(Group::Start).filter(|t| t.starts_with(rows::FILE_ROW)).count(), 1);
    r.set_file(&FileState::Written(String::from("GP000003.TXT")));
    assert_eq!(r.group(Group::Start).filter(|t| t.starts_with(rows::FILE_ROW)).count(), 1, "renewed, not added");
    assert!(r.lines().iter().any(|l| l.ends_with("\\BITSOS2\\REPORT\\GP000003.TXT")));
    assert!(r.same_rows(&before), "the file's row alone is no new collection");
    let mut later = r.clone();
    later.head = String::from("BitsOS-Bericht für den Gaming-PC, Version 1, 107 Sekunden nach dem Start");
    assert!(later.same_rows(&r), "nor is the head's seconds");
    later.push(Group::Network, "Karte eth0: Kabel verbunden (1000 Mbit/s), Adresse 192.168.100.20/24");
    assert!(!later.same_rows(&r));
    assert_eq!(rows::file_first(false, true, false), FileState::Off);
    assert!(matches!(rows::file_first(true, true, true), FileState::Failed(w) if w.contains("Speicherschutz")));
    assert!(matches!(rows::file_first(true, false, false), FileState::Failed(w) if w.contains("USB-Baustein")));
    assert_eq!(rows::file_first(true, false, true), FileState::Waiting);
}

#[test]
fn the_version_number_row_says_numbered_or_development() {
    assert_eq!(rows::version_number(0, ""), "Versionsnummer: keine (ein Entwicklungsstand)");
    assert_eq!(rows::version_number(165, "2026-11-16"), "Versionsnummer: 165 vom 16.11.2026");
    assert_eq!(rows::version_number(165, ""), "Versionsnummer: 165");
}

#[test]
fn the_question_stands_at_the_foot_of_every_page_and_nothing_is_cut() {
    use crate::ask::{self, Back, Key, State, KEY_JA, KEY_ZURUECK};
    let mut r = gaming_pc();
    let plain = layout(&r, COLS, ROWS);
    r.ask = ask::lines(168, &State::asking(Back::To(167)));
    let pages = layout(&r, COLS, ROWS);
    assert!(pages.len() >= plain.len());
    for p in &pages {
        assert!(p.len() <= ROWS as usize, "a page taller than the screen: {}", p.len());
        assert!(p.iter().any(|l| l == "Version 168 läuft. Bestätigen?"), "the question on every page");
        assert!(p.iter().any(|l| l.contains("Taste Z: Zurück zur vorherigen (Version 167")));
    }
    kept_whole(&r, &pages, COLS);
    assert!(!r.lines().iter().any(|l| l.contains("Bestätigen?")), "the file and MCP's text keep the rows only");
    // The keys: J and the German Z, only once armed; Zurück only with an older confirmed version.
    let s = State::asking(Back::To(167));
    assert_eq!(ask::key(&s, KEY_JA, true), Key::Ja);
    assert_eq!(ask::key(&s, KEY_ZURUECK, true), Key::Zurueck);
    assert_eq!(ask::key(&s, KEY_JA, false), Key::Early, "too early: taken, the page neither turns nor ends");
    assert_eq!(ask::key(&s, KEY_ZURUECK, false), Key::Early);
    assert_eq!(ask::key(&s, 57, true), Key::Page, "space turns the page");
    assert_eq!(ask::key(&s, 57, false), Key::Page);
    let none = State::asking(Back::Nothing);
    assert_eq!(ask::key(&none, KEY_ZURUECK, true), Key::Page, "no older confirmed version: no Zurück");
    assert_eq!(ask::key(&none, KEY_ZURUECK, false), Key::Page);
    assert!(ask::lines(168, &none)[1].contains("gibt es nicht"));
    assert_eq!(ask::key(&State::Confirming, KEY_JA, true), Key::Page, "answered: no second answer");
    assert_eq!(ask::lines(168, &State::Back { to: 167 }), vec![String::from("Zurück zu Version 167: BitsOS startet in wenigen Sekunden neu.")]);
}

#[test]
fn an_unread_way_back_offers_only_ja_without_saying_there_is_none() {
    use crate::ask::{self, Back, Key, State, KEY_ZURUECK};
    let s = State::asking(Back::Unread);
    let l = ask::lines(168, &s);
    assert_eq!(l, vec![String::from("Version 168 läuft. Bestätigen?"), String::from("Taste J: Ja (Version 168 bleibt).")]);
    assert_eq!(ask::key(&s, KEY_ZURUECK, true), Key::Page);
}

#[test]
fn a_lost_ja_and_an_early_key_are_said_on_the_page() {
    use crate::ask::{self, Back, State};
    let l = ask::lines(168, &State::Asking { back: Back::To(167), again: true, early: true });
    assert!(l[0].contains("konnte nicht geschrieben werden"), "{l:?}");
    assert_eq!(l[1], "Version 168 läuft. Bestätigen?");
    assert!(l[3].contains("zu früh"), "{l:?}");
    let mut r = gaming_pc();
    r.ask = l;
    let pages = layout(&r, COLS, ROWS);
    assert!(pages.iter().all(|p| p.len() <= ROWS as usize && p.iter().any(|t| t.contains("zu früh"))));
    kept_whole(&r, &pages, COLS);
}
