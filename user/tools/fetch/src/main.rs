//! fetch: one HTTPS (or HTTP) GET into a file, with progress on stderr, the SHA-256 at the end and exit codes
//! scripts can branch on. Same static binary on CachyOS and V2 (UL-4a).

mod args;
mod conn;
mod http;
mod sink;
mod tls;
mod url;

use args::Opts;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Write};
use std::time::{Duration, Instant, SystemTime};
use url::Url;

/// Exit codes (also in `--help`); stable, scripts and the ktests depend on them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Code {
    Usage = 2,
    Dns = 3,
    Connect = 4,
    Tls = 5,
    Clock = 6,
    Status = 7,
    Protocol = 8,
    Write = 9,
    Hash = 10,
    Timeout = 11,
}

#[derive(Debug)]
pub struct Fail(pub Code, pub String);

pub type Res<T> = Result<T, Fail>;

fn main() {
    let code = match args::parse(std::env::args().skip(1)) {
        Ok(None) => 0,
        Ok(Some(o)) => match run(&o) {
            Ok(()) => 0,
            Err(Fail(c, m)) => {
                eprintln!("fetch: {m}");
                c as i32
            }
        },
        Err(m) => {
            eprintln!("fetch: {m}\n(fetch --help shows the usage)");
            Code::Usage as i32
        }
    };
    std::process::exit(code);
}

/// A clock before 2024-01-01 is wrong (no certificate in use was issued that early and still valid).
const SANE_CLOCK: u64 = 1_704_067_200;

fn run(o: &Opts) -> Res<()> {
    let mut url = Url::parse(&o.url).map_err(|m| Fail(Code::Usage, m))?;
    let out = o.out.clone().unwrap_or_else(|| url.file_name().unwrap_or("index.html").to_string());
    let mut tls_cfg = None;
    let mut redirects = 0u32;
    let end = o.max_time.and_then(|t| Instant::now().checked_add(t));
    let (mut body, total) = loop {
        if url.https && tls_cfg.is_none() {
            let bundle = tls::bundle_path(o.ca.as_deref()).map_err(|m| Fail(Code::Tls, m))?;
            tls_cfg = Some(tls::config(&bundle, o.verify_time).map_err(|m| Fail(Code::Tls, m))?);
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            if o.verify_time.is_none() && now < SANE_CLOCK && !o.quiet {
                eprintln!("fetch: warning: the system clock says {now} s after 1970; certificates will look not yet valid (--verify-time SECS)");
            }
        }
        let mut s = conn::open(&url, o.timeout, end, tls_cfg.as_ref())?;
        s.write_all(&http::request(&url.host_header(), &url.target)).and_then(|_| s.flush()).map_err(|e| conn::classify(e, conn::Phase::Request, o.timeout))?;
        let mut r = BufReader::with_capacity(64 * 1024, s);
        let head = http::read_head(&mut r).map_err(|e| conn::http_fail(e, o.timeout))?;
        if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
            // wget treats a redirect without a target as a server error too (curl would keep the 3xx body).
            let loc = head.get("location").ok_or_else(|| Fail(Code::Protocol, format!("HTTP {} redirect without a Location header", head.status)))?;
            redirects += 1;
            if redirects > o.max_redirects {
                return Err(Fail(Code::Protocol, format!("more than {} redirects (--max-redirects N)", o.max_redirects)));
            }
            let next = url.join(loc).map_err(|m| Fail(Code::Protocol, m))?;
            if url.https && !next.https {
                return Err(Fail(Code::Protocol, format!("refusing a redirect from HTTPS to plain HTTP: {loc}")));
            }
            if !o.quiet {
                eprintln!("fetch: HTTP {} -> {loc}", head.status);
            }
            url = next;
            continue;
        }
        if !(200..300).contains(&head.status) {
            return Err(Fail(Code::Status, format!("HTTP {} {}", head.status, head.reason)));
        }
        let framing = head.framing().map_err(|e| conn::http_fail(e, o.timeout))?;
        let total = if let http::Framing::Length(n) = framing { Some(n) } else { None };
        break (http::Body::new(r, framing), total);
    };
    let mut sink = sink::Sink::open(&out)?;
    let hash = match copy(&mut body, &mut sink, total, o) {
        Ok(h) => h,
        Err(f) => {
            sink.discard();
            return Err(f);
        }
    };
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(want) = &o.sha256
        && *want != hex
    {
        sink.discard();
        return Err(Fail(Code::Hash, format!("SHA-256 mismatch: got {hex}, want {want}")));
    }
    let to_stdout = sink.is_stdout();
    sink.commit()?;
    if to_stdout {
        eprintln!("sha256 {hex}");
    } else {
        println!("{hex}  {out}");
    }
    Ok(())
}

fn copy(body: &mut impl Read, sink: &mut sink::Sink, total: Option<u64>, o: &Opts) -> Res<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut p = Progress::new(total, o.quiet);
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                p.abort();
                return Err(conn::classify(e, conn::Phase::Body, o.timeout));
            }
        };
        hasher.update(&buf[..n]);
        sink.write(&buf[..n])?;
        p.tick(n as u64);
    }
    p.done();
    Ok(hasher.finalize().into())
}

/// Progress on stderr: a self-overwriting line on a terminal, a line every few seconds otherwise (logs, MCP).
struct Progress {
    total: Option<u64>,
    got: u64,
    start: Instant,
    last: Instant,
    tty: bool,
    quiet: bool,
}

impl Progress {
    fn new(total: Option<u64>, quiet: bool) -> Progress {
        use std::io::IsTerminal;
        let now = Instant::now();
        Progress { total, got: 0, start: now, last: now, tty: std::io::stderr().is_terminal(), quiet }
    }

    fn line(&self) -> String {
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let rate = format!("{}/s", size(self.got as f64 / secs));
        match self.total {
            Some(t) if t > 0 => format!("{} / {} ({}%) {rate}", size(self.got as f64), size(t as f64), self.got * 100 / t),
            _ => format!("{} {rate}", size(self.got as f64)),
        }
    }

    fn tick(&mut self, n: u64) {
        self.got += n;
        let every = if self.tty { Duration::from_millis(250) } else { Duration::from_secs(5) };
        if self.quiet || self.last.elapsed() < every {
            return;
        }
        self.last = Instant::now();
        if self.tty {
            eprint!("\rfetch: {}\x1b[K", self.line());
        } else {
            eprintln!("fetch: {}", self.line());
        }
    }

    fn abort(&self) {
        if self.tty && !self.quiet && self.got > 0 {
            eprintln!();
        }
    }

    fn done(&self) {
        if !self.quiet {
            let secs = self.start.elapsed().as_secs_f64();
            eprintln!("{}fetch: {} in {secs:.1} s", if self.tty { "\r\x1b[K" } else { "" }, self.line());
        }
    }
}

fn size(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{v:.0} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
