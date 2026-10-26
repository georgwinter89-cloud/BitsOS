//! The bring-up of the RTL8852BE in Linux's order (JS js/bringup.mjs), as far
//! as the ported modules reach: stage fw up to the probe's power-on (WL-1), the
//! partial init up to the firmware download (WL-2 pci/dmac), the download
//! itself with the not-Linux status look after it (WL-2 fw), the probe's end
//! (power off, PCIe basic config, rfkill) and stage mac (rtw89_core_start up to
//! the coexistence power-on: power on, rtw89_mac_init; WL-2 macinit), and the
//! stop. The efuse/phycap step between the look and the probe's end is phy's.

pub mod efuse;
pub mod power;
pub mod rfkill;
pub mod safety;
#[cfg(test)]
mod tests_efuse;

use alloc::format;
use alloc::vec::Vec;

use crate::bus::Bus;
use crate::bus::field;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::{self, C2h, Firmware, FwState};
use crate::macinit::{self, MacInit, MacState};
use crate::pci::{self, BasicCfg};
use crate::pcitx::PciTx;
use crate::phy::{self, ChipOps, Phy, PhyCap, PhyEvent};
use crate::tables::regs::{
    B_AX_WCPU_FWDL_STS_MASK, R_AX_DMAC_FUNC_EN, R_AX_IC_PWR_STATE, R_AX_WCPU_FW_CTRL,
    RTW89_CORE_RX_TYPE_C2H,
};

pub use crate::pci::rings as pci_alloc;
pub use crate::pci::rings::{PciRings, RingConfig};
pub use crate::phy::efuse::EfuseMaps;
pub use power::{ChipVer, PowerOn, PwrOpts};
pub use safety::Safety;

/// The RX budget of the look at RXQ after the download (pciRxPoll's default, the NAPI budget)
pub const LOOK_RX_BUDGET: usize = 32;

/// What the not-Linux look after the download saw (JS bringup.mjs, log only)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FwdlLook {
    /// R_AX_WCPU_FW_CTRL after the download
    pub fw_ctrl: u32,
    /// its FWDL status (7 = ready)
    pub status: u32,
    /// new RXQ buffers
    pub bufs: usize,
    /// the C2Hs among them
    pub c2h: Vec<C2h>,
    /// the others: (RX type, packet size)
    pub other: Vec<(u32, u32)>,
}

/// The cuts BitsOS has tables for: the measured chip (js/bringup-steps.mjs EXPECT)
pub const EXPECT_CV: u32 = 1;
pub const EXPECT_ACV: u32 = 1;
/// The rfe_type BitsOS has tables and power-on values for (the measured chip; spec/wlan.md §3)
pub const EXPECT_RFE: u8 = 1;

/// The chip ops of the modules stage phy calls (Linux reaches them through rtwdev->chip->ops
/// and the coexistence's notify calls): chan's TX power unit, power control, trim and paths and
/// rfk1's RF calibrations ([`ChipOps`]), btc's init and radio-on, and the track work's start.
pub trait PhyStageHooks<B: Bus + ?Sized>: ChipOps<B> {
    /// rtw89_btc_ntfy_init(BTC_MODE_NORMAL): its H2Cs go on CH12
    fn btc_init(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        fws: &mut FwState,
        phy: &Phy,
    ) -> Rtw89Result<()>;
    /// queue the periodic track work (rtw89_core_start: track_work in 2 s); no register
    fn track_start(&mut self) {}
    /// rtw89_btc_ntfy_radio_state(BTC_RFCTRL_WL_ON): its coex run can send H2Cs on CH12
    fn radio_on(&mut self, bus: &mut B, rings: &mut PciRings, fws: &mut FwState)
    -> Rtw89Result<()>;
}

// Not Linux (JS bringup-steps.mjs checkEfuse, changed as spec/wlan.md §3 asks): Linux takes any
// efuse; BitsOS stops before a variant it has no tables for picks the power-on's SPS values and
// the PHY tables, or a broken MAC reaches the address CAM. The JS also demanded the measured MAC
// and xtal cap; V2 checks the MAC for plausibility only (a replacement module must work), the
// per-boot consistency check against a kept MAC is WL-4's.
pub fn check_efuse(ef: &phy::Efuse, ctrl: Option<u16>) -> Rtw89Result<()> {
    use crate::tables::phy::{EF_MAC_ADDR_E, EF_RFE_TYPE};
    if !ef.valid {
        return Err(Rtw89Error::Efuse {
            what: "efuse not autoloaded",
            addr: crate::tables::regs::R_AX_SYS_WL_EFUSE_CTRL,
            value: ctrl.map_or(0, u32::from),
        });
    }
    if ef.rfe_type != EXPECT_RFE {
        return Err(Rtw89Error::Efuse {
            what: "rfe_type BitsOS has no tables for",
            addr: EF_RFE_TYPE as u32,
            value: u32::from(ef.rfe_type),
        });
    }
    let a = ef.addr;
    if a[0] & 1 != 0 || a.iter().all(|&b| b == 0) || a.iter().all(|&b| b == 0xff) {
        return Err(Rtw89Error::Efuse {
            what: "MAC address is no unicast address",
            addr: EF_MAC_ADDR_E as u32,
            value: u32::from_be_bytes([a[0], a[1], a[2], a[3]]),
        });
    }
    Ok(())
}

/// A block that is not Linux's, marked in the trace as the JS driver marks it
fn not_linux<B: Bus + ?Sized, T>(
    bus: &mut B,
    what: &str,
    f: impl FnOnce(&mut B) -> Rtw89Result<T>,
) -> Rtw89Result<T> {
    bus.note(&format!("not Linux: {what}"))?;
    let r = f(bus)?;
    bus.note("Linux again")?;
    Ok(r)
}

/// The state of one bring-up run: what is on, so the stop knows what to undo
#[derive(Debug, Default)]
pub struct Bringup {
    pub safety: Option<Safety>,
    pub rings: Option<PciRings>,
    pub chip: Option<ChipVer>,
    pub power: Option<PowerOn>,
    /// the probe's power-on was started (JS `st.on`)
    pub on: bool,
    /// efuse rfe_type for the stop's power-off (rtw8852b_pwr_off_func takes the
    /// RFE 5 branch). 0 until the efuse is parsed, as Linux's at probe; the
    /// efuse step sets it.
    pub rfe: u8,
    /// the MAC (JS `d.mac`): its DMA state (quota mode, DLE info, HCI flow control),
    /// hal.rx_fltr, the power after the last switch that went through (`power_on`,
    /// `None`: unknown), and the events of the MAC part
    pub mac: MacState,
    /// the PCIe DMA may be running (JS `st.pci`): a stop before the core ran
    /// turns it off before the power-off (not Linux there)
    pub pci: bool,
    /// rtw89_core_start went through (JS `st.core`): the stop runs Linux's
    /// rtw89_hci_deinit and, after the power-off, rtw89_hci_reset
    pub core: bool,
    /// the firmware's counters and flags (JS `d.fw` without the parsed file)
    pub fw: FwState,
    /// the TX path's WD pages (pcitx), built after the download (rtw89_pci_ops_reset's WD part)
    pub tx: Option<PciTx>,
    /// what the look after the download saw
    pub look: Option<FwdlLook>,
    /// what the PCIe basic config of the probe's end found
    pub basic_cfg: Option<BasicCfg>,
    /// R_AX_DMAC_FUNC_EN after rtw89_mac_init (the not-Linux log read of stage mac)
    pub dmac_func_en: Option<u32>,
    /// the PHY part (efuse, hal, the firmware's tables, the DM and tracking state)
    pub phy: Phy,
    /// the efuse dumps of the probe (physical, DAV, logical map, phycap)
    pub efuse_maps: Option<EfuseMaps>,
    /// the firmware's PHY capabilities as they came
    pub phycap: Option<PhyCap>,
}

impl Bringup {
    /// The probe thread's bring-up: the MAC waits as Linux's ([`macinit::MacWaits::probe`])
    pub fn new() -> Bringup {
        Bringup {
            mac: MacState::with_waits(macinit::MacWaits::probe()),
            ..Bringup::default()
        }
    }

    // Linux: rtw89_pci_probe (pci.c:4769) -> rtw89_pci_claim_device, rtw89_pci_setup_resource,
    // then rtw89_chip_info_setup (core.c:7383) up to its rtw89_mac_pwr_on (core.c:7391)
    // with rtw89_read_chip_ver before it. JS: bringup.mjs stage fw up to macPwrOnProbe;
    // the golden trace's lines 1-321.
    pub fn probe_power_on<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &RingConfig,
    ) -> Rtw89Result<()> {
        bus.note("stage fw")?;
        let s = not_linux(
            bus,
            "safety (bus master off, MAC off if on), then pci_set_master",
            safety::bus_master_on,
        )?;
        self.safety = Some(s);
        if s.powered_off {
            self.mac.power_on = Some(false);
        }
        self.rings = Some(pci_alloc::pci_alloc(bus, rings)?);
        let chip = power::read_chip_ver(bus)?;
        self.chip = Some(chip);
        self.phy.cv = chip.cv;
        if chip.cv != EXPECT_CV || chip.acv != EXPECT_ACV {
            return Err(Rtw89Error::UnsupportedChip {
                cv: chip.cv,
                acv: chip.acv,
            });
        }
        self.on = true;
        // after a failed power-on (its own off and on again inside) the state is unknown: the stop powers off
        let r = macinit::mac_pwr_on(bus, &PwrOpts::probe(chip.cv), &mut self.mac, &mut self.fw);
        self.power = Some(r?);
        Ok(())
    }

    // Linux: rtw89_mac_partial_init (mac.c:4307) of rtw89_chip_efuse_info_setup up to
    // rtw89_fw_download: rtw89_mac_ctrl_hci_dma_trx (mac.h:1642), rtw89_mac_dmac_pre_init
    // (mac.c:4258), the HCI's mac_pre_init = rtw89_pci_ops_mac_pre_init_ax (pci.c:3070);
    // rtw89_chip_bb_preinit is not called (include_bb false, and the 8852B has none).
    // JS: macinit.mjs macPartialInit without fwDownload; the golden trace's lines 322-459.
    // The firmware download that follows is the fw module's.
    pub fn partial_init_to_fwdl<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "partial init before the rings",
            value: 0,
        })?;
        self.pci = true;
        macinit::partial_init_to_fwdl(bus, rings, &mut self.mac)
    }

    // Linux: rtw89_mac_partial_init (mac.c:4307) whole, as rtw89_chip_efuse_info_setup calls it
    // at probe: up to the download as above, then rtw89_fw_download (fw.c:2043; type
    // NORMAL(_CE), include_bb false; rtw89_mac_fwdl_preconfig is none on AX). JS:
    // macinit.mjs macPartialInit; the golden trace's lines 322-1144. `fw` is the file as
    // rtw89_fw_recognize made it (fw::Firmware::parse with the chip's cv and secure boot).
    // Then rtw89_pci_ops_reset's WD part (pcitx init, host memory only), as bringup.mjs.
    pub fn partial_init<B: Bus + ?Sized>(&mut self, bus: &mut B, fw: &Firmware) -> Rtw89Result<()> {
        self.partial_init_to_fwdl(bus)?;
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "firmware download before the rings",
            value: 0,
        })?;
        fw::download(bus, rings, fw, &mut self.fw)?;
        self.tx_init();
        Ok(())
    }

    // rtw89_pci_ops_reset's WD part (pcitx init, host memory only), as bringup.mjs's
    // pciTxInit after each partial init
    fn tx_init(&mut self) {
        let Some(rings) = self.rings.as_ref() else {
            return;
        };
        let mut tx = PciTx::new();
        tx.init(rings);
        self.tx = Some(tx);
    }

    // Not Linux (JS bringup.mjs, log only; golden lines 1145-1149): the FWDL status register
    // and a look at RXQ (rtw89_pci_napi_poll's RXQ part), C2Hs parsed as t2-fw.mjs does.
    // Kept in `look` for the caller's log.
    pub fn fwdl_look<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "look at RXQ before the rings",
            value: 0,
        })?;
        let look = not_linux(bus, "FWDL status read and a look at RXQ (log only)", |b| {
            let fw_ctrl = b.r32(R_AX_WCPU_FW_CTRL)?;
            let bufs = pci::rx_poll(b, rings, "RXQ", LOOK_RX_BUDGET)?;
            let mut look = FwdlLook {
                fw_ctrl,
                status: field(fw_ctrl, B_AX_WCPU_FWDL_STS_MASK),
                bufs: bufs.len(),
                ..FwdlLook::default()
            };
            for buf in &bufs {
                let Some(p) = fw::rx_pkt(&buf.data) else {
                    look.other.push((u32::MAX, 0));
                    continue;
                };
                match (p.typ == RTW89_CORE_RX_TYPE_C2H, fw::c2h_parse(&p.body)) {
                    (true, Ok(c)) => look.c2h.push(c),
                    _ => look.other.push((p.typ, p.pkt_size)),
                }
            }
            Ok(look)
        })?;
        self.look = Some(look);
        Ok(())
    }

    // Linux: rtw89_chip_efuse_info_setup (core.c:7274) after rtw89_mac_partial_init:
    // rtw89_parse_efuse_map (the autoload bit, the physical, DAV and logical maps; JS
    // bringup-steps.mjs freshEfuse), rtw89_parse_phycap_map, rtw89_core_setup_phycap, then the
    // not-Linux efuse check (`check_efuse`), then rtw89_mac_setup_phycap (the firmware's
    // GET_FEATURE answer; the probe thread waits Linux's 1 s for it, B4). The tables of the
    // firmware file's elements are taken here too (rtw89_fw_recognize_elements). Sets `rfe`
    // from the parsed efuse once it passed the check; a refused efuse leaves it 0, so the stop
    // powers off as the probe powered on. JS: bringup.mjs stage fw (golden lines 1150-6656).
    pub fn probe_efuse<B: Bus + ?Sized>(&mut self, bus: &mut B, fw: &Firmware) -> Rtw89Result<()> {
        let cv = self.chip.map(|c| c.cv).ok_or(Rtw89Error::Invalid {
            what: "efuse before the chip cut",
            value: 0,
        })?;
        self.phy.cv = cv;
        self.phy.load_tables(fw);
        let mut retries = Vec::new();
        let r = phy::efuse::dump_maps(bus, cv, &mut |retry, e| retries.push((retry, e.clone())));
        for (retry, error) in retries {
            self.phy.push(PhyEvent::EfuseDumpRetry { retry, error });
        }
        let mut maps = r?;
        let r = phy::efuse_parse(bus, &mut self.phy, &mut maps);
        let ctrl = maps.ctrl;
        self.efuse_maps = Some(maps);
        r?;
        let ef = self.phy.efuse.as_ref().ok_or(Rtw89Error::Invalid {
            what: "efuse not parsed",
            value: 0,
        })?;
        check_efuse(ef, ctrl)?;
        self.rfe = ef.rfe_type;
        self.phycap = Some(phy::mac_setup_phycap(bus, &mut self.phy, &mut self.fw, 0)?);
        Ok(())
    }

    /// The power-on options of stage mac from the parsed efuse (rtw89_mac_preinit's
    /// rtw8852b_pwr_on_func reads rfe_type, efuse.valid and power_k_valid)
    pub fn pwr_opts(&self) -> Rtw89Result<PwrOpts> {
        let ef = self.phy.efuse.as_ref().ok_or(Rtw89Error::Invalid {
            what: "power-on options before the efuse",
            value: 0,
        })?;
        Ok(PwrOpts {
            rfe: ef.rfe_type,
            efuse_valid: ef.valid,
            power_k_valid: ef.power_k_valid,
            cv: self.phy.cv,
            probe: false,
        })
    }

    // The end of rtw89_chip_info_setup (core.c:7383) after the efuse and phycap (phy's):
    // rtw89_hci_mac_pre_deinit = rtw89_pci_ops_mac_pre_deinit_ax (pci.c:3146), then
    // rtw89_mac_pwr_off (core.c:7434). `rfe` is the parsed efuse's rfe_type (the
    // power-off's RFE 5 branch); it stays for the stop. JS: bringup.mjs stage fw after
    // macSetupPhycap.
    pub fn probe_done<B: Bus + ?Sized>(&mut self, bus: &mut B, rfe: u8) -> Rtw89Result<()> {
        self.rfe = rfe;
        pci::mac_pre_deinit(bus)?;
        macinit::mac_pwr_off(bus, self.rfe, &mut self.mac, &mut self.fw)?;
        self.pci = false;
        Ok(())
    }

    // Linux: rtw89_pci_basic_cfg (pci.c:4823 in rtw89_pci_probe, after the chip info
    // setup), then rtw89_rfkill_polling_init (core.c:7341) of rtw89_core_register_hw. A
    // blocked radio stops the bring-up (mac80211 would not start it). JS: bringup.mjs stage
    // fw's end.
    pub fn probe_end<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "PCIe basic config before the rings",
            value: 0,
        })?;
        self.basic_cfg = Some(pci::basic_cfg(bus, rings)?);
        rfkill::rfkill_init(bus)
    }

    // Stage mac: rtw89_core_start (core.c:6658) up to rtw89_btc_ntfy_poweron:
    // rtw89_mac_preinit (power on with the efuse's options `opts`), rtw89_phy_init_bb_afe
    // (phy's; nothing for this firmware: no AFE_PWR_SEQ element), rtw89_mac_init with the
    // firmware download again, the TX path's WD pages (pcitx init), and a not-Linux log
    // read of R_AX_DMAC_FUNC_EN (kept in `dmac_func_en`). rtw89_btc_ntfy_poweron (btc's)
    // writes no register. JS: bringup.mjs stage mac; the golden trace's lines 6699-7810.
    pub fn core_start_mac<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        fw: &Firmware,
        opts: &PwrOpts,
    ) -> Rtw89Result<()> {
        bus.note("stage mac")?;
        self.rfe = opts.rfe;
        self.mac.power_on = None; // unknown until the power-on below went through
        macinit::mac_preinit(bus, opts, &mut self.mac, &mut self.fw)?;
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "MAC init before the rings",
            value: 0,
        })?;
        self.pci = true;
        let c = MacInit {
            rings,
            mac: &mut self.mac,
            fws: &mut self.fw,
            fw,
            opts,
        };
        macinit::mac_init(bus, c, false)?;
        self.tx_init();
        self.core = true;
        let v = not_linux(bus, "log read", |b| Ok(b.r32(R_AX_DMAC_FUNC_EN)?))?;
        self.dmac_func_en = Some(v);
        Ok(())
    }

    // Stage phy: rtw89_core_start (core.c:6658) after rtw89_btc_ntfy_poweron: rtw89_chip_reset_bb_rf,
    // rtw89_phy_init_bb_reg (with chan's init_txpwr_unit), rtw89_phy_init_rf_reg, the not-Linux
    // table check, rtw89_btc_ntfy_init (btc's), rtw89_phy_dm_init (with rfk1's rfk_init and
    // chan's set_txpwr_ctrl, power_trim, cfg_txrx_path; the default channel, nothing
    // associated), then the start's tail (PPDU status, RTS threshold, the track work, btc's
    // radio-on, the firmware log H2C). JS: bringup.mjs stage phy; the golden trace's lines
    // 7811-15605.
    pub fn core_start_phy<B: Bus + ?Sized, H: PhyStageHooks<B>>(
        &mut self,
        bus: &mut B,
        hooks: &mut H,
    ) -> Rtw89Result<()> {
        bus.note("stage phy")?;
        phy::reset_bb_rf(bus)?;
        phy::init_bb_reg(bus, &mut self.phy, hooks)?;
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "RF tables before the rings",
            value: 0,
        })?;
        phy::init_rf_reg(bus, &mut self.phy, rings, &mut self.fw)?;
        phy::check_tables(&self.phy)?;
        hooks.btc_init(bus, rings, &mut self.fw, &self.phy)?;
        let mac = crate::rfk1::MacLink {
            mac: &mut self.mac,
            fw: &mut self.fw,
            rings: Some(rings),
        };
        phy::phy_dm_init(bus, &mut self.phy, None, 0, Some(mac), hooks)?;
        let rings = self.rings.as_mut().ok_or(Rtw89Error::Invalid {
            what: "core start before the rings",
            value: 0,
        })?;
        let mut radio_on = |b: &mut B, r: &mut PciRings, f: &mut FwState| {
            hooks.track_start();
            hooks.radio_on(b, r, f)
        };
        macinit::core_start_tail(bus, rings, &mut self.fw, None, Some(&mut radio_on))
    }

    // JS bringup.mjs `stop` for what the ported steps can have switched on: the PCIe DMA
    // (rtw89_hci_deinit = rtw89_pci_ops_deinit, pci.c:3053, after the core ran; before it,
    // the same as a safety step, not Linux), the power-off
    // (rtw89_mac_pwr_off, mac.c:1603, as rtw89_core_stop core.c:6727 does), after the
    // core ran rtw89_hci_reset (rtw89_pci_ops_reset, pci.c:1878) once it is off, and bus
    // master off (not Linux: rtw89_pci_declaim_device, pci.c:3290, disables the whole
    // device; BitsOS gives back the command register it found, memory space kept),
    // then the rings' DMA regions back (rtw89_pci_free_trx_rings, pci.c:3505).
    // Each step runs even when one before failed; the first error is returned.
    pub fn stop<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let note = bus.note("stop").map_err(Into::into);
        let rest = self.stop_hci(bus);
        note.and(rest)
    }

    // The part of `stop` from rtw89_hci_deinit on (rtw89_core_stop core.c:6727 after the
    // coexistence stop and the dropping flushes; JS bringup.mjs `stop` from pciDeinit): for a
    // caller that ran the interface down and the coexistence off itself after the stop's note.
    pub fn stop_hci<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let mut first: Option<Rtw89Error> = None;
        let mut keep = |r: Rtw89Result<()>| {
            if let Err(e) = r {
                first.get_or_insert(e);
            }
        };
        if self.core {
            keep(pci::deinit(bus));
        } else if self.pci {
            keep(not_linux(bus, "DMA off before this power-off", pci::deinit));
        }
        let mut off = false;
        if self.on && self.mac.power_on != Some(false) {
            // clears RTW89_FLAG_FW_RDY with the power flags (mac.c:1572-1578)
            let r = macinit::mac_pwr_off(bus, self.rfe, &mut self.mac, &mut self.fw);
            if r.is_ok() {
                off = true;
                keep(not_linux(bus, "log read", |b| {
                    b.r32(R_AX_IC_PWR_STATE).map(|_| ()).map_err(Into::into)
                }));
            }
            keep(r);
        }
        if let (true, Some(rings)) = (self.core && off, self.rings.as_mut()) {
            keep(pci::reset(bus, rings));
        }
        if let Some(s) = self.safety.take() {
            keep(not_linux(bus, "PCI command back, bus master off", |b| {
                b.cfg_write(safety::PCI_COMMAND, s.cmd0)?;
                b.cfg_read(safety::PCI_COMMAND)?;
                Ok(())
            }));
        }
        if let Some(mut rings) = self.rings.take() {
            keep(rings.free(bus));
        }
        self.on = false;
        self.pci = false;
        self.core = false;
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
