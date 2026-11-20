//! GX-7a: the firmware on the stick (`\BITSOS2\firmware\`, V2 `/boot/BITSOS2/firmware`): the files each driver's
//! `firmware.list` names (amdgpu's GPU microcode, rtw89's WLAN firmware), from the host's linux-firmware packages
//! (`/usr/lib/firmware/<name>.zst`, symlinks followed: several 780M names link to other versions' files),
//! decompressed with the host's `zstd` (BitsOS has no zstd decoder), and each package's licence beside them.
//! Every list lands in the same tree: pushing that tree replaces the stick's whole firmware folder, so a
//! driver whose firmware is not listed here loses it (27.10.: the WLAN firmware went that way).
//! Never into git (public repository). A host without a package or zstd gets a note and an image without
//! that firmware (the `gx.fw.stick` ktest then skips); V2_FW_STRICT=1 makes that an error.
//!
//! GP-NET: the Gaming-PC's cable card (rtl8126) wants its firmware before any USB runs, because on that machine the
//! network is the desk's only way in and the stick hangs on a controller nobody has driven yet. V2_FW_INITRD=1 puts
//! the lists marked `early` into the initramfs too (`/usr/lib/firmware/<name>`, where the driver looks first), with
//! their licence. Without the variable every initramfs is byte for byte what it was.

use std::path::Path;
use std::process::Command;

pub const LIST: &str = "crates/drivers/amdgpu/firmware.list";
const HOST_DIR: &str = "/usr/lib/firmware";

/// One driver's firmware: its list, the folder its names start with, what they end in, the licence file shipped
/// beside them, and whether the driver needs it before the stick is readable (`early`, see the module text).
struct Source {
    list: &'static str,
    prefix: &'static str,
    suffix: &'static str,
    licence: &'static str,
    early: bool,
}

const REALTEK_LICENCE: &str = "/usr/share/licenses/linux-firmware-realtek/LICENCE.rtlwifi_firmware.txt";

const SOURCES: &[Source] = &[
    Source { list: LIST, prefix: "amdgpu/", suffix: ".bin", licence: "/usr/share/licenses/linux-firmware-amdgpu/LICENSE.amdgpu", early: false },
    Source { list: "crates/drivers/rtw89/firmware.list", prefix: "rtw89/", suffix: ".bin", licence: REALTEK_LICENCE, early: false },
    // The only licence file linux-firmware-realtek installs; it covers rtl_nic as well (plan/spec/gaming-pc.md section 6).
    Source { list: RTL8126_LIST, prefix: "rtl_nic/", suffix: ".fw", licence: REALTEK_LICENCE, early: true },
];

pub const RTL8126_LIST: &str = "crates/drivers/rtl8126/firmware.list";
/// Where an `early` file sits in the initramfs (merged /usr: `/lib/firmware` leads here too).
const INITRD_DIR: &str = "usr/lib/firmware";

/// GP-GPU1: firmware a stick carries only when asked for, by set name: `cargo xtask image --firmware gaming-pc` (or
/// V2_FW_EXTRA=gaming-pc) for an image's ESP. GP-R2: `cargo xtask stick` writes generations only and never read
/// V2_FW_EXTRA (round 1's stick had none of these files); `cargo xtask stick --firmware gaming-pc` writes the set onto
/// a stick, `--firmware gaming-pc --tree <dir>` makes it a tree for push.mjs --add. The Mini-PC's stick does not
/// carry the RX 9070 XT's files.
const EXTRA: &[(&str, Source)] = &[(
    "gaming-pc",
    Source { list: "crates/drivers/amdgpu/firmware-gaming-pc.list", prefix: "amdgpu/", suffix: ".bin", licence: "/usr/share/licenses/linux-firmware-amdgpu/LICENSE.amdgpu", early: false },
)];

static ASKED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// `--firmware a,b`: the extra sets of this run (checked against the known names).
pub fn ask(sets: &str) -> Result<(), String> {
    let v = parse_sets(sets)?;
    ASKED.set(v).map_err(|_| String::from("--firmware given twice"))
}

pub fn parse_sets(sets: &str) -> Result<Vec<String>, String> {
    let v: Vec<String> = sets.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    match v.iter().find(|s| !EXTRA.iter().any(|(n, _)| n == s)) {
        Some(bad) => Err(format!("no firmware set '{bad}': known: {}", EXTRA.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "))),
        None => Ok(v),
    }
}

/// The sets asked for: the option's, else V2_FW_EXTRA's.
fn asked() -> Result<Vec<String>, String> {
    match ASKED.get() {
        Some(v) => Ok(v.clone()),
        None => parse_sets(&std::env::var("V2_FW_EXTRA").unwrap_or_default()).map_err(|e| format!("V2_FW_EXTRA: {e}")),
    }
}

/// Writes the decompressed files of every source below `esp/BITSOS2/firmware/` and each licence there.
pub fn copy(root: &Path, esp: &Path) -> Result<(), String> {
    for src in SOURCES {
        copy_one(root, esp, src)?;
    }
    for set in asked()? {
        if let Some((_, src)) = EXTRA.iter().find(|(n, _)| *n == set) {
            copy_one(root, esp, src)?;
        }
    }
    Ok(())
}

fn note(m: String) -> Result<(), String> {
    if std::env::var("V2_FW_STRICT").is_ok_and(|v| v == "1") {
        return Err(m);
    }
    println!("note:     {m}");
    Ok(())
}

/// A list's names, checked: below the source's folder, with its ending, nothing that climbs out.
fn names(list_path: &str, text: &str, s: &Source) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for name in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if name.contains("..") || !name.starts_with(s.prefix) || !name.ends_with(s.suffix) {
            return Err(format!("{list_path}: bad name {name}"));
        }
        v.push(name.to_string());
    }
    Ok(v)
}

/// The decompressed files of one source, as (name, bytes); what the host lacks is noted and left out.
fn files(root: &Path, s: &Source) -> Result<Vec<(String, Vec<u8>)>, String> {
    // A root without the list (xtask's own image test builds from a scratch root) wants no firmware.
    let list = match std::fs::read_to_string(root.join(s.list)) {
        Ok(l) => l,
        Err(e) => return note(format!("{}: {e}: no {} firmware on the image", s.list, s.prefix)).map(|()| Vec::new()),
    };
    let mut v = Vec::new();
    for name in names(s.list, &list, s)? {
        let src = Path::new(HOST_DIR).join(format!("{name}.zst"));
        if !src.exists() {
            note(format!("firmware {} missing on the host (linux-firmware)", src.display()))?;
            continue;
        }
        match Command::new("zstd").arg("-dcf").arg(&src).output() {
            Ok(o) if o.status.success() => v.push((name, o.stdout)),
            Ok(o) => return Err(format!("zstd -dcf {}: {}", src.display(), String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => return note(format!("zstd not runnable ({e}): no firmware on the image")).map(|()| v),
        }
    }
    Ok(v)
}

/// The `early` sources' files for the initramfs, each list with its licence; empty unless V2_FW_INITRD=1.
pub fn initrd(root: &Path) -> Result<Vec<crate::cpio::Entry>, String> {
    let mut v = Vec::new();
    if !std::env::var("V2_FW_INITRD").is_ok_and(|x| x == "1") {
        return Ok(v);
    }
    for s in SOURCES.iter().filter(|s| s.early) {
        let found = files(root, s)?;
        if found.is_empty() {
            return Err(format!("V2_FW_INITRD=1: no {} firmware to put into the initramfs (linux-firmware and zstd on this host?)", s.prefix));
        }
        for (name, bytes) in found {
            println!("firmware: {name} ({} bytes) into the initramfs at /{INITRD_DIR}", bytes.len());
            v.push(crate::cpio::file(&format!("{INITRD_DIR}/{name}"), 0o644, bytes));
        }
        // Never ship the files without their licence.
        let licence = std::fs::read(s.licence).map_err(|e| format!("{}: {e}", s.licence))?;
        let file = Path::new(s.licence).file_name().ok_or("licence path without a file name")?.to_string_lossy().into_owned();
        v.push(crate::cpio::file(&format!("{INITRD_DIR}/{}{file}", s.prefix), 0o644, licence));
    }
    Ok(v)
}

/// GP-R2: one file of an extra set as the stick holds it: its path below `\BITSOS2\firmware\` (where the drivers
/// look: crates/drivers/amdgpu/src/gpucore/fwload.rs `STICK_DIR`), its plain bytes and where they came from.
pub struct SetFile {
    pub rel: String,
    pub bytes: Vec<u8>,
    pub from: String,
}

/// Where a set is read from: the host's linux-firmware files and its licences folder (tests give their own).
pub struct FwFrom<'a> {
    pub firmware: &'a Path,
    pub licences: &'a Path,
}

pub fn host() -> FwFrom<'static> {
    FwFrom { firmware: Path::new(HOST_DIR), licences: Path::new(LICENCES) }
}
const LICENCES: &str = "/usr/share/licenses";

/// The files of the extra set `set` (`xtask stick --firmware`), decompressed from the host's linux-firmware (read
/// only), its licence last. All or nothing: a file the host lacks is an error, never a note, so a stick never
/// gets half a set.
pub fn set_files(root: &Path, set: &str, from: &FwFrom) -> Result<Vec<SetFile>, String> {
    let host_dir = from.firmware;
    let (_, s) = EXTRA.iter().find(|(n, _)| *n == set).ok_or_else(|| format!("no firmware set '{set}'"))?;
    let list = std::fs::read_to_string(root.join(s.list)).map_err(|e| format!("{}: {e}", s.list))?;
    let mut v = Vec::new();
    for name in names(s.list, &list, s)? {
        let src = host_dir.join(format!("{name}.zst"));
        if !src.exists() {
            return Err(format!("firmware set {set}: {} is missing on this host (package linux-firmware-amdgpu); nothing was planned", src.display()));
        }
        let o = Command::new("zstd").arg("-dcf").arg(&src).output().map_err(|e| format!("zstd not runnable ({e})"))?;
        if !o.status.success() {
            return Err(format!("zstd -dcf {}: {}", src.display(), String::from_utf8_lossy(&o.stderr).trim()));
        }
        v.push(SetFile { rel: name, bytes: o.stdout, from: src.display().to_string() });
    }
    let file = Path::new(s.licence).file_name().ok_or("licence path without a file name")?.to_string_lossy().into_owned();
    let lic = from.licences.join(Path::new(s.licence).strip_prefix(LICENCES).map_err(|_| "a licence outside /usr/share/licenses")?);
    let bytes = std::fs::read(&lic).map_err(|e| format!("{}: {e} (the set never goes without its licence)", lic.display()))?;
    v.push(SetFile { rel: file, bytes, from: lic.display().to_string() });
    Ok(v)
}

fn copy_one(root: &Path, esp: &Path, s: &Source) -> Result<(), String> {
    let dst = esp.join("BITSOS2/firmware");
    let mut done = 0usize;
    for (name, out) in files(root, s)? {
        let to = dst.join(&name);
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(&to, out).map_err(|e| format!("{}: {e}", to.display()))?;
        done += 1;
    }
    if done > 0 {
        // Never ship the files without their licence.
        let name = Path::new(s.licence).file_name().ok_or("licence path without a file name")?;
        std::fs::copy(s.licence, dst.join(name)).map_err(|e| format!("{}: {e}", s.licence))?;
        println!(
            "firmware: {done} {} file(s) from {HOST_DIR} (decompressed) + {} into BITSOS2/firmware",
            s.prefix.trim_end_matches('/'),
            name.to_string_lossy()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(prefix: &str) -> &'static Source {
        SOURCES.iter().find(|s| s.prefix == prefix).unwrap()
    }

    #[test]
    fn each_list_takes_only_its_own_folder_and_ending() {
        let nic = source("rtl_nic/");
        assert_eq!(names("l", "# the cable card\nrtl_nic/rtl8126a-3.fw\n\n", nic).unwrap(), ["rtl_nic/rtl8126a-3.fw"]);
        for bad in ["rtl_nic/../x.fw", "rtw89/rtl8126a-3.fw", "rtl_nic/rtl8126a-3.bin", "/usr/lib/firmware/rtl_nic/a.fw"] {
            assert!(names("l", bad, nic).is_err(), "{bad}");
        }
        // The lists that were there before keep their rule: `.bin` only.
        assert!(names("l", "amdgpu/x.bin", source("amdgpu/")).is_ok());
        assert!(names("l", "amdgpu/x.fw", source("amdgpu/")).is_err());
        assert!(names("l", "rtw89/rtw8852b_fw-2.bin", source("rtw89/")).is_ok());
    }

    #[test]
    fn only_the_cable_cards_firmware_is_early_and_only_when_asked() {
        let early: Vec<&str> = SOURCES.iter().filter(|s| s.early).map(|s| s.prefix).collect();
        assert_eq!(early, ["rtl_nic/"]);
        // This test's process has no V2_FW_INITRD: the initramfs gets nothing, whatever the root holds.
        if std::env::var("V2_FW_INITRD").is_err() {
            assert!(initrd(Path::new("/nonexistent-root")).unwrap().is_empty());
        }
    }

    #[test]
    fn the_list_in_the_tree_names_the_gaming_pcs_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = std::fs::read_to_string(root.join(RTL8126_LIST)).unwrap();
        assert_eq!(names(RTL8126_LIST, &text, source("rtl_nic/")).unwrap(), ["rtl_nic/rtl8126a-3.fw"]);
    }

    /// GP-R2: a set is read whole (every file of its list, decompressed, the licence last) or not at all.
    #[test]
    fn a_set_is_all_or_nothing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = std::env::temp_dir().join(format!("xtask-fwset-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("amdgpu")).unwrap();
        let list = std::fs::read_to_string(root.join(EXTRA[0].1.list)).unwrap();
        let wanted = names(EXTRA[0].1.list, &list, &EXTRA[0].1).unwrap();
        let (last, first) = wanted.split_last().unwrap();
        for (i, n) in first.iter().enumerate() {
            let plain = dir.join(n);
            std::fs::write(&plain, vec![i as u8; 100 + i]).unwrap();
            assert!(Command::new("zstd").arg("-qf").arg("--rm").arg(&plain).status().unwrap().success());
        }
        let lic = dir.join("licences");
        std::fs::create_dir_all(lic.join("linux-firmware-amdgpu")).unwrap();
        let from = FwFrom { firmware: &dir, licences: &lic };
        let missing = set_files(&root, "gaming-pc", &from).err().unwrap();
        assert!(missing.contains(&format!("{last}.zst")) && missing.contains("nothing was planned"), "{missing}");
        std::fs::write(dir.join(last), b"last").unwrap();
        assert!(Command::new("zstd").arg("-qf").arg("--rm").arg(dir.join(last)).status().unwrap().success());
        let no_licence = set_files(&root, "gaming-pc", &from).err().unwrap();
        assert!(no_licence.contains("LICENSE.amdgpu") && no_licence.contains("never goes without its licence"), "{no_licence}");
        std::fs::write(lic.join("linux-firmware-amdgpu/LICENSE.amdgpu"), b"licence").unwrap();
        let files = set_files(&root, "gaming-pc", &from).unwrap();
        assert_eq!(files.len(), wanted.len() + 1);
        assert_eq!(files[0].bytes, vec![0u8; 100], "decompressed");
        assert_eq!(files.iter().map(|f| f.rel.as_str()).take(wanted.len()).collect::<Vec<_>>(), wanted);
        assert_eq!((files.last().unwrap().rel.as_str(), files.last().unwrap().bytes.as_slice()), ("LICENSE.amdgpu", &b"licence"[..]));
        assert!(set_files(&root, "minipc", &from).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extra_sets_are_named_and_checked() {
        assert_eq!(parse_sets("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_sets(" gaming-pc ,").unwrap(), ["gaming-pc"]);
        assert_eq!(parse_sets("gaming-pc,minipc").unwrap_err(), "no firmware set 'minipc': known: gaming-pc");
        // Every extra list exists, names only its own folder and no file of an always-copied list.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let always = std::fs::read_to_string(root.join(LIST)).unwrap();
        for (name, s) in EXTRA {
            let list = std::fs::read_to_string(root.join(s.list)).unwrap_or_else(|e| panic!("{name}: {}: {e}", s.list));
            let files: Vec<&str> = list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
            assert!(!files.is_empty(), "{name}");
            for f in files {
                assert!(f.starts_with(s.prefix) && f.ends_with(".bin") && !f.contains(".."), "{name}: {f}");
                assert!(!always.lines().any(|l| l.trim() == f), "{name}: {f} is on every stick already");
            }
        }
    }
}
