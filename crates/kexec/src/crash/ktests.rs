//! RB-2's ktests. Safe ones (rerunnable, also over MCP on the Mini-PC): the frame-pointer walk through known
//! functions, the symbol table, an NMI sample of the other CPUs (one spinning with interrupts off), the record
//! format and the live page read (never written: it may hold a real crash record not yet looked at), the log
//! tail. QemuOnly: the record round trip in the real crash page, `rb.crash.forced`, the real panic (with CONFIG
//! `rb_crash_panic=panic|fault`), and on the boot after it the check of what that boot logged.

use super::*;
use alloc::format;
use core::sync::atomic::AtomicUsize;
use ktest::{TestResult, kassert, ktest};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

fn addr_of(f: fn(&mut [Step]) -> usize) -> u64 {
    f as usize as u64
}

/// True if return address `ret` lies in function `f` (by name with a symbol table, else within 4 KiB after its
/// start: these test functions are small).
fn ret_in(ret: u64, f: u64, name: &str) -> bool {
    match symbols() {
        Some(_) => symbolize(ret, true).as_str().contains(name),
        None => ret > f && ret - f < 4096,
    }
}

#[inline(never)]
fn bt_level_three(out: &mut [Step]) -> usize {
    let n = hal::panic::walk(hal::panic::frame_pointer(), out);
    core::hint::black_box(n)
}

// Each level uses its callee's result after the call, so none becomes a tail call (the frame must exist).
#[inline(never)]
fn bt_level_two(out: &mut [Step]) -> usize {
    let n = bt_level_three(out);
    core::hint::black_box(&out);
    n
}

#[inline(never)]
fn bt_level_one(out: &mut [Step]) -> usize {
    let n = bt_level_two(out);
    core::hint::black_box(&out);
    n
}

/// The walk from a known chain names the callers in order: level two, level one, this test.
fn backtrace() -> TestResult {
    let mut steps = [Step::None; 24];
    let n = bt_level_one(&mut steps);
    kassert!(n >= 3, "only {n} frames: {:?}", &steps[..n]);
    let want = [(addr_of(bt_level_two), "bt_level_two"), (addr_of(bt_level_one), "bt_level_one")];
    for (i, (f, name)) in want.iter().enumerate() {
        match steps[i] {
            Step::Call(ret) => kassert!(ret_in(ret, *f, name), "frame {i} {ret:#x} is not in {name} ({:?})", symbolize(ret, true).as_str()),
            s => return Err(format!("frame {i}: {s:?}")),
        }
    }
    let third_ok = match steps[2] {
        Step::Call(r) => symbols().is_none() || symbolize(r, true).as_str().contains("crash::ktests::backtrace"),
        _ => false,
    };
    kassert!(third_ok, "frame 2: {:?} {:?}", steps[2], if let Step::Call(r) = steps[2] { symbolize(r, true) } else { Buf::new() }.as_str());
    // Every step is a return address into kernel code.
    for s in &steps[..n] {
        kassert!(matches!(s, Step::Call(r) if hal::panic::is_kernel_text(*r)), "{s:?}");
    }
    Ok(())
}
ktest!("rb.crash.backtrace", Safe, backtrace);

/// With a symbol table (scenario boots and bundles that carry `boot/kernel.syms`): it names this build.
fn symbol_table() -> TestResult {
    let Some((t, src)) = symbols() else {
        return ktest::skip("no symbol table in this boot (boot/kernel.syms in the initramfs or fw/KSYMS)");
    };
    kassert!(symbols::matches_build(t, symbols::rb_ksyms_anchor as fn() -> u64 as usize as u64), "anchor");
    let r = report as fn(&core::panic::PanicInfo<'_>) -> ! as usize as u64;
    let s = symbolize(r, false);
    kassert!(s.as_str() == "kexec::crash::report+0x0", "report is {:?}", s.as_str());
    kassert!(symbolize(r + 5, false).as_str() == "kexec::crash::report+0x5");
    kassert!(symbolize(0x1000, false).as_str() == "?", "no name below the kernel");
    klog::event!(Info, "crash", "ktest.symbols", source = src, symbols = t.len(), bytes = t.bytes());
    Ok(())
}
ktest!("rb.crash.symbols", Safe, symbol_table);

static STUCK: AtomicBool = AtomicBool::new(false);
static SPINNING: AtomicUsize = AtomicUsize::new(0);

/// Runs on an AP from its SMP call interrupt (interrupts off) until the test lets go; bounded (10 s).
#[inline(never)]
fn rb_crash_spin(_: u64) -> u64 {
    SPINNING.fetch_add(1, Ordering::AcqRel);
    let end = time::now_ns() + 10 * SEC;
    while STUCK.load(Ordering::Acquire) && time::now_ns() < end {
        core::hint::spin_loop();
    }
    SPINNING.fetch_sub(1, Ordering::AcqRel);
    0
}

fn unstick() -> TestResult {
    STUCK.store(false, Ordering::Release);
    let end = time::now_ns() + 5 * SEC;
    while SPINNING.load(Ordering::Acquire) != 0 {
        kassert!(time::now_ns() < end, "the spinning CPU did not end its spin");
        sched::sleep_ns(MS);
    }
    Ok(())
}

fn sample_round(k: usize) -> TestResult {
    let before = hal::nmi::answers(k);
    let r = hal::nmi::sample_others(time::now_ns, 2 * SEC).map_err(|e| format!("sample: {e:?}"))?;
    let me = hal::percpu::this_cpu();
    kassert!(r.asked == hal::percpu::cpu_count() - 1 && r.answered == r.asked && r.send_failed == 0, "{r:?}");
    kassert!(hal::nmi::answers(k) == before + 1, "CPU {k} answered once");
    let regs = hal::nmi::regs(k).ok_or("no registers")?;
    let spin = rb_crash_spin as fn(u64) -> u64 as usize as u64;
    let at = symbolize(regs.frame.rip, false);
    // The loop calls the clock, so the NMI may land in a callee: the spin function is the interrupted
    // instruction or one of its callers in the walk.
    let in_spin = |a: u64, ret: bool| match symbols() {
        Some(_) => symbolize(a, ret).as_str().contains("rb_crash_spin"),
        None => a > spin && a - spin < 1024,
    };
    let found = regs.steps[..regs.nsteps].iter().any(|s| match *s {
        Step::Call(r) | Step::Guess(r) => in_spin(r, true),
        Step::Trap { rip, .. } => in_spin(rip, false),
        Step::None => false,
    });
    kassert!(found && !regs.user, "CPU {k} at {:#x} {:?}: rb_crash_spin not in its backtrace {:?}", regs.frame.rip, at.as_str(), &regs.steps[..regs.nsteps]);
    kassert!(regs.frame.rflags & (1 << 9) == 0, "it spins with interrupts off");
    kassert!(regs.nsteps >= 2 && matches!(regs.steps[0], Step::Trap { vector: 2, .. }), "{:?}", &regs.steps[..regs.nsteps]);
    for c in (0..hal::percpu::cpu_count()).filter(|&c| c != me) {
        kassert!(hal::nmi::answer(c) == hal::nmi::Answer::Sampled, "CPU {c}: {:?}", hal::nmi::answer(c));
    }
    klog::event!(Info, "crash", "ktest.sampled", cpu = k, rip = klog::Hex(regs.frame.rip), at = at.as_str(), frames = regs.nsteps, us = r.ns / 1000);
    Ok(())
}

/// Every other CPU answers an NMI sample with its registers; the one spinning with interrupts off is caught
/// inside its loop. Twice (the slots are reused).
fn nmi_sample() -> TestResult {
    let n = hal::percpu::cpu_count();
    if n < 2 {
        return ktest::skip("one CPU: nothing to sample");
    }
    if !hal::nmi::prepared() {
        return Err("the NMI slots were not prepared at boot".into());
    }
    let k = n - 1;
    STUCK.store(true, Ordering::Release);
    match hal::smp::call_on(k, rb_crash_spin, 0, time::now_ns, 50 * MS) {
        Err(hal::smp::CallError::Late) => {}
        other => {
            STUCK.store(false, Ordering::Release);
            return Err(format!("CPU {k} should still be spinning, call_on gave {other:?}"));
        }
    }
    let res = sample_round(k).and_then(|_| sample_round(k));
    unstick()?;
    res
}
ktest!("rb.crash.nmi_sample", Safe, nmi_sample);

/// A sample while NMI IPIs are paused sends nothing and leaves no slot waiting for an answer that never comes:
/// the next sample asks every CPU again and each answers (review INT-A1d core 1: the slots stayed ASKED and
/// those CPUs were never sampled again until reboot).
fn nmi_sample_after_paused() -> TestResult {
    let n = hal::percpu::cpu_count();
    if n < 2 {
        return ktest::skip("one CPU: nothing to sample");
    }
    if !hal::nmi::prepared() {
        return Err("the NMI slots were not prepared at boot".into());
    }
    let paused = {
        let _p = hal::apic::pause_nmi_ipis();
        sample_retrying_busy(10 * MS)?
    };
    kassert!(paused.asked == 0 && paused.answered == 0 && paused.send_failed == n - 1, "paused: {paused:?}");
    kassert!(paused.ns < SEC, "a sample with no NMI sent still waited {} ms", paused.ns / 1_000_000);
    for c in 0..n {
        kassert!(hal::nmi::answer(c) != hal::nmi::Answer::Silent, "CPU {c} left waiting after a failed send");
    }
    let r = sample_retrying_busy(2 * SEC)?;
    kassert!(r.asked == n - 1 && r.answered == r.asked && r.send_failed == 0, "after the pause: {r:?}");
    Ok(())
}
ktest!("rb.crash.nmi_sample_after_paused", Safe, nmi_sample_after_paused);

/// `sample_others`, retried while another sample (a late TLB round, the watchdog) runs.
fn sample_retrying_busy(wait_ns: u64) -> Result<hal::nmi::Round, alloc::string::String> {
    let end = time::now_ns() + 5 * SEC;
    loop {
        match hal::nmi::sample_others(time::now_ns, wait_ns) {
            Ok(r) => return Ok(r),
            Err(hal::nmi::NmiError::Busy) if time::now_ns() < end => sched::sleep_ns(MS),
            Err(e) => return Err(format!("sample: {e:?}")),
        }
    }
}

/// The record format on a heap copy of the live page (read only: the page itself is never written here) and this
/// boot's marker at its end.
fn record_format() -> TestResult {
    let Some((_, len)) = hal::panic::crash_page() else {
        return ktest::skip("no crash page in BootInfo");
    };
    let len = len as usize;
    let mut page = alloc::vec![0u8; len];
    kassert!(hal::panic::crash_page_read(0, &mut page));
    let m = Marker::parse(&page[len - record::MARKER..]).ok_or("no boot marker")?;
    kassert!(m.boot_id == my_boot_id(), "marker of another boot");
    let body = b"rb.crash.record_format\nline two\n";
    let h = Header { flags: flags::TEST, boot_id: my_boot_id(), cpu: hal::percpu::this_cpu() as u32, ..Header::default() };
    record::encode(&mut page, &h, body).ok_or("encode")?;
    match record::decode(&page) {
        Ok((got, b)) if b == body && got.flags == flags::TEST && got.boot_id == my_boot_id() => {}
        other => return Err(format!("decode: {other:?}")),
    }
    kassert!(Marker::parse(&page[len - record::MARKER..]) == Some(m), "a record leaves the marker alone");
    Ok(())
}
ktest!("rb.crash.record_format", Safe, record_format);

fn live_marker() -> Result<Marker, alloc::string::String> {
    let (_, len) = hal::panic::crash_page().ok_or("no crash page")?;
    let mut tail = [0u8; record::MARKER];
    kassert!(hal::panic::crash_page_read(len - record::MARKER as u64, &mut tail));
    Ok(Marker::parse(&tail).ok_or("no boot marker")?)
}

/// RB-11: a Reload names the next kernel's id in this boot's marker and takes it back when the jump does not
/// happen; the rest of the marker stays. Rerunnable: the marker ends as it was (a reset in between leaves a
/// marker of an earlier boot, which the next boot never reads an id from).
fn kernel_id_handoff() -> TestResult {
    if hal::panic::crash_page().is_none() {
        return ktest::skip("no crash page in BootInfo");
    }
    let before = live_marker()?;
    kassert!(before.next_kernel == 0, "a kernel id is left in the marker: {before:?}");
    kassert!(hand_on_kernel(0x5eed_0000_0000_0011), "the marker could not be written");
    let during = live_marker()?;
    kassert!(hand_on_kernel(0));
    let after = live_marker()?;
    kassert!(during == Marker { next_kernel: 0x5eed_0000_0000_0011, ..before }, "{during:?}");
    kassert!(after == before, "{after:?}");
    // A Reload whose jump returns (refused by the last checks) takes the id back.
    let inside = jump_handing_on(0x5eed_0000_0000_0022, live_marker)?;
    kassert!(inside == Marker { next_kernel: 0x5eed_0000_0000_0022, ..before }, "{inside:?}");
    let back = live_marker()?;
    kassert!(back == before, "a failed jump left the id: {back:?}");
    let my_reload = boot::boot_info().and_then(|(bi, _)| bi.reload()).map_or(0, |r| r.counter);
    kassert!(record::handed_kernel(MarkerFound::SameBoot, Some(&during), my_reload + 1) == Some(0x5eed_0000_0000_0011));
    klog::event!(Info, "crash", "ktest.kernel_id", running = running_kernel_id().map_or(0, |k| k), reload = my_reload);
    Ok(())
}
ktest!("rb.crash.kernel_id_handoff", Safe, kernel_id_handoff);

/// A test record into the real crash page and back, then the page as it was (QEMU only: on the Mini-PC the
/// page may hold a real record, and a panic on another CPU in between would be overwritten by the restore).
fn record_page() -> TestResult {
    let Some((_, len)) = hal::panic::crash_page() else {
        return ktest::skip("no crash page in BootInfo");
    };
    if previous().is_some_and(|p| !p.reported_before) {
        return ktest::skip("the page holds the crash record the boot before left (kept as it is)");
    }
    let len = len as usize;
    let mut saved = alloc::vec![0u8; len];
    kassert!(hal::panic::crash_page_read(0, &mut saved));
    let mut page = saved.clone();
    let body = b"rb.crash.record_page\nline two\n";
    let h = Header { flags: flags::TEST, boot_id: my_boot_id(), cpu: hal::percpu::this_cpu() as u32, ..Header::default() };
    record::encode(&mut page, &h, body).ok_or("encode")?;
    kassert!(hal::panic::crash_page_write(0, &page));
    let mut back = alloc::vec![0u8; len];
    hal::panic::crash_page_read(0, &mut back);
    let res = match record::decode(&back) {
        Ok((got, b)) if b == body && got.flags == flags::TEST && got.boot_id == my_boot_id() => Ok(()),
        other => Err(format!("read back: {other:?}")),
    };
    hal::panic::crash_page_write(0, &saved);
    res
}
ktest!("rb.crash.record_page", QemuOnly, record_page);

fn log_tail() -> TestResult {
    let id = time::now_ns();
    klog::event!(Info, "crash", "ktest.tail", id = id);
    let mut buf = [0u8; 1024];
    let n = klog::try_tail_text(&mut buf).ok_or("ring busy")?;
    let s = core::str::from_utf8(&buf[..n]).map_err(|_| "not UTF-8")?;
    let want = format!("I crash ktest.tail id={id}\n");
    kassert!(s.contains(&want), "tail ends with {:?}", &s[s.len().saturating_sub(120)..]);
    Ok(())
}
ktest!("rb.crash.log_tail", Safe, log_tail);

// --- The real panic (QemuOnly, and the MCP tool `crash_test`) ---

#[inline(never)]
fn rb_crash_level_three(kind: u64) -> u64 {
    if kind == 1 {
        // A load from a non-canonical address: #GP in kernel mode that no hook claims, so `hal::idt` panics with
        // the trap frame (the fatal-exception case: the walk must cross the trap frame).
        return hal::idt::probe_load(hal::idt::ProbeTarget::NonCanonical).unwrap_or(0);
    }
    panic!("rb.crash forced kind=panic marker={:x}", hal::cpu::rdtsc() & 0xFFFF_FFFF);
}

#[inline(never)]
fn rb_crash_level_two(kind: u64) -> u64 {
    let v = rb_crash_level_three(kind);
    core::hint::black_box(v) + 1
}

#[inline(never)]
fn rb_crash_level_one(kind: u64) -> u64 {
    let v = rb_crash_level_two(kind);
    core::hint::black_box(v) + 1
}

/// Panics through `rb_crash_level_one/two/three`: kind 0 a Rust panic, 1 a kernel #GP.
pub fn force(kind: u64) -> u64 {
    let v = rb_crash_level_one(kind);
    core::hint::black_box(v)
}

/// Boot 1 (CONFIG `rb_crash_panic=panic|fault`): crashes. Boot 2 (after the warm reset, same CONFIG): checks
/// what boot 1 left and that this boot logged it.
fn forced() -> TestResult {
    let kind = match boot::config("rb_crash_panic").as_deref() {
        Some("panic") => 0,
        Some("fault") => 1,
        _ => return ktest::skip("CONFIG rb_crash_panic=panic|fault not set"),
    };
    let sign = if kind == 0 { "rb.crash forced kind=panic" } else { ": kernel general protection (#GP) at rip" };
    match previous() {
        Some(p) if !p.reported_before && p.text.contains(sign) => {
            let r = check_previous(p, kind);
            // The scenario's listener should get this boot's `crash previous` too (sent from the mirror's buffer).
            wait_for_mirror()?;
            r
        }
        _ => {
            wait_for_mirror()?;
            klog::event!(Warn, "crash", "ktest.forcing", kind = kind; "a deliberate kernel panic follows");
            force(kind);
            Err("the forced panic returned".into())
        }
    }
}
ktest!("rb.crash.forced", QemuOnly, forced);

/// With CONFIG `mirror=` the scenario wants the report on its listener: wait (up to 60 s) until klogd says the
/// mirror is live (DHCP done), so the panic's lines can leave through its emergency path.
fn wait_for_mirror() -> TestResult {
    if boot::config("mirror").is_none() {
        return Ok(());
    }
    let end = time::now_ns() + 60 * SEC;
    while !klog::tail(2000).iter().any(|r| r.subsystem == "klog" && r.name == "mirror.live") {
        kassert!(time::now_ns() < end, "the mirror did not go live within 60 s");
        sched::sleep_ns(50 * MS);
    }
    // What waited in the mirror's buffer goes out first (klogd drains every few ms).
    sched::sleep_ns(500 * MS);
    Ok(())
}

fn check_previous(p: &Previous, kind: u64) -> TestResult {
    let t = &p.text;
    let line_with = |s: &str| t.lines().position(|l| l.contains(s));
    kassert!(!t.contains("no symbol table"), "boot 1 had no symbol table:\n{t}");
    let (three, two, one) = (line_with("rb_crash_level_three"), line_with("rb_crash_level_two"), line_with("rb_crash_level_one"));
    kassert!(matches!((three, two, one), (Some(a), Some(b), Some(c)) if a < b && b < c), "levels three, two, one in order:\n{t}");
    if kind == 1 {
        kassert!(p.header.flags & flags::EXCEPTION != 0, "flagged as an exception");
        let trap = line_with(" -- general protection (#GP) at ").ok_or_else(|| format!("no trap line:\n{t}"))?;
        kassert!(two.is_some_and(|b| trap < b), "the trap comes before level two:\n{t}");
    }
    kassert!(p.header.cpus_answered == p.header.cpus_asked, "every other CPU answered: {:?}", p.header);
    for c in (0..p.header.cpus_asked as usize + 1).filter(|&c| c != p.header.cpu as usize) {
        kassert!(line_with(&format!("cpu {c}: rip ")).or(line_with(&format!("cpu {c}: like cpu "))).is_some(), "registers of CPU {c}:\n{t}");
    }
    kassert!(line_with("log:").is_some() && line_with("W crash ktest.forcing ").is_some(), "log tail up to the panic:\n{t}");
    kassert!(p.header.flags & flags::CUT == 0, "the record was cut:\n{t}");
    kassert!(matches!(marker(), Some((MarkerFound::Kept, _))), "the boot marker survived the warm reset: {:?}", marker());
    let logged = klog::tail(4000).iter().any(|r| r.subsystem == "crash" && r.name == "previous");
    kassert!(logged, "crash.previous is in this boot's log");
    klog::event!(Info, "crash", "ktest.previous_ok", kind = kind, lines = t.lines().count(), cpus = p.header.cpus_asked + 1);
    Ok(())
}
