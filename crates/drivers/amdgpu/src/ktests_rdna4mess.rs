//! The measurement run (`rdna4::messung`) in the kernel, on the fake card under the kernel's own locks, clock and
//! stack: from a cold card to the end every stage equal, the protocol whole, the power-off asked for once (a
//! recorded call here), each stage within the stage budget below the run's entry; a difference injected into
//! gchub stops the run there, no later stage of the chain is asked, and the power-off still comes.

use crate::devs::Key;
use crate::rdna4::fakefiles::model;
use crate::rdna4::fakemodel::STAGE_STACK;
use crate::rdna4::messfake::{Fake, Spoil};
use crate::rdna4::messung::{self, Disk, Env, RUN};
use crate::rdna4::stage::Stage;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use ktest::kassert;

#[derive(Default)]
struct Paper(String);

impl Disk for Paper {
    fn open(&mut self, head: &str) -> Result<String, String> {
        self.0 = head.to_string();
        Ok(String::from("GPUM0001.TXT"))
    }
    fn put(&mut self, text: &str) -> Result<(), String> {
        self.0.push_str(text);
        Ok(())
    }
}

#[derive(Default)]
struct Room {
    offs: usize,
}

impl Env for Room {
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    fn log(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn levels(&mut self) -> String {
        String::from("ktest")
    }
    fn build(&mut self) -> String {
        String::from("ktest")
    }
    fn say(&mut self, _line: &str) {}
    fn wait_s(&mut self, _s: u64) {}
    fn poweroff(&mut self) -> String {
        self.offs += 1;
        String::from("a ktest: recorded, not done")
    }
}

fn fake() -> Result<Fake, &'static str> {
    Fake::new(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), model()).ok_or("the measured card did not attach")
}

fn run() -> ktest::TestResult {
    let mut card = fake()?;
    let (mut disk, mut room) = (Paper::default(), Room::default());
    let out = messung::go(&mut card, &mut disk, &mut room);
    kassert!(out.stopped.is_none(), "the run stopped: {:?}", out.stopped);
    kassert!(out.ran == RUN, "the run's stages: {:?}", out.ran);
    kassert!(room.offs == 1, "the power-off was asked for {} times", room.offs);
    let text = &disk.0;
    kassert!(text.lines().filter(|l| l.starts_with("WEITER ")).count() == RUN.len(), "not every stage went on");
    kassert!(text.lines().last().is_some_and(|l| l.starts_with("ENDE nach=ringtest")), "the last line is not the end");
    for l in text.lines().filter(|l| l.starts_with("STAPEL ")) {
        let bytes: usize = l.rsplit('=').next().and_then(|b| b.parse().ok()).ok_or("a STAPEL line without bytes")?;
        kassert!(bytes <= STAGE_STACK, "{l}: over the stage budget of {STAGE_STACK} bytes below the run's entry");
    }
    // A difference in the middle: the run ends at gchub, sdma and the rungs are never asked.
    let mut card = fake()?;
    card.spoil = Some(Spoil { stage: Stage::GcHub, byte: 0x2825 * 4, nth: 1, xor: 0x10 });
    let (mut disk, mut room) = (Paper::default(), Room::default());
    let out = messung::go(&mut card, &mut disk, &mut room);
    kassert!(out.stopped.as_ref().is_some_and(|s| s.0 == "gchub"), "the injected difference did not stop the run at gchub: {:?}", out.stopped);
    kassert!(card.calls.iter().all(|(s, _)| !matches!(s, Stage::GcCp | Stage::GcRing | Stage::Sdma | Stage::SdmaTest | Stage::RingTest)), "a stage after the stop was asked");
    kassert!(room.offs == 1, "no power-off after the stop");
    Ok(())
}
ktest::ktest!("gx.rdna4.messung", Safe, run);
