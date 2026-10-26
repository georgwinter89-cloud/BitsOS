//! The `sched` MCP tool (owner SC, Safety read-only): what every CPU's scheduler does (online, idle or halted,
//! queued threads, busy/idle time since boot, switches, migrations, steals, pushes, reschedule IPIs,
//! interrupt-time wake-ups, topology) and, with `threads`, every thread (class, state, CPU, pin, KernelRt
//! priority, CPU time, migrations, wake-ups; SC-6: user and system ms, nice, policy, affinity narrowed or not,
//! voluntary and involuntary switches). SC-4 adds per CPU the heartbeat, whether the tick is armed (only
//! with more than one runnable thread) and the last idle entry (ms since boot, IF). For the Mini-PC check lists
//! (SC-3, SC-4) and "why is it slow".

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use sched::{CpuStat, ThreadStat};

/// Bound on one answer's thread lines (a request limit, not a count of things).
const MAX_LINES: u64 = 10_000;

fn core_type(t: u8) -> &'static str {
    match t {
        hal::smp::CORE_TYPE_PERFORMANCE => "P",
        hal::smp::CORE_TYPE_EFFICIENCY => "E",
        _ => "-",
    }
}

/// SC-7a: the Interactive line: the group in front, the granted groups, and the wake-to-run waits of
/// Interactive threads (count, average, largest since boot and in the last 10-20 s, late ones).
pub fn interactive_line(st: &sched::interactive::WaitStats, focus: Option<u64>, grants: &[(u64, u8)]) -> String {
    let g: Vec<String> = grants.iter().map(|(g, b)| format!("{g}:{}", sched::interactive::bits_names(*b))).collect();
    format!(
        "interactive focus={} grants={} waits={} avg_us={} max_us={} recent_max_us={} late={} late_ms={}\n",
        focus.map_or(String::from("none"), |f| format!("{f}")),
        if g.is_empty() { String::from("-") } else { g.join(",") },
        st.waits,
        (st.total_ns / 1000).checked_div(st.waits).unwrap_or(0),
        st.max_ns / 1000,
        st.recent_max_ns / 1000,
        st.late,
        sched::interactive::LATE_NS / 1_000_000
    )
}

/// The answer text (pure, host-tested).
pub fn render(cpus: &[CpuStat], threads: Option<&[ThreadStat]>, max: usize, duty: Option<usize>, counters: &[(&str, u64)], inter: Option<&str>) -> String {
    let online = cpus.iter().filter(|c| c.online).count();
    let mut s = format!(
        "sched cpus={} online={} poll_duty={} slice_ms={}\n",
        cpus.len(),
        online,
        duty.map_or(String::from("none"), |d| format!("{d}")),
        sched::SLICE_NS / 1_000_000
    );
    s += "cpu apic core thread cluster type state queued busy_ms idle_ms busy_pct switches migrated_in steals pushes resched_ipis irq_wakes heartbeat tick idle_at_ms idle_if user_ms nice_ms\n";
    for c in cpus {
        let state = if !c.online {
            "offline"
        } else if c.halted {
            "halted"
        } else if c.idle {
            "idle"
        } else {
            "busy"
        };
        let total = c.busy_ns + c.idle_ns;
        let pct = (c.busy_ns * 100).checked_div(total).unwrap_or(0);
        let t = &c.topology;
        s += &format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
            c.cpu,
            t.apic_id,
            t.core_id,
            t.thread_id,
            t.cluster_id,
            core_type(t.core_type),
            state,
            c.queued,
            c.busy_ns / 1_000_000,
            c.idle_ns / 1_000_000,
            pct,
            c.switches,
            c.migrations_in,
            c.steals,
            c.pushes,
            c.resched_ipis,
            c.irq_wakes,
            c.heartbeat,
            if c.tick { "on" } else { "off" },
            c.idle_entry.map_or(String::from("-"), |(t, _)| format!("{}", t / 1_000_000)),
            c.idle_entry.map_or("-", |(_, i)| if i { "1" } else { "0" }),
            c.user_ns / 1_000_000,
            c.nice_ns / 1_000_000
        );
    }
    if let Some(ts) = threads {
        s += &format!("threads={} shown={}\n", ts.len(), ts.len().min(max));
        s += "id name class state cpu pin rt_prio runtime_ms migrations wakeups user_ms sys_ms nice policy affinity vcsw ivcsw group wait_max_us\n";
        for t in ts.iter().take(max) {
            let cpu = if t.cpu == sched::NO_CPU { String::from("-") } else { format!("{}", t.cpu) };
            s += &format!(
                "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
                t.id,
                t.name,
                t.class.name(),
                t.state,
                cpu,
                t.pinned.map_or(String::from("-"), |p| format!("{p}")),
                t.rt_prio,
                t.runtime_ns / 1_000_000,
                t.migrations,
                t.wakeups,
                t.user_ns / 1_000_000,
                t.system_ns / 1_000_000,
                t.nice,
                t.policy.name(),
                if t.affinity_all { "all" } else { "set" },
                t.nvcsw,
                t.nivcsw,
                t.group,
                t.wait_max_ns / 1000
            );
        }
    }
    if let Some(l) = inter {
        s += l;
    }
    if !counters.is_empty() {
        s += "counters";
        for (n, v) in counters {
            s += &format!(" {n}={v}");
        }
        s += "\n";
    }
    s
}



fn tool(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let bad = |n: &str| ToolError::BadArgument(String::from(n));
    let mut with_threads = false;
    let mut max = 200usize;
    if let Some(v) = args.get("threads").filter(|v| !v.is_null()) {
        with_threads = v.as_bool().ok_or_else(|| bad("threads"))?;
    }
    if let Some(v) = args.get("max").filter(|v| !v.is_null()) {
        max = v.as_u64().filter(|m| (1..=MAX_LINES).contains(m)).ok_or_else(|| bad("max"))? as usize;
    }
    let cpus = sched::cpu_stats();
    let threads: Option<Vec<ThreadStat>> = with_threads.then(|| {
        let mut t = sched::thread_stats();
        // Busiest first.
        t.sort_by_key(|x| core::cmp::Reverse(x.runtime_ns));
        t
    });
    let inter = interactive_line(&sched::interactive::wait_stats_now(), sched::interactive::focus(), &sched::interactive::grants());
    Ok(alloc::vec![ToolOutput::Text(render(&cpus, threads.as_deref(), max, sched::duty_cpu(), &sched::counters(), Some(&inter)))])
}

mcp::mcp_tool!(
    name: "sched",
    owner: "SC",
    safety: Read,
    description: "Scheduler state per CPU: online/idle/halted/busy, queued threads, busy and idle time since boot with busy percent, switches, threads migrated in, steals, pushes, reschedule IPIs, interrupt-time wake-ups, heartbeat, tick on/off, last idle entry (ms since boot, IF), user and nice time (ms, SC-6), topology, and the sched.* counters (sleep timer, ticks, fallbacks, halts) (APIC ID, core, SMT thread, cache cluster, P/E core type). SC-7a: the interactive line (group in front, granted groups, wake-to-run waits of Interactive threads: count, average, largest since boot and in the last 10-20 s, late ones). With threads=true also every thread (busiest first): class, state, CPU, pin, KernelRt priority (FIFO/RR: the program's priority), CPU time, migrations, wake-ups, user and system time (ms), nice, policy (other/fifo/rr/batch/idle/ext), affinity (all or set), voluntary and involuntary switches, thread group (tgid, 0 = kernel), longest Interactive wake-to-run wait (us).",
    schema: r#"{"type":"object","properties":{"threads":{"type":"boolean","description":"also list the threads"},"max":{"type":"integer","minimum":1,"maximum":10000,"description":"at most this many thread lines (default 200)"}}}"#,
    handler: tool,
);

fn ktest_sched_tool() -> ktest::TestResult {
    let t = mcp::find("sched").ok_or("tool sched not registered")?;
    ktest::kassert!(t.owner == "SC" && t.safety == mcp::Safety::Read);
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let args = Json::parse(r#"{"threads":true,"max":500}"#).map_err(|e| format!("{e:?}"))?;
    let out = (t.handler)(&ctx, &args).map_err(|e| format!("{e:?}"))?;
    let text = match out.first() {
        Some(ToolOutput::Text(s)) => s.clone(),
        _ => return Err(String::from("no text answer")),
    };
    let cpus = hal::percpu::cpu_count();
    ktest::kassert!(text.starts_with(&format!("sched cpus={cpus} online={}", sched::online_cpus())), "{text}");
    ktest::kassert!(text.contains(" idle/0 background "), "{text}");
    ktest::kassert!(text.contains("\ninteractive focus="), "{text}");
    let bad = Json::parse(r#"{"max":0}"#).map_err(|e| format!("{e:?}"))?;
    ktest::kassert!(matches!((t.handler)(&ctx, &bad), Err(ToolError::BadArgument(ref n)) if n == "max"));
    Ok(())
}
ktest::ktest!("sched.mcp_tool", Safe, ktest_sched_tool);

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn renders_cpus_and_threads() {
        let topology = hal::smp::Topology { apic_id: 3, core_id: 1, thread_id: 1, cluster_id: 0, core_type: hal::smp::CORE_TYPE_PERFORMANCE, ..Default::default() };
        let cpus = [
            CpuStat { cpu: 0, online: true, idle: true, halted: false, busy_ns: 300_000_000, idle_ns: 700_000_000, user_ns: 200_000_000, nice_ns: 50_000_000, switches: 5, ..Default::default() },
            CpuStat { cpu: 1, online: true, halted: true, idle: true, topology, heartbeat: 42, tick: true, idle_entry: Some((5_000_000, true)), ..Default::default() },
            CpuStat { cpu: 2, online: false, ..Default::default() },
        ];
        let th = [ThreadStat {
            id: 7,
            name: String::from("worker"),
            class: sched::Class::KernelRt,
            state: "running",
            cpu: 1,
            pinned: Some(1),
            rt_prio: 60,
            runtime_ns: 12_000_000,
            migrations: 2,
            wakeups: 9,
            user_ns: 5_000_000,
            system_ns: 7_000_000,
            nvcsw: 3,
            nivcsw: 4,
            nice: 5,
            policy: sched::Policy::Batch,
            affinity_all: false,
            group: 42,
            wait_max_ns: 2_500_000,
        }];
        let st = sched::interactive::WaitStats { waits: 4, total_ns: 4_000_000, max_ns: 2_500_000, recent_max_ns: 900_000, late: 1 };
        let inter = interactive_line(&st, Some(42), &[(42, 5), (77, 2)]);
        assert_eq!(inter, "interactive focus=42 grants=42:compositor,focus,77:input waits=4 avg_us=1000 max_us=2500 recent_max_us=900 late=1 late_ms=20\n");
        assert_eq!(interactive_line(&Default::default(), None, &[]), "interactive focus=none grants=- waits=0 avg_us=0 max_us=0 recent_max_us=0 late=0 late_ms=20\n");
        let s = render(&cpus, Some(&th), 10, Some(0), &[("sched.ticks_fired", 3), ("sched.halts", 9)], Some(&inter));
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "sched cpus=3 online=2 poll_duty=0 slice_ms=10");
        assert_eq!(lines[2], "0 0 0 0 0 - idle 0 300 700 30 5 0 0 0 0 0 0 off - - 200 50");
        assert_eq!(lines[3], "1 3 1 1 0 P halted 0 0 0 0 0 0 0 0 0 0 42 on 5 1 0 0");
        assert!(lines[4].starts_with("2 ") && lines[4].contains(" offline "));
        assert_eq!(lines[5], "threads=1 shown=1");
        assert_eq!(lines[7], "7 worker kernel_rt running 1 1 60 12 2 9 5 7 5 batch set 3 4 42 2500");
        assert_eq!(lines[8], "interactive focus=42 grants=42:compositor,focus,77:input waits=4 avg_us=1000 max_us=2500 recent_max_us=900 late=1 late_ms=20");
        assert_eq!(lines[9], "counters sched.ticks_fired=3 sched.halts=9");
        assert!(!render(&cpus, None, 10, None, &[], None).contains("threads="));
        assert!(!render(&cpus, None, 10, None, &[], None).contains("counters"));
    }
}
