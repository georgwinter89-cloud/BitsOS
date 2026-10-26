//! PM-2 ktests of the tool and the console command. Safe and rerunnable: a new pid above any real one per run,
//! only its own connections are looked at (pid filter).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{kassert, ktest, TestResult};
use policy::{IpAddr, NetProto, ProgramId, Subject};

static RUN: AtomicU32 = AtomicU32::new(0);

pub(crate) fn fresh() -> Subject {
    Subject { program: ProgramId(0x504D_0040), pid: 0x7e00_0000 + RUN.fetch_add(1, Ordering::Relaxed), uid: 1000 }
}

/// A DNS answer for `name` with one A record (RFC 1035 §4.1, the answer's name compressed to the question).
pub(crate) fn answer(name: &str, a: [u8; 4]) -> Vec<u8> {
    let mut m = alloc::vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
    for l in name.split('.') {
        m.push(l.len() as u8);
        m.extend_from_slice(l.as_bytes());
    }
    m.extend_from_slice(&[0, 0, 1, 0, 1, 0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
    m.extend_from_slice(&a);
    m
}

fn tool_lists_a_programs_connections() -> TestResult {
    let before = policy::dns_names();
    let s = fresh();
    policy::note_dns_answer(&s, &answer("www.youtube.com", [192, 0, 2, 10]));
    let h = policy::open_conn(s, NetProto::Tcp, false, 40001, IpAddr::V4([192, 0, 2, 10]), 443);
    h.add_in(1000);
    h.add_out(100);
    drop(policy::open_conn(s, NetProto::Udp, false, 40002, IpAddr::V4([192, 0, 2, 53]), 53));
    let all = crate::call(&alloc::format!(r#"{{"pid":{}}}"#, s.pid)).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(all.contains(&alloc::format!("== pid={} (ended) program={}: 2 connections, in 1.0 kB, out 100 B", s.pid, s.program.0)), "{all}");
    kassert!(all.contains("\nlive  tcp 192.0.2.10:443 name=www.youtube.com in=1000 out=100 "), "{all}");
    kassert!(all.contains("\nended udp 192.0.2.53:53 name=- in=0 out=0 "), "{all}");
    let live = crate::call(&alloc::format!(r#"{{"pid":{},"live":true}}"#, s.pid)).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(live.contains("1 connections") && !live.contains("ended udp"), "{live}");
    let f = crate::call(&alloc::format!(r#"{{"pid":{},"filter":"YouTube"}}"#, s.pid)).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(f.contains("1 connections") && f.contains("name=www.youtube.com"), "{f}");
    for bad in [r#"{"max":0}"#, r#"{"max":"x"}"#, r#"{"live":1}"#, r#"{"pid":-1}"#, r#"{"pid":4294967296}"#, r#"{"who":"me"}"#, r#"{"filter":"a\u0001"}"#, "[1]"] {
        kassert!(matches!(crate::call(bad), Err(mcp::ToolError::BadArgument(_))), "{bad} accepted");
    }
    drop(h);
    policy::forget_test_connections(&s);
    names_not_taken(before)
}
ktest!("pm.conn.tool", Safe, tool_lists_a_programs_connections);

fn console_command() -> TestResult {
    let before = policy::dns_names();
    let s = fresh();
    policy::note_dns_answer(&s, &answer("search.brave.com", [192, 0, 2, 11]));
    let h = policy::open_conn(s, NetProto::Tcp, false, 40003, IpAddr::V4([192, 0, 2, 11]), 443);
    let cmd = console::monitor::COMMANDS.entries().iter().find(|c| c.name == "connections").ok_or("no console command")?;
    let mut ctx = console::monitor::Ctx { args: alloc::vec!["live", "search.brave"], injected: false, out: String::new(), echo: false };
    (cmd.run)(&mut ctx)?;
    kassert!(ctx.out.contains("name=search.brave.com"), "{}", ctx.out);
    let mut bad = console::monitor::Ctx { args: alloc::vec!["a", "b"], injected: false, out: String::new(), echo: false };
    kassert!((cmd.run)(&mut bad).is_err(), "two filters accepted");
    drop(h);
    policy::forget_test_connections(&s);
    names_not_taken(before)
}
ktest!("pm.conn.console", Safe, console_command);

/// PM-2b: a program that execs /proc/self/exe (as Chromium's helpers do) is named by its executable, with its
/// comm `exe` shown and matched too. BusyBox's ash execs itself under another applet name (`exec -a`).
fn self_exec_named_by_its_executable() -> TestResult {
    const MS: u64 = 1_000_000;
    let before = policy::dns_names();
    let argv = ["sh", "-c", "exec -a sleep /proc/self/exe 30"];
    let env = ["PATH=/bin"];
    let p = match proc::spawn_with("/bin/sh", &proc::SpawnOptions::new(&argv, &env, ProgramId(1))) {
        Ok(p) => p,
        Err(proc::SpawnError::NotFound) => return ktest::skip("no /bin/sh (BusyBox) in this image"),
        Err(e) => return Err(alloc::format!("spawn /bin/sh: {e}")),
    };
    let end = time::now_ns() + 10_000 * MS;
    let img = loop {
        if let Some(q) = proc::find(p.pid).filter(|q| q.name == "exe") {
            break Some(q);
        }
        if time::now_ns() >= end || p.wait(Some(20 * MS)).is_some() {
            break None;
        }
    };
    let s = Subject { program: p.program, pid: p.pid, uid: p.uid };
    // A name another process of the same program (ProgramId 1) resolved: the forget below must keep it. Before
    // PM-2c it took every name of the program, so this fails then even when the table starts empty.
    let other = Subject { program: p.program, ..fresh() };
    policy::note_dns_answer(&other, &answer("kept.example", [192, 0, 2, 15]));
    let r = img.ok_or_else(|| String::from("the shell never ran /proc/self/exe")).and_then(|q| {
        let exe = crate::program_name(&q.path, &q.name);
        kassert!(exe != "exe" && !q.path.starts_with("/proc/"), "Process.path not resolved: {}", q.path);
        let h = policy::open_conn(s, NetProto::Tcp, false, 40004, IpAddr::V4([192, 0, 2, 12]), 443);
        let all = crate::call(&alloc::format!(r#"{{"pid":{}}}"#, s.pid)).map_err(|e| alloc::format!("{e:?}"))?;
        kassert!(all.contains(&alloc::format!("\n== {exe} pid={} comm=exe program=", s.pid)), "{all}");
        for f in [exe, "EXE"] {
            let got = crate::call(&alloc::format!(r#"{{"pid":{},"filter":"{f}"}}"#, s.pid)).map_err(|e| alloc::format!("{e:?}"))?;
            kassert!(got.contains(": 1 connections"), "filter {f}: {got}");
        }
        drop(h);
        Ok(())
    });
    p.kill(9); // SIGKILL
    let _ = p.wait(Some(10_000 * MS));
    // Runs as ProgramId 1 (Brave's on the Mini-PC): the forget takes only this pid's names, not Brave's (PM-2c).
    policy::forget_test_connections(&s);
    let kept = other_name_kept(&other);
    policy::forget_test_connections(&other);
    r.and_then(|()| kept).and_then(|()| names_not_taken(before))
}

/// `other`'s connection to 192.0.2.15 is named `kept.example` (its program's name for the address survived).
fn other_name_kept(other: &Subject) -> TestResult {
    let h = policy::open_conn(*other, NetProto::Tcp, false, 40007, IpAddr::V4([192, 0, 2, 15]), 443);
    let all = crate::call(&alloc::format!(r#"{{"pid":{}}}"#, other.pid)).map_err(|e| alloc::format!("{e:?}"));
    drop(h);
    let all = all?;
    kassert!(all.contains("kept.example"), "the forget of the test's pid took another pid's name: {all}");
    Ok(())
}
ktest!("pm.conn.self_exec", Safe, self_exec_named_by_its_executable);

/// The name table is not smaller than `before` (`policy`'s pm.conn.forget_own has the reasoning; skipped near
/// `dns_keep`, where a test's own names may push real ones out).
fn names_not_taken(before: usize) -> TestResult {
    let after = policy::dns_names();
    kassert!(after >= before || before + 16 > policy::dns_keep(), "the name table shrank from {before} to {after}");
    Ok(())
}

/// PM-2c (w12 Mini-PC step 5): a connection is named when it opens, so it keeps the name when its process ends
/// before it closes and no query came in between: the `conn.closed` event has comm and exe, and the view of the
/// ended program shows its name, not `pid=N (ended)`.
fn named_after_the_owner_ended() -> TestResult {
    // The trailing `:` keeps ash from exec'ing sleep in its place: the process stays the shell.
    let argv = ["sh", "-c", "sleep 30; :"];
    let env = ["PATH=/bin"];
    let p = match proc::spawn_with("/bin/sh", &proc::SpawnOptions::new(&argv, &env, ProgramId(0x504D_0041))) {
        Ok(p) => p,
        Err(proc::SpawnError::NotFound) => return ktest::skip("no /bin/sh (BusyBox) in this image"),
        Err(e) => return Err(alloc::format!("spawn /bin/sh: {e}")),
    };
    let s = Subject { program: p.program, pid: p.pid, uid: p.uid };
    let r = (|| {
        let q = proc::find(p.pid).ok_or("the shell is not in the table")?;
        let (exe, comm) = (String::from(crate::program_name(&q.path, &q.name)), q.name.clone());
        drop(q);
        let h = policy::open_conn(s, NetProto::Tcp, false, 40006, IpAddr::V4([192, 0, 2, 14]), 443);
        p.kill(9); // SIGKILL
        reap(&p);
        kassert!(proc::find(p.pid).is_none(), "pid {} still in the table after SIGKILL", p.pid);
        // Same name and comm when the shell had not exec'd anything else in between (then the stored ones win).
        let since = klog::next_seq();
        klog::ktests::with_trace("pm", || drop(h))?;
        let page = klog::read(since, 4096, &klog::Filter { level: None, subsystem: Some("pm"), name: Some("conn.closed") });
        let pid_field = alloc::format!(" pid={} ", s.pid);
        let ev = page.records.iter().find(|r| alloc::format!(" {} ", r.text).contains(&pid_field)).ok_or("no conn.closed for the pid")?;
        let want_comm = alloc::format!("comm={comm}");
        kassert!(ev.text.contains(&alloc::format!("exe={exe}")) && ev.text.contains(&want_comm), "{}", ev.text);
        let all = crate::call(&alloc::format!(r#"{{"pid":{}}}"#, s.pid)).map_err(|e| alloc::format!("{e:?}"))?;
        let head = if comm == exe { alloc::format!("\n== {exe} pid={} program=", s.pid) } else { alloc::format!("\n== {exe} pid={} comm={comm} program=", s.pid) };
        kassert!(all.contains(&head) && !all.contains("(ended)"), "{all}");
        Ok(())
    })();
    // Every path: no shell left over for a rerun in the same boot, no test connection or name either.
    p.kill(9);
    reap(&p);
    policy::forget_test_connections(&s);
    r
}

/// Waits (at most 10 s) until `p` is gone from the table (reaped).
fn reap(p: &proc::Process) {
    const MS: u64 = 1_000_000;
    let end = time::now_ns() + 10_000 * MS;
    while proc::find(p.pid).is_some() && time::now_ns() < end {
        let _ = p.wait(Some(20 * MS));
    }
}
ktest!("pm.conn.ended_named", Safe, named_after_the_owner_ended);

#[cfg(test)]
mod tests {
    #[test]
    fn tool_and_console_on_the_host() {
        super::tool_lists_a_programs_connections().unwrap();
        super::console_command().unwrap();
    }

    #[test]
    fn program_name_is_the_executables() {
        assert_eq!(crate::program_name("/opt/brave.com/brave/brave", "exe"), "brave");
        assert_eq!(crate::program_name("/bin/busybox", "sh"), "busybox");
        assert_eq!(crate::program_name("", "init"), "init");
        assert_eq!(crate::program_name("/odd/", "odd"), "odd");
    }

    #[test]
    fn header_shows_a_differing_comm() {
        let s = policy::Subject { program: policy::ProgramId(7), pid: 0x7e7f_0001, uid: 1000 };
        let h = policy::open_conn(s, policy::NetProto::Tcp, false, 40005, policy::IpAddr::V4([192, 0, 2, 13]), 443);
        let mut v = policy::connections(&policy::ConnFilter { pid: Some(s.pid), ..Default::default() }, usize::MAX);
        let clock = crate::Clock { mono_ns: 0, wall_ns: None, offset_min: 0 };
        v[0].set_program("brave", "exe");
        assert!(crate::render(&v, &clock, (1, 0), 0).contains("\n== brave pid=2122252289 comm=exe program=7: 1 connections"));
        v[0].set_program("brave", "brave");
        assert!(crate::render(&v, &clock, (1, 0), 0).contains("\n== brave pid=2122252289 program=7: 1 connections"));
        drop(h);
        policy::forget_test_connections(&s);
    }

    #[test]
    fn clock_text() {
        let c = crate::Clock { mono_ns: 100_000_000_000, wall_ns: Some(3_600_000_000_000 + 10_000_000_000), offset_min: 120 };
        // 90 s before now: 01:00:10 UTC - 90 s = 00:58:40 UTC, 02:58:40 local.
        assert_eq!(c.at(10_000_000_000), "02:58:40");
        let none = crate::Clock { wall_ns: None, ..c };
        assert_eq!(none.at(12_500_000_000), "t+12s");
        assert_eq!(crate::bytes(999), "999 B");
        assert_eq!(crate::bytes(12_345), "12.3 kB");
        assert_eq!(crate::bytes(4_200_000_000), "4.2 GB");
    }
}
