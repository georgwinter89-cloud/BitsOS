//! The Brave and Steam trees split for a release: the makers' files stay out of the asset and are listed in
//! `VENDOR.txt` (where each is published, its size and SHA-256 as this build has it), everything else, which is
//! open source (GTK, fonts, the steamrt graphics provider, dbus, our own scripts), goes into the runtime asset.
//! Brave's files are those `user/ports/brave-launcher/scripts/stage-v2.sh` copies from Brave's release (CachyOS's
//! brave-bin is that release unpacked); Steam's are Valve's bootstrap and licence files from the steam-launcher deb
//! and the client packages named in `user/ports/steam/pins.txt`.

use bitsos_install::sha;
use bitsos_install::vendor::{File, List, Source};
use std::path::{Path, PathBuf};

/// What `brave-launcher/scripts/stage-v2.sh` makes itself next to Brave's own files.
const BRAVE_OURS: &[&str] = &["lib", "fonts", "etc", "fontcache", "share", "glycin", "libexec", "LICENSES"];
const STEAM_CDN: &str = "https://client-update.steamstatic.com";
const STEAM_DEBS: &str = "https://repo.steampowered.com/steam/archive/stable";

fn hash(p: &Path) -> Result<(String, u64), String> {
    sha::of_file(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Brave's version from the stage's LICENSES/README.txt (`Brave 1:1.98.60-1 (pacman brave-bin ...`).
fn brave_version(readme: &str) -> Option<String> {
    let v = readme.lines().next()?.strip_prefix("Brave ")?.split_whitespace().next()?;
    let v = v.rsplit_once(':').map(|(_, r)| r).unwrap_or(v);
    let v = v.rsplit_once('-').map(|(l, _)| l).unwrap_or(v);
    (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')).then(|| v.to_string())
}

/// (files that go into the runtime asset, VENDOR.txt).
pub fn split(root: &Path, tree: &str, files: &[(String, PathBuf)]) -> Result<(Vec<(String, PathBuf)>, Option<String>), String> {
    let prefix = format!("BITSOS2/{tree}/");
    let rel = |r: &str| r.strip_prefix(&prefix).unwrap_or(r).to_string();
    let mut l = List::default();
    let mut keep = Vec::new();
    match tree {
        "brave" => {
            let readme = files.iter().find(|(r, _)| rel(r) == "LICENSES/README.txt").ok_or("release-assets: the brave tree has no LICENSES/README.txt (stage-v2.sh writes it)")?;
            let text = std::fs::read_to_string(&readme.1).map_err(|e| format!("{}: {e}", readme.1.display()))?;
            let v = brave_version(&text).ok_or("release-assets: cannot read Brave's version from the brave tree's LICENSES/README.txt")?;
            let zip = format!("brave-browser-{v}-linux-amd64.zip");
            l.sources.push(Source { id: "brave".into(), url: format!("https://github.com/brave/brave-browser/releases/download/v{v}/{zip}"), size: None, sha256: None, cache_name: zip });
            for (r, p) in files {
                let rr = rel(r);
                if BRAVE_OURS.contains(&rr.split('/').next().unwrap_or("")) {
                    keep.push((r.clone(), p.clone()));
                } else {
                    let (sha256, size) = hash(p)?;
                    l.files.push(File { member: format!("zip:{rr}"), rel: rr, size, sha256, source: "brave".into() });
                }
            }
        }
        "steam" => {
            let pins = std::fs::read_to_string(root.join("user/ports/steam/pins.txt")).map_err(|e| format!("steam pins.txt: {e}"))?;
            let pin = |k: &str| pins.lines().find_map(|line| line.strip_prefix(&format!("{k} "))).map(|v| v.split_whitespace().map(String::from).collect::<Vec<_>>());
            let deb = pin("launcher").filter(|v| v.len() == 2).ok_or("steam pins.txt: no launcher line")?;
            let man = pin("manifest").filter(|v| v.len() == 3).ok_or("steam pins.txt: no manifest line")?;
            let cache = std::env::var_os("V2_DOWNLOADS").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/bitsos-v2/downloads"))).map(|d| d.join("steam"));
            let deb_size = cache.as_ref().and_then(|c| std::fs::metadata(c.join(&deb[0])).ok()).map(|m| m.len());
            l.sources.push(Source { id: "deb".into(), url: format!("{STEAM_DEBS}/{}", deb[0]), size: deb_size, sha256: Some(deb[1].clone()), cache_name: deb[0].clone() });
            let manifest_file = format!("package/{}.manifest", man[0]);
            let from_deb = [
                ("bootstraplinux_ubuntu12_32.tar.xz", "usr/lib/steam/bootstraplinux_ubuntu12_32.tar.xz"),
                ("LICENSES/steam-launcher.copyright", "usr/share/doc/steam-launcher/copyright"),
                ("LICENSES/steam_subscriber_agreement.txt.gz", "usr/share/doc/steam/steam_subscriber_agreement.txt.gz"),
            ];
            for (r, p) in files {
                let rr = rel(r);
                let (sha256, size) = hash(p)?;
                if let Some((_, m)) = from_deb.iter().find(|(f, _)| *f == rr) {
                    l.files.push(File { rel: rr, size, sha256, source: "deb".into(), member: format!("deb:{m}") });
                } else if rr == manifest_file {
                    if sha256 != man[2] {
                        return Err(format!("release-assets: the steam stage's manifest is not the one pins.txt names ({})", man[1]));
                    }
                    l.sources.push(Source { id: "manifest".into(), url: format!("{STEAM_CDN}/{}", man[0]), size: Some(size), sha256: Some(sha256.clone()), cache_name: format!("{}.{}", man[0], man[1]) });
                    l.files.push(File { rel: rr, size, sha256, source: "manifest".into(), member: "-".into() });
                } else if let Some(z) = rr.strip_prefix("package/") {
                    let id = format!("pkg{}", l.sources.len());
                    l.sources.push(Source { id: id.clone(), url: format!("{STEAM_CDN}/{z}"), size: Some(size), sha256: Some(sha256.clone()), cache_name: z.to_string() });
                    l.files.push(File { rel: rr, size, sha256, source: id, member: "-".into() });
                } else {
                    keep.push((r.clone(), p.clone()));
                }
            }
            if !l.files.iter().any(|f| f.rel == manifest_file) {
                return Err(format!("release-assets: the steam tree has no {manifest_file}"));
            }
        }
        _ => return Err(format!("release-assets: no vendor rule for {tree}")),
    }
    // A list the installer could not read back would only fail at a tester's desk.
    let text = l.render();
    List::parse(&text)?;
    println!("vendor:   {tree}: {} files ({} MB) from their maker, {} files in the asset", l.files.len(), l.bytes() >> 20, keep.len());
    Ok((keep, Some(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brave_version_from_the_stage_readme() {
        assert_eq!(brave_version("Brave 1:1.98.60-1 (pacman brave-bin from CachyOS, /opt/brave-bin), packaged").as_deref(), Some("1.98.60"));
        assert_eq!(brave_version("Brave 1.2.3 (x)").as_deref(), Some("1.2.3"));
        assert_eq!(brave_version("Chromium 1"), None);
    }

    #[test]
    fn brave_split_keeps_only_open_source_parts() {
        let d = std::env::temp_dir().join(format!("xt-vendor-{}", std::process::id()));
        std::fs::create_dir_all(d.join("LICENSES")).unwrap();
        for (f, b) in [("brave", &b"bin"[..]), ("LICENSES/README.txt", b"Brave 1:1.98.60-1 (pacman)\n"), ("gtk", b"gtk")] {
            std::fs::write(d.join(f), b).unwrap();
        }
        let files = vec![
            ("BITSOS2/brave/brave".to_string(), d.join("brave")),
            ("BITSOS2/brave/LICENSES/README.txt".to_string(), d.join("LICENSES/README.txt")),
            ("BITSOS2/brave/lib/libgtk-3.so.0".to_string(), d.join("gtk")),
        ];
        let (keep, v) = split(Path::new("/nonexistent"), "brave", &files).unwrap();
        assert_eq!(keep.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(), ["BITSOS2/brave/LICENSES/README.txt", "BITSOS2/brave/lib/libgtk-3.so.0"]);
        let l = List::parse(&v.unwrap()).unwrap();
        assert_eq!(l.files.len(), 1);
        assert_eq!((l.files[0].rel.as_str(), l.files[0].member.as_str()), ("brave", "zip:brave"));
        assert!(l.sources[0].url.ends_with("/v1.98.60/brave-browser-1.98.60-linux-amd64.zip"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
