//! GX-7a: the firmware on the stick (`\BITSOS2\firmware\`, V2 `/boot/BITSOS2/firmware`): the files each driver's
//! `firmware.list` names (amdgpu's GPU microcode, rtw89's WLAN firmware), from the host's linux-firmware packages
//! (`/usr/lib/firmware/<name>.zst`, symlinks followed: several 780M names link to other versions' files),
//! decompressed with the host's `zstd` (BitsOS has no zstd decoder), and each package's licence beside them.
//! Every list lands in the same tree: pushing that tree replaces the stick's whole firmware folder, so a
//! driver whose firmware is not listed here loses it (27.10.: the WLAN firmware went that way).
//! Never into git (public repository). A host without a package or zstd gets a note and an image without
//! that firmware (the `gx.fw.stick` ktest then skips); V2_FW_STRICT=1 makes that an error.

use std::path::Path;
use std::process::Command;

pub const LIST: &str = "crates/drivers/amdgpu/firmware.list";
const HOST_DIR: &str = "/usr/lib/firmware";

/// One driver's firmware: its list, the folder its names start with, and the licence file shipped beside them.
struct Source {
    list: &'static str,
    prefix: &'static str,
    licence: &'static str,
}

const SOURCES: &[Source] = &[
    Source { list: LIST, prefix: "amdgpu/", licence: "/usr/share/licenses/linux-firmware-amdgpu/LICENSE.amdgpu" },
    Source {
        list: "crates/drivers/rtw89/firmware.list",
        prefix: "rtw89/",
        licence: "/usr/share/licenses/linux-firmware-realtek/LICENCE.rtlwifi_firmware.txt",
    },
];

/// Writes the decompressed files of every source below `esp/BITSOS2/firmware/` and each licence there.
pub fn copy(root: &Path, esp: &Path) -> Result<(), String> {
    for src in SOURCES {
        copy_one(root, esp, src)?;
    }
    Ok(())
}

fn copy_one(root: &Path, esp: &Path, s: &Source) -> Result<(), String> {
    let strict = std::env::var("V2_FW_STRICT").is_ok_and(|v| v == "1");
    let note = |m: String| -> Result<(), String> {
        if strict {
            return Err(m);
        }
        println!("note:     {m}");
        Ok(())
    };
    // A root without the list (xtask's own image test builds from a scratch root) wants no firmware.
    let list = match std::fs::read_to_string(root.join(s.list)) {
        Ok(l) => l,
        Err(e) => return note(format!("{}: {e}: no {} firmware on the image", s.list, s.prefix)),
    };
    let dst = esp.join("BITSOS2/firmware");
    let mut done = 0usize;
    for name in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if name.contains("..") || !name.starts_with(s.prefix) || !name.ends_with(".bin") {
            return Err(format!("{}: bad name {name}", s.list));
        }
        let src = Path::new(HOST_DIR).join(format!("{name}.zst"));
        if !src.exists() {
            note(format!("firmware {} missing on the host (linux-firmware)", src.display()))?;
            continue;
        }
        let out = match Command::new("zstd").arg("-dcf").arg(&src).output() {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => return Err(format!("zstd -dcf {}: {}", src.display(), String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => return note(format!("zstd not runnable ({e}): no firmware on the image")),
        };
        let to = dst.join(name);
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
