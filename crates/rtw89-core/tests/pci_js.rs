//! The pci and dmac modules against the JS driver's own pci.mjs and dmac.mjs:
//! tests/fake/pci-js.mjs runs test-pci.mjs's scenarios (and more) on the kit's
//! FakeDev and prints every op; the same scenarios run here on MemBus with the
//! same chip answers, and the two texts must be equal line by line. Needs node
//! and the read-only WLAN tree; RTW89_SKIP_FAKE=1 skips (with a line on stderr), but
//! never with BITSOS_GATE or CI set: a gate run must run the JS oracle.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::bus::{Bus, prep};
use rtw89_core::error::Rtw89Result;
use rtw89_core::membus::MemBus;
use rtw89_core::pci::{self, DmacState, PciRings, QtaMode, RingConfig, RxBuf};
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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS pci.mjs/dmac.mjs)"
        );
        return true;
    }
    false
}

/// test-pci.mjs's chip, as pci-js.mjs `fake(cfg, answers)` builds it
fn fake(cfg: &[(u16, u32)], answers: &[(u32, u32)]) -> MemBus {
    let mut b = MemBus::new();
    for (o, v) in [
        (0x04, 0x0010_0002),
        (0x34, 0x70),
        (0x70, 0x0002_0010),
        (0x80, 0x1011_0000),
        (0x98, 0x400),
    ]
    .into_iter()
    .chain(cfg.iter().copied())
    {
        b.cfg.insert(o, v);
    }
    b.forced.insert(R_AX_MDIO_CFG, 0);
    b.forced.insert(R_AX_MDIO_RDATA, 0xffff);
    b.forced
        .insert(R_AX_DBI_RDATA + (RTW89_PCIE_L1_CTRL & 3), 0x47);
    b.forced.insert(R_AX_LBC_WATCHDOG, 0x1234_5678);
    for &(a, v) in answers {
        b.forced.insert(a, v);
    }
    b
}

struct Out(Vec<String>);

impl Out {
    /// One scenario: its ops since the last one, then what `f` returned (or `! error`)
    fn scenario<T>(
        &mut self,
        name: &str,
        b: &mut MemBus,
        f: impl FnOnce(&mut MemBus) -> Rtw89Result<T>,
        show: impl FnOnce(&T, &MemBus) -> Vec<String>,
    ) {
        self.0.push(format!("## {name}"));
        let t0 = b.trace.len();
        let r = f(b);
        let lines = b.lines();
        self.0.extend(lines[t0..].iter().cloned());
        match r {
            Ok(v) => self.0.extend(show(&v, b)),
            Err(_) => self.0.push("! error".into()),
        }
    }
}

fn none<T>(_: &T, _: &MemBus) -> Vec<String> {
    Vec::new()
}

fn show_rx(got: &[RxBuf]) -> Vec<String> {
    got.iter()
        .map(|x| {
            let first = x.data.first().map_or(-1, |&v| i32::from(v));
            format!(
                "= rx {} {} {} {} {} {}",
                x.data.len(),
                u8::from(x.fs),
                u8::from(x.ls),
                first,
                x.tag,
                x.len
            )
        })
        .collect()
}

fn fill(b: &mut MemBus, rings: &PciRings, i: usize, n: usize, tag: u32) {
    let fs = if !i.is_multiple_of(3) {
        RTW89_PCI_RXBD_FS
    } else {
        0
    };
    let ls = if !i.is_multiple_of(2) {
        0
    } else {
        RTW89_PCI_RXBD_LS
    };
    let info =
        prep(RTW89_PCI_RXBD_WRITE_SIZE, 4 + n as u32) | prep(RTW89_PCI_RXBD_TAG, tag) | fs | ls;
    let mut buf = info.to_le_bytes().to_vec();
    buf.extend(std::iter::repeat_n(((i + 1) & 0xff) as u8, n));
    let at = rings.rx("RXQ").unwrap().bufs[i];
    b.mem_write_bytes(at, &buf).unwrap();
}

fn rust_side() -> Vec<String> {
    let mut out = Out(Vec::new());
    let js = RingConfig::js();

    // ---- test-pci.mjs's run ----
    let mut d = fake(&[], &[]);
    let mut st = DmacState::default();
    let mut rings = PciRings::default();
    out.scenario(
        "alloc, dmac pre-init, pci pre-init",
        &mut d,
        |b| {
            rings = pci::pci_alloc(b, &js)?;
            pci::dmac_pre_init(b, &mut st)?;
            pci::pre_init(b, &mut rings)
        },
        none,
    );
    for (name, rate) in [
        ("pre-init at gen 2", 0x1012_0000),
        ("pre-init at a PHY rate without MDIO pages", 0x1013_0000),
    ] {
        let mut g = fake(&[(0x80, rate)], &[]);
        out.scenario(
            name,
            &mut g,
            |b| {
                let mut r = pci::pci_alloc(b, &js)?;
                pci::pre_init(b, &mut r)
            },
            none,
        );
    }
    out.scenario(
        "capability walk, post-init, basic_cfg",
        &mut d,
        |b| {
            let cap = pci::exp_cap(b, &mut rings)?;
            pci::post_init(b, &mut rings)?;
            Ok((cap, pci::basic_cfg(b, &mut rings)?))
        },
        |(cap, bc), _| {
            vec![
                format!("= cap {cap}"),
                format!("= basic {} {}", bc.devctl2, bc.lnkctl),
            ]
        },
    );
    let mut no_ltr = fake(&[(0x98, 0)], &[]);
    out.scenario(
        "post-init without LTR",
        &mut no_ltr,
        |b| {
            let mut r = pci::pci_alloc(b, &js)?;
            pci::post_init(b, &mut r)
        },
        none,
    );
    let mut no_cap = fake(&[(0x34, 0x50)], &[]);
    out.scenario(
        "no PCIe capability",
        &mut no_cap,
        |b| {
            let mut r = pci::pci_alloc(b, &js)?;
            pci::exp_cap(b, &mut r)
        },
        none,
    );

    // the firmware command channel
    for (n, len, fw_dl) in [(1u8, 100usize, false), (2, 2020, true)] {
        let ring = rings.tx("CH12").unwrap();
        let at = ring.bufs[ring.wp as usize];
        out.scenario(
            &format!("fwcmd {n}"),
            &mut d,
            |b| {
                pci::fwcmd(b, &mut rings, &vec![0xab + n; len], fw_dl, 50)?;
                let ring = rings.tx("CH12").unwrap();
                Ok((ring.wp, ring.rp))
            },
            |&(wp, rp), b| {
                let m: Vec<u8> = (0..24 + len as u32)
                    .map(|i| b.mem.get(&(at + i)).copied().unwrap_or(0))
                    .collect();
                let hex: String = m[..48].iter().fold(String::new(), |mut s, x| {
                    let _ = write!(s, "{x:02x}");
                    s
                });
                vec![
                    format!("= mem {hex} {}", m[24..].iter().all(|&x| x == 0xab + n)),
                    format!("= wp {wp} {rp}"),
                ]
            },
        );
    }
    out.scenario(
        "fwcmd too long",
        &mut d,
        |b| pci::fwcmd(b, &mut rings, &[0u8; 2025], false, 50),
        none,
    );

    // RX
    for (i, n) in [60usize, 1500, 3000].into_iter().enumerate() {
        fill(&mut d, &rings, i, n, i as u32);
    }
    let rxq_idx = rings.rx("RXQ").unwrap().idx;
    d.forced.insert(rxq_idx, prep(TXBD_HW_IDX_MASK, 3));
    for (name, max) in [("rx three", 32), ("rx empty", 32)] {
        out.scenario(
            name,
            &mut d,
            |b| pci::rx_poll(b, &mut rings, "RXQ", max),
            |got, _| show_rx(got),
        );
    }
    for i in 3..64 {
        fill(&mut d, &rings, i, (i * 137) % 4300, i as u32);
    }
    for i in 0..2 {
        fill(&mut d, &rings, i, 10 + i, 100 + i as u32);
    }
    d.forced.insert(rxq_idx, prep(TXBD_HW_IDX_MASK, 2));
    for (name, max) in [("rx budget 5", 5), ("rx wrap", 100)] {
        out.scenario(
            name,
            &mut d,
            |b| pci::rx_poll(b, &mut rings, "RXQ", max),
            |got, _| show_rx(got),
        );
    }

    out.scenario("deinit", &mut d, pci::deinit, none);
    out.scenario(
        "reset",
        &mut d,
        |b| {
            pci::reset(b, &mut rings)?;
            Ok(rings
                .tx
                .iter()
                .map(|x| format!("{}/{}", x.wp, x.rp))
                .chain(rings.rx.iter().map(|x| format!("{}/{}", x.wp, x.rp)))
                .collect::<Vec<_>>())
        },
        |idx, _| vec![format!("= idx {}", idx.join(" "))],
    );

    // ---- the DLE and HFC init of the MAC init (SCC) ----
    let pages = |pub_aval: u32| {
        vec![
            (
                R_AX_PUB_PAGE_INFO1,
                prep(B_AX_G0_USE_PG_MASK, 6) | prep(B_AX_G1_USE_PG_MASK, 0),
            ),
            (R_AX_PUB_PAGE_INFO3, prep(B_AX_G0_AVAL_PG_MASK, 440)),
            (R_AX_PUB_PAGE_INFO2, prep(B_AX_PUB_AVAL_PG_MASK, pub_aval)),
            (R_AX_WP_PAGE_INFO1, prep(B_AX_WP_AVAL_PG_MASK, 77)),
            (
                R_AX_ACH0_PAGE_INFO + 4,
                prep(B_AX_AVAL_PG_MASK, 5) | prep(B_AX_USE_PG_MASK, 3),
            ),
            (
                R_AX_ACH0_PAGE_INFO + 9 * 4,
                prep(B_AX_AVAL_PG_MASK, 4) | prep(B_AX_USE_PG_MASK, 1),
            ),
        ]
    };
    for (name, pub_aval) in [
        ("scc dle and hfc", 440),
        ("scc hfc public pages do not add up", 400),
    ] {
        let mut m = fake(&[], &pages(pub_aval));
        let mut st = DmacState::default();
        out.scenario(
            name,
            &mut m,
            |b| {
                pci::dmac_pre_init(b, &mut st)?;
                pci::dle_init(b, &mut st, QtaMode::Scc, QtaMode::Scc)?;
                pci::hfc_init(b, &mut st, true, true, true)?;
                Ok(st.clone())
            },
            |st: &DmacState, _| {
                let p = &st.hfc;
                let pi = p.pub_info;
                let dle = st.dle_info.unwrap();
                let ch: Vec<String> = p
                    .ch_info
                    .iter()
                    .map(|(c, x)| format!("{c}:{}/{}", x.aval, x.used))
                    .collect();
                vec![
                    format!(
                        "= hfc {} {} {} {} {} {} {} {}",
                        u8::from(p.en),
                        u8::from(p.h2c_en),
                        pi.g0_used,
                        pi.g1_used,
                        pi.g0_aval,
                        pi.g1_aval,
                        pi.pub_aval,
                        pi.wp_aval
                    ),
                    format!("= ch {}", ch.join(" ")),
                    format!(
                        "= dle {} {} {} {} {}",
                        dle.qta_mode.name(),
                        dle.ple_pg_size,
                        dle.ple_free_pg,
                        dle.c0_rx_qta,
                        dle.c1_rx_qta
                    ),
                ]
            },
        );
    }
    for (name, v) in [
        ("dmac not enabled", 0xEAEA_EAEA),
        ("dmac half enabled", B_AX_MAC_FUNC_EN),
    ] {
        let mut m = fake(&[], &[(R_AX_DMAC_FUNC_EN, v)]);
        let mut st = DmacState::default();
        out.scenario(name, &mut m, |b| pci::dmac_pre_init(b, &mut st), none);
    }
    out.0
}

fn js_side() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/pci-js.mjs");
    let o = Command::new("node")
        .arg(&script)
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        o.status.success(),
        "pci-js.mjs: {}\n{}",
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
fn pci_and_dmac_do_what_the_js_driver_does() {
    if skip("pci_and_dmac_do_what_the_js_driver_does") {
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
    assert!(scenarios >= 19, "{scenarios} scenarios");
    assert!(js.len() > 1900, "{} lines", js.len());
}
