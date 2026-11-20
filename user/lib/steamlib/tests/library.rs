//! GM-6s: games from fixture Steam roots: a native game, a Proton game, a Proton tool, a broken file, a 2 MiB file, deep
//! nesting, a second library, the old libraryfolders format; and only the files of sofa-mode.md 3.3 are opened.

mod common;
use common::*;
use std::path::PathBuf;
use steamlib::{State, scan_with};

const PROTON_GAME: &str = "\t\"UserConfig\"\n\t{\n\t\t\"platform_override_dest\"\t\t\"linux\"\n\t\t\"platform_override_source\"\t\t\"windows\"\n\t}\n";

/// A root with everything, decoys included; returns (root, second library).
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let d = fresh(name);
    let root = d.join("Steam");
    let lib2 = d.join("Spiele Bibliothek");
    let sa = root.join("steamapps");
    put(
        &sa.join("libraryfolders.vdf"),
        format!(
            "\"libraryfolders\"\n{{\n\t\"contentstatsid\"\t\t\"-123\"\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\"\n\t\t{{\n\t\t\t\"32360\"\t\t\"1\"\n\t\t}}\n\t}}\n\
             \t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"2\"\n\t{{\n\t\t\"path\"\t\t\"relative/lib\"\n\t}}\n}}\n",
            root.display(),
            lib2.display()
        ),
    );
    put(&sa.join("appmanifest_32360.acf"), manifest(32360, "The Secret of Monkey Island: Special Edition", 4, ""));
    put(&sa.join("appmanifest_440.acf"), manifest(440, "Windows Spiel", 6 | 64, PROTON_GAME));
    put(&sa.join("appmanifest_2805730.acf"), manifest(2805730, "Proton 9.0", 4, ""));
    put(&sa.join("appmanifest_1628350.acf"), manifest(1628350, "Steam Linux Runtime 3.0 (sniper)", 4, ""));
    put(&sa.join("appmanifest_70.acf"), "\"AppState\"\n{\n\t\"appid\" \"70\"\n\t\"name\" \"kaputt");
    let mut big = manifest(80, "Zu gross", 4, "");
    big.push_str(&"// filler\n".repeat(2 * 1024 * 1024 / 10));
    put(&sa.join("appmanifest_80.acf"), big);
    let deep = format!("\"AppState\" {{ \"appid\" \"90\" {}{} }}", "\"x\" {".repeat(70), "}".repeat(70));
    put(&sa.join("appmanifest_90.acf"), deep);
    put(&sa.join("appmanifest_91.acf.tmp"), manifest(91, "Halb geschrieben", 4, ""));
    put(&sa.join("notes.txt"), "not a manifest");
    // Decoys: Steam's login and user files. Fixtures only; never a real one.
    for f in ["config/config.vdf", "config/loginusers.vdf", "userdata/1/config/localconfig.vdf", "ssfn123", "local.vdf"] {
        put(&root.join(f), "\"secret\" \"fixture\"\n");
    }
    let sa2 = lib2.join("steamapps");
    let mounted = "\t\"MountedConfig\"\n\t{\n\t\t\"platform_override_source\"\t\t\"Windows\"\n\t}\n";
    put(&sa2.join("appmanifest_1145360.acf"), manifest(1145360, "Hades", 1026, mounted));
    put(&sa2.join("appmanifest_32360.acf"), manifest(32360, "Doppelt", 4, ""));
    put(&sa2.join("appmanifest_1000.acf"), manifest(1000, "Wird entfernt", 4 | 0x800, ""));
    (root, lib2)
}

#[test]
fn games_tools_proton_and_problems() {
    let (root, lib2) = fixture("all");
    let rec = Recorder::default();
    let s = scan_with(&rec, &root);
    assert_eq!(s.folders, vec![root.clone(), lib2.clone()]);
    let ids: Vec<u32> = s.games.iter().map(|g| g.appid).collect();
    assert_eq!(ids, vec![440, 32360, 1000, 1145360], "{:#?}", s.games);
    let mi = &s.games[1];
    assert_eq!(mi.name, "The Secret of Monkey Island: Special Edition");
    assert_eq!((mi.size_on_disk, mi.last_played, mi.proton, mi.state), (123456789, 1730500000, false, State::Installed));
    assert_eq!(mi.installdir, mi.name);
    assert_eq!(mi.library, root);
    let win = &s.games[0];
    assert!(win.proton);
    assert_eq!(win.state, State::Installed, "a running game (64) with an update waiting (2) is still installed");
    let hades = &s.games[3];
    assert!(hades.proton, "MountedConfig counts too, any case");
    assert_eq!(hades.state, State::Loading);
    assert_eq!(s.games[2].state, State::Other(4 | 0x800));
    assert_eq!(s.tools, 2, "Proton 9.0 and the Steam Linux Runtime");
    let bad: Vec<String> = s.problems.iter().map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(bad, vec!["libraryfolders.vdf", "appmanifest_70.acf", "appmanifest_80.acf", "appmanifest_90.acf"], "{:#?}", s.problems);
    assert!(s.problems[2].what.contains("longer than 1048576"), "{:?}", s.problems[2]);
    assert!(s.problems[3].what.contains("nested too deep"), "{:?}", s.problems[3]);
}

#[test]
fn only_the_files_of_3_3_are_opened() {
    let (root, lib2) = fixture("opened");
    let rec = Recorder::default();
    let _ = scan_with(&rec, &root);
    let lists = rec.lists.borrow();
    let libs = [root.join("steamapps"), lib2.join("steamapps")];
    // The two libraries (the relative one never), and for a game without a platform_override its own prefix folder
    // `compatdata/<appid>`, by name only.
    for l in lists.iter() {
        let prefix_folder = l.parent().is_some_and(|c| c.file_name().is_some_and(|n| n == "compatdata") && libs.iter().any(|s| c.parent() == Some(s.as_path())))
            && l.file_name().is_some_and(|n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit()));
        assert!(libs.contains(l) || prefix_folder, "listed {}", l.display());
    }
    assert!(libs.iter().all(|l| lists.contains(l)), "{lists:#?}");
    let lists: Vec<PathBuf> = libs.to_vec();
    let reads = rec.reads.borrow();
    assert!(reads.contains(&root.join("steamapps/libraryfolders.vdf")));
    for r in reads.iter() {
        let name = r.file_name().unwrap().to_string_lossy();
        let in_lib = lists.iter().any(|l| r.parent() == Some(l.as_path()));
        let ok = name == "libraryfolders.vdf" && r.parent() == Some(root.join("steamapps").as_path())
            || in_lib && name.starts_with("appmanifest_") && name.ends_with(".acf");
        assert!(ok, "opened {}", r.display());
        assert!(!r.to_string_lossy().contains("config/") && !r.to_string_lossy().contains("userdata"));
    }
    assert_eq!(reads.len(), 1 + 7 + 3, "{reads:#?}");
}

#[test]
fn old_format_missing_file_links_and_watch_dirs() {
    let d = fresh("old");
    let root = d.join("Steam");
    put(
        &root.join("steamapps/libraryfolders.vdf"),
        "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\t\"1\"\n\t\"1\"\t\t\"D:\\\\SteamLibrary\"\n\t\"2\"\t\t\"/mnt/spiele\"\n}\n",
    );
    let folders = steamlib::library_folders(&root);
    assert_eq!(folders, vec![root.clone(), PathBuf::from("/mnt/spiele")], "D:\\SteamLibrary is no absolute path here");
    assert_eq!(steamlib::watch_dirs(&root), vec![root.join("steamapps"), PathBuf::from("/mnt/spiele/steamapps")]);
    // A manifest that is a link leads nowhere: it could point at a login file.
    put(&root.join("config/loginusers.vdf"), manifest(5, "Link", 4, ""));
    std::os::unix::fs::symlink(root.join("config/loginusers.vdf"), root.join("steamapps/appmanifest_5.acf")).unwrap();
    let s = steamlib::scan(&root);
    assert!(s.games.is_empty() && s.problems.len() == 2 && s.problems[1].path.ends_with("appmanifest_5.acf"), "{s:#?}");
    let none = fresh("none");
    let s = steamlib::scan(&none.join("Steam"));
    assert_eq!(s.folders.len(), 1);
    assert!(s.games.is_empty() && s.problems.len() == 1, "no Steam at all: one problem (the root's steamapps), no panic");
    assert_eq!(steamlib::run_url(32360), "steam://rungameid/32360");
}

#[test]
fn a_big_libraryfolders_leaves_the_root() {
    let d = fresh("bigvdf");
    let root = d.join("Steam");
    put(&root.join("steamapps/libraryfolders.vdf"), vec![b' '; 2 * 1024 * 1024]);
    put(&root.join("steamapps/appmanifest_32360.acf"), manifest(32360, "Monkey", 4, ""));
    let s = steamlib::scan(&root);
    assert_eq!(s.folders, vec![root]);
    assert_eq!(s.games.len(), 1);
    assert!(s.problems[0].what.contains("longer than"));
}

#[test]
fn at_most_4096_manifests_across_all_libraries() {
    // 4000 + 99: each library alone is under the cap, together they are over: the cap is for the whole scan.
    let d = fresh("cap");
    let root = d.join("Steam");
    let lib2 = d.join("zweite");
    put(&root.join("steamapps/libraryfolders.vdf"), format!("\"libraryfolders\" {{ \"1\" {{ \"path\" \"{}\" }} }}", lib2.display()));
    let tiny = |id: u32| format!("\"AppState\" {{ \"appid\" \"{id}\" \"name\" \"G{id}\" \"StateFlags\" \"4\" }}");
    for id in 1..=4000u32 {
        put(&root.join(format!("steamapps/appmanifest_{id}.acf")), tiny(id));
    }
    for id in 100_001..=100_099u32 {
        put(&lib2.join(format!("steamapps/appmanifest_{id}.acf")), tiny(id));
    }
    let rec = Recorder::default();
    let s = scan_with(&rec, &root);
    assert_eq!(s.games.len(), steamlib::MAX_MANIFESTS);
    let manifests = rec.reads.borrow().iter().filter(|p| p.extension().is_some_and(|e| e == "acf")).count();
    assert_eq!(manifests, steamlib::MAX_MANIFESTS, "no manifest beyond the cap is opened");
    let over: Vec<_> = s.problems.iter().filter(|p| p.what.contains("more than 4096 manifests")).collect();
    assert_eq!(over.len(), 1, "{:#?}", s.problems);
    assert_eq!(over[0].path, lib2.join("steamapps"));
    assert!(s.games.iter().filter(|g| g.appid > 100_000).count() == 96);
}

/// A game Steam starts through Proton by itself has no platform_override in its manifest (the Mini-PC, 11.11.2026):
/// the prefix Steam made for it says so. A prefix folder without `pfx` (a native game's shader folder) does not.
#[test]
fn a_proton_prefix_marks_a_windows_game() {
    let (root, lib2) = fixture("prefix");
    std::fs::create_dir_all(root.join("steamapps/compatdata/32360/pfx")).unwrap();
    std::fs::create_dir_all(lib2.join("steamapps/compatdata/1000")).unwrap();
    let s = scan_with(&Recorder::default(), &root);
    let by = |id: u32| s.games.iter().find(|g| g.appid == id).unwrap();
    assert!(by(32360).proton, "a prefix with pfx");
    assert!(!by(1000).proton, "a prefix folder without pfx");
    assert!(by(440).proton, "the manifest's own word still counts");
    assert!(steamlib::is_tool(4690330, "Legacy Steam Runtime") && !steamlib::is_tool(22380, "Fallout: New Vegas"));
}
