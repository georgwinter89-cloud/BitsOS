//! In-kernel tests of MC-2: every generic tool over the kernel loopback (a server on its own port, a client
//! through `net::stack()`), the refusals (class rules, read_phys outside RAM, MSRs off the list), and the
//! framework's `output`/`stop` and answer-first rule with test tools on a server of their own (test tools
//! never appear in the kernel's real tool list).

use super::run_ktests;
pub(super) use crate::testclient::{Client, Reply, with_server};
use crate::kernel::SEC;
use crate::{Json, Safety, TOOLS, ToolContext, ToolEntry, ToolOutput, ToolResult};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};

const TOOLS_PORT: u16 = 25003;

fn registry() -> TestResult {
    let want = [
        ("hw_info", Safety::Read),
        ("net_info", Safety::Read),
        ("ktest", Safety::Change),
        ("cpus", Safety::Read),
        ("reboot", Safety::Change),
        ("msr_read", Safety::Read),
        ("read_phys", Safety::Read),
        ("output", Safety::Read),
        ("stop", Safety::Change),
        ("screen", Safety::Read),
        ("send_keys", Safety::Change),
        ("send_mouse", Safety::Change),
        ("read_file", Safety::Read),
        ("list_dir", Safety::Read),
        ("stat", Safety::Read),
        ("write_file", Safety::Change),
        ("delete", Safety::DeleteOverwrite),
        ("fetch", Safety::Change),
    ];
    for (name, safety) in want {
        let t = crate::find(name).ok_or_else(|| format!("{name} not registered"))?;
        kassert!(t.owner == "MC" && t.safety == safety, "{name}: owner {} safety {:?}", t.owner, t.safety);
    }
    // R8f over the whole kernel: no lane registers a power-off tool.
    for t in TOOLS.entries() {
        let n = t.name.to_ascii_lowercase();
        kassert!(!n.contains("poweroff") && !n.contains("power_off") && !n.contains("shutdown"), "R8f: tool {}", t.name);
    }
    Ok(())
}
ktest!("mcp.tool.registry", Safe, registry);

/// A port still taken (a server an earlier run left bound, another test's) does not fail the test: the next
/// one is used. Rerunnable: both servers stop before it returns.
fn next_port_when_taken() -> TestResult {
    const PORT: u16 = 25040;
    with_server(PORT, TOOLS.entries(), |outer| {
        kassert!(outer.port() == PORT, "the free port itself first: {}", outer.port());
        with_server(PORT, TOOLS.entries(), |inner| {
            kassert!(inner.port() == PORT + 1, "moved on to {}", inner.port());
            inner.ok("version", "{}")?;
            Ok(())
        })?;
        outer.ok("version", "{}")?;
        Ok(())
    })
}
ktest!("mcp.testclient.next_port_when_taken", Safe, next_port_when_taken);

fn hw_info() -> TestResult {
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let t = c.ok("hw_info", "{}")?;
        for s in ["kernel", "cpu", "memory", "firmware", "interrupts", "pci"] {
            kassert!(t.contains(&format!("== {s} ==")), "section {s} missing");
        }
        kassert!(t.contains("next start: "), "kernel section without the next start");
        let cpu = c.ok("hw_info", r#"{"section":"cpu"}"#)?;
        kassert!(cpu.contains("cpu vendor=") && !cpu.contains("vendor= "), "{cpu}");
        kassert!(cpu.contains("cpu flags: ") && cpu.contains("avx2"), "no CPU flags (x86-64-v3 has avx2): {cpu}");
        let mem = c.ok("hw_info", r#"{"section":"memory"}"#)?;
        kassert!(mem.contains("frames total=") && mem.contains("memory map regions=") && mem.contains(" ram"), "{mem}");
        let fw = c.ok("hw_info", r#"{"section":"firmware"}"#)?;
        kassert!(!fw.contains("not read"), "acpi tables not read: {fw}");
        let irq = c.ok("hw_info", r#"{"section":"interrupts"}"#)?;
        kassert!(irq.contains("apic mode="), "{irq}");
        let pci = c.ok("hw_info", r#"{"section":"pci"}"#)?;
        kassert!(pci.lines().count() >= 3 && !pci.contains("no devices"), "{pci}");
        let k = c.ok("hw_info", r#"{"section":"kernel"}"#)?;
        klog::event!(Info, "ktest", "hw_info.kernel", text = k.replace('\n', " | "));
        let bad = c.call("hw_info", r#"{"section":"disk"}"#)?;
        kassert!(bad.rpc_error == Some(-32602), "unknown section: {:?}", bad.rpc_error);
        Ok(())
    })
}
ktest!("mcp.tool.hw_info", Safe, hw_info);

fn net_info() -> TestResult {
    #[cfg(target_os = "none")]
    let configured = net::stack::kernel().and_then(|s| s.wait_configured(15 * SEC));
    #[cfg(not(target_os = "none"))]
    let configured: Option<(String, net::Ipv4)> = None;
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let t = c.ok("net_info", r#"{"connections":5}"#)?;
        kassert!(t.contains("== interfaces ==") && t.contains("== counters ==") && t.contains("== connections (newest 5) =="), "{t}");
        // INT-W11 (PM-2 CR3): MCP no longer records its own connections; the net stack's view does, for streams
        // on a card. This test server listens on loopback, which the view does not show (pm.conn.* cover it).
        if let Some((name, ip)) = configured {
            let [a, b, cc, d] = ip.0;
            kassert!(t.contains(&format!("{name} mac=")) && t.contains(&format!("addr={a}.{b}.{cc}.{d}/")) && t.contains("link=up"), "{t}");
            kassert!(t.contains("net.poll_rounds="), "{t}");
            if hal::cpu::detect().hypervisor && t.contains("10.0.2.15") {
                kassert!(t.contains("router=10.0.2.2") && t.contains("default-route"), "{t}");
            }
        }
        Ok(())
    })
}
ktest!("mcp.tool.net_info", Safe, net_info);

// Fixtures for the class rule: they do nothing (no sound, no WLAN, no display write); they exist so the rule can
// be shown.
fn probe() -> TestResult {
    Ok(())
}
ktest!("mcp.class_probe.audible", Audible, probe);
ktest!("mcp.class_probe.display", Display, probe);
ktest!("mcp.class_probe.wlan", Wlan, probe);

// Fixtures for the one-run-at-a-time and stop-between-tests checks: `a_hold` waits only while `HOLD` is set
// (a plain run passes at once), `b_next` is the test a stopped run never reaches.
static HOLD: AtomicBool = AtomicBool::new(false);
static HELD: AtomicBool = AtomicBool::new(false);

fn hold() -> TestResult {
    if !HOLD.load(Ordering::Acquire) {
        return Ok(());
    }
    HELD.store(true, Ordering::Release);
    let end = time::now_ns() + 10 * SEC;
    while HOLD.load(Ordering::Acquire) && time::now_ns() < end {
        sched::sleep_ns(SEC / 50);
    }
    Ok(())
}
ktest!("mcp.fixture.a_hold", Safe, hold);
ktest!("mcp.fixture.b_next", Safe, probe);

/// Lets `a_hold` go on every way out of the test.
struct Release;
impl Drop for Release {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
    }
}

fn call_number(text: &str) -> Result<u64, String> {
    text.split("(call ").nth(1).and_then(|s| s.split(')').next()).and_then(|s| s.parse().ok()).ok_or_else(|| format!("no call number: {text}"))
}

/// A second run while one is going on is refused; `stop` ends the first one between two tests.
fn one_run_and_stop(c: &mut Client) -> TestResult {
    HELD.store(false, Ordering::Release);
    HOLD.store(true, Ordering::Release);
    let _release = Release;
    let r = c.call("ktest", r#"{"filter":"mcp.fixture.","timeout_ms":200}"#)?;
    kassert!(r.error && r.text.starts_with("Still running: ktest (call "), "{}", r.text);
    let n = call_number(&r.text)?;
    let start = time::now_ns();
    while !HELD.load(Ordering::Acquire) && time::now_ns() < start + 5 * SEC {
        sched::sleep_ns(SEC / 50);
    }
    kassert!(HELD.load(Ordering::Acquire), "the run never reached mcp.fixture.a_hold");
    let mut d = c.second();
    let second = d.call("ktest", r#"{"filter":"kreg."}"#);
    let second = second?;
    kassert!(second.error && second.text.contains("another ktest run is going on"), "{}", second.text);
    let s = d.call("stop", &format!(r#"{{"call":{n}}}"#))?;
    kassert!(s.text.starts_with(&format!("call {n} ktest: still running")) && s.text.contains("stop asked"), "{}", s.text);
    HOLD.store(false, Ordering::Release);
    let start = time::now_ns();
    loop {
        let o = d.call("output", &format!(r#"{{"call":{n}}}"#))?;
        if o.text.contains("finished") {
            kassert!(!o.error && o.text.contains("ok mcp.fixture.a_hold") && !o.text.contains("mcp.fixture.b_next"), "{}", o.text);
            kassert!(o.text.contains("passed=1 failed=0 skipped=0 refused=0 not_run=1 (stopped)"), "{}", o.text);
            break;
        }
        kassert!(time::now_ns() < start + 5 * SEC, "the stopped run did not end: {}", o.text);
        sched::sleep_ns(SEC / 20);
    }
    d.close();
    Ok(())
}

fn ktest_tool() -> TestResult {
    if run_ktests::run_active() {
        // Run by the ktest tool itself (a run over MCP): the nested call would only be refused.
        return ktest::skip("inside a ktest run over MCP; the tool allows one run at a time");
    }
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let t = c.ok("ktest", r#"{"filter":"kreg."}"#)?;
        kassert!(t.lines().next().is_some_and(|h| h.contains("failed=0") && !h.contains("passed=0")), "{t}");
        kassert!(t.contains("\nok kreg.mcp_tools_unique ("), "{t}");
        let none = c.call("ktest", r#"{"filter":"zz.nothing"}"#)?;
        kassert!(none.error && none.text.contains("no ktest matches"), "{}", none.text);
        // This test inside a run over MCP skips itself instead of failing on the one-run rule.
        let nested = c.ok("ktest", r#"{"filter":"mcp.tool.ktest"}"#)?;
        kassert!(nested.contains("skip mcp.tool.ktest: inside a ktest run over MCP") && nested.contains("failed=0"), "{nested}");
        one_run_and_stop(c)
    })?;
    // The hardware rule, decided here as on the Mini-PC: QemuOnly skipped, Display run, the switch classes refused.
    let ctx = ToolContext::detached(u64::MAX);
    let r = run_ktests::run(&ctx, "nw.stack.tcp_host,mcp.class_probe", true, false).map_err(|e| format!("{e:?}"))?;
    let t = match r.first() {
        Some(ToolOutput::Text(t)) => t.clone(),
        _ => return Err(String::from("no text")),
    };
    kassert!(
        t.contains("skip nw.stack.tcp_host: QemuOnly") && t.contains("ok mcp.class_probe.display") && t.contains("refused mcp.class_probe.audible: Audible") && t.contains("refused mcp.class_probe.wlan: wlan: needs wlan=on"),
        "{t}"
    );
    kassert!(t.starts_with("ktest") && t.contains("passed=1 failed=0 skipped=1 refused=2"), "{t}");
    let only = run_ktests::run(&ctx, "mcp.class_probe.audible,mcp.class_probe.wlan", true, false);
    kassert!(matches!(only, Err(crate::ToolError::Refused(_))), "only refused tests: the call is refused, {only:?}");
    // WL-4 CR 3: with wlan=on the Wlan class runs on hardware, Audible stays refused.
    let on = run_ktests::run(&ctx, "mcp.class_probe.audible,mcp.class_probe.wlan", true, true).map_err(|e| format!("{e:?}"))?;
    let t = match on.first() {
        Some(ToolOutput::Text(t)) => t.clone(),
        _ => return Err(String::from("no text")),
    };
    kassert!(t.contains("ok mcp.class_probe.wlan") && t.contains("refused mcp.class_probe.audible: Audible") && t.contains("Wlan run"), "{t}");
    Ok(())
}
ktest!("mcp.tool.ktest", Safe, ktest_tool);

fn cpus() -> TestResult {
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let t = c.ok("cpus", "{}")?;
        kassert!(t.starts_with("cpus online=") && t.contains("firmware_listed="), "{t}");
        kassert!(t.lines().any(|l| l.ends_with(" mcp-accept class=KernelRt state=sleeping") || l.contains(" mcp-accept class=KernelRt")), "the acceptor is KernelRt: {t}");
        kassert!(t.lines().any(|l| l.contains(" mcp-worker class=Normal")), "the tool runs in a Normal worker: {t}");
        kassert!(!t.lines().any(|l| l.contains("mcp-worker class=KernelRt")), "{t}");
        Ok(())
    })
}
ktest!("mcp.tool.cpus", Safe, cpus);

fn reboot_dry_run() -> TestResult {
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let r = c.call("reboot", r#"{"dry_run":true}"#)?;
        kassert!(!r.error && !r.closed, "a dry run neither fails nor ends the connection: {}", r.text);
        kassert!(r.text.starts_with("reboot (dry run") && r.text.contains("next start: ") && r.text.contains("reset by: "), "{}", r.text);
        if hal::cpu::detect().hypervisor {
            kassert!(r.text.contains("keyboard controller 0x64 <- 0xfe (the FADT names an 8042)"), "q35 names its 8042: {}", r.text);
        }
        Ok(())
    })
}
ktest!("mcp.tool.reboot_dry_run", Safe, reboot_dry_run);

fn msr_read() -> TestResult {
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let t = c.ok("msr_read", r#"{"msr":"0xC0000080"}"#)?;
        let v = t
            .split(" = ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
            .ok_or_else(|| format!("no value: {t}"))?;
        kassert!(v & hal::msr::EFER_LMA != 0 && v & hal::msr::EFER_NXE != 0, "EFER {v:#x}");
        for refused in [r#"{"msr":16}"#, r#"{"msr":"0xC0010015"}"#, r#"{"msr":"0x3A"}"#] {
            let r = c.call("msr_read", refused)?;
            kassert!(r.error && r.text.starts_with("Refused: MSR"), "{refused}: {}", r.text);
        }
        Ok(())
    })
}
ktest!("mcp.tool.msr_read", Safe, msr_read);

fn read_phys() -> TestResult {
    let pattern: Vec<u8> = (0..96u32).map(|i| (i * 7 + 3) as u8).collect();
    let phys = hal::virt_to_phys(pattern.as_ptr() as u64).ok_or("heap not in the direct map")?;
    let map = acpi::live::memory_map().ok_or("no memory map")?;
    let top = map.regions().iter().map(|r| r.end).max().unwrap_or(0);
    let not_ram = map.regions().iter().find(|r| !acpi::memmap::is_ram(r.ty)).map(|r| r.start);
    let ram = |r: &acpi::memmap::Region| acpi::memmap::is_ram(r.ty);
    // RAM that ends against a non-RAM region or a hole, and RAM that runs on into RAM of another type.
    let edge = map.regions().windows(2).find(|w| ram(&w[0]) && (!ram(&w[1]) || w[0].end < w[1].start)).map(|w| w[0].end);
    let seam = map.regions().windows(2).find(|w| ram(&w[0]) && ram(&w[1]) && w[0].end == w[1].start && w[0].ty != w[1].ty).map(|w| w[0].end);
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        let t = c.ok("read_phys", &format!(r#"{{"addr":"{phys:#x}","len":96}}"#))?;
        let hex: Vec<String> = pattern[..16].iter().map(|b| format!("{b:02x}")).collect();
        kassert!(t.contains(&format!("{phys:#014x}: {}", hex.join(" "))), "{t}");
        let b = c.ok("read_phys", &format!(r#"{{"addr":{phys},"len":96,"format":"base64"}}"#))?;
        let mut want = String::new();
        crate::b64::encode_into(&pattern, &mut want);
        kassert!(b.ends_with(&want), "{b}");
        let mut refusals = alloc::vec![
            (0xFEE0_0000u64, 4u64, "the LAPIC"),
            (top + 0x1000, 16, "past the end of the memory map"),
            (1 << 50, 16, "far outside"),
            (phys, crate::tools::PHYS_MAX + 1, "more than 64 KiB"),
        ];
        if let Some(m) = not_ram {
            refusals.push((m, 16, "a non-RAM region of the map"));
        }
        if let Some(e) = edge {
            refusals.push((e - 8, 16, "from RAM into a hole or non-RAM"));
        }
        if let Some(e) = seam {
            let t = c.ok("read_phys", &format!(r#"{{"addr":"{:#x}","len":16}}"#, e - 8))?;
            kassert!(t.starts_with(&format!("phys {:#x} len=16", e - 8)), "RAM to RAM of another type: {t}");
        }
        for (addr, len, what) in refusals {
            let r = c.call("read_phys", &format!(r#"{{"addr":"{addr:#x}","len":{len}}}"#))?;
            kassert!(r.error && r.text.starts_with("Refused: "), "{what} ({addr:#x}): {}", r.text);
        }
        Ok(())
    })
}
ktest!("mcp.tool.read_phys", Safe, read_phys);

/// read_phys trusts that everything the memory map calls RAM is in the direct map (bitsboot maps exactly the
/// `memtype::is_ram` ranges): checked here page by page at each region's start, end and up to 64 points between.
fn read_phys_direct_map() -> TestResult {
    let map = acpi::live::memory_map().ok_or("no memory map")?;
    let root = hal::paging::current_root();
    let mut checked = 0u32;
    for r in map.regions().iter().filter(|r| acpi::memmap::is_ram(r.ty) && r.end > r.start) {
        let pages = (r.end - r.start).div_ceil(4096);
        let step = pages.div_ceil(64).max(1);
        let mut at = (0..pages).step_by(step as usize).map(|i| r.start + i * 4096).collect::<Vec<_>>();
        at.push((r.end - 1) & !0xFFF);
        for p in at {
            let got = hal::paging::translate(root, hal::phys_to_virt(p)).map(|(phys, _)| phys);
            kassert!(got == Some(p), "RAM page {p:#x} (type {:#x}, {:#x}-{:#x}) is not direct-mapped: {got:x?}", r.ty, r.start, r.end);
            checked += 1;
        }
    }
    kassert!(checked > 0, "no RAM in the memory map");
    Ok(())
}
ktest!("mcp.tool.read_phys_direct_map", Safe, read_phys_direct_map);

/// Malformed and hostile arguments over the RPC path: each is answered (invalid params, or a refusal), none
/// panics, and none does anything.
fn hostile_args() -> TestResult {
    with_server(TOOLS_PORT, TOOLS.entries(), |c| {
        for (tool, args) in super::HOSTILE {
            let r = c.call(tool, args)?;
            kassert!(
                r.rpc_error == Some(-32602) || (r.error && (r.text.starts_with("Refused: ") || r.text.starts_with("ERROR: "))),
                "{tool} {args}: error {:?} isError {} {}",
                r.rpc_error,
                r.error,
                r.text
            );
        }
        Ok(())
    })
}
ktest!("mcp.tool.hostile_args", Safe, hostile_args);

// ---------------------------------------------------------------------------------------------------------
// The framework with test tools on a server of their own.

const CALLS_PORT: u16 = 25004;
static SPIN_TICKS: AtomicU64 = AtomicU64::new(0);
static AFTER_DONE: AtomicBool = AtomicBool::new(false);

fn spin(ctx: &ToolContext, _: &Json) -> ToolResult {
    let mut n = 0u64;
    let end = ctx.now_ns() + 20 * SEC;
    while ctx.now_ns() < end {
        if ctx.stop_requested() {
            return Ok(alloc::vec![ToolOutput::Text(format!("spun {n}, stopped"))]);
        }
        ctx.output(&format!("tick {n}\n"));
        n += 1;
        SPIN_TICKS.store(n, Ordering::Release);
        sched::sleep_ns(SEC / 50);
    }
    Ok(alloc::vec![ToolOutput::Text(format!("spun {n}, never stopped"))])
}

fn after(ctx: &ToolContext, _: &Json) -> ToolResult {
    AFTER_DONE.store(false, Ordering::Release);
    let ok = ctx.after_answer(Box::new(|| AFTER_DONE.store(true, Ordering::Release)));
    Ok(alloc::vec![ToolOutput::Text(format!("registered={ok}"))])
}

const NO_ARGS: &str = r#"{"type":"object","properties":{}}"#;
const CALL_ARG: &str = r#"{"type":"object","properties":{"call":{"type":"integer"}}}"#;
static CALL_TOOLS: [ToolEntry; 4] = [
    ToolEntry { name: "t_spin", description: "test", input_schema: NO_ARGS, owner: "MC", safety: Safety::Read, handler: spin },
    ToolEntry { name: "t_after", description: "test", input_schema: NO_ARGS, owner: "MC", safety: Safety::Change, handler: after },
    ToolEntry { name: "output", description: "test", input_schema: CALL_ARG, owner: "MC", safety: Safety::Read, handler: crate::server_only },
    ToolEntry { name: "stop", description: "test", input_schema: CALL_ARG, owner: "MC", safety: Safety::Change, handler: crate::server_only },
];

fn output_stop() -> TestResult {
    SPIN_TICKS.store(0, Ordering::Release);
    with_server(CALLS_PORT, &CALL_TOOLS, |c| {
        let r = c.call("t_spin", r#"{"timeout_ms":200}"#)?;
        kassert!(r.error && r.text.starts_with("Still running: t_spin (call "), "{}", r.text);
        // The progress so far comes with the "Still running" answer, later progress with `output`.
        kassert!(r.text.contains("--- progress so far ---\ntick 0\ntick 1\n"), "{}", r.text);
        let n: u64 = r.text.split("(call ").nth(1).and_then(|s| s.split(')').next()).and_then(|s| s.parse().ok()).ok_or("no call number")?;
        let (start, seen) = (time::now_ns(), SPIN_TICKS.load(Ordering::Acquire));
        while SPIN_TICKS.load(Ordering::Acquire) < seen + 3 && time::now_ns() < start + 3 * SEC {
            sched::sleep_ns(SEC / 50);
        }
        let o = c.ok("output", &format!(r#"{{"call":{n}}}"#))?;
        kassert!(o.starts_with(&format!("call {n} t_spin: running")) && o.contains("tick ") && !o.contains("tick 0\n"), "{o}");
        let s = c.ok("stop", &format!(r#"{{"call":{n}}}"#))?;
        kassert!(s.starts_with(&format!("call {n} t_spin: stopped")) && s.contains("stopped"), "{s}");
        let gone = c.call("output", &format!(r#"{{"call":{n}}}"#))?;
        kassert!(gone.error && gone.text.contains(&format!("no call {n}")), "{}", gone.text);
        Ok(())
    })
}
ktest!("mcp.calls.output_stop", Safe, output_stop);

fn answer_first() -> TestResult {
    with_server(CALLS_PORT, &CALL_TOOLS, |c| {
        let r = c.call("t_after", "{}")?;
        kassert!(r.text == "registered=true" && r.closed, "{} closed={}", r.text, r.closed);
        kassert!(!AFTER_DONE.load(Ordering::Acquire), "the action ran before the answer was read");
        let start = time::now_ns();
        while !AFTER_DONE.load(Ordering::Acquire) && time::now_ns() < start + 3 * SEC {
            sched::sleep_ns(SEC / 50);
        }
        kassert!(AFTER_DONE.load(Ordering::Acquire), "the action never ran");
        Ok(())
    })
}
ktest!("mcp.calls.answer_first", Safe, answer_first);
