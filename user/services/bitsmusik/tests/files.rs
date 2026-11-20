//! Tags, lists, the scan and decoding on files the tests make (common/mod.rs); cut and garbage input never
//! panics. Nothing here opens /dev/snd.

mod common;

use bitsmusik::decode::Decoding;
use bitsmusik::events::Events;
use bitsmusik::sink::Resampler;
use bitsmusik::{m3u, scan, tags};
use common::*;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn id3v2_versions_and_encodings() {
    let utf16: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain("Grüße".encode_utf16().flat_map(|u| u.to_le_bytes()))
        .collect();
    let t = tags::read(&id3(
        3,
        &[
            ("TIT2", 0, b"Caf\xe9"),
            ("TPE1", 1, &utf16),
            ("TALB", 3, "Äpfel".as_bytes()),
            ("TRCK", 0, b"03/12"),
        ],
        64,
    ));
    assert_eq!(
        t,
        tags::Tags {
            title: Some("Café".into()),
            artist: Some("Grüße".into()),
            album: Some("Äpfel".into()),
            track: Some(3)
        }
    );
    let t = tags::read(&id3(
        4,
        &[("TIT2", 3, "Eins\0Zwei".as_bytes()), ("TRCK", 3, b"7")],
        0,
    ));
    assert_eq!((t.title.as_deref(), t.track), (Some("Eins"), Some(7)));
    let t = tags::read(&id3(2, &[("TT2", 0, b"Alt"), ("TP1", 0, b"Wer")], 10));
    assert_eq!(
        (t.title.as_deref(), t.artist.as_deref()),
        (Some("Alt"), Some("Wer"))
    );
}

#[test]
fn flac_and_ogg_comments() {
    let f = flac(
        44100,
        2,
        &sine(44100, 2, 0.1, 440.0),
        &[
            "TITLE=Sinus",
            "artist=Prüfer",
            "ALBUM=Töne",
            "TRACKNUMBER=2/9",
        ],
    );
    assert_eq!(
        tags::read(&f),
        tags::Tags {
            title: Some("Sinus".into()),
            artist: Some("Prüfer".into()),
            album: Some("Töne".into()),
            track: Some(2)
        }
    );
    // An ID3 tag in front of a FLAC: both are read, the ID3 frame first.
    let mut both = id3(3, &[("TIT2", 0, b"Vorne")], 0);
    both.extend_from_slice(&f);
    let t = tags::read(&both);
    assert_eq!(
        (t.title.as_deref(), t.album.as_deref()),
        (Some("Vorne"), Some("Töne"))
    );
    let long = format!("TITLE={}", "x".repeat(400));
    let o = ogg_vorbis_head(&[&long, "ARTIST=Ogg", "ALBUM=Seiten"]);
    let t = tags::read(&o);
    assert_eq!(
        (
            t.title.map(|s| s.len()),
            t.artist.as_deref(),
            t.album.as_deref()
        ),
        (Some(400), Some("Ogg"), Some("Seiten"))
    );
    // A field longer than the cap is cut to TEXT_MAX characters.
    let huge = format!("TITLE={}", "y".repeat(10_000));
    assert_eq!(
        tags::read(&ogg_vorbis_head(&[&huge]))
            .title
            .map(|s| s.chars().count()),
        Some(tags::TEXT_MAX)
    );
}

#[test]
fn cut_and_garbage_never_panic() {
    let samples = [
        id3(3, &[("TIT2", 0, b"Titel"), ("TPE1", 0, b"Wer")], 16),
        id3(4, &[("TIT2", 3, b"Titel")], 0),
        flac(
            8000,
            1,
            &sine(8000, 1, 0.05, 300.0),
            &["TITLE=a", "ARTIST=b"],
        ),
        ogg_vorbis_head(&["TITLE=a"]),
    ];
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    for s in &samples {
        for cut in 0..s.len() {
            let _ = tags::read(&s[..cut]);
        }
        // A sample of mutations (2000 per file; a longer fuzz run is not part of the quick tests).
        for _ in 0..2000 {
            let mut m = s.clone();
            for _ in 0..1 + r.next() % 4 {
                let i = (r.next() as usize) % m.len();
                m[i] = r.next() as u8;
            }
            let _ = tags::read(&m);
        }
    }
    // Claimed sizes and counts far beyond the bytes.
    let _ = tags::vorbis_comments(&[
        4, 0, 0, 0, b'v', b'e', b'n', b'd', 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ]);
    let _ = tags::read(b"ID3\x03\x00\x00\x7f\x7f\x7f\x7fTIT2\xff\xff\xff\xff\x00\x00");
}

#[test]
fn m3u_lines_and_keys() {
    let base = Path::new("/music/Listen");
    let l = m3u::parse(
        "\u{feff}#EXTM3U\n#EXTINF:12,Wer - Was\n..\\Album A\\01.flac\r\n\n/abs/x.mp3\nfile:///abs/y.ogg\n  b.wav  \n",
        base,
    );
    assert_eq!(
        l,
        vec![
            PathBuf::from("/music/Listen/../Album A/01.flac"),
            PathBuf::from("/abs/x.mp3"),
            PathBuf::from("/abs/y.ogg"),
            PathBuf::from("/music/Listen/b.wav")
        ]
    );
    assert_eq!(m3u::key(&l[0]), "/music/album a/01.flac");
}

fn library(root: &Path) -> scan::Library {
    let (ev, _) = Events::capture();
    scan::scan(&[root.to_path_buf(), root.join("fehlt")], &ev)
}

#[test]
fn scan_albums_lists_and_bounds() {
    let d = tmp("scan");
    let m = d.join("Musik");
    fs::create_dir_all(m.join("Album A")).unwrap();
    fs::create_dir_all(m.join("Ohne Tags")).unwrap();
    let pcm = sine(8000, 1, 0.05, 300.0);
    fs::write(
        m.join("Album A/02.flac"),
        flac(
            8000,
            1,
            &pcm,
            &["TITLE=Zwei", "ALBUM=Album A", "ARTIST=Wer", "TRACKNUMBER=2"],
        ),
    )
    .unwrap();
    fs::write(
        m.join("Album A/01.flac"),
        flac(
            8000,
            1,
            &pcm,
            &["TITLE=Eins", "ALBUM=Album A", "ARTIST=Wer", "TRACKNUMBER=1"],
        ),
    )
    .unwrap();
    fs::write(m.join("Ohne Tags/Lied.WAV"), wav(8000, 1, &pcm)).unwrap();
    fs::write(m.join("lose.wav"), wav(8000, 1, &pcm)).unwrap();
    fs::write(m.join("notiz.txt"), "keine Musik").unwrap();
    fs::write(
        m.join("Abend.m3u"),
        "#EXTM3U\nalbum a\\02.FLAC\nOhne Tags/Lied.WAV\nweg.mp3\n",
    )
    .unwrap();
    fs::write(m.join("Leer.m3u8"), "").unwrap();
    // A link loop is not followed.
    std::os::unix::fs::symlink(&m, m.join("Album A/schleife")).unwrap();
    let lib = library(&m);
    assert_eq!(lib.roots, vec![m.clone()]);
    let titles: Vec<&str> = lib.tracks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Eins", "Zwei", "Lied", "lose"]);
    let albums: Vec<(&str, usize, &str)> = lib
        .albums
        .iter()
        .map(|a| (a.name.as_str(), a.tracks.len(), a.artist.as_str()))
        .collect();
    assert_eq!(
        albums,
        [
            ("Album A", 2, "Wer"),
            (scan::LOOSE_ALBUM, 1, ""),
            ("Ohne Tags", 1, "")
        ]
    );
    let a = &lib.albums[0];
    assert_eq!(
        a.tracks
            .iter()
            .map(|&i| lib.tracks[i].number)
            .collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
    let lists: Vec<(&str, Vec<&str>)> = lib
        .lists
        .iter()
        .map(|l| {
            (
                l.name.as_str(),
                l.tracks
                    .iter()
                    .map(|&i| lib.tracks[i].title.as_str())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(lists, [("Abend", vec!["Zwei", "Lied"]), ("Leer", vec![])]);
}

#[test]
fn deep_folders_stop_at_the_bound() {
    let d = tmp("deep");
    let mut p = d.clone();
    for i in 0..scan::DEPTH_MAX + 2 {
        p = p.join(format!("d{i}"));
    }
    fs::create_dir_all(&p).unwrap();
    fs::write(
        p.join("tief.wav"),
        wav(8000, 1, &sine(8000, 1, 0.01, 300.0)),
    )
    .unwrap();
    fs::write(
        d.join("d0/oben.wav"),
        wav(8000, 1, &sine(8000, 1, 0.01, 300.0)),
    )
    .unwrap();
    let (ev, got) = Events::capture();
    let lib = scan::scan(std::slice::from_ref(&d), &ev);
    assert_eq!(
        lib.tracks
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["oben"]
    );
    assert!(
        got.lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("bitsmusik scan.too_deep"))
    );
}

fn decode_all(p: &Path) -> (u32, u32, Vec<f32>, Option<f64>) {
    let mut d = Decoding::open(p).unwrap();
    let mut out = Vec::new();
    let (mut rate, mut ch) = (0, 0);
    while let Some(c) = d.next().unwrap() {
        (rate, ch) = (c.rate, c.channels);
        out.extend_from_slice(&c.samples);
    }
    assert_eq!(d.bad_packets, 0);
    (rate, ch, out, d.duration)
}

#[test]
fn flac_and_wav_decode_to_the_sine() {
    let d = tmp("decode");
    let pcm = sine(44100, 2, 1.3, 440.0);
    fs::write(d.join("s.flac"), flac(44100, 2, &pcm, &["TITLE=Sinus"])).unwrap();
    fs::write(d.join("s.wav"), wav(44100, 2, &pcm)).unwrap();
    for f in ["s.flac", "s.wav"] {
        let (rate, ch, got, dur) = decode_all(&d.join(f));
        assert_eq!((rate, ch, got.len()), (44100, 2, pcm.len()), "{f}");
        let worst = pcm
            .iter()
            .zip(&got)
            .map(|(&a, &b)| (a as f32 / 32768.0 - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "{f}: off by {worst}");
        assert!(
            dur.is_some_and(|s| (s - 1.3).abs() < 0.01),
            "{f}: duration {dur:?}"
        );
    }
    // Not music, cut music: an error, never a panic.
    fs::write(d.join("kaputt.flac"), b"fLaC\x00\x00").unwrap();
    assert!(Decoding::open(&d.join("kaputt.flac")).is_err());
    let full = flac(8000, 1, &sine(8000, 1, 0.5, 300.0), &[]);
    fs::write(d.join("halb.flac"), &full[..full.len() / 2]).unwrap();
    if let Ok(mut dec) = Decoding::open(&d.join("halb.flac")) {
        while let Ok(Some(_)) = dec.next() {}
    }
}

#[test]
fn resampler_keeps_the_rate_ratio() {
    let mut r = Resampler::new(44100, 48000, 2);
    let input: Vec<f32> = (0..44100 * 2)
        .map(|i| ((i / 2) as f32 / 100.0).sin())
        .collect();
    let mut out = Vec::new();
    for c in input.chunks(1152 * 2) {
        r.run(c, &mut out);
    }
    let frames = out.len() / 2;
    assert!((frames as i64 - 48000).abs() < 3, "{frames} frames");
    assert!(out.iter().all(|s| s.abs() <= 1.0));
}

#[test]
fn empty_tags_fall_back_to_the_file_and_folder() {
    let d = tmp("leer");
    let m = d.join("Musik");
    fs::create_dir_all(m.join("Leere Tags")).unwrap();
    let pcm = sine(8000, 1, 0.05, 300.0);
    fs::write(
        m.join("Leere Tags/Stueck.flac"),
        flac(8000, 1, &pcm, &["TITLE=  ", "ALBUM=", "ARTIST=\u{0}"]),
    )
    .unwrap();
    let lib = library(&m);
    let t = &lib.tracks[0];
    assert_eq!(
        (t.title.as_str(), t.album.as_str(), t.artist.as_str()),
        ("Stueck", "Leere Tags", "")
    );
    assert_eq!(lib.albums.len(), 1);
    assert_eq!(
        (lib.albums[0].name.as_str(), lib.albums[0].tracks.len()),
        ("Leere Tags", 1)
    );
}

/// The five codecs of the item are built in (a feature left out of Cargo.toml fails here, not on the sofa).
#[test]
fn every_named_codec_is_registered() {
    use symphonia::core::codecs::audio::well_known::*;
    let reg = symphonia::default::get_codecs();
    for (name, id) in [
        ("mp3", CODEC_ID_MP3),
        ("flac", CODEC_ID_FLAC),
        ("vorbis", CODEC_ID_VORBIS),
        ("aac", CODEC_ID_AAC),
        ("wav pcm", CODEC_ID_PCM_S16LE),
    ] {
        assert!(
            reg.get_audio_decoder(id).is_some(),
            "{name} is not built in"
        );
    }
}

/// MPEG-1 Layer III frames of silence (all side info zero: no coded data), made here: probe and decode work.
#[test]
fn silent_mp3_decodes() {
    let d = tmp("mp3");
    // 128 kbit/s, 44.1 kHz, mono, no CRC: 144 * 128000 / 44100 = 417 bytes per frame.
    let mut b = Vec::new();
    for _ in 0..40 {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0xc0]);
        b.extend_from_slice(&f);
    }
    fs::write(d.join("still.mp3"), &b).unwrap();
    let mut dec = Decoding::open(&d.join("still.mp3")).unwrap();
    let mut n = 0;
    let mut rate = 0;
    while let Some(c) = dec.next().unwrap() {
        rate = c.rate;
        assert_eq!(c.channels, 1);
        assert!(c.samples.iter().all(|s| s.abs() < 1e-3));
        n += c.samples.len();
    }
    assert_eq!(rate, 44100);
    assert!(n >= 30 * 1152, "{n} samples");
}

/// Nothing audible, nothing louder: no source file names a control device or a volume, and only pcm.rs names
/// the sound nodes.
#[test]
fn no_control_device_and_no_volume() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for e in fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        let text = fs::read_to_string(&p).unwrap();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!text.contains("controlC"), "{name} names a control device");
        assert!(
            !text.contains("ELEM_WRITE"),
            "{name} writes a mixer control"
        );
        if name != "pcm.rs" {
            assert!(
                !text.contains("\"/dev/snd"),
                "{name} opens a node under /dev/snd"
            );
        }
    }
}
