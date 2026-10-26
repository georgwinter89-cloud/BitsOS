//! The station's H2Cs against the C (js/test-sta.mjs sections 2 and 3 ported), reading the Linux
//! rtw89 sources the JS driver was ported from (WZ:linux, v7.3-rc5 a74306e2e676, read only):
//! the CMAC table fields against fw.h's SET_CMC_TBL_* / SET_CTRL_INFO_* setters, the dword masks
//! of the packet offload / general packet / packet drop setters, every builder's category, class,
//! function, rack and dack against the rtw89_h2c_pkt_set_hdr call of its Linux function; then
//! every `name (file.c:line)` citation in src/sta/ against that line of WZ:linux, and every
//! `name (linux-6.6/path:line)` citation in src/sta/ and crates/wifi against the Linux 6.6 tree
//! (V2/ref/linux-6.6, read only). No node, no fake chip.

use std::fs;
use std::path::{Path, PathBuf};

use rtw89_core::sta::h2c::{self, Cmc, GeneralPkts, H2cMsg, PktDrop};
use rtw89_core::sta::ra::RaInfo;
use rtw89_core::tables::regs::*;

fn wz() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/georg/BitsOS/werkzeuge/plattform/wlan"))
}

fn linux66() -> PathBuf {
    std::env::var_os("LINUX_6_6")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/georg/BitsOS/V2/ref/linux-6.6"))
}

fn read(p: &Path) -> String {
    let b = fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    String::from_utf8_lossy(&b).into_owned()
}

/// The body of `void <name>(void *<arg>, u32 val) { ... }` in fw.h
fn setter<'a>(fwh: &'a str, name: &str) -> &'a str {
    let at = fwh
        .find(&format!("void {name}("))
        .unwrap_or_else(|| panic!("{name} not in fw.h"));
    let body = &fwh[at..];
    let open = body.find('{').unwrap();
    let close = body.find('}').unwrap();
    &body[open + 1..close]
}

/// GENMASK(h, l) or BIT(n) in a setter body
fn mask_of(body: &str) -> u32 {
    if let Some(g) = body.find("GENMASK(") {
        let r = &body[g + 8..];
        let end = r.find(')').unwrap();
        let v: Vec<u32> = r[..end]
            .split(',')
            .map(|x| x.trim().parse().unwrap())
            .collect();
        return ((1u64 << (v[0] + 1)) - (1u64 << v[1])) as u32;
    }
    let b = body.find("BIT(").unwrap();
    let r = &body[b + 4..];
    let end = r.find(')').unwrap();
    1u32 << r[..end].trim().parse::<u32>().unwrap()
}

/// The dword a setter writes: `+ N, val` before the value (0 when none)
fn dword_of(body: &str) -> usize {
    let Some(v) = body.find(", val") else {
        return 0;
    };
    let head = &body[..v];
    let Some(p) = head.rfind("+ ") else { return 0 };
    head[p + 2..].trim().parse().unwrap_or(0)
}

#[test]
fn cmc_fields_as_fw_h_sets_them() {
    let fwh = read(&wz().join("linux/fw.h"));
    for f in Cmc::ALL {
        let body = setter(&fwh, f.name());
        let (w, mask) = f.place();
        assert_eq!(
            (w, mask),
            (dword_of(body.split(';').next().unwrap()), mask_of(body)),
            "{} layout",
            f.name()
        );
        if w != 0 {
            assert!(
                body.contains(&format!("(table) + {}, SET_CMC_TBL_MASK_", w + 8)),
                "{}: the write mask dword",
                f.name()
            );
        }
    }
    for (name, want) in [
        ("SET_GENERAL_PKT_MACID", (0, 0xff)),
        ("SET_GENERAL_PKT_PROBRSP_ID", (0, 0xff00)),
        ("SET_GENERAL_PKT_PSPOLL_ID", (0, 0xff_0000)),
        ("SET_GENERAL_PKT_NULL_ID", (0, 0xff00_0000)),
        ("SET_GENERAL_PKT_QOS_NULL_ID", (1, 0xff)),
        ("SET_GENERAL_PKT_CTS2SELF_ID", (1, 0xff00)),
        ("RTW89_SET_FWCMD_PKT_DROP_SEL", (0, 0xff)),
        ("RTW89_SET_FWCMD_PKT_DROP_MACID", (0, 0xff00)),
        ("RTW89_SET_FWCMD_PKT_DROP_BAND", (0, 0xff_0000)),
        ("RTW89_SET_FWCMD_PKT_DROP_PORT", (0, 0xff00_0000)),
        ("RTW89_SET_FWCMD_PKT_DROP_MBSSID", (1, 0xff)),
        ("RTW89_SET_FWCMD_PKT_DROP_ROLE_A_INFO_TF_TRS", (1, 0xff00)),
        (
            "RTW89_SET_FWCMD_PACKET_OFLD_PKT_IDX",
            (0, h2c::pkt_ofld::IDX),
        ),
        ("RTW89_SET_FWCMD_PACKET_OFLD_PKT_OP", (0, h2c::pkt_ofld::OP)),
        (
            "RTW89_SET_FWCMD_PACKET_OFLD_PKT_LENGTH",
            (0, h2c::pkt_ofld::LENGTH),
        ),
    ] {
        let body = setter(&fwh, name);
        assert_eq!((dword_of(body), mask_of(body)), want, "{name}");
    }
    // the builders put them there
    let gp = h2c::h2c_general_pkt(
        7,
        &GeneralPkts {
            ps_poll: Some(1),
            null_data: None,
            qos_null: Some(3),
        },
    );
    assert_eq!(
        gp.payload,
        [7, 0xff, 1, 0xff, 3, 0xff],
        "general packet: MAC ID, ids, 0xff for none"
    );
    let d = h2c::h2c_pkt_drop(&PktDrop {
        sel: 1,
        macid: 2,
        band: 0,
        port: 3,
        mbssid: 4,
        tf_trs: 5,
    });
    assert_eq!(d.payload[..8], [1, 2, 0, 3, 4, 5, 0, 0]);
}

/// The text of `int <name>(...) { ... }` (or `static int`) in a C file
fn c_func(src: &str, name: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start_matches("static ");
        if t.starts_with(&format!("int {name}(")) {
            let end = (i..lines.len())
                .find(|&k| lines[k].starts_with('}'))
                .unwrap();
            return lines[i..=end].join("\n");
        }
    }
    panic!("{name} not found")
}

/// cat, cls, func, rack, dack of the function's rtw89_h2c_pkt_set_hdr call
fn hdr_call(body: &str) -> (String, String, String, bool, bool) {
    let at = body
        .find("rtw89_h2c_pkt_set_hdr(rtwdev, skb,")
        .expect("header call");
    let args: Vec<String> = body[at..]
        .split('(')
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap()
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    // rtwdev, skb, FWCMD_TYPE_H2C, cat, cls, func, rack, dack, len
    (
        args[3].clone(),
        args[4].clone(),
        args[5].clone(),
        args[6] == "1",
        args[7] == "1",
    )
}

fn reg(name: &str) -> u32 {
    let n = match name {
        "chip->h2c_cctl_func_id" => "H2C_FUNC_MAC_CCTLINFO_UD", // 8852B
        "h2c_macid_pause_id" => "H2C_FUNC_MAC_MACID_PAUSE",
        n => n,
    };
    rtw89_core::tables::reg_value(n).unwrap_or_else(|| panic!("{n} unknown")) as u32
}

#[test]
fn h2c_header_bits_as_linux_sets_them() {
    let fwc = read(&wz().join("linux/fw.c"));
    let camc = read(&wz().join("linux/cam.c"));
    let mut v = rtw89_core::sta::Sta::new().vif;
    v.wifi_role = RTW89_WIFI_ROLE_STATION;
    let ra = RaInfo::default();
    let cases: Vec<(&str, &str, H2cMsg)> = vec![
        ("rtw89_fw_h2c_cam", &fwc, h2c::h2c_addr_cam(&v, None, None)),
        (
            "rtw89_fw_h2c_role_maintain",
            &fwc,
            h2c::h2c_role_maintain(&v, None, 0),
        ),
        (
            "rtw89_fw_h2c_join_info",
            &fwc,
            h2c::h2c_join_info(&v, None, false),
        ),
        (
            "rtw89_fw_h2c_macid_pause",
            &fwc,
            h2c::h2c_macid_pause(0, 0, true),
        ),
        ("rtw89_fw_h2c_set_edca", &fwc, h2c::h2c_set_edca(&v, 0, 0)),
        (
            "rtw89_fw_h2c_default_cmac_tbl",
            &fwc,
            h2c::h2c_default_cmac_tbl(&v, None, 0),
        ),
        (
            "rtw89_fw_h2c_assoc_cmac_tbl",
            &fwc,
            h2c::h2c_assoc_cmac_tbl(&v, None, true),
        ),
        (
            "rtw89_fw_h2c_txpath_cmac_tbl",
            &fwc,
            h2c::h2c_txpath_cmac_tbl(&ap(), RF_A),
        ),
        (
            "rtw89_fw_h2c_tp_offload",
            &fwc,
            h2c::h2c_tp_offload(0, 0, 0),
        ),
        ("rtw89_fw_h2c_ra", &fwc, h2c::h2c_ra(&ra)),
        (
            "__rtw89_fw_h2c_add_pkt_offload",
            &fwc,
            h2c::h2c_pkt_ofld_add(1, &[0; 8]),
        ),
        (
            "rtw89_fw_h2c_del_pkt_offload",
            &fwc,
            h2c::h2c_pkt_ofld_del(1),
        ),
        (
            "rtw89_fw_h2c_general_pkt",
            &fwc,
            h2c::h2c_general_pkt(0, &GeneralPkts::default()),
        ),
        (
            "rtw89_fw_h2c_set_bcn_fltr_cfg",
            &fwc,
            h2c::h2c_bcn_fltr_cfg(&v, true),
        ),
        (
            "rtw89_fw_h2c_rssi_offload",
            &fwc,
            h2c::h2c_rssi_offload(0, 0),
        ),
        (
            "rtw89_fw_h2c_pkt_drop",
            &fwc,
            h2c::h2c_pkt_drop(&PktDrop {
                sel: 0,
                macid: 0,
                band: 0,
                port: 0,
                mbssid: 0,
                tf_trs: 0,
            }),
        ),
        (
            "rtw89_cam_send_sec_key_cmd",
            &camc,
            h2c::h2c_sec_cam(vec![0; 24]),
        ),
    ];
    for (f, src, m) in cases {
        let (cat, cls, func, rack, dack) = hdr_call(&c_func(src, f));
        assert_eq!(
            (m.h.cat, m.h.cls, m.h.func, m.h.rack, m.h.dack),
            (reg(&cat), reg(&cls), reg(&func), rack, dack),
            "{f}: category, class, function, rack, dack"
        );
    }
}

fn ap() -> rtw89_core::sta::ApLink {
    let mut s = rtw89_core::sta::Sta::new();
    s.ap = None;
    rtw89_core::sta::ApLink {
        mac_id: 0,
        addr: [0; 6],
        avg_rssi: Default::default(),
        rssi: Default::default(),
        supp_rates: 0,
        rx_nss: 1,
        ra: None,
        er_cap: false,
        assoc_link: false,
        state: rtw89_core::sta::ApState::None,
        dup: wifi::replay::DupCheck::new(),
        rx_hw_rate: 0,
        prev_rssi: 0,
    }
}

/// `name (file.ext:line)` citations of a source text
fn citations(text: &str) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(p) = text[i..].find(" (") {
        let at = i + p;
        i = at + 2;
        let close = match text[i..].find(')') {
            Some(c) => i + c,
            None => break,
        };
        let inside = &text[i..close];
        let Some((file, line)) = inside.rsplit_once(':') else {
            continue;
        };
        let Ok(line) = line.parse::<usize>() else {
            continue;
        };
        if file.contains(' ') || !(file.ends_with(".c") || file.ends_with(".h")) {
            continue;
        }
        let mut s = at;
        while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_') {
            s -= 1;
        }
        let name = &text[s..at];
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), file.to_string(), line));
    }
    out
}

fn check_tree(sources: &[PathBuf], base_wz: &Path, base_66: &Path) -> (usize, usize) {
    let (mut wz_n, mut l66_n) = (0, 0);
    let mut bad = Vec::new();
    for p in sources {
        let text = read(p);
        for (name, file, line) in citations(&text) {
            let (base, rel) = match file.strip_prefix("linux-6.6/") {
                Some(r) => (base_66, r.to_string()),
                None => (base_wz, format!("linux/{file}")),
            };
            let path = base.join(&rel);
            if !path.exists() {
                bad.push(format!(
                    "{}: {name} ({file}:{line}): no such file",
                    p.display()
                ));
                continue;
            }
            let src = read(&path);
            let lines: Vec<&str> = src.lines().collect();
            let ok = (line.saturating_sub(2)..=line + 1).any(|k| {
                lines
                    .get(k.saturating_sub(1))
                    .is_some_and(|l| l.contains(&name))
            });
            if !ok {
                bad.push(format!("{}: {name} not at {file}:{line}", p.display()));
            }
            if base == base_66 {
                l66_n += 1;
            } else {
                wz_n += 1;
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} wrong citations:\n{}",
        bad.len(),
        bad.join("\n")
    );
    (wz_n, l66_n)
}

fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    v.sort();
    v
}

#[test]
fn every_linux_citation_points_at_its_function() {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = rs_files(&here.join("src/sta"));
    files.extend(rs_files(&here.join("../wifi/src")));
    let (wz_n, l66_n) = check_tree(&files, &wz(), &linux66());
    assert!(wz_n >= 80, "rtw89 citations checked: {wz_n}");
    assert!(l66_n >= 10, "linux-6.6 citations checked: {l66_n}");
    eprintln!("{wz_n} rtw89 citations (WZ:linux) and {l66_n} linux-6.6 citations checked");
}

#[test]
fn every_station_function_names_its_linux_original() {
    // test-sta.mjs section 3: each exported function has a `// Linux:` line above it
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut bad = Vec::new();
    for p in rs_files(&here.join("src/sta")) {
        if p.ends_with("tests.rs") {
            continue;
        }
        let text = read(&p);
        let lines: Vec<&str> = text.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            if !l.starts_with("pub fn ") {
                continue;
            }
            let mut j = i;
            let mut found = false;
            while j > 0 {
                j -= 1;
                let t = lines[j].trim_start();
                if t.starts_with("// Linux:") {
                    found = true;
                    break;
                }
                if !(t.starts_with("//") || t.starts_with("#[")) {
                    break;
                }
            }
            if !found {
                bad.push(format!("{}:{} {}", p.display(), i + 1, l));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "functions without a `// Linux:` line:\n{}",
        bad.join("\n")
    );
}
