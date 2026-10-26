//! The driver's errors: what failed, with register and value (spec/wlan.md §3:
//! "Result<T, WlanError> with register and value in the error").

use alloc::boxed::Box;

use crate::bus::BusError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rtw89Error {
    /// The bus refused or timed out (register, mask, last value inside)
    Bus(BusError),
    /// rtw89_mac_reset_pwr_state_ax: IC_PWR_STATE says the MAC is already on (-EBUSY)
    MacAlreadyOn { ic_pwr_state: u32 },
    /// A chip cut BitsOS has no tables for (only cv 1 / acv 1, the measured chip)
    UnsupportedChip { cv: u32, acv: u32 },
    /// The DMA allocator handed out an address the chip cannot reach or reuse
    BadDmaAddress { addr: u32 },
    /// rtw89_mac_check_mac_en_ax: the DMAC (or CMAC) is not enabled; `reg` read `value` (-EFAULT)
    MacNotEnabled {
        sel: &'static str,
        reg: u32,
        value: u32,
    },
    /// rtw89_pci_auto_refclk_cal: the link runs at a PCIe PHY rate the chip has no MDIO page for
    /// (-EOPNOTSUPP); `value` is config byte 0x82
    PciePhyRate { value: u32 },
    /// The function has no PCIe capability in its list (status, first pointer as read)
    NoPcieCapability { status: u32, first: u32 },
    /// rtw89_pci_ltr_set: an LTR register does not answer (all ones, 0xEAEAEAEA, 0xDEADBEEF)
    LtrDead { reg: u32, value: u32 },
    /// The chip's ring index register `reg` read `value`: past the ring of `len` entries
    /// (spec/wlan.md §2.3 "Ring indexes from the chip are checked")
    RingIndex { reg: u32, value: u32, len: u32 },
    /// rtw89_pci_tx_write (-ENOSPC): no free BD on the TX ring whose index register is `reg`
    /// (host write index `wp`, read index `rp` as the host last saw it)
    RingFull { reg: u32, wp: u32, rp: u32 },
    /// rtw89_mac_dle_init's `error:` path (mac.c:2325): the DLE configuration was refused
    /// (`what`, `value`); the DLE was switched off and both INI_STATUS registers read
    DleConfig {
        what: &'static str,
        value: u32,
        wde_ini_status: u32,
        ple_ini_status: u32,
    },
    /// hfc_ch_cfg_chk / hfc_pub_cfg_chk (mac.c:926, 961): the flow control configuration of DMA
    /// channel `ch` (DMA_H2C = 12 for the public pages) is refused before it is written; `value`
    /// is the offending number
    HfcConfig {
        what: &'static str,
        ch: u32,
        value: u32,
    },
    /// hfc_pub_info_chk (mac.c:945): with flow control on, the public pages do not add up
    HfcPubPages {
        g0_used: u32,
        g1_used: u32,
        pub_aval: u32,
        pub_max: u32,
    },
    /// pcitx refused a frame for the TX ring of DMA channel `ch` before anything was queued or
    /// written (`what` names the check, `value` is the offending number: a channel, QSEL or length)
    TxRefused {
        what: &'static str,
        ch: u32,
        value: u32,
    },
    /// A request the driver refuses before touching the chip (`what` names it, `value` is the bad one)
    Invalid { what: &'static str, value: u32 },
    /// The firmware file is not what fw.c accepts (`what` names the check, as Linux's message;
    /// `value` is the offending number: an offset, a length, a count or a field)
    FwFile { what: &'static str, value: u32 },
    /// rtw89_mfw_recognize (-ENOENT/-EINVAL): the file holds no entry of type `typ` for chip cut `cv`
    NoFirmware { typ: u32, cv: u32 },
    /// Firmware secure boot (efuse) is on: not ported, refused before anything is parsed
    FwSecureBoot,
    /// rtw89_mac_enable_cpu_ax (-EFAULT): the WLAN CPU is already running; `reg` read `value`
    WcpuAlreadyOn { reg: u32, value: u32 },
    /// rtw89_fwdl_check_path_ready_ax: the H2C or FWDL path never got ready; `reg` read `value`
    FwPathNotReady {
        path: &'static str,
        reg: u32,
        value: u32,
    },
    /// rtw89_fw_check_rdy: the FWDL status never reached 7 (WCPU_FW_INIT_RDY); `reg` read
    /// `value` after the wait, whose status field is `status` (2 checksum, 3 security, 4 cut)
    FwStatus { reg: u32, value: u32, status: u32 },
    /// Not Linux (CH12 resync before a download retry): the chip's CH12 index stayed below the
    /// host index `host`; `value` is the register's last value. `before` is the failed try's error.
    Ch12Stuck {
        reg: u32,
        host: u32,
        value: u32,
        before: Box<Rtw89Error>,
    },
    /// Not Linux (CH12 resync): the CH12 index register read `value`, a host index past the ring
    /// (all ones: the link is gone). `before` is the failed try's error.
    FwLinkGone {
        reg: u32,
        value: u32,
        before: Box<Rtw89Error>,
    },
    /// rtw89_fw_write_h2c_reg: "FW does not process h2c registers"; `reg` still read `value`
    H2cRegBusy { reg: u32, value: u32 },
    /// rtw89_fw_read_c2h_reg: "c2h reg timeout"; `reg` read `value` (0) until the deadline
    C2hRegTimeout { reg: u32, value: u32 },
    /// A register command's answer (`what`) came from another function: `id` instead of
    /// `want` (Linux's -EINVAL); `reg` (C2HREG_DATA0) read `value`
    C2hRegAnswer {
        what: &'static str,
        want: u32,
        id: u32,
        reg: u32,
        value: u32,
    },
    /// rtw89_core_rfkill_get: the hardware switch (GPIO 9) blocks the radio; `reg` read
    /// `value`. mac80211 would not start the radio, neither does BitsOS.
    RadioBlocked { reg: u32, value: u32 },
    /// The efuse is unusable or a variant BitsOS has no tables for (`what` names the check, as
    /// the JS driver's message): `addr` is the efuse (or register) address, `value` what was
    /// found there. Linux would go on with most of these; BitsOS stops before a wrong rfe picks
    /// the power-on's SPS values and the PHY tables, or a broken MAC reaches the address CAM.
    Efuse {
        what: &'static str,
        addr: u32,
        value: u32,
    },
    /// A BB/RF table the bring-up needs is missing from the firmware file or was left out by
    /// rtw89_phy_init_reg (`what` names it; `path` the RF path; `rfe`/`cv` what it was
    /// selected for). Linux would fall back to rtw8852b_table.c or go on; BitsOS stops.
    PhyTable {
        what: &'static str,
        path: u32,
        rfe: u32,
        cv: u32,
    },
    /// The coexistence reached a path that needs a running BT side, another firmware coex layout
    /// or a mode BitsOS does not port (`what` names it, `value` is what led there: a scoreboard
    /// word, a mode, a policy, a firmware version code). The WLAN-only model no longer holds.
    BtcNotPorted { what: &'static str, value: u32 },
    /// txrx refused a frame or a TX request before it reached a ring (`what` names the check, as
    /// the JS driver's message; `value` is the offending number: a length, a frame control word,
    /// a key's cipher suite, a type)
    Frame { what: &'static str, value: u32 },
}

impl From<BusError> for Rtw89Error {
    fn from(e: BusError) -> Self {
        Rtw89Error::Bus(e)
    }
}

impl Rtw89Error {
    /// A failure of the chip (or its stand-in), not of the request: Linux powers off and on again
    /// once for these (rtw89_mac_pwr_on)
    pub fn is_chip_failure(&self) -> bool {
        matches!(
            self,
            Rtw89Error::Bus(BusError::PollTimeout { .. }) | Rtw89Error::MacAlreadyOn { .. }
        )
    }

    /// The FWDL status this error carries (rtw89_fw_check_rdy's value, the JS FwError's
    /// `status`), also through a CH12 resync failure that ended the retries
    pub fn fw_status(&self) -> Option<u32> {
        match self {
            Rtw89Error::FwStatus { status, .. } => Some(*status),
            Rtw89Error::Ch12Stuck { before, .. } | Rtw89Error::FwLinkGone { before, .. } => {
                before.fw_status()
            }
            _ => None,
        }
    }
}

pub type Rtw89Result<T> = Result<T, Rtw89Error>;
