//! NW-10: Linux's quick-ACK mode for the receiving side of outgoing connections, on top of smoltcp's own policy
//! (an ACK for every second full segment, a lone segment's ACK delayed by 10 ms, at once when the window doubled).
//!
//! Linux ACKs every segment at the start of a connection and when data comes after a quiet spell
//! (linux-6.6/net/ipv4/tcp_input.c:301 `tcp_incr_quickack`: up to `TCP_MAX_QUICKACKS` = 16 segments,
//! include/net/tcp.h:83; tcp_input.c:791
//! `tcp_event_data_recv`: again after more than an RTO without data), so a sender in slow start, or one with Nagle
//! that holds its last small segment until everything before is acknowledged, never waits for the delayed-ACK
//! timer. After that, delayed ACKs (`TCP_DELACK_MIN` 40 ms there, smoltcp's 10 ms here, include/net/tcp.h:135)
//! halve the ACKs of a bulk transfer.
//!
//! smoltcp has no quick mode and does not show the segments it takes in, so the connection's reader drives it:
//! the bytes it takes count down the quick budget, and a read after a quiet spell starts a new one. A reader
//! blocked on the connection reads as the data comes, so read time is arrival time; the first segment after a
//! quiet spell is still acknowledged as smoltcp decided on arrival (up to 10 ms later), the following ones at once.
//! Deviations from Linux, which counts arriving segments (checker NW-10, kept: they cost a few ACKs, never a
//! stall): a reader that falls behind gets every segment that arrives before it takes the budget acknowledged at
//! once (more than 16), and a reader that pauses over 200 ms while data keeps arriving starts a new budget.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Linux `TCP_MAX_QUICKACKS` segments of an Ethernet MSS (1460; the measured `rcv_mss` there).
pub const QUICK_BYTES: u64 = 16 * 1460;
/// A quiet spell that starts quick mode again: Linux compares with the connection's RTO, at least
/// `TCP_RTO_MIN` (200 ms, include/net/tcp.h:142).
pub const QUIET_NS: u64 = 200_000_000;

/// What the socket's ACK delay must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// ACK at once (smoltcp `set_ack_delay(None)`).
    Quick,
    /// smoltcp's delayed ACK (`ACK_DELAY`).
    Delayed,
}

/// The quick-ACK state of one connection. The caller holds the socket's lock (the card's) around each call and
/// the `set_ack_delay` it applies, so the delay set last is the state decided last (checker NW-10: two readers of
/// one connection).
pub struct QuickAck {
    /// Bytes still acknowledged at once; 0: delayed mode.
    left: AtomicU64,
    /// When the reader last took bytes.
    last_ns: AtomicU64,
    /// TCP_QUICKACK 0 (Linux pingpong mode, linux-6.6/include/net/inet_connection_sock.h:330): delayed ACKs even
    /// within the quick budget (`tcp_in_quickack_mode` asks for !pingpong, net/ipv4/tcp_input.c:332).
    pingpong: AtomicBool,
}

impl QuickAck {
    /// A new connection starts in quick mode at `now_ns`.
    pub fn new(now_ns: u64) -> Self {
        QuickAck { left: AtomicU64::new(QUICK_BYTES), last_ns: AtomicU64::new(now_ns), pingpong: AtomicBool::new(false) }
    }

    /// The reader took `n` bytes at `now_ns`. True when a quiet spell started a new quick budget (counted).
    pub fn took(&self, n: u64, now_ns: u64) -> bool {
        let quiet = now_ns.saturating_sub(self.last_ns.swap(now_ns, Ordering::Relaxed)) > QUIET_NS;
        let left = self.left.load(Ordering::Relaxed);
        if left == 0 {
            if quiet {
                self.left.store(QUICK_BYTES, Ordering::Relaxed);
                return true;
            }
            return false;
        }
        self.left.store(left.saturating_sub(n), Ordering::Relaxed);
        false
    }

    /// TCP_QUICKACK: 0 enters pingpong mode, 1 leaves it (`__tcp_sock_set_quickack`, net/ipv4/tcp.c:3259; the quick
    /// budget is left as it is, as Linux leaves `icsk_ack.quick`).
    pub fn set_pingpong(&self, on: bool) {
        self.pingpong.store(on, Ordering::Relaxed);
    }

    /// The mode the socket's ACK delay must have now.
    pub fn mode(&self) -> Mode {
        if self.left.load(Ordering::Relaxed) > 0 && !self.pingpong.load(Ordering::Relaxed) { Mode::Quick } else { Mode::Delayed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_for_sixteen_segments_then_delayed_then_quick_after_quiet() {
        let q = QuickAck::new(1000);
        assert_eq!(q.mode(), Mode::Quick, "a new connection ACKs at once");
        assert!(!q.took(1448, 1000));
        assert!(!q.took(QUICK_BYTES - 1448 - 1, 2000));
        assert_eq!(q.mode(), Mode::Quick, "one byte of the budget left");
        assert!(!q.took(64 * 1024, 3000));
        assert_eq!(q.mode(), Mode::Delayed, "budget spent: delayed ACKs");
        assert!(!q.took(1448, 3000 + QUIET_NS), "exactly the quiet time is not quiet");
        assert_eq!(q.mode(), Mode::Delayed);
        assert!(q.took(1, 3000 + 2 * QUIET_NS + 1), "data after a quiet spell: a new budget");
        assert_eq!(q.mode(), Mode::Quick);
        q.took(QUICK_BYTES, 3000 + 2 * QUIET_NS + 2);
        assert_eq!(q.mode(), Mode::Delayed);
    }

    #[test]
    fn a_clock_that_goes_back_is_not_quiet() {
        let q = QuickAck::new(10 * QUIET_NS);
        q.took(QUICK_BYTES, 10 * QUIET_NS);
        assert_eq!(q.mode(), Mode::Delayed);
        assert!(!q.took(1, 0), "saturating: a smaller time is no quiet spell");
        assert_eq!(q.mode(), Mode::Delayed);
    }

    #[test]
    fn pingpong_delays_within_the_budget_and_leaving_it_keeps_the_budget() {
        let q = QuickAck::new(0);
        q.set_pingpong(true);
        assert_eq!(q.mode(), Mode::Delayed, "TCP_QUICKACK 0: delayed ACKs from the start");
        q.took(1000, 1);
        q.set_pingpong(false);
        assert_eq!(q.mode(), Mode::Quick, "TCP_QUICKACK 1: the rest of the budget is quick again");
        q.took(QUICK_BYTES, 2);
        q.set_pingpong(false);
        assert_eq!(q.mode(), Mode::Delayed, "a spent budget stays spent (Linux leaves icsk_ack.quick alone)");
    }
}
