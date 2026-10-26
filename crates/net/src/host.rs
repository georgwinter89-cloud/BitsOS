//! `KSocket` over std::net for host tests (MC-1 runs its HTTP/MCP server against this on CachyOS).

extern crate std;
use crate::*;
use std::io::{Read, Write};
use std::time::Duration;

pub struct StdSockets;

fn conv(e: std::io::Error) -> NetError {
    use std::io::ErrorKind::*;
    match e.kind() {
        WouldBlock => NetError::WouldBlock,
        TimedOut => NetError::TimedOut,
        ConnectionRefused => NetError::ConnectionRefused,
        ConnectionReset | BrokenPipe => NetError::ConnectionReset,
        AddrInUse => NetError::AddrInUse,
        NotConnected => NetError::NotConnected,
        _ => NetError::Other,
    }
}
fn to_std(a: SockAddr) -> std::net::SocketAddr {
    std::net::SocketAddr::from((a.ip.0, a.port))
}
fn from_std(a: std::net::SocketAddr) -> SockAddr {
    match a {
        std::net::SocketAddr::V4(v) => SockAddr { ip: Ipv4(v.ip().octets()), port: v.port() },
        std::net::SocketAddr::V6(v) => SockAddr { ip: Ipv4::ANY, port: v.port() },
    }
}
fn dur(ns: Option<u64>) -> Option<Duration> {
    ns.map(|n| Duration::from_nanos(n.max(1)))
}

struct S(std::net::TcpStream);
impl TcpStream for S {
    fn read(&self, buf: &mut [u8], t: Option<u64>) -> Result<usize, NetError> {
        self.0.set_read_timeout(dur(t)).map_err(conv)?;
        (&self.0).read(buf).map_err(conv)
    }
    fn write_all(&self, buf: &[u8], t: Option<u64>) -> Result<(), NetError> {
        self.0.set_write_timeout(dur(t)).map_err(conv)?;
        (&self.0).write_all(buf).map_err(conv)
    }
    fn peer(&self) -> SockAddr {
        self.0.peer_addr().map(from_std).unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 })
    }
    fn local(&self) -> SockAddr {
        self.0.local_addr().map(from_std).unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 })
    }
    fn close(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Write);
    }
    fn readable(&self) -> bool {
        let mut b = [0u8; 1];
        self.0.set_nonblocking(true).ok();
        let r = self.0.peek(&mut b).map(|n| n > 0).unwrap_or(false);
        self.0.set_nonblocking(false).ok();
        r
    }
}

struct L(std::net::TcpListener);
impl TcpListener for L {
    fn accept(&self, t: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        match t {
            None => {
                self.0.set_nonblocking(false).map_err(conv)?;
                let (s, _) = self.0.accept().map_err(conv)?;
                Ok(Box::new(S(s)))
            }
            Some(ns) => {
                self.0.set_nonblocking(true).map_err(conv)?;
                let end = std::time::Instant::now() + Duration::from_nanos(ns);
                loop {
                    match self.0.accept() {
                        Ok((s, _)) => {
                            s.set_nonblocking(false).map_err(conv)?;
                            return Ok(Box::new(S(s)));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= end {
                                return Err(NetError::TimedOut);
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => return Err(conv(e)),
                    }
                }
            }
        }
    }
    fn local(&self) -> SockAddr {
        self.0.local_addr().map(from_std).unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 })
    }
}

struct U(std::net::UdpSocket);
impl UdpSocket for U {
    fn send_to(&self, buf: &[u8], to: SockAddr) -> Result<(), NetError> {
        self.0.send_to(buf, to_std(to)).map(|_| ()).map_err(conv)
    }
    fn recv_from(&self, buf: &mut [u8], t: Option<u64>) -> Result<(usize, SockAddr), NetError> {
        self.0.set_read_timeout(dur(t)).map_err(conv)?;
        self.0.recv_from(buf).map(|(n, a)| (n, from_std(a))).map_err(conv)
    }
}

impl KSocket for StdSockets {
    fn tcp_listen(&self, addr: SockAddr, _backlog: usize) -> Result<Box<dyn TcpListener>, NetError> {
        Ok(Box::new(L(std::net::TcpListener::bind(to_std(addr)).map_err(conv)?)))
    }
    fn tcp_connect(&self, to: SockAddr, t: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        let s = match dur(t) {
            Some(d) => std::net::TcpStream::connect_timeout(&to_std(to), d),
            None => std::net::TcpStream::connect(to_std(to)),
        }
        .map_err(conv)?;
        Ok(Box::new(S(s)))
    }
    fn udp_bind(&self, addr: SockAddr) -> Result<Box<dyn UdpSocket>, NetError> {
        Ok(Box::new(U(std::net::UdpSocket::bind(to_std(addr)).map_err(conv)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn std_sockets_work() {
        let s = StdSockets;
        let l = s.tcp_listen(SockAddr { ip: Ipv4::LOCALHOST, port: 0 }, 1).unwrap();
        let port = l.local().port;
        let c = s.tcp_connect(SockAddr { ip: Ipv4::LOCALHOST, port }, Some(1_000_000_000)).unwrap();
        let a = l.accept(Some(1_000_000_000)).unwrap();
        c.write_all(b"hi", None).unwrap();
        let mut b = [0u8; 2];
        assert_eq!(a.read(&mut b, Some(1_000_000_000)).unwrap(), 2);
    }
}
