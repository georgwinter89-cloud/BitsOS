//! `fetch`: the kernel pulls a file from an HTTP server on the LAN into the RAM file system or (MC-push) onto
//! the boot stick below `\BITSOS2\` (`spec/net.md` §5.2: big binaries, so MCP answers stay small). R9: only `http://` to a private IPv4 address the caller names (the
//! desk's or Georg's server); no DNS, no TLS, no redirects in the kernel, so nothing else is ever contacted.

use super::{Fnv, Place, path_arg, perm_arg, place, policy_path, refuse};
use crate::tools::{number, opt_str};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use net::http::{self, HttpError};
use net::{KSocket, SockAddr};
use uapi::errno::*;
use vfs::MountNs;

const SEC: u64 = 1_000_000_000;
/// The whole transfer at most (a bound against a server that trickles; 100 MB take seconds on the LAN).
const TOTAL_NS: u64 = 30 * 60 * SEC;
/// The longest pause between two pieces of the answer.
const IDLE_NS: u64 = 20 * SEC;
/// How often the call's progress text grows (`output` shows it once the caller got "Still running").
const REPORT_EVERY: u64 = 8 << 20;

/// Where `url` may be fetched from: `http://` + a private IPv4 (10/8, 172.16/12, 192.168/16, 127/8,
/// 169.254/16 as Reload allows). Err is the refusal text (pure, host-tested).
pub fn check_url(url: &str) -> Result<SockAddr, String> {
    match http::parse_url(url) {
        Ok(u) if u.addr.ip.is_private() => Ok(u.addr),
        Ok(u) => {
            let o = u.addr.ip.0;
            Err(format!("{}.{}.{}.{} is not on a private network: fetch reaches only the LAN (R9: BitsOS never phones home)", o[0], o[1], o[2], o[3]))
        }
        Err(HttpError::Https) => Err(String::from("https:// is not supported in the kernel (no TLS): serve the file over http:// on the LAN")),
        Err(HttpError::NeedsAddress) => Err(String::from("a host name needs DNS, which the kernel does not have: give the IPv4 address")),
        Err(HttpError::BadUrl(w)) => Err(format!("bad URL ({w}): want http://<LAN IPv4>[:port]/path")),
        Err(e) => Err(format!("bad URL: {e:?}")),
    }
}

/// Streams `url` (checked again with `check_url`) into the place `pl` (hidden sibling, renamed over the name at
/// the end) through `sock` (the kernel's stack, or host sockets in tests). `max` bounds the body (None: the file
/// system's own room decides).
pub fn fetch_into(ctx: &ToolContext, sock: &dyn KSocket, url: &str, ns: &MountNs, pl: &Place, perm: u32, max: Option<u64>) -> ToolResult {
    check_url(url).map_err(|why| refuse("fetch", url, why))?;
    let mut opts = http::Options::new(max.unwrap_or(u64::MAX));
    opts.timeout_ns = TOTAL_NS;
    opts.idle_ns = IDLE_NS;
    // PM-2 CR3: the net stack records the connection itself; it is the caller's, not the kernel's.
    opts.owner = ctx.caller;
    let start = ctx.now_ns();
    klog::event!(Info, "mcp", "fetch.start", url = url, path = pl.canon.as_str());
    let mut fnv = Fnv::new();
    let mut got = 0u64;
    let mut next_report = REPORT_EVERY;
    let mut errno: Option<i64> = None;
    let mut stopped = false;
    let length: core::cell::Cell<Option<u64>> = core::cell::Cell::new(None);
    let r = super::write::replace_with(ns, pl, perm, &mut |f| {
        let result = http::get_to_with(
            sock,
            url,
            &opts,
            &mut |h| {
                length.set(h.content_length);
                Ok(())
            },
            &mut |d| {
                if ctx.stop_requested() {
                    stopped = true;
                    return Err(());
                }
                super::write_all_at(f, got, d).map_err(|e| errno = Some(e))?;
                fnv.update(d);
                got += d.len() as u64;
                if got >= next_report {
                    next_report += REPORT_EVERY;
                    let of = length.get().map(|l| format!(" of {} MiB", l >> 20)).unwrap_or_default();
                    ctx.output(&format!("fetch: {} MiB{of} so far\n", got >> 20));
                }
                Ok(())
            },
        );
        match &result {
            Ok(_) => Ok(()),
            Err(e) => Err(ToolError::Failed(why(e, url, got, errno, stopped))),
        }
    });
    let ms = ctx.now_ns().saturating_sub(start) / 1_000_000;
    if let Err(e) = r {
        let text = match &e {
            ToolError::Failed(t) | ToolError::Refused(t) | ToolError::BadArgument(t) => t.clone(),
        };
        klog::event!(Warn, "mcp", "fetch.failed", url = url, path = pl.canon.as_str(), bytes = got, ms = ms, reason = text.as_str());
        return Err(e);
    }
    let rate = (got / 1000).checked_div(ms).unwrap_or(0);
    klog::event!(Info, "mcp", "fetch.done", url = url, path = pl.canon.as_str(), bytes = got, ms = ms, fnv = klog::Hex(fnv.0));
    Ok(vec![ToolOutput::Text(format!("fetched {url} -> {}: {got} bytes in {ms} ms ({rate} MB/s), fnv1a64 {:#018x}, mode {perm:04o}", pl.canon, fnv.0))])
}

fn why(e: &HttpError, url: &str, got: u64, errno: Option<i64>, stopped: bool) -> String {
    match e {
        _ if stopped => format!("stopped after {got} bytes; nothing was kept"),
        HttpError::Sink => match errno {
            Some(ENOSPC) => format!("the file system is full after {got} bytes (ENOSPC); nothing was kept"),
            Some(e) => format!("writing failed after {got} bytes: {} (errno {e}); nothing was kept", super::errno_name(e)),
            None => format!("writing failed after {got} bytes; nothing was kept"),
        },
        HttpError::Status(s) => format!("{url} answered HTTP {s}"),
        HttpError::TooLarge { limit } => format!("{url} sends more than max_bytes {limit}"),
        HttpError::TimedOut => format!("{url}: no answer in time (pause > 20 s or transfer > 30 min) after {got} bytes"),
        HttpError::Connect(n) => format!("cannot connect to {url}: {n:?}"),
        HttpError::NoMemory => String::from("no memory for the transfer's buffers"),
        e => format!("{url}: {e:?} after {got} bytes"),
    }
}

fn fetch(ctx: &ToolContext, args: &Json) -> ToolResult {
    let url = opt_str(args, "url")?.ok_or_else(|| crate::tools::bad("url"))?;
    let path = path_arg(args, "path")?;
    let max = number(args, "max_bytes")?;
    let perm = perm_arg(args, "mode")?;
    let parents = crate::tools::opt_bool(args, "parents")?.unwrap_or(false);
    let to = check_url(url).map_err(|why| refuse("fetch", url, why))?;
    // Every refusal before anything changes: the connection, a CONFIG.TXT by name, then (in make_parents) each
    // directory it makes, then the file itself.
    policy::check(&ctx.caller, &policy::Access::NetConnect { proto: policy::NetProto::Tcp, addr: policy::IpAddr::V4(to.ip.0), port: to.port })
        .map_err(|d| refuse("fetch", url, format!("policy: {}", d.reason)))?;
    let config = |name: &[u8]| name.eq_ignore_ascii_case(b"CONFIG.TXT");
    if config(path.trim_end_matches('/').rsplit('/').next().unwrap_or("").as_bytes()) {
        return Err(refuse("fetch", path, String::from("fetch never writes a CONFIG.TXT (R8a: use write_file, which checks the guarded lines)")));
    }
    let ns = vfs::root_ns();
    if parents {
        super::make_parents(ctx, &ns, "fetch", path)?;
    }
    let pl = place(&ns, "fetch", path, false)?;
    if config(&pl.name) {
        return Err(refuse("fetch", path, String::from("fetch never writes a CONFIG.TXT (R8a: use write_file, which checks the guarded lines)")));
    }
    policy_path(ctx, "fetch", &pl.canon, true, pl.existing.is_none())?;
    let sock = net::stack().ok_or_else(|| ToolError::Failed(String::from("no network stack")))?;
    let perm = perm.or(pl.existing.as_ref().map(|p| p.dentry.inode.meta.mode() & 0o7777)).unwrap_or(0o644);
    fetch_into(ctx, sock, url, &ns, &pl, perm, max)
}

mcp_tool!(
    name: "fetch",
    owner: "MC",
    safety: Change,
    description: "The running BitsOS downloads url (http://<LAN IPv4>[:port]/path only: no DNS, no https, never a public address, R9) into path (the places write_file may write: tmpfs, and the boot stick below /boot/BITSOS2/ without SYS, CONFIG.TXT, STICK.ID). Written beside the target and renamed over it at the end, so a failed or stopped download leaves nothing. Answers bytes, time and the FNV-1a 64 of the content. Default deadline 120 s; a longer download answers 'Still running (call n)', output shows the progress, stop ends it. max_bytes bounds the size; mode (octal text) for the new file; parents:true makes missing directories.",
    schema: r#"{"type":"object","properties":{"url":{"type":"string","description":"http://<private IPv4>[:port]/path"},"path":{"type":"string","description":"absolute target path, e.g. /tmp/big.bin"},"max_bytes":{"type":["integer","string"],"description":"refuse a larger body"},"mode":{"type":"string","description":"permission bits as octal text, e.g. \"0755\""},"parents":{"type":"boolean","description":"make missing parent directories"},"timeout_ms":{"type":"integer","default":120000}},"required":["url","path"]}"#,
    handler: fetch,
);
