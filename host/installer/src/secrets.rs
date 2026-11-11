//! Random identities and the host's copy of them: the stick's `STICK.ID` and, with remote control, its token, both
//! from the operating system's random source, kept in the user's config folder (files 0600, folder 0700 on Unix;
//! on Windows an access list naming the user alone).

use std::path::{Path, PathBuf};

pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).map_err(|e| format!("the system's random source failed: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// A fresh stick identity: 32 hex digits (bitsboot and the kernel compare it with the boot data).
pub fn new_stick_id() -> Result<String, String> {
    random_hex(16)
}

/// A remote-control token: 64 hex digits (CONFIG allows 8 to 127 printable characters).
pub fn new_token() -> Result<String, String> {
    random_hex(32)
}

pub fn is_stick_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<config>/bitsos`: $XDG_CONFIG_HOME or ~/.config on Unix (the invoking user's under sudo), %APPDATA% on Windows.
pub fn default_host_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("BitsOS"))
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = sudo_home() {
            return Some(home.join(".config/bitsos"));
        }
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
            return Some(PathBuf::from(x).join("bitsos"));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/bitsos"))
    }
}

/// Under sudo the files belong to the person who called it, not to root.
#[cfg(not(windows))]
fn sudo_home() -> Option<PathBuf> {
    let user = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty() && u != "root")?;
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 7 && f[0] == user).then(|| PathBuf::from(f[5]))
    })
}

#[cfg(unix)]
fn owner_fix(p: &Path) {
    use std::os::unix::fs::chown;
    let id = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
    if let (Some(u), Some(g)) = (id("SUDO_UID"), id("SUDO_GID")) {
        let _ = chown(p, Some(u), Some(g));
    }
}
/// APPDATA is the user's already, but SYSTEM and the administrators may read it too; the token is for the user
/// alone. A failure leaves the parent folder's protection and says so.
#[cfg(windows)]
fn owner_fix(p: &Path) {
    if let Err(e) = crate::devices::windows::user::restrict_to_user(p) {
        println!("note: {} keeps the access of the folder it is in ({e})", p.display());
    }
}
#[cfg(not(any(unix, windows)))]
fn owner_fix(_: &Path) {}

fn private_dir(d: &Path) -> Result<(), String> {
    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    owner_fix(d);
    Ok(())
}

/// Writes `text` to `p`, readable by its owner only (created 0600, never world-readable for a moment).
pub fn write_private(p: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    f.write_all(text.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", p.display()))?;
    owner_fix(p);
    Ok(())
}

/// The host's record of one stick: `<dir>/sticks/<id>/stick.id`, and `token.txt` with remote control. Returns the
/// stick's folder.
pub fn save(host_dir: &Path, stick_id: &str, token: Option<&str>) -> Result<PathBuf, String> {
    if !is_stick_id(stick_id) {
        return Err("internal: not a stick id".into());
    }
    private_dir(host_dir)?;
    private_dir(&host_dir.join("sticks"))?;
    let d = host_dir.join("sticks").join(stick_id);
    private_dir(&d)?;
    write_private(&d.join("stick.id"), stick_id)?;
    if let Some(t) = token {
        write_private(&d.join("token.txt"), t)?;
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_tokens_are_fresh_hex() {
        let (a, b) = (new_stick_id().unwrap(), new_stick_id().unwrap());
        assert!(is_stick_id(&a) && a != b);
        let t = new_token().unwrap();
        assert!(t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[cfg(unix)]
    #[test]
    fn saved_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("bi-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let id = new_stick_id().unwrap();
        let s = save(&d, &id, Some("tok-tok-tok")).unwrap();
        for (f, want) in [("stick.id", id.as_str()), ("token.txt", "tok-tok-tok")] {
            let p = s.join(f);
            assert_eq!(std::fs::read_to_string(&p).unwrap(), want);
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{f}");
        }
        assert_eq!(std::fs::metadata(&s).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&d);
    }
}
