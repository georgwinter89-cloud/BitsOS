//! BR-9: a package that is the program a stick launcher starts gets the stick entry's id, so the library shows one
//! tile for it (the package wins over the stick by `rank`) and everything that names that id (the sofa's Brave
//! tiles and web favourites, pinned `programm/brave`, habits) keeps working whichever of the two is there.

/// (desktop file id of the package, the stick launcher's id). Brave from the repositories (`brave-bin`'s
/// brave-browser.desktop) is the Brave of the stick's `brave` launcher: the same browser, window class and,
/// through bitsrun's browser start, the same profile; the stick launcher hands its starts to it.
const TWINS: [(&str, &str); 1] = [("brave-browser", "brave")];

/// The library id of a package's `.desktop` entry.
pub fn package_id(desktop_id: String) -> String {
    TWINS.iter().find(|(d, _)| *d == desktop_id).map_or(desktop_id, |(_, s)| (*s).to_string())
}
