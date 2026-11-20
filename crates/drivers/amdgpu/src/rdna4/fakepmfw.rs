//! Wave 43: the model's power controller as Linux's start of it meets it (`smu_hw_init`,
//! ../pm/swsmu/amdgpu_smu.c:1955-2021). It answers as the real card answered Linux (the trace's section smu: the
//! versions, the feature mask after EnableAllSmuFeatures, every clock level, the thermal registers), and it
//! refuses, and keeps, what Linux would not do:
//!
//! - a message of the start before the secure processor loaded the SMU's firmware in this boot (Linux starts the
//!   power controller after `psp_load_fw`); the look's questions pass at any time;
//! - a new message whose sender did not read the answer register as answered since the message before it, the
//!   way `smu_msg_v1_send_msg` makes sure of it (smu_cmn.c:514-522, :532-533), but the first one after the
//!   firmware started (that Linux's own pre-poll comes right before each message is the trace comparison's to
//!   hold, not the model's: a look outside Linux's sequence may have sent the message before);
//! - a table address's low half before its high half, a table outside free VRAM, the transfer before both
//!   tables' addresses, another table than the combo powerplay table;
//! - RunDcBtc before the transfer or a second time; a link level out of order, with another argument than
//!   Linux computes from the model's table and its link, or after the features are on;
//! - EnableAllSmuFeatures before RunDcBtc and every link level, or a second time;
//! - the video block powered up before the features are on, JPEG before VCN, down in the wrong order (VCN
//!   goes down after PowerDownJpeg went out, whatever was answered to it: Linux drops that answer,
//!   amdgpu_smu.c:912-921);
//! - a clock question, once the features are on, for a clock whose feature does not run, for a level past its
//!   count, or for the video clocks while the video block is not powered up;
//! - the thermal alert's registers before the features are on, out of Linux's order or with anything but
//!   Linux's change; AllowIHHostInterrupt before them;
//! - any message whose argument is not the one Linux sends.
//!
//! What its registers beside the mailbox read and take is in `fakepmfwregs`.
//!
//! After EnableAllSmuFeatures the power controller's features are on (`Model::gc_powered`, `fakechain`): the
//! models of the blocks Linux starts behind it ask that for the writes of their own sections.

use super::fakemodel::{MC_BASE, Model, SMU_IF, SMU_VERSION};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::pptable14::{self, OwnParts, PcieLevels, Pptable};
use rdna4_core::smu14::{self, DRIVER_TABLE_BYTES, TOOLS_TABLE_BYTES, msg};
use rdna4_core::{pcielink, refuse};

/// The running features after EnableAllSmuFeatures, as the card answered Linux (init.txt lines 989 and 995).
pub const FEATURES: u64 = 0x048C_F19E_38FF_FCFB;
/// Each clock's levels as the card answered Linux (init.txt lines 1043-1858): (PPCLK id, fine-grained, MHz).
pub const CLOCK_LEVELS: [(u8, bool, &[u32]); 7] = [
    (1, true, &[417, 1476]),
    (0, true, &[500, 3450]),
    (2, false, &[96, 456, 772, 875, 1124, 1258]),
    (3, true, &[313, 2400]),
    (5, true, &[800, 2934]),
    (4, true, &[800, 2200]),
    (9, true, &[147, 1800]),
];
/// How long EnableAllSmuFeatures showed no answer to Linux: 8850 reads of 0 (init.txt line 975, 16.9 ms).
pub const ENABLE_READS: u32 = 8850;

/// The table the model's PMFW writes on a transfer. What the trace settles is in it (the software shutdown
/// temperature 118 of the thermal register Linux wrote, link levels that give Linux's arguments 0x406 on this
/// machine's x16 Gen5 link, GameClockAc 2400 of the libdrm capture); the rest is invented and says so.
pub fn table() -> Pptable {
    Pptable {
        structuresize: pptable14::TABLE_BYTES as u16,
        format_revision: pptable14::TABLE_FORMAT_REVISION,
        content_revision: 0,
        own: Pptable::COMPILED,
        // BACO and MACO ("Using BACO for runtime pm" in Linux's journal).
        platform_caps: 0x18,
        thermal_controller_type: 0,
        software_shutdown_temp: 118,
        features_to_run: [FEATURES as u32, (FEATURES >> 32) as u32],
        // Invented but for the last level, which must reach Gen5 x16.
        pcie: PcieLevels { gen_speed: [0, 2, 4], lane_count: [6, 6, 6], lclk_freq: [310, 619, 619] },
        game_clock_ac: 2400,
        // Invented.
        temperature_limit: [100, 110, 108],
        fan_abnormal_temp_limit_offset: 0,
        // The card's default and highest power limit (plan/spec/gpu-rdna4.md section 1).
        socket_power_limit_ac: 304,
        msg_limit_power_ac: 374,
    }
}

pub struct FakePmfw {
    /// The secure processor loaded the SMU's firmware in this boot (LOAD_IP_FW type 18).
    pub loaded: bool,
    /// No message reached this PMFW since it started.
    pub(super) fresh: bool,
    /// The answer register was read as answered since the last message went.
    pub(super) seen_answer: bool,
    /// Reads of the answer register that still show 0, and the answer that shows after them.
    pub(super) wait: u32,
    pub(super) answer: u32,
    /// What the answer register held before the 0 of the message under way went into it: a message the model
    /// refuses never ran, and the register reads as before it.
    pub(super) held: u32,
    pub driver: [Option<u32>; 2],
    pub tools: [Option<u32>; 2],
    pub transferred: bool,
    pub btc: bool,
    pub pcie: Vec<u32>,
    pub enabled: bool,
    pub vcn_up: bool,
    pub jpeg_up: bool,
    /// PowerDownJpeg went out since JPEG was powered up, whatever was answered to it.
    pub(super) jpeg_down_went: bool,
    /// How many of the thermal alert's four registers were written, in Linux's order.
    pub thermal: u8,
    pub ih_allowed: bool,
    pub thm_ctrl: u32,
    pub thm_ena: u32,
    pub sw_int: u32,
    pub sw_int_ctrl: u32,
    /// What the PMFW writes on a transfer.
    pub table: Pptable,
    /// Tests: the table's own part offsets as the PMFW writes them (the model's default names Linux's compiled ones).
    pub own: OwnParts,
    /// The platform's link limit as Linux computes it for this model's path (`pcielink::caps`): x16 at 32 GT/s.
    pub link: (u8, u8),
    pub features: u64,
    /// Tests: this message is answered with this code.
    pub answer_with: Option<(u16, u32)>,
    /// Tests: the answer of the nth feature question (counted from 1 over both halves) on.
    pub features_from: Option<(u32, u64)>,
    pub(super) feature_questions: u32,
    /// Tests: this clock answers this many levels.
    pub levels_of: Option<(u8, u32)>,
    /// Tests: the driver table does not read out of VRAM (a card that left the bus after the transfer).
    pub table_unreadable: bool,
    /// Wave 44: the thermal block and the metrics table (`fakethermal`).
    pub thm: super::fakethermal::FakeThermal,
}

impl FakePmfw {
    pub fn new() -> FakePmfw {
        let t = table();
        FakePmfw {
            loaded: false,
            fresh: true,
            seen_answer: false,
            wait: 0,
            answer: 1,
            held: 0,
            driver: [None; 2],
            tools: [None; 2],
            transferred: false,
            btc: false,
            pcie: Vec::new(),
            enabled: false,
            vcn_up: false,
            jpeg_up: false,
            jpeg_down_went: false,
            thermal: 0,
            ih_allowed: false,
            // What the card answered Linux (init.txt lines 1870, 1873, 1875).
            thm_ctrl: 0x2FFF_0071,
            thm_ena: 0,
            sw_int: 0,
            sw_int_ctrl: 0,
            own: t.own,
            table: t,
            link: (4, 6),
            features: FEATURES,
            answer_with: None,
            features_from: None,
            feature_questions: 0,
            levels_of: None,
            table_unreadable: false,
            thm: super::fakethermal::FakeThermal::new(),
        }
    }

    pub(super) fn table_mc(halves: [Option<u32>; 2]) -> Option<u64> {
        Some(u64::from(halves[0]?) << 32 | u64::from(halves[1]?))
    }
}

impl Default for FakePmfw {
    fn default() -> Self {
        FakePmfw::new()
    }
}

fn name(m: u16) -> &'static str {
    refuse::SMU_START.iter().chain(refuse::SMU_QUESTIONS.iter()).find(|(x, _)| *x == m).map_or("?", |(_, n)| n)
}

impl Model {
    /// The secure processor loaded the SMU's firmware: a new PMFW runs, told nothing yet. What its answer
    /// register holds then no trace shows (Linux does not look, smu_cmn.c:508-511); the model holds 0 there, so
    /// a driver that waited for it would wait in vain.
    pub(super) fn pmfw_reloaded(&mut self) {
        let old = core::mem::take(&mut self.pmfw);
        *self.pmfw = FakePmfw {
            loaded: true,
            table: old.table,
            own: old.own,
            link: old.link,
            features: old.features,
            answer_with: old.answer_with,
            features_from: old.features_from,
            levels_of: old.levels_of,
            table_unreadable: old.table_unreadable,
            thm: old.thm.after_reload(),
            ..FakePmfw::new()
        };
        self.c90 = 0;
    }

    /// A table the PMFW is told about: on a page, inside VRAM, outside the picture and what GMC holds there.
    fn pmfw_table_ok(&self, mc: u64, bytes: u64) -> bool {
        let off = mc.wrapping_sub(MC_BASE);
        mc.is_multiple_of(0x1000) && self.psp_may_have(mc, bytes) && !(off < self.picture.end && off.saturating_add(bytes) > self.picture.start)
    }

    /// GetDpmFreqByIndex once the features are on: the trace's answers.
    fn pmfw_freq(&self, arg: u32) -> Result<u32, String> {
        let (id, level) = ((arg >> 16) as u8, arg & 0xFFFF);
        let Some(c) = smu14::CLOCKS.iter().find(|c| c.ppclk == id) else { return Err(format!("GetDpmFreqByIndex for clock {id}, which Linux's start does not ask about")) };
        if self.pmfw.features & (1 << c.feature) == 0 {
            return Err(format!("GetDpmFreqByIndex for {}, whose feature does not run", c.name));
        }
        if matches!(id, 4 | 5) && !(self.pmfw.vcn_up && self.pmfw.jpeg_up) {
            return Err(format!("GetDpmFreqByIndex for {} while the video block is not powered up (amdgpu_smu.c:897-904)", c.name));
        }
        let (_, fine, mhz) = CLOCK_LEVELS.iter().find(|l| l.0 == id).copied().unwrap_or((id, false, &[]));
        let count = self.pmfw.levels_of.filter(|(c, _)| *c == id).map_or(mhz.len() as u32, |(_, n)| n);
        let flag = if fine { smu14::FINE_GRAINED } else { 0 };
        match level {
            smu14::LEVEL_COUNT => Ok(flag | count),
            i if i < count => Ok(flag | mhz.get(i as usize).copied().unwrap_or(1000 + i)),
            i => Err(format!("GetDpmFreqByIndex {} level {i}: past its {count} levels", c.name)),
        }
    }

    /// The message `m` with the argument register as it stands: what the argument register reads after it, and
    /// how many reads of the answer register still show 0; or why Linux would not have sent it.
    fn pmfw_take(&mut self, m: u16) -> Result<(u32, u32), String> {
        let arg = self.c82;
        let start = refuse::SMU_START.iter().any(|(x, _)| *x == m);
        if start && !self.pmfw.loaded {
            return Err(format!("{} before the secure processor loaded the SMU's firmware in this boot", name(m)));
        }
        if m == msg::TRANSFER_TABLE_SMU2DRAM && arg == rdna4_core::metrics14::TRANSFER_ARG {
            return self.thm_metrics().map(|()| (arg, super::fakethermal::METRICS_READS));
        }
        // Linux's start is over: its messages are not sent again (the sensors' transfer above is the handler's).
        if start && self.pmfw.ih_allowed {
            return Err(format!("{} with argument {arg:#x} after Linux's start of the power controller ended", name(m)));
        }
        let zero = |arg: u32| if arg == 0 { Ok(()) } else { Err(format!("{} with argument {arg:#x}: Linux sends 0", name(m))) };
        let levels = pcielink::forced(&self.pmfw.table.pcie, self.pmfw.link.0, self.pmfw.link.1);
        let (mut value, mut wait) = (arg, 0);
        match m {
            msg::SET_DRIVER_DRAM_ADDR_HIGH => self.pmfw.driver = [Some(arg), None],
            msg::SET_TOOLS_DRAM_ADDR_HIGH => self.pmfw.tools = [Some(arg), None],
            msg::SET_DRIVER_DRAM_ADDR_LOW | msg::SET_TOOLS_DRAM_ADDR_LOW => {
                let driver = m == msg::SET_DRIVER_DRAM_ADDR_LOW;
                let (halves, bytes) = if driver { (self.pmfw.driver, DRIVER_TABLE_BYTES) } else { (self.pmfw.tools, TOOLS_TABLE_BYTES) };
                let Some(hi) = halves[0] else { return Err(format!("{}: the low half of a table's address before its high half", name(m))) };
                let mc = u64::from(hi) << 32 | u64::from(arg);
                let other = if driver { FakePmfw::table_mc(self.pmfw.tools).map(|t| (t, TOOLS_TABLE_BYTES)) } else { FakePmfw::table_mc(self.pmfw.driver).map(|t| (t, DRIVER_TABLE_BYTES)) };
                if !self.pmfw_table_ok(mc, bytes) || other.is_some_and(|(t, n)| mc < t + n && t < mc + bytes) {
                    return Err(format!("{}: a table of {bytes:#x} bytes at {mc:#x} is not on a page of free VRAM of its own", name(m)));
                }
                if driver { self.pmfw.driver[1] = Some(arg) } else { self.pmfw.tools[1] = Some(arg) }
            }
            msg::TRANSFER_TABLE_SMU2DRAM => {
                if arg != smu14::TABLE_COMBO_PPTABLE {
                    return Err(format!("TransferTableSmu2Dram with argument {arg:#x}: Linux's start transfers table 1, the combo powerplay table"));
                }
                let (Some(at), Some(_)) = (FakePmfw::table_mc(self.pmfw.driver), FakePmfw::table_mc(self.pmfw.tools)) else {
                    return Err(String::from("a table transfer before the driver table's and the tools table's addresses were set"));
                };
                let mut bytes = alloc::vec![0u8; DRIVER_TABLE_BYTES as usize];
                pptable14::synthetic(&Pptable { own: self.pmfw.own, ..self.pmfw.table }, &mut bytes);
                self.poke(at - MC_BASE, &bytes);
                self.pmfw.transferred = true;
            }
            msg::RUN_DC_BTC => {
                zero(arg)?;
                if !self.pmfw.transferred || self.pmfw.btc {
                    return Err(String::from(if self.pmfw.btc { "a second RunDcBtc" } else { "RunDcBtc before the PMFW's table was transferred (Linux's order)" }));
                }
                self.pmfw.btc = true;
            }
            msg::OVERRIDE_PCIE_PARAMETERS => {
                let i = self.pmfw.pcie.len();
                if !self.pmfw.btc || self.pmfw.enabled || i >= levels.1 {
                    return Err(format!("OverridePcieParameters {arg:#x}: not after RunDcBtc, before the features are on, one per link level of the table ({} went, the table has {})", i, levels.1));
                }
                if arg != levels.0[i] {
                    return Err(format!("OverridePcieParameters {arg:#x} for level {i}: Linux sends {:#x} for this table on this link", levels.0[i]));
                }
                self.pmfw.pcie.push(arg);
            }
            msg::ENABLE_ALL_SMU_FEATURES => {
                zero(arg)?;
                if self.pmfw.enabled {
                    return Err(String::from("a second EnableAllSmuFeatures"));
                }
                if !self.pmfw.btc || self.pmfw.pcie.len() != levels.1 {
                    return Err(String::from("EnableAllSmuFeatures before RunDcBtc and every link level (Linux's order)"));
                }
                (self.pmfw.enabled, wait) = (true, ENABLE_READS);
            }
            msg::POWER_UP_VCN | msg::POWER_UP_JPEG | msg::POWER_DOWN_JPEG | msg::POWER_DOWN_VCN => {
                zero(arg)?;
                let p = &mut self.pmfw;
                let ok = p.enabled
                    && match m {
                        msg::POWER_UP_VCN => !p.vcn_up,
                        msg::POWER_UP_JPEG => p.vcn_up && !p.jpeg_up,
                        msg::POWER_DOWN_JPEG => p.jpeg_up,
                        _ => p.vcn_up && (!p.jpeg_up || p.jpeg_down_went),
                    };
                if !ok {
                    return Err(format!("{}: not in Linux's order (the features on, VCN up, JPEG up, JPEG down, VCN down; now features {}, VCN {}, JPEG {})", name(m), p.enabled, p.vcn_up, p.jpeg_up));
                }
                match m {
                    msg::POWER_UP_VCN => p.vcn_up = true,
                    msg::POWER_UP_JPEG => (p.jpeg_up, p.jpeg_down_went) = (true, false),
                    msg::POWER_DOWN_JPEG => p.jpeg_up = false,
                    _ => p.vcn_up = false,
                }
            }
            msg::ALLOW_IH_HOST_INTERRUPT => {
                zero(arg)?;
                if !self.pmfw.enabled || self.pmfw.thermal != 4 || self.pmfw.ih_allowed {
                    return Err(String::from("AllowIHHostInterrupt: not once, after the features are on and the thermal alert's four registers were written"));
                }
                self.pmfw.ih_allowed = true;
            }
            msg::GET_SMU_VERSION => value = SMU_VERSION,
            msg::GET_DRIVER_IF_VERSION => value = SMU_IF,
            msg::GET_RUNNING_SMU_FEATURES_LOW | msg::GET_RUNNING_SMU_FEATURES_HIGH => {
                self.pmfw.feature_questions += 1;
                let mask = match self.pmfw.features_from {
                    Some((n, other)) if self.pmfw.feature_questions >= n => other,
                    _ => self.pmfw.features,
                };
                value = match (self.pmfw.enabled, m == msg::GET_RUNNING_SMU_FEATURES_LOW) {
                    (true, true) => mask as u32,
                    (true, false) => (mask >> 32) as u32,
                    // What runs before Linux's start no trace shows (Linux never asks then): invented.
                    (false, true) => 0x3FFF_FFFF,
                    (false, false) => 0x0000_0001,
                };
            }
            msg::GET_DPM_FREQ_BY_INDEX => value = if self.pmfw.enabled { self.pmfw_freq(arg)? } else { 2 },
            // The look's other questions (invented answers, as before wave 43).
            0x01 => value = arg.wrapping_add(1),
            0x33 => value = 304,
            0x1D | 0x1E => value = self.clock(u32::from(m)),
            0x27 => value = 2970,
            _ => value = 0,
        }
        Ok((value, wait))
    }

    /// A message write that passed the mailbox's order: taken and answered, or refused.
    pub(super) fn pmfw_message(&mut self, m: u16) -> bool {
        let arg = self.c82;
        let answer = match self.pmfw.answer_with {
            Some((x, code)) if x == m => code,
            _ if self.smu_silent_on == Some(m) => 0,
            _ => 1,
        };
        // A message a test has fail or never answer does nothing to the model's power controller.
        let (value, wait) = if answer != 1 {
            (arg, 0)
        } else {
            match self.pmfw_take(m) {
                Ok(x) => x,
                Err(why) => {
                    self.c90 = self.pmfw.held;
                    return self.refuse(why);
                }
            }
        };
        self.messages.push((m, arg));
        self.c82 = value;
        self.pmfw.fresh = false;
        self.pmfw.jpeg_down_went |= m == msg::POWER_DOWN_JPEG;
        if wait > 0 && answer != 0 {
            (self.c90, self.pmfw.wait, self.pmfw.answer) = (0, wait, answer);
        } else {
            self.c90 = answer;
        }
        true
    }
}
