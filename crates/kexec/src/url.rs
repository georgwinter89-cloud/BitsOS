//! Where a bundle may come from (R9: only the LAN Georg's caller names; no DNS, no TLS, no redirects in the
//! kernel) and how a bundle URL names its two files.

use alloc::format;
use alloc::string::{String, ToString};
use net::http::{HttpError, parse_url};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// Not `http://` (ftp, file, ...). `https://` gets its own reason.
    Scheme(String),
    Https,
    /// A host name: the kernel has no DNS.
    NeedsAddress,
    /// Outside the private networks (RFC 1918, loopback, link-local): Reload is LAN only (R9).
    NotLan(String),
    Bad(&'static str),
}

impl UrlError {
    pub fn describe(&self) -> String {
        match self {
            UrlError::Scheme(s) => format!("wrong URL scheme '{s}': Reload fetches only http://<LAN IPv4>[:port]/..."),
            UrlError::Https => "https:// is not supported in the kernel (no TLS); serve the bundle over http:// on the LAN".into(),
            UrlError::NeedsAddress => "a host name needs DNS, which the kernel does not have: give the IPv4 address".into(),
            UrlError::NotLan(ip) => format!("{ip} is not on a private network: Reload fetches only from the LAN (R9)"),
            UrlError::Bad(w) => format!("bad URL: {w}"),
        }
    }
}

/// Checks one URL: `http://` + a private IPv4 (+ port, path).
pub fn check(url: &str) -> Result<(), UrlError> {
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("");
    if scheme.is_empty() {
        return Err(UrlError::Bad("no scheme (want http://)"));
    }
    match parse_url(url) {
        Ok(u) if u.addr.ip.is_private() => Ok(()),
        Ok(u) => {
            let o = u.addr.ip.0;
            Err(UrlError::NotLan(format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3])))
        }
        Err(HttpError::Https) => Err(UrlError::Https),
        Err(HttpError::NeedsAddress) => Err(UrlError::NeedsAddress),
        Err(HttpError::BadUrl("only http://")) => Err(UrlError::Scheme(scheme.to_string())),
        Err(HttpError::BadUrl(w)) => Err(UrlError::Bad(w)),
        Err(_) => Err(UrlError::Bad("unparsable")),
    }
}

/// The two files of a bundle. A directory URL (ending in `/`, the stick's generation layout `SYS/<n>/`) names
/// `<url>KERNEL` and `<url>INITRD`; otherwise `url` is the kernel and `initrd` the initramfs, and without
/// `initrd` the running initramfs is passed on unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub kernel: String,
    pub initrd: Option<String>,
}

pub fn bundle(url: &str, initrd: Option<&str>) -> Result<Bundle, UrlError> {
    check(url)?;
    let b = if url.ends_with('/') && initrd.is_none() {
        Bundle { kernel: format!("{url}KERNEL"), initrd: Some(format!("{url}INITRD")) }
    } else if url.ends_with('/') {
        Bundle { kernel: format!("{url}KERNEL"), initrd: initrd.map(String::from) }
    } else {
        Bundle { kernel: url.to_string(), initrd: initrd.map(String::from) }
    };
    if let Some(i) = &b.initrd {
        check(i)?;
    }
    Ok(b)
}
