//! The client side of `/dev/bitssnap` (the kernel's crates/snapshots/src/dev.rs): one command line per write,
//! the answer read back; a refused command is the write's errno.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

pub const DEV: &str = "/dev/bitssnap";

#[derive(Debug)]
pub struct Error {
    pub errno: i32,
    pub what: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self.errno {
            19 => "no /data mount",
            95 => "/data is a plain ext4 image without snapshots",
            30 => "/data is read-only",
            13 | 1 => "only root may take or restore snapshots",
            16 => "busy (frozen, mounted or another snapshot operation)",
            2 => "no such snapshot",
            22 => "not a snapshot of /data, or a bad request",
            28 => "the pool is full",
            110 => "the pool did not answer in time",
            _ => "",
        };
        if why.is_empty() { write!(f, "{}: errno {}", self.what, self.errno) } else { write!(f, "{}: {why} (errno {})", self.what, self.errno) }
    }
}

fn io_err(what: &str, e: std::io::Error) -> Error {
    Error { errno: e.raw_os_error().unwrap_or(5), what: what.into() }
}

pub struct Dev {
    f: File,
}

/// The words of one answer line (`key=value ...`; the label is the last word and has no spaces).
pub fn words(line: &str) -> BTreeMap<String, String> {
    line.split_ascii_whitespace().filter_map(|w| w.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn num(m: &BTreeMap<String, String>, k: &str) -> u64 {
    m.get(k).and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snap {
    pub id: u64,
    pub created_ns: u64,
    pub auto: bool,
    pub mounted: bool,
    /// /data goes back to it at the next start (VS-3).
    pub rollback: bool,
    pub bytes: Option<u64>,
    pub label: String,
}

impl Snap {
    /// `auto`, `before-restore` (taken by a restore), `before-rollback` (kept by a volume rollback) or `manual`.
    pub fn kind(&self) -> &'static str {
        if self.auto {
            "auto"
        } else if self.label == crate::restore::BEFORE_LABEL {
            "before-restore"
        } else if self.label == crate::restore::BEFORE_ROLLBACK {
            "before-rollback"
        } else {
            "manual"
        }
    }
}

/// The kernel's `restore-at` answer (crates/snapshots/src/system.rs `restore_at`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreAt {
    pub ok: bool,
    pub snapshot: u64,
    pub snapshot_ns: u64,
    /// The generation pinned (0: the generation stays, `note` says why).
    pub pin: u32,
    pub running: u32,
    pub next: u32,
    pub replaced: u64,
    pub note: String,
}

/// The text after `key=` up to the end of the line (a last field that may hold spaces).
pub fn rest_after(line: &str, key: &str) -> String {
    let k = format!(" {key}=");
    let l = format!(" {}", line.trim_end());
    l.find(&k).map(|i| l[i + k.len()..].to_string()).unwrap_or_default()
}

pub fn parse_restore_at(a: &str) -> RestoreAt {
    let m = words(a);
    RestoreAt {
        ok: num(&m, "ok") == 1,
        snapshot: num(&m, "snapshot"),
        snapshot_ns: num(&m, "snapshot_ns"),
        pin: num(&m, "pin") as u32,
        running: num(&m, "running") as u32,
        next: num(&m, "next") as u32,
        replaced: num(&m, "replaced"),
        note: rest_after(a, "note"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    pub id: u64,
    pub new: bool,
    pub created_ns: u64,
    pub pause_us: u64,
}

/// `stats`, the fields bitssnap uses (all of them in `raw`).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub free_permille: u64,
    pub utc_offset_min: i64,
    pub wall_ns: u64,
    pub snapshots: u64,
    pub last_pause_us: u64,
    pub max_pause_us: u64,
    /// The pool's size: `chunks` of `chunk_size` bytes.
    pub chunks: u64,
    pub chunk_size: u64,
    pub raw: String,
}

/// The `stats` answer (crates/snapshots/src/lib.rs `stats`).
pub fn parse_stats(raw: &str) -> Stats {
    let m = words(raw);
    Stats {
        free_permille: num(&m, "free_permille"),
        utc_offset_min: m.get("utc_offset_min").and_then(|v| v.parse().ok()).unwrap_or(0),
        wall_ns: num(&m, "wall_ns"),
        snapshots: num(&m, "snapshots"),
        last_pause_us: num(&m, "last_pause_us"),
        max_pause_us: num(&m, "max_pause_us"),
        chunks: num(&m, "chunks"),
        chunk_size: num(&m, "chunk_size"),
        raw: raw.trim().to_string(),
    }
}

impl Dev {
    pub fn open() -> Result<Dev, Error> {
        Dev::open_at(&std::env::var("BITSSNAP_DEV").unwrap_or_else(|_| DEV.into()))
    }

    /// The device at `path` (tests: /dev/null answers every command with nothing).
    pub fn open_at(path: &str) -> Result<Dev, Error> {
        OpenOptions::new().read(true).write(true).open(path).map(|f| Dev { f }).map_err(|e| io_err(path, e))
    }

    /// Runs one command and returns its answer.
    pub fn cmd(&mut self, line: &str) -> Result<String, Error> {
        self.f.write(format!("{line}\n").as_bytes()).map_err(|e| io_err(line, e))?;
        let mut out = String::new();
        self.f.read_to_string(&mut out).map_err(|e| io_err(line, e))?;
        Ok(out)
    }

    pub fn take(&mut self, label: &str, auto: bool, if_changed: bool) -> Result<Taken, Error> {
        let mut c = String::from("take");
        if auto {
            c.push_str(" auto");
        }
        if if_changed {
            c.push_str(" if-changed");
        }
        c.push_str(&format!(" label={label}"));
        let m = words(&self.cmd(&c)?);
        Ok(Taken { id: num(&m, "id"), new: num(&m, "new") == 1, created_ns: num(&m, "created_ns"), pause_us: num(&m, "pause_us") })
    }

    pub fn list(&mut self, sizes: bool) -> Result<Vec<Snap>, Error> {
        let a = self.cmd(if sizes { "list sizes" } else { "list" })?;
        Ok(a.lines()
            .map(words)
            .map(|m| Snap {
                id: num(&m, "id"),
                created_ns: num(&m, "created_ns"),
                auto: num(&m, "auto") == 1,
                mounted: num(&m, "mounted") == 1,
                rollback: num(&m, "rollback") == 1,
                bytes: m.get("bytes").and_then(|b| b.parse().ok()),
                label: m.get("label").cloned().unwrap_or_default(),
            })
            .collect())
    }

    pub fn stats(&mut self) -> Result<Stats, Error> {
        Ok(parse_stats(&self.cmd("stats")?))
    }

    /// One snapshot's own size (`size ID`): what deleting only it frees.
    pub fn size(&mut self, id: u64) -> Result<u64, Error> {
        let a = self.cmd(&format!("size {id}"))?;
        words(&a).get("bytes").and_then(|b| b.parse().ok()).ok_or(Error { errno: 5, what: format!("size {id}: no bytes in {a:?}") })
    }

    pub fn delete(&mut self, id: u64) -> Result<(), Error> {
        self.cmd(&format!("delete {id}")).map(|_| ())
    }

    pub fn mount(&mut self, id: u64) -> Result<String, Error> {
        let a = self.cmd(&format!("mount {id}"))?;
        words(&a).remove("path").ok_or(Error { errno: 5, what: format!("mount {id}: no path in {a:?}") })
    }

    pub fn umount(&mut self, id: u64) -> Result<(), Error> {
        self.cmd(&format!("umount {id}")).map(|_| ())
    }

    /// /data back to snapshot `id` at its next mount; the rollback scheduled before (replaced), if any.
    pub fn rollback(&mut self, id: u64) -> Result<Option<u64>, Error> {
        let m = words(&self.cmd(&format!("rollback {id}"))?);
        Ok(Some(num(&m, "replaced")).filter(|&r| r != 0))
    }

    pub fn rollback_cancel(&mut self) -> Result<Option<u64>, Error> {
        let m = words(&self.cmd("rollback cancel")?);
        Ok(Some(num(&m, "to")).filter(|&r| r != 0))
    }

    pub fn rollback_pending(&mut self) -> Result<Option<u64>, Error> {
        Ok(self.rollback_status()?.0)
    }

    /// (scheduled, the scheduled one this start's mount could not carry out).
    pub fn rollback_status(&mut self) -> Result<(Option<u64>, Option<u64>), Error> {
        let m = words(&self.cmd("rollback")?);
        let id = |k| Some(num(&m, k)).filter(|&r| r != 0);
        Ok((id("pending"), id("not_done")))
    }

    /// "Everything as at `utc_s`" in the kernel (`dry`: the plan only).
    pub fn restore_at(&mut self, utc_s: i64, dry: bool) -> Result<RestoreAt, Error> {
        Ok(parse_restore_at(&self.cmd(&format!("restore-at {utc_s}{}", if dry { " dry" } else { "" }))?))
    }

    /// The undo of this start's last restore-at (the kernel takes back only what it set): Ok((undone, text));
    /// Err(text): a part failed (what is undone stays undone; again retries the rest).
    pub fn restore_at_cancel(&mut self) -> Result<Result<(bool, String), String>, Error> {
        let a = self.cmd("restore-at cancel")?;
        let (m, t) = (words(&a), rest_after(&a, "text"));
        Ok(if num(&m, "failed") == 1 { Err(t) } else { Ok((num(&m, "undone") == 1, t)) })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn answer_words() {
        let m = super::words("id=7 created_ns=12 auto=1 mounted=0 bytes=- label=before-restore");
        assert_eq!(m["id"], "7");
        assert_eq!(m["label"], "before-restore");
        assert_eq!(m["bytes"], "-");
    }

    #[test]
    fn restore_at_line() {
        let src = include_str!("../../../../crates/snapshots/src/dev.rs");
        for k in ["ok=1 dry={} snapshot={} snapshot_ns={} pin={} running={} next={} replaced={} note={}", "ok=0 note={}", "undone={} text={}", "failed=1 text={}", "pending={} not_done={}", "rollback={}"] {
            assert!(src.contains(k), "the kernel's answer has no {k}");
        }
        let r = super::parse_restore_at("ok=1 dry=0 snapshot=12 snapshot_ns=99 pin=1 running=2 next=2 replaced=0 note=generation 1 ran then: pinned\n");
        assert_eq!((r.ok, r.snapshot, r.pin, r.running, r.next, r.replaced), (true, 12, 1, 2, 2, 0));
        assert_eq!(r.note, "generation 1 ran then: pinned");
        let r = super::parse_restore_at("ok=0 note=no snapshot of /data from that time\n");
        assert!(!r.ok && r.note == "no snapshot of /data from that time");
    }

    #[test]
    fn kernel_stats_line() {
        // The key names as crates/snapshots/src/lib.rs `stats` writes them: a rename there turns this red.
        let src = include_str!("../../../../crates/snapshots/src/lib.rs");
        for k in ["chunk_size={}", "chunks={}", "free_permille={}", "utc_offset_min={}", "last_pause_us={}", "max_pause_us={}", "snapshots={}"] {
            assert!(src.contains(k), "the kernel's stats line has no {k}");
        }
        let st = super::parse_stats(
            "pool=data chunk_size=65536 chunks=1024 free=100 pending=20 reserve=8 snapshots=3 dropped=0 free_permille=117 wall_ns=5 utc_offset_min=-120 taken=4 unchanged=1 last_pause_us=900 max_pause_us=1200\n",
        );
        assert_eq!((st.chunks, st.chunk_size, st.free_permille, st.snapshots), (1024, 65536, 117, 3));
        assert_eq!((st.utc_offset_min, st.last_pause_us, st.max_pause_us), (-120, 900, 1200));
    }
}
