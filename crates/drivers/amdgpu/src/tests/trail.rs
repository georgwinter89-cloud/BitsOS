//! GX-10: the bring-up owner's trail (`gpucore::trail`): every access the device saw, in order, a poll as one wait.

use crate::gpucore::bringup::{Bringup, Gate, Outcome};
use crate::gpucore::psp::{self, Bound, Cond, Hw, PspError, Timeouts};
use crate::gpucore::trail::Access;
use std::collections::BTreeMap;
use std::vec::Vec;

/// Registers that hold what was written; `ready_at` makes register 0x40 read `0x8000_0000` from that time on.
#[derive(Default)]
struct Dev {
    regs: BTreeMap<u32, u32>,
    now: u64,
    ready_at: u64,
    refuse: Option<u32>,
    reads: u32,
    /// The clock stands still, whatever is waited.
    still: bool,
}

impl Hw for Dev {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.reads += 1;
        if byte == 0x40 {
            return Some(if self.now >= self.ready_at { 0x8000_0000 } else { 0x1234 });
        }
        Some(self.regs.get(&byte).copied().unwrap_or(0))
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        if self.refuse == Some(byte) {
            return false;
        }
        self.regs.insert(byte, value);
        true
    }
    fn mem_read(&mut self, _mc: u64, _buf: &mut [u8]) -> bool {
        true
    }
    fn mem_write(&mut self, _mc: u64, _data: &[u8]) -> bool {
        true
    }
    fn now_us(&mut self) -> u64 {
        self.now
    }
    fn delay_us(&mut self, us: u64) {
        if !self.still {
            self.now += us;
        }
    }
}

const T: Timeouts = Timeouts { reg_us: 1000, poll_us: 10, settle_us: 0, fence_us: 0, fence_poll_us: 1 };

fn trail<H: Hw>(o: &Bringup<H>) -> Vec<Access> {
    o.trail().unwrap().recent().copied().collect()
}

#[test]
fn an_owner_keeps_no_trail_unless_asked() {
    let mut o = Bringup::new(Dev::default(), Gate::Allow, 16);
    let _ = o.reg_read(0x10);
    assert!(o.reg_write(0x10, 1));
    psp::wait_for(&mut o, &T, 0x40, 0x8000_0000, 0x8000_0000, "t").unwrap();
    assert!(o.trail().is_none());
    // The owner asks the clock nothing for a wait it does not keep: three reads, one of them the write's read-back.
    assert_eq!(o.hw().reads, 3);
}

#[test]
fn reads_writes_and_a_poll_as_one_wait_in_order() {
    let dev = Dev { ready_at: 55, ..Dev::default() };
    let mut o = Bringup::new(dev, Gate::Allow, 16).with_trail(64).without_read_back();
    assert_eq!(o.reg_read(0x10), Some(0));
    assert!(o.reg_write(0x10, 7));
    let cond = Cond::Masked { mask: 0x8000_ffff, value: 0x8000_0000 };
    assert_eq!(psp::wait_until(&mut o, &T, 0x40, cond, "t"), Ok(0x8000_0000));
    assert_eq!(o.reg_read(0x10), Some(7));
    assert_eq!(
        trail(&o),
        [
            Access::Read { byte: 0x10, value: Some(0) },
            Access::Write { byte: 0x10, value: 7, reached: true },
            Access::Wait { byte: 0x40, cond, bound: Bound::Time { us: 1000, step_us: 10 }, reads: 7, last: Some(0x8000_0000), us: 60 },
            Access::Read { byte: 0x10, value: Some(7) },
        ]
    );
    assert_eq!(o.trail().unwrap().lost(), 0);
    // No read-back: the write log says so, and the device saw four plain reads less the wait's seven.
    assert_eq!(o.log().recent().next().map(|e| (e.read_back, e.outcome)), Some((None, Outcome::Ok)));
    assert_eq!(o.hw().reads, 2 + 7);
}

#[test]
fn a_read_back_is_an_access_the_trail_shows() {
    let mut o = Bringup::new(Dev::default(), Gate::Allow, 16).with_trail(64);
    assert!(o.reg_write(0x10, 7));
    assert_eq!(trail(&o), [Access::Write { byte: 0x10, value: 7, reached: true }, Access::Read { byte: 0x10, value: Some(7) }]);
}

#[test]
fn refused_writes_and_waits_that_run_out() {
    // A device that refuses, and a gate that is shut: tried, not reached.
    let dev = Dev { refuse: Some(0x20), ready_at: u64::MAX, ..Dev::default() };
    let mut o = Bringup::new(dev, Gate::Allow, 16).with_trail(64).without_read_back();
    assert!(!o.reg_write(0x20, 1));
    let cond = Cond::Changed { from: 0x1234 };
    assert_eq!(psp::wait_until(&mut o, &T, 0x40, cond, "t"), Err(PspError::Timeout { what: "t", last: 0x1234 }));
    let t = trail(&o);
    assert_eq!(t[0], Access::Write { byte: 0x20, value: 1, reached: false });
    assert!(matches!(t[1], Access::Wait { byte: 0x40, cond: c, reads: 101, last: Some(0x1234), us: 1000, .. } if c == cond), "{:?}", t[1]);
    assert_eq!(t.len(), 2);
    let mut shut = Bringup::new(Dev::default(), Gate::Refuse("test"), 16).with_trail(64);
    assert!(!shut.reg_write(0x10, 1));
    assert_eq!(trail(&shut), [Access::Write { byte: 0x10, value: 1, reached: false }]);
}

#[test]
fn a_long_poll_is_one_entry_and_a_full_trail_counts_what_it_lost() {
    let dev = Dev { ready_at: 900, ..Dev::default() };
    let mut o = Bringup::new(dev, Gate::Allow, 16).with_trail(4).without_read_back();
    psp::wait_for(&mut o, &T, 0x40, 0x8000_0000, 0x8000_0000, "t").unwrap();
    assert_eq!(o.trail().unwrap().lost(), 0, "91 reads, one entry");
    assert!(matches!(trail(&o)[..], [Access::Wait { reads: 91, .. }]));
    for i in 0..6 {
        let _ = o.reg_read(0x100 + i * 4);
    }
    assert_eq!(o.trail().unwrap().lost(), 3);
    assert_eq!(trail(&o).len(), 4);
    // A wait pushed out while it runs: its later reads are plain reads, and the count says what went.
    let mut o = Bringup::new(Dev { ready_at: 30, ..Dev::default() }, Gate::Allow, 16).with_trail(1).without_read_back();
    o.wait_begin(0x40, Cond::Changed { from: 0 }, Bound::Reads(3));
    let _ = o.reg_read(0x10);
    let _ = o.reg_read(0x40);
    o.wait_end();
    assert_eq!(trail(&o), [Access::Read { byte: 0x40, value: Some(0x1234) }]);
    assert_eq!(o.trail().unwrap().lost(), 2);
}

#[test]
fn a_wait_in_linuxs_form_is_bounded_by_reads_and_asks_no_clock() {
    // psp_wait_for (amdgpu_psp.c:621-630): read, test, udelay(1), `reads` times. The answer comes at read 41.
    let mut o = Bringup::new(Dev { ready_at: 40, ..Dev::default() }, Gate::Allow, 16).with_trail(64).without_read_back();
    let cond = Cond::Masked { mask: 0x8000_ffff, value: 0x8000_0000 };
    assert_eq!(psp::wait_reads(&mut o, 0x40, cond, 100, "t"), Ok(0x8000_0000));
    assert_eq!(trail(&o), [Access::Wait { byte: 0x40, cond, bound: Bound::Reads(100), reads: 41, last: Some(0x8000_0000), us: 40 }]);
    // One read too few for this device: given up after exactly that many reads, 1 us apart.
    let mut o = Bringup::new(Dev { ready_at: 40, ..Dev::default() }, Gate::Allow, 16).with_trail(64).without_read_back();
    assert_eq!(psp::wait_reads(&mut o, 0x40, cond, 40, "t"), Err(PspError::Timeout { what: "t", last: 0x1234 }));
    assert_eq!(trail(&o), [Access::Wait { byte: 0x40, cond, bound: Bound::Reads(40), reads: 40, last: Some(0x1234), us: 40 }]);
    assert_eq!(o.hw().reads, 40);
    // A clock that stands still cannot make it endless, and no read at all is a timeout, not an answer.
    let mut o = Bringup::new(Dev { ready_at: 1, still: true, ..Dev::default() }, Gate::Allow, 16).with_trail(64).without_read_back();
    assert_eq!(psp::wait_reads(&mut o, 0x40, cond, psp::LINUX_POLLS, "t"), Err(PspError::Timeout { what: "t", last: 0x1234 }));
    assert_eq!(o.hw().reads, psp::LINUX_POLLS);
    assert_eq!(psp::wait_reads(&mut o, 0x40, cond, 0, "t"), Err(PspError::Timeout { what: "t", last: 0 }));
    assert_eq!(o.hw().reads, psp::LINUX_POLLS);
}

#[test]
fn a_sleep_is_in_the_trail_where_it_was_made() {
    let mut o = Bringup::new(Dev { ready_at: 20_500, ..Dev::default() }, Gate::Allow, 16).with_trail(64).without_read_back();
    assert!(o.reg_write(0x10, 2));
    o.delay_us(20_000);
    let cond = Cond::Masked { mask: 0x8000_0000, value: 0x8000_0000 };
    // The poll's own steps are the wait, not sleeps, also a step of a millisecond.
    let slow = Timeouts { reg_us: 10_000, poll_us: 1000, ..T };
    psp::wait_until(&mut o, &slow, 0x40, cond, "t").unwrap();
    // Short delays are a memory poll's steps (a fence): no sleep. Delays in a row are one sleep.
    for _ in 0..50 {
        o.delay_us(100);
    }
    o.delay_us(1000);
    o.delay_us(1500);
    let _ = o.reg_read(0x10);
    assert_eq!(
        trail(&o),
        [
            Access::Write { byte: 0x10, value: 2, reached: true },
            Access::Sleep { us: 20_000 },
            Access::Wait { byte: 0x40, cond, bound: Bound::Time { us: 10_000, step_us: 1000 }, reads: 2, last: Some(0x8000_0000), us: 1000 },
            Access::Sleep { us: 2500 },
            Access::Read { byte: 0x10, value: Some(2) },
        ]
    );
    assert_eq!(o.hw().now, 20_000 + 1000 + 5000 + 2500, "every delay reached the device's clock");
    // An owner without a trail hands the delay on and keeps nothing.
    let mut plain = Bringup::new(Dev::default(), Gate::Allow, 16);
    plain.delay_us(20_000);
    assert!(plain.trail().is_none() && plain.hw().now == 20_000);
}
