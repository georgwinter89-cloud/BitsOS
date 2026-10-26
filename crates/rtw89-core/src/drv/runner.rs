//! The driver thread's work, generic over the bus so the fake chip runs it as the kernel does
//! (spec/wlan.md §2.4 trap 2: the chip is served from its own thread, independent of programs).
//! One [`Runner::turn`] per wake-up: the firmware look every 2 s, the station's supervisor step
//! (poll, rejoin), the track work, queued frames out; a broken chip ([`super::health`]) is
//! switched off and brought up again, a bring-up that fails is tried again by the supervisor's
//! rhythm (2 s, 10 s x6, then every 60 s). Not Linux: there mac80211, wpa_supplicant, NAPI and
//! rtw89_ser share this.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::bus::Bus;
use crate::error::Rtw89Error;
use crate::fw::Firmware;
use crate::pci::intr::{self, Masks};
use crate::phy::Band;
use crate::sta::link::{self, Joined, LinkError};
use crate::sta::scan::{self, ScanReq};
use crate::sta::supervise::{Step, Wlan, WlanCfg};
use crate::sta::{self, LinkState, StaEnv};

use wifi::events::LinkLoss;
use wifi::scan::Bss;
use wifi::supervisor::{Rhythm, SvState};

use super::device::{DevConfig, Device};
use super::events::{DevEvent, Level};
use super::health::{self, FwHealth, Health, Verdict};

/// RX poll period while linked without MSI (spec/wlan.md §3: MSI on the chip is untested)
pub const POLL_MS: u64 = 2;
/// The safety poll while linked when MSI wakes the thread (the RXQ of 256 holds ~0.4 s)
pub const IRQ_POLL_MS: u64 = 20;
/// The default scan: every 2.4 GHz channel of the DE domain, V1 bringup.mjs's 120 ms dwell
pub const SCAN_CHANNELS_2G: [u32; 13] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
pub const SCAN_DWELL_MS: u32 = 120;

/// Counters for the status (MCP `wlan`)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    /// queued frames that could not go (no link, a chip error)
    pub tx_dropped: u64,
    /// data frames of our AP the station dropped (PN replay, not decrypted, ...)
    pub rx_dropped: u64,
    pub joins: u64,
    pub join_failures: u64,
    pub link_losses: u64,
    pub restarts: u64,
    pub fw_repaired: u64,
    /// MSI said the RX ring had no free buffer (RDU)
    pub rx_unavailable: u64,
}

/// What a turn gave the network side
#[derive(Debug, Default)]
pub struct TurnOut {
    /// Ethernet frames for the stack
    pub frames: Vec<Vec<u8>>,
    /// the link went up (GTK installed) or down in this turn
    pub link: Option<bool>,
}

/// What the thread runs
pub struct Runner {
    fw: Firmware,
    pub cfg: DevConfig,
    pub dev: Option<Device>,
    pub wlan: Option<Wlan>,
    pub health: Health,
    /// the next bring-up when the chip is down, and how many failed in a row
    pub up_at: Option<u64>,
    pub up_attempt: u32,
    rhythm: Rhythm,
    /// the link with the group key in (the netdev's link)
    pub link: Option<Joined>,
    pub stats: Stats,
    pub last_error: Option<String>,
    /// what the module states logged since the caller took it
    pub log: Vec<DevEvent>,
    pub last_scan: Vec<Bss>,
    /// MSI: the masks the chip gets after each bring-up (`None`: polled)
    pub irq: Option<Masks>,
}

impl core::fmt::Debug for Runner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Runner")
            .field("up", &self.dev.is_some())
            .field("wlan", &self.wlan)
            .field("link", &self.link)
            .field("stats", &self.stats)
            .finish()
    }
}

/// The send found no room: sta's "TX ring full" (pcitx took nothing after a reclaim) or a ring's own
fn ring_full(e: &Rtw89Error) -> bool {
    matches!(
        e,
        Rtw89Error::RingFull { .. }
            | Rtw89Error::Frame {
                what: "TX ring full",
                ..
            }
    )
}

fn err_text(e: &LinkError) -> String {
    format!("{e}")
}

impl Runner {
    pub fn new(fw: Firmware, cfg: DevConfig) -> Runner {
        Runner {
            fw,
            cfg,
            dev: None,
            wlan: None,
            health: Health::default(),
            up_at: Some(0),
            up_attempt: 0,
            rhythm: Rhythm::default(),
            link: None,
            stats: Stats::default(),
            last_error: None,
            log: Vec::new(),
            last_scan: Vec::new(),
            irq: None,
        }
    }

    pub fn firmware(&self) -> &Firmware {
        &self.fw
    }

    fn say(&mut self, level: Level, name: &'static str, text: String) {
        self.log.push(DevEvent::new(level, name, text));
    }

    /// The network to join (CONFIG.TXT); `None`: none. The old one (and its PMK) goes.
    pub fn set_network(&mut self, cfg: Option<WlanCfg>, now: u64) {
        self.wlan = cfg.map(|c| {
            let mut w = Wlan::new(c);
            w.sup.on(now);
            w
        });
    }

    /// The chip up (the first bring-up and every one after a failure); false when it failed
    pub fn bring_up<B: Bus + ?Sized>(&mut self, bus: &mut B, env: &mut dyn StaEnv<B>) -> bool {
        let r = Device::bring_up(bus, &self.fw, &self.cfg, env, &mut self.log);
        let now = env.now_ms(bus);
        match r {
            Ok(dev) => {
                if let Some(m) = self.irq.as_ref()
                    && let Err(e) = intr::enable(bus, m)
                {
                    self.say(Level::Warn, "irq.enable_failed", format!("{e:?}"));
                }
                self.cfg.keep_mac.get_or_insert(dev.mac);
                let mac = super::device::mac_str(&dev.mac);
                self.say(Level::Info, "chip.up", format!("chip up, interface {mac}"));
                self.dev = Some(dev);
                self.health = Health::new(now);
                self.up_at = None;
                self.up_attempt = 0;
                true
            }
            Err(e) => {
                self.up_attempt = self.up_attempt.saturating_add(1);
                let wait = self.rhythm.delay(self.up_attempt);
                let text = format!("{e}");
                self.say(
                    Level::Error,
                    "chip.up_failed",
                    format!("{text}; try {} in {} s", self.up_attempt + 1, wait / 1000),
                );
                self.last_error = Some(text);
                self.up_at = Some(now.saturating_add(wait));
                false
            }
        }
    }

    // The chip off after a verdict, then up again at once (V1 K6: "chip switched off after its
    // failure, joining again"); the supervisor joins by its rhythm.
    fn restart<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        env: &mut dyn StaEnv<B>,
        v: Verdict,
        out: &mut TurnOut,
    ) {
        let now = env.now_ms(bus);
        let text = format!("{v}");
        self.say(
            Level::Error,
            "chip.failed",
            format!("{text}: switching the chip off and on"),
        );
        self.last_error = Some(text);
        self.stats.restarts += 1;
        if let Some(mut d) = self.dev.take() {
            if self.irq.is_some() {
                let _ = intr::disable(bus);
            }
            if let Err(e) = d.power_off(bus) {
                self.say(Level::Warn, "chip.off_failed", format!("{e:?}"));
            }
            d.take_events(&mut self.log);
        }
        self.link_down(now, LinkLoss::ChipFailed, out);
        self.bring_up(bus, env);
    }

    fn link_down(&mut self, now: u64, why: LinkLoss, out: &mut TurnOut) {
        if self.link.take().is_some() {
            out.link = Some(false);
            self.stats.link_losses += 1;
        }
        if let Some(w) = self.wlan.as_mut()
            && matches!(w.sup.state(), SvState::Up | SvState::Joining)
        {
            w.sup.link_lost(now, why);
        }
    }

    /// A chip error of this turn: a restart when it was a dead chip or the rounds say so
    fn chip_error<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        err: &Rtw89Error,
        now: u64,
    ) -> Option<Verdict> {
        if let Some(v) = health::dead_verdict(bus, err) {
            return Some(v);
        }
        self.health.failed(now)
    }

    /// One wake-up of the driver thread; `tx` holds the stack's frames (taken from the front)
    pub fn turn<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        env: &mut dyn StaEnv<B>,
        tx: &mut VecDeque<Vec<u8>>,
    ) -> TurnOut {
        let mut out = TurnOut::default();
        let now = env.now_ms(bus);
        let logged = self.log.len();
        if self.dev.is_none() {
            self.drop_tx(tx);
            if self.up_at.is_some_and(|t| now >= t) {
                self.bring_up(bus, env);
            }
            return out;
        }
        if let Some(v) = self.fw_look(bus, now) {
            self.restart(bus, env, v, &mut out);
            return out;
        }
        if let Some(v) = self.station(bus, env, now, &mut out) {
            self.drop_tx(tx);
            self.restart(bus, env, v, &mut out);
            return out;
        }
        if let Some(v) = self.send(bus, env, now, tx) {
            self.restart(bus, env, v, &mut out);
            return out;
        }
        if let Some(d) = self.dev.as_mut() {
            d.take_events(&mut self.log);
        }
        self.stats.rx_dropped += self.log[logged.min(self.log.len())..]
            .iter()
            .filter(|e| e.name == "rx.dropped")
            .count() as u64;
        out
    }

    // the firmware's own state every HEALTH_MS
    fn fw_look<B: Bus + ?Sized>(&mut self, bus: &mut B, now: u64) -> Option<Verdict> {
        if now < self.health.fw_due {
            return None;
        }
        self.health.fw_due = now + health::HEALTH_MS;
        match health::fw_check(bus) {
            Ok(FwHealth::Running) => None,
            Ok(h @ FwHealth::Repaired { .. }) => {
                self.stats.fw_repaired += 1;
                self.say(Level::Warn, "fw.repaired", health::fw_text(&h));
                None
            }
            Ok(h @ FwHealth::Halted { code, .. }) => {
                self.say(Level::Error, "fw.halted", health::fw_text(&h));
                Some(Verdict::FwHalted { code })
            }
            Ok(FwHealth::NotReady { value }) => Some(Verdict::FwNotReady { value }),
            Err(e) => {
                self.say(Level::Warn, "fw.look_failed", format!("{e:?}"));
                self.chip_error(bus, &e, now)
            }
        }
    }

    // the supervisor's step and the track work
    fn station<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        env: &mut dyn StaEnv<B>,
        now: u64,
        out: &mut TurnOut,
    ) -> Option<Verdict> {
        let dev = self.dev.as_mut()?;
        let step = match self.wlan.as_mut() {
            Some(w) => {
                let (mut chip, sta) = dev.parts();
                w.step(bus, &mut chip, sta, env)
            }
            None => Ok(Step::Idle),
        };
        let step = match step {
            Ok(s) => s,
            Err(e) => {
                self.say(Level::Error, "step.failed", format!("{e:?}"));
                return self.chip_error(bus, &e, now);
            }
        };
        match step {
            Step::Idle => {}
            Step::Frames(f) => {
                self.health.good();
                self.stats.rx_frames += f.len() as u64;
                self.stats.rx_bytes += f.iter().map(|x| x.len() as u64).sum::<u64>();
                out.frames = f;
            }
            Step::Joined(j) if j.group_key => {
                self.health.good();
                self.stats.joins += 1;
                self.link = Some(j);
                out.link = Some(true);
            }
            Step::Joined(_) => {
                // Joined::group_key is always true (WL-3); a link without the GTK would lose every
                // broadcast (DHCP offers come as broadcasts): leave and join again
                self.say(
                    Level::Error,
                    "link.no_gtk",
                    String::from("joined without a group key: leaving"),
                );
                let dev = self.dev.as_mut()?;
                let (mut chip, sta) = dev.parts();
                let _ =
                    link::disconnect(bus, &mut chip, sta, env, wifi::mlme::reason::DEAUTH_LEAVING);
                if let Some(w) = self.wlan.as_mut() {
                    w.sup.link_lost(now, LinkLoss::JoinFailed);
                }
            }
            Step::JoinFailed(e) => {
                self.stats.join_failures += 1;
                self.last_error = Some(err_text(&e));
                if let LinkError::Chip(err) = &e {
                    return self.chip_error(bus, err, now);
                }
            }
            Step::LinkLost(why) => {
                self.link_down(now, why, out);
                if why == LinkLoss::ChipFailed {
                    return Some(Verdict::LinkChipError);
                }
            }
        }
        let dev = self.dev.as_mut()?;
        if dev.sta.state != LinkState::Down {
            let (mut chip, sta) = dev.parts();
            if let Err(e) = sta::track::track_due(bus, &mut chip, sta, now) {
                self.say(Level::Warn, "track.failed", format!("{e:?}"));
                return self.chip_error(bus, &e, now);
            }
        }
        None
    }

    // queued frames out while linked; without a link they go (the stack sends again)
    fn send<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        env: &mut dyn StaEnv<B>,
        now: u64,
        tx: &mut VecDeque<Vec<u8>>,
    ) -> Option<Verdict> {
        if self.link.is_none() {
            self.drop_tx(tx);
            return None;
        }
        while let Some(f) = tx.front() {
            let dev = self.dev.as_mut()?;
            let (mut chip, sta) = dev.parts();
            match link::send(bus, &mut chip, sta, env, f) {
                Ok(()) => {
                    self.stats.tx_frames += 1;
                    self.stats.tx_bytes += f.len() as u64;
                    tx.pop_front();
                    self.health.tx(now, false);
                }
                Err(LinkError::Chip(e)) if ring_full(&e) => {
                    // the frame stays queued; the ring may drain by the next turn
                    return self.health.tx(now, true);
                }
                Err(LinkError::Chip(e)) => {
                    tx.pop_front();
                    self.stats.tx_dropped += 1;
                    self.say(Level::Warn, "tx.failed", format!("{e:?}"));
                    return self.chip_error(bus, &e, now);
                }
                Err(_) => {
                    tx.pop_front();
                    self.stats.tx_dropped += 1;
                }
            }
        }
        None
    }

    fn drop_tx(&mut self, tx: &mut VecDeque<Vec<u8>>) {
        self.stats.tx_dropped += tx.len() as u64;
        tx.clear();
    }

    /// When the thread must turn again at the latest
    pub fn next_wake(&self, now: u64) -> u64 {
        let Some(dev) = self.dev.as_ref() else {
            return self.up_at.unwrap_or(now + health::HEALTH_MS);
        };
        let mut t = self.health.fw_due;
        let mut take = |x: Option<u64>| {
            if let Some(x) = x {
                t = t.min(x);
            }
        };
        take(self.wlan.as_ref().and_then(|w| w.next_wake()));
        if dev.sta.state != LinkState::Down {
            take(dev.sta.track.due);
            take(dev.sta.track.cfo_due);
        }
        if dev.sta.state == LinkState::Connected {
            take(Some(
                now + if self.irq.is_some() {
                    IRQ_POLL_MS
                } else {
                    POLL_MS
                },
            ));
        }
        t.max(now)
    }

    /// Join the configured network (again): the supervisor on
    pub fn join(&mut self, now: u64) -> Result<(), String> {
        let w = self.wlan.as_mut().ok_or_else(|| {
            String::from("no network configured (wlan_ssid / wlan_psk in CONFIG.TXT)")
        })?;
        if w.sup.state() == SvState::Off {
            w.sup.on(now);
        }
        Ok(())
    }

    /// Leave and stay off (no rejoin until `join`); the CH12 copies of the keys are wiped
    pub fn leave<B: Bus + ?Sized>(&mut self, bus: &mut B, env: &mut dyn StaEnv<B>) -> TurnOut {
        let mut out = TurnOut::default();
        let now = env.now_ms(bus);
        if self.link.take().is_some() {
            out.link = Some(false);
        }
        let (Some(dev), Some(w)) = (self.dev.as_mut(), self.wlan.as_mut()) else {
            if let Some(w) = self.wlan.as_mut() {
                w.sup.off();
            }
            return out;
        };
        let r = {
            let (mut chip, sta) = dev.parts();
            w.off(bus, &mut chip, sta, env)
        };
        let wipe = dev.wipe_ch12(bus);
        dev.take_events(&mut self.log);
        for e in [r.err(), wipe.err()].into_iter().flatten() {
            self.say(Level::Warn, "leave.chip_error", format!("{e:?}"));
            if let Some(v) = self.chip_error(bus, &e, now) {
                self.restart(bus, env, v, &mut out);
                // the restart keeps the supervisor off: `off` came first
                break;
            }
        }
        out
    }

    /// A scan of `channels` (default: every 2.4 GHz channel); only while not linked or joining
    pub fn scan<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        env: &mut dyn StaEnv<B>,
        channels: &[u32],
        dwell_ms: u32,
    ) -> Result<Vec<Bss>, String> {
        let dev = self
            .dev
            .as_mut()
            .ok_or_else(|| String::from("the chip is not up"))?;
        if dev.sta.state != LinkState::Up {
            return Err(String::from(
                "a scan runs only without a link (leave first): scanning while associated is not ported",
            ));
        }
        let band = if channels.iter().all(|&c| c <= 14) {
            Band::B2g
        } else {
            Band::B5g
        };
        if band == Band::B5g && channels.iter().any(|&c| c <= 14) {
            return Err(String::from("one band per scan"));
        }
        let req = ScanReq::new(band, channels, dwell_ms);
        let r = {
            let (mut chip, sta) = dev.parts();
            scan::scan(bus, &mut chip, sta, env, &req)
        };
        dev.take_events(&mut self.log);
        match r {
            Ok(list) => {
                self.last_scan = list.clone();
                Ok(list)
            }
            Err(e) => Err(format!("scan: {e:?}")),
        }
    }

    /// The clean way down (Reboot by the console, driver unload): leave, remove the interface,
    /// btc off, flushes, power off. The chip is down afterwards and stays down.
    pub fn stop<B: Bus + ?Sized>(&mut self, bus: &mut B, env: &mut dyn StaEnv<B>) {
        if let Some(w) = self.wlan.as_mut() {
            w.sup.off();
        }
        self.link = None;
        self.up_at = None;
        if let Some(mut d) = self.dev.take() {
            if self.irq.is_some() {
                let _ = intr::disable(bus);
            }
            if let Err(e) = d.shutdown(bus, env) {
                self.say(Level::Warn, "chip.stop_failed", format!("{e:?}"));
            }
            d.take_events(&mut self.log);
            self.say(Level::Info, "chip.off", String::from("chip switched off"));
        }
    }

    /// Quiesce (Reload, reboot, panic): DMA off, MAC off, bus master off; no frame, no H2C
    pub fn quiesce<B: Bus + ?Sized>(&mut self, bus: &mut B) {
        self.link = None;
        self.up_at = None;
        if let Some(mut d) = self.dev.take() {
            if self.irq.is_some() {
                let _ = intr::disable(bus);
            }
            let _ = d.power_off(bus);
        }
    }

    /// Woken by MSI: the chip masked, its causes taken and cleared (rtw89_pci_interrupt_handler and
    /// _threadfn's recognize); a posted halt makes the firmware look due now. [`Runner::irq_rearm`]
    /// after the turn unmasks.
    pub fn irq_ack<B: Bus + ?Sized>(&mut self, bus: &mut B, now: u64) {
        let (Some(m), Some(_)) = (self.irq, self.dev.as_ref()) else {
            return;
        };
        let r = intr::disable(bus).and_then(|()| intr::recognize(bus, &m));
        match r {
            Ok(s) => {
                if s.halt() {
                    self.health.fw_due = now;
                }
                if s.rx_unavailable() {
                    self.stats.rx_unavailable += 1;
                }
            }
            Err(e) => self.say(Level::Warn, "irq.ack_failed", format!("{e:?}")),
        }
    }

    /// The masks back after a turn woken by MSI (rtw89_pci_napi_poll's rtw89_chip_enable_intr)
    pub fn irq_rearm<B: Bus + ?Sized>(&mut self, bus: &mut B) {
        if let (Some(m), Some(_)) = (self.irq, self.dev.as_ref())
            && let Err(e) = intr::enable(bus, &m)
        {
            self.say(Level::Warn, "irq.enable_failed", format!("{e:?}"));
        }
    }
}

/// The 802.11 rates (rtw89_bitrates, 100 kbit/s) of `supp_rates` on `band`: 2.4 GHz counts from
/// CCK 1 Mbit/s, 5 GHz from OFDM 6 Mbit/s (mac80211's sband->bitrates)
pub fn rates_100k(band: Band, supp: u32) -> Vec<u32> {
    const ALL: [u32; 12] = [10, 20, 55, 110, 60, 90, 120, 180, 240, 360, 480, 540];
    let list: &[u32] = if band == Band::B2g { &ALL } else { &ALL[4..] };
    list.iter()
        .enumerate()
        .filter(|(i, _)| supp & (1 << i) != 0)
        .map(|(_, r)| *r)
        .collect()
}

/// RTW89_RSSI_RAW_TO_DBM (core.h): the raw RSSI (two units per dB) as dBm
pub fn raw_rssi_dbm(raw: u32) -> i32 {
    ((raw >> 1) as i32) - crate::tables::regs::MAX_RSSI as i32
}
