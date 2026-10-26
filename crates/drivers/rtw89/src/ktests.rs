//! ktests of the WLAN driver (prefix `wl`). Under QEMU (no RTL8852BE): without `wlan=on` nothing is
//! attached and the chip's IDs stay untouched by the PCI core; the netdev glue against a card that
//! has no chip (queues, link, backpressure, the panic mirror); CONFIG parsing keeps the passphrase
//! out of every text; the MCP tool refuses a passphrase argument. On the Mini-PC with `wlan=on`
//! (class Wlan: runs over MCP on hardware only with `wlan=on`) the attached card's status.
//! Every test is rerunnable (Safe tests run several times per boot over MCP).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::Ordering;
use devmodel::DevError;
use devmodel::net::NetDevice;
use ktest::{TestResult, kassert, ktest};

use crate::card::{Card, txq_max};
use crate::netdev::WlanNet;

fn untouched() -> TestResult {
    if pci::registry::opted_in("wlan") {
        return ktest::skip("wlan=on: the driver may attach (wl.chip.status checks it)");
    }
    kassert!(crate::cards().is_empty(), "a card attached without wlan=on");
    kassert!(
        pci::registry::untouched_key(crate::VENDOR_REALTEK, crate::DEV_RTL8852BE, 0x02_80_00) == Some("wlan"),
        "10EC:B852 is not kept untouched by the wlan key"
    );
    Ok(())
}
ktest!("wl.untouched", Safe, untouched);

fn glue() -> TestResult {
    let c = Arc::new(Card::new(String::from("wlan-test"), String::from("-"), None, None));
    let n = WlanNet(c.clone());
    kassert!(!n.link().up, "a fresh card has no link");
    // no link: a frame from the stack is dropped (counted), never queued
    kassert!(n.transmit(&[0u8; 60]).is_ok());
    kassert!(c.txq.lock().is_empty() && c.tx_no_link.load(Ordering::Relaxed) == 1);
    c.link_up.store(true, Ordering::Release);
    kassert!(n.link().up && n.link().speed_mbps == crate::netdev::LINK_MBPS);
    for i in 0..txq_max() {
        kassert!(n.transmit(&[i as u8; 60]).is_ok(), "frame {} refused", i);
    }
    kassert!(n.transmit(&[0u8; 60]) == Err(DevError::Timeout), "a full queue says Timeout");
    kassert!(c.txq.lock().len() == txq_max());
    // frames from the thread come out in order; one too long for the buffer is dropped
    c.deliver(vec![vec![1u8; 100], vec![2u8; 2000], vec![3u8; 64]]);
    let mut buf = [0u8; 1514];
    kassert!(n.receive(&mut buf) == Some(100) && buf[0] == 1);
    kassert!(n.receive(&mut buf) == Some(64) && buf[0] == 3, "the long frame is skipped");
    kassert!(n.receive(&mut buf).is_none());
    kassert!(c.rx_overflow.load(Ordering::Relaxed) == 1);
    kassert!(!n.poll_tx(&[0u8; 60]), "the panic mirror never goes over the WLAN");
    let s = n.stats();
    kassert!(s.tx_dropped == 2, "dropped {}", s.tx_dropped);
    // the card without a chip: Quiesce touches nothing and says nothing
    devmodel::Quiesce::quiesce(c.as_ref());
    kassert!(c.quiesced.load(Ordering::Acquire));
    Ok(())
}
ktest!("wl.netdev.glue", Safe, glue);

fn config_secret() -> TestResult {
    let pass = "ktest passphrase 12345";
    let get = |k: &str| match k {
        "wlan_ssid" => Some(String::from("BitsOS-Test")),
        "wlan_psk" => Some(String::from(pass)),
        _ => None,
    };
    let n = crate::config::network(&get).map_err(|e| format!("config: {e}"))?;
    let n = n.ok_or("no network")?;
    let shown = format!("{n:?}");
    kassert!(!shown.contains("passphrase 12345"), "the passphrase shows in {}", shown);
    let want = wifi::crypto::psk(pass.as_bytes(), b"BitsOS-Test").map_err(|e| format!("{e}"))?;
    kassert!(n.pmk.ct_eq(&want), "the PMK differs");
    Ok(())
}
ktest!("wl.config.secret", Safe, config_secret);

fn tool_refuses_psk() -> TestResult {
    let args = mcp::Json::parse(r#"{"action":"join","psk":"x"}"#).map_err(|e| format!("{e:?}"))?;
    kassert!(matches!(crate::tool::parse(&args), Err(mcp::ToolError::Refused(_))));
    let args = mcp::Json::parse(r#"{"action":"scan","channels":[1,6,11],"dwell_ms":50}"#).map_err(|e| format!("{e:?}"))?;
    kassert!(
        crate::tool::parse(&args).ok().flatten()
            == Some(crate::card::Cmd::Scan { channels: vec![1, 6, 11], dwell_ms: 50 })
    );
    let args = mcp::Json::parse(r#"{"action":"scan","channels":[14]}"#).map_err(|e| format!("{e:?}"))?;
    kassert!(crate::tool::parse(&args).is_err(), "channel 14 is not in the DE domain");
    // a repeated channel counts once; one band; the whole scan at most SCAN_MAX_MS
    let args = mcp::Json::parse(r#"{"action":"scan","channels":[6,6,1,6]}"#).map_err(|e| format!("{e:?}"))?;
    kassert!(
        crate::tool::parse(&args).ok().flatten()
            == Some(crate::card::Cmd::Scan { channels: vec![6, 1], dwell_ms: rtw89_core::drv::runner::SCAN_DWELL_MS })
    );
    let args = mcp::Json::parse(r#"{"action":"scan","channels":[1,36]}"#).map_err(|e| format!("{e:?}"))?;
    kassert!(crate::tool::parse(&args).is_err(), "two bands in one scan");
    let args = mcp::Json::parse(r#"{"action":"scan","channels":[36,40,44,48,52,56,60,64,100,104,108,112,116,120,124,128],"dwell_ms":1000}"#)
        .map_err(|e| format!("{e:?}"))?;
    kassert!(crate::tool::parse(&args).is_err(), "16 s of scanning");
    kassert!(mcp::find("wlan").is_some_and(|t| t.owner == "WL"), "the wlan tool is registered");
    Ok(())
}
ktest!("wl.tool.args", Safe, tool_refuses_psk);

/// Without an attached card every action answers at once (no panic, no wait)
fn tool_no_card() -> TestResult {
    if !crate::cards().is_empty() {
        return ktest::skip("a card is attached (wl.chip.status checks it)");
    }
    let ctx = mcp::ToolContext::detached(time::now_ns().saturating_add(5_000_000_000));
    for a in ["status", "scan", "join", "leave"] {
        let args = mcp::Json::parse(&format!(r#"{{"action":"{a}"}}"#)).map_err(|e| format!("{e:?}"))?;
        let out = crate::tool::wlan(&ctx, &args).map_err(|e| format!("{a}: {e:?}"))?;
        let text = match out.first() {
            Some(mcp::ToolOutput::Text(t)) => t.clone(),
            _ => String::new(),
        };
        kassert!(text.starts_with("wlan: no card ("), "{}: {}", a, text);
    }
    Ok(())
}
ktest!("wl.tool.no_card", Safe, tool_no_card);

/// A command the thread does not take in time is taken back (a retry never piles up stale ones)
fn command_taken_back() -> TestResult {
    let c = Card::new(String::from("wlan-test"), String::from("-"), None, None);
    let t0 = time::now_ns();
    let r = c.command(crate::card::Cmd::Leave, 20_000_000);
    kassert!(r.as_ref().is_err_and(|e| e.contains("was not run")), "{:?}", r);
    kassert!(c.cmds.lock().is_empty(), "the command stayed queued");
    kassert!(time::now_ns() - t0 < 2_000_000_000, "the wait was not bounded");
    Ok(())
}
ktest!("wl.card.command_taken_back", Safe, command_taken_back);

/// Quiesce while the thread holds the chip lock (a join or bring-up) never waits: it sets the flag
/// the bus refuses further work by and returns
fn quiesce_lock_held() -> TestResult {
    let c = Arc::new(Card::new(String::from("wlan-test"), String::from("-"), None, None));
    let g = c.core.lock();
    let t0 = time::now_ns();
    devmodel::Quiesce::quiesce(c.as_ref());
    kassert!(time::now_ns() - t0 < 100_000_000, "Quiesce waited");
    kassert!(c.quiesced.load(Ordering::Acquire));
    drop(g);
    Ok(())
}
ktest!("wl.quiesce.lock_held", Safe, quiesce_lock_held);

/// On the Mini-PC with wlan=on: the card is attached, its thread runs, the status has no secret.
/// Reads only (rerunnable); the join itself is the desk's check list.
fn chip_status() -> TestResult {
    let cards = crate::cards();
    let Some(c) = cards.first() else {
        return ktest::skip("no RTL8852BE attached (wlan=on and the Mini-PC's 04:00.0 are needed)");
    };
    let lines = crate::tool::status_lines(c);
    let text = lines.join("\n");
    kassert!(text.contains("state="), "no status: {}", text);
    if let Some(psk) = net::config("wlan_psk").filter(|p| p.len() >= 8) {
        kassert!(!text.contains(psk.as_str()), "the status shows the passphrase");
    }
    let phase = c.phase.lock().clone();
    kassert!(phase == "running", "driver thread: {}", phase);
    Ok(())
}
ktest!("wl.chip.status", Wlan, chip_status);
