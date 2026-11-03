//! RB-14 host tests of the storage stop (`shutdown`): inline disks over RAM disks, no scheduler, so every wait
//! below finds its disk quiet at once or has a budget of 0.

use crate::disk::{Data, Disk, Dispatch};
use crate::error::BlockError;
use crate::policy::DiskKind;
use crate::queue::Kind;
use crate::ramdisk::{CallOp, RamDisk};
use crate::shutdown::{stop_these, Flushed, LayerNote, Stage, Stop};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::IoPriority::Normal;
use std::sync::Mutex;

fn disk(name: &str) -> (Arc<RamDisk>, Arc<Disk>) {
    let r = Arc::new(RamDisk::new(name, 512, 256));
    r.record();
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    (r, d)
}

fn ops(r: &RamDisk) -> Vec<CallOp> {
    r.calls().iter().map(|c| c.op).collect()
}

struct Layer {
    name: &'static str,
    log: Arc<Mutex<Vec<String>>>,
    disks: Vec<Arc<Disk>>,
}

impl Stop for Layer {
    fn name(&self) -> &str {
        self.name
    }
    fn stop(&self, _deadline_ns: u64) -> LayerNote {
        // A layer runs after every disk is closed and flushed.
        let closed = self.disks.iter().all(|d| d.is_closed() && d.pending() == (0, 0));
        self.log.lock().unwrap().push(alloc::format!("{} closed={closed}", self.name));
        LayerNote { idle: true, detail: String::new() }
    }
    fn resume(&self) {
        self.log.lock().unwrap().push(alloc::format!("{} resumed", self.name));
    }
}

#[test]
fn a_written_disk_is_flushed_after_its_last_write_and_refuses_new_ones() {
    let (r, d) = disk("st0");
    d.write(3, &[7u8; 1024], false, Normal).unwrap();
    assert_eq!((d.write_state().0, d.write_state().1), (true, true), "written and dirty");
    let sum = stop_these("test", &[d.clone()], Vec::new(), 0);
    let s = &sum.disks[0];
    assert_eq!((s.drained, s.dirty, s.flush, s.queued, s.at_device), (true, true, Flushed::Done, 0, 0));
    assert!(sum.quiet);
    assert_eq!(ops(&r).last(), Some(&CallOp::Flush), "the cache flush is the device's last call");
    assert!(!d.write_state().1, "clean after the flush");
    let n = r.calls().len();
    // Closed: a new request is held back, never reaches the device, and its waiter keeps its own deadline.
    let t = d.submit(Kind::Write { fua: false, preflush: false }, 3, 0, Data::Bytes(vec![1u8; 512]), Normal).unwrap();
    assert_eq!((d.held(), d.pending()), (1, (0, 0)), "held, not queued: the stop does not wait for it");
    let late = d.submit(Kind::Flush, 0, 0, Data::None, Normal).unwrap();
    assert_eq!(late.wait_timeout(1).err(), Some(BlockError::Timeout), "a held request's waiter gives up at its deadline");
    assert_eq!(d.held(), 1, "and takes it out of the held ones");
    assert_eq!(r.calls().len(), n, "nothing reached the device while closed");
    d.reopen();
    assert_eq!(d.held(), 0);
    t.wait().unwrap();
    d.write(4, &[1u8; 512], false, Normal).unwrap();
    assert_eq!(r.calls().len(), n + 2, "reopened: the held write and new ones reach the device");
    assert_eq!(&r.snapshot()[3 * 512..4 * 512], &[1u8; 512][..], "the held write landed after the reopen");
}

#[test]
fn a_write_at_the_device_when_the_disk_closes_is_flushed_once_it_completes() {
    // The install case: the boot's first write is still at the stick when the stop begins, so the disk has no
    // completed write at the close. Its flush must come anyway, after the write.
    let (r, d) = disk("st4");
    r.pause();
    let w = {
        let d = d.clone();
        std::thread::spawn(move || d.write(7, &[5u8; 512], false, Normal))
    };
    while !r.holding() {
        std::thread::yield_now();
    }
    assert!(!d.write_state().0, "nothing completed yet");
    let stop = {
        let d = d.clone();
        std::thread::spawn(move || stop_these("test", &[d], Vec::new(), 10_000_000_000))
    };
    while !d.is_closed() {
        std::thread::yield_now();
    }
    r.resume();
    let sum = stop.join().unwrap();
    w.join().unwrap().unwrap();
    let s = &sum.disks[0];
    assert_eq!((s.at_device, s.drained, s.dirty, s.flush), (1, true, true, Flushed::Done), "{s:?}");
    assert!(s.since_write_ms.is_some());
    let o = ops(&r);
    assert_eq!(o.last(), Some(&CallOp::Flush), "the flush follows the drained write: {o:?}");
    assert!(o.contains(&CallOp::Write { fua: false }));
    assert!(sum.quiet);
}

#[test]
fn an_unregistered_disk_fails_its_held_requests() {
    let (r, d) = disk("st5");
    let n = r.calls().len();
    d.close();
    let t = d.submit(Kind::Write { fua: false, preflush: false }, 1, 0, Data::Bytes(vec![3u8; 512]), Normal).unwrap();
    d.stop();
    assert_eq!(t.wait().err(), Some(BlockError::Gone));
    assert_eq!(r.calls().len(), n, "the held write never reached the device");
}

#[test]
fn a_disk_never_written_is_not_flushed() {
    let (r, d) = disk("st1");
    let _ = d.read_vec(0, 512, Normal).unwrap();
    let sum = stop_these("test", &[d.clone()], Vec::new(), 0);
    assert_eq!(sum.disks[0].flush, Flushed::NotWritten);
    assert_eq!(sum.disks[0].since_write_ms, None);
    assert!(!ops(&r).contains(&CallOp::Flush), "NVMe with writes off gets no command from the stop");
    assert!(sum.quiet);
}

#[test]
fn a_fua_write_alone_leaves_the_cache_clean_but_the_stop_still_syncs() {
    let (r, d) = disk("st2");
    d.write(5, &[9u8; 512], true, Normal).unwrap();
    assert_eq!((d.write_state().0, d.write_state().1), (true, false));
    let sum = stop_these("test", &[d.clone()], Vec::new(), 0);
    assert_eq!((sum.disks[0].dirty, sum.disks[0].flush), (false, Flushed::Done));
    assert_eq!(ops(&r).last(), Some(&CallOp::Flush), "Linux's sd_shutdown syncs whenever the cache may hold data");
}

#[test]
fn a_request_still_queued_at_the_deadline_fails_and_the_flush_follows() {
    let (r, d) = disk("st3");
    d.write(1, &[2u8; 512], false, Normal).unwrap();
    // Inline disk: nobody dispatches this one, so it is still queued when the stop's budget (0) is over.
    let t = d.submit(Kind::Write { fua: false, preflush: false }, 9, 0, Data::Bytes(vec![4u8; 512]), Normal).unwrap();
    let sum = stop_these("test", &[d.clone()], Vec::new(), 0);
    let s = &sum.disks[0];
    assert_eq!((s.queued, s.failed_queued, s.drained, s.flush), (1, 1, true, Flushed::Done));
    assert_eq!(t.wait().err(), Some(BlockError::Gone), "the queued write never reached the device");
    assert!(!r.calls().iter().any(|c| c.op == CallOp::Write { fua: false } && c.lba == 9));
    assert!(!sum.quiet, "a write lost at the deadline is not a quiet stop");
}

#[test]
fn disks_stop_newest_first_then_the_layers_by_stage() {
    // Registration order: the stick, then /data's image on it (a stacked disk): the image must flush while the
    // stick still takes its writes.
    let (r_stick, stick) = disk("stick");
    let (r_img, img) = disk("image");
    stick.write(0, &[1u8; 512], false, Normal).unwrap();
    img.write(0, &[1u8; 512], false, Normal).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let both = vec![stick.clone(), img.clone()];
    let host: Arc<dyn Stop> = Arc::new(Layer { name: "host", log: log.clone(), disks: both.clone() });
    let dev: Arc<dyn Stop> = Arc::new(Layer { name: "device", log: log.clone(), disks: both.clone() });
    // Registered host first (xHCI binds before usb-msd sees a device): the stage decides, not the order.
    let sum = stop_these("test", &both, vec![(Stage::Host, host), (Stage::Device, dev)], 0);
    let names: Vec<&str> = sum.disks.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["image", "stick"]);
    assert_eq!(*log.lock().unwrap(), ["device closed=true", "host closed=true"]);
    assert_eq!(ops(&r_img).last(), Some(&CallOp::Flush));
    assert_eq!(ops(&r_stick).last(), Some(&CallOp::Flush));
    assert!(sum.quiet);
}
