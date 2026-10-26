//! Network (owner: NW; contracts `C: NetDevice` back ends, `C: KSocket`, V2-PLAN §6, §9.1).
//!
//! `KSocket` is the kernel-internal socket API the MCP server, the mirror, `fetch` and Reload use. It exists
//! twice: `host::StdSockets` (std::net, for host tests of MC-1 and others; built only off-target) and the
//! kernel implementation: `stack::Stack` (NW-3), smoltcp interfaces on every network card plus the loopback for
//! 127.0.0.1, installed by `init()`. Every connection over a card is recorded for the connection view (B3);
//! every listener of the kernel's stack takes private networks only (spec §2.4, B8, NW-4), and the lifeline (MCP)
//! listens with `tcp_listen_private`, which adds the reserve: kept listening sockets a memory-hungry program
//! cannot take away. Memory for socket buffers is asked for, never assumed: a refusal is `NetError::NoMemory` to
//! the caller (R2). Cards wake the stack by interrupt where they can (`netdev::irq_wake`, NW-4).
#![cfg_attr(target_os = "none", no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod accept;
pub mod conn;
pub mod dhcpview;
pub mod frames;
pub mod http;
pub mod ifview;
#[cfg(target_os = "none")]
mod ktests;
pub mod loopback;
pub mod netdev;
pub mod route;
pub mod sock;
pub mod stack;
#[cfg(not(target_os = "none"))]
pub mod host;

use alloc::boxed::Box;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const LOCALHOST: Ipv4 = Ipv4([127, 0, 0, 1]);
    pub const ANY: Ipv4 = Ipv4([0, 0, 0, 0]);
    pub const BROADCAST: Ipv4 = Ipv4([255, 255, 255, 255]);
    /// 127.0.0.0/8: handled by the loopback, never by a card.
    pub fn is_loopback(&self) -> bool {
        self.0[0] == 127
    }
    pub fn is_unspecified(&self) -> bool {
        self.0 == [0; 4]
    }
    /// RFC 1918 + loopback + link-local: a home-network address (RB's fetch URLs, MCP's own peer check, which
    /// also sees loopback peers). The kernel stack's private-only listener uses the stricter `is_rfc1918`.
    pub fn is_private(&self) -> bool {
        let [a, b, ..] = self.0;
        a == 10 || a == 127 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 169 && b == 254)
    }
    /// 10/8, 172.16/12, 192.168/16 only (V1 `tcp_private`, spec §2.4): the peers a private-only listener takes
    /// on a card. Link-local (169.254/16) is out: any host on the segment can pick such an address itself.
    pub fn is_rfc1918(&self) -> bool {
        let [a, b, ..] = self.0;
        a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockAddr {
    pub ip: Ipv4,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    WouldBlock,
    TimedOut,
    ConnectionRefused,
    ConnectionReset,
    AddrInUse,
    NotConnected,
    /// Refused by policy (B3) or by the private-network rule.
    Refused,
    NoMemory,
    Unreachable,
    Closed,
    Other,
}

/// A connected TCP stream. Blocking calls take a timeout (B4: no unbounded waits in upper classes).
pub trait TcpStream: Send + Sync {
    /// Reads at least 1 byte or returns 0 at EOF; `Err(TimedOut)` after `timeout_ns`.
    fn read(&self, buf: &mut [u8], timeout_ns: Option<u64>) -> Result<usize, NetError>;
    /// Writes all of `buf` or fails.
    fn write_all(&self, buf: &[u8], timeout_ns: Option<u64>) -> Result<(), NetError>;
    fn peer(&self) -> SockAddr;
    fn local(&self) -> SockAddr;
    /// Graceful close (FIN after the queued data).
    fn close(&self);
    /// Abort (RST, V1 `srv_kill`): queued data is dropped, the peer sees a reset, and a `read` blocked on this
    /// stream in another thread wakes with `Err(ConnectionReset)`. The default (for test doubles) is `close`.
    fn shutdown(&self) {
        self.close();
    }
    /// Bytes waiting to be read (readiness without blocking).
    fn readable(&self) -> bool;
}

pub trait TcpListener: Send + Sync {
    fn accept(&self, timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError>;
    fn local(&self) -> SockAddr;
}

pub trait UdpSocket: Send + Sync {
    fn send_to(&self, buf: &[u8], to: SockAddr) -> Result<(), NetError>;
    fn recv_from(&self, buf: &mut [u8], timeout_ns: Option<u64>) -> Result<(usize, SockAddr), NetError>;
}

/// `C: KSocket`.
pub trait KSocket: Send + Sync {
    fn tcp_listen(&self, addr: SockAddr, backlog: usize) -> Result<Box<dyn TcpListener>, NetError>;
    /// The lifeline's listener (MCP): private networks only (spec §2.4; B8: every BitsOS service): a peer on a card
    /// outside 10/8, 172.16/12 and 192.168/16 (RFC 1918, `Ipv4::is_rfc1918`; link-local too) never gets a
    /// connection; loopback peers are local. The kernel's stack answers such a SYN with RST before any
    /// connection state exists (since NW-4 it does so for `tcp_listen` too) and adds the **reserve** (NW-4,
    /// V2-PLAN §6): at least 3 listening sockets per card, made at once and refilled after each accept from a
    /// kept pool whose sockets return after their connection, so MCP still accepts with the heap exhausted. The
    /// default (loopback, host sockets, test doubles) is `tcp_listen`: they reach no public peer, and the
    /// listener's owner still checks `peer()`.
    fn tcp_listen_private(&self, addr: SockAddr, backlog: usize) -> Result<Box<dyn TcpListener>, NetError> {
        self.tcp_listen(addr, backlog)
    }
    fn tcp_connect(&self, to: SockAddr, timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError>;
    fn udp_bind(&self, addr: SockAddr) -> Result<Box<dyn UdpSocket>, NetError>;
}

static STACK: ksync::OnceRef<dyn KSocket> = ksync::OnceRef::new();

/// Installs the kernel's socket implementation (the first call wins).
pub fn set_stack(s: &'static dyn KSocket) {
    STACK.set(s);
}

static CONFIG: ksync::OnceRef<dyn Fn(&str) -> Option<alloc::string::String> + Sync> = ksync::OnceRef::new();

/// A CONFIG.TXT value (None before `init` or when the key is missing). Keys read by NW: `net_test_url`.
pub fn config(key: &str) -> Option<alloc::string::String> {
    CONFIG.get().and_then(|f| f(key))
}

type SecretFn = dyn Fn(&str, &mut dyn FnMut(&[u8])) -> bool + Sync;
static SECRET: ksync::OnceRef<SecretFn> = ksync::OnceRef::new();

/// WL-4 CR 4: the kernel's take-once secret store (config::take_secret), set before the drivers bind.
pub fn set_secret_source(f: &'static SecretFn) -> bool {
    SECRET.set(f)
}

/// Hands the CONFIG secret `key` (the WLAN passphrase) to `f` once; the kernel zeroes its copy afterwards. False
/// when it is missing or was taken already.
pub fn take_secret(key: &str, f: &mut dyn FnMut(&[u8])) -> bool {
    SECRET.get().is_some_and(|s| s(key, f))
}

/// Starts the network stack (smoltcp on every card the drivers bring up, now or later, plus the loopback) with
/// its poll thread, and installs it as `stack()`. The kernel calls this once after `pci::bind_drivers` and
/// before `mcp::start_kernel`, with its CONFIG lookup. If the poll thread cannot start, the loopback alone is
/// installed (MCP then still works for kernel-internal tests) and the reason is logged. Calling it again returns
/// the installed stack.
#[cfg(target_os = "none")]
pub fn init(config: &'static (dyn Fn(&str) -> Option<alloc::string::String> + Sync)) -> &'static dyn KSocket {
    CONFIG.set(config);
    static FALLBACK: loopback::Loopback = loopback::Loopback::new();
    match stack::start() {
        Ok(s) => set_stack(s),
        Err(e) => {
            klog::event!(Error, "net", "stack.fallback", error = e; "only 127.0.0.1 works");
            set_stack(&FALLBACK);
        }
    }
    STACK.get().unwrap_or(&FALLBACK)
}

pub fn stack() -> Option<&'static dyn KSocket> {
    STACK.get()
}
