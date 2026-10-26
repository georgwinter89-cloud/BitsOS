//! What the MCP `wlan` tool shows (spec/wlan.md §4 soak method: state, SSID, channel, dBm, rates,
//! counters, the last error). A copy the thread takes after each turn, so the tool never waits for
//! the chip. Holds no secret: the SSID is a name, the PMK never leaves `WlanCfg`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::phy::Band;
use crate::sta::LinkState;
use wifi::supervisor::SvState;

use super::device::mac_str;
use super::runner::{Runner, Stats, rates_100k, raw_rssi_dbm};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// off, chip_down, down, idle, waiting, joining, connected
    pub state: &'static str,
    /// the configured network's name
    pub ssid: Option<String>,
    pub bssid: Option<[u8; 6]>,
    pub channel: Option<u32>,
    pub band_5g: bool,
    pub signal_dbm: Option<i32>,
    /// the AP's supported rates (100 kbit/s)
    pub rates_100k: Vec<u32>,
    /// rx_hw_rate of the AP's last frame (rtw89's RTW89_HW_RATE code)
    pub rx_hw_rate: Option<u32>,
    pub mac: Option<[u8; 6]>,
    pub fw: String,
    /// the supervisor's state and its next join (ms of the monotonic clock)
    pub supervisor: String,
    pub stats: Stats,
    pub last_error: Option<String>,
}

impl Status {
    pub fn of(r: &Runner) -> Status {
        let v = &r.firmware().suit.ver;
        let mut s = Status {
            fw: format!("{}.{}.{}.{}", v.major, v.minor, v.sub, v.idx),
            ssid: r
                .wlan
                .as_ref()
                .map(|w| String::from_utf8_lossy(&w.cfg.ssid).into_owned()),
            stats: r.stats,
            last_error: r.last_error.clone(),
            mac: r.cfg.keep_mac,
            ..Status::default()
        };
        s.supervisor = match r.wlan.as_ref().map(|w| (w.sup.state(), w.sup.attempt())) {
            None => String::from("no network configured"),
            Some((SvState::Off, _)) => String::from("off"),
            Some((SvState::Up, _)) => String::from("up"),
            Some((SvState::Joining, n)) => format!("joining (attempt {n})"),
            Some((SvState::Waiting { until }, n)) => {
                format!("next join at {until} ms (attempt {})", n + 1)
            }
        };
        let Some(dev) = r.dev.as_ref() else {
            s.state = if r.up_at.is_some() {
                "chip_down"
            } else {
                "off"
            };
            return s;
        };
        let sta = &dev.sta;
        s.state = match sta.state {
            LinkState::Down => "down",
            LinkState::Connected if r.link.is_some() => "connected",
            LinkState::Connected | LinkState::Auth | LinkState::Assoc | LinkState::Handshake => {
                "joining"
            }
            LinkState::Up => match r.wlan.as_ref().map(|w| w.sup.state()) {
                Some(SvState::Waiting { .. }) => "waiting",
                Some(SvState::Joining) => "joining",
                _ => "idle",
            },
        };
        if let Some(c) = sta.chan.as_ref() {
            s.channel = Some(c.ch);
            s.band_5g = c.band != Band::B2g;
        }
        if let Some(ap) = sta.ap.as_ref() {
            s.bssid = Some(ap.addr);
            let raw = ap.avg_rssi.read();
            s.signal_dbm = (raw != 0).then(|| raw_rssi_dbm(raw));
            let band = sta.chan.as_ref().map_or(Band::B2g, |c| c.band);
            s.rates_100k = rates_100k(band, ap.supp_rates);
            s.rx_hw_rate = Some(ap.rx_hw_rate);
        }
        s
    }

    /// The tool's text: one `key=value` line per part (ip from the stack, `-` without)
    pub fn lines(&self, ip: &str) -> Vec<String> {
        let opt = |o: Option<String>| o.unwrap_or_else(|| String::from("-"));
        let rates = if self.rates_100k.is_empty() {
            String::from("-")
        } else {
            self.rates_100k
                .iter()
                .map(|r| {
                    if r % 10 == 0 {
                        format!("{}", r / 10)
                    } else {
                        format!("{}.{}", r / 10, r % 10)
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let st = &self.stats;
        alloc::vec![
            format!(
                "state={} ssid={} mac={} fw={}",
                self.state,
                opt(self.ssid.clone()),
                opt(self.mac.as_ref().map(mac_str)),
                self.fw
            ),
            format!(
                "bssid={} channel={}{} signal_dbm={} rates_mbps={} rx_hw_rate={} ip={}",
                opt(self.bssid.as_ref().map(mac_str)),
                opt(self.channel.map(|c| format!("{c}"))),
                if self.band_5g { " (5 GHz)" } else { "" },
                opt(self.signal_dbm.map(|d| format!("{d}"))),
                rates,
                opt(self.rx_hw_rate.map(|r| format!("{r:#x}"))),
                ip
            ),
            format!("supervisor={}", self.supervisor),
            format!(
                "rx_frames={} rx_bytes={} tx_frames={} tx_bytes={} tx_dropped={} rx_dropped={}",
                st.rx_frames, st.rx_bytes, st.tx_frames, st.tx_bytes, st.tx_dropped, st.rx_dropped
            ),
            format!(
                "joins={} join_failures={} link_losses={} chip_restarts={} fw_repaired={} rx_unavailable={}",
                st.joins,
                st.join_failures,
                st.link_losses,
                st.restarts,
                st.fw_repaired,
                st.rx_unavailable
            ),
            format!("last_error={}", opt(self.last_error.clone())),
        ]
    }
}
