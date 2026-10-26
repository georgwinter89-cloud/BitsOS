//! One brought-up chip with every module state the station drives (JS `d`): Linux's probe
//! (rtw89_pci_probe -> rtw89_chip_info_setup), rtw89_core_start (stages mac and phy), then
//! mac80211's add_interface, in the golden trace's order (stages fw, mac, phy, chan); and the
//! two ways down: [`Device::shutdown`] (rtw89_core_stop's order after remove_interface) and
//! [`Device::power_off`] (Quiesce: DMA off, MAC off, bus master off, no H2C).

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

use crate::bringup::{Bringup, RingConfig};
use crate::btc::{self, BtcState, H2cLink};
use crate::bus::Bus;
use crate::chan::ChanState;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::Firmware;
use crate::pci::rings::CMD_BUF;
use crate::rfk1::RfkState;
use crate::rfk2::Rfk2State;
use crate::sta::io::Chip;
use crate::sta::{self, Sta, StaEnv};
use crate::txrx::Txrx;

use super::events::{self, DevEvent, Level};
use super::hooks::StageHooks;

/// The stages of a bring-up, for the log and for errors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// safety step, rings, chip version, the probe's power-on
    PowerOn,
    /// partial init and the firmware download, the look after it
    Firmware,
    /// efuse, phycap, the probe's power-off, PCIe basic config, rfkill
    Efuse,
    /// rtw89_core_start up to btc's power-on: MAC init with the second download
    Mac,
    /// BB/RF tables, calibrations, btc init and radio on
    Phy,
    /// add_interface
    Interface,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::PowerOn => "power_on",
            Stage::Firmware => "firmware",
            Stage::Efuse => "efuse",
            Stage::Mac => "mac",
            Stage::Phy => "phy",
            Stage::Interface => "interface",
        }
    }
}

/// A bring-up that stopped: where, why, and what the modules logged on the way
#[derive(Debug)]
pub struct UpError {
    pub stage: Stage,
    pub err: Rtw89Error,
}

impl core::fmt::Display for UpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bring-up stage {}: {:?}", self.stage.name(), self.err)
    }
}

/// What a bring-up needs besides the firmware
#[derive(Debug, Clone)]
pub struct DevConfig {
    pub rings: RingConfig,
    /// the MAC address of the first bring-up of this boot (spec/wlan.md §2.4 trap 3: a second
    /// efuse read once changed a bit; the address stays the first one, a difference is logged)
    pub keep_mac: Option<[u8; 6]>,
}

impl DevConfig {
    /// The kernel's rings: Linux's channels with lengths for one station on legacy rates. The
    /// RXQ holds 256 frames (V1's 64 filled in ~0.1 s on 2.4 GHz, spec/wlan.md §2.4 trap 2);
    /// about 1.9 MB of DMA memory below 4 GB (RingConfig::linux() would take ~5.5 MB).
    pub const fn kernel() -> RingConfig {
        RingConfig {
            ach0: 128,
            ach1: 16,
            ach2: 16,
            ach3: 16,
            ch8: 16,
            ch9: 16,
            ch12: 64,
            rxq: 256,
            rpq: 128,
        }
    }
}

/// A chip that runs, with the interface added. The module states of more than a few hundred
/// bytes live on the heap: a Device by value went through four frames of the bring-up, each with
/// its own copy, and at opt-level 1 the driver thread's 64 KiB stack overflowed (Mini-PC, wlan=on).
#[derive(Debug)]
pub struct Device {
    pub run: Box<Bringup>,
    pub btc: Box<BtcState>,
    pub rfk: Box<RfkState>,
    pub rfk2: Box<Rfk2State>,
    pub chan: Box<ChanState>,
    pub txrx: Box<Txrx>,
    pub sta: Box<Sta>,
    pub beacon_filter: bool,
    /// the interface's address (the kept one when the efuse said otherwise)
    pub mac: [u8; 6],
    /// what the efuse said this time
    pub efuse_mac: [u8; 6],
}

fn at<T>(stage: Stage, r: Rtw89Result<T>) -> Result<T, UpError> {
    r.map_err(|err| UpError { stage, err })
}

// Not on the stack: Quiesce wipes from whatever thread panicked or reboots
static ZERO_CMD_BUF: [u8; CMD_BUF] = [0; CMD_BUF];

// A module state made straight for the heap in a frame of its own: built in the caller, every
// temporary of every state would share the caller's frame (opt-level 1 keeps them apart)
#[inline(never)]
fn boxed<T>(make: impl FnOnce() -> T) -> Box<T> {
    Box::new(make())
}

#[inline(never)]
fn try_boxed<T, E>(make: impl FnOnce() -> Result<T, E>) -> Result<Box<T>, E> {
    make().map(Box::new)
}

impl Device {
    /// The station's view of the module states
    pub fn chip(&mut self) -> Chip<'_> {
        Chip {
            run: &mut self.run,
            btc: &mut self.btc,
            rfk: &mut self.rfk,
            rfk2: &mut self.rfk2,
            chan: &mut self.chan,
            txrx: &mut self.txrx,
            beacon_filter: self.beacon_filter,
        }
    }

    /// The station and the chip at once
    pub fn parts(&mut self) -> (Chip<'_>, &mut Sta) {
        let Device {
            run,
            btc,
            rfk,
            rfk2,
            chan,
            txrx,
            sta,
            beacon_filter,
            ..
        } = self;
        (
            Chip {
                run,
                btc,
                rfk,
                rfk2,
                chan,
                txrx,
                beacon_filter: *beacon_filter,
            },
            sta,
        )
    }

    // Linux: rtw89_pci_probe (pci.c:4769) through rtw89_chip_info_setup (core.c:7383) and
    // rtw89_pci_basic_cfg, rtw89_core_start (core.c:6658), then mac80211's drv_add_interface
    // (rtw89_ops_add_interface). On an error the chip is stopped again (Bringup::stop: DMA off,
    // MAC off, bus master back) and the stage is named; `log` gets every module event either way.
    pub fn bring_up<B: Bus + ?Sized>(
        bus: &mut B,
        fw: &Firmware,
        cfg: &DevConfig,
        env: &mut dyn StaEnv<B>,
        log: &mut Vec<DevEvent>,
    ) -> Result<Device, UpError> {
        let mut run = boxed(Bringup::new);
        let r = Self::up(bus, fw, cfg, env, log, &mut run);
        drain_run(&mut run, log);
        match r {
            Ok(mut dev) => {
                dev.run = run;
                Ok(dev)
            }
            Err(e) => {
                if let Err(stop) = run.stop(bus) {
                    log.push(DevEvent::new(
                        Level::Warn,
                        "chip.stop_failed",
                        format!("the stop after a failed bring-up: {stop:?}"),
                    ));
                }
                drain_run(&mut run, log);
                Err(e)
            }
        }
    }

    // `run` stays the caller's (its stop after an error); the Device it gives has an empty one
    fn up<B: Bus + ?Sized>(
        bus: &mut B,
        fw: &Firmware,
        cfg: &DevConfig,
        env: &mut dyn StaEnv<B>,
        log: &mut Vec<DevEvent>,
        run: &mut Bringup,
    ) -> Result<Device, UpError> {
        at(Stage::PowerOn, run.probe_power_on(bus, &cfg.rings))?;
        if let Some(s) = run.safety
            && (s.powered_off || s.was_master)
        {
            log.push(DevEvent::new(
                Level::Warn,
                "chip.found_on",
                format!(
                    "the chip was left on (bus master {}, MAC state {}): switched off first",
                    s.was_master, s.mac_state
                ),
            ));
        }
        at(Stage::Firmware, run.partial_init(bus, fw))?;
        at(Stage::Firmware, run.fwdl_look(bus))?;
        let v = &fw.suit.ver;
        log.push(DevEvent::new(
            Level::Info,
            "fw.loaded",
            format!(
                "firmware {}.{}.{}.{} running",
                v.major, v.minor, v.sub, v.idx
            ),
        ));
        at(Stage::Efuse, run.probe_efuse(bus, fw))?;
        let efuse_mac = at(
            Stage::Efuse,
            run.phy
                .efuse
                .as_ref()
                .map(|e| e.addr)
                .ok_or(Rtw89Error::Invalid {
                    what: "efuse not parsed",
                    value: 0,
                }),
        )?;
        let mac = match cfg.keep_mac {
            Some(k) if k != efuse_mac => {
                log.push(DevEvent::new(
                    Level::Warn,
                    "mac.changed",
                    format!(
                        "the efuse gave {} this time, the first bring-up {}: keeping the first",
                        mac_str(&efuse_mac),
                        mac_str(&k)
                    ),
                ));
                k
            }
            Some(k) => k,
            None => efuse_mac,
        };
        let rfe = run.rfe;
        at(Stage::Efuse, run.probe_done(bus, rfe))?;
        at(Stage::Efuse, run.probe_end(bus))?;
        let opts = at(Stage::Mac, run.pwr_opts())?;
        at(Stage::Mac, run.core_start_mac(bus, fw, &opts))?;
        // rtw89_coex_recognize_ver at firmware load, rtw89_btc_init at probe (a fresh state per
        // bring-up: WL-2-btc.md, an init cut short leaves it refusing), btc_ntfy_poweron at stage mac
        let mut btc = at(Stage::Mac, try_boxed(|| BtcState::new(fw.suit.ver.code)))?;
        btc::power_on(&mut btc);
        // rtw89_fw_recognize_elements / rtw89_core_setup_rfe_parms / rtw89_regd_setup at probe
        let mut chan = boxed(ChanState::new);
        let (mut rfk, mut rfk2) = (boxed(RfkState::new), boxed(Rfk2State::new));
        let r = chan.load(fw, run.rfe, run.phy.aid).and_then(|()| {
            let mut hooks = StageHooks {
                btc: &mut btc,
                rfk: &mut rfk,
                rfk2: &mut rfk2,
                chan: &mut chan,
            };
            run.core_start_phy(bus, &mut hooks)?;
            // Linux takes the TX power tracking table where it takes the PHY's (fw.c elements)
            rfk2.load_txpwr_trk(fw, run.phy.aid)
        });
        drain_parts(&mut btc, &mut chan, &mut rfk, &mut rfk2, log);
        at(Stage::Phy, r)?;
        let mut dev = Device {
            run: boxed(Bringup::default),
            btc,
            rfk,
            rfk2,
            chan,
            txrx: boxed(Txrx::new),
            sta: boxed(Sta::new),
            beacon_filter: fw.has_feat_name("BEACON_FILTER"),
            mac,
            efuse_mac,
        };
        let now = env.now_ms(bus);
        sta::track::track_start(&mut dev.sta, now);
        // the interface runs on `run` (still the caller's), so a failure leaves it for the stop
        let r = (|| {
            bus.note("stage chan")?;
            let mut chip = Chip {
                run: &mut *run,
                btc: &mut dev.btc,
                rfk: &mut dev.rfk,
                rfk2: &mut dev.rfk2,
                chan: &mut dev.chan,
                txrx: &mut dev.txrx,
                beacon_filter: dev.beacon_filter,
            };
            sta::add_interface(bus, &mut chip, &mut dev.sta, env, mac)
        })();
        drain_parts(
            &mut dev.btc,
            &mut dev.chan,
            &mut dev.rfk,
            &mut dev.rfk2,
            log,
        );
        events::sta(log, dev.sta.take_events());
        at(Stage::Interface, r)?;
        Ok(dev)
    }

    /// Every module's events since the last call
    pub fn take_events(&mut self, log: &mut Vec<DevEvent>) {
        drain_run(&mut self.run, log);
        drain_parts(
            &mut self.btc,
            &mut self.chan,
            &mut self.rfk,
            &mut self.rfk2,
            log,
        );
        events::module(log, "txrx.event", self.txrx.take_events());
        events::sta(log, self.sta.take_events());
    }

    // Linux: rtw89_ops_remove_interface, then rtw89_core_stop (core.c:6727): btc radio off and
    // power off (the antenna to BT), the dropping flushes, then rtw89_hci_deinit, the power-off,
    // rtw89_hci_reset and bus master off (Bringup::stop_hci). The CH12 buffers are wiped before
    // the rings go (they hold the last SEC_CAM H2C: a key). Every step runs even when one before
    // failed; the first error is returned.
    pub fn shutdown<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        env: &mut dyn StaEnv<B>,
    ) -> Rtw89Result<()> {
        let mut first: Option<Rtw89Error> = None;
        let mut keep = |r: Rtw89Result<()>| {
            if let Err(e) = r {
                first.get_or_insert(e);
            }
        };
        keep(bus.note("stop").map_err(Into::into));
        {
            let (mut chip, sta) = self.parts();
            if sta.ap.is_some() {
                keep(sta::link::disconnect(
                    bus,
                    &mut chip,
                    sta,
                    env,
                    wifi::mlme::reason::DEAUTH_LEAVING,
                ));
            }
            keep(sta::remove_interface(bus, &mut chip, sta, env));
            sta::track::track_stop(sta);
        }
        if let Some(rings) = self.run.rings.as_mut() {
            let mut link = H2cLink {
                fw: &mut self.run.fw,
                rings,
            };
            keep(btc::radio_off(bus, &mut self.btc, &mut link));
            keep(btc::power_off(bus, &mut self.btc, &mut link));
        }
        {
            let (mut chip, sta) = self.parts();
            keep(sta::ops::hci_flush_queues(bus, &mut chip, sta));
            keep(sta::ops::mac_flush_txq(bus, sta).map(|_| ()));
        }
        keep(self.wipe_ch12(bus));
        keep(self.run.stop_hci(bus));
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Quiesce, Reload and a chip that failed: no H2C, no frame, no sleep. The CH12 buffers are
    /// wiped, then Bringup's tail: DMA off, the MAC off, bus master back to how it was found.
    pub fn power_off<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let w = self.wipe_ch12(bus);
        let s = self.run.stop_hci(bus);
        w.and(s)
    }

    /// Zeros over every CH12 buffer (V1's net test looked for keys in the WLAN DMA memory)
    pub fn wipe_ch12<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let Some(ring) = self.run.rings.as_ref().and_then(|r| r.tx("CH12")) else {
            return Ok(());
        };
        let mut first = Ok(());
        for &a in &ring.bufs {
            if let Err(e) = bus.mem_write_bytes(a, &ZERO_CMD_BUF)
                && first.is_ok()
            {
                first = Err(e.into());
            }
        }
        first
    }
}

fn drain_run(run: &mut Bringup, log: &mut Vec<DevEvent>) {
    events::module(log, "fw.event", run.fw.take_events());
    events::module(log, "mac.event", run.mac.take_events());
    events::module(log, "phy.event", run.phy.take_events());
    if let Some(tx) = run.tx.as_mut() {
        events::module(log, "tx.event", tx.take_events());
    }
}

fn drain_parts(
    btc: &mut BtcState,
    chan: &mut ChanState,
    rfk: &mut RfkState,
    rfk2: &mut Rfk2State,
    log: &mut Vec<DevEvent>,
) {
    events::module(log, "btc.event", btc.take_events());
    events::module(log, "chan.event", chan.take_events());
    events::module(log, "rfk.event", rfk.take_events());
    events::module(log, "rfk2.event", rfk2.take_events());
}

/// aa:bb:cc:dd:ee:ff
pub fn mac_str(m: &[u8; 6]) -> alloc::string::String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}
