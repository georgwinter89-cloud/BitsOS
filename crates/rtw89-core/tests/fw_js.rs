//! The fw module against the JS driver's own fw.mjs (fw-parse.mjs, fw-h2c.mjs):
//! tests/fake/fw-js.mjs runs test-fw.mjs's scenarios (and more) on the kit's
//! FakeDev and the real firmware file and prints every op, every result and a
//! digest of every DMA memory write; the same scenarios run here on HookBus with
//! the same chip answers, and the two texts must be equal line by line. Needs
//! node and the read-only WLAN tree; RTW89_SKIP_FAKE=1 skips (with a line on
//! stderr), but never with BITSOS_GATE or CI set: a gate run must run the JS oracle.

use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::bus::{Bus, BusError, prep};
use rtw89_core::error::Rtw89Error;
use rtw89_core::fw::hookbus::HookBus;
use rtw89_core::fw::{self, AckKind, C2hReg, Firmware, FwState, H2c, H2cReg, RfRegInfo};
use rtw89_core::membus::MemBus;
use rtw89_core::pci::{self, DmacState, PciRings, RingConfig};
use rtw89_core::tables::fw_feat::FEAT_TBL;
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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS fw.mjs)"
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

fn file() -> Vec<u8> {
    let p = wlan_dir().join("firmware/rtw8852b_fw-2.bin");
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

const FWC: u32 = 0x1e0;
const PE: u32 = 0x88;
const IDX: u32 = R_AX_CH12_TXBD_IDX;

fn hx(v: u32) -> String {
    format!("0x{v:08x}")
}
fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn fnv(b: &[u8]) -> u32 {
    b.iter().fold(0x811c_9dc5u32, |h, &x| {
        (h ^ u32::from(x)).wrapping_mul(0x0100_0193)
    })
}

/// fw-js.mjs `kind`
fn kind(e: &Rtw89Error) -> String {
    let st = |b: &Rtw89Error| b.fw_status().map_or("-".to_string(), |s| s.to_string());
    match e {
        Rtw89Error::FwFile { .. } | Rtw89Error::NoFirmware { .. } | Rtw89Error::FwSecureBoot => {
            "fw".into()
        }
        Rtw89Error::FwPathNotReady { path, .. } => format!("path {path}"),
        Rtw89Error::WcpuAlreadyOn { .. } => "wcpu".into(),
        Rtw89Error::Ch12Stuck { before, .. } => format!("stuck status={}", st(before)),
        Rtw89Error::FwLinkGone { before, .. } => format!("gone status={}", st(before)),
        Rtw89Error::FwStatus { status, .. } => format!("status {status}"),
        Rtw89Error::H2cRegBusy { .. } => "h2creg busy".into(),
        Rtw89Error::C2hRegTimeout { .. } => "c2hreg timeout".into(),
        Rtw89Error::Bus(BusError::PollTimeout { .. } | BusError::Refused { .. })
        | Rtw89Error::Bus(BusError::DmaExhausted { .. })
        | Rtw89Error::RingIndex { .. }
        | Rtw89Error::RingFull { .. } => "device".into(),
        _ => "other".into(),
    }
}

struct Out(Vec<String>);

impl Out {
    /// A scenario without a bus (parsing): what `f` returned, or its error's kind
    fn pure(&mut self, name: &str, f: impl FnOnce() -> Result<Vec<String>, Rtw89Error>) {
        self.0.push(format!("## {name}"));
        match f() {
            Ok(v) => self.0.extend(v),
            Err(e) => self.0.push(format!("! error {}", kind(&e))),
        }
    }
    /// A scenario on a bus: its ops since now, then what `f` returned (or its error's kind)
    fn on(
        &mut self,
        name: &str,
        b: &mut HookBus<'_>,
        f: impl FnOnce(&mut HookBus<'_>) -> Result<Vec<String>, Rtw89Error>,
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
}

fn feat_names(bits: &std::collections::BTreeSet<u32>) -> String {
    let mut n: Vec<&str> = FEAT_TBL
        .iter()
        .filter(|r| bits.contains(&r.feature))
        .map(|r| r.name)
        .collect();
    n.sort_unstable();
    n.dedup();
    n.join(" ")
}

/// fw-js.mjs `parseShow`
fn parse_show(fw: &Firmware) -> Vec<String> {
    let s = &fw.suit;
    let i = &s.info;
    let entry = s.entry.map_or("-".into(), |e| {
        format!("{} {} {} {} {}", e.cv, e.typ, e.mp, e.shift, e.size)
    });
    let mut r = vec![
        format!("= {}", fw.version_text()),
        format!(
            "= entry {entry} type {} hdr {} {} {} {} {} {} {}",
            s.typ,
            s.hdr_ver,
            i.hdr_len,
            i.dynamic_hdr_len,
            i.section_num,
            i.part_size,
            u8::from(i.dynamic_hdr_en),
            i.idmem_share_mode
        ),
    ];
    for x in &i.sections {
        let p: Vec<_> = fw::section_pieces(x, i.part_size).collect();
        r.push(format!(
            "= sec {} {} {} {} {} {} {} {} {} pieces {} {}",
            x.typ,
            x.len,
            x.offset,
            hx(x.dl_addr),
            u8::from(x.checksum),
            u8::from(x.redl),
            x.mssc,
            x.mssc_len,
            u8::from(x.ignore),
            p.len(),
            p.last().map_or(0, |l| l.len)
        ));
    }
    for e in &fw.elements {
        let pl = fw.payload(e);
        r.push(format!(
            "= elm {} {} {} {} {} {} {} {}",
            e.id,
            e.name,
            e.size,
            e.aid,
            hexs(&e.ver),
            e.offset,
            pl.len(),
            hx(fnv(pl))
        ));
    }
    r.push(format!("= feat {}", feat_names(&fw.feat)));
    r.push(format!("= missing {}", fw.missing_elements.join(" ")));
    r
}

#[derive(Clone, Copy)]
struct Synth {
    mssc: u32,
    formatted: bool,
    truncate: usize,
    part: u32,
    dyn_: usize,
    dyn_len: Option<u32>,
}

const SYNTH: Synth = Synth {
    mssc: 1,
    formatted: false,
    truncate: 0,
    part: 2020,
    dyn_: 0,
    dyn_len: None,
};

fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// test-fw.mjs `synth`
fn synth(o: Synth) -> Vec<u8> {
    let dyn_ = o.dyn_;
    let mut hdr = vec![0u8; 64 + dyn_];
    put(&mut hdr, 4, 0x1d1d_0000);
    put(&mut hdr, 12, prep(FW_HDR_W3_LEN, 64 + dyn_ as u32));
    put(&mut hdr, 24, 2 << 8);
    let dynb = if dyn_ != 0 { FW_HDR_W7_DYN_HDR } else { 0 };
    put(&mut hdr, 28, (0x5a << 24) | dynb | o.part);
    if dyn_ != 0 {
        put(&mut hdr, 64, o.dyn_len.unwrap_or(dyn_ as u32));
    }
    put(&mut hdr, 32, 0x1890_0000);
    put(&mut hdr, 36, (2 << 24) | (1 << 28) | 3000);
    put(&mut hdr, 48, 0x18e0_0000);
    put(&mut hdr, 52, (9 << 24) | 100);
    put(&mut hdr, 56, if o.formatted { 0xff } else { o.mssc });
    let sec1 = vec![0x11u8; 3008];
    let mut sec2 = vec![0x22u8; 100];
    let mut sig = vec![0x33u8; o.mssc as usize * 512];
    if o.formatted {
        sig = vec![0x44u8; 32 + 1 + 2 * 512];
        sig[..8].copy_from_slice(b"MSSKPOOL");
        put(&mut sig, 12, 33);
        sig[16] = 0;
        sig[20] = 0;
        sig[21] = 1;
        sig[22..24].copy_from_slice(&2u16.to_le_bytes());
        sig[24..26].copy_from_slice(&1u16.to_le_bytes());
        sig[26..28].copy_from_slice(&8u16.to_le_bytes());
        sec2[60..62].copy_from_slice(&0u16.to_le_bytes()); // key_sign_len 0: 512
    }
    let mut suit = [hdr, sec1, sec2, sig].concat();
    suit.truncate(suit.len() - o.truncate);
    let mut mfw = vec![0u8; 16 + 2 * 16];
    mfw[0] = 0xff;
    mfw[1] = 2;
    for (i, mp) in [(0usize, 1u8), (1, 0)] {
        let at = 16 + i * 16;
        mfw[at] = 1;
        mfw[at + 1] = 5;
        mfw[at + 2] = mp;
        put(&mut mfw, at + 4, 48);
        put(&mut mfw, at + 8, suit.len() as u32);
    }
    [mfw, suit].concat()
}

struct Ready<'a> {
    b: HookBus<'a>,
    rings: PciRings,
    st: FwState,
}

/// test-fw.mjs `ready`
fn ready<'a>() -> Ready<'a> {
    let mut m = MemBus::new();
    m.cfg.insert(0x80, 0x1011_0000);
    let mut b = HookBus::new(m);
    b.set32(R_AX_DMAC_FUNC_EN, B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN)
        .unwrap();
    pci::hci_dma_trx(&mut b, true).unwrap();
    pci::dmac_pre_init(&mut b, &mut DmacState::default()).unwrap();
    let mut rings = pci::pci_alloc(&mut b, &RingConfig::js()).unwrap();
    pci::pre_init(&mut b, &mut rings).unwrap();
    b.writes.clear();
    Ready {
        b,
        rings,
        st: FwState::new(),
    }
}

/// fw-js.mjs `fwShow`
fn fw_show(st: &FwState, ok: bool) -> Vec<String> {
    let tries = if ok { st.tries.to_string() } else { "-".into() };
    let pieces = if st.pieces > 0 {
        st.pieces.to_string()
    } else {
        "-".into()
    };
    let mut v = vec![format!(
        "= fw ready={} tries={tries} pieces={pieces} seq={}",
        u8::from(st.ready),
        st.h2c_seq
    )];
    if let Some(d) = &st.fail_dump {
        let pc: Vec<String> = d.pc.iter().map(|&x| hx(x)).collect();
        v.push(format!(
            "= dump {} {} {}",
            hx(d.fw_ctrl),
            hx(d.boot_dbg),
            pc.join(" ")
        ));
    }
    v
}

fn memw(b: &HookBus<'_>) -> Vec<String> {
    b.writes
        .iter()
        .map(|(a, d)| format!("= memw {} {} {}", hx(*a), d.len(), hx(fnv(d))))
        .collect()
}

fn h2c_bytes(b: &HookBus<'_>, from: usize) -> Vec<String> {
    b.writes[from..]
        .iter()
        .map(|(_, d)| format!("= h2c {}", hexs(&d[24..])))
        .collect()
}

fn c2hreg_show(c: Option<C2hReg>) -> Vec<String> {
    vec![match c {
        Some(c) => {
            let w: Vec<String> = c.w.iter().map(|&x| hx(x)).collect();
            format!("= c2hreg {} {} {}", c.id, c.content_len, w.join(" "))
        }
        None => "= c2hreg none".into(),
    }]
}

fn c2h_show(b: &[u8]) -> Result<Vec<String>, Rtw89Error> {
    let p = fw::c2h_parse(b)?;
    let a = p.ack.map_or("-".to_string(), |a| {
        let k = match a.kind {
            AckKind::Rec => "rec",
            AckKind::Done => "done",
        };
        let ret = a.ret.map_or("-".to_string(), |r| r.to_string());
        format!("{k} {} {} {} {} {ret}", a.cat, a.cls, a.func, a.seq)
    });
    let seq = p.seq.map_or("-".to_string(), |s| s.to_string());
    Ok(vec![format!(
        "= c2h {} {} {} {} {seq} {} ack {a}",
        p.cat,
        p.cls,
        p.func,
        p.len,
        hexs(&p.payload)
    )])
}

/// One failing download scenario (fw-js.mjs `failing`)
fn failing<'a>(
    out: &mut Out,
    name: &str,
    fw: &Firmware,
    on_read: impl FnMut(u32) -> Option<u32> + 'a,
    on_poll: impl FnMut(u32, u32) -> Result<bool, BusError> + 'a,
) {
    let mut d = ready();
    d.b.on_read(on_read);
    d.b.on_poll(on_poll);
    let Ready { b, rings, st } = &mut d;
    out.on(name, b, |b| {
        let r = fw::download(b, rings, fw, st);
        let mut v = fw_show(st, r.is_ok());
        if let Err(e) = r {
            v.push(format!("! error {}", kind(&e)));
        }
        Ok(v)
    });
}

fn rust_side() -> Vec<String> {
    let mut out = Out(Vec::new());
    let f = file();

    // ---- parsing ----
    out.pure("parse real cv 1", || {
        Ok(parse_show(&Firmware::parse(f.clone(), 1, false)?))
    });
    out.pure("parse real cv 2", || {
        Ok(parse_show(&Firmware::parse(f.clone(), 2, false)?))
    });
    out.pure("parse real cv 0", || {
        Ok(parse_show(&Firmware::parse(f.clone(), 0, false)?))
    });
    out.pure("parse secure boot", || {
        Ok(parse_show(&Firmware::parse(f.clone(), 1, true)?))
    });
    let legacy = f[53_072..53_072 + 326_728].to_vec();
    out.pure("legacy", || {
        Ok(vec![format!(
            "= normal {}",
            fw::mfw_recognize(&legacy, RTW89_FW_NORMAL, 1)?.size
        )])
    });
    out.pure("legacy normal ce", || {
        fw::mfw_recognize(&legacy, RTW89_FW_NORMAL_CE, 1).map(|_| vec![])
    });
    out.pure("legacy parse", || {
        Ok(parse_show(&Firmware::parse(legacy.clone(), 1, false)?))
    });
    let real = Firmware::parse(f.clone(), 1, false).unwrap();
    let last = real.elements.last().unwrap();
    out.pure("cut at the last element", || {
        let cut = f[..last.offset + last.size as usize].to_vec();
        Ok(vec![format!(
            "= elements {}",
            Firmware::parse(cut, 1, false)?.elements.len()
        )])
    });
    out.pure("features", || {
        Ok([
            [0, 29, 29, 6],
            [0, 29, 29, 7],
            [0, 29, 29, 255],
            [0, 29, 30, 0],
            [0, 29, 128, 0],
            [0, 29, 130, 0],
            [1, 0, 0, 0],
        ]
        .iter()
        .map(|v| {
            format!(
                "= feat {}.{}.{}.{}: {}",
                v[0],
                v[1],
                v[2],
                v[3],
                feat_names(&fw::features_for(fw::ver_code(v[0], v[1], v[2], v[3])))
            )
        })
        .collect())
    });
    let variants = [
        ("default", SYNTH),
        (
            "formatted",
            Synth {
                formatted: true,
                ..SYNTH
            },
        ),
        ("mssc 3", Synth { mssc: 3, ..SYNTH }),
        (
            "truncate 4",
            Synth {
                truncate: 4,
                ..SYNTH
            },
        ),
        (
            "dynamic header 16/12",
            Synth {
                dyn_: 16,
                dyn_len: Some(12),
                ..SYNTH
            },
        ),
        (
            "part 0x100 dynamic 16",
            Synth {
                part: 0x100,
                dyn_: 16,
                ..SYNTH
            },
        ),
    ];
    for (name, o) in variants {
        out.pure(&format!("synth {name}"), || {
            Ok(parse_show(&Firmware::parse(synth(o), 1, false)?))
        });
    }
    let mut sd = Firmware::parse(
        synth(Synth {
            part: 0x100,
            dyn_: 16,
            ..SYNTH
        }),
        1,
        false,
    )
    .unwrap();
    sd.suit.info.sections[1].len_override = 60;
    let lens = |f: &Firmware| -> String {
        let v: Vec<String> = fw::section_pieces(&f.suit.info.sections[1], 2020)
            .map(|p| p.len.to_string())
            .collect();
        v.join(" ")
    };
    out.pure("header H2C with len_override 60", || {
        Ok(vec![
            format!("= hdr {}", hexs(&fw::fw_hdr_h2c(&FwState::new(), &sd)?)),
            format!("= pieces {}", lens(&sd)),
        ])
    });
    sd.suit.info.sections[1].len_override = 200;
    out.pure("len_override above len", || {
        Ok(vec![format!("= pieces {}", lens(&sd))])
    });
    let s = synth(Synth {
        formatted: true,
        ..SYNTH
    });
    out.pure("cuts of the formatted container", || {
        let res: Vec<String> = (0..=s.len())
            .step_by(7)
            .map(|n| match Firmware::parse(s[..n].to_vec(), 1, false) {
                Ok(fw) => format!("{n}:{}", fw.elements.len()),
                Err(_) => format!("{n}:e"),
            })
            .collect();
        Ok(vec![format!("= {}", res.join(" "))])
    });

    // ---- the download ----
    {
        let mut d = ready();
        let Ready { b, rings, st } = &mut d;
        out.on("download", b, |b| {
            fw::download(b, rings, &real, st)?;
            Ok([fw_show(st, true), memw(b)].concat())
        });
    }
    failing(
        &mut out,
        "status 2 every try",
        &real,
        |a| (a == FWC).then_some(0x40),
        |a, m| Ok(!(a == FWC && m == 0xe0)),
    );
    failing(
        &mut out,
        "h2c path never ready",
        &real,
        |_| None,
        |a, m| Ok(!(a == FWC && m == 0x2)),
    );
    failing(
        &mut out,
        "wcpu already on",
        &real,
        |a| (a == PE).then_some(3),
        |_, _| Ok(true),
    );
    failing(
        &mut out,
        "lost connection",
        &real,
        |_| None,
        |a, m| {
            if a == FWC && m == 0x2 {
                Err(BusError::Gone)
            } else {
                Ok(true)
            }
        },
    );
    {
        let (rdy, pc) = (Cell::new(0), Cell::new(0u32));
        failing(
            &mut out,
            "one bad try",
            &real,
            |a| match a {
                FWC => Some(0x1234_0040),
                R_AX_BOOT_DBG => Some(0xb007_0000),
                R_AX_DBG_PORT_SEL => {
                    pc.set(pc.get() + 1);
                    Some(0x1000 + pc.get() - 1)
                }
                _ => None,
            },
            |a, m| {
                if a == FWC && m == 0xe0 {
                    rdy.set(rdy.get() + 1);
                    return Ok(rdy.get() != 1);
                }
                Ok(true)
            },
        );
    }
    {
        let n = Cell::new(0);
        failing(
            &mut out,
            "ch12 timeout at poll 30",
            &real,
            |_| None,
            |a, _| {
                if a == IDX {
                    n.set(n.get() + 1);
                }
                Ok(!(a == IDX && n.get() == 30))
            },
        );
    }
    {
        let n = Cell::new(0);
        failing(
            &mut out,
            "ch12 stuck",
            &real,
            |_| None,
            |a, _| {
                if a == IDX {
                    n.set(n.get() + 1);
                }
                Ok(!(a == IDX && n.get() >= 30))
            },
        );
    }
    failing(
        &mut out,
        "ch12 all ones",
        &real,
        |a| (a == IDX).then_some(0xffff_ffff),
        |a, m| Ok(!(a == FWC && m == 0x2)),
    );
    {
        // the first status poll fails and the read after it says 4 (cut mismatch)
        let (k, f) = (Cell::new(0), Cell::new(false));
        failing(
            &mut out,
            "status 4 (cut mismatch), then fixed",
            &real,
            |a| (a == FWC && f.replace(false)).then_some(0x80),
            |a, m| {
                if a == FWC && m == 0xe0 {
                    k.set(k.get() + 1);
                    if k.get() == 1 {
                        f.set(true);
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        );
    }

    // ---- H2Cs after the download ----
    {
        let mut d = ready();
        fw::download(&mut d.b, &mut d.rings, &real, &mut d.st).unwrap();
        let from = d.b.writes.len();
        let Ready { b, rings, st } = &mut d;
        out.on("start-up h2cs", b, |b| {
            st.early_h2c.push(vec![0x01, 0, 0, 0x01, 0x08, 0, 0, 0]);
            fw::send_all_early_h2c(b, rings, st)?;
            fw::h2c_set_ofld_cfg(b, rings, st)?;
            fw::h2c_fw_log(b, rings, st, false)?;
            fw::h2c_fw_log(b, rings, st, true)?;
            let mut info = RfRegInfo::new(RF_PATH_B);
            info.store(0x10000, 0x12345);
            info.store(0x1001f, 0xfffff);
            let up = fw::rf_reg_upload(b, rings, st, &mut info)?;
            let mut a = RfRegInfo::new(RF_PATH_A);
            for i in 0..700u32 {
                a.store(0x100 + (i % 0x400), i * 37);
            }
            let up2 = fw::rf_reg_upload(b, rings, st, &mut a)?;
            Ok([
                vec![format!(
                    "= up {up} {up2} {} {} seq {}",
                    info.curr_idx, a.curr_idx, st.h2c_seq
                )],
                h2c_bytes(b, from),
            ]
            .concat())
        });
        let from = b.writes.len();
        out.on("thirty h2cs", b, |b| {
            for i in 0..30u32 {
                let h = H2c {
                    cat: 1,
                    cls: 0x10 + (i % 3),
                    func: i,
                    rack: i % 5 == 0,
                    dack: i % 7 == 0,
                };
                fw::h2c_send(b, rings, st, &h, &vec![i as u8; ((i * 13) % 40) as usize])?;
            }
            Ok([vec![format!("= seq {}", st.h2c_seq)], h2c_bytes(b, from)].concat())
        });
    }

    // ---- register path ----
    {
        let mut m = HookBus::new(MemBus::new());
        m.on_read(|a| match a {
            0x8164 => Some(1),
            0x8150 => Some(0x0011_0403),
            0x8154 => Some(0xabcd),
            _ => None,
        });
        let mut st = FwState::new();
        let h = |id, content_len, w| H2cReg { id, content_len, w };
        out.on("msg reg", &mut m, |b| {
            Ok(c2hreg_show(fw::msg_reg(
                b,
                &mut st,
                Some(&h(3, 0, [2 << 16, 0, 0, 0])),
                Some(0),
            )?))
        });
        out.on("msg reg 9 bytes", &mut m, |b| {
            Ok(c2hreg_show(fw::msg_reg(
                b,
                &mut st,
                Some(&h(0x45, 9, [0xffff_ffff, 1, 2, 3])),
                Some(0),
            )?))
        });
        out.on("msg reg without answer", &mut m, |b| {
            Ok(c2hreg_show(fw::msg_reg(
                b,
                &mut st,
                Some(&h(5, 2, [0x10000, 0, 0, 0])),
                None,
            )?))
        });
        out.on("msg reg counters", &mut m, |_| {
            Ok(vec![format!(
                "= counters {} {}",
                st.h2c_counter, st.c2h_counter
            )])
        });
        let mut busy = HookBus::new(MemBus::new());
        busy.on_poll(|a, _| Ok(a != 0x8160));
        let mut st = FwState::new();
        out.on("h2creg busy", &mut busy, |b| {
            fw::msg_reg(
                b,
                &mut st,
                Some(&H2cReg {
                    id: 3,
                    ..H2cReg::default()
                }),
                Some(0),
            )
            .map(|_| vec![])
        });
    }

    // ---- C2H ----
    let mut c2 = vec![0u8; 12];
    put(&mut c2, 0, (1 << 8) | 1);
    put(&mut c2, 4, 12);
    put(&mut c2, 8, (5 << 24) | (0x14 << 8) | (9 << 2) | 1);
    out.pure("c2h done ack", || c2h_show(&c2));
    let mut r = c2.clone();
    put(&mut r, 0, 1);
    put(&mut r, 8, (7 << 16) | (0x14 << 8) | (9 << 2) | 1);
    out.pure("c2h receive ack", || c2h_show(&r));
    out.pure("c2h longer buffer", || {
        c2h_show(&[r.clone(), vec![0xee; 8]].concat())
    });
    let mut o = r.clone();
    put(&mut o, 0, (1 << 2) | 1);
    out.pure("c2h other class", || c2h_show(&o));
    out.pure("c2h 9 bytes", || c2h_show(&o[..9]));
    out.pure("c2h 7 bytes", || c2h_show(&o[..7]));
    let mut big = vec![0x5au8; 40];
    put(&mut big, 0, (3 << 8) | (2 << 2) | 2);
    put(&mut big, 4, 30);
    out.pure("c2h outsrc", || c2h_show(&big));
    out.0
}

fn js_side() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/fw-js.mjs");
    let o = Command::new("node")
        .arg(&script)
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
    assert!(
        o.status.success(),
        "fw-js.mjs: {}\n{}",
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
fn fw_does_what_the_js_driver_does() {
    if skip("fw_does_what_the_js_driver_does") {
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
    assert!(scenarios >= 40, "{scenarios} scenarios");
    assert!(js.len() > 9000, "{} lines", js.len());
}
