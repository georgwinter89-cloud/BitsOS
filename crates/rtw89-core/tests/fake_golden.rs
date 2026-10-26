//! The bring-up prefix over the trace bus against V1's JS fake chip, and its
//! trace against bringup-golden.txt (both the trace the Rust side recorded and
//! the one the fake saw). Needs node and the read-only V1/WLAN trees;
//! RTW89_SKIP_FAKE=1 skips (with a line on stderr; refused under BITSOS_GATE or CI),
//! anything else missing fails.

use std::io::{BufRead, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rtw89_core::bringup::{Bringup, PciRings, PhyStageHooks, RingConfig};
use rtw89_core::btc::{self, BtcEvent, BtcState, H2cLink, ViaRfk};
use rtw89_core::bus::Bus;
use rtw89_core::chan::{self, ChanEvent, ChanLink, ChanReq, ChanState};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::H2c;
use rtw89_core::fw::{Firmware, FwState};
use rtw89_core::phy::{Band, Chan};
use rtw89_core::phy::{ChipOps, Phy};
use rtw89_core::rfk1::{self, MacLink, RfkHooks, RfkState};
use rtw89_core::rfk2::{Rfk2Link, Rfk2State};
use rtw89_core::tables::phy::EF_MAC_ADDR_E;
use rtw89_core::trace::{self, compare_prefix};
use rtw89_core::tracebus::TraceBus;

/// bringup-golden.txt lines 1-321: stage fw from the safety check to the end of
/// the probe's power-on (rtw89_mac_pwr_on with the secure-boot efuse bytes)
const PREFIX_LINES: usize = 321;
/// lines 322-459: rtw89_mac_partial_init up to the firmware download (hci dma,
/// dmac pre-init with DLE and HFC, the PCIe pre-init with its rings); WL-2 pci/dmac
const PARTIAL_LINES: usize = 459;
/// lines 460-1149: the firmware download (rtw89_fw_download: CPU off/on, the header H2C,
/// 164 pieces on CH12, the status), then the not-Linux FWDL status read and look at RXQ; WL-2 fw
const FW_LINES: usize = 1149;
/// lines 1150-6656: the efuse dumps (autoload, physical, DAV, phycap) and the firmware's PHY
/// capabilities (WL-2 phy); 6657-6698 the probe's end (pre-deinit, power off, PCIe basic
/// config, rfkill); 6699-7810 stage mac (rtw89_core_start: power on, rtw89_mac_init with the
/// second download, the log read); WL-2 macinit
const PHY_EFUSE_END: usize = 6656;
const MAC_LINES: usize = 7810;
/// lines 7811-15605: stage phy (WL-2 phy, with the start's tail of macinit; rfk1's rfk_init
/// 14374-15499 with rfk2's dpk_init 14374-14377 and macinit's sch TX stop/resume; chan's
/// init_txpwr_unit, set_txpwr_ctrl, power_trim (writes nothing) and cfg_txrx_path, WL-2 chan;
/// btc's init with its H2Cs on CH12, inside rfk_init btc's four RFK notifications (DACK
/// start/stop, RX DCK start/stop) and btc's radio-on in the core start's tail, WL-2 btc). All in
/// Rust, nothing replayed; the ranges (1-based, inclusive) are the golden lines each module's
/// part must produce.
const PHY_LINES: usize = 15605;
const TXPWR_UNIT: (usize, usize) = (8847, 8852);
const BTC_INIT: (usize, usize) = (12635, 12810);
const RFK_INIT: (usize, usize) = (14374, 15499);
/// rfk2's dpk_init (Rust, not replayed): R_DPD_BF read twice, the backoff moved to the BB
const DPK_INIT: (usize, usize) = (14374, 14377);
const BTC_RFK: [(usize, usize); 4] = [
    (14452, 14473),
    (15325, 15346),
    (15347, 15368),
    (15478, 15499),
];
/// macinit's rtw89_mac_stop_sch_tx and _resume_sch_tx inside rfk_init's RX DCK (Rust, not replayed)
const SCH_STOP: (usize, usize) = (15369, 15384);
const SCH_RESUME: (usize, usize) = (15463, 15477);
const TXPWR_CTRL: (usize, usize) = (15500, 15504);
const TXRX_PATH: (usize, usize) = (15505, 15539);
const RADIO_ON: (usize, usize) = (15545, 15601);

fn firmware() -> Vec<u8> {
    let p = wlan_dir().join("firmware/rtw8852b_fw-2.bin");
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The MAC address in the kit's dumped logical efuse map (the chip's own, so it stays out of the source)
fn efuse_log_mac() -> Vec<u8> {
    let p = wlan_dir().join("kit/efuse-log.bin");
    let log = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    log[EF_MAC_ADDR_E..EF_MAC_ADDR_E + 6].to_vec()
}

fn wlan_dir() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into())
}

/// The line goes to the real stderr, past the test harness's capture, so a skip is always seen
fn skip(test: &str) -> bool {
    use std::io::Write;
    if std::env::var_os("RTW89_SKIP_FAKE").is_some_and(|v| v == "1") {
        // a gate run never skips the JS oracle: refuse the variable there
        for gate in ["BITSOS_GATE", "CI"] {
            assert!(
                std::env::var_os(gate).is_none_or(|v| v.is_empty() || v == "0"),
                "RTW89_SKIP_FAKE=1 together with {gate}: the gate must run the JS oracle"
            );
        }
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS fake chip)"
        );
        return true;
    }
    false
}

fn golden() -> Vec<String> {
    let p = wlan_dir().join("bringup-golden.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// The fake's traceText without the index in front of op lines (wlan-fake.mjs `normalised`, no RX polls here)
fn strip_index(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            if l.starts_with('#') {
                l.to_string()
            } else {
                l.split_once(' ').map_or(l, |(_, r)| r).to_string()
            }
        })
        .collect()
}

struct Fake {
    child: Child,
    bus: TraceBus<UnixStream>,
    sock: PathBuf,
    /// the fake host's stdout: answers to `ask`, then its trace at the end
    out: std::io::BufReader<std::process::ChildStdout>,
}

/// Starts fake-host.mjs against a fresh socket in the target dir and accepts its connection
fn start(name: &str, flags: &[&str]) -> Fake {
    let sock = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("wl-{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind the fake's socket");
    listener.set_nonblocking(true).unwrap();
    let host = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/fake-host.mjs");
    let mut child = Command::new("node")
        .arg(&host)
        .arg(&sock)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    let end = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(_) if Instant::now() < end => {
                if let Ok(Some(st)) = child.try_wait() {
                    panic!("fake-host.mjs ended before connecting: {st}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                panic!("fake-host.mjs did not connect: {e}");
            }
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let out = std::io::BufReader::new(child.stdout.take().unwrap());
    Fake {
        child,
        bus: TraceBus::new(stream),
        sock,
        out,
    }
}

impl Fake {
    /// Waits for the fake to handle everything, ends it, returns what it printed
    fn finish(mut self) -> String {
        self.bus.sync().expect("sync with the fake chip");
        let mut stdin = self.child.stdin.take().unwrap();
        stdin.write_all(b"done\n").unwrap();
        drop(stdin);
        let mut out = String::new();
        while self.out.read_line(&mut out).unwrap() > 0 {}
        let st = self.child.wait().unwrap();
        assert!(st.success(), "fake-host.mjs: {st}");
        out
    }

    /// One question to the fake host (`--sta`: its station side of the handshake), one line back
    fn ask(&mut self, q: &str) -> String {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(q.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let mut a = String::new();
        self.out.read_line(&mut a).unwrap();
        a.trim_end().to_string()
    }
}

impl Drop for Fake {
    /// A failed test still ends its own node process (by its handle) and removes its socket
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// Runs even with RTW89_SKIP_FAKE=1: a missing oracle always fails
#[test]
fn golden_file_is_there() {
    let gold = golden();
    assert!(
        gold.len() >= PREFIX_LINES,
        "bringup-golden.txt: {} lines",
        gold.len()
    );
    assert_eq!(gold[0], "# stage fw");
    assert_eq!(
        gold[PREFIX_LINES - 1],
        "wr8 0x000000AF 0x00000000 0x00000081",
        "the prefix ends with the scoreboard"
    );
    assert_eq!(
        gold[PARTIAL_LINES - 1],
        "rmw32 0x00001000 0x00000000 0x00002800",
        "the PCIe pre-init ends with TX/RX HCI DMA on"
    );
    assert_eq!(
        gold[FW_LINES - 6],
        "poll8 0x000001E0 0x000000E0 0x000000E0 400 ms",
        "the download ends with rtw89_fw_check_rdy"
    );
    assert_eq!(gold[FW_LINES - 1], "# Linux again", "then the look at RXQ");
    assert_eq!(
        gold[PHY_EFUSE_END], "rmw32 0x00000074 0x00000020 0x00000000",
        "the probe's end starts with the power-wake clear"
    );
    assert_eq!(gold[6698], "# stage mac");
    assert_eq!(
        gold[MAC_LINES - 2],
        "rd32 0x00008400 0x00000000 0x00000000",
        "the log read"
    );
    assert_eq!(gold[MAC_LINES], "# stage phy");
    assert_eq!(
        gold[FW_LINES], "rd16 0x0000000A 0x00000000 0x00000000",
        "the autoload bit"
    );
    assert_eq!(
        gold[TXPWR_UNIT.0 - 2],
        "wr32 0x000100F8 0x00000000 0x20220408",
        "the BB table's last write before init_txpwr_unit"
    );
    assert_eq!(
        gold[RADIO_ON.1], "mwr32 0x00629B00 0x00000000 0x4000002C",
        "the fw log H2C"
    );
    assert_eq!(gold[PHY_LINES], "# stage chan");
    for (a, b) in [
        TXPWR_UNIT,
        BTC_INIT,
        RFK_INIT,
        TXPWR_CTRL,
        TXRX_PATH,
        RADIO_ON,
        CHAN_VIF,
        CHAN_BTC_BAND,
    ] {
        assert!(
            gold[a - 1..b]
                .iter()
                .all(|l| trace::parse_line(l).is_some()),
            "module lines {a}-{b} are ops"
        );
    }
    // btc's init: init_cfg from the GPIO mux on, the scoreboard last; its radio-on: init_cfg only
    for (a, b) in [BTC_INIT, RADIO_ON] {
        assert_eq!(gold[a - 1], "rmw8 0x00000040 0x00000000 0x00000020");
        assert!(gold[a..b].iter().all(|l| !l.starts_with('#')));
    }
    assert_eq!(
        gold[BTC_INIT.1 - 1],
        "delay 0x000003E8 0x00000000 0x00000000"
    );
    assert_eq!(
        gold[RADIO_ON.1 - 1],
        "rmw32 0x0000DA40 0x00000000 0x00010004"
    );
    // inside rfk_init: rfk2's dpk_init (R_DPD_BF reads, the backoff writes), btc's RFK
    // notifications (scoreboard and coex run, ending with the 1 ms wait), macinit's sch TX
    assert_eq!(
        gold[DPK_INIT.0 - 1],
        "rd32 0x000144A0 0x00000000 0x00000000"
    );
    assert_eq!(
        gold[DPK_INIT.1 - 1],
        "rmw32 0x000182BC 0x007FFFFF 0x007F7F7F",
        "dpk_init ends with path B's DPD backoff"
    );
    // stage chan: chan's set_channel with rfk1's RF channel set, btc's band switch and rfk2's
    // TSSI scan (Rust), between replayed parts of sta
    assert!(PHY_LINES < CHAN_VIF.0 && CHAN_VIF.1 + 1 == CHAN_SWITCH.0);
    assert!(CHAN_STOP.0 == CHAN_SWITCH.0 && CHAN_RF.1 + 1 == CHAN_TXPWR.0);
    assert!(CHAN_RESUME.1 + 1 == CHAN_BTC_BAND.0 && CHAN_BTC_BAND.1 + 1 == CHAN_TSSI.0);
    assert!(CHAN_TSSI.1 == CHAN_SWITCH.1 && CHAN_SWITCH.1 < CHAN_LINES);
    assert_eq!(
        gold[CHAN_TSSI.0 - 1],
        "rmw32 0x00015820 0x80000000 0x00000000"
    );
    assert_eq!(
        gold[CHAN_TSSI.1 - 1],
        "rmw32 0x00017830 0x003FF000 0x00008000"
    );
    // the stages after chan (sta.rs): their notes and where the stop's tail starts
    assert_eq!(gold.len(), sta::ALL_LINES);
    for (l, want) in [
        (sta::SCAN, "# stage scan"),
        (sta::ASSOC, "# stage assoc"),
        (sta::DHCP, "# stage dhcp"),
        (sta::STOP, "# stop"),
    ] {
        assert_eq!(gold[l - 1], want);
    }
    assert_eq!(
        gold[sta::STOP_HCI - 1],
        "rmw32 0x00000074 0x00000020 0x00000000"
    );
    for (a, b) in BTC_RFK {
        assert!(RFK_INIT.0 <= a && b <= RFK_INIT.1);
        assert_eq!(gold[a - 1], "poll8 0x0000DAF3 0x00000020 0x00000020 50 ms");
        assert_eq!(gold[b - 1], "delay 0x000003E8 0x00000000 0x00000000");
    }
    assert_eq!(
        gold[SCH_STOP.0 - 1],
        "rd16 0x0000C348 0x00000000 0x00000000"
    );
    assert_eq!(
        gold[SCH_RESUME.1 - 1],
        "rmw8 0x000001F5 0x000000F0 0x00000020"
    );
}

#[test]
fn fw_download_trace_is_golden() {
    if skip("fw_download_trace_is_golden") {
        return;
    }
    let gold = golden();
    let mut fake = start("fwdl", &[]);
    let mut run = Bringup::new();
    run.probe_power_on(&mut fake.bus, &RingConfig::js())
        .expect("the bring-up prefix on the fake chip");
    let fw = Firmware::parse(
        firmware(),
        run.chip.unwrap().cv,
        run.power.unwrap().secure_boot.unwrap_or(false),
    )
    .expect("rtw89_fw_recognize");
    run.partial_init(&mut fake.bus, &fw)
        .expect("the partial init with the firmware download");
    run.fwdl_look(&mut fake.bus).expect("the look after it");

    let ours = fake.bus.lines();
    assert_eq!(ours.len(), FW_LINES, "trace length");
    if let Err(m) = compare_prefix(&ours, &gold) {
        panic!(
            "Rust trace differs from bringup-golden.txt at line {}: ours {:?}, golden {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert!(
        run.fw.ready && run.fw.tries == 1 && run.fw.pieces == 164,
        "{:?}",
        run.fw
    );
    let look = run.look.as_ref().expect("the look");
    assert_eq!(
        look.status, 7,
        "FWDL status 7 (WCPU_FW_INIT_RDY) from the fake"
    );

    let seen = strip_index(&fake.finish());
    assert_eq!(seen.len(), FW_LINES, "the fake saw as many lines");
    if let Err(m) = compare_prefix(&seen, &gold) {
        panic!(
            "the fake's trace differs at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
}

#[test]
fn mac_stage_trace_is_golden() {
    if skip("mac_stage_trace_is_golden") {
        return;
    }
    let gold = golden();
    let mut fake = start("mac", &[]);
    let mut run = Bringup::new();
    run.probe_power_on(&mut fake.bus, &RingConfig::js())
        .expect("the bring-up prefix on the fake chip");
    let fw = Firmware::parse(
        firmware(),
        run.chip.unwrap().cv,
        run.power.unwrap().secure_boot.unwrap_or(false),
    )
    .expect("rtw89_fw_recognize");
    run.partial_init(&mut fake.bus, &fw)
        .expect("the partial init with the firmware download");
    run.fwdl_look(&mut fake.bus).expect("the look after it");
    run.probe_efuse(&mut fake.bus, &fw)
        .expect("the efuse dumps and the firmware's PHY capabilities");
    // the efuse the fake chip's dumps give: rfe 1, autoloaded, no power K
    let opts = run.pwr_opts().expect("the efuse's power-on options");
    assert_eq!(
        (opts.rfe, opts.efuse_valid, opts.power_k_valid, opts.cv),
        (1, true, false, 1)
    );
    assert_eq!(run.rfe, 1, "Bringup::rfe from the parsed efuse");
    let ef = run.phy.efuse.as_ref().unwrap();
    assert_eq!(
        (&ef.addr[..], ef.xtal_cap, ef.tssi.thermal),
        (&efuse_log_mac()[..], 0x3c, [0x1a, 0x18])
    );
    let cap = run.phycap.unwrap();
    assert_eq!(
        (cap.tx_nss, cap.rx_nss, cap.tx_ant, cap.rx_ant),
        (2, 2, 2, 2),
        "the fake firmware's GET_FEATURE answer"
    );
    assert_eq!(
        fake.bus.lines().len(),
        PHY_EFUSE_END,
        "the efuse part ends where the golden file's does"
    );
    run.probe_done(&mut fake.bus, run.rfe)
        .expect("the probe's power-off");
    run.probe_end(&mut fake.bus)
        .expect("PCIe basic config and rfkill");
    run.core_start_mac(&mut fake.bus, &fw, &opts)
        .expect("stage mac on the fake chip");

    let ours = fake.bus.lines();
    if let Err(m) = compare_prefix(&ours, &gold) {
        panic!(
            "Rust trace differs from bringup-golden.txt at line {}: ours {:?}, golden {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert_eq!(ours.len(), MAC_LINES, "trace length");
    assert!(run.fw.ready && run.fw.tries == 1 && run.fw.pieces == 164);
    assert_eq!(run.mac.power_on, Some(true));
    assert!(run.mac.events().is_empty(), "{:?}", run.mac.events());
    assert_eq!(
        run.dmac_func_en.map(|v| v & 0xffff_0000),
        Some(0xfffd_0000),
        "DMAC_FUNC_EN after rtw89_mac_init (NOTES-mac.md)"
    );

    let seen = strip_index(&fake.finish());
    if let Err(m) = compare_prefix(&seen, &gold) {
        panic!(
            "the fake's trace differs at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert_eq!(seen.len(), MAC_LINES, "the fake saw as many lines");
}

#[test]
fn partial_init_trace_is_golden() {
    if skip("partial_init_trace_is_golden") {
        return;
    }
    let gold = golden();
    let mut fake = start("partial", &[]);
    let mut run = Bringup::new();
    run.probe_power_on(&mut fake.bus, &RingConfig::js())
        .expect("the bring-up prefix on the fake chip");
    run.partial_init_to_fwdl(&mut fake.bus)
        .expect("the partial init up to the firmware download");

    let ours = fake.bus.lines();
    assert_eq!(ours.len(), PARTIAL_LINES, "trace length");
    if let Err(m) = compare_prefix(&ours, &gold) {
        panic!(
            "Rust trace differs from bringup-golden.txt at line {}: ours {:?}, golden {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    let dle = run.mac.dmac.dle_info.expect("DLE info");
    assert_eq!(dle.qta_mode, rtw89_core::pci::QtaMode::Dlfw);
    assert!(
        run.mac.dmac.hfc.h2c_en && !run.mac.dmac.hfc.en,
        "only the H2C channel flows"
    );
    assert!(run.pci, "the stop knows the PCIe DMA runs");

    let seen = strip_index(&fake.finish());
    assert_eq!(seen.len(), PARTIAL_LINES, "the fake saw as many lines");
    if let Err(m) = compare_prefix(&seen, &gold) {
        panic!(
            "the fake's trace differs at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
}

#[test]
fn prefix_trace_is_golden() {
    if skip("prefix_trace_is_golden") {
        return;
    }
    let gold = golden();
    let mut fake = start("prefix", &[]);
    let mut run = Bringup::new();
    run.probe_power_on(&mut fake.bus, &RingConfig::js())
        .expect("the bring-up prefix on the fake chip");

    let ours = fake.bus.lines();
    assert_eq!(ours.len(), PREFIX_LINES, "trace length");
    if let Err(m) = compare_prefix(&ours, &gold) {
        panic!(
            "Rust trace differs from bringup-golden.txt at line {}: ours {:?}, golden {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    let chip = run.chip.unwrap();
    assert_eq!((chip.cv, chip.acv), (1, 1), "the fake chip's cuts");
    assert_eq!(
        run.power.unwrap().secure_boot,
        Some(false),
        "efuse 0x5EC/0x5ED read FF/FF"
    );
    let rings = run.rings.as_ref().unwrap();
    assert_eq!(
        rings.tx("ACH0").unwrap().bd,
        0x0061_0000,
        "DMA from 0x610000 in the JS order"
    );
    assert_eq!(rings.rx("RXQ").unwrap().bd, 0x0063_1B80);
    assert_eq!(rings.rx("RXQ").unwrap().bufs[0], 0x0063_1D80);

    let seen = strip_index(&fake.finish());
    assert_eq!(seen.len(), PREFIX_LINES, "the fake saw as many lines");
    if let Err(m) = compare_prefix(&seen, &gold) {
        panic!(
            "the fake's trace differs at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
}

#[test]
fn stop_powers_off_and_clears_bus_master() {
    if skip("stop_powers_off_and_clears_bus_master") {
        return;
    }
    let mut fake = start("stop", &[]);
    let mut run = Bringup::new();
    run.probe_power_on(&mut fake.bus, &RingConfig::js())
        .unwrap();
    run.stop(&mut fake.bus).expect("stop");
    let ours = fake.bus.lines();
    let tail = &ours[PREFIX_LINES..];
    assert_eq!(tail[0], "# stop");
    assert!(
        tail.iter()
            .any(|l| l == "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        "APFM_OFFMAC poll of the power-off"
    );
    let n = tail.len();
    assert_eq!(tail[n - 4], "# not Linux: PCI command back, bus master off");
    assert_eq!(
        tail[n - 3],
        "cfgw 0x00000004 0x00000000 0x00000002",
        "bus master off, memory space kept"
    );
    assert_eq!(tail[n - 1], "# Linux again");
    let seen = strip_index(&fake.finish());
    assert_eq!(seen, ours, "the fake saw what the Rust side sent");
}

#[test]
fn warm_start_powers_the_mac_off_first_as_the_js_driver() {
    if skip("warm_start_powers_the_mac_off_first_as_the_js_driver") {
        return;
    }
    let mut fake = start("warm", &["--warm"]);
    let s = rtw89_core::bringup::safety::bus_master_on(&mut fake.bus).expect("safety step");
    assert_eq!(s.mac_state, 1, "the fake said MAC on");
    let ours = fake.bus.lines();
    let out = fake.finish();
    let (seen, js) = out.split_once("=== js ===\n").expect("the JS reference");
    assert_eq!(strip_index(seen), ours);
    let js = strip_index(js);
    assert!(js.len() > 20, "the JS power-off ran: {js:?}");
    if let Err(m) = compare_prefix(&ours, &js) {
        panic!(
            "warm safety step differs from the JS driver at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert_eq!(ours.len(), js.len());
}

/// The modules stage phy calls, all in Rust now: chan's chip ops, rfk1/rfk2's rfk_init and btc's
/// init and radio-on, each noted with the golden line it starts at
struct GoldenHooks {
    calls: Vec<&'static str>,
    /// btc's state (WL-2 btc), powered on at stage mac; (name, first golden line) of its parts
    btc: BtcState,
    btc_at: Vec<(&'static str, usize)>,
    rfk: RfkState,
    rfk2: Rfk2State,
    rfk_calls: Vec<String>,
    /// chan's state (WL-2 chan): its chip ops run in Rust; (name, first golden line) of each
    chan: ChanState,
    chan_at: Vec<(&'static str, usize)>,
}

/// The sta parts replayed from the golden file sent H2Cs on CH12: the ring's host index and the
/// H2C sequence follow them, as if the Rust side had sent them
fn follow_replayed_h2cs(
    gold: &[String],
    (a, b): (usize, usize),
    rings: &mut PciRings,
    fws: &mut FwState,
) {
    let ch12 = rings.tx_mut("CH12").expect("CH12");
    let kick = format!("wr16 0x{:08X} ", ch12.idx);
    let kicks: Vec<&String> = gold[a - 1..b]
        .iter()
        .filter(|l| l.starts_with(&kick))
        .collect();
    if let Some(last) = kicks.last() {
        let last = trace::parse_line(last).unwrap().c;
        ch12.wp = last;
        ch12.rp = last;
    }
    fws.h2c_seq = fws.h2c_seq.wrapping_add(kicks.len() as u8);
}

fn replay_lines(
    gold: &[String],
    bus: &mut TraceBus<UnixStream>,
    (a, b): (usize, usize),
) -> Result<(), Rtw89Error> {
    for l in &gold[a - 1..b] {
        // a stage's `# ` line goes to the fake as a note, as the JS bring-up wrote it
        if let Some(text) = l.strip_prefix("# ") {
            bus.note(text)?;
            continue;
        }
        let op = trace::parse_line(l).unwrap_or_else(|| panic!("not an op: {l}"));
        trace::replay(bus, &op)?;
    }
    Ok(())
}

/// rfk1's neighbours during rfk_init: btc's RFK notifications (WL-2 btc, `None`: none may come)
/// and macinit's sch TX stop/resume run for real through the MAC side Bringup hands over, as do
/// the H2Cs; rfk2's entries run in Rust through `Rfk2Link` around this
struct GoldenRfk<'m> {
    btc: Option<&'m mut BtcState>,
    mac: MacLink<'m>,
    calls: Vec<String>,
}

impl RfkHooks<TraceBus<UnixStream>> for GoldenRfk<'_> {
    fn btc_wl_rfk(
        &mut self,
        b: &mut TraceBus<UnixStream>,
        phy_map: u32,
        typ: u32,
        state: u32,
    ) -> Result<(), Rtw89Error> {
        self.calls.push(format!(
            "btc_wl_rfk {phy_map:#x} {typ} {state} at {}",
            b.lines().len() + 1
        ));
        let st = self.btc.as_deref_mut().expect("no btc notification here");
        btc::role::wl_rfk(b, st, &mut self.mac, phy_map, typ, state)?;
        Ok(())
    }
    fn btc_conn_rfk(&mut self, _: &mut TraceBus<UnixStream>, _: bool) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
    fn btc_preserve_bt_time(
        &mut self,
        _: &mut TraceBus<UnixStream>,
        _: u32,
    ) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
    fn stop_sch_tx(&mut self, b: &mut TraceBus<UnixStream>) -> Result<u16, Rtw89Error> {
        self.calls
            .push(format!("stop_sch_tx at {}", b.lines().len() + 1));
        self.mac.stop_sch_tx(b)
    }
    fn resume_sch_tx(
        &mut self,
        b: &mut TraceBus<UnixStream>,
        tx_en: u16,
    ) -> Result<(), Rtw89Error> {
        self.calls.push(format!(
            "resume_sch_tx {tx_en:#x} at {}",
            b.lines().len() + 1
        ));
        self.mac.resume_sch_tx(b, tx_en)
    }
    fn h2c(&mut self, b: &mut TraceBus<UnixStream>, h: &H2c, p: &[u8]) -> Result<(), Rtw89Error> {
        self.mac.h2c(b, h, p) // btc's band switch inside the channel switch (ViaRfk)
    }
    fn dpk_init(&mut self, _: &mut TraceBus<UnixStream>, _: &mut Phy) -> Result<(), Rtw89Error> {
        unreachable!("rfk2's own, through Rfk2Link")
    }
    fn dpk(
        &mut self,
        _: &mut TraceBus<UnixStream>,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
    ) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
    fn dpk_track(&mut self, _: &mut TraceBus<UnixStream>, _: &mut Phy) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
    fn tssi(
        &mut self,
        _: &mut TraceBus<UnixStream>,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
        _: bool,
    ) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
    fn tssi_scan(
        &mut self,
        _: &mut TraceBus<UnixStream>,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
    ) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
    fn wifi_scan_notify(
        &mut self,
        _: &mut TraceBus<UnixStream>,
        _: &mut Phy,
        _: bool,
        _: u32,
    ) -> Result<(), Rtw89Error> {
        unreachable!("not in rfk_init")
    }
}

impl ChipOps<TraceBus<UnixStream>> for GoldenHooks {
    fn init_txpwr_unit(&mut self, b: &mut TraceBus<UnixStream>, _: &Phy) -> Result<(), Rtw89Error> {
        self.calls.push("init_txpwr_unit");
        self.chan_at.push(("init_txpwr_unit", b.lines().len() + 1));
        chan::init_txpwr_unit(b, &mut self.chan)
    }
    fn rfk_init(
        &mut self,
        b: &mut TraceBus<UnixStream>,
        phy: &mut Phy,
        mac: Option<MacLink<'_>>,
    ) -> Result<(), Rtw89Error> {
        self.calls.push("rfk_init");
        let mut hooks = GoldenRfk {
            btc: Some(&mut self.btc),
            mac: mac.expect("Bringup hands the MAC side to rfk_init"),
            calls: Vec::new(),
        };
        let at = b.lines().len() + 1;
        let mut link = Rfk2Link {
            st: &mut self.rfk2,
            hooks: &mut hooks,
            chan: None,
        };
        rfk1::rfk_init(b, phy, &mut self.rfk, None, &mut link)?;
        hooks.calls.insert(0, format!("dpk_init at {at}"));
        self.rfk_calls = hooks.calls;
        Ok(())
    }
    fn set_txpwr_ctrl(&mut self, b: &mut TraceBus<UnixStream>, _: &Phy) -> Result<(), Rtw89Error> {
        self.calls.push("set_txpwr_ctrl");
        self.chan_at.push(("set_txpwr_ctrl", b.lines().len() + 1));
        chan::set_txpwr_ctrl(b, &mut self.chan)
    }
    fn power_trim(&mut self, b: &mut TraceBus<UnixStream>, phy: &Phy) -> Result<(), Rtw89Error> {
        self.calls.push("power_trim"); // no trim programmed (phycap all 0xff): no register
        self.chan_at.push(("power_trim", b.lines().len() + 1));
        chan::power_trim(b, phy)
    }
    fn cfg_txrx_path(&mut self, b: &mut TraceBus<UnixStream>, phy: &Phy) -> Result<(), Rtw89Error> {
        self.calls.push("cfg_txrx_path");
        self.chan_at.push(("cfg_txrx_path", b.lines().len() + 1));
        chan::cfg_txrx_path(b, phy, &mut self.chan)
    }
}

impl PhyStageHooks<TraceBus<UnixStream>> for GoldenHooks {
    fn btc_init(
        &mut self,
        b: &mut TraceBus<UnixStream>,
        rings: &mut PciRings,
        fws: &mut FwState,
        phy: &Phy,
    ) -> Result<(), Rtw89Error> {
        self.calls.push("btc_init");
        self.btc_at.push(("btc_init", b.lines().len() + 1));
        btc::core_start_init(b, &mut self.btc, rings, fws, phy)
    }
    fn track_start(&mut self) {
        self.calls.push("track_start");
    }
    fn radio_on(
        &mut self,
        b: &mut TraceBus<UnixStream>,
        rings: &mut PciRings,
        fws: &mut FwState,
    ) -> Result<(), Rtw89Error> {
        self.calls.push("radio_on");
        self.btc_at.push(("radio_on", b.lines().len() + 1));
        btc::radio_on(b, &mut self.btc, &mut H2cLink { fw: fws, rings })
    }
}

/// The bring-up through stage phy on V1's fake chip (stage fw and mac in Rust, stage phy with
/// the unported chip ops replayed)
fn run_to_phy_stage(name: &str) -> (Fake, Bringup, GoldenHooks) {
    run_to_phy_stage_with(name, &[])
}

fn run_to_phy_stage_with(name: &str, flags: &[&str]) -> (Fake, Bringup, GoldenHooks) {
    let mut fake = start(name, flags);
    let mut run = Bringup::new();
    run.probe_power_on(&mut fake.bus, &RingConfig::js())
        .expect("the bring-up prefix on the fake chip");
    let fw = Firmware::parse(firmware(), 1, false).expect("rtw89_fw_recognize");
    run.partial_init(&mut fake.bus, &fw).expect("partial init");
    run.fwdl_look(&mut fake.bus).expect("the look");
    run.probe_efuse(&mut fake.bus, &fw)
        .expect("efuse and phycap");
    run.probe_done(&mut fake.bus, run.rfe)
        .expect("probe's power-off");
    run.probe_end(&mut fake.bus)
        .expect("PCIe basic config and rfkill");
    let opts = run.pwr_opts().unwrap();
    run.core_start_mac(&mut fake.bus, &fw, &opts)
        .expect("stage mac");
    let rfe = run.rfe;
    // rtw89_coex_recognize_ver at firmware load, rtw89_btc_init at probe, rtw89_btc_ntfy_poweron
    // at stage mac
    let mut bst = BtcState::new(fw.suit.ver.code).expect("the 0.29.29 coex layout");
    btc::power_on(&mut bst);
    let mut hooks = GoldenHooks {
        calls: Vec::new(),
        btc: bst,
        btc_at: Vec::new(),
        rfk: RfkState::new(),
        rfk2: Rfk2State::new(),
        rfk_calls: Vec::new(),
        chan: ChanState::new(),
        chan_at: Vec::new(),
    };
    // rtw89_fw_recognize_elements / rtw89_core_setup_rfe_parms / rtw89_regd_setup at probe
    hooks
        .chan
        .load(&fw, rfe, 0)
        .expect("chan's TX power tables");
    run.core_start_phy(&mut fake.bus, &mut hooks)
        .expect("stage phy on the fake chip");
    (fake, run, hooks)
}

#[test]
fn phy_stage_trace_is_golden() {
    if skip("phy_stage_trace_is_golden") {
        return;
    }
    let gold = golden();
    let (fake, run, hooks) = run_to_phy_stage("phy");

    let ours = fake.bus.lines();
    if let Err(m) = compare_prefix(&ours, &gold) {
        panic!(
            "Rust trace differs from bringup-golden.txt at line {}: ours {:?}, golden {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert_eq!(ours.len(), PHY_LINES, "trace length");
    assert_eq!(
        hooks.calls,
        [
            "init_txpwr_unit",
            "btc_init",
            "rfk_init",
            "set_txpwr_ctrl",
            "power_trim",
            "cfg_txrx_path",
            "track_start",
            "radio_on"
        ],
        "the chip ops in Linux's order"
    );
    let p = &run.phy;
    assert_eq!(
        p.loads.bb.as_ref().map(|l| l.applied),
        Some(1013),
        "BB_REG writes"
    );
    assert_eq!(
        p.loads
            .rf
            .iter()
            .map(|l| (l.applied, l.h2c_values, l.h2c_ok))
            .collect::<Vec<_>>(),
        [(947, 294, true), (932, 294, true)],
        "RADIO tables and their H2Cs"
    );
    assert!(p.nctl_ready, "NCTL answered");
    assert_eq!(p.cfo.crystal_cap, 0x3c, "crystal cap from the efuse");
    assert!(p.events().is_empty(), "{:?}", p.events());
    // rfk1's rfk_init in Rust (WL-2 rfk1): the neighbours in Linux's order, macinit's part at
    // its golden lines, the calibrations' results as the fake chip gives them
    assert_eq!(
        hooks.rfk_calls,
        [
            format!("dpk_init at {}", DPK_INIT.0),
            format!("btc_wl_rfk 0x10 4 1 at {}", BTC_RFK[0].0),
            format!("btc_wl_rfk 0x10 4 0 at {}", BTC_RFK[1].0),
            format!("btc_wl_rfk 0x10 5 1 at {}", BTC_RFK[2].0),
            format!("stop_sch_tx at {}", SCH_STOP.0),
            format!("resume_sch_tx 0x0 at {}", SCH_RESUME.0), // V1's fake reads CTN_TXEN as 0
            format!("btc_wl_rfk 0x10 5 0 at {}", BTC_RFK[3].0),
        ],
        "rfk_init's neighbours"
    );
    // btc in Rust (WL-2 btc) at its golden lines: the init with 24 H2Cs on CH12, the four RFK
    // notifications above, the radio-on (init_cfg; its run returns)
    assert_eq!(
        hooks.btc_at,
        [("btc_init", BTC_INIT.0), ("radio_on", RADIO_ON.0)],
        "btc's parts"
    );
    let bs = &hooks.btc;
    assert_eq!(
        bs.fwinfo.cnt_h2c, 19,
        "btc's H2Cs with done-ack (5 more without)"
    );
    assert_eq!(bs.fwinfo.rpt_en_map, 1 << 2, "only the MREG report");
    assert_eq!(bs.wl.scbd, 0x4003, "WL active, on, BT log");
    assert_eq!(bs.dm.gnt_set[0].gnt_wl, 1, "WLAN owns the antenna");
    assert!(!bs.dm.pta_owner_warn);
    assert_eq!(
        bs.events(),
        [
            BtcEvent::Action {
                reason: 1,
                action: 107
            },
            BtcEvent::Action {
                reason: 6,
                action: 112
            }
        ],
        "init: WL_INIT; the first RFK notification: BT_OFF"
    );
    let rk = &hooks.rfk;
    assert!(rk.events().is_empty(), "{:?}", rk.events());
    assert!(
        rk.rck.iter().all(|r| r.is_some_and(|r| !r.timeout)),
        "{:?}",
        rk.rck
    );
    assert!(rk.dack.dack_done && rk.dack.dack_cnt == 1);
    // rfk2's dpk_init in Rust (WL-2 rfk2): V1's fake reads an OFDM backoff + scale of 44 or more
    assert_eq!(
        hooks.rfk2.dpk.dpk_gs[0], 0x7f,
        "DPD backoff moved to the BB"
    );
    assert!(hooks.rfk2.events().is_empty(), "{:?}", hooks.rfk2.events());
    assert_eq!(rk.dack.addc.len(), 5, "five ADC DC checks");
    assert_eq!(p.tssi_mode, [false, false]);
    // chan's chip ops in Rust (WL-2 chan) at their golden lines; power_trim writes nothing
    assert_eq!(
        hooks.chan_at,
        [
            ("init_txpwr_unit", TXPWR_UNIT.0),
            ("set_txpwr_ctrl", TXPWR_CTRL.0),
            ("power_trim", TXRX_PATH.0),
            ("cfg_txrx_path", TXRX_PATH.0),
        ],
        "chan's chip ops"
    );
    assert!(hooks.chan.dm_init_done, "cfg_txrx_path ran");
    assert_eq!(
        hooks.chan.events(),
        [ChanEvent::RegdSet {
            alpha2: *b"DE",
            txpwr_regd: [1, 1, 4]
        }],
        "only the default country (ETSI on 2.4 and 5 GHz)"
    );

    let seen = strip_index(&fake.finish());
    if let Err(m) = compare_prefix(&seen, &gold) {
        panic!(
            "the fake's trace differs at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert_eq!(seen.len(), PHY_LINES, "the fake saw as many lines");
}

/// Stage chan (golden 15606-16255, the JS bring-up's add_interface with mac80211's first channel,
/// 2.4 GHz channel 1, 20 MHz): sta's vif init and the WMM/ERP set-up before the switch are
/// replayed (sta not ported; btc's role start in it writes nothing and runs in Rust), then chan's
/// __rtw89_set_channel runs in Rust from macinit's sch TX stop (H2C register path) to the
/// band-change steps: help enter, MAC, BB, rfk1's RF channel set, the TX power, help leave, btc's
/// band switch (WL-2 btc, through `ChanLink`'s hook, its policy H2C on CH12 through the
/// neighbours' MAC side) and rfk1/rfk2's TSSI scan; then sta's PHY status parsing init and
/// filter are replayed to the stage's end. From stage scan (16256) on, frames go into DMA memory
/// untraced: `sta.rs` sends them with txrx (WL-2 txrx) and gives sta's replayed H2Cs the JS
/// driver's bytes, so V1's fake answers as it did for the golden run.
const CHAN_VIF: (usize, usize) = (15607, 15700);
const CHAN_SWITCH: (usize, usize) = (15701, 16222);
/// inside the switch: macinit's sch TX stop, rfk1's RF channel set, the TX power, macinit's
/// resume, btc's band switch, rfk2's TSSI scan
const CHAN_STOP: (usize, usize) = (15701, 15716);
const CHAN_RF: (usize, usize) = (15804, 15956);
const CHAN_TXPWR: (usize, usize) = (15957, 16014);
const CHAN_RESUME: (usize, usize) = (16028, 16042);
const CHAN_BTC_BAND: (usize, usize) = (16043, 16068);
const CHAN_TSSI: (usize, usize) = (16069, 16222);
const CHAN_LINES: usize = 16255;

fn check_golden(ours: &[String], gold: &[String], what: &str) {
    if let Err(m) = compare_prefix(ours, gold) {
        panic!(
            "{what}: Rust trace differs from bringup-golden.txt at line {}: ours {:?}, golden {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
}

/// The bring-up to the end of stage chan's switch (golden 1-16222) in Rust with sta's parts
/// replayed, checked on the way; `flags` go to the fake host
fn chan_stage(gold: &[String], name: &str, flags: &[&str]) -> Fake {
    assert_eq!(gold[PHY_LINES], "# stage chan");
    assert_eq!(gold[CHAN_LINES], "# stage scan");
    assert_eq!(
        gold[CHAN_VIF.0 - 1],
        "rd32 0x0000C400 0x00000000 0x00000000"
    );
    assert_eq!(
        gold[CHAN_STOP.0 - 1],
        "rd16 0x0000C348 0x00000000 0x00000000",
        "the switch starts with macinit's sch TX stop"
    );
    assert_eq!(
        gold[CHAN_RF.0], "rmw32 0x00010378 0x000007FF 0x00000018",
        "RF 0x18 of path A first"
    );
    assert_eq!(
        gold[CHAN_RF.1 - 1],
        "wr32 0x00010370 0x00000000 0x9EE00000",
        "RTXBW of path B last"
    );
    assert_eq!(
        gold[CHAN_TXPWR.0 - 1],
        "wr32 0x0000D2C0 0x00000000 0x28282828",
        "the TX power by rate first"
    );
    assert_eq!(
        gold[CHAN_RESUME.1 - 1],
        "rmw8 0x000001F5 0x000000F0 0x00000040",
        "the help's leave ends with macinit's resume"
    );
    assert_eq!(
        gold[CHAN_BTC_BAND.0 - 1],
        "poll8 0x0000DAF3 0x00000020 0x00000020 50 ms"
    );
    assert_eq!(
        gold[CHAN_TSSI.0 - 1],
        "rmw32 0x00015820 0x80000000 0x00000000",
        "TSSI off on path A first"
    );
    assert_eq!(
        gold[CHAN_TSSI.1 - 1],
        "rmw32 0x00017830 0x003FF000 0x00008000",
        "the MCS 10 MHz DE of path B last"
    );
    let (mut fake, mut run, hooks) = run_to_phy_stage_with(name, flags);
    let mut rfk = hooks.rfk;
    let mut rfk2 = hooks.rfk2;
    let mut cs = hooks.chan;
    let mut bst = hooks.btc;
    bst.take_events();
    let fw = Firmware::parse(firmware(), 1, false).expect("rtw89_fw_recognize");
    rfk2.load_txpwr_trk(&fw, 0).expect("TXPWR_TRK element");
    cs.take_events();
    replay_lines(gold, &mut fake.bus, (PHY_LINES + 1, CHAN_VIF.1)).expect("replay");
    // btc's role start (rtw89_btc_ntfy_role_info BTC_ROLE_START) inside it writes nothing
    btc::role::role(
        &mut fake.bus,
        &mut bst,
        &mut H2cLink {
            fw: &mut run.fw,
            rings: run.rings.as_mut().unwrap(),
        },
        btc::RoleState::Start,
        &btc::RoleReq::default(),
    )
    .expect("btc role start");
    assert_eq!(
        fake.bus.lines().len(),
        CHAN_VIF.1,
        "role start writes nothing"
    );
    follow_replayed_h2cs(
        gold,
        (PHY_LINES + 1, CHAN_VIF.1),
        run.rings.as_mut().unwrap(),
        &mut run.fw,
    );
    let chan = Chan {
        band: Band::B2g,
        ch: 1,
        bw: 20,
    };
    let mut golden_rfk = GoldenRfk {
        btc: None,
        mac: MacLink {
            mac: &mut run.mac,
            fw: &mut run.fw,
            rings: run.rings.as_mut(),
        },
        calls: Vec::new(),
    };
    let mut link = Rfk2Link {
        st: &mut rfk2,
        hooks: &mut golden_rfk,
        chan: Some(chan),
    };
    let mut band_at = Vec::new();
    let mut btc = |b: &mut TraceBus<UnixStream>, band: Band, h: &mut dyn RfkHooks<_>| {
        band_at.push((band, b.lines().len() + 1));
        btc::switch_band(b, &mut bst, &mut ViaRfk(h), band)
    };
    let mut hooks = ChanLink {
        rfk: &mut rfk,
        hooks: &mut link,
        btc_switch_band: &mut btc,
    };
    let req = ChanReq {
        band: Band::B2g,
        ch: 1,
        bw: 20,
    };
    chan::set_channel(&mut fake.bus, &mut run.phy, &mut cs, req, &mut hooks, false)
        .expect("set_channel on the fake chip");
    let calls = std::mem::take(&mut golden_rfk.calls);
    let ours = fake.bus.lines();
    check_golden(&ours, gold, "set_channel to 2.4 GHz channel 1");
    assert_eq!(ours.len(), CHAN_SWITCH.1, "the switch ends there");
    assert_eq!(band_at, [(Band::B2g, CHAN_BTC_BAND.0)], "btc's band switch");
    assert_eq!(
        calls,
        [
            format!("stop_sch_tx at {}", CHAN_STOP.0),
            format!("resume_sch_tx 0x0 at {}", CHAN_RESUME.0), // V1's fake reads CTN_TXEN as 0
        ],
        "macinit's sch TX stop/resume"
    );
    assert!(rfk.lck.is_empty(), "the LO locked at once: {:?}", rfk.lck);
    assert!(rfk.events().is_empty(), "{:?}", rfk.events());
    assert_eq!(run.phy.tssi_mode, [true, true], "TSSI on");
    assert!(rfk2.events().is_empty(), "{:?}", rfk2.events());
    assert_eq!(
        cs.events(),
        [ChanEvent::Switched {
            band: Band::B2g,
            ch: 1,
            prev_ch: 0,
            band_changed: false
        }],
        "the switch went through"
    );
    assert!(cs.entity_active);
    assert_eq!(cs.chan.freq, 2412);
    // btc's band switch in Rust (WL-2 btc): the radio-on left the TDMA instant execute pending,
    // so the policy goes out in full (230 bytes) with option_ctrl BIT(0)
    assert!(bst.events().is_empty(), "{:?}", bst.events());
    assert_eq!(bst.fwinfo.cnt_h2c, 20, "the band switch's forced policy");
    assert_eq!(bst.dm.tdma_instant_excute, 0);
    assert_eq!(bst.dm.tdma_now[7], 1, "option_ctrl");
    fake
}

#[test]
fn chan_stage_trace_is_golden() {
    if skip("chan_stage_trace_is_golden") {
        return;
    }
    let gold = golden();
    let mut fake = chan_stage(&gold, "chan", &[]);
    replay_lines(&gold, &mut fake.bus, (CHAN_SWITCH.1 + 1, CHAN_LINES)).expect("replay");
    let seen = strip_index(&fake.finish());
    check_golden(&seen, &gold, "the fake's trace");
    assert_eq!(seen.len(), CHAN_LINES, "the fake saw stage chan to its end");
}

#[path = "fake_golden/sta.rs"]
mod sta;

#[path = "fake_golden/life.rs"]
mod life;

#[path = "fake_golden/track.rs"]
mod track;

#[path = "fake_golden/net.rs"]
mod net;

#[path = "fake_golden/fault.rs"]
mod fault;

#[path = "fake_golden/drv.rs"]
mod drv;
