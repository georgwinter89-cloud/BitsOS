//! GM-6b: the sofa mode's collections, sort orders, lines and hero from a fake index (fake stick launchers) and a
//! fake Steam root (Georg's four games of the Mini-PC check list, a Proton tool, a game being loaded and one being
//! removed). Fixtures only: no login file of Steam's is ever written or read.

use std::path::{Path, PathBuf};

use super::*;
use crate::library::{Roots, scan};
use crate::playtime::{Book, Rec};

pub(crate) fn scratch(name: &str) -> PathBuf {
    let target = option_env!("CARGO_TARGET_DIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from);
    let d = target.join("gm-6b-tests").join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn put(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// An appmanifest as Steam writes it (steamlib's fixture format).
pub(crate) fn manifest(appid: u32, name: &str, flags: u32, last: u64, size: u64, proton: bool) -> String {
    let extra = if proton { "\t\"UserConfig\"\n\t{\n\t\t\"platform_override_dest\"\t\t\"linux\"\n\t\t\"platform_override_source\"\t\t\"windows\"\n\t}\n" } else { "" };
    format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\t\"{flags}\"\n\t\"installdir\"\t\t\"{name}\"\n\
         \t\"LastPlayed\"\t\t\"{last}\"\n\t\"SizeOnDisk\"\t\t\"{size}\"\n{extra}}}\n"
    )
}

pub const NOW: i64 = crate::status::FIXED_SECS; // Wed 2026-10-21 14:32 UTC
const DAY: i64 = 86_400;

/// Georg's four games: Monkey Island (Linux, yesterday), Farthest Frontier (Proton, today 12:32), Dungeon Keeper 2
/// (Proton, never by Steam), KKND2 (Proton, 9 days ago); Proton 9.0 (a tool), one loading, one being removed.
pub(crate) fn steam_root(d: &Path) -> PathBuf {
    let root = d.join("Steam");
    let sa = root.join("steamapps");
    put(&sa.join("libraryfolders.vdf"), &format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n", root.display()));
    let games: [(u32, &str, u32, i64, u64, bool); 7] = [
        (32360, "The Secret of Monkey Island: Special Edition", 4, NOW - DAY, 2_100_000_000, false),
        (1044720, "Farthest Frontier", 4, NOW - 2 * 3600, 12_500_000_000, true),
        (900001, "Dungeon Keeper 2", 4, 0, 1_200_000_000, true),
        (900002, "KKND2: Krossfire", 4, NOW - 9 * DAY, 650_000_000, true),
        (2805730, "Proton 9.0", 4, 0, 1_000_000_000, false),
        (900003, "Neues Spiel", 4 | 0x100_000, 0, 0, false),
        (900004, "Wird entfernt", 4 | 0x800, 0, 0, false),
    ];
    for (id, name, flags, last, size, proton) in games {
        put(&sa.join(format!("appmanifest_{id}.acf")), &manifest(id, name, flags, last.max(0) as u64, size, proton));
    }
    root
}

/// Fake stick launchers (vkQuake, Brave, Cobalt, Steam) and the fake Steam root.
pub(crate) fn roots(d: &Path) -> Roots {
    use std::os::unix::fs::PermissionsExt;
    let bin = d.join("bin");
    for (id, name, cats, wm) in [("vkquake", "vkQuake", "Game;ActionGame;", ""), ("brave", "Brave", "Network;", ""), ("cobalt", "Cobalt", "AudioVideo;", ""), ("steam", "Steam", "Game;", "StartupWMClass=steam\n")] {
        put(&bin.join(format!("{id}.desktop")), &format!("[Desktop Entry]\nType=Application\nName={name}\nExec={id}\nCategories={cats}\n{wm}"));
        put(&bin.join(id), "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(bin.join(id), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    Roots { bin, apps: d.join("apps"), path: String::new(), steam: Some(steam_root(d)) }
}

pub(crate) fn index(name: &str) -> Index {
    let d = scratch(name);
    let exe = |p: &Path| p.is_file();
    scan(&roots(&d), &exe).index
}

fn tm(s: i64) -> libc::tm {
    crate::status::tm_of(s, true)
}

fn input<'a>(ix: &'a Index, play: &'a Book, sort: &'a str) -> Input<'a> {
    Input { index: ix, play, favourites: &[], music: None, windows: false, sort, now: NOW, tm: &tm }
}

fn keys(t: &[Tile]) -> Vec<&str> {
    t.iter().map(|t| t.key.as_str()).collect()
}

#[test]
fn the_collections_are_views_on_the_shared_index() {
    let ix = index("colls");
    let play = Book::default();
    let i = input(&ix, &play, "az");
    let spiele = tiles(Coll::Spiele, &i);
    assert_eq!(
        keys(&spiele),
        ["steam-900001", "steam-1044720", "steam-900002", "steam-900003", "steam-32360", "vkquake"],
        "Steam's games and vkQuake (Categories Game), A-Z; no tool, no game being removed, not the Steam client"
    );
    let windows = tiles(Coll::Windows, &i);
    assert_eq!(keys(&windows), ["steam-900001", "steam-1044720", "steam-900002"], "Proton games only");
    assert_eq!(count(Coll::Spiele, &i), Some(6));
    assert_eq!(count(Coll::Windows, &i), Some(3));
    assert_eq!(count(Coll::Brave, &i), None);
    let mi = spiele.iter().find(|t| t.key == "steam-32360").unwrap();
    assert_eq!((mi.meta.as_str(), mi.steam, mi.proton), ("Linux · Gestern", Some(32360), false));
    let ff = spiele.iter().find(|t| t.key == "steam-1044720").unwrap();
    assert_eq!(ff.meta, "Proton · 12:32", "today: the clock time");
    assert_eq!(spiele.iter().find(|t| t.key == "steam-900002").unwrap().meta, "Proton · Letzte Woche");
    assert_eq!(spiele.iter().find(|t| t.key == "steam-900001").unwrap().meta, "Proton · Noch nie gespielt");
    let dl = spiele.iter().find(|t| t.key == "steam-900003").unwrap();
    assert_eq!(dl.meta, "Linux · wird geladen");
    assert!(dl.greyed && dl.act == Act::Refused("Das Spiel wird noch geladen."), "a game still loading does not start");
    assert_eq!(dl.steam, Some(900003), "In Steam öffnen still has its id");
    let vk = spiele.iter().find(|t| t.key == "vkquake").unwrap();
    assert_eq!((vk.colour, vk.steam), (None, None), "a game that is no Steam game uses panel");
    assert!(spiele.iter().filter(|t| t.steam.is_some()).all(|t| t.colour == Some(colour(&t.key))));
    assert_eq!(info(Coll::Spiele, &spiele, &i), "6 installiert · 16 GB auf /data");
    assert_eq!(facts(ff), "über Proton · 13 GB", "the selection bar's facts");
    assert_eq!(facts(mi), "Linux · 2,1 GB");
    // What the tiles start (3.2).
    assert_eq!(ff.act, Act::Start { id: "steam-1044720".into(), extra: vec![], restore: true });
    let e = ix.entries.iter().find(|e| e.id == "steam-1044720").unwrap();
    assert_eq!(e.exec.last().map(String::as_str), Some("steam://rungameid/1044720"));
    assert!(e.exec[1].ends_with("bin/steam"), "through the stick's Steam launcher: {:?}", e.exec);
    let big = &tiles(Coll::BigPicture, &i)[0];
    assert!(matches!(&big.act, Act::Argv(v) if v.last().map(String::as_str) == Some("steam://open/bigpicture")));
    let brave = &tiles(Coll::Brave, &i)[0];
    assert_eq!(brave.act, Act::Start { id: "brave".into(), extra: vec!["--".into(), "--start-fullscreen".into()], restore: true });
}

#[test]
fn sort_orders_newest_a_to_z_and_largest() {
    let ix = index("sort");
    let mut play = Book::default();
    // BitsOS's own record is newer than Steam's for Dungeon Keeper 2 and counts (3.4).
    play.recs.insert("steam-900001".into(), Rec { last: NOW - 600, today: 600, total: 8100 });
    play.recs.insert("vkquake".into(), Rec { last: NOW - 3 * DAY, today: 60, total: 60 });
    let z = tiles(Coll::Spiele, &input(&ix, &play, "zuletzt"));
    assert_eq!(keys(&z), ["steam-900001", "steam-1044720", "steam-32360", "vkquake", "steam-900002", "steam-900003"]);
    let g = tiles(Coll::Spiele, &input(&ix, &play, "groesse"));
    assert_eq!(keys(&g), ["steam-1044720", "steam-32360", "steam-900001", "steam-900002", "steam-900003", "vkquake"]);
    let a = tiles(Coll::Spiele, &input(&ix, &play, "az"));
    assert_eq!(a[0].key, "steam-900001", "Dungeon Keeper 2 first A-Z");
    assert_eq!(a.iter().find(|t| t.key == "vkquake").unwrap().meta, "Linux · Vor 3 Tagen");
}

#[test]
fn the_hero_and_zuletzt_gespielt() {
    let ix = index("hero");
    let mut play = Book::default();
    play.recs.insert("steam-900001".into(), Rec { last: NOW - 600, today: 600, total: 8100 });
    let i = input(&ix, &play, "zuletzt");
    let h = hero(&i);
    assert_eq!(h.key.as_deref(), Some("steam-900001"));
    assert_eq!(h.label, "WEITERSPIELEN · WINDOWS-SPIEL ÜBER PROTON");
    assert_eq!(h.name, "Dungeon Keeper 2");
    assert_eq!(h.line, "Zuletzt gespielt heute um 14:22 · 2 Std. 15 Min.");
    assert_eq!(h.colour, colour("steam-900001"));
    let r = recent(&i, &h, 4);
    assert_eq!(keys(&r), ["steam-1044720", "steam-32360", "steam-900002"], "newest first, the hero left out, never played left out");
    assert_eq!(r.iter().map(|t| t.meta.as_str()).collect::<Vec<_>>(), ["Heute", "Gestern", "Letzte Woche"]);
    let mi = hero(&input(&ix, &Book::default(), "az"));
    assert_eq!((mi.key.as_deref(), mi.label.as_str()), (Some("steam-1044720"), "WEITERSPIELEN · WINDOWS-SPIEL ÜBER PROTON"));
    assert_eq!(mi.line, "Zuletzt gespielt heute um 12:32", "no own play time: no duration");
}

#[test]
fn the_empty_states_without_any_game_played() {
    let d = scratch("empty");
    let mut r = roots(&d);
    r.steam = Some(d.join("kein-steam"));
    let ix = scan(&r, &|p: &Path| p.is_file()).index;
    let play = Book::default();
    let i = input(&ix, &play, "zuletzt");
    let h = hero(&i);
    assert_eq!(h.key, None);
    assert_eq!((h.name.as_str(), h.line.as_str()), ("Bibliothek", "8 Programme · 1 Spiel"));
    assert_eq!(keys(&recent(&i, &h, 4)), ["vkquake"], "the installed games instead");
    assert_eq!(info(Coll::Musik, &[], &i), "Musik: noch nicht da");
    assert!(tiles(Coll::Musik, &i).is_empty());
    assert_eq!(count(Coll::Musik, &i), None);
}

#[test]
fn youtube_waits_while_other_windows_exist_and_brave_takes_web_favourites() {
    let ix = index("yt");
    let play = Book::default();
    let favs = vec!["programm/terminal".to_string(), "https://www.elster.de/".to_string(), "youtube.com".to_string()];
    let mut i = input(&ix, &play, "az");
    i.favourites = &favs;
    let yt = &tiles(Coll::YouTube, &i)[0];
    assert_eq!((yt.greyed, yt.meta.as_str()), (false, "Cobalt"));
    assert_eq!(yt.act, Act::Start { id: "cobalt".into(), extra: vec![], restore: true });
    i.windows = true;
    let yt = &tiles(Coll::YouTube, &i)[0];
    assert_eq!((yt.greyed, yt.meta.as_str()), (true, "kommt bald"), "decision S1");
    assert!(matches!(yt.act, Act::Refused(_)));
    let b = tiles(Coll::Brave, &i);
    assert_eq!(keys(&b), ["brave", "web:https://www.elster.de/", "web:youtube.com"]);
    assert_eq!(b[1].name, "elster.de");
    assert_eq!(b[2].act, Act::Start { id: "brave".into(), extra: vec!["https://youtube.com".into(), "--".into(), "--start-fullscreen".into()], restore: false });
    let m = Music { tracks: 412, lists: vec![("Abendliste".into(), 34)], albums: vec![("Blau".into(), 10)], last: None, playing: false };
    i.music = Some(&m);
    let t = tiles(Coll::Musik, &i);
    assert_eq!(keys(&t), ["musik:list:Abendliste", "musik:album:Blau"]);
    assert_eq!(t[0].act, Act::Music { kind: "list", name: "Abendliste".into() });
    assert_eq!(count(Coll::Musik, &i), Some(412));
    assert_eq!(info(Coll::Musik, &t, &i), "412 Titel · 1 Listen · 1 Alben");
    let none = crate::library::Index { entries: ix.entries.iter().filter(|e| e.id != "brave").cloned().collect() };
    i.index = &none;
    let b = tiles(Coll::Brave, &i);
    assert_eq!(keys(&b), ["brave"], "no Brave: one greyed tile, no web tiles");
    assert!(b[0].greyed && b[0].meta == "nicht installiert" && matches!(b[0].act, Act::Refused(_)));
}

#[test]
fn german_days_durations_sizes_and_a_stable_colour() {
    let now = tm(NOW);
    let t = |d: i64| days_text(&tm(NOW - d * DAY), &now);
    assert_eq!([t(0), t(1), t(2), t(6), t(7), t(13), t(14), t(30)], ["Heute", "Gestern", "Vor 2 Tagen", "Vor 6 Tagen", "Letzte Woche", "Letzte Woche", "Vor 2 Wochen", "Vor 4 Wochen"]);
    assert_eq!(t(400), "16.09.2025");
    assert_eq!(days_text(&tm(NOW + DAY), &now), "Heute", "a time after now (a clock set back) counts as today");
    assert_eq!(hero_when(&tm(NOW - DAY), &now), "gestern um 14:32");
    assert_eq!(hero_when(&tm(NOW - 3 * DAY), &now), "vor 3 Tagen");
    assert_eq!(hero_when(&tm(NOW - 400 * DAY), &now), "am 16.09.2025");
    // Across the year's end.
    let ny = tm(1_798_761_600 + 3600); // 2027-01-01 01:00 UTC
    assert_eq!(days_text(&tm(1_798_761_600 - 3600), &ny), "Gestern");
    assert_eq!([duration(0), duration(20), duration(2700), duration(7200), duration(8100)], [None, Some("unter 1 Min.".into()), Some("45 Min.".into()), Some("2 Std.".into()), Some("2 Std. 15 Min.".into())]);
    assert_eq!([size_text(238_000_000_000), size_text(1_234_000_000), size_text(850_000_000)], ["238 GB", "1,2 GB", "850 MB"]);
    assert_eq!(colour("steam-32360"), colour("steam-32360"));
    let spread: std::collections::HashSet<usize> = (0..40).map(|i| colour(&format!("steam-{i}"))).collect();
    assert_eq!(spread.len(), 4, "all four colours are used");
}

#[test]
fn the_launch_hint_takes_only_a_top_window_that_is_not_steams() {
    use crate::sofa_body::hint_takes;
    assert!(hint_takes("MonkeyIsland", false), "a native Steam game's own class");
    assert!(!hint_takes("steam", false), "the Steam client mapping first is not the game");
    assert!(!hint_takes("Steam", false));
    assert!(!hint_takes("steamwebhelper", false));
    assert!(!hint_takes("MonkeyIsland", true), "a dialog or popup is not the game's main window");
    assert!(!hint_takes("  ", false), "no class at all");
}

#[test]
fn the_search_finds_programs_best_first_and_starts_them_as_their_tiles_do() {
    let ix = index("gm6d-hits");
    let play = Book::default();
    let i = input(&ix, &play, "zuletzt");
    let hits = search_hits(&i, "dung");
    assert_eq!(hits.first().map(|h| (h.tile.key.as_str(), h.game)), Some(("steam-900001", true)), "Dungeon Keeper 2 first");
    assert_eq!(hits[0].tile.act, Act::Start { id: "steam-900001".into(), extra: Vec::new(), restore: true });
    assert!(search_hits(&i, "  ").is_empty(), "spaces find nothing");
    let b = search_hits(&i, "brave");
    assert_eq!(b[0].tile.act, Act::Start { id: "brave".into(), extra: vec!["--".into(), "--start-fullscreen".into()], restore: true }, "Brave fullscreen");
    // Cobalt as a hit keeps decision S1: refused while windows exist.
    let w = Input { windows: true, ..input(&ix, &play, "zuletzt") };
    let c = search_hits(&w, "cobalt");
    assert!(matches!(c[0].tile.act, Act::Refused(_)) && c[0].tile.greyed, "{:?}", c[0].tile.act);
    let web = brave_search(&i, "dungeon keeper 2");
    let want = vec!["https://search.brave.com/search?q=dungeon%20keeper%202".to_string(), "--".into(), "--start-fullscreen".into()];
    assert_eq!(web.act, Act::Start { id: "brave".into(), extra: want, restore: false });
    let none = Index::builtin();
    let n = Input { index: &none, ..input(&ix, &play, "zuletzt") };
    assert!(matches!(brave_search(&n, "x").act, Act::Refused(_)), "no Brave: refused");
}
