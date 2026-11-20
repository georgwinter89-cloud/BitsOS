//! FS-W: who waits for a file's step at the device (`Volume::wait_flight`) and has a signal. A fallocate and a
//! freeze have changed nothing yet and give up with EINTR, as a change does while the volume is frozen; a truncate
//! stays (the VFS has cut its pages already, Linux's truncate waits for write-back without a way out too).

use super::flight::{pattern, read_all, slow, step, wait_inside, Opener, BOTH, MIB};
use super::util::*;
use super::write::file;
use crate::errno::EINTR;
use crate::inode::ROOT;
use std::cell::Cell;
use std::format;
use std::sync::atomic::Ordering;
use std::time::Duration;

std::thread_local! {
    /// This thread's program "has a signal" (the process layer's check, per thread as in the kernel).
    static SIGNAL: Cell<bool> = const { Cell::new(false) };
}

fn signalled() -> bool {
    SIGNAL.with(|s| s.get())
}

/// `f` on a thread of its own that has a signal pending.
fn with_a_signal<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> std::thread::JoinHandle<T> {
    vfs::dentry::set_interrupt_check(signalled);
    std::thread::spawn(move || {
        SIGNAL.with(|s| s.set(true));
        f()
    })
}

#[test]
fn a_signal_ends_the_wait_of_a_fallocate_and_a_freeze_not_of_a_truncate() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "64M", layout);
        let what = format!("{layout:?}");
        let f = file(&v, ROOT, "f");
        let data = pattern(MIB, 8);
        dev.shut.store(true, Ordering::Release);
        let open = Opener(dev.clone());
        let (w, wd) = (v.clone(), data.clone());
        let writer = std::thread::spawn(move || step(&w, f, 0, &wd, MIB as u64));
        wait_inside(&dev, 1);

        let w = v.clone();
        assert_eq!(with_a_signal(move || w.fallocate(f, 2 * MIB as u64, 8192)).join().unwrap(), Err(EINTR), "{what}: fallocate");
        let w = v.clone();
        assert_eq!(with_a_signal(move || w.freeze()).join().unwrap(), Err(EINTR), "{what}: freeze");
        assert!(!v.is_frozen(), "{what}: the given-up freeze left the volume frozen");
        let w = v.clone();
        let cut = with_a_signal(move || w.set_size(f, 5000));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!cut.is_finished(), "{what}: the truncate did not wait for the step");
        // None of them touched the step: it is still at the device.
        assert_eq!(dev.inside.load(Ordering::Acquire), 1, "{what}");

        drop(open);
        assert_eq!(writer.join().unwrap(), Ok(()), "{what}");
        assert_eq!(cut.join().unwrap(), Ok(()), "{what}: the truncate behind the step");
        assert!(read_all(&v, f) == data[..5000], "{what}");
        // The volume works on: nothing of the interrupted calls is left.
        assert_eq!(step(&v, f, 8192, &data[..4096], 8192 + 4096), Ok(()), "{what}");
        v.sync_fs().unwrap();
        let img = dev.mem.image();
        assert_clean(&s, &img, &what);
        let (_d2, v2) = mount(img);
        let f2 = lookup(&v2, "/f");
        assert_eq!(v2.stat(f2).unwrap().blocks512(&v2.geo), 3 * 8, "{what}: the interrupted fallocate left blocks");
    }
}
