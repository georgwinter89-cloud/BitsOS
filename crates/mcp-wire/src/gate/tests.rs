//! The gate protocol both ways, its hostile-input bounds and the offer table.
use super::*;
use alloc::vec;

#[test]
fn commands_round_trip() {
    assert_eq!(parse_command(b"hello\n"), Some(Command::Hello));
    assert_eq!(parse_command(b"list"), Some(Command::List { client: String::new() }));
    let c = "0123456789abcdef".repeat(4);
    assert_eq!(parse_command(&encode_list_for(&c)), Some(Command::List { client: c.clone() }));
    assert_eq!(encode_list_for(""), b"list".to_vec());
    assert_eq!(parse_command(&crate::access::encode_access(&c)), Some(Command::Access { client: c.clone() }));
    assert_eq!(parse_command(b"answered"), Some(Command::Answered));
    assert_eq!(parse_command(&encode_auth("10.0.2.2:5000", b"tok-12345")), Some(Command::Auth { peer: "10.0.2.2:5000".into(), token: b"tok-12345".to_vec() }));
    let r = Request { tool: "hw_info".into(), args: Json::parse(r#"{"section":"cpu","n":[1,"\u00e4\n"]}"#).unwrap(), timeout_ms: 2500, peer: "10.0.2.2:5000".into(), client: String::new() };
    assert_eq!(parse_command(&encode_call(&r)), Some(Command::Call(r.clone())));
    assert_eq!(parse_command(&encode_install(&r)), Some(Command::Install(r.clone())));
    let rc = Request { client: c, ..r };
    assert_eq!(parse_command(&encode_call(&rc)), Some(Command::Call(rc.clone())));
    assert_eq!(parse_command(&encode_install(&rc)), Some(Command::Install(rc)));
}

#[test]
fn hostile_commands() {
    for bad in [&b""[..], b"hello x", b"list\n\n", b"auth", b"auth 1.2.3.4", b"auth 1.2.3.4 ", b"call", b"call {", b"call []", b"call {\"tool\":\"\"}", b"call {\"tool\":\"a\",\"arguments\":[]}", b"call {\"tool\":\"a\",\"timeout_ms\":0}", b"call {\"tool\":\"a\",\"timeout_ms\":86400001}", b"rm -rf /", b"\xff\xfe", b"list x", b"list ABCDEF", b"access", b"access 12", b"call {\"tool\":\"a\",\"client\":\"zz\"}", b"call {\"tool\":\"a\",\"client\":5}"] {
        assert_eq!(parse_command(bad), None, "{:?}", core::str::from_utf8(bad));
    }
    let mut big = b"call {\"tool\":\"a\",\"arguments\":{\"x\":\"".to_vec();
    big.resize(LINE_MAX + 10, b'a');
    assert_eq!(parse_command(&big), None);
    // A peer is printable and short whatever the service sends.
    let Some(Command::Call(r)) = parse_command(b"call {\"tool\":\"a\",\"peer\":\"\\u0007xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}") else { panic!() };
    assert_eq!(r.peer.len(), PEER_MAX);
    assert!(r.peer.starts_with('?'));
    assert_eq!(r.timeout_ms, rpc::DEFAULT_TIMEOUT_MS);
}

#[test]
fn results_round_trip() {
    let all: [Result<ToolResult, String>; 5] = [
        Err("Still running: ktest (call 3)".into()),
        Ok(Err(ToolError::BadArgument("section".into()))),
        Ok(Err(ToolError::Refused("not now".into()))),
        Ok(Err(ToolError::Failed("it broke\n\"x\"".into()))),
        Ok(Ok(vec![ToolOutput::Text("a\u{1}b".into()), ToolOutput::Png(vec![0x89, b'P', 0, 255]), ToolOutput::Json(Json::obj(vec![("a", Json::Num(1.0))]))])),
    ];
    for r in all {
        let back = decode_result(&encode_result(&r)).unwrap();
        assert_eq!(alloc::format!("{r:?}"), alloc::format!("{back:?}"));
    }
    assert!(decode_result("{\"ok\":[{\"what\":1}]}").is_none());
    assert!(decode_result("nonsense").is_none());
}

#[test]
fn hello_and_list_round_trip() {
    let h = Hello { mode: Mode::Both, port: 8766, conns: 8, version: "0.1.0".into() };
    assert_eq!(decode_hello_answer(&encode_hello_answer(&h)), Some(h));
    assert_eq!(decode_hello_answer(r#"{"mode":"nope","port":1,"conns":1}"#), None);
    let t = Listed { name: "hw_info".into(), description: "Facts \"quoted\".".into(), schema: r#"{"type":"object","properties":{}}"#.into(), owner: "MC".into(), safety: Safety::Read, verified: true, pid: None, service: false, installer: false };
    let s = Listed { name: "journal".into(), pid: Some(77), service: true, ..t.clone() };
    let i = Listed { name: "reload".into(), safety: Safety::Change, installer: true, ..t.clone() };
    let v = vec![t, s, i];
    assert_eq!(decode_list_answer(&encode_list_answer(&v)), Some(v));
}

#[test]
fn modes_and_ports() {
    assert_eq!(Mode::parse(" both "), Some(Mode::Both));
    assert_eq!(Mode::parse("on"), None);
    assert_eq!(Mode::Both.service_port(8765), Some(8766));
    assert_eq!(Mode::Both.service_port(65535), None);
    assert_eq!(Mode::Service.service_port(8765), Some(8765));
    assert_eq!(Mode::Kernel.service_port(8765), None);
    assert!(Mode::Both.kernel_serves() && Mode::Both.service_serves());
    assert!(!Mode::Service.kernel_serves() && !Mode::Kernel.service_serves());
}

#[test]
fn offer_table() {
    for t in ["version", "hw_info", "metrics", "events", "log", "ktest", "output", "stop", "reload_info", "system_list"] {
        assert_eq!(offer(t), Offer::Gate, "{t}");
    }
    for t in ["install_kernel", "install_system", "reload", "system_pin", "reboot"] {
        assert_eq!(offer(t), Offer::Installer, "{t}");
    }
    for t in ["read_phys", "msr_read", "mmio_write", "pci_write", "run", "write_file", "read_file", "send_keys", "screen", "snd", "some_future_tool"] {
        assert!(matches!(offer(t), Offer::Dropped(_)), "{t}");
    }
    assert_eq!(known_dropped("run"), Some(WHY_PROCESSES));
    assert_eq!(known_dropped("sched"), Some(WHY_OTHER));
    assert_eq!(known_dropped("hw_info"), None);
    assert_eq!(known_dropped("typo_tool"), None);
    let args = Json::parse(r#"{"pid":1}"#).unwrap();
    assert!(process_target("stop", &args).is_some() && process_target("output", &args).is_some());
    assert!(process_target("stop", &Json::parse(r#"{"call":3,"pid":null}"#).unwrap()).is_none());
    assert!(installer_text("reload", 1).contains("EPERM"));
}

/// MC-10: a live grant offers every kernel tool and every registered service tool to that client; without one
/// the table is the narrow one, byte for byte.
#[test]
fn offer_table_with_and_without_a_grant() {
    for t in ["run", "write_file", "read_file", "screen", "send_keys", "install_kernel", "reboot", "ktest", "read_phys", "some_future_tool", "hw_info"] {
        assert_eq!(offer_for(t, true), Offer::Gate, "{t}");
        assert_eq!(offer_for(t, false), offer(t), "{t}");
    }
    assert_eq!(service_offer_for(Safety::DeleteOverwrite, false, true), Some(Offer::Gate));
    assert_eq!(service_offer_for(Safety::DeleteOverwrite, false, false), None);
    assert_eq!(service_offer_for(Safety::Change, true, false), Some(Offer::Installer));
    assert_eq!(service_offer_for(Safety::Read, true, false), Some(Offer::Gate));
}
