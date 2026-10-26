//! `/dev/bitssnap` (root only, 0600): the snapshots of /data for `bitssnap` (user/tools/bitssnap). Each open is a
//! session: a write is one command line, run at once (a failure is the write's errno, as Linux's device
//! ioctls answer), and its answer is read back from the start; the next write replaces it. Text instead of
//! ioctls so the same lines serve the tool, MCP and a person at a root shell (one open for both directions:
//! `exec 3<>/dev/bitssnap; echo stats >&3; cat <&3`).
//!
//! | command | answer |
//! |---|---|
//! | `take [auto] [if-changed] [label=L]` | `id= new=0/1 created_ns= preflush_us= freeze_us= snap_us= pause_us= commit_us= barriers=` |
//! | `list [sizes]` | one line per snapshot, oldest first: `id= created_ns= auto= mounted= rollback= bytes= label=` |
//! | `size ID` | `id= bytes=` |
//! | `stats` | the pool's space, the wall clock and this boot's freeze pauses (`crate::stats`) |
//! | `delete ID` | `deleted id=` |
//! | `mount ID` | `path=` (read-only, `/run/bitsos/snap/ID`) |
//! | `umount ID` | `unmounted id=` |
//! | `rollback ID` | `scheduled to= replaced=` (VS-3: /data goes back to snapshot ID at its next mount) |
//! | `rollback cancel` | `cancelled to=` (0: nothing was scheduled) |
//! | `rollback` | `pending= not_done=` (0: none; not_done: the pending one this start's mount could not carry out) |
//! | `system-at UTC_S` | `generation= running= next= pin= writable=` (0: none; RB-9's generations at a time) |
//! | `restore-at UTC_S [dry]` | `ok=1 dry= snapshot= snapshot_ns= pin= running= next= replaced= note=` or `ok=0 note=` (`system::restore_at`) |
//! | `restore-at cancel` | `undone=1 text=`, `undone=0 text=` (nothing of a restore-at to undo) or `failed=1 text=` (`system::undo_restore_at`) |
//!
//! One command per write(2), whole (a line is at most `LINE_MAX` bytes, far below any split by libc's
//! buffering); a newline inside it is EINVAL, not a second command. A second write on the same open file
//! while one runs (threads sharing the fd) is EBUSY, so an answer always belongs to the command before it.
//! The device is a stream: the file offset of pread/pwrite is not used. Root only: V2's credentials have no
//! capabilities yet, so uid 0 stands for Linux's CAP_SYS_ADMIN (which FIFREEZE asks for, fs/ioctl.c).
//! The calls are V2's own (no Linux program uses them), so the ABI diff does not cover them.
//!
//! Errors: ENODEV no /data, EOPNOTSUPP /data is a plain ext4 image, EROFS /data is read-only, EINVAL a bad line
//! or an id that is not a snapshot of /data, ENOENT no such snapshot, EBUSY frozen/mounted/another operation,
//! ENOSPC the pool is full, ETIMEDOUT the pool's lock.

use crate::{Target, mounts};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use ksync::SpinLock;
use uapi::errno::*;
use vfs::{Errno, File};

/// Longest command line (a bound against hostile input; the longest real one is ~90 bytes).
/// An answer line is at most a few hundred bytes plus one line per snapshot.
pub const LINE_MAX: usize = 256;
/// misc major 10; a minor no Linux driver uses (not visible to programs beyond fstat).
const DEV: (u32, u32) = (10, 239);

fn id_arg(w: Option<&str>) -> Result<u64, Errno> {
    w.and_then(|s| s.parse().ok()).ok_or(EINVAL)
}

/// Where `mount ID` puts a snapshot: `/run/bitsos/snap/ID` for /data's pool, `<pool>-ID` for any other.
pub fn mount_path(t: &Target, id: u64) -> String {
    if t.pool_name == fs_ext4::data::VOLUME { format!("{}/{id}", mounts::BASE) } else { format!("{}/{}-{id}", mounts::BASE, t.pool_name) }
}

/// Runs one command line against the target `target()` names.
pub fn run(line: &[u8], target: &dyn Fn() -> Result<Target, Errno>) -> Result<String, Errno> {
    if line.len() > LINE_MAX {
        return Err(EINVAL);
    }
    let line = core::str::from_utf8(line).map_err(|_| EINVAL)?.trim();
    if line.contains(['\n', '\r', '\0']) {
        return Err(EINVAL);
    }
    let mut w = line.split_ascii_whitespace();
    let cmd = w.next().ok_or(EINVAL)?;
    let words: Vec<&str> = w.collect();
    let one = |words: &[&str]| if words.len() == 1 { id_arg(words.first().copied()) } else { Err(EINVAL) };
    match cmd {
        "take" => {
            let (mut auto, mut if_changed, mut label) = (false, false, None);
            for x in &words {
                match *x {
                    "auto" => auto = true,
                    "if-changed" => if_changed = true,
                    l if l.starts_with("label=") => label = Some(&l[6..]),
                    _ => return Err(EINVAL),
                }
            }
            let label = label.unwrap_or(if auto { "auto" } else { "manual" });
            let r = crate::take(&target()?, label, auto, if_changed)?;
            Ok(format!(
                "id={} new={} created_ns={} preflush_us={} freeze_us={} snap_us={} pause_us={} commit_us={} barriers={}\n",
                r.id,
                r.new as u8,
                r.created_ns,
                r.preflush_ns / 1000,
                r.freeze_ns / 1000,
                r.snap_ns / 1000,
                r.pause_ns / 1000,
                r.commit_ns / 1000,
                r.pause_barriers
            ))
        }
        "list" => {
            let sizes = match words.as_slice() {
                [] => false,
                ["sizes"] => true,
                _ => return Err(EINVAL),
            };
            let mut out = String::new();
            for s in crate::list(&target()?, sizes)? {
                let bytes = s.bytes.map(|b| format!("{b}")).unwrap_or_else(|| "-".into());
                let _ = writeln!(
                    out,
                    "id={} created_ns={} auto={} mounted={} rollback={} bytes={bytes} label={}",
                    s.id, s.created_ns, s.auto as u8, s.mounted as u8, s.rollback as u8, s.label
                );
            }
            Ok(out)
        }
        "size" => {
            let id = one(&words)?;
            Ok(format!("id={id} bytes={}\n", crate::size_of(&target()?, id)?))
        }
        "stats" if words.is_empty() => Ok(format!("{}\n", crate::stats(&target()?)?)),
        "delete" => {
            let id = one(&words)?;
            crate::delete(&target()?, id)?;
            Ok(format!("deleted id={id}\n"))
        }
        "mount" => {
            let id = one(&words)?;
            let t = target()?;
            let at = mount_path(&t, id);
            Ok(format!("path={}\n", mounts::mount_at(&t, id, vfs::root_ns(), &at)?))
        }
        "umount" => {
            let id = one(&words)?;
            mounts::umount(&target()?, id)?;
            Ok(format!("unmounted id={id}\n"))
        }
        "rollback" => match words.as_slice() {
            [] => {
                let t = target()?;
                Ok(format!("pending={} not_done={}\n", crate::pending_rollback(&t)?.unwrap_or(0), crate::rollback_not_done(&t)?.unwrap_or(0)))
            }
            ["cancel"] => Ok(format!("cancelled to={}\n", crate::cancel_rollback(&target()?)?.unwrap_or(0))),
            _ => {
                let id = one(&words)?;
                let old = crate::schedule_rollback(&target()?, id)?;
                Ok(format!("scheduled to={id} replaced={}\n", old.unwrap_or(0)))
            }
        },
        "system-at" => {
            let [t] = words.as_slice() else { return Err(EINVAL) };
            let utc: i64 = t.parse().map_err(|_| EINVAL)?;
            let a = crate::system::at(utc).map_err(|e| {
                klog::event!(Warn, "snap", "system.listing_failed", why = e.as_str());
                EIO
            })?;
            let n = |x: Option<u32>| x.unwrap_or(0);
            Ok(format!("generation={} running={} next={} pin={} writable={}\n", n(a.generation), n(a.running), n(a.next), n(a.pin), a.writable as u8))
        }
        "restore-at" if words.as_slice() == ["cancel"] => Ok(match crate::system::undo_restore_at(&target()?) {
            Ok((done, t)) => format!("undone={} text={}\n", done as u8, crate::system::one_line(&t)),
            Err(t) => format!("failed=1 text={}\n", crate::system::one_line(&t)),
        }),
        "restore-at" => {
            let (t, dry) = match words.as_slice() {
                [t] => (*t, false),
                [t, "dry"] => (*t, true),
                _ => return Err(EINVAL),
            };
            let utc: i64 = t.parse().map_err(|_| EINVAL)?;
            Ok(match crate::system::restore_at(&target()?, utc, dry) {
                Ok(p) => format!(
                    "ok=1 dry={} snapshot={} snapshot_ns={} pin={} running={} next={} replaced={} note={}\n",
                    dry as u8,
                    p.snapshot,
                    p.snapshot_ns,
                    p.pin.unwrap_or(0),
                    p.at.running.unwrap_or(0),
                    p.at.next.unwrap_or(0),
                    p.replaced.unwrap_or(0),
                    crate::system::one_line(&p.note)
                ),
                Err(e) => format!("ok=0 note={}\n", crate::system::one_line(&e)),
            })
        }
        _ => Err(EINVAL),
    }
}

/// One open of the node.
struct Session {
    answer: SpinLock<(Vec<u8>, usize)>,
    /// Held for a whole command and its answer.
    busy: ksync::Mutex<()>,
}

impl Session {
    fn read(&self, buf: &mut [u8]) -> usize {
        let mut a = self.answer.lock();
        let (data, pos) = &mut *a;
        let n = buf.len().min(data.len().saturating_sub(*pos));
        buf[..n].copy_from_slice(&data[*pos..*pos + n]);
        *pos += n;
        n
    }

    fn write(&self, buf: &[u8]) -> Result<usize, Errno> {
        let Some(_one) = self.busy.try_lock() else {
            klog::event!(Info, "snap", "dev.concurrent_write"; "a second write while a command runs");
            return Err(EBUSY);
        };
        let line = buf.strip_suffix(b"\n").unwrap_or(buf);
        let r = run(line, &Target::data);
        let mut a = self.answer.lock();
        a.0.clear();
        a.1 = 0;
        let text = r?;
        a.0.try_reserve(text.len()).map_err(|_| ENOMEM)?;
        a.0.extend_from_slice(text.as_bytes());
        Ok(buf.len())
    }
}

impl File for Session {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        Ok(self.read(buf))
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write(buf)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(DEV.0, DEV.1)
    }
}

/// The node itself; every open gets its own `Session`.
struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(DEV.0, DEV.1)
    }
    fn open_node(&self, _flags: u64, who: vfs::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        // The mode already keeps others out; this holds when someone chmods the node.
        if who.uid != 0 {
            return Some(Err(EACCES));
        }
        Some(Ok(Arc::new(Session { answer: SpinLock::named("snap.session", (Vec::new(), 0)), busy: ksync::Mutex::named("snap.session.busy", ()) })))
    }
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/bitssnap", make);
