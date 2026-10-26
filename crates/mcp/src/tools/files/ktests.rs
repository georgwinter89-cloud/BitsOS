//! In-kernel tests of MC-4a over the kernel loopback (a server of the test's own, the kernel's real tool list):
//! the file tools on the booted root view (incl. the initramfs files and the kernel's /dev and /proc nodes) and
//! `fetch` from a tiny HTTP server thread on 127.0.0.1. All Safe and rerunnable: each works in its own scratch
//! folder below /tmp, removed before and after.

use super::Fnv;
use crate::kernel::SEC;
use crate::testclient::{Client, with_server};
use crate::{Json, TOOLS, b64};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use net::{Ipv4, SockAddr};

const PORT: u16 = 25070;
/// The fetch test's HTTP server (the next ones of 16 when taken).
const HTTP_PORT: u16 = 25090;

fn scratch(name: &str) -> String {
    let p = format!("/tmp/mc4a-{name}");
    let ns = vfs::root_ns();
    let _ = ns.remove_tree(&p);
    let _ = ns.mkdir_p(&p);
    p
}

fn json(c: &mut Client, tool: &str, args: &str) -> Result<Json, String> {
    let t = c.ok(tool, args)?;
    Json::parse(&t).map_err(|e| format!("{tool}: not JSON ({e:?}): {t}"))
}

fn refused(c: &mut Client, tool: &str, args: &str, want: &str) -> TestResult {
    let r = c.call(tool, args)?;
    kassert!(r.error && r.text.starts_with("Refused: ") && r.text.contains(want), "{tool} {args}: want a refusal with '{want}', got {:?} {}", r.rpc_error, r.text);
    Ok(())
}

fn files_roundtrip() -> TestResult {
    let d = scratch("roundtrip");
    let r = with_server(PORT, TOOLS.entries(), |c| {
        let t = c.ok("write_file", &format!(r#"{{"path":"{d}/sub/a.txt","text":"hello\n","parents":true}}"#))?;
        kassert!(t.contains("6 bytes written (new file)"), "{t}");
        c.ok("write_file", &format!(r#"{{"path":"{d}/sub/a.txt","text":"!","append":true}}"#))?;
        c.ok("write_file", &format!(r#"{{"path":"{d}/sub/a.txt","text":"J","offset":0}}"#))?;
        let j = json(c, "read_file", &format!(r#"{{"path":"{d}/sub/a.txt","encoding":"text"}}"#))?;
        kassert!(j.get("text").and_then(Json::as_str) == Some("Jello\n!"), "{}", j.to_string());
        // A binary file in pieces (each request below 256 KB), read back in two calls, hash compared.
        let data: Vec<u8> = (0..(2 << 20) + 4321u32).map(|i| (i ^ (i >> 9)) as u8).collect();
        let piece = 128 * 1024;
        for (i, ch) in data.chunks(piece).enumerate() {
            let mut s = String::new();
            b64::encode_into(ch, &mut s);
            let how = if i == 0 { String::from(r#""mode":"0755""#) } else { format!(r#""offset":{}"#, i * piece) };
            c.ok("write_file", &format!(r#"{{"path":"{d}/bin.dat","data":"{s}",{how}}}"#))?;
        }
        let mut back = Vec::new();
        let mut off = 0u64;
        loop {
            let j = json(c, "read_file", &format!(r#"{{"path":"{d}/bin.dat","offset":{off}}}"#))?;
            back.extend(b64::decode(j.get("data").and_then(Json::as_str).unwrap_or("")).ok_or("no base64")?);
            match j.get("next_offset").and_then(Json::as_u64) {
                Some(n) => off = n,
                None => break,
            }
        }
        kassert!(back == data, "read back {} bytes, wrote {}", back.len(), data.len());
        let mut h = Fnv::new();
        h.update(&data);
        let s = json(c, "stat", &format!(r#"{{"path":"{d}/bin.dat","hash":true}}"#))?;
        kassert!(s.get("fnv1a64").and_then(Json::as_str) == Some(format!("{:#018x}", h.0).as_str()) && s.get("mode").and_then(Json::as_str) == Some("0755"), "{}", s.to_string());
        let l = c.ok("list_dir", &format!(r#"{{"path":"{d}"}}"#))?;
        kassert!(l.contains("2 entries (tmpfs)") && l.contains("sub/") && l.contains(&format!("{} bin.dat", data.len())), "{l}");
        // The initramfs and the kernel's own nodes can be read and listed.
        let l = c.ok("list_dir", r#"{"path":"/"}"#)?;
        kassert!(l.contains("tmp/") && l.contains("dev/"), "{l}");
        let s = json(c, "stat", r#"{"path":"/dev/console"}"#)?;
        kassert!(s.get("type").and_then(Json::as_str) == Some("chardev"), "{}", s.to_string());
        // Delete: a non-empty directory only with recursive.
        let r = c.call("delete", &format!(r#"{{"path":"{d}/sub"}}"#))?;
        kassert!(r.error && r.text.contains("recursive:true"), "{}", r.text);
        c.ok("delete", &format!(r#"{{"path":"{d}","recursive":true}}"#))?;
        let r = c.call("stat", &format!(r#"{{"path":"{d}"}}"#))?;
        kassert!(r.error && r.text.contains("ENOENT"), "{}", r.text);
        Ok(())
    });
    let _ = vfs::root_ns().remove_tree(&d);
    r
}
ktest!("mc.files.roundtrip", Safe, files_roundtrip);

fn files_refusals() -> TestResult {
    let d = scratch("refusals");
    let r = with_server(PORT, TOOLS.entries(), |c| {
        refused(c, "write_file", r#"{"path":"/dev/console","text":"x"}"#, "/dev holds")?;
        refused(c, "write_file", r#"{"path":"/dev/mc4a-new","text":"x"}"#, "/dev holds")?;
        refused(c, "write_file", r#"{"path":"/tmp/../dev/mc4a","text":"x"}"#, "/dev holds")?;
        refused(c, "read_file", r#"{"path":"/dev/console"}"#, "device node")?;
        refused(c, "delete", r#"{"path":"/dev/console"}"#, "/dev holds")?;
        // FS-5: /tmp is a tmpfs mount of its own (refused as a mount point); /bin is a plain top-level entry.
        refused(c, "delete", r#"{"path":"/tmp"}"#, "mount point")?;
        refused(c, "delete", r#"{"path":"/bin"}"#, "top-level entry")?;
        refused(c, "delete", r#"{"path":"/"}"#, "does not name an entry")?;
        c.ok("write_file", &format!(r#"{{"path":"{d}/CONFIG.TXT","text":"token=x\n","parents":true}}"#))?;
        refused(c, "write_file", &format!(r#"{{"path":"{d}/CONFIG.TXT","text":"nvme_write=on\n","append":true}}"#), "R8a")?;
        refused(c, "write_file", &format!(r#"{{"path":"{d}/CONFIG.TXT","text":"stick_write=on"}}"#), "R8a")?;
        let j = json(c, "read_file", &format!(r#"{{"path":"{d}/CONFIG.TXT","encoding":"text"}}"#))?;
        kassert!(j.get("text").and_then(Json::as_str) == Some("token=x\n"), "unchanged: {}", j.to_string());
        let r = c.call("write_file", &format!(r#"{{"path":"{d}/x","text":"a","offset":1,"append":true}}"#))?;
        kassert!(r.rpc_error == Some(-32602), "offset+append: {:?} {}", r.rpc_error, r.text);
        let r = c.call("read_file", r#"{"path":"tmp/x"}"#)?;
        kassert!(r.rpc_error == Some(-32602), "relative path: {:?}", r.rpc_error);
        // B4 (fixer): a sparse 1 TiB file is refused by stat's hash at once, not read for minutes.
        // Since FS-14 tmpfs charges pages when allocated (Linux VM_NORESERVE), so the 1 TiB sparse file costs one page
        // and its hash is refused by its size at once; then the bounded hash is checked on a small file (INT-A1b).
        let r = c.call("write_file", &format!(r#"{{"path":"{d}/sparse","text":"x","offset":1099511627776}}"#))?;
        kassert!(r.rpc_error.is_none() && r.text.contains("1 bytes written"), "1 TiB tmpfs file: {:?} {}", r.rpc_error, r.text);
        refused(c, "stat", &format!(r#"{{"path":"{d}/sparse","hash":true}}"#), "more than hash_max")?;
        c.ok("delete", &format!(r#"{{"path":"{d}/sparse"}}"#))?;
        c.ok("write_file", &format!(r#"{{"path":"{d}/sparse","text":"x","offset":8192}}"#))?;
        refused(c, "stat", &format!(r#"{{"path":"{d}/sparse","hash":true,"hash_max":4096}}"#), "has 8193 bytes, more than hash_max 4096")?;
        // A proc file of the test's own (EEXIST on a rerun in the same boot is fine: same content).
        let _ = vfs::root_ns().create_proc("/proc/mc4a-hash", || String::from("facts\n"));
        refused(c, "stat", r#"{"path":"/proc/mc4a-hash","hash":true,"hash_max":3}"#, "more than hash_max 3")?;
        let s = json(c, "stat", r#"{"path":"/proc/mc4a-hash","hash":true}"#)?;
        kassert!(s.get("hashed_bytes").and_then(Json::as_u64) == Some(6), "{}", s.to_string());
        Ok(())
    });
    let _ = vfs::root_ns().remove_tree(&d);
    r
}
ktest!("mc.files.refusals", Safe, files_refusals);

/// WL-4 CR 4 (R8 i): read_file and stat show the stick's CONFIG.TXT with every secret value as `***`; the rest
/// is the file as it is. Reads only (rerunnable); skips without a stick CONFIG.TXT.
fn config_redacted() -> TestResult {
    const CFG: &str = "/boot/BITSOS2/CONFIG.TXT";
    let Ok(raw) = vfs::root_ns().read_file(CFG, 1 << 20) else {
        return ktest::skip("no /boot/BITSOS2/CONFIG.TXT");
    };
    let want = fs_fat::protect::redact_config(&raw);
    with_server(PORT, TOOLS.entries(), |c| {
        let j = json(c, "read_file", &format!(r#"{{"path":"{CFG}","encoding":"text"}}"#))?;
        let text = j.get("text").and_then(Json::as_str).unwrap_or_default();
        kassert!(text.as_bytes() == want.as_slice(), "read_file shows the redacted file: {text:?}");
        for line in text.lines() {
            let l = line.trim();
            for k in fs_fat::protect::SECRET_KEYS {
                if let Some((n, v)) = l.split_once('=')
                    && n.trim() == *k
                {
                    kassert!(v == fs_fat::protect::REDACTED, "{k} shown as {v:?}");
                }
            }
        }
        let st = json(c, "stat", &format!(r#"{{"path":"{CFG}","hash":true}}"#))?;
        let size = st.get("size").and_then(|n| if let Json::Num(n) = n { Some(*n as usize) } else { None });
        kassert!(size == Some(want.len()), "stat size is the shown size: {size:?} vs {}", want.len());
        // FS-15 MC-1: marked as the redacted view; with a secret set it is shorter than the file itself.
        kassert!(st.get("redacted").and_then(Json::as_bool) == Some(true), "stat does not say redacted: {}", st.to_string());
        if want != raw {
            kassert!(want.len() < raw.len(), "a set secret makes the shown size {} smaller than the file's {}", want.len(), raw.len());
        }
        Ok(())
    })
}
ktest!("mc.files.config_redacted", Safe, config_redacted);

/// A one-request HTTP server thread on 127.0.0.1 (the first free port from HTTP_PORT): answers `status` with
/// `body`, then ends. Returns the port.
fn serve_once(status: &'static str, body: Vec<u8>) -> Result<(u16, sched::JoinHandle), String> {
    let sock = net::stack().ok_or("no socket stack")?;
    let (l, port) = (0..16u16)
        .find_map(|i| sock.tcp_listen(SockAddr { ip: Ipv4::LOCALHOST, port: HTTP_PORT + i }, 2).ok().map(|l| (l, HTTP_PORT + i)))
        .ok_or("no free port for the HTTP server")?;
    let h = sched::spawn("mc4a-http", sched::Class::Normal, move || {
        let Ok(s) = l.accept(Some(10 * SEC)) else { return };
        let mut req = Vec::new();
        let mut buf = [0u8; 512];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 8192 {
            match s.read(&mut buf, Some(5 * SEC)) {
                Ok(0) | Err(_) => break,
                Ok(n) => req.extend_from_slice(&buf[..n]),
            }
        }
        let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        if s.write_all(head.as_bytes(), Some(5 * SEC)).is_ok() {
            let _ = s.write_all(&body, Some(10 * SEC));
        }
        s.close();
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    Ok((port, h))
}

fn fetch_loopback() -> TestResult {
    let d = scratch("fetch");
    let r = with_server(PORT, TOOLS.entries(), |c| {
        let body: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        let mut h = Fnv::new();
        h.update(&body);
        let (port, srv) = serve_once("200 OK", body.clone())?;
        let t = c.ok("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/blob","path":"{d}/blob","mode":"0755","timeout_ms":60000}}"#));
        srv.join_timeout(5 * SEC);
        let t = t?;
        kassert!(t.contains("700000 bytes") && t.contains(&format!("{:#018x}", h.0)), "{t}");
        let back = vfs::root_ns().read_file(&format!("{d}/blob"), 1 << 20).map_err(|e| format!("read back: {e}"))?;
        kassert!(back == body, "fetched content differs");
        // A 404 leaves the file as it was and no temp file.
        let (port, srv) = serve_once("404 Not Found", Vec::from(&b"gone"[..]))?;
        let r = c.call("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/x","path":"{d}/blob"}}"#));
        srv.join_timeout(5 * SEC);
        let r = r?;
        kassert!(r.error && r.text.contains("HTTP 404"), "{}", r.text);
        let names = vfs::root_ns().list_dir(&d).map_err(|e| format!("list: {e}"))?;
        kassert!(names.len() == 1, "{} entries in {d} (a temp file left?)", names.len());
        // R9 and the address rules refuse before any connection.
        refused(c, "fetch", &format!(r#"{{"url":"http://8.8.8.8/x","path":"{d}/y"}}"#), "R9")?;
        refused(c, "fetch", &format!(r#"{{"url":"https://192.168.1.2/x","path":"{d}/y"}}"#), "https")?;
        refused(c, "fetch", &format!(r#"{{"url":"http://server.fritz.box/x","path":"{d}/y"}}"#), "DNS")?;
        Ok(())
    });
    let _ = vfs::root_ns().remove_tree(&d);
    sched::reap();
    r
}
ktest!("mc.files.fetch_loopback", Safe, fetch_loopback);
