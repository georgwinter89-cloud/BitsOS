//! The kernel's mirror: CONFIG, the ring from RAM, the deferred sink (from boot) and the `mirror` thread that
//! waits for the network, starts the listener's view and follows the ring.

use super::buf;
use super::wire::{self, DGRAM_MAX, Target};
use super::{MIRROR, Mirror, MirrorQuiesce, RouteInfo, StopReport};
use alloc::string::String;
use alloc::sync::Arc;
use kreg::Level;
use net::route::{self, IfRoute};
use net::{Ipv4, NetError, SockAddr, UdpSocket};

/// While the network is not up: how often the thread looks.
const ROUTE_POLL_MS: u64 = 50;
/// While live: route snapshot refresh for the emergency path (DHCP renew, link changes).
const ROUTE_REFRESH_NS: u64 = 1_000_000_000;
/// Idle wait between looks at the ring (klogd's kick ends it earlier).
const IDLE_NS: u64 = 50_000_000;
/// After a failed send.
const RETRY_MS: u64 = 50;
/// Datagrams per round before the thread lets others run.
const ROUND: usize = 64;
/// "Still waiting for the network" is said after this long, and again after each further stretch.
const WAIT_NOTE_NS: u64 = 30_000_000_000;
/// Failed binds / start sends: said on the first and then every this many tries.
const NOTE_EVERY: u32 = 100;
/// While stopped: how long one look for a resume waits (a resume kicks earlier).
const STOPPED_LOOK_NS: u64 = 1_000_000_000;

/// The listener from CONFIG: `mirror=`, else V1's `console=`.
fn configured() -> Option<(&'static str, String)> {
    if let Some(v) = kexec::boot::config("mirror") {
        return Some(("mirror", v));
    }
    kexec::boot::config("console").map(|v| ("console", v))
}

fn total_ram() -> u64 {
    mm_core::frames().map(|f| f.stats().total_bytes).unwrap_or(0)
}

fn start() -> Result<(), &'static str> {
    let Some((key, value)) = configured() else {
        // R9: no listener named, nothing is ever sent.
        klog::event!(Info, "klog", "mirror.off"; "no mirror= in CONFIG");
        return Ok(());
    };
    let target = match wire::parse_target(&value) {
        Ok(t) => t,
        Err(why) => {
            klog::event!(Warn, "klog", "mirror.config_bad", key = key, value = value.as_str(), why = why; "mirror off");
            return Ok(());
        }
    };
    let want = buf::bytes_for_memory(total_ram());
    // Smaller rings if memory is short (each try halves; 16 KiB is the end of it).
    let mut bytes = want;
    let m = loop {
        if let Some(m) = Mirror::new(target, bytes, time::now_ns) {
            break m;
        }
        if bytes <= 16 << 10 {
            klog::event!(Error, "klog", "mirror.no_memory", want = want; "mirror off");
            return Err("no memory for the mirror ring");
        }
        bytes /= 2;
    };
    let m: &'static Mirror = alloc::boxed::Box::leak(alloc::boxed::Box::new(m));
    if !MIRROR.set(m) {
        return Ok(());
    }
    klog::event!(Info, "klog", "mirror.config", target = ip_port(target).as_str(), ring_bytes = bytes, from = key);
    // From the oldest record klog still holds: the boot before the network is buffered here.
    klog::add_deferred_sink("mirror", m, true);
    if let Err(e) = sched::spawn("mirror", sched::Class::Normal, move || run(m)) {
        klog::event!(Error, "klog", "mirror.spawn_failed", error = alloc::format!("{e:?}"); "lines stay in the mirror ring");
        return Err("mirror thread");
    }
    Ok(())
}
kreg::initcall!(Level::Service, "klog.mirror", start);

/// klogd fed the sinks: the mirror thread sends now instead of at its next look.
pub fn kick() {
    if let Some(m) = MIRROR.get() {
        m.kick();
    }
}

fn ip_port(t: Target) -> String {
    let [a, b, c, d] = t.ip;
    alloc::format!("{a}.{b}.{c}.{d}:{}", t.port)
}

fn ip_str(ip: [u8; 4]) -> String {
    let [a, b, c, d] = ip;
    alloc::format!("{a}.{b}.{c}.{d}")
}

/// The card routing picks for the listener, with our address on it (None: no card can reach it yet).
pub fn find_route(t: Target) -> Option<(RouteInfo, String)> {
    let stack = net::stack::kernel()?;
    let list = stack.interfaces();
    let cards: alloc::vec::Vec<IfRoute> = list
        .iter()
        .enumerate()
        .map(|(index, s)| IfRoute { index, link_up: s.info.link_up, addr: s.info.addr, router: s.info.router, lease_seq: s.info.lease_seq, metric: s.info.metric })
        .collect();
    let pick = route::pick(&cards, Ipv4(t.ip))?;
    let s = list.get(pick.index)?;
    let (addr, _) = s.info.addr?;
    let card = net::netdev::by_name(&s.name)?;
    Some((RouteInfo { card, src_ip: addr.0, src_mac: s.mac, dst_mac: wire::ETH_BROADCAST }, s.name.clone()))
}

fn bind() -> Result<alloc::boxed::Box<dyn UdpSocket>, NetError> {
    let stack = net::stack().ok_or(NetError::Unreachable)?;
    // V1's source port when free (the emergency frames use it too), else any.
    stack
        .udp_bind(SockAddr { ip: Ipv4::ANY, port: wire::DEFAULT_PORT })
        .or_else(|_| stack.udp_bind(SockAddr { ip: Ipv4::ANY, port: 0 }))
}

fn reload_counter() -> u32 {
    kexec::boot_info().and_then(|(bi, _)| bi.reload().map(|r| r.counter)).unwrap_or(0)
}

/// What a start of the listener's view gave (for the `mirror.live` event).
struct Live {
    sock: alloc::boxed::Box<dyn UdpSocket>,
    ifname: String,
    src: [u8; 4],
    id: u32,
    reload: u32,
    start_pos: u64,
    skipped: u64,
    waited_ms: u64,
}

/// Steps 1-3 of a start (boot or resume): a route to the listener, a socket, the start datagram. Gives up
/// only if the start datagram cannot be built (never with `DGRAM_MAX`). Every wait is bounded and repeats.
fn go_live(m: &'static Mirror, sock: Option<alloc::boxed::Box<dyn UdpSocket>>) -> Option<Live> {
    let t0 = time::now_ns();
    let mut sock = sock;
    let mut next_note = t0 + WAIT_NOTE_NS;
    let mut bind_fails = 0u32;
    let mut send_fails = 0u32;
    // A new id for every start (boot, Reload or resume), as V1 did from the TSC.
    let reload = reload_counter();
    let id = (t0 ^ (t0 >> 32)) as u32 ^ reload.rotate_left(16);
    loop {
        // 1. A route to the listener (DHCP done, link up); looked up again after every failed send, so a new
        //    lease or another card is noticed.
        let Some((route, ifname)) = find_route(m.target()) else {
            m.set_route(None);
            if time::now_ns() >= next_note {
                next_note = time::now_ns() + WAIT_NOTE_NS;
                klog::event!(Warn, "klog", "mirror.waiting", target = ip_port(m.target()).as_str(), waited_s = (time::now_ns() - t0) / 1_000_000_000; "no card reaches the listener yet; the ring keeps the newest lines");
            }
            sched::sleep_ms(ROUTE_POLL_MS);
            continue;
        };
        let src = route.src_ip;
        m.set_route(Some(route));
        // 2. A socket (the stack is up once a card has an address).
        let s = match sock.take() {
            Some(s) => s,
            None => match bind() {
                Ok(s) => s,
                Err(e) => {
                    bind_fails += 1;
                    if bind_fails == 1 || bind_fails.is_multiple_of(NOTE_EVERY) {
                        klog::event!(Warn, "klog", "mirror.bind_failed", error = alloc::format!("{e:?}"), tries = bind_fails; "retrying every second");
                    }
                    sched::sleep_ms(1000);
                    continue;
                }
            },
        };
        // 3. The start datagram.
        let mut b = [0u8; DGRAM_MAX];
        let Some((n, start_pos, skipped)) = m.begin(id, reload, src, &mut b) else {
            klog::event!(Error, "klog", "mirror.begin_failed"; "mirror off");
            return None;
        };
        match s.send_to(&b[..n], SockAddr { ip: Ipv4(m.target().ip), port: m.target().port }) {
            Ok(()) => {
                if send_fails > 0 {
                    klog::event!(Info, "klog", "mirror.start_ok", tries = send_fails + 1);
                }
                m.set_live(true);
                return Some(Live { sock: s, ifname, src, id, reload, start_pos, skipped, waited_ms: (time::now_ns() - t0) / 1_000_000 });
            }
            Err(e) => {
                send_fails += 1;
                if send_fails == 1 || send_fails.is_multiple_of(NOTE_EVERY) {
                    klog::event!(Warn, "klog", "mirror.start_retry", error = alloc::format!("{e:?}"), tries = send_fails; "retrying");
                }
                sock = Some(s);
                sched::sleep_ms(RETRY_MS);
            }
        }
    }
}

fn run(m: &'static Mirror) {
    let mut sock = None;
    let mut registered = false;
    loop {
        let Some(l) = go_live(m, sock.take()) else { return };
        if !registered {
            // Once only: the quiesce entry stays registered across stops and resumes.
            registered = true;
            devmodel::register_quiesce(Arc::new(MirrorQuiesce(m)));
        }
        klog::event!(
            Info,
            "klog",
            "mirror.live",
            target = ip_port(m.target()).as_str(),
            ifname = l.ifname.as_str(),
            src = ip_str(l.src).as_str(),
            id = klog::Hex(l.id as u64),
            reload = l.reload,
            start_pos = l.start_pos,
            waited_ms = l.waited_ms
        );
        if l.skipped > 0 {
            klog::event!(Warn, "klog", "mirror.early_lost", bytes = l.skipped; "the ring overwrote text before it could go out");
        }
        follow(m, &*l.sock);
        sock = Some(l.sock);
        // After a Reload or reboot this thread never runs again (the listener's "mirror ended" or its absence
        // tells); after a stop that did not lead there it logs what the quiesce could not.
        if let Some(r) = m.take_stop_report() {
            log_stop(r);
        }
        // 5. Stopped by a quiesce (Reload or reboot under way): the cards are off. Only `Mirror::resume` (the
        //    way back after a failed Reload, RB-5b) starts a new view; until then the ring keeps collecting.
        while !m.take_resume() {
            m.wait_kick(STOPPED_LOOK_NS);
        }
        klog::event!(Info, "klog", "mirror.restart", previous_id = klog::Hex(l.id as u64); "new start after a stop");
    }
}

fn log_stop(r: StopReport) {
    if r.ended {
        klog::event!(Info, "klog", "mirror.stopped", ended = true, frames = r.frames, dropped = r.dropped);
    } else {
        klog::event!(
            Warn,
            "klog",
            "mirror.stopped",
            ended = false,
            frames = r.frames,
            dropped = r.dropped,
            no_route = r.no_route,
            timed_out = r.timed_out;
            "the end datagram did not go out"
        );
    }
}

/// 4. Follow the ring until a quiesce stops the mirror.
fn follow(m: &'static Mirror, sock: &dyn UdpSocket) {
    let mut next_route = time::now_ns() + ROUTE_REFRESH_NS;
    let mut failing = false;
    while m.is_live() {
        if time::now_ns() >= next_route {
            next_route = time::now_ns() + ROUTE_REFRESH_NS;
            let r = find_route(m.target());
            let has = r.is_some();
            if m.set_route(r.map(|r| r.0)) {
                klog::event!(Info, "klog", "mirror.route", reachable = has);
            }
        }
        match m.flush(sock, ROUND) {
            Ok((sent, lost)) => {
                if failing {
                    failing = false;
                    klog::event!(Info, "klog", "mirror.send_ok");
                }
                if lost > 0 {
                    klog::event!(Warn, "klog", "mirror.lost", bytes = lost; "the ring overwrote text before it went out");
                }
                if sent == ROUND {
                    sched::yield_now();
                } else {
                    m.wait_kick(IDLE_NS);
                }
            }
            Err(e) => {
                if !failing {
                    failing = true;
                    klog::event!(Warn, "klog", "mirror.send_failed", error = alloc::format!("{e:?}"); "retrying");
                }
                sched::sleep_ms(RETRY_MS);
            }
        }
    }
}
