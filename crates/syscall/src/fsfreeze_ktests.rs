//! FIFREEZE / FITHAW (VS-2 CR5; linux-6.6/fs/ioctl.c:387-415): EPERM for uid 1000, EOPNOTSUPP on tmpfs and on a
//! file without a file system of its own, EINVAL for a thaw nobody asked for; on an ext4 /data (FS-8c) a freeze,
//! EBUSY for a second one, the thaw, and EINVAL for a second thaw. Rerunnable: /data is thawed before the end
//! (also when a step fails), and the tmpfs file is removed.

use crate::ab6_ktests::T;
use crate::fsfreeze::{FIFREEZE, FITHAW};
use crate::wave_a_ktests::expect;
use alloc::format;
use ktest::{TestResult, ktest};
use uapi::errno::*;
use uapi::nr;

const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_RDONLY: u64 = 0;
const TMP: &[u8] = b"/tmp/ktest-fsfreeze";
/// FS-8c's state line (`fs_ext4::data::STATE_FILE`; syscall does not depend on fs-ext4).
const STATE: &str = "/run/bitsos/data-state";
/// The /data thread waits up to 120 s for the stick; a missing line means a boot without that thread.
const DATA_WAIT_NS: u64 = 150_000_000_000;

ktest!("ab.fsfreeze", Safe, fsfreeze);
fn fsfreeze() -> TestResult {
    let ns = vfs::root_ns();
    let file = ns.open_flags(TMP, O_RDWR | O_CREAT, 0o600).map_err(|e| format!("{TMP:?}: {e}"))?;
    let r = (|| {
        let mut user = T::with("ab-fsfreeze-u", 1000, false)?;
        let ufd = user.p.fds.install(file.clone()).map_err(|e| format!("{e}"))? as u64;
        expect("FIFREEZE as uid 1000", user.call(nr::IOCTL, &[ufd, FIFREEZE, 0]), -EPERM)?;
        expect("FITHAW as uid 1000", user.call(nr::IOCTL, &[ufd, FITHAW, 0]), -EPERM)?;
        let mut root = T::with("ab-fsfreeze-r", 0, false)?;
        let fd = root.p.fds.install(file.clone()).map_err(|e| format!("{e}"))? as u64;
        expect("FIFREEZE on tmpfs", root.call(nr::IOCTL, &[fd, FIFREEZE, 0]), -EOPNOTSUPP)?;
        expect("FITHAW on tmpfs", root.call(nr::IOCTL, &[fd, FITHAW, 0]), -EINVAL)?;
        let null = ns.open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let nfd = root.p.fds.install(vfs::OpenFile::new(null, vfs::OpenFlags { read: true, ..Default::default() })).map_err(|e| format!("{e}"))? as u64;
        expect("FIFREEZE on a file without a path", root.call(nr::IOCTL, &[nfd, FIFREEZE, 0]), -EOPNOTSUPP)?;
        expect("FITHAW on a file without a path", root.call(nr::IOCTL, &[nfd, FITHAW, 0]), -EINVAL)?;
        // /data comes up in the background after boot (FS-8c): wait while its state line says it is still coming.
        let start = time::now_ns();
        while time::now_ns().saturating_sub(start) < DATA_WAIT_NS {
            let st = ns.read_file(STATE, 256).unwrap_or_default();
            if !(st.is_empty() || st.starts_with(b"waiting") || st.starts_with(b"creating")) {
                break;
            }
            sched::sleep_ns(100_000_000);
        }
        let ext4 = ns.lookup("/data").ok().filter(|d| vfs::statfs::of(&d.sb, false).is_ok_and(|s| s.ftype == 0xEF53));
        let Some(_) = ext4 else {
            return Ok(false);
        };
        let data = ns.open_flags(b"/data", O_RDONLY | 0o200000, 0).map_err(|e| format!("/data: {e}"))?;
        let dfd = root.p.fds.install(data).map_err(|e| format!("{e}"))? as u64;
        let frozen = root.call(nr::IOCTL, &[dfd, FIFREEZE, 0]);
        let steps = (|| {
            expect("FIFREEZE /data", frozen, 0)?;
            expect("FIFREEZE /data again", root.call(nr::IOCTL, &[dfd, FIFREEZE, 0]), -EBUSY)?;
            expect("FITHAW /data", root.call(nr::IOCTL, &[dfd, FITHAW, 0]), 0)?;
            expect("FITHAW /data again", root.call(nr::IOCTL, &[dfd, FITHAW, 0]), -EINVAL)
        })();
        if frozen == 0 && steps.is_err() {
            let _ = root.call(nr::IOCTL, &[dfd, FITHAW, 0]);
        }
        steps.map(|_| true)
    })();
    let _ = vfs::ops::Ctx::kernel(&ns).unlink(None, TMP);
    match r? {
        true => Ok(()),
        false => ktest::skip("no ext4 /data in this boot: only the refusals were checked"),
    }
}
