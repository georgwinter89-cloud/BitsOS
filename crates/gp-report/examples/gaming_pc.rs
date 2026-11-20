//! The Gaming-PC's expected report, as the wave notes of 14.11. give each getter's lines for this machine, laid out
//! on the console grid of 2560x1440 (160 x 45 cells): `cargo run -p gp-report --example gaming_pc [cols rows]`.
//! For the round's notes (the round-1 page: plan/hw/gaming-pc-round1-2026-11-15/GP000001.TXT); rows marked [U] in the notes are guesses.
use gp_report::rows::{self, Clock, Cpus, FileState, Remote, Watchdog};
use gp_report::{Group, Report};

fn main() {
    let a: Vec<u32> = std::env::args().skip(1).filter_map(|x| x.parse().ok()).collect();
    let (cols, rows_n) = (a.first().copied().unwrap_or(160), a.get(1).copied().unwrap_or(45));
    let mut r = Report::new(String::from("BitsOS-Bericht für den Gaming-PC, Version 1, 41 Sekunden nach dem Start"));
    r.extend(Group::Start, gp_platform_lines());
    r.push(Group::Start, "Laufende Version: 1 (die neueste, bestätigt)");
    r.push(Group::Start, rows::file(&FileState::Written(String::from("GP000001.TXT"))));
    r.extend(Group::Cpu, rows::cpus(Cpus { up: 20, planned: 20, p_cores: 8, e_cores: 12, hwp_on: 20 }));
    r.extend(Group::Cpu, rows::clock(Clock { hz: 3_878_400_000, cpuid15_hz: 3_878_400_000, ppm: Some(-40), cpuid15_checked: true, hpet_left_out: true }));
    r.push(Group::Cpu, rows::madt(20, 28, 0));
    r.push(Group::Cpu, rows::power(Some(5), true));
    r.push(Group::Cpu, rows::msr_faults(0, &[]));
    r.push(Group::Cpu, rows::spec_ctrl(Some(0x401)));
    r.push(Group::Guard, rows::dma("free"));
    r.push(Group::Guard, "Speicherschutz-Bausteine: 2 gefunden (die Werte stehen in der Datei)");
    for l in [
        "Anschluss-Stränge: 2 (Bus 00 bis 7f und 80 bis df), 41 Geräte und Abzweige gefunden",
        "Strang 1 (ab Bus 00): 13 Geräte, dazu 6 Abzweige und 1 Hauptknoten ohne eigene Zeile",
        "  00:02.0 Grafik [8086:7d67]: erkannt, ohne Treiber",
        "  00:0d.0 USB-Anschlüsse [8086:7ec0]: läuft mit xhci",
        "  00:0d.2 Thunderbolt/USB4 [8086:7ec2]: wird nie angefasst",
        "  00:0e.0 Intel VMD (Laufwerks-Verwaltung) [8086:ad0b]: wird nie angefasst",
        "  00:1f.5 Baustein mit der Firmware des Mainboards [8086:ae23]: wird nie angefasst",
        "  01:00.0 SSD [144d:a80d]: abgeschaltet, nicht angefasst",
        "  04:00.0 Grafik [1002:7550]: läuft mit amdgpu-rdna4",
        "  04:00.1 Ton der Grafikkarte [1002:ab40]: wird nie angefasst",
        "Strang 2 (ab Bus 80): 15 Geräte, dazu 6 Abzweige ohne eigene Zeile",
        "  80:14.0 USB-Anschlüsse [8086:7f6e]: läuft mit xhci",
        "  80:16.0 Intel-Verwaltungschip [8086:7f68]: wird nie angefasst",
        "  80:1f.3 Ton vom Mainboard-Chip [8086:7f50]: wird nie angefasst",
        "  80:1f.5 Baustein mit der Firmware des Mainboards [8086:7f24]: wird nie angefasst",
        "  81:00.0 SSD [1d97:5236]: abgeschaltet, nicht angefasst",
        "  82:00.0 SATA-Anschlüsse [1b21:0612]: abgeschaltet, nicht angefasst",
        "  83:00.0 Netzwerkkarte (Kabel) [10ec:8126]: läuft mit rtl8126",
        "  84:00.0 SATA-Anschlüsse [1b21:0612]: abgeschaltet, nicht angefasst",
        "  85:00.0 WLAN-Karte [8086:272b]: wird nie angefasst",
        "  86:00.0 SSD [144d:a80a]: abgeschaltet, nicht angefasst",
        "Nie angefasst: 8 Geräte (00:0d.2, 00:0e.0, 00:1f.5, 04:00.1, 80:16.0, 80:1f.3, 80:1f.5, 85:00.0), dazu 1 Abzweig ohne eigene Zeile (80:1c.3)",
        "Intel VMD ist im BIOS eingeschaltet (00:0e.0). BitsOS fasst es nicht an. SSDs hinter VMD wären unsichtbar; gefunden wurden 3 SSDs.",
    ] {
        r.push(Group::Devices, l);
    }
    for l in [
        "01:00.0 Samsung 990 EVO Plus 2TB: kein Treiber, nicht gelesen",
        "81:00.0 Lexar SSD NQ790 2TB: kein Treiber, nicht gelesen",
        "82:00.0 SATA-Anschlüsse (ASMedia, ohne Platte): kein Treiber, nicht gelesen",
        "84:00.0 SATA-Anschlüsse (ASMedia, ohne Platte): kein Treiber, nicht gelesen",
        "86:00.0 Samsung 980 PRO 1TB: kein Treiber, nicht gelesen",
        "00:0e.0 Intel VMD (leer): kein Treiber, nicht gelesen",
    ] {
        r.push(Group::Disks, l);
    }
    r.push(Group::Picture, "Bild: 2560 x 1440, liegt im Speicher von Grafik 04:00.0 (Adresse 0xa000000000, oberhalb von 512 GiB)");
    r.push(Group::Picture, rows::picture_speed(Some(true)));
    for l in [
        "USB-Baustein 00:0d.0: läuft, 3 Anschlüsse, 0 Geräte",
        "USB-Baustein 80:14.0: läuft, 22 Anschlüsse, 11 Geräte",
        "  Anschluss 3: Eingabegerät (Tastatur, Maus oder Ähnliches) [10f5:501b]",
        "  Anschluss 5: Speicher (Stick oder Laufwerk) [04f9:0228]",
        "  Anschluss 7: Eingabegerät (Tastatur, Maus oder Ähnliches) [046d:c08b]",
        "  Anschluss 10: Verteiler [05e3:0610]",
        "  Anschluss 10.3: Tongerät [0ecb:2068]",
        "  Anschluss 14: erkannt, wird nicht benutzt [8087:0036]",
        "  Anschluss 22.3: Speicher (Stick oder Laufwerk) [090c:1000]",
        "USB zusammen: 5 Eingabegeräte, 2 Speicher, 1 Tongeräte",
    ] {
        r.push(Group::Usb, l);
    }
    for l in [
        "Netzwerk 1/8: Karte erkannt: RTL8126A, Kennung 64a, Steckplatz 83:00.0",
        "Netzwerk 2/8: Firmware geladen: rtl8126a-3_0.0.5 08/30/24",
        "Netzwerk 3/8: Leitungsteil erkannt, Kennung 001cc862",
        "Netzwerk 4/8: Kabelverbindung steht: 1000 Mbit/s",
        "Netzwerk 5/8: Empfang läuft: das erste Datenpaket ist angekommen",
        "Netzwerk 6/8: Adresse vom Router erhalten: 192.168.100.20",
        "Netzwerk 7/8: Der Router antwortet (412 Mikrosekunden)",
        "Netzwerk 8/8: Die Karte meldet sich von selbst (Unterbrechung)",
        "Karte rtl0: Kabel verbunden (1000 Mbit/s), Adresse 192.168.100.20/24",
    ] {
        r.push(Group::Network, l);
    }
    r.push(Group::Network, rows::remote(&Remote::Reachable { addr: String::from("192.168.100.20"), port: 8765 }));
    r.push(Group::Gpu, "Grafikkarte 1 (zeigt das Bild): erkannt, 16 GB Speicher, Bauteilliste gelesen (wie unter Linux gemessen). Nichts verändert.");
    r.push(Group::Watchdog, rows::watchdog(Watchdog::Halted));
    r.push(Group::Sound, rows::sound(&[String::from("dem USB-Tongerät xhci1-10.3 (2 Kanäle)")]));
    println!("=== file / MCP text ===");
    print!("{}", r.file_text().replace("\r\n", "\n"));
    let pages = gp_report::page::layout(&r, cols, rows_n);
    for (i, p) in pages.iter().enumerate() {
        println!("=== screen page {} of {} ({cols}x{rows_n}) ===", i + 1, pages.len());
        for l in p {
            println!("{l}");
        }
    }
}

/// `gp_platform::tiers::report_lines` for the dmi reason and both tiers (copied: the example has no dependency).
fn gp_platform_lines() -> Vec<&'static str> {
    vec![
        "Sicherer Modus für den Gaming-PC (am Mainboard erkannt).",
        "Netzwerk über Kabel: an. WLAN bleibt aus.",
        "Start-Stick: darf beschrieben werden (neue Versionen aus der Ferne).",
        "Andere USB-Datenträger: werden nur gelesen.",
        "Eingebaute Platten: kein Treiber, nicht gelesen.",
    ]
}
