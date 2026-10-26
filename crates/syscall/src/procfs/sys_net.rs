//! AB-7b: the network interfaces under `/sys` (linux-6.6 net/core/net-sysfs.c `net_class_attrs` and the
//! `statistics` group): `/sys/class/net/<if>` links to `/sys/devices/virtual/net/<if>` (see the notes: V2's
//! stack does not say which PCI function a card is, so cards sit beside `lo`). The interfaces come from
//! `net::ifview::links()` at every question (a card can appear after boot); each attribute is read at open.
//! Root-writable attributes keep Linux's 0644 and parse as `netdev_store` does (kstrtoul base 0, EINVAL);
//! writing the value an interface already has succeeds (Linux returns early there too), a valid other value
//! answers EOPNOTSUPP because V2's stack cannot change it, a value Linux refuses gets Linux's errno.

use super::attr::{Live, PAGE};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use net::ifview::{IF_OPER_DOWN, IF_OPER_UNKNOWN, IF_OPER_UP, IFF_LOWER_UP, IFF_RUNNING, Link};
use uapi::errno::*;
use vfs::synth::{DirSource, SynthEntry};
use vfs::{Cred, Errno};

/// `operstates[]` by RFC 2863 code.
pub(super) fn operstate_name(code: u8) -> &'static str {
    match code {
        IF_OPER_UNKNOWN => "unknown",
        1 => "notpresent",
        IF_OPER_DOWN => "down",
        3 => "lowerlayerdown",
        4 => "testing",
        5 => "dormant",
        IF_OPER_UP => "up",
        _ => "unknown",
    }
}

/// `sysfs_format_mac`: "%*phC" and a newline.
pub(super) fn mac_text(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// `dev->flags` (what `ip link` set), without the states `dev_get_flags` adds (RUNNING, LOWER_UP).
pub(super) fn sysfs_flags(l: &Link) -> u32 {
    l.flags() & !(IFF_RUNNING | IFF_LOWER_UP)
}

/// One attribute: its text from the interface named `name` at open (ENODEV once it is gone).
fn attr(name: &str, f: fn(&Link) -> String) -> Live {
    let n = String::from(name);
    Live::new(PAGE, move || net::ifview::by_name(n.as_bytes()).map(|l| f(&l)).ok_or(ENODEV))
}

/// `kstrtoul(buf, 0, ..)` as `netdev_store` reads a value: one trailing newline allowed, EINVAL otherwise
/// (ERANGE on overflow).
pub(super) fn kstrtoul(data: &[u8]) -> Result<u64, Errno> {
    let d = data.strip_suffix(b"\n").unwrap_or(data);
    let d = d.strip_prefix(b"+").unwrap_or(d);
    let (radix, digits) = match d {
        [b'0', x, rest @ ..] if (*x | 0x20) == b'x' && !rest.is_empty() => (16, rest),
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, d),
    };
    if digits.is_empty() {
        return Err(EINVAL);
    }
    let mut v: u64 = 0;
    for &c in digits {
        let x = (c as char).to_digit(radix).ok_or(EINVAL)?;
        v = v.checked_mul(radix as u64).and_then(|v| v.checked_add(x as u64)).ok_or(ERANGE)?;
    }
    Ok(v)
}

/// What a write of `v` to attribute `name` of `l` gives (after the parse): `dev_set_mtu`'s range check
/// (`dev_validate_mtu`: min_mtu, max_mtu), `dev_change_tx_queue_len`'s ERANGE, `change_carrier`'s and
/// `change_proto_down`'s EOPNOTSUPP (neither lo nor igc has the hooks), the same value accepted.
pub(super) fn store_result(name: &str, l: &Link, v: u64) -> Result<(), Errno> {
    let same = match name {
        "mtu" => {
            // lo: min_mtu 0, no max (gen_lo_setup); igc: ETH_MIN_MTU 68 ..= MAX_STD_JUMBO_FRAME_SIZE 9216.
            let (min, max) = if l.loopback { (0, i32::MAX as u64) } else { (68, 9216) };
            let v32 = v as u32 as i32;
            if v32 < 0 || (v32 as u64) < min || v32 as u64 > max {
                return Err(EINVAL);
            }
            v32 as u64 == l.mtu as u64
        }
        "tx_queue_len" => {
            if v > u32::MAX as u64 {
                return Err(ERANGE);
            }
            v == TX_QUEUE_LEN
        }
        "flags" => v as u32 == sysfs_flags(l),
        "netdev_group" => v as u32 as i32 == 0,
        "carrier" | "proto_down" => return Err(EOPNOTSUPP),
        _ => return Err(EIO),
    };
    if same { Ok(()) } else { Err(EOPNOTSUPP) }
}

/// `netdev_store`: EPERM without CAP_NET_ADMIN (V2: root), the parse, then the change.
fn store(name: &'static str, ifname: &str) -> impl Fn(Cred, &[u8]) -> Result<usize, Errno> + Send + Sync + 'static {
    let ifname = String::from(ifname);
    move |who: Cred, data: &[u8]| {
        if who.uid != 0 {
            return Err(EPERM);
        }
        let v = kstrtoul(data)?;
        let l = net::ifview::by_name(ifname.as_bytes()).ok_or(ENODEV)?;
        let r = store_result(name, &l, v);
        if let Err(e) = r {
            klog::event!(Info, "syscall", "sysfs.net_write_refused", iface = ifname.as_str(), attr = name, value = v, errno = e);
        }
        r.map(|()| data.len())
    }
}

/// `kobject_synth_uevent`: a known action word is accepted (V2 has no udev to tell), anything else EINVAL.
pub(super) fn uevent_store(_who: Cred, data: &[u8]) -> Result<usize, Errno> {
    let word = data.split(|&b| b == b' ' || b == b'\n').next().unwrap_or(&[]);
    match word {
        b"add" | b"remove" | b"change" | b"move" | b"online" | b"offline" | b"bind" | b"unbind" => Ok(data.len()),
        _ => Err(EINVAL),
    }
}

/// `DEFAULT_TX_QUEUE_LEN`: lo's too (`alloc_netdev_mqs` gives it to IFF_NO_QUEUE devices).
const TX_QUEUE_LEN: u64 = 1000;

/// `speed_show`/`duplex_show`: lo has no `get_link_ksettings` (EINVAL); a card without carrier answers
/// SPEED_UNKNOWN (-1) and "unknown"; with carrier the speed the driver reported. V2 keeps no duplex bit, so
/// only speeds without a half-duplex mode (1000 Mb/s and up on igc) say "full".
fn link_speed(l: &Link) -> Result<Option<u32>, Errno> {
    if l.loopback {
        return Err(EINVAL);
    }
    if !l.carrier {
        return Ok(None);
    }
    let s = net::stack::kernel().and_then(|st| st.interfaces().into_iter().find(|c| c.name == l.name)).map(|c| c.info.speed_mbps);
    Ok(s.filter(|&s| s != 0))
}

pub(super) fn speed_text(s: Option<u32>) -> String {
    match s {
        Some(s) => format!("{s}\n"),
        None => String::from("-1\n"),
    }
}

pub(super) fn duplex_text(s: Option<u32>) -> String {
    String::from(if s.is_some_and(|s| s >= 1000) { "full\n" } else { "unknown\n" })
}

fn link_attr(name: &str, f: fn(Option<u32>) -> String) -> Live {
    let n = String::from(name);
    Live::new(PAGE, move || {
        let l = net::ifview::by_name(n.as_bytes()).ok_or(ENODEV)?;
        link_speed(&l).map(f)
    })
}

type Attr = (&'static str, fn(&Link) -> String);

/// `net_class_attrs` that V2 can answer, in Linux's order.
const ATTRS: &[Attr] = &[
    ("netdev_group", |_| String::from("0\n")),
    ("type", |l| format!("{}\n", l.arphrd())),
    ("dev_id", |_| String::from("0x0\n")),
    ("dev_port", |_| String::from("0\n")),
    ("iflink", |l| format!("{}\n", l.index)),
    ("ifindex", |l| format!("{}\n", l.index)),
    // NET_NAME_PREDICTABLE for lo (as Linux), NET_NAME_ENUM for a card the driver named.
    ("name_assign_type", |l| String::from(if l.loopback { "2\n" } else { "1\n" })),
    ("addr_assign_type", |_| String::from("0\n")),
    ("addr_len", |_| String::from("6\n")),
    ("link_mode", |_| String::from("0\n")),
    ("address", |l| mac_text(&l.mac)),
    ("broadcast", |l| mac_text(&l.broadcast_hw())),
    ("dormant", |_| String::from("0\n")),
    ("testing", |_| String::from("0\n")),
    ("operstate", |l| format!("{}\n", operstate_name(l.operstate()))),
    ("carrier_changes", |l| format!("{}\n", l.carrier_up.saturating_add(l.carrier_down))),
    ("carrier", |l| String::from(if l.carrier { "1\n" } else { "0\n" })),
    ("mtu", |l| format!("{}\n", l.mtu)),
    ("flags", |l| format!("{:#x}\n", sysfs_flags(l))),
    ("tx_queue_len", |_| format!("{TX_QUEUE_LEN}\n")),
    ("proto_down", |_| String::from("0\n")),
    ("carrier_up_count", |l| format!("{}\n", l.carrier_up)),
    ("carrier_down_count", |l| format!("{}\n", l.carrier_down)),
];

/// Attributes root may write on Linux (mode 0644).
const WRITABLE: &[&'static str] = &["netdev_group", "carrier", "mtu", "flags", "tx_queue_len", "proto_down"];

/// `netstat_attrs` in Linux's order: V2's cards count frames, bytes and drops; the rest are 0.
const STATS: &[Attr] = &[
    ("rx_packets", |l| format!("{}\n", l.stats.rx_frames)),
    ("tx_packets", |l| format!("{}\n", l.stats.tx_frames)),
    ("rx_bytes", |l| format!("{}\n", l.stats.rx_bytes)),
    ("tx_bytes", |l| format!("{}\n", l.stats.tx_bytes)),
    ("rx_errors", |_| String::from("0\n")),
    ("tx_errors", |_| String::from("0\n")),
    ("rx_dropped", |l| format!("{}\n", l.stats.rx_dropped)),
    ("tx_dropped", |l| format!("{}\n", l.stats.tx_dropped)),
    ("multicast", |_| String::from("0\n")),
    ("collisions", |_| String::from("0\n")),
    ("rx_length_errors", |_| String::from("0\n")),
    ("rx_over_errors", |_| String::from("0\n")),
    ("rx_crc_errors", |_| String::from("0\n")),
    ("rx_frame_errors", |_| String::from("0\n")),
    ("rx_fifo_errors", |_| String::from("0\n")),
    ("rx_missed_errors", |_| String::from("0\n")),
    ("tx_aborted_errors", |_| String::from("0\n")),
    ("tx_carrier_errors", |_| String::from("0\n")),
    ("tx_fifo_errors", |_| String::from("0\n")),
    ("tx_heartbeat_errors", |_| String::from("0\n")),
    ("tx_window_errors", |_| String::from("0\n")),
    ("rx_compressed", |_| String::from("0\n")),
    ("tx_compressed", |_| String::from("0\n")),
    ("rx_nohandler", |_| String::from("0\n")),
];

struct StatsDir(String);

impl DirSource for StatsDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(STATS.iter().enumerate().map(|(i, (n, f))| SynthEntry::file(n, 0o444, Arc::new(attr(&self.0, *f))).cookie(i as u64 + 1)).collect())
    }
}

struct IfDir(String);

impl DirSource for IfDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let Some(l) = net::ifview::by_name(self.0.as_bytes()) else { return Ok(Vec::new()) };
        let mut v: Vec<SynthEntry> = ATTRS
            .iter()
            .map(|(n, f)| {
                let a = attr(&self.0, *f);
                if WRITABLE.contains(n) { SynthEntry::file(n, 0o644, Arc::new(a.writable(store(*n, &self.0)))) } else { SynthEntry::file(n, 0o444, Arc::new(a)) }
            })
            .collect();
        // After `operstate` in Linux's list; only devices with ethtool link settings answer them.
        v.push(SynthEntry::file("speed", 0o444, Arc::new(link_attr(&self.0, speed_text))));
        v.push(SynthEntry::file("duplex", 0o444, Arc::new(link_attr(&self.0, duplex_text))));
        v.push(SynthEntry::dir("statistics", Arc::new(StatsDir(self.0.clone()))).with_perm(0o755));
        v.push(SynthEntry::link("subsystem", "../../../../class/net"));
        let ev = format!("INTERFACE={}\nIFINDEX={}\n", l.name, l.index);
        v.push(SynthEntry::file("uevent", 0o644, Arc::new(Live::text(ev).writable(uevent_store))));
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = i as u64 + 1;
        }
        Ok(v)
    }
}

/// `/sys/devices/virtual/net`: one directory per interface.
struct NetDevices;

impl DirSource for NetDevices {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(net::ifview::links().into_iter().map(|l| SynthEntry::dir(&l.name, Arc::new(IfDir(l.name.clone()))).with_perm(0o755).cookie(l.index as u64)).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        Ok(net::ifview::by_name(name).map(|l| SynthEntry::dir(&l.name, Arc::new(IfDir(l.name.clone()))).with_perm(0o755)))
    }
}

/// `/sys/class/net`: one link per interface.
struct NetClass;

fn class_link(l: &Link) -> SynthEntry {
    SynthEntry::link(&l.name, &format!("../../devices/virtual/net/{}", l.name)).cookie(l.index as u64)
}

impl DirSource for NetClass {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(net::ifview::links().iter().map(class_link).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        Ok(net::ifview::by_name(name).map(|l| class_link(&l)))
    }
}

pub(super) fn net_devices() -> Arc<dyn DirSource> {
    Arc::new(NetDevices)
}

pub(super) fn net_class() -> Arc<dyn DirSource> {
    Arc::new(NetClass)
}
