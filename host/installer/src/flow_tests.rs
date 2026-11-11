//! Whole runs as an agent starts them, from an assets folder to an image file: the answers on the stick, what the
//! host keeps, and the refusals that come before the stick is touched.

use crate::cli::Opts;
use crate::devices;
use crate::manifest::{Asset, Kind, Manifest};
use crate::sha;
use crate::tests::{Scratch, asset, kernel_read, kernel_view, system};
use std::io::Read;
use std::path::{Path, PathBuf};

struct Release {
    s: Scratch,
    assets: PathBuf,
    text: String,
    img: PathBuf,
}

const BRAVE_BIN: &[u8] = b"brave binary";

/// A release 0.9.0 with the system and a Mesa tree; `mesa_unpacked` overrides what the manifest says Mesa takes.
fn release(tag: &str, mesa_unpacked: Option<u64>) -> Release {
    release_with(tag, mesa_unpacked, false)
}

/// The same, with `brave`: Brave's runtime asset, whose VENDOR.txt names one file from its maker (`BRAVE_BIN`).
fn release_with(tag: &str, mesa_unpacked: Option<u64>, brave: bool) -> Release {
    let s = Scratch::new(tag);
    let assets = s.0.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let (sys, sb, sf) = system(&assets, "0.9.0");
    let (mesa, mb, mf) = asset(&assets, "bitsos-mesa-0.9.0.tar.zst", &[("BITSOS2/mesa/lib/x.so", b"x".to_vec())]);
    let a = |p: &Path, kind, tree: Option<&str>, unpacked, files| {
        let (sha, size) = sha::of_file(p).unwrap();
        Asset { name: p.file_name().unwrap().to_string_lossy().into(), size, sha256: sha, kind, tree: tree.map(String::from), needs: vec![], unpacked, files, vendor_bytes: 0, vendor_files: 0 }
    };
    let mut m = Manifest { version: "0.9.0".into(), assets: vec![a(&sys, Kind::System, None, sb, sf), a(&mesa, Kind::Tree, Some("mesa"), mesa_unpacked.unwrap_or(mb), mf)] };
    if brave {
        let vendor = format!("source bin https://example.invalid/brave {} {} brave-bin\nfile brave {} {} bin -\n", BRAVE_BIN.len(), sha::hex(BRAVE_BIN), BRAVE_BIN.len(), sha::hex(BRAVE_BIN));
        let (rt, rb, rf) = asset(&assets, "bitsos-brave-runtime-0.9.0.tar.zst", &[("BITSOS2/brave/lib/libgtk-3.so.0", b"gtk".to_vec()), ("VENDOR.txt", vendor.into_bytes())]);
        let mut e = a(&rt, Kind::Runtime, Some("brave"), rb, rf);
        (e.vendor_bytes, e.vendor_files) = (BRAVE_BIN.len() as u64, 1);
        m.assets.push(e);
    }
    let text = m.to_json();
    std::fs::write(assets.join("manifest.json"), &text).unwrap();
    std::fs::write(s.0.join("psk"), "a long passphrase\n").unwrap();
    let img = s.0.join("stick.img");
    Release { s, assets, text, img }
}

impl Release {
    fn host(&self) -> PathBuf {
        self.s.0.join("host")
    }

    fn opts(&self, host: &Path, extra: &str) -> Opts {
        let line = format!(
            "--image {} --image-size 256M --assets {} --manifest-sha256 {} --host-dir {} --cache {} --steam no --brave no {extra}",
            self.img.display(),
            self.assets.display(),
            sha::hex(self.text.as_bytes()),
            host.display(),
            self.s.0.join("cache").display(),
        );
        Opts::parse(line.split_whitespace().map(String::from).collect()).unwrap().unwrap()
    }

    fn erase(&self) -> String {
        format!("--erase {} --cable-only --remote no", devices::make_id("image", 256 << 20))
    }

    /// The image still has nothing at its start: no partition table, no wipe.
    fn untouched(&self) -> bool {
        let mut b = vec![0u8; 1 << 20];
        std::fs::File::open(&self.img).unwrap().read_exact(&mut b).unwrap();
        b.iter().all(|&x| x == 0)
    }
}

#[test]
fn a_batch_run_from_an_assets_folder() {
    let r = release("batch", None);
    // The wrong id: refused before anything is written.
    assert!(crate::flow::run(r.opts(&r.host(), "--erase sdb-0.3G-0000 --cable-only --remote no")).unwrap_err().contains("refused"));
    let id = devices::make_id("image", 256 << 20);
    let psk = r.s.0.join("psk");
    crate::flow::run(r.opts(&r.host(), &format!("--erase {id} --remote yes --wifi Lab --wifi-psk-file {} --extra-config desktop=on", psk.display()))).unwrap();
    let mut f = devices::open_image(&r.img, None).unwrap();
    let v = kernel_view(&mut f);
    let cfg = String::from_utf8(kernel_read(&v, &[b"BITSOS2", b"CONFIG.TXT"]).unwrap()).unwrap();
    assert!(cfg.contains("\nmcp=service\n") && cfg.contains("\nwlan_ssid=Lab\nwlan_psk=a long passphrase\n") && cfg.contains("\ndesktop=on\n"), "{cfg}");
    let stick_id = String::from_utf8(kernel_read(&v, &[b"BITSOS2", b"STICK.ID"]).unwrap()).unwrap();
    let host = r.host().join("sticks").join(&stick_id);
    let token = std::fs::read_to_string(host.join("token.txt")).unwrap();
    assert!(cfg.contains(&format!("\ntoken={token}\n")));
    assert_eq!(std::fs::read_to_string(host.join("stick.id")).unwrap(), stick_id);
    // A tampered manifest stops the run before the stick.
    std::fs::write(r.assets.join("manifest.json"), r.text.replace("0.9.0\"", "0.9.1\"")).unwrap();
    assert!(crate::flow::run(r.opts(&r.host(), &format!("--update {id}"))).unwrap_err().contains("SHA-256"));
}

#[test]
fn a_tree_asked_for_that_cannot_come_stops_the_batch_run() {
    let r = release("missing", None);
    let e = crate::flow::run(r.opts(&r.host(), &format!("{} --brave yes", r.erase()))).unwrap_err();
    assert!(e.contains("Nothing was written") && e.contains("--allow-missing"), "{e}");
    assert!(r.untouched());
    crate::flow::run(r.opts(&r.host(), &format!("{} --brave yes --allow-missing", r.erase()))).unwrap();
    assert!(!r.untouched());
}

#[test]
fn a_stick_too_small_is_refused_before_the_downloads() {
    let r = release("small", Some(1 << 30));
    let e = crate::flow::run(r.opts(&r.host(), &r.erase())).unwrap_err();
    assert!(e.contains("Nothing was written") && e.contains("bigger stick"), "{e}");
    assert!(r.untouched());
}

#[test]
fn the_host_keeps_the_stick_before_it_is_written() {
    let r = release("host", None);
    let blocked = r.s.0.join("a-file");
    std::fs::write(&blocked, b"not a folder").unwrap();
    let e = crate::flow::run(r.opts(&blocked.join("bitsos"), &r.erase().replace("--remote no", "--remote yes"))).unwrap_err();
    assert!(e.contains("Nothing was written") && e.contains("--host-dir"), "{e}");
    assert!(r.untouched(), "the stick is not written when its token could not be kept");
}

#[test]
fn brave_comes_from_its_maker_and_a_changed_file_is_not_installed() {
    let r = release_with("brave", None, true);
    let vendor = r.s.0.join("vendor");
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::write(vendor.join("brave-bin"), b"brave binarY").unwrap();
    let flags = |extra: &str| format!("{} --brave yes --offline --vendor-cache {} {extra}", r.erase(), vendor.display());
    // Not the file BitsOS was tested with: the run stops before the stick.
    assert!(crate::flow::run(r.opts(&r.host(), &flags(""))).unwrap_err().contains("Nothing was written"));
    assert!(r.untouched());
    std::fs::write(vendor.join("brave-bin"), BRAVE_BIN).unwrap();
    crate::flow::run(r.opts(&r.host(), &flags(""))).unwrap();
    let mut f = devices::open_image(&r.img, None).unwrap();
    let v = kernel_view(&mut f);
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"brave", b"brave"]).unwrap(), BRAVE_BIN);
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"brave", b"lib", b"libgtk-3.so.0"]).unwrap(), b"gtk");
    assert!(kernel_read(&v, &[b"BITSOS2", b"brave", b"VENDOR.txt"]).is_none(), "the list stays on the host");
}
