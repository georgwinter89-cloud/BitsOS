//! HTTP/1.1 POST with a streamed answer over TCP or TLS (rustls), std only and blocking. Every wait ends at the
//! caller's deadline or when the caller cancels; every buffer is bounded by the caller's budget.

pub mod conn;
pub mod http;
pub mod sse;
pub mod tls;
pub mod url;

use std::time::Instant;

/// What a caller allows one exchange: until when, how many bytes, and a question asked while waiting.
pub struct Ctl<'a> {
    pub deadline: Instant,
    pub budget: usize,
    pub cancel: &'a dyn Fn() -> bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    Dns(String),
    Connect(String),
    /// TLS failed for a reason other than the clock.
    Tls(String),
    /// The certificate is not valid at this machine's time (V2 without a clock source counts from 1970).
    Clock(String),
    Timeout,
    Cancelled,
    /// The connection ended in the middle of the answer.
    Cut(String),
    Protocol(String),
    /// More than the budget.
    TooBig,
}
