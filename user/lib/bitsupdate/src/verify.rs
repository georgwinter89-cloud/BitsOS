//! What is checked before anything is written (Georg 15.11.): the manifest's signature against the desk's key,
//! that the release is the version its name says, that it is newer than every version on this medium (an old,
//! validly signed manifest replayed is refused), that this BitsOS may install it (`min_previous`), and each file's
//! size and SHA-256. Pure: the caller hands the bytes.

use crate::manifest::{Manifest, NOTES};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// This build has no desk key compiled in.
    NoKey,
    /// The signature does not fit the manifest and the desk's key.
    Signature,
    /// The manifest cannot be read (its text says why).
    Manifest(String),
    /// The release is named for another version than its manifest.
    Tag { tag: u32, manifest: u32 },
    /// Not newer than what the medium holds.
    NotNewer { version: u32, newest: u32 },
    /// This BitsOS is older than the manifest's `min_previous`.
    TooOld { need: u32, have: u32 },
    /// A file is missing, too short, too long, or its bytes are not the signed ones.
    File { name: String, what: String },
    /// UPD-4: the release lacks its parts (no MANIFEST2, or one without parts) though it has part assets or this
    /// medium runs with parts already.
    PartsWithheld,
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// Checks the manifest and its signature (`key`: the compiled-in one in BitsOS, a test key in the tests).
pub fn manifest(key: Option<&[u8; 32]>, manifest: &[u8], sig: &[u8], tag: u32, newest: u32, running: u32) -> Result<Manifest, Why> {
    let key = key.ok_or(Why::NoKey)?;
    if !crate::sig::verify(key, manifest, sig) {
        return Err(Why::Signature);
    }
    let m = Manifest::parse(manifest).map_err(Why::Manifest)?;
    if m.version != tag {
        return Err(Why::Tag { tag, manifest: m.version });
    }
    if m.version <= newest {
        return Err(Why::NotNewer { version: m.version, newest });
    }
    if let Some(need) = m.min_previous
        && running < need
    {
        return Err(Why::TooOld { need, have: running });
    }
    Ok(m)
}

/// One downloaded file against its manifest line.
pub fn file(m: &Manifest, name: &str, bytes: &[u8]) -> Result<(), Why> {
    let bad = |what: String| Why::File { name: name.to_string(), what };
    let e = m.entry(name).ok_or_else(|| bad(String::from("not in the manifest")))?;
    if bytes.len() as u64 != e.size {
        return Err(bad(format!("{} bytes, the manifest says {}", bytes.len(), e.size)));
    }
    if sha256(bytes) != e.sha256 {
        return Err(bad(String::from("its SHA-256 is not the signed one")));
    }
    Ok(())
}

/// The release notes as text (the manifest's WASISTNEU.TXT, already checked by `file`).
pub fn notes(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

pub const NOTES_FILE: &str = NOTES;
