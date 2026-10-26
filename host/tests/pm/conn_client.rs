//! PM-2 scenario program (static musl, std only): a program with several connections on the fake LAN.
//! Usage: conn_client <dns server> <hold_s> <target>...; a target is a name (resolved by its own DNS query on a
//! UDP socket, as a browser's resolver does) or an IPv4 address (no DNS), optionally with "+hold" (that
//! connection stays open for hold_s seconds after its answer, so the view can see it live).
//! Per target: one TCP connection to port 80, one request line, the answer "LEN <n>\n" + n bytes.
//! Prints `resolved <name> <ip> query=<bytes> answer=<bytes>`, `conn <target> <ip> out=<bytes> in=<bytes>`,
//! `holding <target>`, `done`; exit 1 on any failure.
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::time::Duration;

fn query(id: u16, name: &str) -> Vec<u8> {
    let mut m = vec![(id >> 8) as u8, id as u8, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for l in name.split('.') {
        m.push(l.len() as u8);
        m.extend_from_slice(l.as_bytes());
    }
    m.extend_from_slice(&[0, 0, 1, 0, 1]);
    m
}

fn skip_name(m: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *m.get(i)? as usize;
        if l & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        if l == 0 {
            return Some(i + 1);
        }
        i += 1 + l;
    }
}

fn first_a(m: &[u8]) -> Option<Ipv4Addr> {
    if m.len() < 12 || m[3] & 0xF != 0 {
        return None;
    }
    let an = u16::from_be_bytes([m[6], m[7]]);
    let mut i = skip_name(m, 12)? + 4;
    for _ in 0..an {
        i = skip_name(m, i)?;
        let ty = u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?]);
        let len = u16::from_be_bytes([*m.get(i + 8)?, *m.get(i + 9)?]) as usize;
        let d = m.get(i + 10..i + 10 + len)?;
        if ty == 1 && len == 4 {
            return Some(Ipv4Addr::new(d[0], d[1], d[2], d[3]));
        }
        i += 10 + len;
    }
    None
}

fn resolve(dns: Ipv4Addr, name: &str, id: u16) -> Result<Ipv4Addr, String> {
    let s = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    let q = query(id, name);
    let mut buf = [0u8; 1500];
    for _ in 0..3 {
        s.send_to(&q, (dns, 53)).map_err(|e| format!("send: {e}"))?;
        if let Ok((n, from)) = s.recv_from(&mut buf) {
            if from.port() == 53 && n >= 2 && buf[..2] == q[..2] {
                let ip = first_a(&buf[..n]).ok_or_else(|| format!("{name}: no address"))?;
                println!("resolved {name} {ip} query={} answer={n}", q.len());
                return Ok(ip);
            }
        }
    }
    Err(format!("{name}: no DNS answer"))
}

fn fetch(target: &str, ip: Ipv4Addr, hold_s: u64, hold: bool) -> Result<(), String> {
    let mut c = TcpStream::connect((ip, 80)).map_err(|e| format!("connect {ip}: {e}"))?;
    c.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let req = format!("GET {target}\n");
    c.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    let want = loop {
        if let Some(nl) = got.iter().position(|&b| b == b'\n') {
            let head = std::str::from_utf8(&got[..nl]).map_err(|_| "bad head")?;
            let n: usize = head.strip_prefix("LEN ").and_then(|x| x.parse().ok()).ok_or("bad head")?;
            break nl + 1 + n;
        }
        let k = c.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if k == 0 {
            return Err(String::from("EOF before the head"));
        }
        got.extend_from_slice(&buf[..k]);
    };
    while got.len() < want {
        let k = c.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if k == 0 {
            return Err(format!("EOF after {} of {want}", got.len()));
        }
        got.extend_from_slice(&buf[..k]);
    }
    println!("conn {target} {ip} out={} in={}", req.len(), got.len());
    if hold {
        println!("holding {target}");
        std::thread::sleep(Duration::from_secs(hold_s));
    }
    Ok(())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let run = || -> Result<(), String> {
        let dns: Ipv4Addr = a.get(1).and_then(|x| x.parse().ok()).ok_or("usage")?;
        let hold_s: u64 = a.get(2).and_then(|x| x.parse().ok()).ok_or("usage")?;
        let mut held = Vec::new();
        for (i, t) in a[3..].iter().enumerate() {
            let (name, hold) = match t.strip_suffix("+hold") {
                Some(n) => (n, true),
                None => (t.as_str(), false),
            };
            let ip = match name.parse::<Ipv4Addr>() {
                Ok(ip) => ip,
                Err(_) => resolve(dns, name, 0x5000 + i as u16)?,
            };
            if hold {
                held.push((name.to_string(), ip));
            } else {
                fetch(name, ip, hold_s, false)?;
            }
        }
        for (name, ip) in held {
            fetch(&name, ip, hold_s, true)?;
        }
        Ok(())
    };
    match run() {
        Ok(()) => println!("done"),
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    }
}
