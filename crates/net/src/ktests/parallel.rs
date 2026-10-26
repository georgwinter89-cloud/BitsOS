//! NW-7: many downloads at once on the kernel's stack, every one checked against the server's hash, while a
//! small request (the way MCP is asked: a new connection, a few bytes each way) keeps being answered.

use super::{e, fetch_base, sha256, st, SEC};
use crate::{http, stack};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};

/// Downloads at once (the A2 check: 64).
const PARALLEL: usize = 64;
/// Each download's size when the server can cut the file (`/slice/N`): 64 x 4 MiB = 256 MiB in all.
const SLICE: u64 = 4 * 1024 * 1024;
/// Pause between two probe requests.
const PROBE_GAP_NS: u64 = 50_000_000;

struct Probe {
    answered: AtomicU64,
    failed: AtomicU64,
    worst_us: AtomicU64,
    sum_us: AtomicU64,
}

/// One download into a running hash: (bytes, hex digest) or why it failed.
fn one(s: &'static stack::Stack, url: &str, limit: u64) -> Result<(u64, String), String> {
    let mut h = sha256::Sha256::new();
    let mut n = 0u64;
    http::get_to(s, url, &http::Options::new(limit), &mut |d| {
        h.update(d);
        n += d.len() as u64;
        Ok(())
    })
    .map_err(|x| format!("GET {url}: {x:?}"))?;
    Ok((n, sha256::hex(&h.finish())))
}

/// `<hex> <size>` from a `.sha256` path.
fn want(s: &'static stack::Stack, url: &str) -> Result<Option<(String, u64)>, String> {
    let body = match http::get(s, url, &http::Options::new(200)) {
        Ok((_, b)) => b,
        Err(http::HttpError::Status(404)) => return Ok(None),
        Err(x) => return Err(format!("GET {url}: {x:?}")),
    };
    let t = String::from_utf8_lossy(&body).into_owned();
    let mut it = t.split_whitespace();
    let hex = it.next().ok_or("empty hash answer")?.to_ascii_lowercase();
    let size = it.next().and_then(|x| x.parse().ok()).ok_or("no size in the hash answer")?;
    Ok(Some((hex, size)))
}

fn fetch_parallel() -> TestResult {
    let s = st()?;
    let Some(base) = fetch_base(s)? else { return ktest::skip("no test server: net_test_url= in CONFIG, or 30-stack.sh under QEMU") };
    // `/slice/N` where the server has it (stack_server.py since NW-7), else the whole file.
    let (path, (hex, size)) = match want(s, &format!("{base}/slice/{SLICE}.sha256"))? {
        Some(w) => (format!("/slice/{SLICE}"), w),
        None => (String::from("/file"), want(s, &format!("{base}/file.sha256"))?.ok_or("no /file.sha256")?),
    };
    let url = Arc::new(format!("{base}{path}"));
    let no_memory = stack::NO_MEMORY.get();
    let drops = || s.interface_details().iter().fold((0u64, 0u64), |a, d| (a.0 + d.stats.rx_dropped, a.1 + d.stats.tx_dropped));
    let (drops0, rounds0, busy0, wakes0, requeued0) = (drops(), stack::POLL_ROUNDS.get(), stack::RX_BUSY_ROUNDS.get(), stack::wake::SOCK_WAKES.get(), stack::TX_REQUEUED.get());
    let results: Arc<SpinLock<Vec<Result<(u64, String), String>>>> = Arc::new(SpinLock::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let probe = Arc::new(Probe { answered: AtomicU64::new(0), failed: AtomicU64::new(0), worst_us: AtomicU64::new(0), sum_us: AtomicU64::new(0) });
    let t0 = time::now_ns();
    let mut threads = Vec::new();
    let mut spawn_err = None;
    for i in 0..PARALLEL {
        let (url, out) = (url.clone(), results.clone());
        match sched::spawn("ktest-fetch", sched::Class::Normal, move || {
            let r = one(s, &url, size);
            out.lock().push(r);
        }) {
            Ok(h) => threads.push(h),
            Err(x) => {
                spawn_err = Some(format!("download {i}: no thread: {x:?}"));
                break;
            }
        }
    }
    // The probe: `/ping` on a fresh connection, again and again, while the downloads run.
    let (p, halt, ping) = (probe.clone(), stop.clone(), format!("{base}/ping"));
    let prober = sched::spawn("ktest-probe", sched::Class::Normal, move || {
        loop {
            let t = time::now_ns();
            let ok = matches!(http::get(s, &ping, &http::Options { timeout_ns: 10 * SEC, idle_ns: 10 * SEC, ..http::Options::new(100) }), Ok((_, ref b)) if b == b"pong\n");
            let us = (time::now_ns() - t) / 1000;
            if ok {
                p.answered.fetch_add(1, Ordering::Relaxed);
                p.sum_us.fetch_add(us, Ordering::Relaxed);
                p.worst_us.fetch_max(us, Ordering::Relaxed);
            } else {
                p.failed.fetch_add(1, Ordering::Relaxed);
            }
            if halt.load(Ordering::Acquire) {
                break;
            }
            sched::sleep_ns(PROBE_GAP_NS);
        }
    })
    .map_err(e);
    // B4: bounded; at 1 MB/s (the slowest card we know of) 256 MiB would not fit, so the limit is generous.
    let end = t0 + 240 * SEC;
    let mut hung = 0;
    for h in &threads {
        let left = end.saturating_sub(time::now_ns());
        if !h.join_timeout(left.max(1)) {
            hung += 1;
        }
    }
    let ms = (time::now_ns() - t0) / 1_000_000;
    stop.store(true, Ordering::Release);
    let prober_done = match &prober {
        Ok(h) => h.join_timeout(20 * SEC),
        Err(_) => true,
    };
    sched::reap();
    let res = results.lock();
    let bytes: u64 = res.iter().filter_map(|r| r.as_ref().ok().map(|x| x.0)).sum();
    let answered = probe.answered.load(Ordering::Relaxed);
    let worst_ms = probe.worst_us.load(Ordering::Relaxed) / 1000;
    let mean_ms = probe.sum_us.load(Ordering::Relaxed) / 1000 / answered.max(1);
    klog::event!(Info, "net", "ktest.fetch_parallel", downloads = threads.len(), each = size, ms = ms, mb_per_s = bytes / 1000 / ms.max(1),
        probes = answered, probe_failed = probe.failed.load(Ordering::Relaxed), probe_mean_ms = mean_ms, probe_worst_ms = worst_ms,
        rounds = stack::POLL_ROUNDS.get() - rounds0, busy_rounds = stack::RX_BUSY_ROUNDS.get() - busy0,
        rx_dropped = drops().0 - drops0.0, tx_dropped = drops().1 - drops0.1, tx_requeued = stack::TX_REQUEUED.get() - requeued0,
        sock_wakes = stack::wake::SOCK_WAKES.get() - wakes0);
    if let Some(x) = spawn_err {
        return Err(x);
    }
    prober?;
    kassert!(hung == 0 && prober_done, "{hung} downloads (or the probe) still running after 240 s");
    kassert!(res.len() == PARALLEL, "{} results of {PARALLEL}", res.len());
    for (i, r) in res.iter().enumerate() {
        let (n, got) = r.as_ref().map_err(|x| format!("download {i}: {x}"))?;
        kassert!(*n == size && *got == hex, "download {i}: {n} bytes sha256 {got}, want {size} {hex}");
    }
    kassert!(answered >= 1 && probe.failed.load(Ordering::Relaxed) == 0, "the probe was not answered every time ({answered} ok, {} failed)", probe.failed.load(Ordering::Relaxed));
    kassert!(stack::NO_MEMORY.get() == no_memory, "real socket buffer refusals during the downloads: {}", stack::NO_MEMORY.get() - no_memory);
    Ok(())
}
ktest!("nw.stack.fetch_parallel", Safe, fetch_parallel);
