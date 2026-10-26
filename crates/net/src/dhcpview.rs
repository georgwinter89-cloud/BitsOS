//! `/proc/net/dhcp`: every network card the kernel's stack drives, one line each:
//!
//! ```text
//! # BitsOS network cards and DHCP leases (one line per card)
//! iface=eth0 mac=52:54:00:12:34:56 link=up speed=1000 dhcp=yes addr=10.0.2.15/24 router=10.0.2.2 dns=10.0.2.3 lease_s=86391 default=yes
//! ```
//!
//! Unknown values are `-`; `lease_s` is the time left (`infinite` for a lease without end). Linux has no such
//! file (DHCP is a user program there); BitsOS's DHCP client is in the kernel, so the kernel shows what it got and
//! init writes the files programs read (/etc/resolv.conf, /etc/hosts): one source for them. Owner NW: here in `net`
//! since NW-5 (UL-2 change request 3; UL drops its copy, `crates/userland/src/dhcpview.rs`); the format stays.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One card's line (pure: host-tested).
#[allow(clippy::too_many_arguments)]
pub fn card_line(
    name: &str,
    mac: [u8; 6],
    link_up: bool,
    speed_mbps: u32,
    dhcp: bool,
    addr: Option<([u8; 4], u8)>,
    router: Option<[u8; 4]>,
    dns: &[[u8; 4]],
    lease_left_ns: Option<Option<u64>>,
    default: bool,
) -> String {
    let ip = |a: [u8; 4]| format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
    // A name with spaces or '=' would break the line format; such a name is shown with '_' instead.
    let name: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '=' { c } else { '_' }).collect();
    let addr = addr.map(|(a, l)| format!("{}/{l}", ip(a))).unwrap_or_else(|| "-".into());
    let router = router.map(ip).unwrap_or_else(|| "-".into());
    let dns = if dns.is_empty() { "-".into() } else { dns.iter().map(|d| ip(*d)).collect::<Vec<_>>().join(",") };
    let lease = match lease_left_ns {
        None => "-".into(),
        Some(None) => "infinite".into(),
        Some(Some(ns)) => format!("{}", ns / 1_000_000_000),
    };
    format!(
        "iface={name} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link={} speed={speed_mbps} dhcp={} addr={addr} router={router} dns={dns} lease_s={lease} default={}\n",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        if link_up { "up" } else { "down" },
        if dhcp { "yes" } else { "no" },
        if default { "yes" } else { "no" },
    )
}

pub const HEADER: &str = "# BitsOS network cards and DHCP leases (one line per card)\n";

#[cfg(target_os = "none")]
fn render() -> String {
    let mut s = String::from(HEADER);
    let Some(stack) = crate::stack::kernel() else { return s };
    let now = time::now_ns();
    for c in stack.interfaces() {
        let i = &c.info;
        let lease = if i.addr.is_none() {
            None
        } else if !i.dhcp {
            Some(None)
        } else {
            match i.lease_expires_ns {
                Some(e) => Some(Some(e.saturating_sub(now))),
                None => Some(None),
            }
        };
        let dns: Vec<[u8; 4]> = i.dns.iter().map(|d| d.0).collect();
        s.push_str(&card_line(&c.name, c.mac, i.link_up, i.speed_mbps, i.dhcp, i.addr.map(|(a, l)| (a.0, l)), i.router.map(|r| r.0), &dns, lease, c.default_route));
    }
    s
}

#[cfg(target_os = "none")]
vfs::procfile!("/proc/net/dhcp", render);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let l = card_line("eth0", [0x52, 0x54, 0, 0x12, 0x34, 0x56], true, 1000, true, Some(([10, 0, 2, 15], 24)), Some([10, 0, 2, 2]), &[[10, 0, 2, 3], [1, 1, 1, 1]], Some(Some(86_391_500_000_000)), true);
        assert_eq!(l, "iface=eth0 mac=52:54:00:12:34:56 link=up speed=1000 dhcp=yes addr=10.0.2.15/24 router=10.0.2.2 dns=10.0.2.3,1.1.1.1 lease_s=86391 default=yes\n");
        let l = card_line("we ird=", [0; 6], false, 0, true, None, None, &[], None, false);
        assert_eq!(l, "iface=we_ird_ mac=00:00:00:00:00:00 link=down speed=0 dhcp=yes addr=- router=- dns=- lease_s=- default=no\n");
        assert!(card_line("lo", [0; 6], true, 0, false, Some(([127, 0, 0, 1], 8)), None, &[], Some(None), false).contains("lease_s=infinite"));
    }
}
