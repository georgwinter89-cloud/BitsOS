//! The pure parts of BitsOS init (host-tested): arguments, the kernel's DHCP view, the `/etc` files rendered
//! from it, the restart back-off and the log de-duplication. `main.rs` does the system calls.
#![forbid(unsafe_code)]

use std::collections::HashMap;

/// Where the kernel shows its network cards and DHCP leases (one line per card, `key=value` fields; written by
/// the kernel's `userland` crate today, owner NW, see plan/notes/ul-UL-2.md).
pub const DEFAULT_NET_FILE: &str = "/proc/net/dhcp";

/// What init was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opts {
    /// Prefix for every path init creates or writes ("" on a real boot; a scratch directory in tests).
    pub root: String,
    pub net_file: String,
    /// One pass (set-up, /etc, one reap round), then exit 0. Only for tests: a real init never exits.
    pub once: bool,
    pub interval_ms: u64,
    /// Shell candidates in order (each a path below `root`).
    pub shells: Vec<String>,
    /// Arguments init did not understand (logged, otherwise ignored: the kernel may pass anything).
    pub unknown: Vec<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            root: String::new(),
            net_file: DEFAULT_NET_FILE.to_string(),
            once: false,
            interval_ms: 1000,
            shells: vec!["/bin/sh".to_string(), "/bin/busybox".to_string()],
            unknown: Vec::new(),
        }
    }
}

impl Opts {
    /// Parses argv after argv[0]. Never fails: bad values are kept in `unknown`.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Opts {
        let mut o = Opts::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--once" => o.once = true,
                "--root" | "--net-file" | "--interval-ms" | "--shell" => {
                    let Some(v) = it.next() else {
                        o.unknown.push(a);
                        continue;
                    };
                    match a.as_str() {
                        "--root" => o.root = v.trim_end_matches('/').to_string(),
                        "--net-file" => o.net_file = v,
                        "--shell" => o.shells = vec![v],
                        _ => match v.parse::<u64>() {
                            Ok(ms) if (10..=3_600_000).contains(&ms) => o.interval_ms = ms,
                            _ => o.unknown.push(format!("{a} {v}")),
                        },
                    }
                }
                _ => o.unknown.push(a),
            }
        }
        o
    }

    /// `path` below the root.
    pub fn at(&self, path: &str) -> String {
        format!("{}{}", self.root, path)
    }
}

/// UL-6: the console's language (decision 21.10.: German, as Georg's CachyOS): LANG for the console shell and the
/// programs started from it (bitscomp, its terminals, Cobalt's UI language via Starboard's SbSystemGetLocaleId).
/// Only LANG is set; LC_MESSAGES etc. follow it unless a program or the user sets them.
pub const LANGS: &[(&str, &str)] = &[("de_DE.UTF-8", "/usr/lib/locale/de_DE.utf8"), ("C.UTF-8", "/usr/lib/locale/C.utf8")];

/// The first of `LANGS` whose glibc locale directory is usable (`have`; init checks its LC_CTYPE file): an image built without the German locale
/// (V2_PKG_OPTIONAL=1) gets C.UTF-8, not a LANG glibc silently drops to ASCII; None without any.
pub fn pick_lang(have: impl Fn(&str) -> bool) -> Option<&'static str> {
    LANGS.iter().find(|(_, dir)| have(dir)).map(|(l, _)| *l)
}

/// One network card as the kernel shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Card {
    pub name: String,
    pub link_up: bool,
    /// Address and prefix length.
    pub addr: Option<([u8; 4], u8)>,
    pub router: Option<[u8; 4]>,
    pub dns: Vec<[u8; 4]>,
    /// The card that carries the default route.
    pub default: bool,
    /// DHCP option 15 (domain name), when the kernel shows it (`domain=`; NW change request).
    pub domain: Option<String>,
}

pub fn ip_text(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// A dotted IPv4 address (exactly four decimal octets 0..=255, no leading '+').
pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for o in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *o = p.parse::<u16>().ok().filter(|v| *v <= 255)? as u8;
    }
    if parts.next().is_some() { None } else { Some(out) }
}

/// Parses the kernel's DHCP view. Lines: `iface=<name> link=up|down addr=<ip>/<len>|- router=<ip>|-
/// dns=<ip>,<ip>|- default=yes|no ...` (unknown keys are ignored, `#` lines are comments). A line without a
/// valid `iface=` is skipped; a bad value leaves that field empty. Never panics.
pub fn parse_net(text: &str) -> Vec<Card> {
    let mut cards = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut c = Card::default();
        for field in line.split_ascii_whitespace() {
            let Some((k, v)) = field.split_once('=') else { continue };
            match k {
                "iface" if !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_graphic()) => c.name = v.to_string(),
                "link" => c.link_up = v == "up",
                "addr" => {
                    c.addr = v.split_once('/').and_then(|(a, l)| Some((parse_ip(a)?, l.parse::<u8>().ok().filter(|l| *l <= 32)?)))
                }
                "router" => c.router = parse_ip(v),
                "dns" => c.dns = v.split(',').filter_map(parse_ip).collect(),
                "default" => c.default = v == "yes",
                "domain" if v != "-" && valid_name(v) => c.domain = Some(v.to_string()),
                _ => {}
            }
        }
        if !c.name.is_empty() {
            cards.push(c);
        }
    }
    cards
}

/// musl's resolver reads at most three `nameserver` lines (MAXNS 3, a resolver limit, not ours); more would be
/// ignored, so init writes the first three.
const RESOLVER_MAXNS: usize = 3;

/// DNS servers in use: the default card's first, then the other cards' in their order, without repeats.
pub fn dns_servers(cards: &[Card]) -> Vec<[u8; 4]> {
    let mut out: Vec<[u8; 4]> = Vec::new();
    let ordered = cards.iter().filter(|c| c.default).chain(cards.iter().filter(|c| !c.default));
    for c in ordered.filter(|c| c.addr.is_some()) {
        for d in &c.dns {
            if !out.contains(d) {
                out.push(*d);
            }
        }
    }
    out
}

/// The card whose address names this machine: the default card, else the first with an address.
pub fn primary(cards: &[Card]) -> Option<&Card> {
    cards.iter().find(|c| c.default && c.addr.is_some()).or_else(|| cards.iter().find(|c| c.addr.is_some()))
}

pub fn render_resolv(cards: &[Card]) -> String {
    let dns = dns_servers(cards);
    let mut s = String::new();
    match primary(cards) {
        Some(p) if !dns.is_empty() => s.push_str(&format!("# written by BitsOS init from the DHCP lease of {}\n", p.name)),
        _ => s.push_str("# written by BitsOS init: no DNS server known yet (no DHCP lease)\n"),
    }
    if !dns.is_empty()
        && let Some(d) = primary(cards).and_then(|p| p.domain.as_deref())
    {
        s.push_str(&format!("search {d}\n"));
    }
    for d in dns.iter().take(RESOLVER_MAXNS) {
        s.push_str(&format!("nameserver {}\n", ip_text(*d)));
    }
    s
}

pub fn render_hosts(cards: &[Card], hostname: &str) -> String {
    let mut s = String::from("# written by BitsOS init\n127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    match primary(cards).and_then(|c| c.addr) {
        Some((a, _)) => s.push_str(&format!("{}\t{}\n", ip_text(a), hostname)),
        None => s.push_str(&format!("127.0.1.1\t{hostname}\n")),
    }
    s
}

/// A host name as `/etc/hostname` may hold it: the kernel's node name when it is a valid label sequence
/// (letters, digits, '-', '.'; 1..=64 bytes), else "bitsos".
pub fn clean_hostname(node: &str) -> String {
    let n = node.trim();
    if valid_name(n) { n.to_string() } else { "bitsos".to_string() }
}

/// Letters, digits, '-' and '.', 1..=64 bytes (what a host or domain name in /etc may hold here).
fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// The accounts init writes once when /etc has none (Linux layout, passwd(5), group(5)): root, uid 1000 (the
/// uid V2 gives programs it starts, `proc::SpawnOptions`) and nobody, so ps, ls -l and id show names.
pub const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nuser:x:1000:1000:BitsOS programs:/:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n";
pub const GROUP: &str = "root:x:0:\nuser:x:1000:\nnobody:x:65534:\n";

pub fn render_hostname(hostname: &str) -> String {
    format!("{hostname}\n")
}

/// One line per card for the log: `eth0 10.0.2.15/24 via 10.0.2.2 dns 10.0.2.3` (what init saw).
pub fn describe(cards: &[Card]) -> String {
    if cards.is_empty() {
        return "no network card".to_string();
    }
    let parts: Vec<String> = cards
        .iter()
        .map(|c| {
            let mut s = c.name.clone();
            s.push_str(if c.link_up { " up" } else { " down" });
            match c.addr {
                Some((a, l)) => s.push_str(&format!(" {}/{}", ip_text(a), l)),
                None => s.push_str(" no-lease"),
            }
            if let Some(r) = c.router {
                s.push_str(&format!(" via {}", ip_text(r)));
            }
            if !c.dns.is_empty() {
                let d: Vec<String> = c.dns.iter().map(|d| ip_text(*d)).collect();
                s.push_str(&format!(" dns {}", d.join(",")));
            }
            if c.default {
                s.push_str(" default");
            }
            s
        })
        .collect();
    parts.join("; ")
}

/// Restart back-off for the shell: 1 s doubling to 60 s; a run that lasted 30 s or more resets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub delay_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { delay_ms: 1000 }
    }
}

impl Backoff {
    pub const MAX_MS: u64 = 60_000;
    /// The process (or attempt) ended after `ran_ms`; returns how long to wait before the next start.
    pub fn next(&mut self, ran_ms: u64) -> u64 {
        if ran_ms >= 30_000 {
            self.delay_ms = 1000;
        }
        let d = self.delay_ms;
        self.delay_ms = (self.delay_ms * 2).min(Self::MAX_MS);
        d
    }
}

/// Repeated messages: the first occurrence and then the 10th, 100th, ... are shown (with the count), so a
/// condition that lasts does not flood the console.
#[derive(Default)]
pub struct Dedup {
    seen: HashMap<String, u64>,
}

impl Dedup {
    /// Some(count) when this occurrence of `key` should be shown.
    pub fn hit(&mut self, key: &str) -> Option<u64> {
        let n = self.seen.entry(key.to_string()).or_insert(0);
        *n += 1;
        let c = *n;
        let mut p = 1u64;
        while p < c {
            p = p.saturating_mul(10);
        }
        (p == c).then_some(c)
    }
    /// Forget a key (the condition ended), so its next occurrence shows again.
    pub fn clear(&mut self, key: &str) {
        self.seen.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn lang_follows_the_locales_present() {
        assert_eq!(pick_lang(|_| true), Some("de_DE.UTF-8"));
        assert_eq!(pick_lang(|d| d.ends_with("C.utf8")), Some("C.UTF-8"));
        assert_eq!(pick_lang(|_| false), None);
    }

    #[test]
    fn options() {
        assert_eq!(Opts::parse(Vec::new()), Opts::default());
        let o = Opts::parse(s(&["--once", "--root", "/tmp/x/", "--net-file", "/n", "--interval-ms", "50", "--shell", "/bin/ash"]));
        assert!(o.once);
        assert_eq!(o.root, "/tmp/x");
        assert_eq!(o.at("/etc/hosts"), "/tmp/x/etc/hosts");
        assert_eq!(o.net_file, "/n");
        assert_eq!(o.interval_ms, 50);
        assert_eq!(o.shells, s(&["/bin/ash"]));
        let o = Opts::parse(s(&["single", "--interval-ms", "0", "--root"]));
        assert_eq!(o.unknown, s(&["single", "--interval-ms 0", "--root"]));
        assert_eq!(o.interval_ms, 1000);
    }

    #[test]
    fn ips() {
        assert_eq!(parse_ip("10.0.2.15"), Some([10, 0, 2, 15]));
        for bad in ["", "1.2.3", "1.2.3.4.5", "256.1.1.1", "+1.2.3.4", "1..2.3", "a.b.c.d", "1.2.3.4 ", "0001.2.3.4"] {
            assert_eq!(parse_ip(bad), None, "{bad}");
        }
    }

    const VIEW: &str = "# BitsOS DHCP view\n\
        iface=eth1 mac=00:00:00:00:00:02 link=down dhcp=yes addr=- router=- dns=- default=no\n\
        iface=eth0 mac=52:54:00:12:34:56 link=up dhcp=yes addr=10.0.2.15/24 router=10.0.2.2 dns=10.0.2.3,1.1.1.1 lease_s=86400 default=yes\n";

    #[test]
    fn net_view_and_files() {
        let c = parse_net(VIEW);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].addr, Some(([10, 0, 2, 15], 24)));
        assert!(c[1].default && c[1].link_up && !c[0].link_up);
        assert_eq!(render_resolv(&c), "# written by BitsOS init from the DHCP lease of eth0\nnameserver 10.0.2.3\nnameserver 1.1.1.1\n");
        assert_eq!(render_hosts(&c, "bitsos"), "# written by BitsOS init\n127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n10.0.2.15\tbitsos\n");
        assert_eq!(describe(&c), "eth1 down no-lease; eth0 up 10.0.2.15/24 via 10.0.2.2 dns 10.0.2.3,1.1.1.1 default");
        // No lease anywhere.
        let none = parse_net("iface=eth0 link=up addr=- dns=-\n");
        assert_eq!(render_resolv(&none), "# written by BitsOS init: no DNS server known yet (no DHCP lease)\n");
        assert!(render_hosts(&none, "h").ends_with("127.0.1.1\th\n"));
        assert_eq!(describe(&[]), "no network card");
        // A domain from DHCP becomes the search line; a bad one is ignored.
        let d = parse_net("iface=eth0 addr=192.168.100.113/24 dns=192.168.100.1 default=yes domain=fritz.box\n");
        assert_eq!(render_resolv(&d), "# written by BitsOS init from the DHCP lease of eth0\nsearch fritz.box\nnameserver 192.168.100.1\n");
        assert_eq!(parse_net("iface=eth0 domain=a;b\n")[0].domain, None);
    }

    #[test]
    fn dns_order_dedup_and_limit() {
        let t = "iface=a addr=1.1.1.2/8 dns=9.9.9.9,8.8.8.8\niface=b addr=2.2.2.2/8 dns=8.8.8.8,4.4.4.4,5.5.5.5 default=yes\niface=c dns=7.7.7.7\n";
        let c = parse_net(t);
        // b (default) first; c has no address, so its server is not used.
        assert_eq!(dns_servers(&c), vec![[8, 8, 8, 8], [4, 4, 4, 4], [5, 5, 5, 5], [9, 9, 9, 9]]);
        assert_eq!(render_resolv(&c).matches("nameserver").count(), 3);
        assert_eq!(primary(&c).map(|c| c.name.as_str()), Some("b"));
    }

    #[test]
    fn hostile_views_never_panic() {
        let mut seed = 0x1234_5678u64;
        let alphabet = b"iface=addr/router dns,.0123456789 \n#=-";
        for _ in 0..5000 {
            let mut t = Vec::new();
            for _ in 0..(seed % 200) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                t.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            let text = String::from_utf8_lossy(&t);
            let c = parse_net(&text);
            let _ = (render_resolv(&c), render_hosts(&c, "x"), describe(&c));
        }
        let c = parse_net("iface=eth0 addr=10.0.0.1/99 router=300.1.1.1 dns=,,,\niface= addr=1.2.3.4/8\n");
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].addr, c[0].router, c[0].dns.len()), (None, None, 0));
    }

    #[test]
    fn hostnames() {
        assert_eq!(clean_hostname("bitsos"), "bitsos");
        assert_eq!(clean_hostname(" mini.fritz.box "), "mini.fritz.box");
        assert_eq!(clean_hostname(""), "bitsos");
        assert_eq!(clean_hostname("bad name"), "bitsos");
        assert_eq!(render_hostname("bitsos"), "bitsos\n");
    }

    #[test]
    fn backoff_and_dedup() {
        let mut b = Backoff::default();
        let v: Vec<u64> = (0..8).map(|_| b.next(10)).collect();
        assert_eq!(v, vec![1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000]);
        assert_eq!(b.next(40_000), 1000);
        let mut d = Dedup::default();
        let shown: Vec<u64> = (0..150).filter_map(|_| d.hit("k")).collect();
        assert_eq!(shown, vec![1, 10, 100]);
        d.clear("k");
        assert_eq!(d.hit("k"), Some(1));
    }
}
