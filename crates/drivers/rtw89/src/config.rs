//! CONFIG.TXT for the WLAN (spec/wlan.md §2.4; R8 i): `wlan_ssid=` (a name, shown), `wlan_psk=` (a
//! secret: 8-63 characters or 64 hex digits; never shown, logged, sent in an MCP answer or kept
//! outside wifi's `Passphrase`/`Secret`), `wlan_channels=` (default every 2.4 GHz channel, 1-13),
//! `wlan_scan_dwell_ms=` (default 120, V1 bringup.mjs) and `wlan_fw=` (the firmware file). The
//! passphrase becomes the PMK at once; its copies here are wiped.

use alloc::string::String;
use alloc::vec::Vec;
use rtw89_core::drv::runner::{SCAN_CHANNELS_2G, SCAN_DWELL_MS};
use rtw89_core::phy::Band;
use rtw89_core::sta::supervise::WlanCfg;
use wifi::mlme::JoinTiming;
use wifi::secret::{Passphrase, wipe};

/// Where the firmware is looked for, in order (`wlan_fw=` first): the stick's BITSOS2 tree (the
/// desk copies the file there), then Linux's place (an initrd may carry it)
pub const FW_PATHS: [&str; 2] = [
    "/boot/BITSOS2/FIRMWARE/rtw89/rtw8852b_fw-2.bin",
    "/lib/firmware/rtw89/rtw8852b_fw-2.bin",
];
/// rtw8852b_fw-2.bin is 1,554,594 bytes; a file past this is not it
pub const FW_MAX: usize = 4 << 20;

/// The scan's channels: `wlan_channels=1,6,11` (one band: all <= 14, or all 5 GHz)
pub fn channels(v: Option<&str>) -> Result<(Band, Vec<u32>), String> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok((Band::B2g, SCAN_CHANNELS_2G.to_vec()));
    };
    let mut out = Vec::new();
    for p in v.split(',') {
        let c: u32 = p
            .trim()
            .parse()
            .map_err(|_| alloc::format!("wlan_channels: {:?} is not a channel number", p.trim()))?;
        let ok = (1..=13).contains(&c) || (36..=165).contains(&c);
        if !ok {
            return Err(alloc::format!("wlan_channels: {c} is no 2.4/5 GHz channel of the DE domain"));
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    let band = if out.iter().all(|&c| c <= 14) {
        Band::B2g
    } else if out.iter().all(|&c| c > 14) {
        Band::B5g
    } else {
        return Err(String::from("wlan_channels: one band only (2.4 GHz or 5 GHz)"));
    };
    Ok((band, out))
}

/// The network from the CONFIG values (`get` is a lookup that also answers `wlan_psk`; the tests' form).
/// `Ok(None)`: no SSID configured (the chip comes up, nothing is joined). The passphrase's copies are wiped here.
pub fn network(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<WlanCfg>, String> {
    network_with(get, &mut |f| match get("wlan_psk") {
        Some(p) => {
            let mut raw = p.into_bytes();
            f(&raw);
            wipe(&mut raw);
            true
        }
        None => false,
    })
}

/// As `network`, with the passphrase from `take_psk` (the kernel: `net::take_secret("wlan_psk", ..)`, WL-4 CR 4:
/// handed out once and zeroed there), asked only when an SSID is configured.
pub fn network_with(
    get: &dyn Fn(&str) -> Option<String>,
    take_psk: &mut dyn FnMut(&mut dyn FnMut(&[u8])) -> bool,
) -> Result<Option<WlanCfg>, String> {
    let Some(ssid) = get("wlan_ssid").filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let ssid = ssid.trim().as_bytes().to_vec();
    let mut pass = None;
    take_psk(&mut |b| pass = Some(Passphrase::new(b)));
    let Some(pass) = pass else {
        return Err(String::from("wlan_ssid= is set but wlan_psk= is missing (or a bring-up earlier in this boot took it)"));
    };
    let pmk = wifi::crypto::pmk_from_config(&pass, &ssid).map_err(|e| alloc::format!("{e}"))?;
    drop(pass);
    let (band, channels) = channels(get("wlan_channels").as_deref())?;
    let dwell_ms = match get("wlan_scan_dwell_ms") {
        None => SCAN_DWELL_MS,
        Some(v) => v
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&d| (10..=1000).contains(&d))
            .ok_or_else(|| String::from("wlan_scan_dwell_ms: 10 to 1000"))?,
    };
    Ok(Some(WlanCfg { ssid, pmk, band, channels, dwell_ms, timing: JoinTiming::default() }))
}

/// The firmware paths to try, `wlan_fw=` first
pub fn fw_paths(get: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(p) = get("wlan_fw").map(|p| String::from(p.trim())).filter(|p| !p.is_empty()) {
        v.push(p);
    }
    v.extend(FW_PATHS.iter().map(|p| String::from(*p)));
    v
}

/// How long the firmware loop waits for the boot stick's mount before a missing file is a
/// warning (fs-ext4's STICK_WAIT_MS: USB and the stick's write decision settle within it), and
/// its step: the loop looks at `quiesced` before every try, so at least once per step
pub const STICK_WAIT_MS: u64 = 120_000;
pub const STICK_STEP_MS: u64 = 1_000;

/// What the firmware loop does after a try found no file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwMiss {
    /// wait up to STICK_STEP_MS for the stick's mount; `say`: log `fw.wait_stick` (Info) first
    WaitStick { say: bool },
    /// try again after `sleep_ms`; `warn`: log `fw.missing` (Warn) first
    Retry { warn: bool, sleep_ms: u64 },
}

// USB enumerates after the PCI probe (WL-9: fw.missing at 0.97 s on 24.10.): until the boot
// stick is mounted, for at most STICK_WAIT_MS, a missing file is no fault yet. `waited_ms`: the
// stick wait so far; `tries`: the tries since the stick came (or the wait ended).
pub fn fw_miss(stick_mounted: bool, waited_ms: u64, tries: u64) -> FwMiss {
    if !stick_mounted && waited_ms < STICK_WAIT_MS {
        return FwMiss::WaitStick { say: waited_ms == 0 };
    }
    // every second for 30 s, then every 10 s; a warning on the first try and every 30th
    FwMiss::Retry {
        warn: tries == 0 || tries.is_multiple_of(30),
        sleep_ms: if tries + 1 < 30 { 1_000 } else { 10_000 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    fn cfg<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| String::from(*v))
    }

    #[test]
    fn firmware_waits_for_the_stick_then_warns() {
        // no stick yet: Info once, no warning, for the whole bounded wait
        assert_eq!(fw_miss(false, 0, 0), FwMiss::WaitStick { say: true });
        let mut w = STICK_STEP_MS;
        while w < STICK_WAIT_MS {
            assert_eq!(fw_miss(false, w, 0), FwMiss::WaitStick { say: false });
            w += STICK_STEP_MS;
        }
        // the wait is over without a stick: a warning
        assert_eq!(fw_miss(false, STICK_WAIT_MS, 0), FwMiss::Retry { warn: true, sleep_ms: 1_000 });
        // the stick is there and the file is not: a warning at once, whatever the wait
        assert_eq!(fw_miss(true, 0, 0), FwMiss::Retry { warn: true, sleep_ms: 1_000 });
        assert_eq!(fw_miss(true, 5_000, 1), FwMiss::Retry { warn: false, sleep_ms: 1_000 });
        assert_eq!(fw_miss(true, 0, 29), FwMiss::Retry { warn: false, sleep_ms: 10_000 });
        assert_eq!(fw_miss(true, 0, 30), FwMiss::Retry { warn: true, sleep_ms: 10_000 });
        // quiesce is looked at before every try: a step stays short
        assert!(STICK_STEP_MS <= 1_000 && STICK_WAIT_MS.is_multiple_of(STICK_STEP_MS));
    }

    #[test]
    fn no_ssid_no_network() {
        assert!(network(&cfg(&[])).unwrap().is_none());
        assert!(network(&cfg(&[("wlan_ssid", "  ")])).unwrap().is_none());
    }

    /// WL-4 CR 4: the kernel's store hands the passphrase out once; a second bring-up says so instead of joining
    /// with nothing, and without an SSID the store is not asked at all.
    #[test]
    fn a_taken_passphrase_is_asked_once() {
        let mut store = Some(b"correct horse battery".to_vec());
        let mut asked = 0;
        let get = cfg(&[("wlan_ssid", "x")]);
        let mut take = |f: &mut dyn FnMut(&[u8])| {
            asked += 1;
            store.take().map(|v| f(&v)).is_some()
        };
        assert!(network_with(&get, &mut take).unwrap().is_some());
        let e = network_with(&get, &mut take).unwrap_err();
        assert!(e.contains("wlan_psk= is missing"), "{e}");
        assert!(network_with(&cfg(&[]), &mut take).unwrap().is_none());
        assert_eq!(asked, 2);
    }

    #[test]
    fn the_psk_becomes_the_pmk_and_never_shows() {
        let pass = "correct horse battery";
        let n = network(&cfg(&[("wlan_ssid", "FRITZ!Box 7590 XY"), ("wlan_psk", pass)]))
            .unwrap()
            .unwrap();
        let want = wifi::crypto::psk(pass.as_bytes(), b"FRITZ!Box 7590 XY").unwrap();
        assert!(n.pmk.ct_eq(&want));
        assert_eq!(n.channels, SCAN_CHANNELS_2G.to_vec());
        assert_eq!(n.dwell_ms, SCAN_DWELL_MS);
        let shown = format!("{n:?}");
        assert!(!shown.contains("horse"), "{shown}");
        for b in &want.expose()[..8] {
            assert!(!shown.contains(&format!("{b:02x}{:02x}", want.expose()[1])), "{shown}");
        }
    }

    #[test]
    fn bad_values_are_refused_without_the_value() {
        let e = network(&cfg(&[("wlan_ssid", "x"), ("wlan_psk", "short")])).unwrap_err();
        assert!(e.contains("8 to 63") && !e.contains("short"), "{e}");
        let e = network(&cfg(&[("wlan_ssid", "x")])).unwrap_err();
        assert!(e.contains("missing"), "{e}");
        let e = network(&cfg(&[("wlan_ssid", "x"), ("wlan_psk", "12345678"), ("wlan_channels", "1,36")]))
            .unwrap_err();
        assert!(e.contains("one band"), "{e}");
        assert!(channels(Some("14")).is_err() && channels(Some("x")).is_err());
        assert_eq!(channels(Some("1, 6,11,6")).unwrap(), (Band::B2g, alloc::vec![1, 6, 11]));
        assert_eq!(channels(Some("36,40")).unwrap().0, Band::B5g);
        let e = network(&cfg(&[("wlan_ssid", "x"), ("wlan_psk", "12345678"), ("wlan_scan_dwell_ms", "5")]))
            .unwrap_err();
        assert!(e.contains("dwell"), "{e}");
    }

    #[test]
    fn firmware_paths_config_first() {
        let p = fw_paths(&cfg(&[("wlan_fw", "/boot/x.bin")]));
        assert_eq!(p[0], "/boot/x.bin");
        assert_eq!(p.len(), 1 + FW_PATHS.len());
        assert_eq!(fw_paths(&cfg(&[])).len(), FW_PATHS.len());
    }
}
