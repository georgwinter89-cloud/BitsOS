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

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
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
    if r == 0 { cred.pid.max(0) } else { 0 }
}

/// Whether the peer has closed its end (a peek reads end of file). False while it is alive, even if it does not
/// read: then bitscomp itself dropped it (its output queue overflowed, or a malformed request).
pub fn peer_closed(w: &Watch) -> bool {
    let mut b = [0u8; 1];
    // SAFETY: a one-byte non-blocking peek into `b` on the duplicate the Arc keeps open; nothing is consumed.
    let n = unsafe { libc::recv(w.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n == 0 || (n < 0 && !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)))
}

pub fn watch(handle: &LoopHandle<'static, App>, w: Watch, id: ClientId) {
    let raw = w.as_raw_fd();
    let _ = handle.insert_source(Generic::new(w, Interest::READ, Mode::Level), move |_, _, app| {
        if app.dh.backend_handle().get_client_data(id.clone()).is_err() {
            return Ok(PostAction::Remove);
        }
        let mut b = [0u8; 1];
        // SAFETY: a one-byte peek into `b` on the duplicate this source owns; nothing is consumed.
        let n = unsafe { libc::recv(raw, b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        let again = n < 0 && matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR));
        // Data waits: the display reads it this turn. End of file or an error: the client is gone.
        // B4 (WY-4): bitscomp never pauses reading a client (a hung one simply sends nothing), so this source never
        // fires for unread data of a client left alone.
        Ok(if n > 0 || again { PostAction::Continue } else { PostAction::Remove })
    });
}
