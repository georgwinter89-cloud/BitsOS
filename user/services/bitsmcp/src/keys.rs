//! The service's keys on the stick, `\BITSOS2\mcp\` (`/boot/BITSOS2/mcp`; never in the repository): read, and
//! made at the first start, while the service is still root and before the seccomp filter closes the file
//! system (MC-8a decision 8). Layout:
//! - `server.key` (PKCS#8 DER) and `server.crt` (DER, self-signed): the Mini-PC's key, made once and kept; a
//!   pair that does not fit (a half-written first start, a damaged file) is made anew and the old certificate
//!   is kept as `server.bad`, so a host that pinned it refuses the new one loudly (`host/mcp/mcp-call.mjs`).
//! - `paired\*.der`: the paired clients' certificates (DER). `paired\first.der` is the one-time pairing's file:
//!   while no client is paired it is opened (empty) before the filter, and the first `bitsos/pair` writes into
//!   that descriptor; later pairings (MC-9's confirmation) add files of their own.

use crate::cert;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Linux's EROFS: the stick is read-only (until its identity is decided, or for good: CONFIG.TXT stick_write).
const EROFS: i32 = 30;

/// Where the keys live on the Mini-PC, and the stick's mount point.
pub const DIR: &str = "/boot/BITSOS2/mcp";
pub const MOUNT: &str = "/boot";
/// How long a start waits for the stick (mounted, then writable) before it serves without what it lacks: the
/// stick is mounted shortly after init starts the service and is writable about 2 s later (fs-fat's rule).
pub const STICK_WAIT: Duration = Duration::from_secs(8);
pub const SERVER_KEY: &str = "server.key";
pub const SERVER_CERT: &str = "server.crt";
pub const SERVER_BAD: &str = "server.bad";
pub const PAIRED: &str = "paired";
pub const FIRST: &str = "first.der";
/// The common name in the Mini-PC's certificate.
pub const SERVER_CN: &str = "bitsmcp";

/// What the start found or made.
pub struct Keys {
    /// The server's key (PKCS#8 DER) and certificate (DER).
    pub key: Vec<u8>,
    pub cert: Vec<u8>,
    /// True when this start made the key.
    pub made: bool,
    /// The paired clients' certificates.
    pub paired: Vec<Vec<u8>>,
    /// While none is paired: `paired\first.der`, open for the one-time pairing (None: it could not be opened).
    pub slot: Option<File>,
}

/// Created files' modes (open(2) applies them at creation only; FAT shows its own, see the note's change
/// request for fs-fat): the private key for root alone, certificates readable, the directories root's.
const KEY_MODE: u32 = 0o600;
const CERT_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o700;

fn create(path: &Path, mode: u32) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(path)
}

fn write_synced(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    let mut f = create(path, mode)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Why the keys could not be read or made. `unpaired`: it is certain that no client is paired on this stick (no
/// paired directory, or one without any certificate), so plain HTTP with the token is the service's state before
/// the pairing anyway; otherwise a pairing may exist and the service must not fall back to plain HTTP.
#[derive(Debug)]
pub struct OpenError {
    pub text: String,
    pub unpaired: bool,
}

struct Fail {
    text: String,
    read_only: bool,
    unpaired: bool,
}

impl Fail {
    fn unpaired(mut self, yes: bool) -> Fail {
        self.unpaired = yes;
        self
    }
    fn public(self) -> OpenError {
        OpenError { text: self.text, unpaired: self.unpaired }
    }
}

fn fail(p: &Path, e: std::io::Error) -> Fail {
    Fail { text: format!("{}: {e}", p.display()), read_only: e.raw_os_error() == Some(EROFS), unpaired: false }
}

impl From<String> for Fail {
    fn from(text: String) -> Fail {
        Fail { text, read_only: false, unpaired: false }
    }
}

/// The server's key and certificate: read when they fit together, made otherwise. Files are only written when no
/// fitting pair is there, so a key a host pinned (it was served, so it fitted) is never overwritten by this.
fn server_pair(dir: &Path, rng: &dyn ring::rand::SecureRandom) -> Result<(Vec<u8>, Vec<u8>, bool), Fail> {
    let (kp, cp) = (dir.join(SERVER_KEY), dir.join(SERVER_CERT));
    let (key, crt) = (fs::read(&kp), fs::read(&cp));
    if let (Ok(k), Ok(c)) = (&key, &crt)
        && cert::key_fits(k, c, rng)
    {
        return Ok((k.clone(), c.clone(), false));
    }
    if key.is_ok() || crt.is_ok() {
        // B6: the old key is not taken; whoever pinned it is refused until paired again.
        let old = crt.as_ref().ok().filter(|c| cert::is_certificate(c)).map(|c| cert::fingerprint(c)).unwrap_or_else(|| String::from("unreadable"));
        crate::log::warn(&format!("{}: {SERVER_KEY} and {SERVER_CERT} do not fit together (old certificate {old}); making a new key", dir.display()));
        if let Ok(c) = &crt {
            let _ = write_synced(&dir.join(SERVER_BAD), c, CERT_MODE);
        }
    }
    let (k, c) = cert::generate(rng, SERVER_CN)?;
    // The key first: a start that ends between the two writes finds a pair that does not fit and makes it anew
    // (that key was never served, so no host pinned it).
    write_synced(&kp, &k, KEY_MODE).map_err(|e| fail(&kp, e))?;
    write_synced(&cp, &c, CERT_MODE).map_err(|e| fail(&cp, e))?;
    Ok((k, c, true))
}

/// Reads the keys below `dir` (making the server's key at the first start). Err: no TLS this start; the service
/// serves plain HTTP with the token (as MC-8a) only when `unpaired` says no client can be paired.
pub fn open(dir: &Path) -> Result<Keys, OpenError> {
    open_with(dir, false).map_err(Fail::public)
}

/// `open` for the stick at `mount`: waits until it is mounted (the service may start before) and, while it is
/// still read-only, until it can be written, both within `budget`; then takes what it can (an existing key and
/// pairing are read from a read-only stick; the first start's key and the one-time pairing need it writable).
/// A stick that is not mounted within `budget` may hold a pairing: that error is not `unpaired`.
pub fn open_on_stick(dir: &Path, mount: &Path, budget: Duration) -> Result<Keys, OpenError> {
    use std::os::unix::fs::MetadataExt;
    let until = Instant::now() + budget;
    let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
    while dev(mount).is_none() || dev(mount) == dev(Path::new("/")) {
        if Instant::now() >= until {
            return Err(OpenError { text: format!("the stick is not mounted at {} after {} s", mount.display(), budget.as_secs()), unpaired: false });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut waited = false;
    loop {
        match open_with(dir, true) {
            Ok(k) => return Ok(k),
            Err(f) if f.read_only && Instant::now() < until => {
                if !waited {
                    crate::log::info(&format!("{}: the stick is read-only so far; waiting up to {} s", f.text, budget.as_secs()));
                    waited = true;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(f) if f.read_only => return open(dir),
            Err(f) => return Err(f.public()),
        }
    }
}

/// `strict`: a slot that cannot be opened because the stick is read-only is an error (to wait for it).
fn open_with(dir: &Path, strict: bool) -> Result<Keys, Fail> {
    use std::os::unix::fs::DirBuilderExt;
    let rng = ring::rand::SystemRandom::new();
    let pdir: PathBuf = dir.join(PAIRED);
    if let Err(e) = fs::DirBuilder::new().recursive(true).mode(DIR_MODE).create(&pdir) {
        // No paired directory: no client can be paired (a read-only stick before the first start, say).
        let none = matches!(fs::metadata(&pdir), Err(m) if m.kind() == std::io::ErrorKind::NotFound);
        return Err(fail(&pdir, e).unpaired(none));
    }
    let mut names: Vec<(String, PathBuf)> = fs::read_dir(&pdir)
        .map_err(|e| fail(&pdir, e))?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(n, _)| n.to_ascii_lowercase().ends_with(".der"))
        .collect();
    names.sort();
    let mut found: Vec<Vec<u8>> = Vec::new();
    let mut junk_first = false;
    for (n, p) in names {
        match fs::read(&p) {
            Ok(d) if cert::is_certificate(&d) => {
                if !found.contains(&d) {
                    found.push(d);
                }
            }
            Ok(d) if d.is_empty() && n.eq_ignore_ascii_case(FIRST) => {}
            Ok(_) => {
                crate::log::warn(&format!("{}: no certificate; not paired", p.display()));
                junk_first |= n.eq_ignore_ascii_case(FIRST);
            }
            // A file that cannot be read may be a pairing: no plain fallback, no slot over it.
            Err(e) => return Err(fail(&p, e)),
        }
    }
    let (key, cert, made) = server_pair(dir, &rng).map_err(|f| f.unpaired(found.is_empty()))?;
    let paired: Vec<Vec<u8>> = found.into_iter().filter(|d| *d != cert).collect();
    let slot = if paired.is_empty() {
        let p = pdir.join(FIRST);
        // B6: a first.der that holds no certificate (a pairing whose write failed) is kept as first.bad, never
        // truncated without a trace.
        let moved = if junk_first {
            let bad = pdir.join("first.bad");
            match fs::rename(&p, &bad) {
                Ok(()) => {
                    crate::log::warn(&format!("{}: no certificate in it; kept as {} and the one-time pairing opened again", p.display(), bad.display()));
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else {
            Ok(())
        };
        match moved.and_then(|()| create(&p, CERT_MODE)) {
            Ok(f) => Some(f),
            Err(e) if strict && e.raw_os_error() == Some(EROFS) => return Err(fail(&p, e)),
            Err(e) => {
                crate::log::warn(&format!("{}: {e}; no pairing possible this start", p.display()));
                None
            }
        }
    } else {
        None
    };
    Ok(Keys { key, cert, made, paired, slot })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitsmcp-keys-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn made_once_then_kept() {
        let d = tmp("kept");
        let a = open(&d).unwrap();
        assert!(a.made && a.paired.is_empty() && a.slot.is_some());
        let b = open(&d).unwrap();
        assert!(!b.made);
        assert_eq!(a.cert, b.cert, "the same key at the next start");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_key_is_made_anew_and_the_old_certificate_kept() {
        let d = tmp("damaged");
        let a = open(&d).unwrap();
        fs::write(d.join(SERVER_KEY), b"broken").unwrap();
        let b = open(&d).unwrap();
        assert!(b.made && b.cert != a.cert);
        assert_eq!(fs::read(d.join(SERVER_BAD)).unwrap(), a.cert);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_private_key_is_created_for_root_alone() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("modes");
        open(&d).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        // At most these (the umask may take more away).
        assert_eq!(mode(&d.join(SERVER_KEY)) & !0o600, 0);
        assert_eq!(mode(&d.join(SERVER_CERT)) & !0o644, 0);
        assert_eq!(mode(&d.join(PAIRED)) & !0o700, 0);
        assert_eq!(mode(&d) & !0o700, 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_half_written_first_pairing_is_kept_aside() {
        let d = tmp("half");
        drop(open(&d).unwrap());
        fs::write(d.join(PAIRED).join(FIRST), [0x30, 0x82, 0x01]).unwrap();
        let k = open(&d).unwrap();
        assert!(k.paired.is_empty() && k.slot.is_some());
        assert_eq!(fs::read(d.join(PAIRED).join("first.bad")).unwrap(), [0x30, 0x82, 0x01]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_key_directory_is_unpaired_an_unreadable_pairing_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("unpaired");
        fs::write(&d, b"a file where the directory should be").unwrap();
        assert!(!open(&d).err().unwrap().unpaired, "a path that is no directory proves nothing");
        fs::remove_file(&d).unwrap();
        // A directory that cannot be written and has no paired directory: no client can be paired.
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::create_dir(d.join("probe")).is_err() {
            assert!(open(&d).err().unwrap().unpaired);
        }
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        // A paired file that cannot be read may be a pairing (as root every file can be read: checked otherwise).
        drop(open(&d).unwrap());
        let f = d.join(PAIRED).join("second.der");
        fs::write(&f, b"x").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0)).unwrap();
        if fs::read(&f).is_err() {
            assert!(!open(&d).err().unwrap().unpaired);
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_first_pairing_counts_at_the_next_start() {
        let d = tmp("first");
        let mut a = open(&d).unwrap();
        let rng = ring::rand::SystemRandom::new();
        let (_, client) = cert::generate(&rng, "desk").unwrap();
        a.slot.take().unwrap().write_all(&client).unwrap();
        fs::write(d.join(PAIRED).join("junk.der"), b"no certificate").unwrap();
        let b = open(&d).unwrap();
        assert_eq!(b.paired, vec![client]);
        assert!(b.slot.is_none(), "no second one-time pairing");
        let _ = fs::remove_dir_all(&d);
    }
}
