use super::*;

/// 2026-11-14 12:00 UTC, an hour ahead locally (13:00).
const NOW: u64 = 1_794_657_600_000;
const OFF: i32 = 60;

fn summary() -> Json {
    Json::parse(
        r#"{"since":0,"now":0,"on":true,"lost":3,"oldest":0,"programs":[
        {"app":"/apps/brave/brave","name":"brave","system":false,"pids":[300],"first":1,"last":1794657590000,"new":true,
         "files":[{"folder":"/data/home/1000/Briefe","private":true,"read":4,"write":0,"create":0,"delete":0,"rename":0,"last":1794657500000,"new":false},
                  {"folder":"/data/home/1000/Downloads","private":false,"read":0,"write":0,"create":1,"delete":0,"rename":0,"last":1794657400000,"new":false,"example":"bild.png"}],
         "files_more":0,
         "net":[{"host":"93.184.216.34","words":"93.184.216.34","proto":"tcp","port":443,"port_words":"Port 443, sicheres Web","connect":2,"accept":0,"listen":0,"tx":1200,"rx":56000,"last":1794657590000,"new":true}],
         "net_more":0,
         "devices":[{"class":"mikrofon","label":"Mikrofon","paths":["/dev/snd/pcmC0D0c"],"count":1,"last":1794571200000,"new":true,"write":false}],
         "rights":[{"kind":"caps","what":"4096","count":1,"last":1}]},
        {"app":"/bin/bitspkgd","name":"bitspkgd","system":true,"pids":[301],"first":1,"last":1794657000000,"new":false,
         "files":[],"files_more":0,"net":[{"host":"192.168.100.1","words":"192.168.100.1 (Heimnetz)","proto":"tcp","port":80,"port_words":"Port 80, Web","connect":1,"accept":0,"listen":0,"tx":0,"rx":0,"last":1794657000000,"new":false}],"net_more":0,"devices":[],"rights":[]},
        {"app":"/apps/spiel/spiel","name":"spiel","system":false,"started":3,"pids":[302],"first":1,"last":1794656000000,"new":false,
         "files":[],"files_more":0,"net":[],"net_more":0,"devices":[{"class":"ton","label":"Ton","paths":["/dev/snd/pcmC0D0p","/dev/snd/controlC0"],"count":4,"last":1794656000000,"new":false,"write":true}],"rights":[]},
        {"app":"/apps/start/start","name":"start","system":false,"started":2,"pids":[303],"first":1,"last":1794655000000,"new":false,
         "files":[],"files_more":0,"net":[],"net_more":0,"devices":[],"rights":[]}]}"#,
    )
    .unwrap()
}

#[test]
fn the_page_reads_like_the_mock_up_says() {
    let d = parse(&summary());
    assert_eq!((d.programs.len(), d.lost, d.on), (4, 3, true));
    let b = build(&d, "heute", true, NOW, OFF);
    let text = rows_text(&b);
    assert!(text.starts_with("[ANZEIGE]\nZeitraum | heute |"), "{text}");
    assert!(text.contains("Aufschreiben | true |"), "{text}");
    assert!(text.contains("Lücken | 3 Einträge |"), "{text}");
    assert!(text.contains("[BRAVE · NEU]"), "{text}");
    assert!(text.contains("Dateien: Briefe | 4× gelesen | 12:58 | /data/home/1000/Briefe · privater Ordner"), "{text}");
    assert!(text.contains("Dateien: Downloads | 1× angelegt | 12:56 | /data/home/1000/Downloads"), "{text}");
    assert!(text.contains("Netz: 93.184.216.34 | 2× verbunden | 12:59 | Port 443, sicheres Web · 1,2 kB gesendet, 56,0 kB empfangen · Neu: zum ersten Mal"), "{text}");
    assert!(text.contains("Gerät: Mikrofon | 1× geöffnet | gestern 13:00 | Neu: zum ersten Mal"), "{text}");
    assert!(text.contains("Gerät: Ton | 4× geöffnet | 12:33 | 2 Geräte"), "{text}");
    assert!(text.contains("Gestartet | 2× | 12:16 | Sonst hat es nichts angefasst, das BitsOS aufschreibt."), "{text}");
    assert!(!text.contains("/dev/") && !text.contains("/apps/"), "device nodes and program paths are code words: {text}");
    assert!(text.contains("Rechte | 1× |  | hat sich mehr Rechte gegeben"), "{text}");
    assert!(text.contains("[BITSOS SELBST]\nbitspkgd |  | 12:50 | 1 Adresse"), "{text}");
    // What is new or private stands out; the rest is plain.
    let tone = |label: &str| b.model.rows.iter().position(|r| r.label == label).and_then(|i| b.subs[i].as_ref()).map(|s| s.1);
    assert_eq!(tone("Dateien: Briefe"), Some(Tone::Plain), "private is said in words, not in red");
    assert_eq!(tone("Netz: 93.184.216.34"), Some(Tone::Bad), "new");
    assert_eq!(tone("Dateien: Downloads"), Some(Tone::Plain));
    // No code words in what Georg reads.
    for w in ["read", "connect", "netclose", "pid", "exe", "null"] {
        assert!(!text.to_lowercase().contains(&format!(" {w} ")), "{w}: {text}");
    }
}

#[test]
fn empty_waiting_and_unreachable_say_so() {
    let b = build(&Data::default(), "7", false, NOW, OFF);
    let t = rows_text(&b);
    assert!(t.contains("Wird geladen") && t.contains("Aufschreiben | false |  | Ausgeschaltet: BitsOS schreibt nichts auf."), "{t}");
    let b = build(&Data { loaded: true, ..Data::default() }, "30", true, NOW, OFF);
    assert!(rows_text(&b).contains("Noch nichts"));
    let b = build(&Data { loaded: true, error: Some("weg".into()), ..Data::default() }, "heute", true, NOW, OFF);
    assert!(rows_text(&b).contains("Keine Antwort |  |  | weg"));
}

#[test]
fn ranges_times_and_sizes_in_plain_words() {
    // 13:00 local: 13 hours (and a second) back to midnight.
    assert_eq!(since_s("heute", NOW, OFF), 13 * 3600 + 1);
    assert_eq!(since_s("7", NOW, OFF), 7 * 86_400);
    assert_eq!(since_s("30", NOW, OFF), 30 * 86_400);
    assert_eq!(when(NOW - 3 * 86_400_000, NOW, OFF), "am 11.11. 13:00");
    assert_eq!(bytes(999), "999 Byte");
    assert_eq!(bytes(3_400_000), "3,4 MB");
    assert_eq!(link_row().kind, Kind::Link("werhatwas".into()));
    assert_eq!(crate::address::normalise("werhatwas"), ADDRESS);
    assert!(matches!(crate::address::resolve("werhatwas"), crate::address::Open::Settings(_)));
}
