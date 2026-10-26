//! The boot stick's rules (R8b), enforced here and not by callers (V2-PLAN §7, V2-LANES FS-7):
//! - writes only below `\BITSOS2\`; the root, `\EFI\` and everything else outside is read-only (EROFS);
//! - `\BITSOS2\STICK.ID` is never written, renamed, removed or created (EPERM);
//! - `\BITSOS2\SYS\` (the system generations and their NEW/TRY/OK/PIN markers) only through the narrow API of
//!   `sysapi` (RB-6, install); any other change there is EPERM;
//! - `\BITSOS2\CONFIG.TXT` may be written, but no write may add or change a line of the keys only Georg sets
//!   (`wlan`, `nvme_write`, `stick_write`, `usb_msd_write`; R8a/R8d; the same rule as MC's file tools);
//! - a name matches a protected one by its long name (case-insensitive, trailing dots ignored) **or** its 8.3
//!   alias, so a Windows alias like `SYS` or a crafted entry cannot reach around a rule (V1 `fs/filealias.asm`).

use alloc::string::String;
use alloc::vec::Vec;
use uapi::errno::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The volume's root directory.
    Root,
    /// Anything outside `\BITSOS2\` (`\EFI\` included).
    Outside,
    /// `\BITSOS2\` itself.
    Bitsos2,
    StickId,
    Config,
    /// `\BITSOS2\SYS\` and everything below it.
    Sys,
    /// Everything else below `\BITSOS2\`.
    Free,
    /// `\BITSOS2\home\` and everything below it: written like `Free`, but owned by the desktop user 1000:1000
    /// with 0700/0600 (YT-2: Cobalt's profile, bitscomp's session).
    Home,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Root => "root",
            Class::Outside => "outside BITSOS2",
            Class::Bitsos2 => "BITSOS2",
            Class::StickId => "STICK.ID",
            Class::Config => "CONFIG.TXT",
            Class::Sys => "SYS",
            Class::Free => "BITSOS2 file",
            Class::Home => "home",
        }
    }
}

/// Whether a component (shown name, 8.3 alias key) is the protected name `p` (upper case).
pub fn is(name: &[u8], alias: &[u8], p: &[u8]) -> bool {
    crate::name::fold(name) == p || alias == p
}

/// The class of a child of a directory of class `parent`, named `name` with alias key `alias`.
pub fn child_class(parent: Class, name: &[u8], alias: &[u8]) -> Class {
    match parent {
        Class::Root => {
            if is(name, alias, b"BITSOS2") {
                Class::Bitsos2
            } else {
                Class::Outside
            }
        }
        Class::Bitsos2 => {
            if is(name, alias, b"STICK.ID") {
                Class::StickId
            } else if is(name, alias, b"SYS") {
                Class::Sys
            } else if is(name, alias, b"HOME") {
                Class::Home
            } else if is(name, alias, b"CONFIG.TXT") {
                Class::Config
            } else {
                Class::Free
            }
        }
        Class::Sys => Class::Sys,
        Class::Home => Class::Home,
        Class::Outside => Class::Outside,
        // Below a file nothing exists; below CONFIG.TXT or STICK.ID as a directory (a stick made elsewhere):
        // treat the subtree like its top.
        c => c,
    }
}

/// The questions the volume asks before a change.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// The volume may be written at all (the identified stick, mounted read-write).
    pub rw: bool,
    /// The stick rules apply (false only for a plain volume in tests).
    pub stick: bool,
    /// The caller holds the SYS grant (`sysapi`).
    pub sys: bool,
}

impl Rules {
    fn base(&self) -> Result<(), i64> {
        if self.rw { Ok(()) } else { Err(EROFS) }
    }
    /// Creating a child of class `child` in a directory of class `dir`.
    pub fn create(&self, dir: Class, child: Class) -> Result<(), i64> {
        self.base()?;
        if !self.stick {
            return Ok(());
        }
        match (dir, child) {
            (Class::Root | Class::Outside, _) => Err(EROFS),
            (_, Class::StickId) => Err(EPERM),
            (Class::Sys, _) | (_, Class::Sys) => self.sys_ok(),
            _ => Ok(()),
        }
    }
    /// Removing (or renaming away) a node of class `c`.
    pub fn remove(&self, c: Class) -> Result<(), i64> {
        self.base()?;
        if !self.stick {
            return Ok(());
        }
        match c {
            Class::Root | Class::Outside | Class::Bitsos2 => Err(EROFS),
            Class::StickId => Err(EPERM),
            Class::Sys => self.sys_ok(),
            Class::Config | Class::Free | Class::Home => Ok(()),
        }
    }
    /// Changing the content (or size, or time) of a node of class `c`.
    pub fn write(&self, c: Class) -> Result<(), i64> {
        self.base()?;
        if !self.stick {
            return Ok(());
        }
        match c {
            Class::Root | Class::Outside | Class::Bitsos2 => Err(EROFS),
            Class::StickId => Err(EPERM),
            Class::Sys => self.sys_ok(),
            Class::Config | Class::Free | Class::Home => Ok(()),
        }
    }
    /// The write-back of content `write` admitted earlier (it runs on the flusher's thread, which never holds
    /// the SYS grant): as `write`, except that SYS content passes. A SYS file's pages get dirty only through
    /// `write_at`/`truncate` under the grant, since opening one for writing (and so a writable shared mapping)
    /// is refused without it.
    pub fn writeback(&self, c: Class) -> Result<(), i64> {
        match c {
            Class::Sys => Rules { sys: true, ..*self }.write(c),
            _ => self.write(c),
        }
    }
    fn sys_ok(&self) -> Result<(), i64> {
        if self.sys { Ok(()) } else { Err(EPERM) }
    }
}

/// Keys only Georg's own edit may add or change (R8a; MC's GUARDED_KEYS). `wlan` is free since 21.10. (R8d).
pub const GUARDED_KEYS: &[&str] = &["nvme_write", "stick_write", "usb_msd_write"];

/// CONFIG.TXT names whose values the file tools never show (the kernel's `config::is_secret`, WL-4 CR 4).
pub const SECRET_KEYS: &[&str] = &["token", "wlan_psk", "claude_api_key"];

/// What a secret's value reads as (read_file/stat), and in a new CONFIG.TXT: "keep the stick's value".
pub const REDACTED: &str = "***";

/// The secret key a CONFIG.TXT line sets, with the byte where its value starts (as the parsers read the name:
/// trimmed, exact; a BOM before it tolerated).
fn secret_line(line: &[u8]) -> Option<(&'static str, usize)> {
    let eq = line.iter().position(|&b| b == b'=')?;
    let name = core::str::from_utf8(&line[..eq]).ok()?;
    let name = name.trim().trim_start_matches('\u{feff}').trim();
    SECRET_KEYS.iter().copied().find(|k| *k == name).map(|k| (k, eq + 1))
}

/// WL-4 CR 4: `text` with every secret line's value replaced by `***` (comments on such a line go too; a `\r`
/// line end stays). Everything else is unchanged.
pub fn redact_config(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for (i, line) in text.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        match secret_line(line) {
            Some((_, at)) => {
                out.extend_from_slice(&line[..at]);
                out.extend_from_slice(REDACTED.as_bytes());
                if line.ends_with(b"\r") {
                    out.push(b'\r');
                }
            }
            None => out.extend_from_slice(line),
        }
    }
    out
}

/// The way back for a CONFIG.TXT built from a redacted read: a secret line whose value is `***` takes the value
/// `old` has for that key (last line wins, as the parsers read it), or goes when `old` has none.
pub fn unredact_config(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(new.len());
    let mut first = true;
    for line in new.split(|&b| b == b'\n') {
        let mut line: Vec<u8> = line.to_vec();
        if let Some((key, at)) = secret_line(&line) {
            let cr = line.ends_with(b"\r");
            let value = &line[at..line.len() - cr as usize];
            if core::str::from_utf8(value).is_ok_and(|v| v.split('#').next().unwrap_or("").trim() == REDACTED) {
                match effective(old, key) {
                    Some(v) => {
                        line.truncate(at);
                        line.extend_from_slice(v.as_bytes());
                        if cr {
                            line.push(b'\r');
                        }
                    }
                    None => continue,
                }
            }
        }
        if !first {
            out.push(b'\n');
        }
        first = false;
        out.extend_from_slice(&line);
    }
    out
}

/// The largest CONFIG.TXT the guard reads (a text of a few lines; above this a write is EFBIG, so a program
/// can never make the kernel hold a 4 GiB copy).
pub const CONFIG_MAX: u64 = 1 << 20;

/// The guarded lines of a CONFIG.TXT text (key lower case, value trimmed), sorted: every line that might be one
/// of the keys, read more loosely than the parsers do (case, a BOM), so nothing slips past as "not a key".
fn guarded_lines(text: &[u8]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = text
        .split(|&b| b == b'\n')
        .filter_map(|l| {
            let l = String::from_utf8_lossy(l);
            let l = l.trim().trim_start_matches('\u{feff}');
            let (k, val) = l.split_once('=')?;
            let k = k.trim().to_ascii_lowercase();
            GUARDED_KEYS.contains(&k.as_str()).then(|| (k, String::from(val.trim())))
        })
        .collect();
    v.sort();
    v
}

/// The value `key` has for the kernel and bitsboot (`kexec::boot::config`, bitsboot `pure::config::get`:
/// `#` starts a comment, spaces around name and value ignored, exact name, the last line wins).
fn effective<'a>(text: &'a [u8], key: &str) -> Option<&'a str> {
    let mut found = None;
    for raw in text.split(|&b| b == b'\n') {
        // A line that is not UTF-8 is skipped by bitsboot and makes the kernel read no key at all.
        let Ok(line) = core::str::from_utf8(raw) else { continue };
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            found = Some(v.trim());
        }
    }
    found
}

/// R8a: the guarded lines of `new` must all be in `old` (dropping one is allowed, adding or changing one is
/// not), and the value each guarded key has for the parsers (last line wins) must stay as it was or go: a
/// reordering or a dropped duplicate may not flip `nvme_write=off` / `nvme_write=on`. Err names the key.
pub fn config_guard(old: &[u8], new: &[u8]) -> Result<(), String> {
    let before = guarded_lines(old);
    for line in guarded_lines(new) {
        if !before.contains(&line) {
            return Err(line.0);
        }
    }
    for &k in GUARDED_KEYS {
        if let Some(v) = effective(new, k)
            && effective(old, k) != Some(v)
        {
            return Err(String::from(k));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn secrets_redacted_and_kept() {
        let old = b"token=abc\nwlan_psk = correct horse # mine\r\nwlan=on\nwlan_pskx=1\n";
        let r = redact_config(old);
        assert_eq!(r, b"token=***\nwlan_psk =***\r\nwlan=on\nwlan_pskx=1\n".to_vec());
        // A file built from the redacted read keeps the stick's values; a new real value wins.
        assert_eq!(unredact_config(old, &r), b"token=abc\nwlan_psk =correct horse\r\nwlan=on\nwlan_pskx=1\n".to_vec());
        assert_eq!(unredact_config(old, b"token=new\nwlan_psk=***\n"), b"token=new\nwlan_psk=correct horse\n".to_vec());
        // Without an old value the redacted line goes.
        assert_eq!(unredact_config(b"", b"token=t\nwlan_psk=***\nx=1"), b"token=t\nx=1".to_vec());
        assert_eq!(redact_config(b""), b"".to_vec());
    }

    #[test]
    fn classes() {
        assert_eq!(child_class(Class::Root, b"bitsos2", b"BITSOS2"), Class::Bitsos2);
        assert_eq!(child_class(Class::Root, b"Something", b"BITSOS2"), Class::Bitsos2, "by alias");
        assert_eq!(child_class(Class::Root, b"EFI", b"EFI"), Class::Outside);
        assert_eq!(child_class(Class::Bitsos2, b"Stick.Id", b"STICK~1.ID"), Class::StickId);
        assert_eq!(child_class(Class::Bitsos2, b"x", b"STICK.ID"), Class::StickId);
        assert_eq!(child_class(Class::Bitsos2, b"sys.", b"SYS"), Class::Sys);
        assert_eq!(child_class(Class::Bitsos2, b"config.txt", b"CONFIG.TXT"), Class::Config);
        assert_eq!(child_class(Class::Bitsos2, b"bin", b"BIN"), Class::Free);
        assert_eq!(child_class(Class::Sys, b"5", b"5"), Class::Sys);
        assert_eq!(child_class(Class::Free, b"CONFIG.TXT", b"CONFIG.TXT"), Class::Free);
        assert_eq!(child_class(Class::Bitsos2, b"home", b"HOME"), Class::Home);
        assert_eq!(child_class(Class::Bitsos2, b"x", b"HOME"), Class::Home, "by alias");
        assert_eq!(child_class(Class::Home, b"SYS", b"SYS"), Class::Home, "inherited");
        assert_eq!(child_class(Class::Free, b"home", b"HOME"), Class::Free, "only directly below BITSOS2");
    }

    #[test]
    fn rules() {
        let r = Rules { rw: true, stick: true, sys: false };
        assert_eq!(r.create(Class::Root, Class::Outside), Err(EROFS));
        assert_eq!(r.create(Class::Outside, Class::Outside), Err(EROFS));
        assert_eq!(r.create(Class::Bitsos2, Class::Free), Ok(()));
        assert_eq!(r.create(Class::Bitsos2, Class::StickId), Err(EPERM));
        assert_eq!(r.create(Class::Bitsos2, Class::Sys), Err(EPERM));
        assert_eq!(r.create(Class::Sys, Class::Sys), Err(EPERM));
        assert_eq!(r.remove(Class::Bitsos2), Err(EROFS));
        assert_eq!(r.remove(Class::StickId), Err(EPERM));
        assert_eq!(r.write(Class::Config), Ok(()));
        assert_eq!(r.create(Class::Home, Class::Home), Ok(()));
        assert_eq!(r.write(Class::Home), Ok(()));
        assert_eq!(r.remove(Class::Home), Ok(()));
        let s = Rules { sys: true, ..r };
        assert_eq!(s.create(Class::Sys, Class::Sys), Ok(()));
        assert_eq!(s.remove(Class::Sys), Ok(()));
        assert_eq!(s.create(Class::Bitsos2, Class::StickId), Err(EPERM), "the grant never opens STICK.ID");
        assert_eq!(s.write(Class::Outside), Err(EROFS));
        let ro = Rules { rw: false, ..s };
        assert_eq!(ro.write(Class::Free), Err(EROFS));
        let plain = Rules { rw: true, stick: false, sys: false };
        assert_eq!(plain.create(Class::Root, Class::Outside), Ok(()));
    }

    #[test]
    fn config() {
        assert!(config_guard(b"nvme_write=on\n", b"nvme_write=on\nscreen=x\n").is_ok());
        assert!(config_guard(b"nvme_write=on\n", b"").is_ok(), "dropping a line is allowed");
        assert_eq!(config_guard(b"", b"nvme_write=on\n"), Err(String::from("nvme_write")));
        assert_eq!(config_guard(b"nvme_write=off\n", b"nvme_write=on\n"), Err(String::from("nvme_write")));
        assert_eq!(config_guard(b"", b" NVME_WRITE = on \r\n"), Err(String::from("nvme_write")));
        assert!(config_guard(b"", b"#nvme_write=on\n").is_ok());
        assert!(config_guard(b"wlan=off\n", b"wlan=on\n").is_ok(), "wlan is no guarded key (R8d, 21.10.)");
        assert_eq!(config_guard(b"x=1\n", "\u{feff}stick_write=on\n".as_bytes()), Err(String::from("stick_write")));
    }

    #[test]
    fn config_evasions() {
        let w = |k: &str| Err(String::from(k));
        // Case, spaces, CRLF, a BOM, a comment after the value: all still the key.
        for k in GUARDED_KEYS {
            for line in [format!("{}=on\n", k.to_ascii_uppercase()), format!("  {k}  =  on  \r\n"), format!("\u{feff}{k}=on"), format!("{k}=on # x\n")] {
                assert_eq!(config_guard(b"screen=1\n", line.as_bytes()), w(k), "{line:?}");
            }
        }
        // Last line wins: reordering or dropping one of two lines may not flip the value.
        assert_eq!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=off\nnvme_write=on\n"), w("nvme_write"));
        assert_eq!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=on\n"), w("nvme_write"));
        assert!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=on\nnvme_write=off\nscreen=2\n").is_ok());
        assert!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=off\n").is_ok(), "the value it had");
        assert!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"").is_ok(), "dropping every line: off by default");
        // A repeated identical line changes nothing.
        assert!(config_guard(b"nvme_write=off\n", b"nvme_write=off\nnvme_write=off\n").is_ok());
        // Commented and unrelated lines stay free.
        assert!(config_guard(b"nvme_write=off\n", b"nvme_write=off\n#nvme_write=on\nscreen=2560x1440\n").is_ok());
        assert!(config_guard(b"nvme_write=off\n", b"nvme_write=off\nnvme_writex=on\n").is_ok());
        // Unicode white space around the name counts for the parsers (str::trim) and here.
        assert_eq!(config_guard(b"nvme_write=on\nnvme_write=off\n", "nvme_write=off\nnvme_write\u{a0}=on\n".as_bytes()), w("nvme_write"));
    }
}
