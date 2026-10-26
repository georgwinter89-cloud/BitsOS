//! The WLAN interface as the driver thread runs it (spec/wlan.md §3: "Supervisor thread: rejoin
//! at 2 s, then backoff 10 s x6, then 60 s, independent of programs"): one [`Wlan::step`] per
//! wake-up polls the link, notices a link that ended (the AP's deauthentication or
//! disassociation, the firmware's beacon loss) and, when wifi's supervisor says so, scans and
//! joins the configured network again. The thread sleeps until [`Wlan::next_wake`] or the next
//! interrupt (B4: never an unbounded wait). Not Linux: there wpa_supplicant (reconnects) and
//! mac80211 (ieee80211_beacon_connection_loss_work) share this; V1 had it in nicwlanwatch.asm.

use alloc::vec::Vec;

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::phy::Band;

use wifi::crypto::Pmk;
use wifi::eapol::Supplicant;
use wifi::events::LinkLoss;
use wifi::mlme::JoinTiming;
use wifi::rsn;
use wifi::scan::ScanList;
use wifi::supervisor::{Rhythm, Supervisor};

use super::io::Chip;
use super::link::{self, Joined, LinkError};
use super::scan::{self, ScanReq};
use super::{LinkState, Sta, StaEnv, StaEvent};

/// IEEE 802.11 reason 4: disassociated due to inactivity (mac80211's connection loss:
/// ieee80211_beacon_connection_loss_work (linux-6.6/net/mac80211/mlme.c:3422) ->
/// __ieee80211_disconnect (linux-6.6/net/mac80211/mlme.c:3415), our deauthentication with
/// WLAN_REASON_DISASSOC_DUE_TO_INACTIVITY (linux-6.6/net/mac80211/mlme.c:3396)). mac80211 probes
/// the AP first after a plain beacon loss (ieee80211_mgd_probe_ap); BitsOS takes the firmware's
/// report (60 beacons missed) as the connection loss.
pub const REASON_INACTIVITY: u16 = 4;

/// What to join (CONFIG.TXT wlan_ssid= and the PMK of wlan_psk=; the PMK is wiped with this)
pub struct WlanCfg {
    pub ssid: Vec<u8>,
    pub pmk: Pmk,
    pub band: Band,
    pub channels: Vec<u32>,
    pub dwell_ms: u32,
    pub timing: JoinTiming,
}

impl core::fmt::Debug for WlanCfg {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WlanCfg")
            .field("ssid", &alloc::string::String::from_utf8_lossy(&self.ssid))
            .field("pmk", &self.pmk)
            .field("channels", &self.channels)
            .finish()
    }
}

/// What one step did
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// nothing to do now
    Idle,
    /// linked: the Ethernet frames that came
    Frames(Vec<Vec<u8>>),
    Joined(Joined),
    /// a join failed (the supervisor plans the next)
    JoinFailed(LinkError),
    /// the link went (the supervisor plans the join)
    LinkLost(LinkLoss),
}

/// The interface: the station's supervisor and what it joins
#[derive(Debug)]
pub struct Wlan {
    pub sup: Supervisor,
    pub cfg: WlanCfg,
}

impl Wlan {
    pub fn new(cfg: WlanCfg) -> Wlan {
        Wlan {
            sup: Supervisor::new(Rhythm::default()),
            cfg,
        }
    }

    /// When the thread must look again at the latest (`None`: when the chip has something)
    pub fn next_wake(&self) -> Option<u64> {
        self.sup.next_wake()
    }

    // Linux: none (BitsOS's supervisor step, see the module comment)
    pub fn step<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        chip: &mut Chip,
        sta: &mut Sta,
        env: &mut dyn StaEnv<B>,
    ) -> Rtw89Result<Step> {
        let now = env.now_ms(bus);
        for e in self.sup.take_events() {
            sta.push(StaEvent::Wifi(e));
        }
        if sta.state == LinkState::Connected {
            let p = match link::poll(bus, chip, sta, env) {
                Ok(p) => p,
                Err(err) => return Ok(self.chip_lost(bus, chip, sta, env, now, "link poll", err)),
            };
            if p.ended {
                let why = LinkLoss::EndedByAp {
                    reason: sta.last_reason,
                };
                self.sup.link_lost(now, why);
                return Ok(Step::LinkLost(why));
            }
            if sta.beacon_loss {
                // the connection is lost: we leave with reason 4 (our frame), as mac80211. The
                // link ends locally even if the frame or an H2C fails (teardown logs that), so
                // the supervisor is told in any case.
                sta.beacon_loss = false;
                let _ = link::disconnect(bus, chip, sta, env, REASON_INACTIVITY);
                self.sup.link_lost(now, LinkLoss::BeaconLoss);
                return Ok(Step::LinkLost(LinkLoss::BeaconLoss));
            }
            return Ok(Step::Frames(p.eth));
        }
        if !self.sup.tick(now) {
            return Ok(Step::Idle);
        }
        Ok(match self.join(bus, chip, sta, env) {
            Ok(j) => {
                self.sup.joined();
                Step::Joined(j)
            }
            Err(e) => {
                let now = env.now_ms(bus);
                self.sup.join_failed(now);
                Step::JoinFailed(e)
            }
        })
    }

    // the scan of the configured channels, the strongest usable BSS of the SSID, the join
    fn join<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        chip: &mut Chip,
        sta: &mut Sta,
        env: &mut dyn StaEnv<B>,
    ) -> Result<Joined, LinkError> {
        let req = ScanReq::new(self.cfg.band, &self.cfg.channels, self.cfg.dwell_ms);
        let mut list = ScanList::new();
        for b in scan::scan(bus, chip, sta, env, &req)? {
            list.add(b);
        }
        let bss = match list.choose(&self.cfg.ssid) {
            Some(Ok(b)) => b,
            Some(Err(u)) => return Err(LinkError::Unusable(u)),
            None => return Err(LinkError::Unusable(rsn::Unusable::NoRsn)),
        };
        let own = rsn::own_ie(bss.group_cipher());
        let sup = Supplicant::new(
            self.cfg.pmk.duplicate(),
            sta.vif.addr,
            bss.bssid,
            &own,
            bss.rsn_ie.as_deref(),
        )
        .map_err(|_| LinkError::Unusable(rsn::Unusable::NoRsn))?;
        link::connect(bus, chip, sta, env, &bss, sup, self.cfg.timing)
    }

    // Linux: none (Wlan "off"): no more joins; a link is left (reason 3), the handshake context
    // closed. The PMK goes with this `Wlan` (wiped on drop).
    pub fn off<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        chip: &mut Chip,
        sta: &mut Sta,
        env: &mut dyn StaEnv<B>,
    ) -> Rtw89Result<()> {
        self.sup.off();
        for e in self.sup.take_events() {
            sta.push(StaEvent::Wifi(e));
        }
        let left = if sta.ap.is_some() {
            link::disconnect(bus, chip, sta, env, wifi::mlme::reason::DEAUTH_LEAVING)
        } else {
            Ok(())
        };
        // the keys go whatever the chip said
        if let Some(m) = sta.mlme.as_mut() {
            m.close();
        }
        left
    }

    // A chip access of the running link failed: the link ends here (locally, without our frame:
    // the chip may be what failed), the supervisor joins again by its rhythm (a chip that stays
    // broken fails those joins, which back off), and the error is logged (B6). Not Linux: there
    // a firmware or bus error goes to rtw89_ser (the chip's own recovery), which BitsOS leaves to
    // the driver thread's health check (WL-4).
    #[allow(clippy::too_many_arguments)]
    fn chip_lost<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        chip: &mut Chip,
        sta: &mut Sta,
        env: &mut dyn StaEnv<B>,
        now: u64,
        during: &'static str,
        err: crate::error::Rtw89Error,
    ) -> Step {
        sta.push(StaEvent::ChipError { during, err });
        sta.beacon_loss = false;
        let _ = link::teardown(bus, chip, sta, env, 0);
        self.sup.link_lost(now, LinkLoss::ChipFailed);
        Step::LinkLost(LinkLoss::ChipFailed)
    }
}
