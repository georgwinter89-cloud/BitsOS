//! Pieces of single benchmarks: Steam's stop on BitsOS, the scroll's frame time.

use bitsbench::bench::brave::per_frame;
use bitsbench::config::Config;
use std::path::Path;

#[test]
fn the_bitsos_steam_stop_is_one_the_launcher_knows() {
    let c = Config::defaults(Path::new("/data/leistung"), true);
    assert_eq!(c.steam_stop, ["/bin/sh", "/boot/BITSOS2/bin/steam", "--stop"]);
    assert_eq!(c.steam[..2], c.steam_stop[..2], "the same launcher starts and stops");
    let launcher = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ports/steam/bitsos/steam")).unwrap();
    let arms: Vec<&str> = launcher.lines().filter_map(|l| l.trim().strip_suffix(')')).filter(|w| w.starts_with('-') && !w.contains(' ')).collect();
    assert!(arms.contains(&"--stop"), "the launcher's case arms: {arms:?}");
    assert!(!arms.contains(&"-shutdown"), "Steam's own -shutdown would take the launcher's start path");
    // Another Linux asks Steam itself.
    assert_eq!(Config::defaults(Path::new("/x"), false).steam_stop, ["steam", "-shutdown"]);
}

#[test]
fn the_scroll_has_a_value_only_when_frames_were_counted() {
    assert_eq!(per_frame(1000.0, Some(100.0), Some(150.0)), Ok(20.0));
    assert_eq!(per_frame(1000.0, Some(100.0), Some(100.0)), Err("während des Scrollens wurde kein Bild gezeichnet".into()));
    assert_eq!(per_frame(1000.0, None, Some(100.0)), Err("Bildzähler der Arbeitsfläche nicht lesbar".into()));
    assert_eq!(per_frame(1000.0, Some(100.0), None), Err("Bildzähler der Arbeitsfläche nicht lesbar".into()));
}
