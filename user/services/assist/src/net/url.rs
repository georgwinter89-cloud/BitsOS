//! The address of a provider: `http(s)://host[:port][/base]`. No user part, query or fragment: the key travels
//! in a header, and an address with a query would be glued to the adapter's own path.

use std::net::IpAddr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Without a trailing slash; empty for the root.
    pub base: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, String> {
        let s = s.trim();
        let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            return Err("die Adresse muss mit http:// oder https:// beginnen".into());
        };
        if rest.contains(['?', '#', '@']) || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("die Adresse darf kein ?, #, @ und keine Leerzeichen enthalten".into());
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (host, port) = if let Some(r) = authority.strip_prefix('[') {
            let (h, after) = r.split_once(']').ok_or("eckige Klammer der IPv6-Adresse nicht geschlossen")?;
            (h, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err("in der Adresse fehlt der Rechnername".into());
        }
        let port = match port {
            None => {
                if https {
                    443
                } else {
                    80
                }
            }
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or("der Port in der Adresse ist keine Zahl von 1 bis 65535")?,
        };
        Ok(Url { https, host: host.to_string(), port, base: path.trim_end_matches('/').to_string() })
    }

    pub fn host_header(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == if self.https { 443 } else { 80 } { h } else { format!("{h}:{}", self.port) }
    }

    /// For messages: the address without its path.
    pub fn origin(&self) -> String {
        format!("{}://{}", if self.https { "https" } else { "http" }, self.host_header())
    }
}

/// An address of the home network or this machine: the only ones plain http may go to.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_private() || a.is_loopback() || a.is_link_local(),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => is_private(IpAddr::V4(v4)),
            // fc00::/7 unique local, fe80::/10 link-local.
            None => a.is_loopback() || (a.segments()[0] & 0xfe00) == 0xfc00 || (a.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

/// Percent-encodes everything outside the unreserved set, for one path segment a setting provides.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_providers_use() {
        let u = Url::parse("https://api.example.com/v1/").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.base.as_str()), (true, "api.example.com", 443, "/v1"));
        let u = Url::parse("http://192.168.100.20:8080").unwrap();
        assert_eq!((u.https, u.port, u.base.as_str(), u.host_header().as_str()), (false, 8080, "", "192.168.100.20:8080"));
        let u = Url::parse("http://[fd00::1]:11434/v1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.host_header().as_str()), ("fd00::1", 11434, "[fd00::1]:11434"));
        for bad in ["api.example.com", "ftp://x", "https://", "https://u:p@x", "https://x/v1?key=1", "http://x:0", "http://x:99999", "https://x y"] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn private_means_home_network_or_this_machine() {
        for a in ["10.0.2.2", "192.168.100.20", "172.16.0.1", "127.0.0.1", "169.254.1.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(is_private(a.parse().unwrap()), "{a}");
        }
        for a in ["8.8.8.8", "172.32.0.1", "2001:db8::1", "::ffff:8.8.8.8", "100.64.0.1"] {
            assert!(!is_private(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn a_model_name_stays_one_path_segment() {
        assert_eq!(encode_segment("gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(encode_segment("a/b?c d"), "a%2Fb%3Fc%20d");
    }
}
