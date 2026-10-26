//! Every call the station code makes into the other modules of this crate, in one place (JS
//! js/sta-io.mjs): fw's H2C, macinit's vif init / port update / RX filter, chan's channel switch
//! and the country's channel flags, phy's PPDU status parsing init and its tracking parts,
//! rfk1/rfk2's scan notification and channel calibration, btc's notifications, txrx/pcitx's frame
//! TX and the RX poll (rtw89_pci_napi_poll). [`Chip`] borrows the module states the driver keeps.

use alloc::vec::Vec;

use crate::bringup::Bringup;
use crate::btc::{self, BtcState, H2cLink, RoleReq, RoleState, ViaRfk, WlStaLink};
use crate::bus::Bus;
use crate::chan::{self, ChanFlags, ChanLink, ChanReq, ChanState};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::{self, C2h, FwState, H2c};
use crate::macinit::{self, FilterFlags, MacState, VifHooks};
use crate::pci::{self, PciRings};
use crate::pcitx::PciTx;
use crate::phy::{Band, Chan, Phy};
use crate::rfk1::{self, MacLink, RfkHooks, RfkState};
use crate::rfk2::{Rfk2Link, Rfk2State};
use crate::tables::regs::*;
use crate::txrx::{self, PhyOps, PhyPpdu, RxEntry, RxIn, TxOpts, TxSent, Txrx, TxrxChan};

use super::h2c::H2cMsg;

/// The module states the station drives, borrowed from the driver (JS `d`)
pub struct Chip<'a> {
    /// fw's counters, the MAC, the PHY, the PCIe rings and the TX path
    pub run: &'a mut Bringup,
    pub btc: &'a mut BtcState,
    pub rfk: &'a mut RfkState,
    pub rfk2: &'a mut Rfk2State,
    pub chan: &'a mut ChanState,
    pub txrx: &'a mut Txrx,
    /// RTW89_CHK_FW_FEATURE(BEACON_FILTER) of the loaded firmware (0.29.29.18 has it)
    pub beacon_filter: bool,
}

/// One item of an RX poll as the station gets it (sta-io.mjs rxPoll): in Linux's order. A frame
/// is moved through once per poll; boxing it would cost an allocation per frame.
/// Both large variants boxed: every RX loop (the H2C waits under a join among them) moved
/// RxItems through several slots of its frame
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxItem {
    /// a PPDU status where rtw89_core_rx_process_phy_sts iterates the stations
    Ppdu(alloc::boxed::Box<PhyPpdu>),
    C2h(C2h),
    /// a frame for mac80211 (FCS good)
    Frame(alloc::boxed::Box<RxEntry>),
}

fn no_rings() -> Rtw89Error {
    Rtw89Error::Invalid {
        what: "sta: the PCIe rings are not set up",
        value: 0,
    }
}

/// btc's RFK notifications and macinit's/fw's MAC side for rfk1/rfk2 (rfk2's own entries go
/// through `Rfk2Link` around this); `btc: None` refuses a notification (never one in a switch)
pub(crate) struct BtcRfk<'a> {
    pub(crate) btc: Option<&'a mut BtcState>,
    pub(crate) mac: MacLink<'a>,
}

fn not_here(what: &'static str) -> Rtw89Error {
    Rtw89Error::Invalid { what, value: 0 }
}

impl<B: Bus + ?Sized> RfkHooks<B> for BtcRfk<'_> {
    fn btc_wl_rfk(&mut self, bus: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()> {
        let st = self
            .btc
            .as_deref_mut()
            .ok_or(not_here("sta: btc RFK notification outside a calibration"))?;
        btc::role::wl_rfk(bus, st, &mut self.mac, phy_map, typ, state)?;
        Ok(())
    }
    fn btc_conn_rfk(&mut self, _: &mut B, start: bool) -> Rtw89Result<()> {
        if let Some(st) = self.btc.as_deref_mut() {
            btc::role::conn_rfk(st, start);
        }
        Ok(())
    }
    fn btc_preserve_bt_time(&mut self, bus: &mut B, ms: u32) -> Rtw89Result<()> {
        match self.btc.as_deref() {
            Some(st) => btc::role::preserve_bt_time(bus, st, ms),
            None => Ok(()),
        }
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.mac.stop_sch_tx(bus)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        self.mac.resume_sch_tx(bus, tx_en)
    }
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()> {
        self.mac.h2c(bus, h, payload)
    }
    fn dpk_init(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        Err(not_here("sta: rfk2's dpk_init goes through Rfk2Link"))
    }
    fn dpk(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        Err(not_here("sta: rfk2's dpk goes through Rfk2Link"))
    }
    fn dpk_track(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        Err(not_here("sta: rfk2's dpk_track goes through Rfk2Link"))
    }
    fn tssi(
        &mut self,
        _: &mut B,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
        _: bool,
    ) -> Rtw89Result<()> {
        Err(not_here("sta: rfk2's tssi goes through Rfk2Link"))
    }
    fn tssi_scan(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        Err(not_here("sta: rfk2's tssi_scan goes through Rfk2Link"))
    }
    fn wifi_scan_notify(&mut self, _: &mut B, _: &mut Phy, _: bool, _: u32) -> Rtw89Result<()> {
        Err(not_here("sta: rfk2's scan notify goes through Rfk2Link"))
    }
}

/// The steps of rtw89_mac_vif_init that are sta's (cam.c and their H2Cs), for macinit's vif_init
pub(super) struct VifSteps<'a> {
    pub fw: &'a mut FwState,
    pub rings: &'a mut PciRings,
    pub vif: &'a mut super::Vif,
    pub cam: &'a mut super::cam::CamInfo,
    pub antenna_tx: u32,
}

impl VifSteps<'_> {
    fn send<B: Bus + ?Sized>(&mut self, bus: &mut B, m: &H2cMsg) -> Rtw89Result<()> {
        fw::h2c_send(bus, self.rings, self.fw, &m.h, &m.payload)?;
        Ok(())
    }
}

// Linux: rtw89_mac_vif_init (mac.c:5045): its H2C and CAM steps (sta.mjs macVifHooks)
impl<B: Bus + ?Sized> VifHooks<B> for VifSteps<'_> {
    fn macid_pause(&mut self, bus: &mut B, sh: u32, grp: u32, pause: bool) -> Rtw89Result<()> {
        self.send(bus, &super::h2c::h2c_macid_pause(sh, grp, pause))
    }
    fn role_maintain(&mut self, bus: &mut B, upd: u32) -> Rtw89Result<()> {
        let m = super::h2c::h2c_role_maintain(self.vif, None, upd);
        self.send(bus, &m)
    }
    fn join_info(&mut self, bus: &mut B, dis_conn: bool) -> Rtw89Result<()> {
        let m = super::h2c::h2c_join_info(self.vif, None, dis_conn);
        self.send(bus, &m)
    }
    fn cam_init(&mut self, _: &mut B) -> Rtw89Result<()> {
        super::cam::cam_init(self.cam, self.vif)
    }
    fn addr_cam(&mut self, bus: &mut B, _upd: u32) -> Rtw89Result<()> {
        // 8852B: the v0 layout has no UPD_MODE word
        let m = super::h2c::h2c_addr_cam(self.vif, None, None);
        self.send(bus, &m)
    }
    fn default_cmac_tbl(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let m = super::h2c::h2c_default_cmac_tbl(self.vif, None, self.antenna_tx);
        self.send(bus, &m)
    }
}

impl Chip<'_> {
    pub fn rings(&mut self) -> Rtw89Result<&mut PciRings> {
        self.run.rings.as_mut().ok_or_else(no_rings)
    }

    // Linux: rtw89_h2c_tx (core.c:1373); header and sequence by fw's h2c_send
    pub fn h2c<B: Bus + ?Sized>(&mut self, bus: &mut B, m: &H2cMsg) -> Rtw89Result<()> {
        let rings = self.run.rings.as_mut().ok_or_else(no_rings)?;
        fw::h2c_send(bus, rings, &mut self.run.fw, &m.h, &m.payload)?;
        Ok(())
    }

    // Linux: rtw89_mac_vif_init (mac.c:5045): macinit's port and table part, sta's H2C and CAM
    // steps as hooks
    pub(super) fn vif_init<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        port_vif: &macinit::Vif,
        vif: &mut super::Vif,
        cam: &mut super::cam::CamInfo,
    ) -> Rtw89Result<()> {
        let antenna_tx = self.run.phy.hal.antenna_tx;
        let run = &mut *self.run;
        let rings = run.rings.as_mut().ok_or_else(no_rings)?;
        let mut steps = VifSteps {
            fw: &mut run.fw,
            rings,
            vif,
            cam,
            antenna_tx,
        };
        macinit::vif_init(bus, &mut run.mac, port_vif, &mut steps)
    }

    // Linux: rtw89_mac_port_update (mac.c:5104)
    pub fn port_update<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        vif: &macinit::Vif,
    ) -> Rtw89Result<()> {
        macinit::port_update(bus, &mut self.run.mac, vif)
    }

    // Linux: rtw89_ops_configure_filter (mac80211.c:308): macinit's rx_filter (hal.rx_fltr in
    // `MacState`)
    pub fn rx_filter<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        bcn_prbresp_promisc: bool,
        scanning: bool,
    ) -> Rtw89Result<u32> {
        let flags = FilterFlags {
            bcn_prbresp_promisc,
            ..self.run.mac.fif
        };
        macinit::rx_filter(bus, &mut self.run.mac, flags, scanning)
    }

    /// hal.rx_fltr as macinit keeps it
    pub fn mac(&self) -> &MacState {
        &self.run.mac
    }

    // Linux: rtw89_set_channel (core.c:600) for (band, ch, 20 MHz) without RF calibration; then
    // txrx's channel (with the one before, rtw89_chan_rcd)
    pub fn set_channel<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        band: Band,
        ch: u32,
    ) -> Rtw89Result<()> {
        let chan = Chan { band, ch, bw: 20 };
        let run = &mut *self.run;
        let mut hooks = BtcRfk {
            btc: None,
            mac: MacLink {
                mac: &mut run.mac,
                fw: &mut run.fw,
                rings: run.rings.as_mut(),
            },
        };
        let mut link = Rfk2Link {
            st: &mut *self.rfk2,
            hooks: &mut hooks,
            chan: Some(chan),
        };
        let bst = &mut *self.btc;
        let mut btc_band = |b: &mut B, band: Band, h: &mut dyn RfkHooks<B>| {
            btc::switch_band(b, bst, &mut ViaRfk(h), band)
        };
        let mut cl = ChanLink {
            rfk: &mut *self.rfk,
            hooks: &mut link,
            btc_switch_band: &mut btc_band,
        };
        let req = ChanReq { band, ch, bw: 20 };
        chan::set_channel(bus, &mut run.phy, self.chan, req, &mut cl, false)?;
        self.txrx.chan = Some(TxrxChan {
            band,
            ch,
            prev: (self.chan.prev_ch != 0).then_some((self.chan.prev_band, self.chan.prev_ch)),
        });
        Ok(())
    }

    // Linux: none (cfg80211's channel flags for the country, chan's regd)
    pub fn channel_flags(&self, band: Band, ch: u32) -> ChanFlags {
        self.chan.channel_flags(band, ch)
    }

    // Linux: rtw89_chip_rfk_scan (core.h:8738) -> rtw8852b_rfk_scan (rtw8852b.c:738)
    pub fn rfk_scan<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        start: bool,
        chan: Option<Chan>,
    ) -> Rtw89Result<()> {
        let run = &mut *self.run;
        let mut hooks = BtcRfk {
            btc: Some(&mut *self.btc),
            mac: MacLink {
                mac: &mut run.mac,
                fw: &mut run.fw,
                rings: run.rings.as_mut(),
            },
        };
        let mut link = Rfk2Link {
            st: &mut *self.rfk2,
            hooks: &mut hooks,
            chan,
        };
        rfk1::rfk_scan(bus, &mut run.phy, start, &mut link, RTW89_PHY_0)
    }

    // Linux: rtw89_chip_rfk_channel (core.c:521) -> rtw8852b_rfk_channel (rtw8852b.c:711), with
    // rtw89_core_rfk_record (core.c:490) before it: its RF reads are on the path (debugfs only)
    pub fn rfk_channel<B: Bus + ?Sized>(&mut self, bus: &mut B, chan: Chan) -> Rtw89Result<()> {
        for path in 0..RTW89_RFK_RECORD_PATH_NR {
            crate::phy::io::rf_r(bus, path, RR_CFGCH, RR_CFGCH_BAND0 | RR_CFGCH_CH)?;
            crate::phy::io::rf_r(bus, path, RR_VCO, RR_VCO_SEL | RR_VCO_STS)?;
            crate::phy::io::rf_r(bus, path, RR_SYNFB, RFREG_MASK)?;
        }
        let run = &mut *self.run;
        let mut hooks = BtcRfk {
            btc: Some(&mut *self.btc),
            mac: MacLink {
                mac: &mut run.mac,
                fw: &mut run.fw,
                rings: run.rings.as_mut(),
            },
        };
        let mut link = Rfk2Link {
            st: &mut *self.rfk2,
            hooks: &mut hooks,
            chan: Some(chan),
        };
        rfk1::rfk_channel(
            bus,
            &mut run.phy,
            self.rfk,
            Some(&chan),
            &mut link,
            RTW89_PHY_0,
        )
    }

    // Linux: rtw89_chip_rfk_track (core.h) -> rtw8852b_rfk_track (rtw8852b.c:746)
    pub fn rfk_track<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        chan: Option<Chan>,
    ) -> Rtw89Result<()> {
        let run = &mut *self.run;
        let mut hooks = BtcRfk {
            btc: Some(&mut *self.btc),
            mac: MacLink {
                mac: &mut run.mac,
                fw: &mut run.fw,
                rings: run.rings.as_mut(),
            },
        };
        let mut link = Rfk2Link {
            st: &mut *self.rfk2,
            hooks: &mut hooks,
            chan,
        };
        rfk1::rfk_track(bus, &mut run.phy, &mut link)
    }

    // Linux: rtw89_btc_ntfy_scan_start (coex.c:8784) / _scan_finish (coex.c:8814), PHY 0
    pub fn btc_scan<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        start: bool,
        band: Band,
    ) -> Rtw89Result<()> {
        let rings = self.run.rings.as_mut().ok_or_else(no_rings)?;
        let mut link = H2cLink {
            fw: &mut self.run.fw,
            rings,
        };
        btc::scan(bus, self.btc, &mut link, start, band)
    }

    // Linux: rtw89_btc_ntfy_role_info (coex.c:9229)
    pub fn btc_role<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        state: RoleState,
        req: &RoleReq,
    ) -> Rtw89Result<()> {
        let rings = self.run.rings.as_mut().ok_or_else(no_rings)?;
        let mut link = H2cLink {
            fw: &mut self.run.fw,
            rings,
        };
        btc::role::role(bus, self.btc, &mut link, state, req)
    }

    // Linux: rtw89_btc_ntfy_specific_packet (coex.c:8859)
    pub fn btc_packet<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        typ: btc::Packet,
    ) -> Rtw89Result<()> {
        let rings = self.run.rings.as_mut().ok_or_else(no_rings)?;
        let mut link = H2cLink {
            fw: &mut self.run.fw,
            rings,
        };
        btc::role::packet(bus, self.btc, &mut link, typ)
    }

    // Linux: rtw89_coex_act1_work (coex.c:7716)
    pub fn btc_act1_work<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let rings = self.run.rings.as_mut().ok_or_else(no_rings)?;
        let mut link = H2cLink {
            fw: &mut self.run.fw,
            rings,
        };
        btc::role::act1_work(bus, self.btc, &mut link)
    }

    // Linux: rtw89_btc_ntfy_wl_sta (coex.c:9655)
    pub fn btc_wl_sta<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        link: Option<&WlStaLink>,
    ) -> Rtw89Result<()> {
        let rings = self.run.rings.as_mut().ok_or_else(no_rings)?;
        let mut h = H2cLink {
            fw: &mut self.run.fw,
            rings,
        };
        btc::wlsta::wl_sta(bus, self.btc, &mut h, link)
    }

    // Linux: rtw89_btc_c2h_handle (coex.c:9767)
    pub fn btc_c2h(&mut self, c: &C2h) {
        btc::c2h(self.btc, c);
    }

    // Linux: rtw89_ops_tx (mac80211.c:19) / rtw89_core_txq_schedule: txrx's tx_frame (sequence
    // number, rtw89_core_tx_write, kick off). A full ring is reclaimed once
    // (rtw89_pci_poll_rpq_dma), then the frame goes again with its sequence number kept; still
    // full: refused (mac80211 would drop it).
    pub fn tx_frame<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        frame: &mut [u8],
        o: &TxOpts,
    ) -> Rtw89Result<TxSent> {
        let run = &mut *self.run;
        let rings = run.rings.as_mut().ok_or_else(no_rings)?;
        let tx: &mut PciTx = run.tx.as_mut().ok_or_else(no_rings)?;
        let sent = txrx::tx_frame(bus, self.txrx, tx, rings, frame, o)?;
        if sent.queued.is_some() {
            return Ok(sent);
        }
        tx.reclaim(bus, rings)?;
        let again = TxOpts {
            keep_seq: true,
            ..o.clone()
        };
        let sent = txrx::tx_frame(bus, self.txrx, tx, rings, frame, &again)?;
        if sent.queued.is_none() {
            return Err(Rtw89Error::Frame {
                what: "TX ring full",
                value: sent.info.ch_dma,
            });
        }
        Ok(sent)
    }

    // Linux: rtw89_pci_napi_poll (pci.c:4517): the release reports (RPQ, pcitx's reclaim) first,
    // then rtw89_pci_rxbd_deliver (pci.c:413) -> rtw89_core_rx (core.c:4527) for the RX queue:
    // PPDU statuses where rtw89_core_rx_process_phy_sts iterates the stations (only where
    // rtw89_core_rx_process_phy_ppdu succeeded), C2Hs, and the frames for mac80211 (FCS good)
    pub fn rx_poll<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        max: usize,
    ) -> Rtw89Result<Vec<RxItem>> {
        let run = &mut *self.run;
        let rings = run.rings.as_mut().ok_or_else(no_rings)?;
        let tx: &mut PciTx = run.tx.as_mut().ok_or_else(no_rings)?;
        tx.reclaim(bus, rings)?;
        let bufs = pci::rx_poll(bus, rings, "RXQ", max)?;
        let mut out = Vec::new();
        for b in &bufs {
            let r = txrx::rx_parse(self.txrx, RxIn::from(b), &mut PhyOps(&mut run.phy));
            if let Some(c) = r.c2h {
                out.push(RxItem::C2h(c));
            }
            if r.typ == txrx::RxType::Ppdu
                && let Some(p) = r.ppdu.filter(|p| p.hdr2_en.is_some())
            {
                out.push(RxItem::Ppdu(alloc::boxed::Box::new(p)));
            }
            for e in r.deliver {
                if !e.status.failed_fcs {
                    out.push(RxItem::Frame(alloc::boxed::Box::new(e)));
                }
            }
        }
        Ok(out)
    }
}
