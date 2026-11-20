//! GP-GPU2: a clock that stands still cannot make a poll of the RX 9070 XT's stages endless: the power
//! controller's answer poll (`smu14`) and the secure processor's waits are Linux's, a count of reads and no clock
//! at all (`psp::wait_reads`). Without the counts neither test would return.

use super::rdna4fix::*;
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::smu14::{self, ANSWER_READS, SmuStop};
use crate::rdna4::stage::Stage;

/// The model behind a clock that never moves; `mute` reads as 0 whatever the model holds. Delays still reach the
/// model, so its own latencies pass.
struct Still {
    m: Model,
    mute: u32,
    reads: u64,
}

/// The autoload stage needs nothing of the host side: every method refuses.
impl crate::rdna4::host::Host for Still {}

impl Hw for Still {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        if byte == self.mute {
            self.reads += 1;
            return Some(0);
        }
        self.m.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.m.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.m.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.m.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        0
    }
    fn delay_us(&mut self, us: u64) {
        self.m.delay_us(us)
    }
}

#[test]
fn the_power_controllers_answer_poll_ends_without_a_clock() {
    let rig = Rig::new();
    let regs = Regs14::from(&rig.att.discovery).expect("the card's table names the registers");
    let mut hw = Still { m: model(), mute: regs.smu_answer, reads: 0 };
    let mut sent = false;
    // The boot's first message: no pre-poll, then the answer never comes.
    assert_eq!(smu14::ask(&mut hw, &regs, 0x2, 0, true, &mut sent), Err(SmuStop::Timeout { msg: 0x2 }));
    assert!(sent);
    // Linux's bound: `default_timeout` reads (smu_v14_0_2_ppt.c:2154), each followed by udelay(1).
    assert_eq!(hw.reads, u64::from(ANSWER_READS));
    // A later message: the pre-poll finds the mailbox busy after as many reads and nothing is sent.
    let mut sent = false;
    assert_eq!(smu14::ask(&mut hw, &regs, 0x2, 0, false, &mut sent), Err(SmuStop::Busy { last: 0 }));
    assert!(!sent);
    assert_eq!(hw.reads, 2 * u64::from(ANSWER_READS));
}

#[test]
fn the_secure_processors_waits_end_without_a_clock() {
    let rig = Rig::new();
    let regs = Regs14::from(&rig.att.discovery).expect("the card's table names the registers");
    // A bootloader that never shows ready, on a clock that stands still: Linux's ten rounds of 100000 reads.
    let (mut s, m) = rig.up_to(&[]);
    let hw = Still { m, mute: regs.psp.c2pmsg_35, reads: 0 };
    let (d, hw) = rig.run_on(&mut s, Stage::Bootloader, hw, &mut Stick::full());
    assert!(!d.pass && d.line.contains("Startlader der Karte meldet sich nicht bereit"), "{}", d.line);
    assert_eq!(hw.reads, 1_000_000);
    // The same before the first part: nothing was sent, so the loads may be tried again.
    let hw = Still { m: hw.m, mute: regs.psp.c2pmsg_35, reads: 0 };
    let (d, hw) = rig.run_on(&mut s, Stage::Sos, hw, &mut Stick::full());
    assert!(!d.pass && d.line.contains("Schlüsselliste") && hw.reads == 1_000_000 && hw.m.writes.is_empty(), "{}", d.line);
    assert_eq!((s.sos, s.boot_sent()), (None, 0));
    let (done, _) = rig.pass(&mut s, hw.m, &mut Stick::full(), &[Stage::Sos]);
    assert!(done[0].pass);
    // A secure OS that never shows ready for a ring.
    let (mut s, m) = rig.up_to(&[Stage::Sos]);
    let writes = m.writes.len();
    let hw = Still { m, mute: regs.psp.c2pmsg_64, reads: 0 };
    let (d, hw) = rig.run_on(&mut s, Stage::Ring, hw, &mut Stick::full());
    assert!(!d.pass && d.line.contains("antwortet nicht rechtzeitig"), "{}", d.line);
    assert_eq!((hw.reads, hw.m.writes.len()), (100_000, writes));
}
