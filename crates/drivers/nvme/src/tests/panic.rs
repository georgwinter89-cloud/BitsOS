//! CRASHDISK: the panic writer against the fake controller: reset, its own queues, Write with FUA only inside
//! the boot ESP's window, then Flush; a run outside it writes nothing and resets nothing; a closed window or a
//! second write refuses.

use super::*;
use crate::panic::PanicWriter;
use core::cell::Cell;
use kexec::crash::disk::Run;

const ESP: (u64, u64) = (100, 900);

fn armed_ctrl() -> (Arc<Controller<FakeHw>>, FakeHw) {
    let (c, hw) = up(vec![fake::ns(1, 9, 4096)]);
    c.arm_window(1, 2048, 1024);
    assert!(c.open_boot_window(1, ESP.0, ESP.1));
    (c, hw)
}

fn clock() -> impl Fn() -> u64 {
    let t = Cell::new(0u64);
    move || {
        t.set(t.get() + 1000);
        t.get()
    }
}

#[test]
fn writes_only_the_esp_with_its_own_queues() {
    let (c, hw) = armed_ctrl();
    assert!(PanicWriter::new(c.clone(), 2, 4096).is_err(), "no such namespace window");
    let w = PanicWriter::new(c.clone(), 1, 256 << 10).unwrap();
    assert_eq!((w.capacity(), w.block_size(), w.allowed()), (256 << 10, 512, ESP));
    let data: Vec<u8> = (0..256u32 << 10).map(|i| (i * 7 + (i >> 9)) as u8).collect();
    assert!(w.put(0, &data) && !w.put((256 << 10) - 1, &[0, 0]));
    let mut back = vec![0u8; 16];
    assert!(w.get(100, &mut back) && back == data[100..116]);
    let enables = hw.st.lock().enables;
    // Two runs: 16 blocks at 200, then the rest (496 blocks, more than one command) at 300.
    let runs = [Run { lba: 200, off: 0, blocks: 16 }, Run { lba: 300, off: 16 * 512, blocks: 496 }];
    w.write(&runs, &clock(), 1_000_000_000).unwrap();
    let st = hw.st.lock();
    assert_eq!(st.enables, enables + 1, "one reset");
    let ns = &st.ns[0].data;
    assert_eq!(&ns[200 * 512..216 * 512], &data[..16 * 512]);
    assert_eq!(&ns[300 * 512..796 * 512], &data[16 * 512..]);
    assert!(ns[216 * 512..300 * 512].iter().enumerate().all(|(i, &b)| b == fake::pattern(1, (216 * 512 + i) as u64)), "between the runs untouched");
    assert!(st.writes.len() > 2 && st.writes.iter().all(|x| x.fua && x.nsid == 1 && x.lba >= ESP.0 && x.lba + x.blocks <= ESP.0 + ESP.1));
    assert_eq!(st.writes.iter().map(|x| x.blocks).sum::<u64>(), 512);
    assert!(st.seen_io.last() == Some(&0x00), "a flush ends it");
    drop(st);
    assert!(w.write(&runs, &clock(), 1_000_000_000).is_err(), "one panic write per boot");
}

#[test]
fn a_run_outside_writes_and_resets_nothing() {
    let (c, hw) = armed_ctrl();
    let w = PanicWriter::new(c.clone(), 1, 64 << 10).unwrap();
    let before = { let st = hw.st.lock(); (st.enables, st.writes.len(), st.ns[0].data.clone()) };
    for bad in [
        [Run { lba: 200, off: 0, blocks: 8 }, Run { lba: ESP.0 + ESP.1 - 4, off: 4096, blocks: 8 }],
        [Run { lba: 200, off: 0, blocks: 8 }, Run { lba: 2048, off: 4096, blocks: 8 }],
        [Run { lba: 0, off: 0, blocks: 1 }, Run { lba: 200, off: 0, blocks: 1 }],
        [Run { lba: 200, off: 0, blocks: 8 }, Run { lba: 300, off: 60 << 10, blocks: 16 }],
    ] {
        assert!(w.write(&bad, &clock(), 1_000_000_000).is_err(), "{bad:?}");
    }
    let st = hw.st.lock();
    assert_eq!((st.enables, st.writes.len()), (before.0, before.1));
    assert!(st.ns[0].data == before.2);
}

#[test]
fn a_closed_or_changed_window_refuses() {
    let (c, hw) = armed_ctrl();
    let w = PanicWriter::new(c.clone(), 1, 4096).unwrap();
    assert!(w.ready().is_ok());
    c.close_boot_window();
    assert!(w.ready().is_err());
    assert!(c.open_boot_window(1, ESP.0, ESP.1));
    assert!(w.ready().is_err(), "opened again: another window than the one at boot");
    let n = hw.st.lock().writes.len();
    assert!(w.write(&[Run { lba: 200, off: 0, blocks: 8 }], &clock(), 1_000_000_000).is_err());
    assert_eq!(hw.st.lock().writes.len(), n);
    let (c2, _) = up(vec![fake::ns(1, 9, 4096)]);
    c2.arm_window(1, 2048, 1024);
    assert!(PanicWriter::new(c2, 1, 4096).is_err(), "no boot window: no writer");
}

#[test]
fn a_controller_that_never_gets_ready_ends_in_time() {
    let (c, hw) = armed_ctrl();
    let w = PanicWriter::new(c.clone(), 1, 4096).unwrap();
    hw.st.lock().inject.never_ready = true;
    let e = w.write(&[Run { lba: 200, off: 0, blocks: 8 }], &clock(), 50_000_000).unwrap_err();
    assert_eq!(e.stage, "not ready after the enable");
    assert!(hw.st.lock().writes.is_empty());
}
