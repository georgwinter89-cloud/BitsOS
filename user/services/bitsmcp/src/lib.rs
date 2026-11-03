//! bitsmcp (MC-8a): the MCP server as a user-space service. It speaks exactly the kernel server's protocol
//! (HTTP/1.1 + JSON-RPC, bearer token, the same tool names, schemas and answers: `mcp_wire` is the kernel's own
//! code), and reaches the kernel only through the gate `/dev/bitsos/mcp` (`mcp_wire::gate`): the tool list, the
//! token check (the kernel keeps the token), calls of the narrow set (hw_info, metrics, events, the journal,
//! Safe-class ktests, the start's identity) and installer requests (MC-9a: the kernel hands them to bitsinstall,
//! which asks a person or finds a standing grant; EPERM while no installer runs). Every other kernel tool
//! answers with the reason it is not offered. The kernel events of the gate put each call into the journal.
//!
//! `link`: the gate's descriptors (opened as root, before the service gives root up); `tools`: the catalog and
//! the runner over the gate; `serve`: connections, eviction and the request loop (the kernel server's rules);
//! `filter`: the seccomp program the service runs under; `log`: lines into the kernel log. MC-8b: `cert` (self-signed
//! certificates), `keys` (the keys on the stick), `tls` (mutual TLS with pinned keys, the one-time pairing).
#![forbid(unsafe_code)]

pub mod cert;
pub mod filter;
pub mod keys;
pub mod link;
pub mod log;
pub mod serve;
pub mod tls;
pub mod tools;

/// The service's own user (system range; no home, no login): uid and gid.
pub const SERVICE_ID: (u32, u32) = (961, 961);

/// Monotonic nanoseconds since the first call (the service's clock for deadlines).
pub fn now_ns() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}
