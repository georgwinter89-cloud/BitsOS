//! The txrx module against the JS driver's own js/txrx.mjs: tests/fake/txrx-js.mjs generates
//! seeded inputs (TX requests, sequence numbers, the station's frames, data frames both ways,
//! replays, the parsers, RX buffers under several states and chip ops), prints each as a `> `
//! command and what txrx.mjs answered as `= ` lines. This file runs every command on the Rust
//! side and prints the same lines; the texts must be equal, line by line. Needs node and the
//! read-only WLAN tree; RTW89_SKIP_FAKE=1 skips (with a line on stderr), never with
//! BITSOS_GATE or CI set.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::fw::C2h;
use rtw89_core::pci::fill_txdesc;
use rtw89_core::phy::{Band, Phy};
use rtw89_core::txrx::ieee::{self, Elements, Hdr, Mac};
use rtw89_core::txrx::parse::{self, Beacon, MgmtKind, Rsn};
use rtw89_core::txrx::*;

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
            "SKIPPED (RTW89_SKIP_FAKE=1): {test} (JS txrx.mjs)"
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

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    if b.is_empty() {
        return "-".into();
    }
    b.iter().map(|v| format!("{v:02x}")).collect()
}

fn mac(s: &str) -> Mac {
    unhex(s).try_into().unwrap()
}

fn b(v: bool) -> &'static str {
    if v { "1" } else { "0" }
}

fn o<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("-".into(), |x| x.to_string())
}

fn band(s: &str) -> Band {
    match s {
        "2g" => Band::B2g,
        "5g" => Band::B5g,
        "6g" => Band::B6g,
        _ => panic!("band {s}"),
    }
}

fn band_str(b: Option<Band>) -> &'static str {
    match b {
        Some(Band::B2g) => "2g",
        Some(Band::B5g) => "5g",
        Some(Band::B6g) => "6g",
        None => "-",
    }
}

fn opt_num(s: &str) -> Option<u32> {
    (s != "-").then(|| s.parse().unwrap())
}

// ---- printing (txrx-js.mjs prints the same) ----

fn tx_info_lines(out: &mut Vec<String>, i: &TxInfo) {
    let n = |v: bool| u32::from(v);
    let f = [
        i.pkt_size,
        n(i.is_bmc),
        n(i.wd_page),
        i.ch_dma,
        i.seq,
        n(i.report),
        i.sn,
        n(i.tx_cnt_lmt_en),
        i.tx_cnt_lmt,
        n(i.hiq),
        n(i.upd_wlan_hdr),
        i.qsel,
        n(i.sw_mld),
        i.port,
        i.mac_id,
        i.hw_ssn_sel,
        i.hw_seq_mode,
        n(i.en_wd_info),
        n(i.use_rate),
        n(i.dis_data_fb),
        i.data_rate,
        n(i.tid_indicate),
        n(i.er_cap),
        n(i.stbc),
        n(i.ldpc),
        n(i.sec_en),
        i.sec_keyid,
        i.sec_type,
        i.sec_cam_idx,
        i.data_retry_lowest_rate,
        n(i.a_ctrl_bsr),
        n(i.bk),
        i.hdr_llc_len,
    ];
    let typ = match i.tx_type {
        Some(TxType::Mgmt) => "mgmt",
        Some(TxType::Data) => "data",
        None => "-",
    };
    let btc = match i.btc_pkt {
        Some(BtcPkt::Eapol) => "EAPOL",
        Some(BtcPkt::Arp) => "ARP",
        Some(BtcPkt::Dhcp) => "DHCP",
        Some(BtcPkt::Icmp) => "ICMP",
        None => "-",
    };
    let fs: Vec<String> = f.iter().map(u32::to_string).collect();
    out.push(format!("= info {typ} {} btc {btc}", fs.join(" ")));
    let wd = i.wd_info.map_or("-".into(), |w| {
        w.iter()
            .map(|v| format!("{v:x}"))
            .collect::<Vec<_>>()
            .join(",")
    });
    out.push(format!("= wd {wd} desc {}", hex(&fill_txdesc(&i.desc()))));
}

fn hdr_line(out: &mut Vec<String>, h: Option<&Hdr>) {
    let Some(h) = h else {
        out.push("= hdr -".into());
        return;
    };
    let a = |m: Option<Mac>| m.map_or("-".into(), |m| hex(&m));
    out.push(format!(
        "= hdr {} {} {} {}{}{}{}{} {} {} {} {} {} {} {} {} {} {} {}",
        h.fc,
        h.typ,
        h.stype,
        b(h.to_ds),
        b(h.from_ds),
        b(h.retry),
        b(h.pm),
        b(h.protected),
        h.hdr_len,
        h.dur,
        a(h.addr1),
        a(h.addr2),
        a(h.addr3),
        o(h.seq),
        o(h.frag),
        b(h.short),
        a(h.addr4),
        o(h.qos),
        o(h.tid)
    ));
}

fn rsn_line(out: &mut Vec<String>, x: Option<&Rsn>) {
    let Some(x) = x else {
        out.push("= rsn -".into());
        return;
    };
    let list = |v: &[u32]| {
        v.iter()
            .map(|s| format!("{s:x}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let pmk: Vec<String> = x.pmkids.iter().map(|p| hex(p)).collect();
    let mut s = format!(
        "= rsn {} {:x} {} {} {} {} {} {}",
        x.version,
        x.group,
        list(&x.pairwise),
        list(&x.akms),
        x.caps,
        if pmk.is_empty() {
            "-".into()
        } else {
            pmk.join(",")
        },
        x.group_mgmt.map_or("-".into(), |g| format!("{g:x}")),
        x.bad.unwrap_or("-")
    );
    if x.bad != Some("version") {
        let _ = write!(s, " {}{}{}{}", b(x.mfpc), b(x.mfpr), b(x.psk), b(x.ccmp));
    }
    out.push(s);
}

fn ies_line(out: &mut Vec<String>, l: &Elements) {
    let v: Vec<String> = l
        .list
        .iter()
        .map(|e| format!("{}:{}", e.id, hex(&e.data)))
        .collect();
    out.push(format!(
        "= ies {} {}",
        if v.is_empty() {
            "-".into()
        } else {
            v.join(" ")
        },
        b(l.truncated)
    ));
}

fn beacon_lines(out: &mut Vec<String>, bc: Option<&Beacon>) {
    let Some(bc) = bc else {
        out.push("= beacon -".into());
        return;
    };
    let rates = |v: &[u8]| {
        if v.is_empty() {
            "-".into()
        } else {
            v.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        }
    };
    let ht = bc.ht.map_or("-".into(), |h| {
        format!(
            "{},{},{}",
            o(h.cap_info),
            o(h.primary_ch),
            o(h.secondary_offset)
        )
    });
    out.push(format!(
        "= beacon {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        if bc.beacon { "beacon" } else { "probeResp" },
        hex(&bc.bssid),
        hex(&bc.sa),
        bc.tsf,
        bc.beacon_int,
        bc.capability,
        b(bc.privacy),
        hex(&bc.ssid_raw),
        hex(bc.ssid.as_bytes()),
        o(bc.ch),
        hex(&bc.rate_bytes),
        rates(&bc.rates_500k),
        rates(&bc.basic_rates_500k),
        b(bc.wpa),
        ht,
        o(bc.dtim_period),
        bc.country.map_or("-".into(), |c| hex(&c))
    ));
    rsn_line(out, bc.rsn.as_ref());
    ies_line(out, &bc.ies);
}

fn mgmt_line(out: &mut Vec<String>, m: Option<&parse::Mgmt>) {
    let Some(m) = m else {
        out.push("= mgmt -".into());
        return;
    };
    hdr_line(out, Some(&m.hdr));
    match &m.kind {
        MgmtKind::Auth {
            alg,
            auth_seq,
            status,
        } => out.push(format!("= mgmt auth {alg} {auth_seq} {status}")),
        MgmtKind::AssocResp {
            capability,
            status,
            aid,
            ies,
        } => {
            out.push(format!("= mgmt assocResp {capability} {status} {aid}"));
            ies_line(out, ies);
        }
        MgmtKind::Deauth { reason } => out.push(format!("= mgmt deauth {reason}")),
        MgmtKind::Disassoc { reason } => out.push(format!("= mgmt disassoc {reason}")),
        MgmtKind::Other => out.push("= mgmt other".into()),
    }
}

fn eth_line(out: &mut Vec<String>, e: Option<&EthRx>) {
    let Some(e) = e else {
        out.push("= eth -".into());
        return;
    };
    out.push(format!(
        "= eth {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        hex(&e.eth),
        hex(&e.dst),
        hex(&e.src),
        hex(&e.bssid),
        e.typ,
        b(e.eapol),
        b(e.protected),
        o(e.pn),
        o(e.key_id),
        e.seq,
        b(e.retry),
        b(e.qos),
        e.tid,
        e.sec_idx,
        b(e.multicast)
    ));
}

fn desc_str(d: Option<&RxDesc>) -> String {
    let Some(d) = d else { return "-".into() };
    let n = |v: bool| u32::from(v);
    let f = [
        d.pkt_size,
        d.drv_info_size,
        n(d.long_rxdesc),
        d.pkt_type,
        n(d.mac_info_valid),
        d.bw,
        d.data_rate,
        d.gi_ltf,
        d.user_id,
        n(d.sr_en),
        d.ppdu_cnt,
        d.ppdu_type,
        d.free_run_cnt,
        n(d.icv_err),
        n(d.crc32_err),
        n(d.hw_dec),
        n(d.sw_dec),
        n(d.addr1_match),
        n(d.ampdu),
        d.shift,
        n(d.bb_sel),
        d.shift_field,
        d.offset as u32,
        d.rxd_len as u32,
        n(d.ready),
        d.frame_type,
        n(d.addr_cam_valid),
        d.addr_cam_id,
        d.sec_cam_id,
        d.mac_id,
        d.rx_pl_id,
        d.sec_type,
        d.copy_len as u32,
    ];
    let fs: Vec<String> = f.iter().map(u32::to_string).collect();
    format!("{} {}", fs.join(" "), o(d.write_size))
}

fn ppdu_str(p: Option<&PhyPpdu>) -> String {
    let Some(p) = p else { return "-".into() };
    format!(
        "{} {} {} {} {} {} {} {} {} {} {} {} {} {},{},{},{} {}{} {} {} {} {}{}{}{} {}",
        p.start,
        p.len,
        b(p.valid),
        b(p.to_self),
        p.rate,
        p.mac_id,
        p.phy_idx,
        p.rssi.map(|v| v.to_string()).join(","),
        p.rssi_avg,
        p.rpl_avg,
        p.chan_idx,
        p.ie,
        p.rx_path_en,
        b(p.ofdm.has),
        p.ofdm.avg_snr,
        p.ofdm.evm_max,
        p.ofdm.evm_min,
        b(p.has_data),
        b(p.has_bcn),
        o(p.ie09),
        o(p.ie10),
        p.hdr2_en.map_or("-", b),
        b(p.ldpc),
        b(p.stbc),
        b(p.bf),
        b(p.su),
        o(p.cfo)
    )
}

fn status_str(s: &RxStatus) -> String {
    let enc = match s.encoding {
        Encoding::Legacy => "legacy",
        Encoding::Ht => "ht",
        Encoding::Vht => "vht",
        Encoding::He => "he",
        Encoding::Invalid => "invalid",
    };
    let gi = match s.he_gi {
        HeGi::Gi08 => "0.8",
        HeGi::Gi16 => "1.6",
        HeGi::Gi32 => "3.2",
    };
    let cs = if s.chains != 0 {
        format!("{},{}", s.chain_signal[0], s.chain_signal[1])
    } else {
        "-".into()
    };
    format!(
        "{} {} {} {}{}{} {} {} {} {} {} {} {} {} {} {} {} {}",
        band_str(s.band),
        o(s.ch),
        o(s.freq),
        b(s.failed_fcs),
        b(s.decrypted),
        b(s.ampdu),
        o(s.ampdu_ref),
        s.bw,
        s.signal,
        s.chains,
        cs,
        enc,
        s.rate_idx,
        o(s.nss),
        b(s.short_gi),
        gi,
        s.mactime,
        b(s.no_signal)
    )
}

fn c2h_str(c: Option<&C2h>) -> String {
    let Some(c) = c else { return "-".into() };
    let a = c.ack.map_or("-".into(), |a| {
        format!(
            "{},{},{},{},{},{}",
            match a.kind {
                rtw89_core::fw::AckKind::Rec => "rec",
                rtw89_core::fw::AckKind::Done => "done",
            },
            a.cat,
            a.cls,
            a.func,
            a.seq,
            o(a.ret)
        )
    });
    format!(
        "{} {} {} {} {} {} {a}",
        c.cat,
        c.cls,
        c.func,
        c.len,
        o(c.seq),
        hex(&c.payload)
    )
}

fn deliver_lines(out: &mut Vec<String>, list: &[RxEntry]) {
    for e in list {
        out.push(format!("= d {}", desc_str(Some(&e.desc))));
        let pd = e
            .ppdu_desc
            .as_ref()
            .map_or("-".into(), |d| format!("{},{}", d.pkt_type, d.ppdu_cnt));
        out.push(format!(
            "= d frame {} match {} pdesc {pd}",
            hex(&e.frame),
            b(e.ppdu_match)
        ));
        out.push(format!("= d ppdu {}", ppdu_str(e.ppdu.as_ref())));
        out.push(format!("= d status {}", status_str(&e.status)));
    }
}

fn rx_lines(out: &mut Vec<String>, r: &RxResult, stats: &mut Vec<String>) {
    let typ = match r.typ {
        RxType::Frame => "frame",
        RxType::Ppdu => "ppdu",
        RxType::C2h => "c2h",
        RxType::Other => "other",
        RxType::Partial => "partial",
        RxType::Dropped => "dropped",
    };
    out.push(format!(
        "= rx {typ} {} {} {}",
        r.reason.unwrap_or("-"),
        o(r.pending),
        r.frame.as_deref().map_or("-".into(), hex)
    ));
    out.push(format!("= rx desc {}", desc_str(r.desc.as_ref())));
    out.push(format!("= rx ppdu {}", ppdu_str(r.ppdu.as_ref())));
    out.push(format!("= rx c2h {}", c2h_str(r.c2h.as_ref())));
    deliver_lines(out, &r.deliver);
    for s in stats.drain(..) {
        out.push(format!("= stats {s}"));
    }
}

// ---- RX chip ops (txrx-js.mjs `makeOps`) ----

#[derive(Clone, Copy, PartialEq)]
enum OpsKind {
    None,
    Standin,
    Phy,
}

struct Ops<'a> {
    kind: OpsKind,
    stats: &'a mut Vec<String>,
    phy: &'a mut Phy,
}

impl RxOps for Ops<'_> {
    fn convert_rpl_to_rssi(&mut self, p: &mut PhyPpdu) {
        match self.kind {
            OpsKind::None => {}
            OpsKind::Standin => {
                let delta = p.rpl_avg.wrapping_sub(p.rssi_avg);
                p.rssi[0] = p.rssi[0].wrapping_add(delta);
                p.rssi[1] = p.rssi[1].wrapping_add(delta);
                p.rssi_avg = p.rpl_avg;
            }
            OpsKind::Phy => PhyOps(self.phy).convert_rpl_to_rssi(p),
        }
    }
    fn query_ppdu(&mut self, p: &PhyPpdu, st: &mut RxStatus) {
        match self.kind {
            OpsKind::None => {}
            OpsKind::Standin => {
                st.signal = (i32::from(p.rssi[0].max(p.rssi[1])) >> 1) - 110;
                st.chains = 3;
                st.chain_signal = [
                    (i32::from(p.rssi[0]) >> 1) - 110,
                    (i32::from(p.rssi[1]) >> 1) - 110,
                ];
            }
            OpsKind::Phy => PhyOps(self.phy).query_ppdu(p, st),
        }
    }
    fn rx_stats(&mut self, e: &RxEntry, p: Option<&PhyPpdu>, d: Option<&RxDesc>) {
        if self.kind == OpsKind::None {
            return;
        }
        let h = hex(&e.frame);
        self.stats.push(format!(
            "{} {} {}",
            u32::from(p.is_some()),
            d.map_or("-".into(), |d| d.pkt_type.to_string()),
            &h[..h.len().min(16)]
        ));
    }
}

// ---- the interpreter ----

fn kv(args: &[&str]) -> std::collections::HashMap<String, String> {
    args.iter()
        .filter_map(|a| a.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn tx_opts(args: &[&str]) -> TxOpts {
    let m = kv(args);
    let mut o = TxOpts {
        mac_id: m.get("mac_id").map(|v| v.parse().unwrap()),
        sta: m.contains_key("sta"),
        basic_rates: m.get("basic").map_or(0, |v| v.parse().unwrap()),
        supp_rates: m.get("supp").map_or(0, |v| v.parse().unwrap()),
        band: m.get("band").map(|v| band(v)),
        tid: m.get("tid").map_or(0, |v| v.parse().unwrap()),
        no_cck: m.contains_key("nocck"),
        req_tx_status: m.contains_key("rpt"),
        ..TxOpts::default()
    };
    if m.contains_key("ersl") {
        o.er_cap = true;
        o.stbc = true;
        o.ldpc = true;
    }
    if let Some(k) = m.get("key") {
        let p: Vec<&str> = k.split(',').collect();
        o.key = Some(TxKey {
            key_idx: p[0].parse().unwrap(),
            sec_cam_idx: opt_num(p[1]),
            cipher: p[2].parse().unwrap(),
        });
    }
    o
}

struct Rx {
    t: Txrx,
    kind: OpsKind,
    stats: Vec<String>,
    phy: Phy,
}

fn run(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut tx = Txrx::new();
    let mut pn_key = CcmpKey::default();
    let mut rkey = CcmpKey::default();
    let mut rx: Option<Rx> = None;
    for line in lines.iter().filter(|l| l.starts_with("> ")) {
        out.push(line.clone());
        let a: Vec<&str> = line[2..].split(' ').collect();
        match a[0] {
            "txnew" => {
                tx = Txrx::new();
                if a[1] != "-" {
                    let (bd, ch) = a[1].split_once(',').unwrap();
                    tx.chan = Some(TxrxChan {
                        band: band(bd),
                        ch: ch.parse().unwrap(),
                        prev: None,
                    });
                }
                tx.mac_id = opt_num(a[2]);
                tx.tx.tx_rpt_enabled = a[3] == "1";
            }
            "actrl" => tx.tx.last_actrl = true,
            "txseq" => {
                let mut f = unhex(a[1]);
                match tx_seq(&mut tx, &mut f) {
                    Ok(()) => {
                        let ts: Vec<String> = tx.tx.tid_seq.iter().map(u16::to_string).collect();
                        out.push(format!(
                            "= seq {} {} {}",
                            hex(&f),
                            tx.tx.seq_ctrl,
                            ts.join(",")
                        ));
                    }
                    Err(_) => out.push("= error".into()),
                }
            }
            "txinfo" => {
                let kind = match a[1] {
                    "mgmt" => Some(TxKind::Mgmt),
                    "data" => Some(TxKind::Data),
                    "fwcmd" => Some(TxKind::Fwcmd),
                    _ => None,
                };
                let f = unhex(a[2]);
                let opts = tx_opts(&a[3..]);
                match tx_info(&mut tx, kind, &f, &opts) {
                    Ok(i) => {
                        tx_info_lines(&mut out, &i);
                        out.push(format!(
                            "= txstate {} {}",
                            tx.tx.tx_rpt_sn,
                            b(tx.tx.last_actrl)
                        ));
                    }
                    Err(_) => out.push("= error".into()),
                }
            }
            "probe" => {
                let ssid = unhex(a[2]);
                let ch: u8 = a[4].parse().unwrap();
                let p = ieee::ProbeReq {
                    own: mac(a[1]),
                    ssid: &ssid,
                    band: band(a[3]),
                    ch: Some(ch),
                    dst: mac(a[5]),
                    bssid: mac(a[6]),
                    seq: a[7].parse().unwrap(),
                };
                out.push(format!("= {}", hex(&ieee::probe_req(&p))));
            }
            "auth" => {
                let f = ieee::auth_open(&mac(a[1]), &mac(a[2]), a[3].parse().unwrap());
                out.push(format!("= {}", hex(&f)));
            }
            "assoc" => {
                let ssid = unhex(a[3]);
                let rates = (a[5] != "*").then(|| unhex(a[5]));
                let rsn = (a[7] != "-").then(|| unhex(a[7]));
                let q = ieee::AssocReq {
                    own: mac(a[1]),
                    bssid: mac(a[2]),
                    ssid: &ssid,
                    band: band(a[4]),
                    ap_rate_bytes: rates.as_deref(),
                    ap_capability: a[6].parse().unwrap(),
                    rsn: rsn.as_deref(),
                    listen_interval: a[8].parse().unwrap(),
                    seq: a[9].parse().unwrap(),
                };
                out.push(format!("= {}", hex(&ieee::assoc_req(&q))));
            }
            "deauth" => {
                let f = ieee::deauth(
                    &mac(a[1]),
                    &mac(a[2]),
                    a[3].parse().unwrap(),
                    a[4].parse().unwrap(),
                );
                out.push(format!("= {}", hex(&f)));
            }
            "null" => {
                let f = ieee::null_func(
                    &mac(a[1]),
                    &mac(a[2]),
                    a[3].parse().unwrap(),
                    (a[4] != "-").then(|| a[4].parse().unwrap()),
                );
                out.push(format!("= {}", hex(&f)));
            }
            "rsnie" => {
                let l = |s: &str| -> Vec<u32> {
                    if s == "-" {
                        Vec::new()
                    } else {
                        s.split(',')
                            .map(|x| u32::from_str_radix(x, 16).unwrap())
                            .collect()
                    }
                };
                let (pw, ak) = (l(a[2]), l(a[3]));
                let req = ieee::RsnIeReq {
                    group: u32::from_str_radix(a[1], 16).unwrap(),
                    pairwise: &pw,
                    akms: &ak,
                    caps: a[4].parse().unwrap(),
                };
                out.push(format!("= {}", hex(&ieee::rsn_ie(&req))));
            }
            "pnkey" => {
                pn_key = CcmpKey::new(a[1].parse().unwrap(), None);
                pn_key.tx_pn = a[2].parse().unwrap();
            }
            "e2w" => {
                let eth = unhex(a[1]);
                let key = (a[3] == "1").then_some(&mut pn_key);
                let o = DataTx {
                    bssid: mac(a[2]),
                    key,
                    qos: (a[4] != "-").then(|| a[4].parse().unwrap()),
                    seq: a[5].parse().unwrap(),
                };
                match eth_to_wlan(&eth, o) {
                    Ok(w) => out.push(format!("= {} {}", hex(&w), pn_key.tx_pn)),
                    Err(_) => out.push("= error".into()),
                }
            }
            "w2e" => eth_line(&mut out, wlan_to_eth(&unhex(a[1])).as_ref()),
            "rkey" => {
                rkey = CcmpKey::default();
                rkey.rsc = a[1].parse().unwrap();
            }
            "replay" => {
                let r = ccmp_replay(&mut rkey, a[1].parse().unwrap(), a[2].parse().unwrap());
                out.push(format!("= {} {}", b(r), rkey.replays));
            }
            "parse" => {
                let f = unhex(a[1]);
                beacon_lines(&mut out, parse::beacon_parse(&f).as_ref());
                mgmt_line(&mut out, parse::mgmt_parse(&f).as_ref());
                hdr_line(&mut out, ieee::hdr_parse(&f).as_ref());
                let l = ieee::parse_ies(&f, 36, f.len());
                ies_line(&mut out, &l);
                out.push(format!(
                    "= chan {}",
                    parse::ies_channel(&l).map_or(-1, i32::from)
                ));
            }
            "rsn" => rsn_line(&mut out, parse::rsn_parse(&unhex(a[1])).as_ref()),
            "bitmap" => {
                let m = ieee::rate_bitmap(&unhex(a[1]), band(a[2]), a[3] == "1");
                out.push(format!("= {m}"));
            }
            "rxnew" => {
                let mut t = Txrx::new();
                if a[1] != "-" {
                    let p: Vec<&str> = a[1].split(',').collect();
                    let prev = (p[2] != "-").then(|| (band(p[2]), p[3].parse().unwrap()));
                    t.chan = Some(TxrxChan {
                        band: band(p[0]),
                        ch: p[1].parse().unwrap(),
                        prev,
                    });
                }
                t.scanning = a[2] == "1";
                let kind = match a[3] {
                    "none" => OpsKind::None,
                    "standin" => OpsKind::Standin,
                    _ => OpsKind::Phy,
                };
                rx = Some(Rx {
                    t,
                    kind,
                    stats: Vec::new(),
                    phy: Phy::default(),
                });
            }
            "rx" => {
                let r = rx.as_mut().unwrap();
                let data = unhex(a[1]);
                let input = match opt_num(a[4]) {
                    None => RxIn::packet(&data),
                    Some(len) => RxIn {
                        data: &data,
                        fs: a[2] == "1",
                        ls: a[3] == "1",
                        len: Some(len),
                    },
                };
                let mut ops = Ops {
                    kind: r.kind,
                    stats: &mut r.stats,
                    phy: &mut r.phy,
                };
                let res = rx_parse(&mut r.t, input, &mut ops);
                rx_lines(&mut out, &res, &mut r.stats);
            }
            "rxflush" => {
                let r = rx.as_mut().unwrap();
                let mut ops = Ops {
                    kind: r.kind,
                    stats: &mut r.stats,
                    phy: &mut r.phy,
                };
                let d = r.t.rx_flush(&mut ops);
                deliver_lines(&mut out, &d);
                for s in r.stats.drain(..) {
                    out.push(format!("= stats {s}"));
                }
            }
            "rxreset" => rx.as_mut().unwrap().t.pci_reset(),
            other => panic!("unknown command {other}"),
        }
    }
    out
}

#[test]
fn txrx_equals_the_js_module() {
    if skip("txrx_equals_the_js_module") {
        return;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/txrx-js.mjs");
    let mut total = 0;
    for seed in ["20261015", "7", "424242"] {
        let res = Command::new("node")
            .arg(&script)
            .arg(seed)
            .env("WLAN_TOOLS", wlan_dir())
            .output()
            .expect("node (RTW89_SKIP_FAKE=1 skips this test)");
        assert!(
            res.status.success(),
            "txrx-js.mjs: {}\n{}",
            res.status,
            String::from_utf8_lossy(&res.stderr)
        );
        let js: Vec<String> = String::from_utf8(res.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert!(js.len() > 5000, "seed {seed}: only {} lines", js.len());
        let ours = run(&js);
        for (i, (a, b)) in ours.iter().zip(&js).enumerate() {
            if a != b {
                let from = i.saturating_sub(6);
                panic!(
                    "seed {seed}, line {}: Rust {a:?}\n                JS   {b:?}\ncontext (JS):\n{}",
                    i + 1,
                    js[from..=i].join("\n")
                );
            }
        }
        assert_eq!(ours.len(), js.len(), "seed {seed}: line count");
        total += js.len();
    }
    eprintln!("txrx_js: {total} lines equal");
}
