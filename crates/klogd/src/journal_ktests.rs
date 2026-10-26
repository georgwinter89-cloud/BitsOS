//! In-kernel tests of RB-8: the node `/dev/bitsos/events` (whole lines, own position per open, the three
//! sources, ack and drain, heartbeat, poll, lseek) and the reach-back of MCP `events` without a journal. All Safe
//! and rerunnable: every wait is bounded, and the tests that ack run only while no real bitsjournal is attached
//! (an ack of theirs would tell a Reload that records are on /data which the service has not written).

use crate::devevents;
use crate::tool;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use vfs::File;

/// Reads non-blocking until `want` matches a line or `reads` reads gave nothing new.
fn read_until(r: &dyn File, mut want: impl FnMut(&str) -> bool, reads: usize) -> Result<(Vec<String>, Option<String>), String> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut seen = Vec::new();
    let mut idle = 0;
    while idle < reads {
        match r.read_stream(&mut buf, true) {
            Ok(n) => {
                idle = 0;
                let text = core::str::from_utf8(&buf[..n]).map_err(|_| String::from("not UTF-8"))?;
                kassert!(text.ends_with('\n'), "a read ends with a whole line: {:?}", &text[text.len().saturating_sub(80)..]);
                for l in text.lines() {
                    if want(l) {
                        return Ok((seen, Some(String::from(l))));
                    }
                    if seen.len() < 4 {
                        seen.push(String::from(l));
                    }
                }
            }
            Err(uapi::errno::EAGAIN) => {
                idle += 1;
                sched::sleep_ns(5_000_000);
            }
            Err(e) => return Err(alloc::format!("read: errno {e}")),
        }
    }
    Ok((seen, None))
}

fn node_is_there() -> TestResult {
    let d = vfs::root_ns().lookup("/dev/bitsos/events").map_err(|e| alloc::format!("lookup: errno {e}"))?;
    let _ = d;
    kassert!(devevents::boot_id() != 0);
    Ok(())
}
ktest!("rb.journal.node", Safe, node_is_there);

fn records_with_wall_time_per_open() -> TestResult {
    let a = devevents::open_for_test();
    let s = klog::next_seq();
    // The reader starts at the journal start, so an earlier run's marker is read first: each run tags its marker
    // with its own start sequence number and counts only that one (desk-minipc-w13 FAIL 2).
    let run = alloc::format!("run={s}");
    klog::ktests::with_trace("rbjournal", || {
        klog::event!(Info, "rbjournal", "t.marker", n = 7u32, run = s, peer = "10.0.2.2:443"; "two\nlines")
    })?;
    let mine = |l: &str| l.starts_with("E ") && l.contains(" rbjournal t.marker ") && l.split(' ').any(|w| w == run);
    let (head, line) = read_until(&*a, mine, 20)?;
    kassert!(head.first().is_some_and(|h| h.starts_with(&alloc::format!("B {:016x} ", devevents::boot_id()))), "{:?}", head);
    let line = line.ok_or("marker not read")?;
    let f: Vec<&str> = line.splitn(10, ' ').collect();
    kassert!(f.len() == 10 && f[1].parse::<u64>().is_ok_and(|q| q >= s) && f[4] == "I", "{}", line);
    kassert!(f[3] == "-" || f[3].parse::<u64>().is_ok_and(|w| w > 1_000_000_000_000), "wall ms: {}", line);
    kassert!(f[9].contains("n=7") && f[9].contains("peer=10.0.2.2:443") && f[9].contains("two\\\\nlines"), "{}", line);
    // A second open has its own position: it reads the same record again.
    let b = devevents::open_for_test();
    let (_, again) = read_until(&*b, |l| l == line, 20)?;
    kassert!(again.is_some(), "second reader did not see {}", line);
    Ok(())
}
ktest!("rb.journal.records", Safe, records_with_wall_time_per_open);

/// RB-11: the same test twice in one boot (the Mini-PC runs Safe tests several times per boot).
fn records_twice() -> TestResult {
    records_with_wall_time_per_open()?;
    records_with_wall_time_per_open()
}
ktest!("rb.journal.records_twice", Safe, records_twice);

fn whole_lines_or_einval() -> TestResult {
    let r = devevents::open_for_test();
    let mut tiny = [0u8; 8];
    kassert!(r.read_stream(&mut tiny, true) == Err(uapi::errno::EINVAL), "the head line does not fit 8 bytes");
    let mut buf = vec![0u8; 4096];
    let n = r.read_stream(&mut buf, true).map_err(|e| alloc::format!("errno {e}"))?;
    kassert!(n > 0 && buf[..n].starts_with(b"B ") && buf[n - 1] == b'\n');
    kassert!(r.write_at(0, b"X 1\n") == Err(uapi::errno::EINVAL));
    kassert!(r.write_at(0, b"A x\n") == Err(uapi::errno::EINVAL));
    Ok(())
}
ktest!("rb.journal.whole_lines", Safe, whole_lines_or_einval);

fn trace_and_metrics_arrive() -> TestResult {
    let r = devevents::open_for_test();
    let rings = klog::trace::rings().next().is_some();
    let rounds = crate::metrics::with_store(1_000_000, |s| s.rounds().end > s.rounds().start) == Some(true);
    klog::trace::mark("rbjournal", "t.mark", 11, 22);
    // One pass: a read hands out log, trace and metric lines together.
    let (mut t, mut m) = (None, None);
    let _ = read_until(&*r, |l| {
        if l.starts_with("T ") && l.contains(" rbjournal t.mark ") {
            t = Some(String::from(l));
        }
        if l.starts_with("M ") && m.is_none() {
            m = Some(String::from(l));
        }
        (t.is_some() || !rings) && (m.is_some() || !rounds)
    }, 20)?;
    kassert!(t.is_some() || !rings, "trace mark not read");
    if !rounds {
        return ktest::skip("no metric rounds (metrics=off or the sampler did not run yet)");
    }
    let m = m.ok_or("no metric round read")?;
    kassert!(m.split(' ').nth(4).is_some_and(|kv| kv.contains('=')), "{}", m);
    Ok(())
}
ktest!("rb.journal.trace_metrics", Safe, trace_and_metrics_arrive);

fn ack_and_drain() -> TestResult {
    // With the real bitsjournal attached (the Mini-PC), acking here would tell a Reload that records are on /data
    // which the service has not written: the ack part runs only without it.
    if klog::journal::attached() != 0 {
        return ktest::skip("bitsjournal is attached: a test ack would claim records durable that it has not written");
    }
    let r = devevents::open_for_test();
    // Drain the backlog so the flush request is the next line.
    let _ = read_until(&*r, |_| false, 3)?;
    let start = klog::journal::acked();
    // A write is checked whole: a bad line applies none of its acks, and a reader that never acked stays detached.
    let top = klog::next_seq();
    kassert!(r.write_at(0, alloc::format!("A {top}\nA x\n").as_bytes()) == Err(uapi::errno::EINVAL));
    kassert!(r.write_at(0, b"A\n") == Err(uapi::errno::EINVAL) && r.write_at(0, b"A 1 2\n") == Err(uapi::errno::EINVAL));
    kassert!(klog::journal::attached() == 0 && klog::journal::acked() == start, "nothing of a bad write applied");
    r.write_at(0, alloc::format!("A {start}\n").as_bytes()).map_err(|e| alloc::format!("ack: errno {e}"))?;
    kassert!(klog::journal::attached() >= 1);
    // An ack from the future is clamped to the newest record.
    r.write_at(0, b"A 99999999999\n").map_err(|e| alloc::format!("ack: errno {e}"))?;
    kassert!(klog::journal::acked() <= klog::next_seq(), "acked {} next {}", klog::journal::acked(), klog::next_seq());
    // A record after the ack: the drain below has something to wait for.
    klog::ktests::with_trace("rbjournal", || klog::event!(Info, "rbjournal", "t.drain"))?;
    let saw = core::cell::Cell::new(false);
    let d = klog::journal::wait_drained("ktest", klog::journal::DRAIN_NS, &|_| {
        if let Ok((_, Some(f))) = read_until(&*r, |l| l.starts_with("F "), 2)
            && let Some(t) = f[2..].parse::<u64>().ok()
        {
            saw.set(true);
            let _ = r.write_at(0, alloc::format!("A {t}\n").as_bytes());
        }
    });
    kassert!(matches!(d, klog::journal::Drain::Done { .. }), "{:?}", d);
    kassert!(saw.get(), "no flush request line");
    // Nobody acks: the wait ends at its bound.
    let t = klog::journal::wait_drained("ktest", 30_000_000, &sched::sleep_ns);
    kassert!(matches!(t, klog::journal::Drain::Timeout { .. }), "{:?}", t);
    drop(r);
    kassert!(klog::journal::attached() == 0, "closing the reader detaches it");
    kassert!(klog::journal::wait_drained("ktest", 1_000_000_000, &|_| panic!("no wait")) == klog::journal::Drain::NoJournal);
    Ok(())
}
ktest!("rb.journal.drain", Safe, ack_and_drain);

/// The heartbeat names the reader's own position, poll says readable only with something new, lseek as
/// devkmsg_llseek, a second blocking read of the same file waits instead of EAGAIN.
fn heartbeat_poll_seek() -> TestResult {
    let r = devevents::open_for_test();
    let _ = read_until(&*r, |_| false, 3)?;
    kassert!(r.rdev() == vfs::abi::encode_dev(10, 240) && r.ioctl(1, 0) == Err(uapi::errno::ENOTTY));
    kassert!(r.lseek(5, vfs::abi::SEEK_SET) == Some(Err(uapi::errno::ESPIPE)));
    kassert!(r.lseek(0, vfs::abi::SEEK_CUR) == Some(Err(uapi::errno::EINVAL)));
    // A blocking read with nothing new ends with the heartbeat (other CPUs may log meanwhile: then their lines).
    let mut buf = vec![0u8; 64 * 1024];
    let before = klog::next_seq();
    let n = r.read_stream(&mut buf, false).map_err(|e| alloc::format!("errno {e}"))?;
    let text = core::str::from_utf8(&buf[..n]).map_err(|_| String::from("not UTF-8"))?;
    if let Some(i) = text.lines().find(|l| l.starts_with("I ")) {
        let pos = i.split(' ').nth(1).and_then(|p| p.parse::<u64>().ok()).ok_or("I line")?;
        kassert!(pos >= before && pos <= klog::next_seq(), "heartbeat at {} (next before {})", pos, before);
    }
    // SEEK_END: only what comes later; poll is readable once a record is logged.
    kassert!(r.lseek(0, vfs::abi::SEEK_END) == Some(Ok(0)));
    klog::ktests::with_trace("rbjournal", || klog::event!(Info, "rbjournal", "t.poll"))?;
    kassert!(r.events().0 & vfs::Events::IN != 0, "a new record makes it readable");
    kassert!(r.events().0 & vfs::Events::OUT == 0, "no POLLOUT, as devkmsg_poll");
    let (_, got) = read_until(&*r, |l| l.contains(" rbjournal t.poll"), 20)?;
    kassert!(got.is_some(), "the record after SEEK_END was read");
    // SEEK_SET: from the oldest again, with a new head line.
    kassert!(r.lseek(0, vfs::abi::SEEK_SET) == Some(Ok(0)));
    let (_, head) = read_until(&*r, |l| l.starts_with("B "), 3)?;
    kassert!(head.is_some(), "SEEK_SET starts with a head line");
    Ok(())
}
ktest!("rb.journal.poll_seek", Safe, heartbeat_poll_seek);

/// CONTRACTS §4: what one event costs on the node's read path (rendering, copying), logged as an event.
fn cost_per_event() -> TestResult {
    const N: u32 = 400;
    let r = devevents::open_for_test();
    let _ = read_until(&*r, |_| false, 3)?;
    klog::ktests::with_trace("rbjournal", || {
        for i in 0..N {
            klog::event!(Info, "rbjournal", "t.cost", i = i, peer = "192.168.100.20:51234", bytes = 123_456u64);
        }
    })?;
    let (_, lines0, bytes0) = devevents::stats();
    let t0 = time::now_ns();
    let mut got = 0u32;
    let _ = read_until(&*r, |l| {
        if l.contains(" rbjournal t.cost ") {
            got += 1;
        }
        got >= N
    }, 20)?;
    let ns = time::now_ns().saturating_sub(t0);
    let (_, lines1, bytes1) = devevents::stats();
    kassert!(got >= N, "read {} of {} cost events", got, N);
    let lines = (lines1 - lines0).max(1);
    klog::event!(Info, "klog", "journal.cost", events = got, ns_per_line = ns / lines, bytes_per_line = (bytes1 - bytes0) / lines);
    Ok(())
}
ktest!("rb.journal.cost", Safe, cost_per_event);

fn reach_back_without_journal() -> TestResult {
    if crate::journal::present() {
        return ktest::skip("bitsjournal runs: the ring fallback is not used");
    }
    let Some(now) = time::wall::realtime_ns() else { return ktest::skip("no wall clock") };
    klog::ktests::with_trace("rbjournal", || klog::event!(Info, "rbjournal", "t.back"))?;
    let since = now / 1_000_000_000 - 30;
    let a = tool::call_tool("events", &alloc::format!(r#"{{"since":"{since}","subsystem":"rbjournal","name":"t.back"}}"#)).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(a.starts_with("journal not running") && a.contains("rbjournal t.back"), "{}", a);
    let m = tool::call_tool("metrics", &alloc::format!(r#"{{"since":{since},"name":"cpu.all"}}"#)).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(m.starts_with("journal not running"), "{}", m);
    kassert!(tool::call_tool("events", r#"{"since":"25:99"}"#) == Err(mcp::ToolError::BadArgument(String::from("since"))));
    Ok(())
}
ktest!("rb.journal.reach_back", Safe, reach_back_without_journal);
