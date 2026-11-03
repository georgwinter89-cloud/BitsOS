//! RB-14 host tests: what the storage stop sees of a controller (`shutdown::Activity`) against the fake: bulk
//! transfers in flight while a stick NAKs, none once they ended, and the bounded wait giving up on its deadline.

use crate::fake::Cfg;
use crate::fake_usb::FakeDev;
use crate::hc::{Source, MS};
use crate::tests_usb::{devs, up, C};
use alloc::sync::Arc;
use core::cell::Cell;
use std::vec::Vec;
use usb_core::{Device, Request, Speed, XferResult};

const OUT: u8 = 0x02;

fn stick(c: &C) -> Arc<Device> {
    devs(c).into_iter().find(|d| d.config.as_ref().is_some_and(|x| x.defaults().any(|f| f.class == 8))).expect("a stick")
}

#[test]
fn bulk_transfers_in_flight_keep_the_stop_waiting_until_they_end() {
    let (c, env, _hp) = up("t-sd-bulk", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let quiet = c.activity();
    assert!(quiet.storage_idle(), "{}", quiet.describe());
    let path = [1u8];
    env.model().dev(&path).unwrap().nak_out.push(OUT);
    let ts: Vec<_> = (0..3).map(|_| d.submit(OUT, Request::new(d.alloc_buffer(512).unwrap(), 0, 512)).unwrap()).collect();
    let a = c.activity();
    assert_eq!((a.bulk, a.zombies, a.storage_idle()), (3, 0, false), "{}", a.describe());
    // The wait gives up at its deadline (a fake clock: each look advances 1 ms; a budget of 5 ms).
    let t = Cell::new(0u64);
    let now = || {
        t.set(t.get() + MS);
        t.get()
    };
    let (seen, waited) = c.wait_storage_idle(5 * MS, &now, &|_| {});
    assert!(!seen.storage_idle() && waited <= 6 * MS, "waited {waited} ns: {}", seen.describe());
    env.model().dev(&path).unwrap().nak_out.clear();
    env.model().pump();
    c.process_events(Source::Waiter);
    for t in &ts {
        assert_eq!(t.wait(1000 * MS), XferResult::ok(512));
    }
    let a = c.activity();
    assert!(a.storage_idle(), "{}", a.describe());
}
