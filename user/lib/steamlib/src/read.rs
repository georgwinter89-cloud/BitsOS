//! The only way this crate opens anything of Steam's.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub trait Fs {
    /// The whole file, refused when it is longer than `max` bytes.
    fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>>;
    /// The names in a folder that `keep` accepts, at most `max` of them: a folder of millions of entries costs time, not
    /// memory (which ones are kept then is the folder's order).
    fn list(&self, dir: &Path, keep: &dyn Fn(&str) -> bool, max: usize) -> io::Result<Vec<String>>;
}

pub struct RealFs;

impl Fs for RealFs {
    fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>> {
        // No link (a manifest name must not lead to another file of Steam's), no waiting on a FIFO, only a plain file.
        let f: File = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
        if !f.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a plain file"));
        }
        let mut out = Vec::new();
        f.take(max + 1).read_to_end(&mut out)?;
        if out.len() as u64 > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("longer than {max} bytes")));
        }
        Ok(out)
    }

    fn list(&self, dir: &Path, keep: &dyn Fn(&str) -> bool, max: usize) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir)?.flatten() {
            if out.len() >= max {
                break;
            }
            if let Some(n) = e.file_name().to_str().filter(|n| keep(n)) {
                out.push(n.to_string());
            }
        }
        Ok(out)
    }
}
