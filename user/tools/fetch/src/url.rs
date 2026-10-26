//! The part of URL syntax fetch needs: `http[s]://host[:port][/path][?query]`, IPv6 literals in brackets,
//! and Location headers (absolute, scheme-relative, absolute-path or relative) resolved against a base.

#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub https: bool,
    /// Without brackets for IPv6 literals (that is what DNS lookup and the TLS server name want).
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with '/'; the fragment is dropped (it never goes to the server).
    pub target: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, String> {
        let (https, rest) = if let Some(r) = strip_prefix_ci(s, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(s, "http://") {
            (false, r)
        } else {
            return Err(format!("{s}: only http:// and https:// URLs"));
        };
        let rest = rest.split('#').next().unwrap_or("");
        let cut = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, target) = rest.split_at(cut);
        if authority.contains('@') {
            return Err(format!("{s}: user names in URLs are not supported"));
        }
        let (host, port) = split_host_port(authority).ok_or_else(|| format!("{s}: bad host or port"))?;
        let port = port.unwrap_or(if https { 443 } else { 80 });
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("{s}: bad host"));
        }
        if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("{s}: spaces or control characters in the path (percent-encode them)"));
        }
        let target = if target.starts_with('/') { target.to_string() } else { format!("/{target}") };
        Ok(Url { https, host: host.to_string(), port, target })
    }

    /// The value of the Host header: the port only when it is not the scheme's default.
    pub fn host_header(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == if self.https { 443 } else { 80 } { h } else { format!("{h}:{}", self.port) }
    }

    /// The last path segment (for the default output name), without the query.
    pub fn file_name(&self) -> Option<&str> {
        let path = self.target.split('?').next().unwrap_or("");
        path.rsplit('/').next().filter(|n| !n.is_empty() && *n != "." && *n != "..")
    }

    /// Resolves a Location header against this URL (RFC 3986 section 5.2, the cases servers send).
    pub fn join(&self, loc: &str) -> Result<Url, String> {
        let loc = loc.trim();
        if strip_prefix_ci(loc, "http://").is_some() || strip_prefix_ci(loc, "https://").is_some() {
            return Url::parse(loc);
        }
        let scheme = if self.https { "https:" } else { "http:" };
        if loc.starts_with("//") {
            return Url::parse(&format!("{scheme}{loc}"));
        }
        if loc.contains("://") {
            return Err(format!("redirect to an unsupported scheme: {loc}"));
        }
        let base = format!("{scheme}//{}", self.host_header());
        let target = if loc.starts_with('/') {
            loc.to_string()
        } else if loc.starts_with('?') {
            format!("{}{loc}", self.target.split('?').next().unwrap_or("/"))
        } else {
            let path = self.target.split('?').next().unwrap_or("/");
            let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
            format!("{dir}{loc}")
        };
        let mut u = Url::parse(&format!("{base}{target}"))?;
        u.target = remove_dot_segments(&u.target);
        Ok(u)
    }
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    (s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p)).then(|| &s[p.len()..])
}

fn split_host_port(a: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if let Some(r) = a.strip_prefix('[') {
        let end = r.find(']')?;
        let after = &r[end + 1..];
        (&r[..end], if after.is_empty() { None } else { Some(after.strip_prefix(':')?) })
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        }
    };
    let port = match port {
        None | Some("") => None,
        Some(p) => Some(p.parse::<u16>().ok().filter(|&p| p != 0)?),
    };
    Some((host, port))
}

fn remove_dot_segments(target: &str) -> String {
    let (path, query) = match target.find('?') {
        Some(i) => target.split_at(i),
        None => (target, ""),
    };
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = path.split('/').skip(1).collect();
    for (i, s) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        match *s {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}{query}", out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_ports_and_targets() {
        let u = Url::parse("https://Example.org/a/b.tar.gz?x=1#frag").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.target.as_str()), (true, "Example.org", 443, "/a/b.tar.gz?x=1"));
        assert_eq!(u.file_name(), Some("b.tar.gz"));
        let u = Url::parse("HTTP://10.0.2.2:28541").unwrap();
        assert_eq!((u.https, u.port, u.target.as_str(), u.host_header()), (false, 28541, "/", "10.0.2.2:28541".to_string()));
        assert_eq!(u.file_name(), None);
        let u = Url::parse("https://[::1]:8443/x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.host_header()), ("::1", 8443, "[::1]:8443".to_string()));
        assert_eq!(Url::parse("https://h?q").unwrap().target, "/?q");
    }

    #[test]
    fn rejects_bad_urls() {
        for s in ["ftp://x/", "https://", "https://h:0/", "https://h:99999/", "https://u@h/", "https://h/a b", "https://[::1/", "x"] {
            assert!(Url::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn joins_locations() {
        let b = Url::parse("https://h:444/dir/sub/file?q").unwrap();
        assert_eq!(b.join("http://o/x").unwrap(), Url::parse("http://o/x").unwrap());
        assert_eq!(b.join("//o2/y").unwrap(), Url::parse("https://o2/y").unwrap());
        assert_eq!(b.join("/abs").unwrap().target, "/abs");
        assert_eq!(b.join("other").unwrap().target, "/dir/sub/other");
        assert_eq!(b.join("../up/x").unwrap().target, "/dir/up/x");
        assert_eq!(b.join("../../../../x").unwrap().target, "/x");
        assert_eq!(b.join("?z").unwrap().target, "/dir/sub/file?z");
        assert_eq!(b.join("other").unwrap().port, 444);
        assert!(b.join("ftp://x/").is_err());
    }
}
