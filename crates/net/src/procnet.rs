//! `/proc/net` (NW-12): the network files of the reading program's network namespace, as Linux's per-task
//! `/proc/net` shows them (linux-6.6/fs/proc/proc_net.c:267 `get_proc_task_net`: the namespace of the task that
//! looks the name up, not of whoever mounted /proc). `dev`, `if_inet6`, `route`, `tcp`, `udp` and `unix` in
//! Linux's column layout; V2's own `dhcp` only in the initial namespace (a namespace has no DHCP client: the
//! name does not exist there, ENOENT).
//!
//! The namespace is taken when a name is looked up (at open), so an open file keeps showing the namespace it
//! was opened in after its reader moved (Linux's seq files keep their `struct net`). What V2 does not keep shows
//! Linux's value for an idle socket: queues 0, no timer, socket inode 0 (V2's sockets have no inode numbers),
//! the kernel pointer as `%pK` prints it without CAP_SYSLOG (zeros).
//!
//! A read takes a short reference to each socket of the namespace and changes none of them (no state is synced
//! for its owner). A socket whose program closes it during the read is released when the read drops that
//! reference, on the reader's thread.

use crate::ifview::{self, Link};
use crate::netns::Net;
use crate::sock::{Ids, SockFile};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use vfs::Errno;
use vfs::synth::{DirSource, FileSource, SynthEntry};
use vfs::inode::Cred;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dev,
    Route,
    Tcp,
    Udp,
    Unix,
    Dhcp,
    IfInet6,
}

const FILES: &[(&str, Kind)] = &[("dev", Kind::Dev), ("dhcp", Kind::Dhcp), ("if_inet6", Kind::IfInet6), ("route", Kind::Route), ("tcp", Kind::Tcp), ("udp", Kind::Udp), ("unix", Kind::Unix)];

/// Who reads: the namespace whose files these are and the reader's ids (uids are shown as it names them).
#[derive(Clone)]
pub struct Reader {
    pub net: Net,
    pub ids: Option<Arc<dyn Ids>>,
}

impl Reader {
    /// The calling program (the namespace hook; the initial namespaces without it).
    pub fn current() -> Reader {
        let n = crate::sock::ns_now();
        Reader { net: n.net, ids: n.ids }
    }
    fn uid(&self, kuid: u32) -> u32 {
        self.ids.as_ref().map_or(kuid, |i| i.uid_from_kernel(kuid))
    }
}

/// The names of `/proc/net` for `r`: `dhcp` exists in the initial namespace only.
pub fn names(r: &Reader) -> Vec<(&'static str, Kind)> {
    FILES.iter().copied().filter(|(_, k)| *k != Kind::Dhcp || r.net.is_none()).collect()
}

/// The text of one file for `r`.
pub fn render(kind: Kind, r: &Reader) -> String {
    match kind {
        Kind::Dev => dev(&ifview::links_in(&r.net)),
        // A namespace's main table holds nothing of lo (its routes are local); before lo's first address
        // the table does not exist and the file is empty.
        Kind::Route => match &r.net {
            None => route(ifview::routes4()),
            Some(v) if v.with_dev(|d| d.table) => route(Vec::new()),
            Some(_) => String::new(),
        },
        Kind::Tcp | Kind::Udp => inet(kind == Kind::Tcp, r),
        Kind::Unix => unix(r),
        Kind::Dhcp => crate::dhcpview::text(),
        Kind::IfInet6 => if_inet6(&ifview::links_in(&r.net)),
    }
}

/// `if6_seq_show` (linux-6.6/net/ipv6/addrconf.c): address, ifindex, prefix, the IPv6 scope class
/// (`ipv6_addr_scope`: host 0x10, link 0x20, global 0) and the low byte of the address flags, the name right
/// aligned to 8. V2's only IPv6 address is ::1 on a namespace's lo while it is UP.
pub fn if_inet6(links: &[Link]) -> String {
    let mut s = String::new();
    for l in links {
        for a in &l.addrs6 {
            a.ip.iter().for_each(|b| {
                let _ = write!(s, "{b:02x}");
            });
            let scope = match a.scope {
                ifview::RT_SCOPE_HOST => 0x10,
                253 => 0x20, // RT_SCOPE_LINK
                _ => 0,
            };
            let _ = writeln!(s, " {:02x} {:02x} {:02x} {:02x} {:>8}", l.index, a.prefix, scope, a.flags as u8, l.name);
        }
    }
    s
}

/// `dev_seq_show` / `dev_seq_printf_stats` (linux-6.6/net/core/net-procfs.c:77, :105).
pub fn dev(links: &[Link]) -> String {
    let mut s = String::from(concat!(
        "Inter-|   Receive                                                |  Transmit\n",
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n"
    ));
    for l in links {
        let t = &l.stats;
        let _ = writeln!(
            s,
            "{:>6}: {:7} {:7} {:4} {:4} {:4} {:5} {:10} {:9} {:8} {:7} {:4} {:4} {:4} {:5} {:7} {:10}",
            l.name, t.rx_bytes, t.rx_frames, 0, t.rx_dropped, 0, 0, 0, 0, t.tx_bytes, t.tx_frames, 0, t.tx_dropped, 0, 0, 0, 0
        );
    }
    s
}

/// A line padded with blanks to `width` characters (`seq_setwidth` + `seq_pad`), then the newline.
fn padded(s: &mut String, line: &str, width: usize) {
    s.push_str(line);
    for _ in line.len()..width {
        s.push(' ');
    }
    s.push('\n');
}

/// An IPv4 address as Linux prints a `__be32` with %08X on a little-endian machine.
fn hex4(ip: [u8; 4]) -> u32 {
    u32::from_le_bytes(ip)
}

const RTF_UP: u32 = 0x1;
const RTF_GATEWAY: u32 = 0x2;
const RTF_HOST: u32 = 0x4;

/// `fib_route_seq_show` (linux-6.6/net/ipv4/fib_trie.c:2967): the main table only, 127 characters a line.
pub fn route(routes: Vec<ifview::Route4>) -> String {
    let mut s = String::new();
    padded(&mut s, "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT", 127);
    for r in routes {
        let mask = crate::route::mask(r.prefix).to_be_bytes();
        let flags = RTF_UP | if r.gateway != [0; 4] { RTF_GATEWAY } else { 0 } | if r.prefix == 32 { RTF_HOST } else { 0 };
        let line = format!("{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t{}\t{:08X}\t0\t0\t0", r.dev, hex4(r.dst), hex4(r.gateway), flags, r.metric, hex4(mask));
        padded(&mut s, &line, 127);
    }
    s
}

/// `tcp4_seq_show` (149 characters a line) and `udp4_seq_show` (127), linux-6.6/net/ipv4/tcp_ipv4.c:2737,
/// net/ipv4/udp.c:3057. The slot number of a TCP line counts the lines; a UDP line shows the hash bucket, here
/// the port (Linux's table size depends on the memory).
fn inet(tcp: bool, r: &Reader) -> String {
    let mut s = String::new();
    let rows = SockFile::all_in(&r.net).into_iter().filter_map(|f| f.inet().and_then(|i| i.proc_row())).filter(|x| x.tcp == tcp);
    if tcp {
        padded(&mut s, "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode", 149);
        for (n, x) in rows.enumerate() {
            let listen = x.state == crate::sock::inet::TCP_LISTEN;
            // rto, ato, quick/pingpong, cwnd, ssthresh (or the fastopen queue of a listener) of a fresh socket.
            let tail = if listen { "100 0 0 10 0" } else { "20 4 0 10 -1" };
            let line = format!(
                "{:4}: {:08X}:{:04X} {:08X}:{:04X} {:02X} {:08X}:{:08X} {:02X}:{:08X} {:08X} {:5} {:8} {} {} {:016x} {}",
                n,
                hex4(x.local.ip.0),
                x.local.port,
                hex4(x.peer.ip.0),
                x.peer.port,
                x.state,
                0,
                0,
                0,
                0,
                0,
                r.uid(x.uid),
                0,
                0,
                1,
                0,
                tail
            );
            padded(&mut s, &line, 149);
        }
    } else {
        padded(&mut s, "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops", 127);
        for x in rows {
            let line = format!(
                "{:5}: {:08X}:{:04X} {:08X}:{:04X} {:02X} {:08X}:{:08X} {:02X}:{:08X} {:08X} {:5} {:8} {} {} {:016x} {}",
                x.local.port,
                hex4(x.local.ip.0),
                x.local.port,
                hex4(x.peer.ip.0),
                x.peer.port,
                x.state,
                0,
                0,
                0,
                0,
                0,
                r.uid(x.uid),
                0,
                0,
                2,
                0,
                0
            );
            padded(&mut s, &line, 127);
        }
    }
    s
}

/// `unix_seq_show` (linux-6.6/net/unix/af_unix.c:3261): every AF_UNIX socket of the namespace; an abstract
/// name starts with '@' and its NUL bytes show as '@'.
fn unix(r: &Reader) -> String {
    let mut s = String::from("Num       RefCount Protocol Flags    Type St Inode Path\n");
    const SO_ACCEPTCON: u32 = 0x10000;
    const SS_UNCONNECTED: u32 = 1;
    const SS_CONNECTED: u32 = 3;
    for f in SockFile::all_in(&r.net) {
        let Some(u) = f.unix() else { continue };
        let (listen, connected, name) = u.proc_row();
        let _ = write!(
            s,
            "{:016x}: {:08X} {:08X} {:08X} {:04X} {:02X} {:5}",
            0,
            2,
            0,
            if listen { SO_ACCEPTCON } else { 0 },
            u.ty.linux(),
            if connected { SS_CONNECTED } else { SS_UNCONNECTED },
            0
        );
        if let Some(n) = name {
            s.push_str(&unix_path(&n));
        }
        s.push('\n');
    }
    s
}

/// " " and the path of a bound name (`sockaddr_un` bytes: family, then the path): a path name without its
/// NUL, an abstract one as '@' followed by its bytes with NULs as '@'.
pub fn unix_path(raw: &[u8]) -> String {
    let p = raw.get(2..).unwrap_or(&[]);
    if p.is_empty() {
        return String::new();
    }
    let mut out = String::from(" ");
    if p[0] == 0 {
        out.push('@');
        p[1..].iter().for_each(|b| out.push(if *b == 0 { '@' } else { *b as char }));
    } else {
        let p = &p[..p.iter().position(|b| *b == 0).unwrap_or(p.len())];
        out.push_str(&String::from_utf8_lossy(p));
    }
    out
}

/// One file of `/proc/net`, bound to the reader that looked it up.
struct NetFile {
    kind: Kind,
    reader: Reader,
}

impl FileSource for NetFile {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(render(self.kind, &self.reader).into_bytes())
    }
}

/// `/proc/net` itself: its names are made per lookup for the program that asks (`fixed` None), or for one
/// fixed reader (`dir_of`, for a /proc/<pid>/net of another task).
struct ProcNet {
    fixed: Option<Reader>,
}

fn entry(i: usize, name: &'static str, kind: Kind, r: &Reader) -> SynthEntry {
    SynthEntry::file(name, 0o444, Arc::new(NetFile { kind, reader: r.clone() })).cookie(i as u64 + 1)
}

impl ProcNet {
    fn reader(&self) -> Reader {
        self.fixed.clone().unwrap_or_else(Reader::current)
    }
}

impl DirSource for ProcNet {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let r = self.reader();
        Ok(names(&r).into_iter().enumerate().map(|(i, (n, k))| entry(i, n, k, &r)).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        let r = self.reader();
        Ok(names(&r).into_iter().enumerate().find(|(_, (n, _))| n.as_bytes() == name).map(|(i, (n, k))| entry(i, n, k, &r)))
    }
}

/// `/proc/net`: the files of the reading program's namespace.
pub fn dir() -> Arc<dyn DirSource> {
    Arc::new(ProcNet { fixed: None })
}

/// The files of one task's namespace, read with `r`'s ids (procfs's `/proc/<pid>/net`, INT-W21).
pub fn dir_of(r: Reader) -> Arc<dyn DirSource> {
    Arc::new(ProcNet { fixed: Some(r) })
}

#[cfg(target_os = "none")]
vfs::procdir!("/proc/net", dir);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let mut lo = ifview::lo_link(ifview::IFF_UP, Vec::new(), Vec::new(), 65536, 1000);
        lo.stats.rx_bytes = 1234;
        lo.stats.rx_frames = 5;
        let d = dev(&[lo]);
        assert_eq!(d.lines().nth(2), Some("    lo:    1234       5    0    0    0     0          0         0        0       0    0    0    0     0       0          0"));
        let r = route(alloc::vec![
            ifview::Route4 { dev: String::from("eth0"), dst: [0; 4], gateway: [192, 0, 2, 1], prefix: 0, metric: 100 },
            ifview::Route4 { dev: String::from("eth0"), dst: [192, 0, 2, 0], gateway: [0; 4], prefix: 24, metric: 100 },
        ]);
        let l: Vec<&str> = r.lines().collect();
        assert_eq!(l[0].trim_end(), "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT");
        assert_eq!(l[0].len(), 127);
        assert_eq!(l[1].trim_end(), "eth0\t00000000\t010200C0\t0003\t0\t0\t100\t00000000\t0\t0\t0");
        assert_eq!(l[2].trim_end(), "eth0\t000200C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0");
        assert!(l.iter().all(|x| x.len() == 127));
        assert_eq!(unix_path(&[1, 0, 0, b'a', 0, b'b']), " @a@b");
        assert_eq!(unix_path(&[1, 0, b'/', b't', 0]), " /t");
        assert_eq!(unix_path(&[1, 0]), "");
        let init = Reader { net: None, ids: None };
        assert!(names(&init).iter().any(|(n, _)| *n == "dhcp"));
        let ns = Reader { net: Some(crate::netns::NetView::new(7)), ids: None };
        assert!(!names(&ns).iter().any(|(n, _)| *n == "dhcp"));
        assert_eq!(render(Kind::Route, &ns), "", "no table before lo's first address");
        assert_eq!(render(Kind::IfInet6, &ns), "", "no ::1 before lo's first UP");
        let lo = ifview::lo_link(ifview::IFF_UP, Vec::new(), alloc::vec![ifview::lo6()], 65536, 1000);
        assert_eq!(if_inet6(&[lo]), "00000000000000000000000000000001 01 80 10 80       lo\n", "as CachyOS shows it");
    }
}
