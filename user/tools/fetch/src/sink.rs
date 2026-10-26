//! Where the body goes: stdout, a device or other non-regular file as it is, or a regular file through a
//! staging file next to it (synced and renamed only when the download is complete and the hash matched, so a
//! script never sees half a file under the final name). The staging file is made with O_EXCL: `FILE.part`, or
//! `FILE.part.N` when that exists (a parallel run to the same FILE, or one killed earlier), so two runs never
//! write into the same file. A symlink FILE is written through to its target, as wget -O and curl -o do.

use crate::{Code, Fail, Res};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub enum Sink {
    Stdout(io::Stdout),
    Direct(File, String),
    Part { f: File, part: PathBuf, dest: String },
}

impl Sink {
    pub fn open(out: &str) -> Res<Sink> {
        if out == "-" {
            return Ok(Sink::Stdout(io::stdout()));
        }
        let wfail = |e: io::Error, p: &str| Fail(Code::Write, format!("{p}: {e}"));
        if fs::metadata(out).is_ok_and(|m| !m.is_file()) {
            let f = OpenOptions::new().write(true).truncate(true).open(out).map_err(|e| wfail(e, out))?;
            return Ok(Sink::Direct(f, out.to_string()));
        }
        let dest = resolve(Path::new(out));
        let dest_s = dest.to_string_lossy().into_owned();
        let mut n = 0u64;
        loop {
            let part = PathBuf::from(if n == 0 { format!("{dest_s}.part") } else { format!("{dest_s}.part.{n}") });
            match OpenOptions::new().write(true).create_new(true).open(&part) {
                Ok(f) => return Ok(Sink::Part { f, part, dest: dest_s }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(wfail(e, &part.to_string_lossy())),
            }
        }
    }

}

/// Follows `p` while it is a symlink (also a dangling one, whose target is then created), so the rename replaces
/// the target and the link stays. Linux gives up after 40 links (ELOOP, linux-6.6/include/linux/namei.h:13); so
/// does this, keeping the last path, which then fails or is replaced like any other file.
fn resolve(p: &Path) -> PathBuf {
    let mut p = p.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&p) {
            Ok(t) if t.is_absolute() => p = t,
            Ok(t) => p = p.parent().map_or(t.clone(), |d| d.join(&t)),
            Err(_) => break,
        }
    }
    p
}

impl Sink {
    pub fn is_stdout(&self) -> bool {
        matches!(self, Sink::Stdout(_))
    }

    pub fn write(&mut self, b: &[u8]) -> Res<()> {
        let (r, name) = match self {
            Sink::Stdout(s) => (s.write_all(b), "stdout".into()),
            Sink::Direct(f, n) => (f.write_all(b), n.clone()),
            Sink::Part { f, part, .. } => (f.write_all(b), part.to_string_lossy().into_owned()),
        };
        r.map_err(|e| Fail(Code::Write, format!("writing {name}: {e}")))
    }

    /// After a failure: the partial file goes away (stdout and devices keep what they got).
    pub fn discard(self) {
        if let Sink::Part { f, part, .. } = self {
            drop(f);
            let _ = fs::remove_file(part);
        }
    }

    pub fn commit(self) -> Res<()> {
        match self {
            Sink::Stdout(mut s) => s.flush().map_err(|e| Fail(Code::Write, format!("writing stdout: {e}"))),
            Sink::Direct(..) => Ok(()),
            Sink::Part { f, part, dest } => {
                if let Err(e) = f.sync_all() {
                    drop(f);
                    let _ = fs::remove_file(&part);
                    return Err(Fail(Code::Write, format!("syncing {}: {e}", part.display())));
                }
                drop(f);
                fs::rename(&part, &dest).map_err(|e| {
                    let _ = fs::remove_file(&part);
                    Fail(Code::Write, format!("renaming {} to {dest}: {e}", part.display()))
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fetch-sink-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parallel_runs_get_their_own_staging_file() {
        let d = dir("excl");
        let out = d.join("f").to_string_lossy().into_owned();
        let (mut a, mut b) = (Sink::open(&out).unwrap(), Sink::open(&out).unwrap());
        a.write(b"aaaa").unwrap();
        b.write(b"bb").unwrap();
        a.commit().unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"aaaa");
        b.commit().unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"bb");
        let s = Sink::open(&out).unwrap();
        s.discard();
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1, "only f is left");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn symlink_is_written_through() {
        let d = dir("link");
        fs::write(d.join("target"), b"old").unwrap();
        std::os::unix::fs::symlink("target", d.join("link")).unwrap();
        std::os::unix::fs::symlink("new", d.join("dangling")).unwrap();
        for (name, real) in [("link", "target"), ("dangling", "new")] {
            let mut s = Sink::open(&d.join(name).to_string_lossy()).unwrap();
            s.write(b"fresh").unwrap();
            s.commit().unwrap();
            assert!(fs::symlink_metadata(d.join(name)).unwrap().file_type().is_symlink(), "{name} is still a link");
            assert_eq!(fs::read(d.join(real)).unwrap(), b"fresh");
        }
        fs::remove_dir_all(&d).unwrap();
    }
}
