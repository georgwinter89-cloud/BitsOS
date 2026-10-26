//! `net_info`: the network as the kernel's stack sees it (V1's `HwInfo "network"`, `spec/net.md` §5.2):
//! every card with link, address, router, DNS, its DHCP lease and whether it holds the default route; the
//! network counters (`net.*` metrics of the stack and the drivers); the last connections the stack and MCP
//! recorded (B3's connection view, newest first).

use super::{bad, number, text};
use crate::{Json, ToolContext, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use net::Ipv4;

/// Connections listed when `connections` is not given.
const DEFAULT_CONNS: u64 = 20;
/// At most this many connection records per answer (the record list itself is policy's).
const MAX_CONNS: u64 = 1000;

fn ip(a: Ipv4) -> String {
    let [a, b, c, d] = a.0;
    format!("{a}.{b}.{c}.{d}")
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
fn mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// One line per card, from the stack's own view.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub(crate) fn iface_lines(list: &[net::stack::IfSummary], now_ns: u64) -> Vec<String> {
    if list.is_empty() {
        return alloc::vec![String::from("interfaces: none (no network card registered)")];
    }
    list.iter()
        .map(|i| {
            let inf = &i.info;
            let addr = inf.addr.map(|(a, p)| format!("{}/{p}", ip(a))).unwrap_or_else(|| String::from("-"));
            let lease = match (inf.dhcp, inf.lease_expires_ns) {
                (false, _) => String::from("static"),
                (true, Some(t)) => format!("{} s left", t.saturating_sub(now_ns) / 1_000_000_000),
                (true, None) if inf.addr.is_some() => String::from("infinite"),
                (true, None) => String::from("none yet"),
            };
            format!(
                "{} mac={} link={}{} addr={} router={} dns={} lease={}{}",
                i.name,
                mac(&i.mac),
                if inf.link_up { "up" } else { "down" },
                if inf.link_up && inf.speed_mbps > 0 { format!(" {} Mbit/s", inf.speed_mbps) } else { String::new() },
                addr,
                inf.router.map(ip).unwrap_or_else(|| String::from("-")),
                if inf.dns.is_empty() { String::from("-") } else { inf.dns.iter().map(|d| ip(*d)).collect::<Vec<_>>().join(",") },
                lease,
                if i.default_route { " default-route" } else { "" }
            )
        })
        .collect()
}

#[cfg(target_os = "none")]
fn interfaces(now_ns: u64) -> Vec<String> {
    match net::stack::kernel() {
        Some(s) => {
            // NW-4 change request 2 (INT-A1): per card whether it runs on MSI(-X) (`irq=on`) or is polled, and the
            // lifeline's kept listener pool.
            let list = s.interfaces();
            let mut out = iface_lines(&list, now_ns);
            if !list.is_empty() {
                for (l, i) in out.iter_mut().zip(list.iter()) {
                    l.push_str(if net::netdev::is_irq_driven(&i.name) { " irq=on" } else { " irq=off" });
                }
            }
            let k = s.kept_info();
            out.push(format!("kept made={} idle={} spare={} cap={} reused={} returned={}", k.made, k.idle, k.spare, k.cap, k.reused, k.returned));
            // NW-5 CR9 (INT-A2a): the program sockets (NW-6 added netlink_bound=).
            out.push(net::sock::summary());
            out
        }
        None => alloc::vec![String::from("interfaces: the network stack is not running (loopback only)")],
    }
}

#[cfg(not(target_os = "none"))]
fn interfaces(_now_ns: u64) -> Vec<String> {
    alloc::vec![String::from("interfaces: no kernel network stack in a host build")]
}

fn conn_line(r: &policy::ConnRecord) -> String {
    let remote = match r.remote {
        policy::IpAddr::V4(a) => ip(Ipv4(a)),
        policy::IpAddr::V6(_) => String::from("v6"),
    };
    format!(
        "conn {:?} {:?} local_port={} remote={}:{} in={} out={} program={} t_s={}",
        r.kind,
        r.proto,
        r.local_port,
        remote,
        r.remote_port,
        r.bytes_in,
        r.bytes_out,
        r.subject.program.0,
        r.time_ns / 1_000_000_000
    )
}

fn net_info(ctx: &ToolContext, args: &Json) -> ToolResult {
    let n = number(args, "connections")?.unwrap_or(DEFAULT_CONNS);
    if n > MAX_CONNS {
        return Err(bad("connections"));
    }
    let mut out = alloc::vec![String::from("== interfaces ==")];
    out.extend(interfaces(ctx.now_ns()));
    out.push(String::from("== counters =="));
    let mut c: Vec<(&str, u64)> = klog::metrics::snapshot().filter(|(name, _)| name.starts_with("net.")).collect();
    c.sort();
    out.extend(c.iter().map(|(k, v)| format!("{k}={v}")));
    out.push(format!("== connections (newest {n}) =="));
    out.extend(policy::recent_connections(n as usize).iter().map(conn_line));
    text(out)
}

mcp_tool!(
    name: "net_info",
    owner: "MC",
    safety: Read,
    description: "The network as the kernel sees it: each card with link, address, router, DNS, DHCP lease time left and the default route; the net.* counters of the stack and the drivers; the newest connection records (B3 connection view).",
    schema: r#"{"type":"object","properties":{"connections":{"type":"integer","minimum":0,"maximum":1000,"description":"connection records to list, newest first (default 20)"}}}"#,
    handler: net_info,
);

#[cfg(test)]
mod tests {
    use super::*;
    use net::stack::IfSummary;
    use net::stack::iface::Info;

    #[test]
    fn interface_lines() {
        let card = |name: &str, info: Info, def: bool| IfSummary { name: name.into(), mac: [2, 0, 0, 0, 0, 1], info, default_route: def };
        let leased = Info {
            link_up: true,
            speed_mbps: 1000,
            addr: Some((Ipv4([192, 168, 100, 113]), 24)),
            router: Some(Ipv4([192, 168, 100, 1])),
            dns: alloc::vec![Ipv4([192, 168, 100, 1])],
            lease_seq: Some(0),
            lease_expires_ns: Some(864_000_000_000_000),
            dhcp: true,
            metric: 100,
        };
        let idle = Info { dhcp: true, ..Default::default() };
        let l = iface_lines(&[card("igc0", leased, true), card("igc1", idle, false)], 1_000_000_000);
        assert_eq!(l[0], "igc0 mac=02:00:00:00:00:01 link=up 1000 Mbit/s addr=192.168.100.113/24 router=192.168.100.1 dns=192.168.100.1 lease=863999 s left default-route");
        assert_eq!(l[1], "igc1 mac=02:00:00:00:00:01 link=down addr=- router=- dns=- lease=none yet");
        assert!(iface_lines(&[], 0)[0].contains("none"));
    }
}
