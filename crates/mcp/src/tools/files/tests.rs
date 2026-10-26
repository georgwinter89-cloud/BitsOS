//! Host tests of the file tools on the host's own VFS root view (an empty tmpfs; every test works in its own
//! top-level directory, made with `parents:true`) and of `fetch` against a std HTTP server on 127.0.0.1.

extern crate std;
use super::*;
use crate::{Json, TOOLS, rpc};
use std::io::{Read, Write};
use std::string::ToString;

fn call(name: &str, args: &str) -> Json {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#);
    let a = rpc::handle(body.as_bytes(), 1_000_000_000, TOOLS.entries());
    Json::parse(&a.body).unwrap()
}
fn text_of(j: &Json) -> String {
    let r = j.get("result").unwrap_or_else(|| panic!("no result: {}", j.to_string()));
    r.get("content").and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string()
}
fn failed(j: &Json) -> bool {
    j.get("result").and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true)
}
fn code(j: &Json) -> Option<i64> {
    j.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64)
}
fn ok(name: &str, args: &str) -> String {
    let j = call(name, args);
    assert!(!failed(&j) && code(&j).is_none(), "{name} {args}: {}", j.to_string());
    text_of(&j)
}
fn refused(name: &str, args: &str) -> String {
    let j = call(name, args);
    let t = text_of(&j);
    assert!(failed(&j) && t.starts_with("Refused: "), "{name} {args} should be refused: {}", j.to_string());
    t
}
fn error(name: &str, args: &str) -> String {
    let j = call(name, args);
    let t = text_of(&j);
    assert!(failed(&j) && t.starts_with("ERROR: "), "{name} {args} should fail: {}", j.to_string());
    t
}
fn json(name: &str, args: &str) -> Json {
    Json::parse(&ok(name, args)).unwrap()
}
fn b64(d: &[u8]) -> String {
    let mut s = String::new();
    crate::b64::encode_into(d, &mut s);
    s
}
fn read_all(path: &str) -> Vec<u8> {
    let j = json("read_file", &format!(r#"{{"path":"{path}"}}"#));
    crate::b64::decode(j.get("data").and_then(Json::as_str).unwrap()).unwrap()
}

#[test]
fn write_read_list_stat_delete() {
    let t = ok("write_file", r#"{"path":"/h1/a/b.txt","text":"hello\n","parents":true}"#);
    assert!(t.starts_with("write_file /h1/a/b.txt: 6 bytes written (new file); size now 6, mode 0644"), "{t}");
    assert_eq!(read_all("/h1/a/b.txt"), b"hello\n");
    // offset in the middle, past the end (a hole), append
    ok("write_file", r#"{"path":"/h1/a/b.txt","text":"J","offset":0}"#);
    ok("write_file", r#"{"path":"/h1/a/b.txt","data":"QUJD","offset":"10"}"#);
    ok("write_file", r#"{"path":"/h1/a/b.txt","text":"!","append":true}"#);
    assert_eq!(read_all("/h1/a/b.txt"), b"Jello\n\0\0\0\0ABC!");
    // ranges and text
    let j = json("read_file", r#"{"path":"/h1/a/b.txt","offset":1,"length":4,"encoding":"text"}"#);
    assert_eq!(j.get("text").and_then(Json::as_str), Some("ello"));
    assert_eq!(j.get("eof").and_then(Json::as_bool), Some(false));
    assert_eq!(j.get("next_offset").and_then(Json::as_u64), Some(5));
    assert_eq!(j.get("size").and_then(Json::as_u64), Some(14));
    let j = json("read_file", r#"{"path":"/h1/a/b.txt","offset":100}"#);
    assert_eq!((j.get("length").and_then(Json::as_u64), j.get("eof").and_then(Json::as_bool)), (Some(0), Some(true)));
    // replace keeps the mode unless given; a new mode
    ok("write_file", r#"{"path":"/h1/a/b.txt","text":"new","mode":"0755"}"#);
    let s = json("stat", r#"{"path":"/h1/a/b.txt","hash":true}"#);
    assert_eq!(s.get("mode").and_then(Json::as_str), Some("0755"));
    assert_eq!(s.get("type").and_then(Json::as_str), Some("file"));
    assert_eq!(s.get("fs").and_then(Json::as_str), Some("tmpfs"));
    let mut h = Fnv::new();
    h.update(b"new");
    assert_eq!(s.get("fnv1a64").and_then(Json::as_str), Some(format!("{:#018x}", h.0).as_str()));
    ok("write_file", r#"{"path":"/h1/a/b.txt","text":"newer"}"#);
    assert_eq!(json("stat", r#"{"path":"/h1/a/b.txt"}"#).get("mode").and_then(Json::as_str), Some("0755"), "replace keeps the mode");
    // no temp file left beside it
    let l = ok("list_dir", r#"{"path":"/h1/a"}"#);
    assert!(l.starts_with("/h1/a: 1 entries (tmpfs)") && l.contains("-0755") && l.contains(" 5 b.txt"), "{l}");
    let l = ok("list_dir", r#"{"path":"/h1"}"#);
    assert!(l.contains("a/"), "{l}");
    // create:false
    assert!(error("write_file", r#"{"path":"/h1/a/c","text":"x","create":false}"#).contains("ENOENT"));
    assert!(error("write_file", r#"{"path":"/h1/a/c","text":"x","offset":3,"create":false}"#).contains("ENOENT"));
    // delete: non-empty dir needs recursive; then everything goes
    assert!(error("delete", r#"{"path":"/h1/a"}"#).contains("recursive:true"));
    assert!(ok("delete", r#"{"path":"/h1/a/b.txt"}"#).contains("file, 5 bytes"));
    assert!(ok("delete", r#"{"path":"/h1/a"}"#).contains("empty directory"));
    ok("write_file", r#"{"path":"/h1/x/y/z","text":"1","parents":true}"#);
    assert!(ok("delete", r#"{"path":"/h1/x","recursive":true}"#).contains("everything below"));
    assert!(error("stat", r#"{"path":"/h1/x"}"#).contains("ENOENT"));
    assert!(error("delete", r#"{"path":"/h1/x"}"#).contains("ENOENT"));
}

#[test]
fn a_big_file_in_pieces() {
    let data: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i * 7 + i / 251) as u8).collect();
    ok("write_file", r#"{"path":"/h2/big","text":"","parents":true}"#);
    for (i, c) in data.chunks(192 * 1024).enumerate() {
        ok("write_file", &format!(r#"{{"path":"/h2/big","data":"{}","offset":{}}}"#, b64(c), i * 192 * 1024));
    }
    let j = json("read_file", r#"{"path":"/h2/big"}"#);
    assert_eq!(j.get("length").and_then(Json::as_u64), Some(READ_MAX));
    assert_eq!(j.get("eof").and_then(Json::as_bool), Some(false));
    let first = crate::b64::decode(j.get("data").and_then(Json::as_str).unwrap()).unwrap();
    let j = json("read_file", &format!(r#"{{"path":"/h2/big","offset":{READ_MAX}}}"#));
    assert_eq!(j.get("eof").and_then(Json::as_bool), Some(true));
    let rest = crate::b64::decode(j.get("data").and_then(Json::as_str).unwrap()).unwrap();
    assert_eq!([first, rest].concat(), data);
    assert_eq!(code(&call("read_file", &format!(r#"{{"path":"/h2/big","length":{}}}"#, READ_MAX + 1))), Some(-32602));
}

/// Checker finding (B4): a sparse 1 TiB file (write_file at a far offset) is refused by stat's hash instead of
/// being read for minutes; hash_max bounds regular and proc files alike and is exact at its edge.
#[test]
fn stat_hash_is_bounded() {
    let ns = vfs::root_ns();
    ns.create_proc("/proc/h5info", || String::from("facts\n")).unwrap();
    ok("write_file", r#"{"path":"/h5/sparse","text":"x","offset":1099511627776,"parents":true}"#);
    let s = json("stat", r#"{"path":"/h5/sparse"}"#);
    assert_eq!(s.get("size").and_then(Json::as_u64), Some(1099511627777));
    let t = refused("stat", r#"{"path":"/h5/sparse","hash":true}"#);
    assert!(t.contains("has 1099511627777 bytes, more than hash_max 268435456"), "{t}");
    assert!(refused("stat", r#"{"path":"/h5/sparse","hash":true,"hash_max":"4096"}"#).contains("hash_max 4096"));
    // exactly at the bound: hashed; one byte below: refused
    ok("write_file", r#"{"path":"/h5/six","text":"abcdef"}"#);
    let mut h = Fnv::new();
    h.update(b"abcdef");
    let s = json("stat", r#"{"path":"/h5/six","hash":true,"hash_max":6}"#);
    assert_eq!(s.get("fnv1a64").and_then(Json::as_str), Some(format!("{:#018x}", h.0).as_str()));
    assert_eq!(s.get("hashed_bytes").and_then(Json::as_u64), Some(6));
    assert!(refused("stat", r#"{"path":"/h5/six","hash":true,"hash_max":5}"#).contains("more than hash_max 5"));
    // an empty file with hash_max 0
    ok("write_file", r#"{"path":"/h5/empty","text":""}"#);
    assert_eq!(json("stat", r#"{"path":"/h5/empty","hash":true,"hash_max":0}"#).get("hashed_bytes").and_then(Json::as_u64), Some(0));
    // a proc file has no trustworthy size: the read itself stops one byte past the bound
    let s = json("stat", r#"{"path":"/proc/h5info","hash":true}"#);
    assert_eq!(s.get("hashed_bytes").and_then(Json::as_u64), Some(6));
    assert!(refused("stat", r#"{"path":"/proc/h5info","hash":true,"hash_max":3}"#).contains("more than hash_max 3"));
    assert_eq!(code(&call("stat", r#"{"path":"/h5/six","hash":true,"hash_max":-1}"#)), Some(-32602));
    ok("delete", r#"{"path":"/h5/sparse"}"#);
}

#[test]
fn refusals() {
    let ns = vfs::root_ns();
    ns.create_device("/dev/h3null", alloc::sync::Arc::new(vfs::node::ProcFile::new(|| String::from("x"), uapi::S_IFCHR | 0o666))).unwrap();
    ns.create_proc("/proc/h3info", || String::from("facts\n")).unwrap();
    ns.create_file("/h3/ro", 0o444, b"ro".to_vec()).unwrap();
    ns.create_symlink("/h3/link", "/dev").unwrap();
    ns.create_symlink("/h3/flink", "/h3/ro").unwrap();
    // the kernel's own trees, also through a link
    assert!(refused("write_file", r#"{"path":"/dev/h3new","text":"x"}"#).contains("/dev holds the nodes"));
    assert!(refused("write_file", r#"{"path":"/h3/link/h3new","text":"x"}"#).contains("/dev holds"));
    assert!(refused("write_file", r#"{"path":"/proc/h3info","text":"x"}"#).contains("/proc holds"));
    assert!(refused("write_file", r#"{"path":"/sys/x","text":"x","parents":true}"#).contains("/sys holds") || error("write_file", r#"{"path":"/sys/x","text":"x"}"#).contains("ENOENT"));
    assert!(refused("write_file", r#"{"path":"/h3/../dev/h3null","text":"x"}"#).contains("/dev holds"));
    assert!(refused("delete", r#"{"path":"/dev/h3null"}"#).contains("/dev holds"));
    assert!(refused("fetch", r#"{"url":"http://10.0.2.2/x","path":"/proc/x"}"#).contains("/proc holds"));
    // nodes that are not writable files
    assert!(refused("write_file", r#"{"path":"/h3/ro","text":"x"}"#).contains("read-only (mode 0444)"));
    assert!(refused("write_file", r#"{"path":"/h3/flink","text":"x"}"#).contains("symbolic link"));
    assert!(error("write_file", r#"{"path":"/h3","text":"x"}"#).contains("EISDIR"));
    assert!(refused("write_file", r#"{"path":"/h3/new/","text":"x"}"#).contains("ends in '/'"));
    assert!(refused("write_file", r#"{"path":"/h3/..","text":"x"}"#).contains("does not name an entry"));
    assert!(refused("delete", r#"{"path":"/"}"#).contains("does not name an entry"));
    assert!(refused("delete", r#"{"path":"/h3"}"#).contains("top-level entry"));
    assert!(refused("read_file", r#"{"path":"/dev/h3null"}"#).contains("device node"));
    assert!(error("read_file", r#"{"path":"/h3"}"#).contains("EISDIR"));
    assert!(refused("stat", r#"{"path":"/dev/h3null","hash":true}"#).contains("not a file"));
    // reading a proc file works; deleting a link removes the link only
    assert_eq!(json("read_file", r#"{"path":"/proc/h3info","encoding":"text"}"#).get("text").and_then(Json::as_str), Some("facts\n"));
    assert_eq!(json("stat", r#"{"path":"/h3/link","follow":false}"#).get("target").and_then(Json::as_str), Some("/dev"));
    ok("delete", r#"{"path":"/h3/link"}"#);
    assert!(json("stat", r#"{"path":"/dev/h3null"}"#).get("type").and_then(Json::as_str) == Some("chardev"));
    // bad arguments
    for (tool, args) in [
        ("read_file", r#"{"path":"rel"}"#),
        ("read_file", r#"{"path":"/x","encoding":"hex"}"#),
        ("read_file", r#"{"path":"/x","offset":-1}"#),
        ("write_file", r#"{"path":"/h3/x"}"#),
        ("write_file", r#"{"path":"/h3/x","text":"a","data":"YQ=="}"#),
        ("write_file", r#"{"path":"/h3/x","data":"***"}"#),
        ("write_file", r#"{"path":"/h3/x","text":"a","offset":1,"append":true}"#),
        ("write_file", r#"{"path":"/h3/x","text":"a","mode":"999"}"#),
        ("write_file", r#"{"path":"/h3/x","text":"a","mode":493}"#),
        ("write_file", r#"{"path":"/h3/x","text":"a","offset":"18446744073709551615"}"#),
        ("write_file", r#"{"path":"/h3/../x","text":"a","parents":true}"#),
        ("list_dir", r#"{}"#),
        ("delete", r#"{"path":5}"#),
        ("fetch", r#"{"url":"http://10.0.0.1/x"}"#),
    ] {
        assert_eq!(code(&call(tool, args)), Some(-32602), "{tool} {args}");
    }
    assert!(ns.lookup("/h3/x").is_err(), "nothing was written by a refused call");
}

#[test]
fn config_txt_lines_only_georg_sets() {
    assert_eq!(config_guard(b"", b"token=abc\nmcp_port=8765\n"), Ok(()));
    assert!(config_guard(b"", b"nvme_write=on\n").is_err());
    assert!(config_guard(b"", b"stick_write=on\n").is_err(), "FS-6: the boot stick's write switch");
    assert!(config_guard(b"", b"Usb_Msd_Write=on\n").is_err(), "US-4: every USB disk's write switch");
    assert!(config_guard(b"", b" NVME_WRITE = on \r\n").is_err(), "case and spaces");
    assert!(config_guard(b"nvme_write=off\n", b"nvme_write=on\n").is_err(), "changed");
    assert_eq!(config_guard(b"nvme_write=on\nx=1\n", b"x=2\nnvme_write=on\n"), Ok(()), "kept as it was");
    assert_eq!(config_guard(b"wlan=off\n", b"wlan=on\n"), Ok(()), "wlan is free (R8d, 21.10.)");
    assert_eq!(config_guard(b"nvme_write=on\n", b"x=1\n"), Ok(()), "dropping it is allowed");
    assert!(config_guard(b"", b"\xef\xbb\xbfnvme_write=on").is_err(), "behind a BOM");
    assert_eq!(apply(b"abc", b"Z", Change::At(5)), b"abc\0\0Z");
    assert_eq!(apply(b"abc", b"Z", Change::Append), b"abcZ");
    assert_eq!(apply(b"abc", b"Z", Change::Replace), b"Z");
    // through the tool: any new or changed line of a guarded key is refused, also "off" (strict: only Georg edits
    // them), also when an append completes it; other lines change freely and a kept line stays allowed
    vfs::root_ns().create_file("/h4/CONFIG.TXT", 0o644, b"token=t\nstick_write=".to_vec()).unwrap();
    assert!(refused("write_file", r#"{"path":"/h4/CONFIG.TXT","text":"on\n","append":true}"#).contains("stick_write=on"));
    assert!(refused("write_file", r#"{"path":"/h4/config.txt","text":"nvme_write=off"}"#).contains("R8a"));
    assert!(refused("fetch", r#"{"url":"http://10.0.2.2/c","path":"/h4/CONFIG.TXT"}"#).contains("never writes a CONFIG.TXT"));
    ok("write_file", r#"{"path":"/h4/CONFIG.TXT","text":"token=u\nstick_write=","offset":0}"#);
    ok("write_file", r#"{"path":"/h4/CONFIG.TXT","text":"\nx=1\nwlan=on\n","append":true}"#);
    assert_eq!(read_all("/h4/CONFIG.TXT"), b"token=u\nstick_write=\nx=1\nwlan=on\n");
}

#[test]
fn urls_only_on_the_lan() {
    assert!(check_url("http://192.168.100.20:20010/x").is_ok());
    assert!(check_url("http://10.0.2.2/x").is_ok());
    assert!(check_url("http://8.8.8.8/x").unwrap_err().contains("not on a private network"));
    assert!(check_url("http://172.32.0.1/x").unwrap_err().contains("not on a private network"));
    assert!(check_url("https://192.168.1.1/x").unwrap_err().contains("https"));
    assert!(check_url("http://example.org/x").unwrap_err().contains("DNS"));
    assert!(check_url("ftp://10.0.0.1/x").unwrap_err().contains("bad URL"));
    assert!(refused("fetch", r#"{"url":"http://1.1.1.1/x","path":"/h5/x"}"#).contains("R9"));
}

/// A one-shot HTTP server on 127.0.0.1 answering `status` with `body`; returns the port.
fn serve_once(status: &'static str, body: Vec<u8>) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = l.accept() {
            let mut req = [0u8; 2048];
            let _ = s.read(&mut req);
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });
    port
}

#[test]
fn fetch_into_a_file() {
    let ns = vfs::root_ns();
    ns.mkdir_p("/h6").unwrap();
    let body: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    let port = serve_once("200 OK", body.clone());
    let url = format!("http://127.0.0.1:{port}/blob");
    let ctx = crate::ToolContext::detached(u64::MAX);
    let pl = place(&ns, "fetch", "/h6/blob", false).unwrap();
    let r = fetch_into(&ctx, &net::host::StdSockets, &url, &ns, &pl, 0o755, None).unwrap();
    let mut h = Fnv::new();
    h.update(&body);
    let crate::ToolOutput::Text(t) = &r[0] else { panic!() };
    assert!(t.contains("-> /h6/blob: 300000 bytes") && t.contains(&format!("{:#018x}", h.0)), "{t}");
    assert_eq!(ns.read_file("/h6/blob", 1 << 20).unwrap(), body);
    assert_eq!(ns.lookup("/h6/blob").unwrap().inode.meta.mode() & 0o7777, 0o755);
    // a 404 keeps the old file and leaves no temp file
    let port = serve_once("404 Not Found", b"no".to_vec());
    let url = format!("http://127.0.0.1:{port}/gone");
    let pl = place(&ns, "fetch", "/h6/blob", false).unwrap();
    let e = fetch_into(&ctx, &net::host::StdSockets, &url, &ns, &pl, 0o644, None).unwrap_err();
    assert!(matches!(&e, crate::ToolError::Failed(t) if t.contains("HTTP 404")), "{e:?}");
    assert_eq!(ns.read_file("/h6/blob", 1 << 20).unwrap(), body);
    // too large for max_bytes
    let port = serve_once("200 OK", alloc::vec![7u8; 5000]);
    let url = format!("http://127.0.0.1:{port}/big");
    let pl = place(&ns, "fetch", "/h6/big", false).unwrap();
    let e = fetch_into(&ctx, &net::host::StdSockets, &url, &ns, &pl, 0o644, Some(1000)).unwrap_err();
    assert!(matches!(&e, crate::ToolError::Failed(t) if t.contains("max_bytes")), "{e:?}");
    assert!(ns.lookup("/h6/big").is_err());
    let names: Vec<_> = ns.list_dir("/h6").unwrap().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, alloc::vec![b"blob".to_vec()], "no temp files left");
}

#[test]
fn temp_names_stay_short_and_hidden() {
    let t = temp_name(&[b'a'; 255]);
    assert!(t.len() <= 255 && t[0] == b'.' && t.ends_with(b"~"));
    assert_ne!(temp_name(b"x"), temp_name(b"x"));
    assert_eq!(top_level("/dev/null"), "dev");
    assert_eq!(top_level("/"), "");
}

/// MC-4b: `read_whole` hands another tool (install_kernel's paths) a whole uploaded file, or refuses.
#[test]
fn read_whole_gives_uploads_whole_or_not_at_all() {
    let data: std::vec::Vec<u8> = (0..300_000u32).map(|i| (i * 7 ^ (i >> 5)) as u8).collect();
    ok("write_file", &format!(r#"{{"path":"/h9/up/KERNEL","data":"{}","parents":true}}"#, b64(&data)));
    let ctx = crate::ToolContext::detached(u64::MAX);
    assert_eq!(read_whole(&ctx, "install_kernel", "/h9/up/KERNEL", 300_000).unwrap(), data);
    // Over the caller's limit: refused, nothing read in part.
    let e = read_whole(&ctx, "install_kernel", "/h9/up/KERNEL", 299_999).unwrap_err();
    assert!(matches!(&e, ToolError::Refused(t) if t.contains("has 300000 bytes, more than")), "{e:?}");
    ok("write_file", r#"{"path":"/h9/up/empty","text":""}"#);
    assert_eq!(read_whole(&ctx, "install_kernel", "/h9/up/empty", 0).unwrap(), b"");
    assert!(matches!(read_whole(&ctx, "install_kernel", "/h9/up", 1 << 20), Err(ToolError::Failed(t)) if t.contains("EISDIR")));
    assert!(matches!(read_whole(&ctx, "install_kernel", "/h9/up/none", 1 << 20), Err(ToolError::Failed(t)) if t.contains("ENOENT")));
    assert!(matches!(read_whole(&ctx, "install_kernel", "h9/up/KERNEL", 1 << 20), Err(ToolError::BadArgument(_))));
    assert!(matches!(read_whole(&ctx, "install_kernel", "/h9/\0", 1 << 20), Err(ToolError::BadArgument(_))));
    let long = format!("/{}", "a".repeat(PATH_MAX));
    assert!(matches!(read_whole(&ctx, "install_kernel", &long, 1 << 20), Err(ToolError::BadArgument(_))));
    ok("delete", r#"{"path":"/h9/up","recursive":true}"#);
}

#[test]
fn config_guard_sees_the_last_line_of_a_key() {
    // FS-7 CR10: swapping two guarded lines changes the value that counts.
    assert!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=off\nnvme_write=on\n").is_err());
    assert!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=on\n").is_err(), "dropping the last line turns it on");
    assert_eq!(config_guard(b"nvme_write=on\nnvme_write=off\n", b"nvme_write=on\nnvme_write=off\ntoken=x\n"), Ok(()));
    assert_eq!(config_guard(b"nvme_write=off\n", b""), Ok(()), "dropping every guarded line stays allowed");
}

fn num(j: &Json, k: &str) -> Option<u64> {
    j.get(k).and_then(Json::as_i64).map(|n| n as u64)
}

/// FS-15 MC-1: on a FAT volume stat reports a file's real size after a shorter (O_TRUNC) and a longer (pwrite
/// past the end) rewrite, with no `redacted` field; /boot/BITSOS2/CONFIG.TXT reports the size read_file shows
/// (secrets as ***), marked redacted:true. The volume sits at /boot of the test root (no other test uses /boot).
#[test]
fn stat_sizes_on_fat_and_the_redacted_config() {
    let dir = std::env::temp_dir().join(format!("bitsos-mc1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    let _ = std::fs::remove_file(&img);
    let o = std::process::Command::new("mkfs.fat").args(["-F", "32", "-n", "MC1", "-C"]).arg(&img).arg("65536").output().unwrap();
    assert!(o.status.success(), "mkfs.fat: {}", String::from_utf8_lossy(&o.stderr));
    let dev = std::sync::Arc::new(fs_fat::MemDev::new(std::fs::read(&img).unwrap(), 512));
    let _ = std::fs::remove_dir_all(&dir);
    let vol = fs_fat::Volume::mount(dev, fs_fat::Options { writable: true, stick: false }, fs_fat::Clock::UTC_EPOCH).unwrap();
    let ns = vfs::root_ns();
    fs_fat::glue::mount(&vol, &ns, "/boot", false).unwrap();
    ns.mkdir_p("/boot/BITSOS2").unwrap();
    let plain = "/boot/BITSOS2/DATA.BIN";
    let stat = |p: &str| json("stat", &format!(r#"{{"path":"{p}"}}"#));
    ns.write_file(plain, &[7u8; 5000], Some(0)).unwrap();
    let f = ns.open_flags(plain.as_bytes(), vfs::abi::O_WRONLY | vfs::abi::O_TRUNC, 0).unwrap();
    f.write(b"short").unwrap();
    drop(f);
    let s = stat(plain);
    assert_eq!(num(&s, "size"), Some(5), "after O_TRUNC: {}", s.to_string());
    assert!(s.get("redacted").is_none(), "a plain file is not redacted: {}", s.to_string());
    ns.write_file(plain, b"end", Some(9000)).unwrap();
    let s = stat(plain);
    assert_eq!(num(&s, "size"), Some(9003), "after a pwrite past the end: {}", s.to_string());
    assert!(s.get("redacted").is_none());
    let (token, psk) = ("abcdefghijklmnop", "a long wlan passphrase");
    let cfg = format!("mcp_port=8765\ntoken={token}\nwlan_psk={psk}\nwlan=on\n");
    ns.write_file("/boot/BITSOS2/CONFIG.TXT", cfg.as_bytes(), Some(0)).unwrap();
    let real = ns.lookup_path(b"/boot/BITSOS2/CONFIG.TXT", true).unwrap().attr().size;
    assert_eq!(real, cfg.len() as u64);
    let shown = json("read_file", r#"{"path":"/boot/BITSOS2/CONFIG.TXT","encoding":"text"}"#);
    let text = shown.get("text").and_then(Json::as_str).unwrap().to_string();
    assert!(text.contains("token=***\n") && text.contains("wlan_psk=***\n"), "{text}");
    let s = json("stat", r#"{"path":"/boot/BITSOS2/CONFIG.TXT","hash":true}"#);
    assert_eq!(num(&s, "size"), Some(text.len() as u64), "stat size is read_file's: {}", s.to_string());
    assert_eq!(s.get("redacted").and_then(Json::as_bool), Some(true), "{}", s.to_string());
    assert_eq!(real - text.len() as u64, (token.len() + psk.len() - 6) as u64);
}
