//! FS-12 in-kernel test of the FAT code's request priorities (class Safe: its own RAM disk, rerunnable):
//! - `fs.fat.prio`: from a Background thread, a `DiskDev` request goes as Background without the volume lock,
//!   as Normal while the thread holds it (`lift`), as Interactive while an Interactive thread waits for it.

use crate::dev::Dev;
use crate::gate::DiskDev;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use block::{Disk, DiskKind, Dispatch, RamDisk};
use devmodel::block::IoPriority::{self, *};
use ktest::{kassert, ktest, TestResult};
use sched::Class;

const MS: u64 = 1_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn fat_prio() -> TestResult {
    let r = Arc::new(RamDisk::try_new("fs12-fat", 512, 64).map_err(e)?);
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Thread).map_err(e)?;
    let dev = Arc::new(DiskDev::new(d.clone(), 0, 64, None));
    r.record();
    let out: Arc<ksync::SpinLock<Option<Result<(), i64>>>> = Arc::new(ksync::SpinLock::new(None));
    let (o, dv) = (out.clone(), dev.clone());
    let h = sched::spawn("fs12-fat", Class::Background, move || {
        let res = (|| -> Result<(), i64> {
            let lift = dv.lift().ok_or(-1i64)?;
            let mut b = vec![0u8; 512];
            dv.read(1, &mut b)?;
            {
                let _held = lift.hold();
                dv.read(2, &mut b)?;
                dv.write(3, &b)?;
                let _w = lift.wait_as(Interactive);
                dv.read(4, &mut b)?;
            }
            dv.read(5, &mut b)
        })();
        *o.lock() = Some(res);
    })
    .map_err(e)?;
    let joined = h.join_timeout(5000 * MS);
    let calls: Vec<(u64, IoPriority)> = r.calls().into_iter().map(|c| (c.lba, c.prio)).collect();
    d.stop();
    kassert!(joined, "the Background thread did not finish");
    let res = out.lock().take();
    kassert!(res == Some(Ok(())), "the requests: {res:?}");
    let want = [(1, Background), (2, Normal), (3, Normal), (4, Interactive), (5, Background)];
    kassert!(calls == want, "device calls {calls:?}, want {want:?}");
    let mut left = 2000;
    while !d.worker_done() && left > 0 {
        sched::sleep_ns(MS);
        left -= 1;
    }
    kassert!(d.worker_done(), "the dispatcher thread did not end");
    Ok(())
}
ktest!("fs.fat.prio", Safe, fat_prio);
