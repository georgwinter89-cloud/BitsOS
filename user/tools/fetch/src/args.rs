//! Command line. `Ok(None)` means --help/--version was printed and fetch ends with 0.

use rustls::pki_types::UnixTime;
use std::time::Duration;

pub const USAGE: &str = "\
usage: fetch [options] URL
  Downloads URL (https:// or http://) with one GET, follows redirects, writes the body to a file
  (to FILE.part first, or FILE.part.N when that exists; renamed when complete; a symlink FILE is written
  through to its target) and prints \"<sha256>  <file>\" on stdout.
options:
  -o, --output FILE     where to write (default: the URL's last path segment, else index.html; - = stdout,
                        the hash line then goes to stderr as \"sha256 <hex>\")
  --ca FILE             PEM CA bundle to trust instead of the system one (SSL_CERT_FILE, then
                        /etc/ssl/certs/ca-certificates.crt, /etc/ssl/cert.pem)
  --sha256 HEX          expected SHA-256; on a mismatch nothing is kept and fetch exits 10
  --timeout SECS        idle limit: connecting (shared by all addresses of the host) and the longest
                        silence while sending or receiving (default 30)
  --max-time SECS       limit for the whole run, redirects included (default: none)
  --max-redirects N     default 10
  --verify-time SECS    check certificates against this Unix time instead of the clock (wrong-clock machines)
  -q, --quiet           no progress or redirect lines (errors and the hash line stay)
exit codes:
  0 ok   2 usage   3 name lookup   4 connect   5 TLS (handshake, certificate, CA bundle)
  6 certificate not valid at this time (clock?)   7 HTTP status not 2xx   8 bad or truncated response,
  too many or insecure redirects   9 writing the output   10 SHA-256 mismatch   11 timeout
A run killed by a signal can leave FILE.part (or FILE.part.N) behind.";

#[derive(Debug, PartialEq)]
pub struct Opts {
    pub url: String,
    pub out: Option<String>,
    pub ca: Option<String>,
    /// Lowercase hex.
    pub sha256: Option<String>,
    pub timeout: Duration,
    pub max_time: Option<Duration>,
    pub max_redirects: u32,
    pub verify_time: Option<UnixTime>,
    pub quiet: bool,
}

pub fn parse(mut it: impl Iterator<Item = String>) -> Result<Option<Opts>, String> {
    let mut o =
        Opts { url: String::new(), out: None, ca: None, sha256: None, timeout: Duration::from_secs(30), max_time: None, max_redirects: 10, verify_time: None, quiet: false };
    let mut url = None;
    let mut only_args = false;
    while let Some(a) = it.next() {
        if only_args || !a.starts_with('-') || a == "-" {
            if url.replace(a).is_some() {
                return Err("only one URL".into());
            }
            continue;
        }
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |what: &str| inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{what} needs a value"));
        match name.as_str() {
            "--" => only_args = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" => {
                println!("fetch {} (rustls 0.23, ring)", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-q" | "--quiet" => o.quiet = true,
            "-o" | "--output" => o.out = Some(value(&name)?),
            "--ca" => o.ca = Some(value(&name)?),
            "--sha256" => {
                let v = value(&name)?.to_ascii_lowercase();
                if v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("--sha256 wants 64 hex digits, got {v:?}"));
                }
                o.sha256 = Some(v);
            }
            "--timeout" => {
                let v = value(&name)?;
                let s: u64 = v.parse().ok().filter(|&s| s > 0).ok_or_else(|| format!("--timeout wants seconds > 0, got {v:?}"))?;
                o.timeout = Duration::from_secs(s);
            }
            "--max-time" => {
                let v = value(&name)?;
                let s: u64 = v.parse().ok().filter(|&s| s > 0).ok_or_else(|| format!("--max-time wants seconds > 0, got {v:?}"))?;
                o.max_time = Some(Duration::from_secs(s));
            }
            "--max-redirects" => {
                let v = value(&name)?;
                o.max_redirects = v.parse().map_err(|_| format!("--max-redirects wants a number, got {v:?}"))?;
            }
            "--verify-time" => {
                let v = value(&name)?;
                let s: u64 = v.parse().map_err(|_| format!("--verify-time wants Unix seconds, got {v:?}"))?;
                o.verify_time = Some(UnixTime::since_unix_epoch(Duration::from_secs(s)));
            }
            _ => return Err(format!("unknown option {a}")),
        }
    }
    if o.out.as_deref() == Some("") {
        return Err("-o needs a file name".into());
    }
    o.url = url.ok_or("no URL given")?;
    Ok(Some(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Option<Opts>, String> {
        parse(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn options() {
        let o = p(&["-q", "--ca=/c.pem", "https://h/x", "-o", "/tmp/y", "--timeout", "5", "--sha256", &"AB".repeat(32), "--verify-time", "1800000000", "--max-time", "60"])
            .unwrap()
            .unwrap();
        assert_eq!((o.url.as_str(), o.out.as_deref(), o.ca.as_deref(), o.quiet), ("https://h/x", Some("/tmp/y"), Some("/c.pem"), true));
        assert_eq!((o.timeout.as_secs(), o.sha256.as_deref()), (5, Some("ab".repeat(32).as_str())));
        assert_eq!(o.verify_time.map(|t| t.as_secs()), Some(1_800_000_000));
        assert_eq!(o.max_time, Some(Duration::from_secs(60)));
        assert_eq!(p(&["--help"]).unwrap(), None);
        assert_eq!(p(&["--", "-weird"]).unwrap().unwrap().url, "-weird");
    }

    #[test]
    fn usage_errors() {
        for a in [
            &[][..],
            &["a", "b"],
            &["--bogus", "u"],
            &["u", "-o"],
            &["u", "--sha256", "12"],
            &["u", "--timeout", "0"],
            &["u", "--max-time", "0"],
            &["u", "--max-time", "-1"],
            &["u", "-o", ""],
            &["u", "--verify-time", "x"],
        ] {
            assert!(p(a).is_err(), "{a:?}");
        }
    }
}
