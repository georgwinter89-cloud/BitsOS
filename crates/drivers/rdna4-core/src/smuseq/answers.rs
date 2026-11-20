//! `smuseq::Script` moved on by an answer: which step follows a message that was answered OK, with the value Linux
//! reads behind it (the feature masks decide which clocks have a table, a table's first answer how many levels
//! are asked).

use super::{At, ClockAt, Script, Step, Stopped, Tail};
use crate::smu14::{self, CLOCKS, DpmTable, GFXCLK, msg};
use alloc::vec::Vec;

impl Script {
    /// The clock after `c`, or the power-down that follows the last.
    fn after_clock(c: usize) -> At {
        if c + 1 < CLOCKS.len() { At::Clock(c + 1, ClockAt::Feature) } else { At::JpegDown }
    }

    /// A clock's table is whole (T:502-509: GFXCLK's top level is capped to the table's GameClockAc).
    fn clock_done(&mut self, c: usize) {
        if c == GFXCLK
            && let (Some(t), Some(table)) = (self.facts.table, self.facts.dpm.last_mut())
            && let Some(top) = table.mhz.last_mut()
            && t.game_clock_ac != 0
            && *top > u32::from(t.game_clock_ac)
        {
            *top = u32::from(t.game_clock_ac);
        }
        self.at = Script::after_clock(c);
    }

    /// A whole feature mask arrived while `at` asked for it.
    fn masked(&mut self, mask: u64) -> Result<(), Stopped> {
        self.facts.features = Some(mask);
        let on = |c: usize| mask & (1 << CLOCKS[c].feature) != 0;
        let direct = |c: usize, asking: ClockAt, question: ClockAt| if CLOCKS[c].asked_each_time { asking } else { question };
        self.at = match self.at {
            At::Xgmi => At::Supported,
            At::Supported => {
                self.facts.supported = Some(mask);
                At::Running
            }
            At::Running => {
                self.facts.dpm_running = Some(mask & smu14::DPM_FEATURES == smu14::DPM_FEATURES);
                At::VcnUp
            }
            At::Clock(c, ClockAt::Feature) if on(c) => {
                self.facts.dpm.push(DpmTable { clock: CLOCKS[c].name, ppclk: CLOCKS[c].ppclk, fine_grained: false, mhz: Vec::new(), from_pmfw: true });
                At::Clock(c, direct(c, ClockAt::CountAsk, ClockAt::Count))
            }
            At::Clock(c, ClockAt::Feature) => {
                self.facts.dpm.push(DpmTable { clock: CLOCKS[c].name, ppclk: CLOCKS[c].ppclk, fine_grained: false, mhz: Vec::new(), from_pmfw: false });
                Script::after_clock(c)
            }
            At::Clock(c, asking) if !on(c) => {
                let _ = asking;
                let why = Stopped::FeatureWentOff { clock: CLOCKS[c].name };
                self.stop(why);
                return Err(why);
            }
            At::Clock(c, ClockAt::CountAsk) => At::Clock(c, ClockAt::Count),
            At::Clock(c, ClockAt::FineAsk) => At::Clock(c, ClockAt::Fine),
            At::Clock(c, ClockAt::LevelAsk(i)) => At::Clock(c, ClockAt::Level(i)),
            other => other,
        };
        Ok(())
    }

    /// One answer of GetDpmFreqByIndex.
    fn freq(&mut self, c: usize, at: ClockAt, value: u32) -> Result<(), Stopped> {
        let direct = |asking: ClockAt, question: ClockAt| if CLOCKS[c].asked_each_time { asking } else { question };
        match at {
            ClockAt::Count => {
                // V:1321: the count is the answer without bit 31.
                self.count = value & !smu14::FINE_GRAINED;
                if self.count > smu14::MAX_DPM_LEVELS {
                    let why = Stopped::Levels { clock: CLOCKS[c].name, count: self.count };
                    self.stop(why);
                    return Err(why);
                }
                self.at = At::Clock(c, direct(ClockAt::FineAsk, ClockAt::Fine));
            }
            ClockAt::Fine => {
                if let Some(t) = self.facts.dpm.last_mut() {
                    t.fine_grained = value & smu14::FINE_GRAINED != 0;
                }
                if self.count == 0 {
                    self.clock_done(c);
                } else {
                    self.at = At::Clock(c, direct(ClockAt::LevelAsk(0), ClockAt::Level(0)));
                }
            }
            ClockAt::Level(i) => {
                if let Some(t) = self.facts.dpm.last_mut() {
                    t.mhz.push(value & !smu14::FINE_GRAINED);
                }
                if i + 1 < self.count {
                    self.at = At::Clock(c, direct(ClockAt::LevelAsk(i + 1), ClockAt::Level(i + 1)));
                } else {
                    self.clock_done(c);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The next step's message was answered OK; `value` is the argument register read after it, for a step
    /// that reads it.
    pub fn answered(&mut self, value: Option<u32>) -> Result<(), Stopped> {
        let Step::Send { msg, value: wants, .. } = self.next() else { return Err(Stopped::Stage) };
        if wants != value.is_some() {
            self.stop(Stopped::Stage);
            return Err(Stopped::Stage);
        }
        let v = value.unwrap_or(0);
        if self.stopped.is_some() {
            self.tail = match self.tail {
                Tail::JpegDown => {
                    self.facts.jpeg_up = false;
                    self.tail_from(Tail::VcnDown)
                }
                Tail::VcnDown => {
                    self.facts.vcn_up = false;
                    self.tail_from(Tail::High)
                }
                Tail::High => {
                    self.high = Some(v);
                    Tail::Low
                }
                Tail::Low => {
                    self.facts.read_back = Some(u64::from(self.high.take().unwrap_or(0)) << 32 | u64::from(v));
                    Tail::Over
                }
                Tail::Over => Tail::Over,
            };
            return Ok(());
        }
        // A feature pair: the high half, then the low one.
        if msg == msg::GET_RUNNING_SMU_FEATURES_HIGH {
            self.high = Some(v);
            return Ok(());
        }
        if msg == msg::GET_RUNNING_SMU_FEATURES_LOW {
            let mask = u64::from(self.high.take().unwrap_or(0)) << 32 | u64::from(v);
            return self.masked(mask);
        }
        self.at = match self.at {
            At::IfVersion => {
                self.facts.fw_if = Some(v);
                At::SmuVersion
            }
            At::SmuVersion => {
                self.facts.version = Some(v);
                At::DriverHigh
            }
            At::DriverHigh => At::DriverLow,
            At::DriverLow => At::ToolsHigh,
            At::ToolsHigh => At::ToolsLow,
            At::ToolsLow => At::Transfer,
            At::Transfer => At::Table,
            // T:1443-1444: a table with no link level sends nothing.
            At::Btc => if self.facts.pcie.1 == 0 { At::Enable } else { At::Pcie(0) },
            At::Pcie(i) => if i + 1 < self.facts.pcie.1 { At::Pcie(i + 1) } else { At::Enable },
            At::Enable => {
                self.facts.enabled = true;
                At::Xgmi
            }
            At::VcnUp => {
                self.facts.vcn_up = true;
                At::JpegUp
            }
            At::JpegUp => {
                self.facts.jpeg_up = true;
                At::Clock(0, ClockAt::Feature)
            }
            At::Clock(c, at) => return self.freq(c, at, v),
            At::JpegDown => {
                self.facts.jpeg_up = false;
                At::VcnDown
            }
            At::VcnDown => {
                self.facts.vcn_up = false;
                At::Thermal
            }
            At::AllowIh => At::Done,
            other => other,
        };
        Ok(())
    }
}
