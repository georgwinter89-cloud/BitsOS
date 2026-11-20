//! The trash (plan/spec/assistent-eins K6) as `loeschen` and the removal of a chat use it. On BitsOS the kernel
//! keeps it (EINS-C, eins-c.md §2): a removal inside an assistant's process tree goes to
//! `<fs>/.papierkorb/<id>/` with `files/` and `record.json`, and nothing of the tree may write there. So the name is
//! removed by a child in a tree (the service's own, or a fresh one the child enters when assist runs outside one)
//! that carries the chat as its label; the entry is then found by the child's pid. Without the kernel's node (a
//! kernel without the trash, the host tests) assist moves the name itself into the same layout, in
//! `/<mount>/.papierkorb` or, where the user may not make that, `$HOME/.papierkorb` on the same file system
//! ([`bin_for`]). On a file system that keeps nothing over a restart (tmpfs, proc and the like, told by its type
//! alone) there is no trash and the call deletes for real; it asked Georg first in every case (F4), and the
//! question then says "endgültig". Inside the trash nothing is deleted: emptying it is Georg's own step.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const FOLDER: &str = ".papierkorb";
/// The kernel's control node of the trash: where it is, the kernel diverts removals of a tree.
const NODE: &str = "/dev/bitsos/papierkorb";
/// The trash of a file system whose top takes no writes (the boot stick).
const STICK_HOME: &str = "BITSOS2/home";
/// The child that removes: in the service's tree, or in a fresh one it enters itself; its chat as its label.
const REMOVER: &str = "t=/proc/self/bitsos_tree; case $(cat $t) in tree=*) ;; *) echo enter > $t || exit 97;; esac; \
                       echo \"chat $2\" > $t 2>/dev/null; exec rm -rf -- \"$1\"";

/// File systems that keep nothing over a restart, or are the kernel's views.
const FLEETING: [&str; 15] =
    ["tmpfs", "ramfs", "proc", "sysfs", "devtmpfs", "devpts", "cgroup", "cgroup2", "debugfs", "tracefs", "securityfs", "pstore", "bpf", "mqueue", "configfs"];

#[derive(Debug, PartialEq, Eq)]
pub struct Put {
    /// The trash entry's id; None: deleted for real on a fleeting file system.
    pub id: Option<String>,
    pub original: PathBuf,
    /// Where it lies now.
    pub at: Option<PathBuf>,
    /// Removed by the kernel without an entry: a file the assistant made itself (or one more name of a file).
    pub own: bool,
}

impl Put {
    pub fn said(&self) -> String {
        match (&self.id, &self.at) {
            (Some(id), Some(at)) => format!("{} is in the trash now (entry {id}, at {}); Georg can get it back from there", self.original.display(), at.display()),
            _ if self.own => format!("{} is deleted: the assistant made it itself (or it has another name), so it is not kept in the trash", self.original.display()),
            _ => format!("{} is deleted (it was on a file system that keeps nothing over a restart, which has no trash)", self.original.display()),
        }
    }
}

/// The mount point of the file system `p` lies on: the highest folder above it on the same device.
fn mount_of(p: &Path) -> Option<PathBuf> {
    let dev = std::fs::symlink_metadata(p).ok()?.dev();
    let mut at = p.to_path_buf();
    while let Some(up) = at.parent() {
        if std::fs::metadata(up).ok()?.dev() != dev {
            break;
        }
        at = up.to_path_buf();
    }
    Some(at)
}

/// Whether the file system mounted at `mount` keeps nothing, by its type in `mounts` (the text of /proc/mounts).
/// Only the type decides, never the path: a disk mounted below /run (/run/media) keeps everything. A type that
/// cannot be told counts as one that keeps: its trash is tried, and nothing is deleted when that fails.
fn fleeting_in(mounts: &str, mount: &Path) -> bool {
    let Some(m) = mount.to_str() else { return false };
    let kind = mounts.lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 3 && f[1] == m).then(|| f[2].to_string())
    }).last();
    kind.is_some_and(|t| FLEETING.contains(&t.as_str()))
}

fn fleeting(mount: &Path) -> bool {
    let text = std::fs::read_to_string("/proc/mounts").or_else(|_| std::fs::read_to_string("/proc/self/mounts")).unwrap_or_default();
    fleeting_in(&text, mount)
}

/// The path as it is deleted (the folder it is in resolved, its own name kept) and the mount it lies on.
fn resolve(path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let name = path.file_name().ok_or_else(|| format!("{} cannot be deleted", path.display()))?;
    let parent = std::fs::canonicalize(path.parent().unwrap_or(Path::new("/"))).map_err(|e| format!("{}: {e}", path.display()))?;
    let real = parent.join(name);
    let mount = mount_of(&real).ok_or_else(|| format!("{}: its file system is unknown", real.display()))?;
    Ok((real, mount))
}

/// Whether deleting `path` would put it into a trash: false when it lies on a file system that keeps nothing,
/// where it is deleted for real (Georg's question says so). A path that cannot be resolved counts as trash:
/// the call fails then anyway, before anything is deleted.
pub fn has_trash(path: &Path) -> bool {
    resolve(path).map_or(true, |(_, mount)| !fleeting(&mount))
}

static COUNT: AtomicU64 = AtomicU64::new(0);

/// The trash for a file on the file system at `mount`: `/<mount>/.papierkorb`, made when it is not there. Where the
/// session user may not make it (the mount's root is root's, before wave C's kernel makes it), the one in the home
/// folder when that lies on the same file system: a rename stays possible, and nothing is copied.
fn bin_for(mount: &Path, real: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let make = |p: &Path| std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p);
    let first = mount.join(FOLDER);
    let Err(e) = make(&first) else { return Ok(first) };
    let dev = std::fs::symlink_metadata(real).map(|m| m.dev()).ok();
    if let Some(home) = home.filter(|h| h.is_absolute() && h.parent().is_some())
        && std::fs::metadata(home).ok().map(|m| m.dev()) == dev
    {
        let second = home.join(FOLDER);
        if make(&second).is_ok() {
            return Ok(second);
        }
    }
    Err(format!("the trash {} could not be made ({e}); nothing was deleted", first.display()))
}

static KEEP_IN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Host tests only: the trash without the kernel's node goes into `dir` (below the build's target folder), so a
/// test never writes into a trash of the host itself (a mount's root or the user's home). The first call wins.
#[doc(hidden)]
pub fn keep_tests_in(dir: &Path) -> PathBuf {
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    let _ = KEEP_IN.set(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
    KEEP_IN.get().cloned().unwrap_or_default()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Moves `path` (a file, a link or a whole folder) into the trash of its file system. `chat` and `command` go
/// into the record.
pub fn put(path: &Path, chat: &str, command: &str) -> Result<Put, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // The folder it is in, resolved; the name itself stays (a link is moved, not what it points to).
    let (real, mount) = resolve(path)?;
    let name = path.file_name().ok_or_else(|| format!("{} cannot be deleted", path.display()))?;
    if real.components().any(|c| c.as_os_str() == FOLDER) {
        return Err("this lies in the trash: only Georg empties the trash himself (bits papierkorb leeren)".into());
    }
    if real == mount || real == Path::new("/") {
        return Err(format!("{} is where a file system is mounted; it cannot be deleted", real.display()));
    }
    if fleeting(&mount) {
        let gone = if meta.is_dir() { std::fs::remove_dir_all(&real) } else { std::fs::remove_file(&real) };
        gone.map_err(|e| format!("{}: {e}", real.display()))?;
        return Ok(Put { id: None, original: real, at: None, own: false });
    }
    if Path::new(NODE).exists() {
        return by_kernel(real, &mount, chat);
    }
    let bin = match KEEP_IN.get() {
        Some(b) => b.clone(),
        None if cfg!(test) => keep_tests_in(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target").join(FOLDER)),
        None => bin_for(&mount, &real, Some(&super::home()))?,
    };
    let id = format!("{}-{}-{}", now(), std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed));
    let entry = bin.join(&id);
    let at = entry.join("files").join(real.strip_prefix(&mount).unwrap_or(Path::new(name)));
    let mount_text = mount.to_string_lossy();
    let record = crate::json::Json::obj()
        .with("v", 1i64)
        .with("id", id.as_str())
        .with("fs", mount_text.as_ref())
        .with("path", real.to_string_lossy().as_ref())
        .with("what", "unlink")
        .with("time", i64::try_from(now()).unwrap_or(0))
        .with("pid", i64::from(std::process::id()))
        .with("command", command)
        .with("chat", chat);
    let made = std::fs::DirBuilder::new().mode(0o700).create(&entry).and_then(|()| std::fs::DirBuilder::new().recursive(true).mode(0o700).create(at.parent().unwrap_or(&entry)));
    made.map_err(|e| format!("{}: {e}; nothing was deleted", entry.display()))?;
    let undo = || {
        let _ = std::fs::remove_dir_all(&entry);
    };
    if let Err(e) = std::fs::write(entry.join("record.json"), record.to_text() + "\n") {
        undo();
        return Err(format!("the trash's record could not be written ({e}); nothing was deleted"));
    }
    if let Err(e) = std::fs::rename(&real, &at) {
        undo();
        return Err(format!("{}: {e}; nothing was deleted", real.display()));
    }
    Ok(Put { id: Some(id), original: real, at: Some(at), own: false })
}

/// The kernel's way: the child removes, the kernel moves the name into the trash; its entry is the newest one of
/// that child that holds the name.
fn by_kernel(real: PathBuf, mount: &Path, chat: &str) -> Result<Put, String> {
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", REMOVER, "loeschen"])
        .arg(&real)
        .arg(chat)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}; nothing was deleted", real.display()))?;
    let pid = child.id();
    let out = child.wait_with_output().map_err(|e| format!("{}: {e}", real.display()))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(match out.status.code() {
            Some(97) => "the trash's process tree could not be entered; nothing was deleted".into(),
            _ => format!("{}: {}", real.display(), if why.is_empty() { "not removed".into() } else { why }),
        });
    }
    let found = [mount.join(FOLDER), mount.join(STICK_HOME).join(FOLDER)]
        .iter()
        .filter_map(|bin| std::fs::read_dir(bin).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let rec = crate::json::parse(&std::fs::read_to_string(e.path().join("record.json")).ok()?).ok()?;
            let top = PathBuf::from(rec.get("path")?.as_str()?);
            let mine = rec.get("pid").and_then(crate::json::Json::as_i64) == Some(i64::from(pid)) && real.starts_with(&top);
            mine.then(|| (rec.get("time").and_then(crate::json::Json::as_i64).unwrap_or(0), e.file_name().to_string_lossy().into_owned(), e.path()))
        })
        .max();
    Ok(match found {
        Some((_, id, dir)) => Put { id: Some(id), original: real, at: Some(dir), own: false },
        None => Put { id: None, original: real, at: None, own: true },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_and_a_folder_go_into_the_trash_of_their_file_system() {
        // The target folder of the build is on a persistent file system.
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../../target/assist-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("ordner/innen")).unwrap();
        std::fs::write(d.join("a.txt"), "eins").unwrap();
        std::fs::write(d.join("ordner/innen/b.txt"), "zwei").unwrap();
        let d = std::fs::canonicalize(&d).unwrap();
        let mount = mount_of(&d).unwrap();
        assert!(!fleeting(&mount), "the build's target folder lies on a file system that keeps nothing: this test needs a disk");
        let bin = keep_tests_in(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target").join(FOLDER));
        let p = put(&d.join("a.txt"), "c1", "loeschen").unwrap();
        let at = p.at.clone().unwrap();
        assert!(!d.join("a.txt").exists() && std::fs::read_to_string(&at).unwrap() == "eins", "{p:?}");
        assert!(at.starts_with(&bin) && p.said().contains("in the trash"));
        let record = std::fs::read_to_string(bin.join(p.id.clone().unwrap()).join("record.json")).unwrap();
        assert!(record.contains(&format!(r#""path":"{}""#, d.join("a.txt").display())) && record.contains(r#""chat":"c1""#), "{record}");
        assert!(at.starts_with(bin.join(p.id.clone().unwrap()).join("files")), "the kernel's layout: {}", at.display());
        let q = put(&d.join("ordner"), "c1", "loeschen").unwrap();
        assert!(q.at.unwrap().join("innen/b.txt").exists() && !d.join("ordner").exists());
        assert!(put(&at, "c1", "loeschen").unwrap_err().contains("only Georg empties"), "the trash itself");
        assert!(put(&d.join("nichts"), "c1", "loeschen").is_err());
        assert!(put(&mount, "c1", "loeschen").is_err());
        for id in [p.id.unwrap(), q.id.unwrap()] {
            let _ = std::fs::remove_dir_all(bin.join(&id));
        }
        // A mount whose root the user may not write: the home's trash on the same file system.
        use std::os::unix::fs::PermissionsExt;
        let (closed, home) = (d.join("zu"), d.join("heim"));
        std::fs::create_dir_all(&closed).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::create_dir(closed.join("probe")).is_err() {
            assert_eq!(bin_for(&closed, &home, Some(&home)).unwrap(), home.join(FOLDER));
            assert!(bin_for(&closed, &home, None).unwrap_err().contains("nothing was deleted"));
        }
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(has_trash(&d.join("a.txt")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_the_type_of_a_file_system_says_it_keeps_nothing() {
        let mounts = "tmpfs /run tmpfs rw 0 0\n/dev/sdb1 /run/media/georg/Platte ntfs3 rw 0 0\nproc /proc proc rw 0 0\n\
                      tmpfs /tmp tmpfs rw 0 0\n/dev/sda2 /data ext4 rw 0 0\n";
        assert!(!fleeting_in(mounts, Path::new("/run/media/georg/Platte")), "a disk below /run keeps everything");
        assert!(fleeting_in(mounts, Path::new("/run")) && fleeting_in(mounts, Path::new("/tmp")) && fleeting_in(mounts, Path::new("/proc")));
        assert!(!fleeting_in(mounts, Path::new("/data")) && !fleeting_in(mounts, Path::new("/nirgends")), "unknown: the trash is tried");
    }
}
