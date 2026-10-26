//! The pcitx module against the JS driver's own js/pcitx.mjs: tests/fake/pcitx-js.mjs
//! runs tests/fake/pcitx-scenarios.txt (test-pcitx.mjs's checks in its order, then
//! more) on the kit's FakeDev and prints every op and result; this file interprets
//! the same scenarios on MemBus and the two texts must be equal line by line. Needs
//! node and the read-only WLAN tree; RTW89_SKIP_FAKE=1 skips (with a line on stderr),
//! but never with BITSOS_GATE or CI set: a gate run must run the JS oracle.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::bus::{Bus, prep};
use rtw89_core::error::Rtw89Result;
use rtw89_core::membus::MemBus;
use rtw89_core::pci::{self, PciRings, RingConfig, TxDescInfo};
use rtw89_core::pcitx::fakechip::{TxChip, data_info, mgmt_info, rpp};
use rtw89_core::pcitx::{PciTx, TxEvent};
use rtw89_core::tables::regs::*;

fn skip(test: &str) -> bool {
    use std::io::Write;
    if std::env::var_os("RTW89_SKIP_FAKE").is_some_and(|v| v == "1") {
        for gate in ["BITSOS_GATE", "CI"] {
            assert!(
                std::env::var_os(gate).is_none_or(|v| v.is_empty() || v == "0"),
                "RTW89_SKIP_FAKE=1 together with {gate}: the gate must run the JS oracle"
            );
        }
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS pcitx.mjs)"
        );
        return true;
    }
    false
}

fn scenarios() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/pcitx-scenarios.txt")
}

fn num(s: &str) -> u32 {
    match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16).unwrap(),
        None => s.parse().unwrap(),
    }
}

/// The scenario's TX info (pcitx-js.mjs `info`)
fn info(kind: &str, len: usize, mac_id: u32, seq: u32) -> TxDescInfo {
    let k: Vec<&str> = kind.split(':').collect();
    match k[0] {
        "h2c" => TxDescInfo {
            ch_dma: RTW89_DMA_H2C,
            pkt_size: len as u32,
            ..TxDescInfo::default()
        },
        "mgmt" => mgmt_info(len, mac_id, seq),
        "data" => data_info(num(k[1]), len, mac_id, seq),
        "nowd" => TxDescInfo {
            wd_info: None,
            ..data_info(num(k[1]), len, mac_id, seq)
        },
        "qsel" => TxDescInfo {
            qsel: num(k[2]),
            ..data_info(num(k[1]), len, mac_id, seq)
        },
        _ => panic!("unknown kind {kind}"),
    }
}

fn frame(n: usize, seq: u32) -> Vec<u8> {
    (0..n).map(|i| (seq as usize * 7 + i) as u8).collect()
}

/// pcitx-js.mjs `rpqBuf`
fn rpq_buf(
    rpps: &[u32],
    flags: u32,
    long: bool,
    drv: u32,
    size: Option<u32>,
    pad: usize,
) -> Vec<u8> {
    let n = rpps.len() as u32;
    let d0 = 0x0700_0000
        | (4 * n)
        | if long { AX_RXD_LONG_RXD } else { 0 }
        | prep(AX_RXD_DRV_INFO_SIZE_MASK, drv);
    let mut body = vec![0u8; if long { 32 } else { 16 }];
    body[..4].copy_from_slice(&d0.to_le_bytes());
    body.extend(std::iter::repeat_n(0, drv as usize * 8));
    for r in rpps {
        body.extend_from_slice(&r.to_le_bytes());
    }
    let ws = size.unwrap_or(4 + body.len() as u32);
    let mut all = (flags | prep(RTW89_PCI_RXBD_WRITE_SIZE, ws))
        .to_le_bytes()
        .to_vec();
    all.extend(body);
    if all.len() < pad {
        all.resize(pad, 0);
    }
    all
}

struct Run {
    out: Vec<String>,
    d: MemBus,
    rings: PciRings,
    tx: PciTx,
    chip: TxChip,
    mark: usize,
}

impl Run {
    fn ch(&self, name: &str) -> u32 {
        u32::from(self.rings.tx(name).unwrap().ch)
    }
    fn idx(&self, name: &str) -> u32 {
        self.rings
            .tx(name)
            .map(|r| r.idx)
            .or_else(|| self.rings.rx(name).map(|r| r.idx))
            .unwrap()
    }
    fn rpq_slot(&self, e: &str) -> u32 {
        let rpq = self.rings.rx("RPQ").unwrap();
        match e.strip_prefix("rpq+") {
            Some(k) => (rpq.wp + num(k)) % rpq.len,
            None => num(e),
        }
    }
    fn value(&self, v: &str) -> u32 {
        match v.strip_prefix("hw:") {
            Some(e) => self.rpq_slot(e) << 16,
            None => num(v),
        }
    }
    fn mem(&mut self, at: u32, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.d.mem_read_bytes(at, &mut v).unwrap();
        v
    }

    fn run(&mut self, cmd: &str, i: u32) -> Rtw89Result<()> {
        let t: Vec<String> = cmd
            .split_whitespace()
            .map(|x| x.replace("$i", &i.to_string()))
            .collect();
        let opt = |k: &str| {
            t.iter()
                .find_map(|x| x.strip_prefix(&format!("{k}=")).map(String::from))
        };
        let has = |k: &str| t.iter().any(|x| x == k);
        match t[0].as_str() {
            "every" => {
                if i % num(&t[1]) == num(&t[2]) {
                    self.run(&t[3..].join(" "), i)?;
                }
            }
            "new" => {
                self.d = MemBus::new();
                self.rings = pci::pci_alloc(&mut self.d, &RingConfig::js())?;
                self.tx = PciTx::new();
                self.tx.init(&self.rings);
                self.mark = self.d.trace.len();
            }
            "chipnew" => self.chip = TxChip::new(&mut self.d, &self.rings),
            "chip" => self.chip.run(&mut self.d, &self.rings),
            "tx" => {
                let (len, seq) = (num(&t[3]) as usize, num(&t[5]));
                let extra = opt("extra").map_or(0, |v| num(&v)) as usize;
                let ch = self.ch(&t[1]);
                let inf = info(&t[2], len, num(&t[4]), seq);
                let f = frame(len + extra, seq);
                let r = self
                    .tx
                    .tx(&mut self.d, &mut self.rings, ch, &inf, &f, !has("nokick"))?;
                self.out.push(match r {
                    Some(q) => format!("= tx {} {} {}", q.ch, q.seq, q.wp),
                    None => "= tx null".into(),
                });
            }
            "txframe" => {
                let (len, seq) = (num(&t[2]) as usize, num(&t[3]));
                let inf = info(&t[1], len, 0, seq);
                let mut q = false;
                if t[1] == "mgmt" || self.tx.resource(&mut self.d, &mut self.rings, inf.ch_dma)? > 0
                {
                    q = self
                        .tx
                        .tx(
                            &mut self.d,
                            &mut self.rings,
                            inf.ch_dma,
                            &inf,
                            &frame(len, seq),
                            true,
                        )?
                        .is_some();
                }
                self.out.push(format!("= queued {}", u8::from(q)));
            }
            "kick" => {
                let ch = self.ch(&t[1]);
                self.tx.kick(&mut self.d, &self.rings, ch)?;
            }
            "resource" => {
                let ch = self.ch(&t[1]);
                let n = self.tx.resource(&mut self.d, &mut self.rings, ch)?;
                self.out.push(format!("= res {n}"));
            }
            "reclaim" => {
                for x in self.tx.reclaim(&mut self.d, &mut self.rings)? {
                    self.out.push(format!(
                        "= rep {} {} {} {} {} {} {} {}",
                        x.ch,
                        x.seq,
                        x.tx_status,
                        u8::from(x.acked),
                        x.len,
                        x.qsel,
                        x.mac_id,
                        x.wlan_seq
                    ));
                }
            }
            "init" => self.tx.init(&self.rings),
            "force" => {
                let (a, v) = (self.idx(&t[1]), self.value(&t[2]));
                self.d.forced.insert(a, v);
            }
            "rpqbuf" => {
                let slot = self.rpq_slot(&t[1]);
                let rpps: Vec<u32> = if t[3] == "-" {
                    Vec::new()
                } else {
                    t[3].split(',')
                        .map(|x| {
                            let v: Vec<u32> = x.split(':').map(num).collect();
                            rpp(v[0], v[1], v[2])
                        })
                        .collect()
                };
                let b = rpq_buf(
                    &rpps,
                    num(&t[2]),
                    has("long"),
                    opt("drv").map_or(0, |v| num(&v)),
                    opt("size").map(|v| num(&v)),
                    opt("pad").map_or(0, |v| num(&v)) as usize,
                );
                let at = self.rings.rx("RPQ").unwrap().bufs[slot as usize];
                self.d.mem_write_bytes(at, &b).unwrap();
            }
            "page" => {
                let p = self.tx.wd_ring(&t[1]).unwrap().pages[num(&t[2]) as usize].clone();
                let pg = self.mem(p.addr, p.len as usize);
                let fr = self.mem(p.addr + 128, 16);
                self.out.push(format!("= page {}", hex(&pg)));
                self.out.push(format!("= frame {}", hex(&fr)));
            }
            "wd" => {
                let w = self.tx.wd_ring(&t[1]).unwrap();
                let r = self.rings.tx(&t[1]).unwrap();
                let list = |v: Vec<u32>| v.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                self.out.push(format!(
                    "= wd {} free={} busy={} wp={} rp={}",
                    t[1],
                    list(w.free_seqs()),
                    list(w.busy_seqs()),
                    r.wp,
                    r.rp
                ));
            }
            "rpq" => {
                let r = self.rings.rx("RPQ").unwrap();
                self.out.push(format!("= rpq {} {}", r.wp, r.rp));
            }
            "kicks" => {
                let a = self.idx(&t[1]);
                let ks: Vec<u32> = self
                    .d
                    .lines()
                    .iter()
                    .filter_map(|l| {
                        let w: Vec<&str> = l.split(' ').collect();
                        (w[0] == "wr16" && num(w[1]) == a).then(|| num(w[3]))
                    })
                    .collect();
                self.out.push(format!(
                    "= kicks {} {} {}",
                    t[1],
                    ks.len(),
                    ks.iter().max().unwrap()
                ));
            }
            other => panic!("unknown command {other}"),
        }
        Ok(())
    }

    // one command: its ops, then its results, then its events as the JS log lines
    fn step(&mut self, cmd: &str, i: u32) {
        let at = self.out.len();
        if self.run(cmd, i).is_err() {
            self.out.push("! error".into());
        }
        let lines = self.d.lines();
        let ops: Vec<String> = lines[self.mark..].to_vec();
        self.mark = lines.len();
        self.out.splice(at..at, ops);
        for e in self.tx.take_events() {
            // Linux's "Cannot map qsel" warning the JS left out, and the Rust-only faults
            if !matches!(e, TxEvent::QselUnmapped { .. } | TxEvent::Fault(_)) {
                self.out.push(format!("= log {e}"));
            }
        }
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn rust_side() -> Vec<String> {
    let mut r = Run {
        out: Vec::new(),
        d: MemBus::new(),
        rings: PciRings::default(),
        tx: PciTx::new(),
        chip: TxChip::default(),
        mark: 0,
    };
    let text = std::fs::read_to_string(scenarios()).unwrap();
    for raw in text.lines() {
        let l = raw.trim();
        if l.is_empty() || (l.starts_with('#') && !l.starts_with("## ")) {
            continue;
        }
        if l.starts_with("## ") {
            r.out.push(l.into());
            continue;
        }
        let (n, body) = match l.strip_prefix("repeat ") {
            Some(rest) => {
                let (n, b) = rest.split_once(' ').unwrap();
                (num(n), b)
            }
            None => (1, l),
        };
        for i in 0..n {
            for c in body.split(';') {
                r.step(c, i);
            }
        }
    }
    r.out
}

fn js_side() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/pcitx-js.mjs");
    let o = Command::new("node")
        .arg(&script)
        .arg(scenarios())
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        o.status.success(),
        "pcitx-js.mjs: {}\n{}",
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
fn pcitx_does_what_the_js_driver_does() {
    if skip("pcitx_does_what_the_js_driver_does") {
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
    assert!(scenarios >= 12, "{scenarios} scenarios");
    assert!(js.len() > 1900, "{} lines", js.len());
    for must in [
        "= log queue 2 txwd 0 is not idle",
        "= log no available TXBD on CH8",
        "! error",
    ] {
        assert!(js.iter().any(|l| l == must), "the scenarios reach {must}");
    }
}
