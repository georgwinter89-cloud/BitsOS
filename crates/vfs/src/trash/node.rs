//! `/dev/bitsos/papierkorb` (0666; each command checks its caller): a command is its fields, one per line, paths
//! as the boot view names them; it may come in several writes (a shell's printf writes piece by piece):
//!
//! - `restore\n<fs>\n<id>\n` puts entry `<id>` of the trash on the file system whose top is `<fs>` back
//!   (EEXIST: a name is taken, nothing moved);
//! - `restore-to\n<fs>\n<id>\n<path>\n` puts the entry's top at `<path>` instead;
//! - `empty\n<fs>\n<id>\n` / `empty\n<fs>\nall\n` removes for real; EPERM from a process of an assistant tree.
//!
//! The write that completes a command answers its errno (the others their length); a read on the same open file
//! then gives the answer's line once (`moved=<n>` / `emptied=<n>`).

use super::restore::{self, Caller};
use crate::{Cred, Errno, File};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

pub const CONTROL: &str = "/dev/bitsos/papierkorb";

/// vfs-sys installs the calling process's view: pid, tree, file-system credentials.
pub type CallerOf = fn() -> Option<Caller>;
static CALLER: ksync::Once<CallerOf> = ksync::Once::new();

pub fn set_caller(f: CallerOf) {
    CALLER.get_or_init(|| f);
}

fn rdev_number() -> u64 {
    crate::abi::encode_dev(10, 236)
}

/// The longest command taken (paths of PATH_MAX and the words).
const PENDING_MAX: usize = 3 * 4096 + 64;

/// How many lines a command has (by its first one).
fn arity(verb: &[u8]) -> Option<usize> {
    match verb {
        b"restore" | b"empty" => Some(3),
        b"restore-to" => Some(4),
        _ => None,
    }
}

/// Runs one whole command for `who` and answers its line.
pub fn command(who: &Caller, text: &[u8]) -> Result<Vec<u8>, Errno> {
    let f: Vec<&[u8]> = text.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
    let ns = crate::root_ns();
    match f.as_slice() {
        [b"restore", fs, id] => restore::restore(&ns, who, fs, id, None).map(|r| format!("moved={}\n", r.moved).into_bytes()),
        [b"restore-to", fs, id, to] => restore::restore(&ns, who, fs, id, Some(to)).map(|r| format!("moved={}\n", r.moved).into_bytes()),
        [b"empty", fs, b"all"] => restore::empty(&ns, who, fs, None).map(|n| format!("emptied={n}\n").into_bytes()),
        [b"empty", fs, id] => restore::empty(&ns, who, fs, Some(id)).map(|n| format!("emptied={n}\n").into_bytes()),
        _ => Err(EINVAL),
    }
}

struct Session {
    /// A command's lines so far.
    pending: ksync::Mutex<Vec<u8>>,
    answer: ksync::Mutex<Vec<u8>>,
}

impl Session {
    /// `buf` added to the command under way; Some(the whole command) once its last line came.
    fn take(&self, buf: &[u8]) -> Result<Option<Vec<u8>>, Errno> {
        let mut p = self.pending.lock();
        if p.len() + buf.len() > PENDING_MAX {
            p.clear();
            return Err(EINVAL);
        }
        p.extend_from_slice(buf);
        let lines: Vec<usize> = p.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i).collect();
        let Some(&first) = lines.first() else { return Ok(None) };
        let Some(n) = arity(&p[..first]) else {
            p.clear();
            return Err(EINVAL);
        };
        let Some(&end) = lines.get(n - 1) else { return Ok(None) };
        let whole: Vec<u8> = p.drain(..=end).collect();
        Ok(Some(whole))
    }
}

impl File for Session {
    /// The last command's answer, once (a character device: the file position is not looked at).
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut a = self.answer.lock();
        let n = a.len().min(buf.len());
        buf[..n].copy_from_slice(&a[..n]);
        a.drain(..n);
        Ok(n)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let Some(whole) = self.take(buf)? else { return Ok(buf.len()) };
        let who = CALLER.get().and_then(|f| f()).ok_or(EPERM)?;
        let a = command(&who, &whole)?;
        *self.answer.lock() = a;
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
}

struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
    fn open_node(&self, _flags: u64, _who: Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(Ok(Arc::new(Session { pending: ksync::Mutex::named("vfs.trash.pending", Vec::new()), answer: ksync::Mutex::named("vfs.trash.answer", Vec::new()) })))
    }
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
crate::devnode!("/dev/bitsos/papierkorb", make);

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session { pending: ksync::Mutex::new(Vec::new()), answer: ksync::Mutex::new(Vec::new()) }
    }

    /// A command in pieces (printf's way) is one command once its last line came; the next starts fresh.
    #[test]
    fn a_command_may_come_in_pieces() {
        let s = session();
        assert_eq!(s.take(b"restore\n/da"), Ok(None));
        assert_eq!(s.take(b"ta\n17-"), Ok(None));
        assert_eq!(s.take(b"3\nempty\n"), Ok(Some(b"restore\n/data\n17-3\n".to_vec())));
        assert_eq!(s.take(b"/data\nall\n"), Ok(Some(b"empty\n/data\nall\n".to_vec())));
        assert_eq!(s.take(b"restore-to\n/data\n1-1\n/data/x\n"), Ok(Some(b"restore-to\n/data\n1-1\n/data/x\n".to_vec())));
        assert_eq!(s.take(b"tidy\n"), Err(EINVAL), "an unknown word");
        assert_eq!(s.take(b"empty\n"), Ok(None), "after a refusal a new command starts");
        assert_eq!(s.take(&[b'x'; PENDING_MAX]), Err(EINVAL));
    }
}
