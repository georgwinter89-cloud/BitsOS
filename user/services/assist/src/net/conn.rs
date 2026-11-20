//! One connection: name lookup, TCP connect, the TLS handshake. The socket wakes every [`TICK`] so a wait ends
//! at the deadline or when the caller cancels (Esc in the chat), whatever the other side does.

use super::url::Url;
use super::{Ctl, NetError, tls};
use rustls::{ClientConnection, StreamOwned};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(150);
/// A host that does not answer the connect, or a name server that does not answer the lookup, is given up
/// after this long, so a fallback provider gets its turn well before the answer timeout (Linux gives up on an
/// unanswered SYN only after about two minutes).
const CONNECT: Duration = Duration::from_secs(10);

enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

pub struct Conn {
    s: Stream,
}

fn timed_out(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

/// Err when the caller's patience is over.
fn check(ctl: &Ctl) -> Result<(), NetError> {
    if (ctl.cancel)() {
        return Err(NetError::Cancelled);
    }
    if Instant::now() >= ctl.deadline {
        return Err(NetError::Timeout);
    }
    Ok(())
}

fn tls_error(e: &io::Error) -> Option<NetError> {
    let te = e.get_ref()?.downcast_ref::<rustls::Error>()?;
    Some(if tls::is_time_error(te) { NetError::Clock(te.to_string()) } else { NetError::Tls(te.to_string()) })
}

/// The addresses of the URL's host. The system's lookup cannot be interrupted, so it runs on a thread of its
/// own and this side waits in [`TICK`] steps: Esc and the deadline end the wait, and a name server that stays
/// silent is given up like a host that does not answer the connect. A thread left behind ends with the
/// resolver's own timeout.
pub fn resolve(url: &Url, ctl: &Ctl) -> Result<Vec<SocketAddr>, NetError> {
    if let Ok(ip) = url.host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, url.port)]);
    }
    check(ctl)?;
    let (tx, rx) = mpsc::channel();
    let name = (url.host.clone(), url.port);
    std::thread::Builder::new()
        .name("assist-lookup".into())
        .spawn(move || {
            let found = (name.0.as_str(), name.1).to_socket_addrs().map(|a| a.collect::<Vec<_>>()).map_err(|e| e.to_string());
            let _ = tx.send(found);
        })
        .map_err(|e| NetError::Dns(format!("{}: {e}", url.host)))?;
    let give_up = Instant::now() + CONNECT;
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Ok(addrs)) if !addrs.is_empty() => return Ok(addrs),
            Ok(Ok(_)) => return Err(NetError::Dns(format!("{}: no address", url.host))),
            Ok(Err(e)) => return Err(NetError::Dns(format!("{}: {e}", url.host))),
            Err(RecvTimeoutError::Disconnected) => return Err(NetError::Dns(format!("{}: the lookup ended without an answer", url.host))),
            Err(RecvTimeoutError::Timeout) => {
                check(ctl)?;
                if Instant::now() >= give_up {
                    return Err(NetError::Dns(format!("{}: no answer from the name server", url.host)));
                }
            }
        }
    }
}

impl Conn {
    /// Connects to the first of `addrs` that answers (the caller resolved and checked them) and, for https,
    /// completes the handshake.
    pub fn open(url: &Url, addrs: &[SocketAddr], ctl: &Ctl) -> Result<Conn, NetError> {
        let mut last = NetError::Connect(format!("{}: nothing to connect to", url.host));
        let mut tcp = None;
        for a in addrs {
            check(ctl)?;
            let left = ctl.deadline.saturating_duration_since(Instant::now()).min(CONNECT);
            match TcpStream::connect_timeout(a, left.max(Duration::from_millis(1))) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) if timed_out(&e) => last = NetError::Connect(format!("{a}: no answer")),
                Err(e) => last = NetError::Connect(format!("{a}: {e}")),
            }
        }
        let Some(tcp) = tcp else { return Err(last) };
        tcp.set_read_timeout(Some(TICK)).map_err(|e| NetError::Connect(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        if !url.https {
            return Ok(Conn { s: Stream::Plain(tcp) });
        }
        let bundle = tls::bundle_path().map_err(NetError::Tls)?;
        let cfg = tls::config(&bundle).map_err(NetError::Tls)?;
        let name = tls::server_name(&url.host).map_err(NetError::Tls)?;
        let conn = ClientConnection::new(cfg, name).map_err(|e| NetError::Tls(e.to_string()))?;
        let mut s = StreamOwned::new(conn, tcp);
        s.sock.set_write_timeout(Some(TICK)).map_err(|e| NetError::Connect(e.to_string()))?;
        while s.conn.is_handshaking() {
            check(ctl)?;
            match s.conn.complete_io(&mut s.sock) {
                Ok(_) => {}
                Err(e) if timed_out(&e) || e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(tls_error(&e).unwrap_or_else(|| NetError::Tls(format!("handshake: {e}")))),
            }
        }
        Ok(Conn { s: Stream::Tls(Box::new(s)) })
    }

    fn sock(&self) -> &TcpStream {
        match &self.s {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }

    /// The whole request in one piece. Bounded by the deadline, not cancellable: a half-written TLS record
    /// cannot be taken back, and a request is small next to its answer.
    pub fn write_all(&mut self, data: &[u8], ctl: &Ctl) -> Result<(), NetError> {
        check(ctl)?;
        let left = ctl.deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        self.sock().set_write_timeout(Some(left)).map_err(|e| NetError::Connect(e.to_string()))?;
        let r = match &mut self.s {
            Stream::Plain(s) => s.write_all(data).and_then(|_| s.flush()),
            Stream::Tls(s) => s.write_all(data).and_then(|_| s.flush()),
        };
        r.map_err(|e| {
            if timed_out(&e) {
                NetError::Timeout
            } else {
                tls_error(&e).unwrap_or_else(|| NetError::Cut(format!("sending the request: {e}")))
            }
        })
    }

    /// Some bytes, or 0 when the other side closed.
    pub fn read(&mut self, buf: &mut [u8], ctl: &Ctl) -> Result<usize, NetError> {
        loop {
            check(ctl)?;
            let r = match &mut self.s {
                Stream::Plain(s) => s.read(buf),
                Stream::Tls(s) => s.read(buf),
            };
            match r {
                Ok(n) => return Ok(n),
                Err(e) if timed_out(&e) || e.kind() == ErrorKind::Interrupted => {}
                // A TLS peer that closes without close_notify: the framing above decides whether that is a cut.
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(tls_error(&e).unwrap_or_else(|| NetError::Cut(e.to_string()))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_needs_no_lookup_and_a_lookup_ends_with_the_callers_patience() {
        let no = || false;
        let yes = || true;
        let patient = Ctl { deadline: Instant::now() + Duration::from_secs(5), budget: 1 << 20, cancel: &no };
        let v4 = Url::parse("http://192.168.100.20:8080/v1").unwrap();
        assert_eq!(resolve(&v4, &patient), Ok(vec!["192.168.100.20:8080".parse().unwrap()]));
        let v6 = Url::parse("https://[fd00::1]:11434").unwrap();
        assert_eq!(resolve(&v6, &patient), Ok(vec!["[fd00::1]:11434".parse().unwrap()]));
        // A name: Esc or a deadline that passed ends it before any lookup starts.
        let name = Url::parse("https://provider.invalid/v1").unwrap();
        let cancelled = Ctl { deadline: Instant::now() + Duration::from_secs(5), budget: 1 << 20, cancel: &yes };
        assert_eq!(resolve(&name, &cancelled), Err(NetError::Cancelled));
        let late = Ctl { deadline: Instant::now(), budget: 1 << 20, cancel: &no };
        assert_eq!(resolve(&name, &late), Err(NetError::Timeout));
    }
}
