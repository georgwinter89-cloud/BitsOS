//! Fixture Steam roots written by the tests (no Steam files in git) and a reader that records what it opens.
#![allow(dead_code)]

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use steamlib::{Fs, RealFs};

pub fn fresh(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("steamlib").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn put(path: &Path, text: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub fn manifest(appid: u32, name: &str, flags: u32, extra: &str) -> String {
    format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"universe\"\t\t\"1\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\t\"{flags}\"\n\
         \t\"installdir\"\t\t\"{name}\"\n\t\"LastUpdated\"\t\t\"1730000000\"\n\t\"LastPlayed\"\t\t\"1730500000\"\n\
         \t\"SizeOnDisk\"\t\t\"123456789\"\n\t\"InstalledDepots\"\n\t{{\n\t\t\"{d}\"\n\t\t{{\n\t\t\t\"manifest\"\t\t\"42\"\n\t\t}}\n\t}}\n{extra}}}\n",
        d = appid + 1
    )
}

#[derive(Default)]
pub struct Recorder {
    pub reads: RefCell<Vec<PathBuf>>,
    pub lists: RefCell<Vec<PathBuf>>,
}

impl Fs for Recorder {
    fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>> {
        self.reads.borrow_mut().push(path.to_path_buf());
        RealFs.read(path, max)
    }
    fn list(&self, dir: &Path, keep: &dyn Fn(&str) -> bool, max: usize) -> io::Result<Vec<String>> {
        self.lists.borrow_mut().push(dir.to_path_buf());
        RealFs.list(dir, keep, max)
    }
}
