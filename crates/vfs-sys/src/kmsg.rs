//! `/dev/kmsg` (1:11; UL-2 change request 4, applied by INT-A1): a program's write becomes one klog event per
//! line (subsystem `user`, name `kmsg`), so init's lines reach MCP `log` and the mirror on a machine without
//! COM1. A leading `<n>` is the syslog priority as Linux's `devkmsg_emit` reads it (linux-6.6/kernel/printk/
//! printk.c): its level part (n & 7) chooses the event level (0-3 Error, 4 Warn, 5-6 Info, 7 Debug); without
//! it the line is Info. Reads answer end of file (V2 has no record reader on this node yet; the log is MCP's
//! `log` tool). Lines are cut at 240 bytes per event, at most 1 KiB per write (Linux: LOG_LINE_MAX ~1 KiB).

use alloc::string::String;
use alloc::sync::Arc;
use uapi::errno::*;
use vfs::{Errno, File};

/// Bytes of one write that become events (the rest is accepted and dropped, as Linux truncates a record).
const WRITE_MAX: usize = 1024;

/// The level (0..=7) and the text after an optional `<n>` prefix.
pub fn split_priority(line: &[u8]) -> (u8, &[u8]) {
    if line.first() == Some(&b'<')
        && let Some(end) = line.iter().take(12).position(|&b| b == b'>')
        && end > 1
        && line[1..end].iter().all(u8::is_ascii_digit)
    {
        let n: u32 = core::str::from_utf8(&line[1..end]).ok().and_then(|s| s.parse().ok()).unwrap_or(6);
        return ((n & 7) as u8, &line[end + 1..]);
    }
    (6, line)
}

struct Kmsg;

impl File for Kmsg {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Ok(0)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let take = &buf[..buf.len().min(WRITE_MAX)];
        for line in take.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let (lvl, text) = split_priority(line);
            for piece in text.chunks(240) {
                let t = String::from_utf8_lossy(piece);
                let t = t.trim_end_matches('\r');
                match lvl {
                    0..=3 => klog::event!(Error, "user", "kmsg", text = t),
                    4 => klog::event!(Warn, "user", "kmsg", text = t),
                    7 => klog::event!(Debug, "user", "kmsg", text = t),
                    _ => klog::event!(Info, "user", "kmsg", text = t),
                }
            }
        }
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o644
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(1, 11)
    }
    fn ioctl(&self, _cmd: u64, _arg: u64) -> Result<alloc::vec::Vec<u8>, Errno> {
        Err(ENOTTY)
    }
}

fn make_kmsg() -> Arc<dyn File> {
    Arc::new(Kmsg)
}
vfs::devnode!("/dev/kmsg", make_kmsg);

#[cfg(test)]
mod tests {
    use super::split_priority;

    #[test]
    fn priorities() {
        assert_eq!(split_priority(b"<3>init: boom"), (3, &b"init: boom"[..]));
        assert_eq!(split_priority(b"<14>user info"), (6, &b"user info"[..]));
        assert_eq!(split_priority(b"plain"), (6, &b"plain"[..]));
        assert_eq!(split_priority(b"<x>not a prefix"), (6, &b"<x>not a prefix"[..]));
        assert_eq!(split_priority(b"<>empty"), (6, &b"<>empty"[..]));
    }
}
