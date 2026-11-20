//! Linux's start of the power controller as one sequence, message by message, fed with the answers:
//! `smu_hw_init` (`S:` pm/swsmu/amdgpu_smu.c:1955-2021) through `smu_start_smc_engine` (S:1916-1953) and
//! `smu_smc_hw_setup` (S:1698-1913) with SMU 14.0.2's functions (`V:` pm/swsmu/smu14/smu_v14_0.c, `T:`
//! pm/swsmu/smu14/smu_v14_0_2_ppt.c, `C:` pm/swsmu/smu_cmn.c). The register trace of Linux's own cold start on
//! this card holds exactly these 125 messages (plan/hw/gaming-pc-gpu-2026-11-18, init.txt lines 893-1883; the
//! table in plan/spec/gpu-rdna4.md section 6 has each with its line).
//!
//! | step | messages | Linux |
//! |---|---|---|
//! | versions | 0x03 GetDriverIfVersion, 0x02 GetSmuVersion | `smu_cmn_get_smc_version` C:1045-1081 |
//! | tables | 0x0E/0x0F the driver table's address, 0x10/0x11 the tools table's | V:560-598 |
//! | the PMFW's table | 0x12 TransferTableSmu2Dram, argument 1, then the copy out of VRAM | C:1109-1158 |
//! | calibration | 0x36 RunDcBtc | V:1478-1487 |
//! | link | 0x20 OverridePcieParameters per level | T:1411-1489 |
//! | features | 0x06 EnableAllSmuFeatures | V:649-654 |
//! | what runs | three times 0x0D/0x0C: the XGMI policy's question, the supported mask, `is_dpm_running` | S:1345, S:1857, S:1865 |
//! | clocks | 0x2A PowerUpVcn, 0x2C PowerUpJpeg, per clock 0x0D/0x0C and 0x1F GetDpmFreqByIndex, 0x2D, 0x2B | S:877-923, T:465-596, V:1293-1416 |
//! | thermal alert | four registers, then 0x4D AllowIHHostInterrupt | V:754-797, V:1808-1827 |
//!
//! SCPM is on (`scpm`), so no allowed-feature masks go out (S:1806-1812); 14.0.2 has no `write_pptable`,
//! `init_display_count`, `notify_display_change`, `set_min_dcef_deep_sleep`, `enable_uclk_shadow` or
//! `init_max_sustainable_clocks` (T:2916-2981), and its memory pool has no size (V:529-558): none of their
//! messages exist here.
//!
//! One `Script` is the stage's own; a second one, inside `smugate::Gate`, is moved only by what crossed the bus,
//! and refuses a write that is not this sequence's next step. Nothing here sends anything.

use crate::pcielink;
use crate::pptable14::{self, LINK_LEVELS, Pptable};
use crate::smu14::{self, CLOCKS, DpmTable, ThermalRange, msg};
use alloc::vec::Vec;

mod answers;

/// What Linux does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A message with its argument; `value`: Linux reads the argument register once the answer is OK.
    Send { msg: u16, arg: u32, value: bool },
    /// Linux copies the PMFW's table out of the driver table (C:1152-1155).
    Table,
    /// The thermal alert's four registers (`thm14`).
    Thermal,
    /// Nothing more: the start is done, or it stopped and what a stop leaves to do is done too.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClockAt {
    Feature,
    CountAsk,
    Count,
    FineAsk,
    Fine,
    LevelAsk(u32),
    Level(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    IfVersion,
    SmuVersion,
    DriverHigh,
    DriverLow,
    ToolsHigh,
    ToolsLow,
    Transfer,
    Table,
    Btc,
    Pcie(usize),
    Enable,
    Xgmi,
    Supported,
    Running,
    VcnUp,
    JpegUp,
    Clock(usize, ClockAt),
    JpegDown,
    VcnDown,
    Thermal,
    AllowIh,
    Done,
}

/// What is left to do after a stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
    JpegDown,
    VcnDown,
    High,
    Low,
    Over,
}

/// Why the start stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// A message was not answered OK (`answer` 0: not within Linux's bound).
    Answer { msg: u16, arg: u32, answer: u32 },
    Table(pptable14::Bad),
    /// A clock's feature ran when Linux asked for its table and not at a later question: Linux would go on
    /// with a level count nothing set (V:1303-1304 returns 0 and leaves the value).
    FeatureWentOff { clock: &'static str },
    /// More levels than Linux's table holds (`smu14::MAX_DPM_LEVELS`).
    Levels { clock: &'static str, count: u32 },
    /// The stage's own reason (a register or memory that does not read, a write that did not arrive, an answer
    /// register that did not read as answered): nothing more is sent.
    Stage,
}

/// The two tables the PMFW is told about, as MC addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tables {
    pub driver_mc: u64,
    pub tools_mc: u64,
}

/// What the start learned and did so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub fw_if: Option<u32>,
    pub version: Option<u32>,
    pub table: Option<Pptable>,
    /// OverridePcieParameters' arguments and how many of them there are.
    pub pcie: ([u32; LINK_LEVELS], usize),
    /// EnableAllSmuFeatures was written to the mailbox (whatever was answered), and it was answered OK.
    pub enable_sent: bool,
    pub enabled: bool,
    /// `smu_feature_get_enabled_mask`'s answer (S:1857) and `smu_is_dpm_running`'s (S:1865).
    pub supported: Option<u64>,
    pub dpm_running: Option<bool>,
    /// The last feature mask the PMFW answered whole.
    pub features: Option<u64>,
    pub vcn_up: bool,
    pub jpeg_up: bool,
    pub dpm: Vec<DpmTable>,
    pub thermal: Option<ThermalRange>,
    /// How many of the thermal alert's four registers the stage wrote (`thm14`): a stop after them leaves them
    /// as written, as Linux's error path does.
    pub alert_written: u8,
    /// The mask read back after a stop.
    pub read_back: Option<u64>,
}

impl Facts {
    pub const fn new() -> Facts {
        Facts {
            fw_if: None,
            version: None,
            table: None,
            pcie: ([0; LINK_LEVELS], 0),
            enable_sent: false,
            enabled: false,
            supported: None,
            dpm_running: None,
            features: None,
            vcn_up: false,
            jpeg_up: false,
            dpm: Vec::new(),
            thermal: None,
            alert_written: 0,
            read_back: None,
        }
    }
}

impl Default for Facts {
    fn default() -> Self {
        Facts::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    tables: Tables,
    /// The platform's link limit as (generation code, width code), `pcielink::caps`.
    caps: (u8, u8),
    at: At,
    /// The high half of a feature mask was answered; the low half is asked next.
    high: Option<u32>,
    /// The clock table being read.
    count: u32,
    stopped: Option<Stopped>,
    tail: Tail,
    pub facts: Facts,
}

impl Script {
    pub fn new(tables: Tables, caps: (u8, u8)) -> Script {
        Script { tables, caps, at: At::IfVersion, high: None, count: 0, stopped: None, tail: Tail::Over, facts: Facts::default() }
    }

    pub fn tables(&self) -> Tables {
        self.tables
    }

    pub fn stopped(&self) -> Option<Stopped> {
        self.stopped
    }

    /// Linux's `smu_hw_init` came to its end.
    pub fn passed(&self) -> bool {
        self.at == At::Done && self.stopped.is_none()
    }

    /// The feature pair's next half: GetRunningSmuFeaturesHigh first (C:832-840; GetEnabledSmuFeatures is not
    /// mapped for 14.0.2, T:94-150).
    fn pair(&self) -> Step {
        let msg = if self.high.is_none() { msg::GET_RUNNING_SMU_FEATURES_HIGH } else { msg::GET_RUNNING_SMU_FEATURES_LOW };
        Step::Send { msg, arg: 0, value: true }
    }

    pub fn next(&self) -> Step {
        if self.stopped.is_some() {
            return match self.tail {
                Tail::JpegDown => Step::Send { msg: msg::POWER_DOWN_JPEG, arg: 0, value: false },
                Tail::VcnDown => Step::Send { msg: msg::POWER_DOWN_VCN, arg: 0, value: false },
                Tail::High => Step::Send { msg: msg::GET_RUNNING_SMU_FEATURES_HIGH, arg: 0, value: true },
                Tail::Low => Step::Send { msg: msg::GET_RUNNING_SMU_FEATURES_LOW, arg: 0, value: true },
                Tail::Over => Step::End,
            };
        }
        let plain = |msg, arg| Step::Send { msg, arg, value: false };
        let asked = |msg, arg| Step::Send { msg, arg, value: true };
        match self.at {
            At::IfVersion => asked(msg::GET_DRIVER_IF_VERSION, 0),
            At::SmuVersion => asked(msg::GET_SMU_VERSION, 0),
            At::DriverHigh => plain(msg::SET_DRIVER_DRAM_ADDR_HIGH, (self.tables.driver_mc >> 32) as u32),
            At::DriverLow => plain(msg::SET_DRIVER_DRAM_ADDR_LOW, self.tables.driver_mc as u32),
            At::ToolsHigh => plain(msg::SET_TOOLS_DRAM_ADDR_HIGH, (self.tables.tools_mc >> 32) as u32),
            At::ToolsLow => plain(msg::SET_TOOLS_DRAM_ADDR_LOW, self.tables.tools_mc as u32),
            At::Transfer => plain(msg::TRANSFER_TABLE_SMU2DRAM, smu14::TABLE_COMBO_PPTABLE),
            At::Table => Step::Table,
            At::Btc => plain(msg::RUN_DC_BTC, 0),
            At::Pcie(i) => plain(msg::OVERRIDE_PCIE_PARAMETERS, self.facts.pcie.0[i]),
            At::Enable => plain(msg::ENABLE_ALL_SMU_FEATURES, 0),
            At::Xgmi | At::Supported | At::Running => self.pair(),
            At::VcnUp => plain(msg::POWER_UP_VCN, 0),
            At::JpegUp => plain(msg::POWER_UP_JPEG, 0),
            At::Clock(_, ClockAt::Feature | ClockAt::CountAsk | ClockAt::FineAsk | ClockAt::LevelAsk(_)) => self.pair(),
            At::Clock(c, ClockAt::Count | ClockAt::Fine) => asked(msg::GET_DPM_FREQ_BY_INDEX, smu14::freq_arg(CLOCKS[c].ppclk, smu14::LEVEL_COUNT)),
            At::Clock(c, ClockAt::Level(i)) => asked(msg::GET_DPM_FREQ_BY_INDEX, smu14::freq_arg(CLOCKS[c].ppclk, i)),
            At::JpegDown => plain(msg::POWER_DOWN_JPEG, 0),
            At::VcnDown => plain(msg::POWER_DOWN_VCN, 0),
            At::Thermal => Step::Thermal,
            At::AllowIh => plain(msg::ALLOW_IH_HOST_INTERRUPT, 0),
            At::Done => Step::End,
        }
    }

    /// The next step's message reached the mailbox.
    pub fn sent(&mut self) {
        if self.stopped.is_none() && self.at == At::Enable {
            self.facts.enable_sent = true;
        }
    }

    /// The first thing a stop leaves to do, from `from` on: what Linux's `err_out` powers down again
    /// (S:915-921), then the read-back of what runs, which is not Linux's but this driver's: after
    /// EnableAllSmuFeatures a stop says what the card was left with.
    fn tail_from(&self, from: Tail) -> Tail {
        let mut t = from;
        loop {
            let (wanted, then) = match t {
                Tail::JpegDown => (self.facts.jpeg_up, Tail::VcnDown),
                Tail::VcnDown => (self.facts.vcn_up, Tail::High),
                Tail::High => (self.facts.enable_sent, Tail::Over),
                Tail::Low | Tail::Over => return t,
            };
            if wanted {
                return t;
            }
            t = then;
        }
    }

    /// Stops the start. `silent`: the PMFW must get nothing more (no answer, or one Linux takes for a hang).
    fn stop_at(&mut self, why: Stopped, silent: bool) {
        if self.stopped.is_some() {
            // A second failure, on what the stop left to do. A power-down the PMFW answered with an error:
            // Linux drops PowerDownJpeg's result and powers VCN down all the same (S:912-921). A failed
            // read-back, a PMFW that takes nothing, or the stage's own trouble with the bus ends it.
            let answered = matches!(why, Stopped::Answer { .. }) && !silent;
            self.tail = match self.tail {
                Tail::JpegDown if answered => self.tail_from(Tail::VcnDown),
                Tail::VcnDown if answered => self.tail_from(Tail::High),
                _ => Tail::Over,
            };
            return;
        }
        // Only the power-down that failed is not tried a second time: after a failed PowerDownJpeg Linux still
        // sends PowerDownVcn (it drops the first one's result and falls into `err_out`, S:912-921).
        let from = match self.at {
            At::JpegDown => Tail::VcnDown,
            At::VcnDown => Tail::High,
            _ => Tail::JpegDown,
        };
        self.stopped = Some(why);
        self.tail = if silent { Tail::Over } else { self.tail_from(from) };
    }

    /// The sequence stops on an answer Linux's own code cannot go on with. Every `Script` fed the same answers
    /// stops the same way (the one in the stage's gate too), so what a stop leaves to do still follows.
    pub(crate) fn stop(&mut self, why: Stopped) {
        self.stop_at(why, false);
    }

    /// The stage stops and the PMFW gets nothing more: its answer register did not read as answered before a
    /// message (Linux then sends nothing, C:514-522), or the bus failed the stage (a register that does not
    /// read, a write that did not arrive, memory that does not read). The gate in the stage's guard saw none of
    /// that as a stop and would let nothing of a stop's rest through.
    pub fn mute(&mut self) {
        self.stop_at(Stopped::Stage, true);
    }

    /// The next step's message was answered with `answer`, not OK.
    pub fn failed(&mut self, answer: u32) {
        let Step::Send { msg, arg, .. } = self.next() else { return };
        self.stop_at(Stopped::Answer { msg, arg, answer }, !smu14::decode(answer).lets_send());
    }

    /// The PMFW's table as it lies in the driver table.
    pub fn table(&mut self, bytes: &[u8]) -> Result<(), Stopped> {
        if self.stopped.is_some() || self.at != At::Table {
            return Err(Stopped::Stage);
        }
        match pptable14::parse(bytes) {
            Ok(t) => {
                self.facts.pcie = pcielink::forced(&t.pcie, self.caps.0, self.caps.1);
                self.facts.thermal = Some(ThermalRange::of(&t));
                self.facts.table = Some(t);
                self.at = At::Btc;
                Ok(())
            }
            Err(bad) => {
                self.stop(Stopped::Table(bad));
                Err(Stopped::Table(bad))
            }
        }
    }

    /// The thermal alert's registers are written.
    pub fn thermal_done(&mut self) {
        if self.stopped.is_none() && self.at == At::Thermal {
            self.at = At::AllowIh;
        }
    }
}
