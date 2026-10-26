//! The firmware of the RTL8852BE (JS js/fw.mjs, fw-parse.mjs, fw-h2c.mjs): the
//! file read as Linux rtw89 fw.c reads it (`parse`), the download into the
//! chip's WLAN CPU with mac.c's CPU enable/disable (`download`), host-to-chip
//! commands on CH12 and the register path (`h2c`), and the chip's answers
//! (`c2h`). Each function keeps the JS driver's (= Linux's) order of register
//! accesses and names its Linux original.
//!
//! The state is split as Linux's `struct rtw89_fw_info` is used: [`Firmware`]
//! is what rtw89_fw_recognize made of the file (read only afterwards),
//! [`FwState`] the counters, flags and what the download left (JS `d.fw`'s
//! other fields). Linux's warnings become [`FwEvent`]s that the caller drains
//! (`FwState::take_events`) and logs (B6); the crate itself never logs.

pub mod c2h;
pub mod download;
pub mod h2c;
#[cfg(any(test, feature = "std"))]
pub mod hookbus;
pub mod parse;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::error::Rtw89Error;
use crate::events::EventLog;

pub use c2h::{AckKind, C2h, C2hAck, RxPkt, c2h_parse, rx_pkt};
pub use download::{
    CH12_WAIT_MS, FWDL_RETRIES, FWDL_WAIT_MS, PC_DUMPS, Piece, Pieces, check_rdy, disable_cpu,
    dl_fail_dump, download, enable_cpu, fw_hdr_h2c, piece_bytes, section_pieces,
};
pub use h2c::{
    C2hReg, H2c, H2cReg, RfRegInfo, h2c_fw_log, h2c_hdr, h2c_hdr_fwdl, h2c_init_ba_cam, h2c_raw,
    h2c_rf_reg, h2c_send, h2c_set_ofld_cfg, msg_reg, rf_reg_upload, send_all_early_h2c,
};
pub use parse::{
    ELM_NAMES, Firmware, FwBinInfo, FwElement, FwSection, FwSuit, FwVersion, MfwEntry, MfwFound,
    features_for, mfw_recognize, ver_code,
};

#[cfg(test)]
mod tests;

/// rtw89_fw_prog_cnt_dump / rtw89_fw_dl_fail_dump's values after a failed try
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailDump {
    /// R_AX_WCPU_FW_CTRL (0x1E0)
    pub fw_ctrl: u32,
    /// R_AX_BOOT_DBG (0x83F0)
    pub boot_dbg: u32,
    /// R_AX_DBG_PORT_SEL read 15 times: the WLAN CPU's program counter
    pub pc: Vec<u32>,
}

/// What Linux only warns about (rtw89_warn/rtw89_err), for the caller's log (B6)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FwEvent {
    /// rtw89_fw_download's loop: try `try_no` (from 1) failed; another follows unless it was the fifth
    TryFailed { try_no: u32, error: Rtw89Error },
    /// rtw89_fw_dl_fail_dump: the dump after a failed try
    FailDump(FailDump),
    /// the dump itself could not be read
    FailDumpFailed(Rtw89Error),
    /// rtw89_phy_config_rf_reg_fw: more stored RF values than the H2C pages hold; nothing sent
    RfUploadTooLong { len: u32, max: u32 },
    /// rtw89_fw_read_c2h_reg: "c2h reg timeout"; the firmware did not answer within
    /// `timeout_us` (`value` = the last read of `reg`)
    C2hRegTimeout {
        reg: u32,
        value: u32,
        timeout_us: u32,
    },
}

fn hex(v: u32) -> String {
    format!("0x{v:08x}")
}

impl fmt::Display for FwEvent {
    // the JS driver's log lines (fw.mjs d.log)
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FwEvent::TryFailed { try_no, error } => {
                write!(f, "firmware download try {try_no}: {error:?}")
            }
            FwEvent::FailDump(d) => {
                let pc: Vec<String> = d.pc.iter().map(|&x| hex(x)).collect();
                write!(
                    f,
                    "[ERR]fwdl 0x1E0 = {}, 0x83f0 = {}, fw PC = {}",
                    hex(d.fw_ctrl),
                    hex(d.boot_dbg),
                    pc.join(" ")
                )
            }
            FwEvent::FailDumpFailed(e) => write!(f, "fail dump: {e:?}"),
            FwEvent::RfUploadTooLong { len, max } => {
                write!(f, "rf reg h2c total len {len} larger than {max}")
            }
            FwEvent::C2hRegTimeout {
                reg,
                value,
                timeout_us,
            } => write!(
                f,
                "c2h reg timeout after {timeout_us} us ({}={})",
                hex(*reg),
                hex(*value)
            ),
        }
    }
}

/// Linux: struct rtw89_fw_info (core.h:6107) without the parsed file: the H2C/C2H
/// sequence numbers and counters, RTW89_FLAG_FW_RDY, the early H2C list and what
/// the download left (JS `d.fw` via `fwState`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FwState {
    /// u8 h2c_seq: the next H2C's sequence number
    pub h2c_seq: u8,
    pub rec_seq: u8,
    /// the register path's counters (R_AX_UDM1 + 1, 4 bits each on the chip)
    pub h2c_counter: u8,
    pub c2h_counter: u8,
    /// RTW89_FLAG_FW_RDY
    pub ready: bool,
    /// rtw89_fw_send_all_early_h2c's list (complete H2Cs, header included); Linux fills it
    /// only from debugfs, so it stays empty unless a caller adds one
    pub early_h2c: Vec<Vec<u8>>,
    /// pieces the last download sent (164 for 0.29.29.18)
    pub pieces: u32,
    /// the try that passed (1-5)
    pub tries: u32,
    /// the last fail dump
    pub fail_dump: Option<FailDump>,
    events: EventLog<FwEvent>,
}

impl FwState {
    pub fn new() -> FwState {
        FwState::default()
    }
    /// What happened since the last call, oldest first
    pub fn take_events(&mut self) -> Vec<FwEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[FwEvent] {
        self.events.as_slice()
    }
    /// Events lost to the log's cap so far (never reset; WL-4 drains after every call)
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }
    /// How many undrained events are kept (default [`crate::events::DEFAULT_EVENT_CAP`])
    pub fn set_event_cap(&mut self, cap: usize) {
        self.events.set_cap(cap);
    }
    pub(crate) fn push(&mut self, e: FwEvent) {
        self.events.push(e);
    }
}
