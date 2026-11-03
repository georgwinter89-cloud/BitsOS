//! Wakes for client sockets without relying on epoll inside epoll (WY-1a). Smithay's display keeps its clients in
//! an epoll of its own and calloop watches that epoll's fd; on V2 (20.10.) the inner epoll's readiness did not
//! reach the outer one after the first message (a client stuck after its first roundtrip; change request in
//! wy-WY-1a.md). So each client socket is also watched directly through a duplicate fd, and the main loop
//! dispatches the display every turn (one epoll_wait(0) when nothing is pending). Remove this file once the
//! nested-epoll change request has landed.
//!
//! The duplicate must never keep a client alive that the display has dropped (a protocol error, later B4's "kill
//! the hung client"): `ClientState::disconnected` shuts the socket down through the shared duplicate, so the
//! client sees end of file at once and this source ends; the source also ends when the display no longer knows
//! the client, when the socket reports end of file or an error.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::backend::ClientId;

use crate::state::App;

/// The duplicate of a client's socket, shared by its watcher source and its `ClientState`.
pub type Watch = Arc<OwnedFd>;

pub fn duplicate(stream: &UnixStream) -> Option<Watch> {
    stream.try_clone().ok().map(|s| Arc::new(OwnedFd::from(s)))
}

/// Called when the display drops the client, for whatever reason.
pub fn hang_up(w: &Watch) {
    // SAFETY: shutdown on the duplicate the Arc keeps open; it only changes the socket's state, no memory.
    unsafe { libc::shutdown(w.as_raw_fd(), libc::SHUT_RDWR) };
}

/// The peer's pid from SO_PEERCRED at accept (linux-6.6/net/core/sock.c sk_getsockopt SO_PEERCRED: the creds
/// stored at connect, af_unix.c unix_stream_connect); 0 when unknown. Read once here, so no later lookup can fail.
pub fn peer_pid(stream: &UnixStream) -> i32 {
    peer_cred(stream).map_or(0, |(pid, _)| pid)
}

/// The peer's (pid, uid) from SO_PEERCRED; None when the socket does not say.
pub fn peer_cred(stream: &UnixStream) -> Option<(i32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, a local of exactly that size.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0 && cred.pid > 0).then_some((cred.pid, cred.uid))
}

/// Whether the peer has closed its end (a peek reads end of file). False while it is alive, even if it does not
/// read: then bitscomp itself dropped it (its output queue overflowed, or a malformed request).
pub fn peer_closed(w: &Watch) -> bool {
    let mut b = [0u8; 1];
    // SAFETY: a one-byte non-blocking peek into `b` on the duplicate the Arc keeps open; nothing is consumed.
    let n = unsafe { libc::recv(w.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n == 0
        || (n < 0 && !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)))
}

pub fn watch(handle: &LoopHandle<'static, App>, w: Watch, id: ClientId) {
    let raw = w.as_raw_fd();
    let peek = w.clone();
    let _ = handle.insert_source(Generic::new(w, Interest::READ, Mode::Level), move |_, _, app| {
        app.woke.clients += 1;
        if app.dh.backend_handle().get_client_data(id.clone()).is_err() {
            return Ok(PostAction::Remove);
        }
        let mut b = [0u8; 1];
        // SAFETY: a one-byte peek into `b` on the duplicate this source owns; nothing is consumed.
        let n = unsafe { libc::recv(raw, b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        let again =
            n < 0 && matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR));
        // Data waits: the display reads it this turn. End of file or an error: the client is gone.
        // B4 (WY-4): bitscomp never pauses reading a client (a hung one simply sends nothing), so this source never
        // fires for unread data of a client left alone.
        if !again && !app.unread.iter().any(|(i, _)| *i == id) {
            // Data or end of file: `read_missed` makes sure the display takes it this turn.
            app.unread.push((id.clone(), peek.clone()));
        }
        Ok(if n > 0 || again { PostAction::Continue } else { PostAction::Remove })
    });
}

/// Whether the socket takes more data now (poll POLLOUT). A unix stream socket stops being writable once the data
/// its peer has not read fills a quarter of its send buffer (linux-6.6/net/unix/af_unix.c unix_writable), so a
/// peer that reads nothing makes this false long before wayland-backend's own bounded queue would have to hold
/// anything. True when poll fails (no reason to hold back).
pub fn writable(w: &Watch) -> bool {
    let mut p = libc::pollfd { fd: w.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
    // SAFETY: poll on one pollfd, a local; timeout 0, so it never blocks.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n != 0 && (n < 0 || p.revents & libc::POLLOUT != 0)
}

/// Clients whose data still waited after the display's last dispatch, and how often `read_missed` had to read
/// a client itself since its last line.
#[derive(Default)]
pub struct Missed {
    suspects: Vec<ClientId>,
    count: u64,
    logged: Option<Duration>,
}

/// After the display's dispatch. wayland-backend reads a client it dispatches until its socket would block
/// (common_poll.rs dispatch_events_for), so data still waiting afterwards either came in since or was not
/// reported by the display's epoll. The first is taken by the next turn's dispatch; only a client still waiting
/// after two dispatches in a row is read directly (wayland-backend's dispatch_single_client; its errors drop the
/// client as the display's own dispatch would). The usual case costs one peek. One line a minute at most (B6: a
/// fallback).
pub fn read_missed(display: &mut Display<App>, app: &mut App, log: &mut Missed, now: Duration) {
    let unread = std::mem::take(&mut app.unread);
    let reads = read_twice_missed(display, app, unread, &mut log.suspects);
    app.stats.unread_reads += reads;
    log.count += reads;
    if log.count > 0 && log.logged.is_none_or(|t| now.saturating_sub(t) >= crate::spin::LOG_EVERY) {
        eprintln!(
            "bitscomp: clients: {} time(s) a client's data still waited after two display dispatches in a row (the display's epoll did not report it); read directly",
            log.count
        );
        log.logged = Some(now);
        log.count = 0;
    }
}

/// The core of `read_missed` for any display state: the clients read directly.
fn read_twice_missed<D: 'static>(
    display: &mut Display<D>,
    data: &mut D,
    unread: Vec<(ClientId, Watch)>,
    suspects: &mut Vec<ClientId>,
) -> u64 {
    let before = std::mem::take(suspects);
    let mut reads = 0;
    for (id, w) in unread {
        // Gone already (the display read its end of file and dropped it), or read by the display: nothing to do.
        if display.backend().handle().get_client_data(id.clone()).is_err() || !waiting(&w) {
            continue;
        }
        if !before.contains(&id) {
            suspects.push(id);
            continue;
        }
        reads += 1;
        let _ = display.backend().dispatch_single_client(data, id);
    }
    reads
}

/// Whether data, end of file or an error waits on the socket (a peek; nothing is consumed). An error the display
/// must see (ECONNRESET: the peer closed with data unread) counts; EBADF cannot happen while the Arc holds the fd.
fn waiting(w: &Watch) -> bool {
    let mut b = [0u8; 1];
    // SAFETY: a one-byte non-blocking peek into `b` on the duplicate the Arc keeps open; nothing is consumed.
    let n = unsafe { libc::recv(w.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n >= 0
        || !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN) | Some(libc::EINTR) | Some(libc::EBADF) | Some(libc::ENOTSOCK)
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn waiting_sees_data_and_end_of_file_but_consumes_nothing() {
        let (mut ours, mut peer) = UnixStream::pair().unwrap();
        let w = duplicate(&ours).unwrap();
        assert!(!waiting(&w), "nothing sent yet");
        peer.write_all(b"x").unwrap();
        assert!(waiting(&w));
        assert!(waiting(&w), "a peek leaves the byte for the display");
        let mut b = [0u8; 1];
        ours.read_exact(&mut b).unwrap();
        assert!(!waiting(&w), "read by the display: nothing to do");
        drop(peer);
        assert!(waiting(&w), "end of file must reach the display too");
    }

    #[test]
    fn writable_until_the_peer_leaves_too_much_unread() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let w = duplicate(&ours).unwrap();
        assert!(writable(&w));
        ours.set_nonblocking(true).unwrap();
        let mut sent = 0usize;
        while (&ours).write(&[0u8; 12]).is_ok() {
            sent += 12;
        }
        assert!(!writable(&w), "a peer that reads nothing ({sent} bytes unread)");
        let mut b = vec![0u8; sent];
        peer.read_exact(&mut b).unwrap();
        assert!(writable(&w), "read: room again");
    }

    struct NoData;
    impl smithay::reexports::wayland_server::backend::ClientData for NoData {}

    /// wl_display (object 1) sync (opcode 0) with new_id 2: a valid request the backend answers itself.
    fn sync_request() -> [u8; 12] {
        let mut m = [0u8; 12];
        m[0..4].copy_from_slice(&1u32.to_ne_bytes());
        m[4..8].copy_from_slice(&(12u32 << 16).to_ne_bytes());
        m[8..12].copy_from_slice(&2u32.to_ne_bytes());
        m
    }

    #[test]
    fn data_the_display_did_not_take_is_read_after_the_second_turn() {
        let mut display: Display<()> = Display::new().unwrap();
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let w = duplicate(&ours).unwrap();
        let id = display.handle().insert_client(ours, Arc::new(NoData)).unwrap().id();
        let mut suspects = Vec::new();
        // Nothing waits: no suspect, no read.
        assert_eq!(read_twice_missed(&mut display, &mut (), vec![(id.clone(), w.clone())], &mut suspects), 0);
        assert!(suspects.is_empty());
        // Data the display's dispatch did not take (it is never called here): first a suspect, then read.
        peer.write_all(&sync_request()).unwrap();
        assert_eq!(read_twice_missed(&mut display, &mut (), vec![(id.clone(), w.clone())], &mut suspects), 0);
        assert_eq!(suspects, vec![id.clone()]);
        assert!(waiting(&w), "a suspect is left for the display's next dispatch");
        assert_eq!(read_twice_missed(&mut display, &mut (), vec![(id.clone(), w.clone())], &mut suspects), 1);
        assert!(!waiting(&w), "read directly");
        assert!(suspects.is_empty());
        assert!(display.backend().handle().get_client_data(id.clone()).is_ok(), "a valid request keeps the client");
        // Data that the display took in between (here: read by hand) clears the suspicion.
        peer.write_all(&sync_request()).unwrap();
        read_twice_missed(&mut display, &mut (), vec![(id.clone(), w.clone())], &mut suspects);
        assert_eq!(suspects.len(), 1);
        display.backend().dispatch_single_client(&mut (), id.clone()).unwrap();
        assert_eq!(read_twice_missed(&mut display, &mut (), vec![(id.clone(), w.clone())], &mut suspects), 0);
        assert!(suspects.is_empty());
    }

    #[test]
    fn a_closed_or_malformed_client_left_unread_is_dropped() {
        let mut display: Display<()> = Display::new().unwrap();
        for garbage in [false, true] {
            let (ours, mut peer) = UnixStream::pair().unwrap();
            let w = duplicate(&ours).unwrap();
            let id = display.handle().insert_client(ours, Arc::new(NoData)).unwrap().id();
            if garbage {
                // Object 99 does not exist: a protocol error.
                let mut m = sync_request();
                m[0..4].copy_from_slice(&99u32.to_ne_bytes());
                peer.write_all(&m).unwrap();
            } else {
                drop(peer);
            }
            let mut suspects = Vec::new();
            let unread = || vec![(id.clone(), w.clone())];
            assert_eq!(read_twice_missed(&mut display, &mut (), unread(), &mut suspects), 0);
            assert_eq!(read_twice_missed(&mut display, &mut (), unread(), &mut suspects), 1);
            assert!(display.backend().handle().get_client_data(id.clone()).is_err(), "garbage={garbage}: dropped");
            assert_eq!(read_twice_missed(&mut display, &mut (), unread(), &mut suspects), 0, "gone: nothing to do");
        }
    }
}
