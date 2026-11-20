//! The protocol of BitsOS's own windowed programs (opt-in, a side channel keyed by pid): a program writes its
//! elements as JSON to `$XDG_RUNTIME_DIR/bitsos-ui/<pid>.json` whenever they change (written whole, then renamed),
//! `{"elements":[E...]}` with C1 elements whose rects are in the window's own surface coordinates (logical px).
//! bitscomp reads the file of a window's client pid when it is asked for that window's list, only when the file
//! belongs to the same user as the process, and turns the rects into screen pixels.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Element;

pub const OWN_DIR: &str = "bitsos-ui";
/// The largest file read (a program with thousands of elements is cut off, never read without bound).
pub const OWN_MAX: u64 = 256 * 1024;
/// Most elements and nesting taken from one file.
const MAX_ELEMENTS: usize = 2000;
const MAX_DEPTH: usize = 16;

#[derive(Serialize, Deserialize)]
struct File {
    elements: Vec<Element>,
}

pub fn own_file(runtime_dir: &Path, pid: u32) -> PathBuf {
    runtime_dir.join(OWN_DIR).join(format!("{pid}.json"))
}

fn count(v: &[Element], depth: usize) -> Result<usize, String> {
    if depth > MAX_DEPTH {
        return Err(format!("deeper than {MAX_DEPTH} levels"));
    }
    let mut n = v.len();
    for e in v {
        n += count(&e.children, depth + 1)?;
    }
    Ok(n)
}

/// A program's file: its elements (values of secret ones dropped), or why not.
pub fn parse_own(bytes: &[u8]) -> Result<Vec<Element>, String> {
    if bytes.len() as u64 > OWN_MAX {
        return Err(format!("larger than {OWN_MAX} bytes"));
    }
    let f: File = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let n = count(&f.elements, 0)?;
    if n > MAX_ELEMENTS {
        return Err(format!("{n} elements, more than {MAX_ELEMENTS}"));
    }
    let mut v = f.elements;
    crate::scrub(&mut v);
    Ok(v)
}

/// The program's side: writes its elements (0600, whole, then renamed so bitscomp never reads half a file).
pub fn write_own(runtime_dir: &Path, pid: u32, elements: &[Element]) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = runtime_dir.join(OWN_DIR);
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    let mut v = elements.to_vec();
    crate::scrub(&mut v);
    let text = serde_json::to_vec(&File { elements: v }).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(".{pid}.json.new"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(&text)?;
    drop(f);
    std::fs::rename(&tmp, own_file(runtime_dir, pid))
}

/// The program's side at its end.
pub fn remove_own(runtime_dir: &Path, pid: u32) {
    let _ = std::fs::remove_file(own_file(runtime_dir, pid));
}
