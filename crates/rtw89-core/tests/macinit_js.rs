//! The macinit module against the JS driver's own macinit.mjs (macinit-io/-trx/-imr/
//! -port/-h2c.mjs with the real fw.mjs, pci.mjs, dmac.mjs) and against the C as
//! macinit-spec.mjs transcribes it: tests/fake/macinit-js.mjs runs test-macinit.mjs's
//! and test-macinit-probe.mjs's scenarios on the kit's FakeDev and prints every op, every
//! result and the state; it also prints the C's register sequences (`## spec ...`). The
//! same scenarios run here on HookBus with the same chip answers, and the two texts must
//! be equal line by line. Needs node and the read-only WLAN tree; RTW89_SKIP_FAKE=1 skips
//! (with a line on stderr), but never with BITSOS_GATE or CI set.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::bringup::PwrOpts;
use rtw89_core::bus::{Bus, BusError};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::hookbus::HookBus;
use rtw89_core::fw::{Firmware, FwState};
use rtw89_core::macinit::{self, FilterFlags, MacInit, MacSel, MacState, SchTxSel, Vif, VifHooks};
use rtw89_core::membus::MemBus;
use rtw89_core::pci::{self, DmacState, PciRings, RingConfig};
use rtw89_core::tables::regs::*;

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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS macinit.mjs)"
        );
        return true;
    }
    false
}

fn wlan_dir() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into())
}

fn firmware() -> Firmware {
    let p = wlan_dir().join("firmware/rtw8852b_fw-2.bin");
    let f = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    Firmware::parse(f, 1, false).expect("the real file")
}

fn hx(v: u32) -> String {
    format!("0x{v:08x}")
}
fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// macinit-js.mjs `kind`: MacError (an error return of the C), FwError, RtwError (the bus)
fn kind(e: &Rtw89Error) -> &'static str {
    use Rtw89Error::*;
    match e {
        MacAlreadyOn { .. } | MacNotEnabled { .. } | C2hRegAnswer { .. } | Invalid { .. } => "mac",
        H2cRegBusy { .. }
        | C2hRegTimeout { .. }
        | FwStatus { .. }
        | FwPathNotReady { .. }
        | WcpuAlreadyOn { .. }
        | Ch12Stuck { .. }
        | FwLinkGone { .. } => "fw",
        Bus(BusError::PollTimeout { .. } | BusError::Refused { .. })
        | RingIndex { .. }
        | RingFull { .. } => "device",
        _ => "other",
    }
}

struct Out(Vec<String>);

impl Out {
    /// A scenario on a bus: its ops since now, then what `f` returned (or its error's kind)
    fn on<'a>(
        &mut self,
        name: &str,
        b: &mut HookBus<'a>,
        f: impl FnOnce(&mut HookBus<'a>) -> Result<Vec<String>, Rtw89Error>,
    ) {
        self.0.push(format!("## {name}"));
        let t0 = b.mem.trace.len();
        let r = f(b);
        self.0.extend(b.mem.lines()[t0..].iter().cloned());
        match r {
            Ok(v) => self.0.extend(v),
            Err(e) => self.0.push(format!("! error {}", kind(&e))),
        }
    }
    /// The C's sequence as the Rust side writes it: `lines` of a run
    fn spec(&mut self, name: &str, lines: &[String]) {
        self.0.push(format!("## spec {name}"));
        self.0.extend(lines.iter().cloned());
    }
}

/// macinit-js.mjs `fake`: `extra(addr)` answers first; then the BA CAM reset done and all
/// public pages free; config space with the PCIe capability at 0x70, the link at gen 1
fn fake<'a>(extra: impl Fn(u32) -> Option<u32> + 'a) -> HookBus<'a> {
    let mut m = MemBus::new();
    for (off, v) in [
        (0x04u16, 0x0010_0002u32),
        (0x34, 0x70),
        (0x70, 0x0002_0010),
        (0x80, 0x1011_0000),
    ] {
        m.cfg.insert(off, v);
    }
    let mut b = HookBus::new(m);
    b.on_read(move |a| {
        extra(a).or(match a {
            R_AX_RESPBA_CAM_CTRL => Some(0),
            R_AX_PUB_PAGE_INFO2 => Some(446),
            _ => None,
        })
    });
    b
}
fn plain<'a>() -> HookBus<'a> {
    fake(|_| None)
}

fn enabled(b: &mut HookBus<'_>) {
    b.w32(R_AX_DMAC_FUNC_EN, B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN)
        .unwrap();
    b.w32(R_AX_CMAC_FUNC_EN, B_AX_CMAC_EN).unwrap();
    b.mem.trace.clear();
}

/// fw-js.mjs `ready`: the rings and the partial init up to the download
fn ready(b: &mut HookBus<'_>) -> PciRings {
    b.set32(R_AX_DMAC_FUNC_EN, B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN)
        .unwrap();
    pci::hci_dma_trx(b, true).unwrap();
    pci::dmac_pre_init(b, &mut DmacState::default()).unwrap();
    let mut rings = pci::pci_alloc(b, &RingConfig::js()).unwrap();
    pci::pre_init(b, &mut rings).unwrap();
    b.writes.clear();
    rings
}

const OPTS: PwrOpts = PwrOpts {
    rfe: 1,
    efuse_valid: true,
    power_k_valid: false,
    cv: 1,
    probe: false,
};

fn pw(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "1",
        Some(false) => "0",
        None => "-",
    }
}
/// macinit-js.mjs `logs`: the MAC part's lines up to the first parenthesis
fn logs(m: &MacState) -> Vec<String> {
    m.event_lines()
        .iter()
        .map(|l| format!("= log {}", l.split(" (").next().unwrap_or("")))
        .collect()
}
fn state(m: &MacState, fw: &FwState) -> Vec<String> {
    let mut v = vec![format!(
        "= power {} ready {}",
        pw(m.power_on),
        u8::from(fw.ready)
    )];
    v.extend(logs(m));
    v
}
fn h2c_bytes(b: &HookBus<'_>, from: usize) -> Vec<String> {
    b.writes[from..]
        .iter()
        .map(|(_, d)| format!("= h2c {}", hexs(&d[24..])))
        .collect()
}

fn dle(m: &mut MacState) {
    m.dmac.dle_info = Some(pci::dmac::DleInfo {
        qta_mode: pci::QtaMode::Scc,
        ple_pg_size: 128,
        ple_free_pg: 0,
        c0_rx_qta: 89,
        c1_rx_qta: 0,
    });
}

const HOOK_MARK: u32 = 0x700;

/// macinit-js.mjs `hooks`: each a register write to a marker and a record
#[derive(Default)]
struct Hooks(Vec<String>);

impl Hooks {
    fn mark(&mut self, b: &mut HookBus<'_>, what: String) -> Result<(), Rtw89Error> {
        b.w32(HOOK_MARK + 4 * self.0.len() as u32, 1)?;
        self.0.push(what);
        Ok(())
    }
}

impl<'a> VifHooks<HookBus<'a>> for Hooks {
    fn macid_pause(
        &mut self,
        b: &mut HookBus<'a>,
        sh: u32,
        grp: u32,
        p: bool,
    ) -> Result<(), Rtw89Error> {
        self.mark(b, format!("macidPause {sh} {grp} {p}"))
    }
    fn role_maintain(&mut self, b: &mut HookBus<'a>, upd: u32) -> Result<(), Rtw89Error> {
        self.mark(b, format!("roleMaintain {upd}"))
    }
    fn join_info(&mut self, b: &mut HookBus<'a>, d: bool) -> Result<(), Rtw89Error> {
        self.mark(b, format!("joinInfo {d}"))
    }
    fn cam_init(&mut self, b: &mut HookBus<'a>) -> Result<(), Rtw89Error> {
        self.mark(b, "camInit".into())
    }
    fn addr_cam(&mut self, b: &mut HookBus<'a>, upd: u32) -> Result<(), Rtw89Error> {
        self.mark(b, format!("addrCam {upd}"))
    }
    fn default_cmac_tbl(&mut self, b: &mut HookBus<'a>) -> Result<(), Rtw89Error> {
        self.mark(b, "defaultCmacTbl".into())
    }
}

/// The lines `f` makes on a fresh bus (after `prep`)
fn lines_of<'a>(
    mut b: HookBus<'a>,
    f: impl FnOnce(&mut HookBus<'a>) -> Result<(), Rtw89Error>,
) -> Vec<String> {
    let t0 = b.mem.trace.len();
    f(&mut b).unwrap();
    b.mem.lines()[t0..].to_vec()
}

fn from_line(lines: Vec<String>, starts: &str, before: usize) -> Vec<String> {
    let i = lines.iter().position(|l| l.contains(starts)).unwrap();
    lines[i - before..].to_vec()
}

fn spec_side(out: &mut Out) {
    let ss_ctrl = format!("0x{R_AX_SS_CTRL:08X}");
    let wdrls = format!("0x{R_AX_WDRLS_ERR_IMR:08X}");
    out.spec("sys_init", &lines_of(plain(), macinit::sys_init));
    out.spec("enable_bb_rf", &lines_of(plain(), macinit::enable_bb_rf));
    let mut b = plain();
    enabled(&mut b);
    let l = lines_of(b, |b| macinit::dmac_init(b, &mut MacState::new()));
    out.spec("dmac_init tail", &from_line(l, &ss_ctrl, 1));
    let mut b = plain();
    enabled(&mut b);
    let l = lines_of(b, |b| {
        let mut m = MacState::new();
        dle(&mut m);
        macinit::cmac_init(b, &mut m)
    });
    out.spec("cmac_init", &l);
    let mut b = plain();
    enabled(&mut b);
    let l = lines_of(b, |b| macinit::trx_init(b, &mut MacState::new()));
    out.spec("trx_init tail", &from_line(l, &wdrls, 1));
    for port in 0..4u8 {
        let l = lines_of(plain(), |b| {
            macinit::port_update(b, &mut MacState::new(), &Vif::station(0, port, [0; 6]))
        });
        out.spec(&format!("port_update {port} no link"), &l);
    }
    let infra = Vif {
        net_type: RTW89_NET_TYPE_INFRA,
        beacon_int: 102,
        dtim_period: 3,
        bss_color: 5,
        ..Vif::station(3, 1, [0; 6])
    };
    let cfg1 = R_AX_PORT_CFG_P0 + 0x40;
    let l = lines_of(fake(|a| (a == cfg1).then_some(B_AX_PORT_FUNC_EN)), |b| {
        macinit::port_update(b, &mut MacState::new(), &infra)
    });
    out.spec("port_update 1 infra func_sw", &l);
    let vif = Vif::station(3, 1, [0; 6]);
    let n = lines_of(plain(), |b| {
        macinit::port_update(b, &mut MacState::new(), &vif)
    })
    .len();
    let mut b = plain();
    enabled(&mut b);
    let l = lines_of(b, |b| {
        macinit::vif_init(b, &mut MacState::new(), &vif, &mut Hooks::default())
    });
    out.spec("vif tables 3", &l[n..n + 17]);
    let mut b = plain();
    enabled(&mut b);
    let mut rings = ready(&mut b);
    enabled(&mut b);
    let l = lines_of(b, |b| {
        macinit::core_start_tail(b, &mut rings, &mut FwState::new(), None, None)
    });
    out.spec("core tail", &l[..5]);
    let mut b = plain();
    b.on_read(|a| match a {
        R_AX_CMAC_FUNC_EN => Some(B_AX_CMAC_EN),
        R_AX_C2HREG_CTRL => Some(1),
        R_AX_C2HREG_DATA0 => Some(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT | (2 << 8)),
        _ => None,
    });
    let l = lines_of(b, |b| {
        let mut fw = FwState::new();
        fw.ready = true;
        macinit::resume_sch_tx(b, &mut MacState::new(), &mut fw, 0x1234)
    });
    out.spec("sch tx h2c", &l[1..]);
}

fn rust_side() -> Vec<String> {
    let mut out = Out(Vec::new());
    spec_side(&mut out);
    let fw = firmware();

    // ---- power switch ----
    {
        let (mut m, mut f) = (MacState::new(), FwState::new());
        let mut d = plain();
        out.on("preinit", &mut d, |b| {
            macinit::mac_preinit(b, &OPTS, &mut m, &mut f)?;
            Ok(state(&m, &f))
        });
        let (mut m, mut f) = (MacState::new(), FwState::new());
        let mut v = plain();
        let o = PwrOpts {
            efuse_valid: false,
            ..OPTS
        };
        out.on("preinit efuse invalid", &mut v, |b| {
            macinit::mac_preinit(b, &o, &mut m, &mut f)?;
            Ok(state(&m, &f))
        });
        let reads = Cell::new(0);
        let mut a = fake(|x| {
            (x == R_AX_IC_PWR_STATE && {
                reads.set(reads.get() + 1);
                reads.get() == 1
            })
            .then_some(0x100)
        });
        let (mut m, mut f) = (MacState::new(), FwState::new());
        f.ready = true;
        let o = PwrOpts {
            power_k_valid: true,
            ..OPTS
        };
        out.on("preinit mac already on", &mut a, |b| {
            macinit::mac_preinit(b, &o, &mut m, &mut f)?;
            Ok(state(&m, &f))
        });
        let fails = RefCell::new(vec![B_AX_APFN_ONMAC, B_AX_APFM_OFFMAC]);
        let mut r = plain();
        r.on_poll(|x, mask| {
            let mut v = fails.borrow_mut();
            Ok(
                match v.iter().position(|&k| x == R_AX_SYS_PW_CTRL && k == mask) {
                    Some(i) => {
                        v.remove(i);
                        false
                    }
                    None => true,
                },
            )
        });
        let (mut m, mut f) = (MacState::new(), FwState::new());
        out.on("preinit retry, failing power off", &mut r, |b| {
            macinit::mac_preinit(b, &OPTS, &mut m, &mut f)?;
            Ok(state(&m, &f))
        });
        let mut p = plain();
        let (mut m, mut f) = (MacState::new(), FwState::new());
        f.ready = true;
        m.power_on = Some(true);
        out.on("power off", &mut p, |b| {
            macinit::mac_pwr_off(b, 1, &mut m, &mut f)?;
            Ok(state(&m, &f))
        });
        let secure = |p: &rtw89_core::bringup::PowerOn| {
            format!("= secure {}", u8::from(p.secure_boot == Some(true)))
        };
        let once = Cell::new(true);
        let mut g = fake(|x| (x == R_AX_EFUSE_CTRL).then_some(B_AX_EF_RDY | 0xff));
        g.on_poll(|x, mask| {
            Ok(!(x == R_AX_SYS_PW_CTRL && mask == B_AX_APFN_ONMAC && once.replace(false)))
        });
        let (mut m, mut f) = (MacState::new(), FwState::new());
        out.on("probe power on with a retry", &mut g, |b| {
            let p = macinit::mac_pwr_on(b, &PwrOpts::probe(1), &mut m, &mut f)?;
            let mut v = vec![secure(&p)];
            v.extend(state(&m, &f));
            Ok(v)
        });
        let mut s = fake(|x| (x == R_AX_EFUSE_CTRL).then_some(B_AX_EF_RDY | 0x01));
        out.on("probe power on, secure boot bytes", &mut s, |b| {
            let p = macinit::mac_pwr_on(
                b,
                &PwrOpts::probe(1),
                &mut MacState::new(),
                &mut FwState::new(),
            )?;
            Ok(vec![secure(&p)])
        });
        let mut n = plain();
        n.on_poll(|x, mask| Ok(!(x == R_AX_EFUSE_CTRL && mask == B_AX_EF_RDY)));
        let (mut m, mut f) = (MacState::new(), FwState::new());
        out.on("probe power on, efuse never ready", &mut n, |b| {
            let p = macinit::mac_pwr_on(b, &PwrOpts::probe(1), &mut m, &mut f)?;
            let mut v = vec![secure(&p)];
            v.extend(state(&m, &f));
            Ok(v)
        });
    }

    // ---- sys/dmac/cmac/trx init ----
    {
        let mut d = plain();
        out.on("sys_init", &mut d, |b| {
            macinit::sys_init(b)?;
            Ok(vec![])
        });
        out.on("enable_bb_rf", &mut d, |b| {
            macinit::enable_bb_rf(b)?;
            Ok(vec![])
        });
        let mut m = plain();
        enabled(&mut m);
        out.on("dmac_init", &mut m, |b| {
            macinit::dmac_init(b, &mut MacState::new())?;
            Ok(vec![])
        });
        let mut c = plain();
        enabled(&mut c);
        let mut ms = MacState::new();
        dle(&mut ms);
        out.on("cmac_init", &mut c, |b| {
            macinit::cmac_init(b, &mut ms)?;
            Ok(logs(&ms))
        });
        let mut t = plain();
        enabled(&mut t);
        out.on("trx_init", &mut t, |b| {
            macinit::trx_init(b, &mut MacState::new())?;
            Ok(vec![])
        });
    }

    // ---- rtw89_mac_init ----
    let mac_init = |out: &mut Out, name: &str, b: &mut HookBus<'_>, preinit: bool, en: bool| {
        let mut rings = pci::pci_alloc(b, &RingConfig::js()).unwrap();
        if en {
            enabled(b);
        }
        let (mut m, mut f) = (MacState::new(), FwState::new());
        out.on(name, b, |b| {
            let c = MacInit {
                rings: &mut rings,
                mac: &mut m,
                fws: &mut f,
                fw: &fw,
                opts: &OPTS,
            };
            let r = macinit::mac_init(b, c, preinit);
            let mut v = match &r {
                Ok(()) if name == "mac_init" => vec![format!("= seq {}", f.h2c_seq)],
                Ok(()) => vec!["= no error".into()],
                Err(e) => vec![format!("! error {}", kind(e))],
            };
            v.extend(state(&m, &f));
            Ok(v)
        });
    };
    let mut d = plain();
    mac_init(&mut out, "mac_init", &mut d, true, false);
    let mut f = plain();
    f.on_poll(|a, _| Ok(a != R_AX_SS_CTRL));
    mac_init(
        &mut out,
        "mac_init, sta sch never ready",
        &mut f,
        true,
        false,
    );
    let mut p = fake(|a| (a == R_AX_PUB_PAGE_INFO2).then_some(445));
    mac_init(&mut out, "mac_init, public pages", &mut p, false, true);

    // ---- vif init and the port update ----
    for (port, mac_id) in [(0u8, 0u8), (1, 3)] {
        let mut d = plain();
        enabled(&mut d);
        out.on(
            &format!("vif_init port {port} macid {mac_id}"),
            &mut d,
            |b| {
                let vif = Vif::station(mac_id, port, [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01]);
                let mut h = Hooks::default();
                macinit::vif_init(b, &mut MacState::new(), &vif, &mut h)?;
                Ok(h.0.iter().map(|c| format!("= hook {c}")).collect())
            },
        );
    }
    let cfg1 = R_AX_PORT_CFG_P0 + 0x40;
    let mut a = fake(|r| (r == cfg1).then_some(B_AX_PORT_FUNC_EN));
    out.on("port_update infra port 1 running", &mut a, |b| {
        let v = Vif {
            net_type: RTW89_NET_TYPE_INFRA,
            beacon_int: 102,
            dtim_period: 3,
            bss_color: 5,
            ..Vif::station(3, 1, [0; 6])
        };
        macinit::port_update(b, &mut MacState::new(), &v).map(|_| vec![])
    });
    for port in [2u8, 3, 4] {
        let mut p = plain();
        out.on(&format!("port_update ad hoc port {port}"), &mut p, |b| {
            let v = Vif {
                net_type: RTW89_NET_TYPE_AD_HOC,
                beacon_int: 200,
                dtim_period: 1,
                bss_color: 9,
                nontransmitted: true,
                bssid_index: 2,
                ..Vif::station(1, port, [0; 6])
            };
            macinit::port_update(b, &mut MacState::new(), &v).map(|_| vec![])
        });
    }
    let mut x = plain();
    out.on("port_update port 5", &mut x, |b| {
        macinit::port_update(b, &mut MacState::new(), &Vif::station(0, 5, [0; 6])).map(|_| vec![])
    });
    let mut ap = fake(|r| (r == R_AX_PORT_CFG_P0).then_some(B_AX_PORT_FUNC_EN));
    out.on("port_update running AP port", &mut ap, |b| {
        let v = Vif {
            net_type: RTW89_NET_TYPE_AP_MODE,
            ..Vif::station(0, 0, [0; 6])
        };
        macinit::port_update(b, &mut MacState::new(), &v).map(|_| vec![])
    });

    // ---- the MACID pause H2C, the core start tail ----
    {
        let mut d = plain();
        let mut rings = ready(&mut d);
        enabled(&mut d);
        let mut f = FwState::new();
        out.on("macid pause 37", &mut d, |b| {
            macinit::macid_pause(b, &mut rings, &mut f, 37, true)?;
            Ok(h2c_bytes(b, 0))
        });
        let from = d.writes.len();
        out.on("macid unpause 2", &mut d, |b| {
            macinit::macid_pause(b, &mut rings, &mut f, 2, false)?;
            Ok(h2c_bytes(b, from))
        });
        let from = d.writes.len();
        let order = RefCell::new(Vec::new());
        out.on("core start tail", &mut d, |b| {
            let mut track = || order.borrow_mut().push("track");
            let mut radio = |x: &mut HookBus<'_>,
                             _: &mut PciRings,
                             _: &mut FwState|
             -> Result<(), Rtw89Error> {
                order.borrow_mut().push("radio");
                x.w32(0xB7C, 1)?;
                Ok(())
            };
            macinit::core_start_tail(b, &mut rings, &mut f, Some(&mut track), Some(&mut radio))?;
            let mut v = vec![format!("= order {}", order.borrow().join(" "))];
            v.extend(h2c_bytes(b, from));
            v.push(format!("= seq {}", f.h2c_seq));
            Ok(v)
        });
        out.on("ppdu status off", &mut d, |b| {
            macinit::cfg_ppdu_status(b, false).map(|_| vec![])
        });
    }

    // ---- check_mac_en ----
    for (sel, name, v) in [
        (MacSel::Dmac, "dmac", 0xEAEA_EAEA),
        (MacSel::Dmac, "dmac", B_AX_MAC_FUNC_EN),
        (MacSel::Dmac, "dmac", B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN),
        (MacSel::Cmac, "cmac", 0xDEAD_BEEF),
        (MacSel::Cmac, "cmac", 0),
        (MacSel::Cmac, "cmac", B_AX_CMAC_EN),
    ] {
        let mut d = fake(move |a| (a == R_AX_DMAC_FUNC_EN || a == R_AX_CMAC_FUNC_EN).then_some(v));
        out.on(&format!("check_mac_en {name} {}", hx(v)), &mut d, |b| {
            macinit::check_mac_en(b, sel).map(|_| vec!["= ok".into()])
        });
    }

    // ---- sch TX ----
    let sch_fake = |id: u32, cmac: u32| {
        let b = fake(move |a| match a {
            R_AX_CTN_TXEN => Some(0x1234),
            R_AX_CMAC_FUNC_EN => Some(cmac),
            R_AX_C2HREG_CTRL => Some(1),
            R_AX_C2HREG_DATA0 => Some(id | (2 << 8)),
            _ => None,
        });
        let mut f = FwState::new();
        f.ready = true;
        (b, MacState::new(), f)
    };
    let sch_err = |m: &MacState| {
        let mut v = vec![format!("= err {}", m.sch_tx_err.as_ref().map_or("-", kind))];
        v.extend(logs(m));
        v
    };
    let ok_id = RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT;
    {
        let (mut d, mut m, mut f) = sch_fake(ok_id, B_AX_CMAC_EN);
        out.on("stop sch tx all", &mut d, |b| {
            let t = macinit::stop_sch_tx(b, &mut m, &mut f, SchTxSel::All)?;
            let mut v = vec![format!("= tx_en {}", hx(t.into()))];
            v.extend(sch_err(&m));
            Ok(v)
        });
        out.on("resume sch tx", &mut d, |b| {
            macinit::resume_sch_tx(b, &mut m, &mut f, 0x1234)?;
            Ok(sch_err(&m))
        });
        for (sel, name) in [
            (SchTxSel::Hiq, "hiq"),
            (SchTxSel::Mg0, "mg0"),
            (SchTxSel::Macid, "macid"),
        ] {
            let (mut s, mut m, mut f) = sch_fake(ok_id, B_AX_CMAC_EN);
            out.on(&format!("stop sch tx {name}"), &mut s, |b| {
                let t = macinit::stop_sch_tx(b, &mut m, &mut f, sel)?;
                let mut v = vec![format!("= tx_en {}", hx(t.into()))];
                v.extend(sch_err(&m));
                Ok(v)
            });
        }
        let stop =
            |out: &mut Out, name: &str, b: &mut HookBus<'_>, m: &mut MacState, f: &mut FwState| {
                out.on(name, b, |b| {
                    let t = macinit::stop_sch_tx(b, m, f, SchTxSel::All)?;
                    let mut v = vec![format!("= tx_en {}", hx(t.into()))];
                    v.extend(sch_err(m));
                    Ok(v)
                });
            };
        let (mut w, mut m, mut f) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_NULL, B_AX_CMAC_EN);
        stop(&mut out, "stop sch tx, wrong c2h", &mut w, &mut m, &mut f);
        let (mut b, mut m, mut f) = sch_fake(ok_id, B_AX_CMAC_EN);
        b.on_poll(|a, _| Ok(a != R_AX_H2CREG_CTRL));
        stop(
            &mut out,
            "stop sch tx, h2c register busy",
            &mut b,
            &mut m,
            &mut f,
        );
        let (mut c, mut m, mut f) = sch_fake(ok_id, 0xDEAD_BEEF);
        out.on("resume sch tx, cmac off", &mut c, |b| {
            macinit::resume_sch_tx(b, &mut m, &mut f, 0x1234)?;
            Ok(sch_err(&m))
        });
        let (mut n, mut m, mut f) = sch_fake(ok_id, B_AX_CMAC_EN);
        macinit::mac_pwr_off(&mut n, 1, &mut m, &mut f).unwrap();
        out.on("resume sch tx before fw", &mut n, |b| {
            macinit::resume_sch_tx(b, &mut m, &mut f, 0x1234)?;
            Ok(sch_err(&m))
        });
        out.on("stop sch tx hiq before fw", &mut n, |b| {
            let t = macinit::stop_sch_tx(b, &mut m, &mut f, SchTxSel::Hiq)?;
            let mut v = vec![format!("= tx_en {}", hx(t.into()))];
            v.extend(sch_err(&m));
            Ok(v)
        });
    }

    // ---- configure_filter ----
    {
        let mut d = plain();
        let mut m = MacState::new();
        m.rx_fltr = macinit::default_rx_fltr() | B_AX_SNIFFER_MODE;
        let f = FilterFlags::default;
        let all = FilterFlags {
            allmulti: true,
            fcsfail: true,
            other_bss: true,
            bcn_prbresp_promisc: true,
            probe_req: true,
        };
        let steps = [
            (
                "allmulti",
                FilterFlags {
                    allmulti: true,
                    ..f()
                },
                false,
            ),
            ("none", f(), false),
            (
                "fcsfail",
                FilterFlags {
                    fcsfail: true,
                    ..f()
                },
                false,
            ),
            (
                "otherBss",
                FilterFlags {
                    other_bss: true,
                    ..f()
                },
                false,
            ),
            (
                "probeReq",
                FilterFlags {
                    probe_req: true,
                    ..f()
                },
                false,
            ),
            (
                "bcnPrbrespPromisc",
                FilterFlags {
                    bcn_prbresp_promisc: true,
                    ..f()
                },
                true,
            ),
            ("none", f(), true),
            ("none", f(), false),
            (
                "allmulti,bcnPrbrespPromisc,fcsfail,otherBss,probeReq",
                all,
                true,
            ),
            (
                "allmulti,probeReq",
                FilterFlags {
                    allmulti: true,
                    probe_req: true,
                    ..f()
                },
                false,
            ),
        ];
        for (name, flags, scanning) in steps {
            let s = if scanning { " scanning" } else { "" };
            out.on(&format!("rx filter {name}{s}"), &mut d, |b| {
                let rx = macinit::rx_filter(b, &mut m, flags, scanning)?;
                Ok(vec![format!("= rx {} hal {}", hx(rx), hx(m.rx_fltr))])
            });
        }
    }
    out.0
}

fn js_side() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/macinit-js.mjs");
    let o = Command::new("node")
        .arg(&script)
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        o.status.success(),
        "macinit-js.mjs: {}\n{}",
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn macinit_does_what_the_js_driver_and_the_c_do() {
    if skip("macinit_does_what_the_js_driver_and_the_c_do") {
        return;
    }
    let js = js_side();
    let ours = rust_side();
    let mut scenario = String::new();
    for (i, (o, j)) in ours.iter().zip(js.iter()).enumerate() {
        if j.starts_with("## ") {
            scenario = j.clone();
        }
        assert_eq!(o, j, "line {} in {scenario}", i + 1);
    }
    assert_eq!(ours.len(), js.len(), "as many lines");
    let scenarios = js.iter().filter(|l| l.starts_with("## ")).count();
    let specs = js.iter().filter(|l| l.starts_with("## spec ")).count();
    assert!(
        scenarios >= 60 && specs == 13,
        "{scenarios} scenarios, {specs} specs"
    );
    assert!(js.len() > 4500, "{} lines", js.len());
}
