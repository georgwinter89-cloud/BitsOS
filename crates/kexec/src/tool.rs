//! MCP tools `reload` (answer first, then into the new kernel) and `reload_info` (what this kernel was handed).

use crate::stage::{self, StageError, Staged, Summary};
use crate::url::{self, Bundle};
use crate::boot;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

/// One Reload at a time: from the first fetch until the jump (or until it is refused).
static BUSY: AtomicBool = AtomicBool::new(false);
/// Set right before the Reload's quiesce: drivers whose Quiesce differs between Reload and reboot read it (the
/// watchdog stops for a Reload and stays armed for a reboot, RB-3).
static JUMPING: AtomicBool = AtomicBool::new(false);
/// Called (no lock, no allocation) when the jump returned after the quiesce: RB-3's watchdog arms itself again
/// so the machine, now without network, gets reset.
static JUMP_FAILED_HOOK: ksync::OnceRef<fn()> = ksync::OnceRef::new();

/// True from the Reload's quiesce on (a Quiesce asks: Reload or reboot?).
pub fn reload_jumping() -> bool {
    JUMPING.load(Ordering::Acquire)
}

/// Installs the hook a failed jump calls (the first call wins).
pub fn set_jump_failed_hook(f: &'static fn()) -> bool {
    JUMP_FAILED_HOOK.set(f)
}

/// One Reload or install at a time (an install's stick writes and a Reload's quiesce never overlap). The private
/// field keeps every `Busy` coming from `take`: a stray value would clear the flag when it drops.
pub(crate) struct Busy(());
impl Busy {
    pub(crate) fn take() -> Option<Busy> {
        // Not `then_some(Busy(()))`: its argument is built (and dropped, clearing the flag) also when the flag
        // was already set, so a refused caller would let the next one in (RB-6 fixer, found by 50-install.sh).
        BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok().then(|| Busy(()))
    }
}
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

pub(crate) fn opt_str<'a>(args: &'a Json, k: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or_else(|| ToolError::BadArgument(format!("{k} must be a string"))),
    }
}

pub(crate) fn opt_bool(args: &Json, k: &str) -> Result<Option<bool>, ToolError> {
    match args.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::BadArgument(format!("{k} must be true or false"))),
    }
}

/// Arguments to a bundle; refusals for scheme, LAN and form.
pub fn bundle_from(args: &Json) -> Result<Bundle, ToolError> {
    let url = opt_str(args, "url")?.ok_or_else(|| ToolError::BadArgument(String::from("url is missing")))?;
    let initrd = opt_str(args, "initrd")?;
    url::bundle(url, initrd).map_err(|e| {
        klog::event!(Warn, "kexec", "reload.refused", url = url, reason = e.describe());
        ToolError::Refused(e.describe())
    })
}

pub fn describe(s: &Summary) -> String {
    let initrd = match (s.initrd, s.initrd_kept) {
        (Some((p, n)), _) => format!("initramfs {n} bytes at {p:#x} (fnv {:#018x})", s.initrd_fnv),
        (None, true) => String::from("initramfs: the running one, unchanged"),
        (None, false) => String::from("no initramfs"),
    };
    format!(
        "kernel {} ({} bytes, image {} bytes at {:#x}, entry {:#x}); {initrd}; page tables {} pages; BootInfo {} of {} bytes; \
         TSC {} Hz handed on; Reload counter -> {}; the new kernel has {} MiB free ({} MiB in its largest piece)",
        s.kernel_url,
        s.kernel_file_bytes,
        s.span,
        s.image_phys,
        s.entry,
        s.table_pages,
        s.bootinfo_bytes,
        s.bootinfo_capacity,
        s.tsc_hz,
        s.counter,
        s.new_free >> 20,
        s.new_largest >> 20
    )
}

/// Masks every I/O APIC pin (spec/core.md §2.4 "IOAPIC quiet"). Runs with interrupts still on: `mask_all`
/// takes the I/O APIC's register lock, which a preempted holder must be able to release.
fn quiet_ioapics() -> u32 {
    let Some(st) = acpi::irq::state() else { return 0 };
    let mut masked = 0;
    for io in &st.ioapics {
        masked += io.apic.mask_all().masked;
    }
    masked
}

/// How long the other CPUs get to stop: the park IPI, then one NMI (generous under a hypervisor, where timing is
/// never judged; an AP answers in microseconds when it has interrupts on).
/// RB-quiesce: shared with the reboot and power-off park (`hal::kexec_stub::park_for_shutdown`).
fn stop_options() -> hal::kexec_stub::StopOptions {
    hal::kexec_stub::live_stop_options(time::now_ns)
}

/// Whether the Reload may go on after the stop (pure; host-tested). A CPU that had to be parked by NMI had
/// interrupts off for a second or more (a kernel bug, e.g. spinning on an `IrqSpinLock`): it may hold any lock,
/// and the I/O APIC masking and the quiesce that follow would then wait for it forever after the point of no
/// return. Refused instead: the CPUs are released and this kernel goes on with its devices and MCP, which is
/// better than a silent hang (reboot by hand or the watchdog then).
pub fn stop_verdict(r: &hal::kexec_stub::StopReport) -> Result<(), String> {
    if hal::kexec_stub::lock_safe(r) {
        return Ok(());
    }
    Err(format!(
        "{} CPU(s) (first: CPU {}) had interrupts off and answered only an NMI: a stuck kernel path that may hold a lock the quiesce needs",
        r.by_nmi,
        r.first_nmi_cpu.map_or(String::from("?"), |c| format!("{c}"))
    ))
}

/// The APIC IDs of the MADT's enabled CPUs that are not online (they failed to come up): they get INIT too.
/// Allocates: runs before the stop.
fn unused_apic_ids() -> alloc::vec::Vec<u32> {
    let Some(madt) = acpi::tables().and_then(|t| t.madt.as_ref()) else { return alloc::vec::Vec::new() };
    madt.cpus.iter().filter(|c| c.enabled && hal::smp::cpu_of_apic(c.apic_id).is_none()).map(|c| c.apic_id).collect()
}

/// The functions enabled with bus master (`pci::dma_enabled`), taken before the stop (locks and heap): after
/// `quiesce_all` each gets `disable_device` again, which catches a function no registered `Quiesce` covers
/// (spec §2.4 "clear bus master on what the kernel enabled"). Harmless for those a driver quiesced already.
fn dma_functions() -> alloc::vec::Vec<pci::PciFunction> {
    let on = pci::dma_enabled();
    pci::devices().into_iter().filter(|f| on.contains(&f.loc())).collect()
}

/// What the bus-master sweep after `quiesce_all` did.
#[derive(Default)]
struct Sweep {
    /// Bus master was still on (no driver's Quiesce turned it off) and is off now.
    left_on: usize,
    /// Untouched (R8d) or mass storage not bound to NVMe (R8a): nothing written.
    refused: usize,
    /// A configuration lock stayed busy: an access was skipped.
    incomplete: usize,
}

fn sweep_bus_master(fns: &[pci::PciFunction]) -> Sweep {
    let mut s = Sweep::default();
    for f in fns {
        let d = f.disable_device();
        s.left_on += (d.bus_master_was_on && !d.refused) as usize;
        s.refused += d.refused as usize;
        s.incomplete += d.incomplete as usize;
    }
    s
}

/// After the answer (MCP has closed the connection and waited its grace, igc drains its TX ring in the
/// quiesce): every check of the jump first and then the other CPUs parked (RB-5b), so a refusal of either leaves
/// this kernel running with its network and its CPUs; then the I/O APIC pins, the parked CPUs' LAPICs quiet and
/// halted, interrupts off, the console mirror's last lines out (before any NIC stops), every device, bus master
/// off on whatever is left, this CPU's LAPIC and the 8259s quiet, LINT0 back to virtual wire, INIT to every
/// other CPU, and into the new kernel.
pub(crate) fn go(st: Staged) {
    // RB-5b: only the boot CPU may stop the others; the answer may have been sent from any CPU.
    if !sched::pin_current(Some(0)) {
        klog::event!(Error, "kexec", "reload.failed", reload = st.summary.counter, error = "could not move to CPU 0", quiesced = false; "this kernel goes on");
        return;
    }
    go_on_boot_cpu(st);
    // Only reached when the Reload was refused: the thread may run anywhere again.
    sched::pin_current(None);
}

fn go_on_boot_cpu(mut st: Staged) {
    let counter = st.summary.counter;
    if let Err(e) = st.check() {
        klog::event!(Error, "kexec", "reload.failed", reload = counter, error = format!("{e:?}"), quiesced = false; "refused before anything was stopped: this kernel goes on");
        return;
    }
    // RB-10: the stick's CONFIG.TXT and markers as they are now; reads the stick in a helper with a deadline (up
    // to 4 s) and allocates: before the stop. INT-W13: also before the write-back below, which leaves /data clean;
    // programs run on while the stick is read, and a write in that gap made the next kernel replay /data's journal
    // (fs/51-data-reload.sh). The read goes through the page cache, so a key settingsd wrote just before is in.
    crate::refresh::apply(&mut st);
    // RB-8: the journal (bitsjournal) writes this kernel's last records to /data first, so the write-back below
    // makes them durable; bounded (B4), skipped at once without a journal, a timeout is an event and goes on.
    let _ = klog::journal::wait_drained("reload", klog::journal::DRAIN_NS, &sched::sleep_ns);
    // FS-7 change request 1: every file's dirty pages, the stick's entries, FSInfo and its cache flush, while
    // threads, interrupts and the disks still run (the background pass would make them durable only within 5 s).
    let synced = vfs::writeback::sync_all();
    if synced.failed > 0 {
        klog::event!(Warn, "kexec", "reload.synced", reload = counter, flushed = synced.flushed, failed = synced.failed;
            "some files or file systems could not be written back before the Reload");
    } else {
        klog::event!(Info, "kexec", "reload.synced", reload = counter, flushed = synced.flushed, failed = synced.failed);
    }
    // Everything that allocates or takes a lock happens before the stop.
    let unused = unused_apic_ids();
    let dma_fns = dma_functions();
    let (stray0, deferred0) = hal::kexec_stub::stray_and_deferred();
    let stopped = match hal::kexec_stub::stop_others(&stop_options()) {
        Ok(s) => s,
        Err(e) => {
            let (stray, deferred) = hal::kexec_stub::stray_and_deferred();
            klog::event!(Error, "kexec", "reload.failed", reload = counter, error = format!("{e:?}"), quiesced = false,
                guard_deferred = deferred.wrapping_sub(deferred0), stray_ipis = stray.wrapping_sub(stray0);
                "the other CPUs could not be stopped; those that were run again and this kernel goes on");
            return;
        }
    };
    let cpus = stopped.report();
    if let Err(why) = stop_verdict(&cpus) {
        // Released first (the drop), so the event below cannot wait on a lock the stuck CPU's interrupted path holds
        // while it is parked.
        drop(stopped);
        klog::event!(Error, "kexec", "reload.failed", reload = counter, error = why.as_str(), quiesced = false,
            cpus_stopped = cpus.others, by_irq = cpus.by_irq, by_nmi = cpus.by_nmi, park_ipis = cpus.ipis;
            "the other CPUs were released and this kernel goes on");
        return;
    }
    let (stray, deferred) = hal::kexec_stub::stray_and_deferred();
    let (stray, deferred) = (stray.wrapping_sub(stray0), deferred.wrapping_sub(deferred0));
    // From here on no plain `event!` (try_event never waits).
    if cpus.ipis as usize > cpus.others || deferred > 0 {
        klog::try_event!(Warn, "kexec", "reload.park_retried", reload = counter, park_ipis = cpus.ipis, cpus_stopped = cpus.others,
            guard_deferred = deferred, stray_ipis = stray; "some CPUs needed the park interrupt again");
    }
    let pins = quiet_ioapics();
    let quiet = stopped.quiet(1_000_000_000);
    hal::cpu::disable_interrupts();
    // spec §2.4: the mirror's last lines go out before the NIC they travel on stops (quiesce_all runs newest
    // first, which puts the mirror first only by accident of registration order). A second quiesce is a no-op.
    let mirror = devmodel::quiesce_matching("mirror");
    JUMPING.store(true, Ordering::Release);
    let devices = devmodel::quiesce_all();
    let sweep = sweep_bus_master(&dma_fns);
    let lapic = hal::kexec_stub::quiet_local_apic();
    hal::apic::quiet_legacy_pic();
    let extint = hal::kexec_stub::virtual_wire();
    // spec §6.1: 10 ms after INIT, the delay the new kernel's bring-up also gives before its SIPI.
    let init = quiet.send_init(time::now_ns, 10_000_000, &unused);
    if sweep.incomplete > 0 {
        klog::try_event!(Warn, "kexec", "reload.bus_master_incomplete", reload = counter, functions = sweep.incomplete; "a PCI configuration lock stayed busy: bus master may still be on");
    }
    if !lapic.timer_masked {
        klog::try_event!(Warn, "kexec", "reload.lapic_timer_live"; "xAPIC mode with the LAPIC timer unmasked: it cannot be stopped here (change request hal::apic::quiet_local); the new kernel masks it in its LAPIC set-up before it enables interrupts");
    }
    klog::try_event!(
        Info,
        "kexec",
        "reload.jump",
        reload = counter,
        devices = devices.join(","),
        ioapic_pins_masked = pins,
        lapic_timer_masked = lapic.timer_masked,
        lapic_timer_stopped = lapic.timer_stopped,
        lint0_masked = lapic.lint0_masked,
        lvts_masked = lapic.other_lvts_masked,
        cpus_stopped = cpus.others,
        by_irq = cpus.by_irq,
        by_nmi = cpus.by_nmi,
        stop_us = cpus.ns / 1000,
        halted = init.halted,
        init_sent = init.init_sent,
        init_failed = init.init_failed,
        init_unused = init.unused_sent,
        init_unused_failed = init.unused_failed,
        init_refused = init.refused,
        park_ipis = cpus.ipis,
        guard_deferred = deferred,
        stray_ipis = stray,
        mirror_first = mirror,
        bus_master_swept = sweep.left_on,
        bus_master_refused = sweep.refused,
        lint0_extint = extint,
        entry = klog::Hex(st.summary.entry)
    );
    klog::drain_deferred(usize::MAX);
    let e = st.jump();
    klog::try_event!(Error, "kexec", "reload.failed", reload = counter, error = format!("{e:?}"), quiesced = true; "the devices are quiesced and the other CPUs halted: the machine stays without network until it is reset (watchdog, reboot by hand)");
    // Not jumping any more: a later quiesce (a reboot by hand) is a reboot's, not a Reload's.
    JUMPING.store(false, Ordering::Release);
    if let Some(hook) = JUMP_FAILED_HOOK.get() {
        hook();
    }
}

fn reload(ctx: &ToolContext, args: &Json) -> ToolResult {
    let b = bundle_from(args)?;
    let dry = opt_bool(args, "dry_run")?.unwrap_or(false);
    // A refusal that needs no fetch: the CPU this runs on cannot stop the others.
    if !dry {
        // NotBootCpu is no refusal here: `go` moves to CPU 0 before it checks again.
        match hal::kexec_stub::check_cpus() {
            Ok(()) | Err(hal::kexec_stub::StubError::NotBootCpu { .. }) => {}
            Err(e) => return Err(ToolError::Refused(format!("{e:?}: the other CPUs cannot be stopped (dry_run works)"))),
        }
    }
    let Some(busy) = Busy::take() else {
        return Err(ToolError::Refused(String::from("a Reload, an install_kernel or the boot confirmation's stick write is in progress; try again in a few seconds")));
    };
    klog::event!(Info, "kexec", "reload.requested", kernel = b.kernel.as_str(), initrd = b.initrd.as_deref().unwrap_or("(running one)"), dry_run = dry);
    let st = stage::stage(&b, ctx.deadline_ns).map_err(|e| {
        klog::event!(Warn, "kexec", "reload.refused", reason = e.text());
        match e {
            StageError::Refused(t) | StageError::Bad(t) => ToolError::Refused(t),
            StageError::Fetch(t) | StageError::NotReady(t) => ToolError::Failed(t),
        }
    })?;
    let text = describe(&st.summary);
    klog::event!(
        Info,
        "kexec",
        "reload.staged",
        reload = st.summary.counter,
        span = st.summary.span,
        initrd_bytes = st.summary.initrd.map(|i| i.1).unwrap_or(0),
        initrd_fnv = klog::Hex(st.summary.initrd_fnv),
        tables = st.summary.table_pages,
        bootinfo_bytes = st.summary.bootinfo_bytes,
        dry_run = dry
    );
    if dry {
        // RB-10: what the jump would hand on from the stick now (the real Reload reads it again at the jump).
        let cfg = crate::refresh::dry_run(&st);
        return Ok(vec![ToolOutput::Text(format!("reload (dry run, nothing happens): {text}; {cfg}"))]);
    }
    if !ctx.after_answer(Box::new(move || {
        let _busy = busy;
        go(st)
    })) {
        return Err(ToolError::Failed(String::from(
            "reload happens only after its answer, and this call has no answer to wait for (called outside the MCP server, or past its deadline)",
        )));
    }
    let mut note = String::new();
    if let Some((g, covered, need)) = crate::install::confirm::pending() {
        note = format!(
            " Note: this kernel is the one try of stick generation {g} and is not confirmed yet (MCP answers over {covered} s of {need} s): after this Reload it stays unconfirmed, so the next reboot starts the older confirmed generation."
        );
        klog::event!(Warn, "kexec", "reload.unconfirmed_generation", generation = g, covered_s = covered, need_s = need);
    }
    Ok(vec![ToolOutput::Text(format!(
        "reloading once this answer is out: {text}. CONFIG.TXT and the generation's markers are read from the boot stick again right before the jump. MCP answers again from the new kernel in seconds (QEMU) to ~15 s (Mini-PC).{note}"
    ))])
}

mcp_tool!(
    name: "reload",
    owner: "RB",
    safety: Change,
    description: "Starts a new kernel without a reboot (kexec-like Reload): fetches the bundle over http from the LAN, checks it, answers, stops the other CPUs (they wait for the new kernel), quiesces every device and jumps; CONFIG, the stick record and the crash page are handed on, the Reload counter rises. url: a directory ending in / (fetches KERNEL and INITRD below it, the stick's SYS/<n>/ layout) or the kernel file; initrd: the initramfs URL (default for a file url: keep the running one, which is refused once the kernel released it after unpacking it into tmpfs). dry_run: fetch and check only.",
    schema: r#"{"type":"object","properties":{"url":{"type":"string","description":"http://<LAN IPv4>[:port]/dir/ (KERNEL + INITRD) or .../KERNEL"},"initrd":{"type":"string","description":"http URL of the initramfs (optional)"},"dry_run":{"type":"boolean","description":"fetch and check, but do not reload"}},"required":["url"]}"#,
    handler: reload,
);

fn reload_info(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    let mut t = String::new();
    match boot::facts() {
        Some(f) => t.push_str(&format!(
            "reload counter {} (0 = started by the firmware)\nTSC handed over: {} Hz (measured by the kernel of Reload {})\ninitramfs {} bytes at {:#x}, fnv {:#018x}\nCONFIG {} bytes, BootInfo {} bytes\n",
            f.counter, f.tsc_hz_handed, f.tsc_calibrated_at_reload, f.initrd_bytes, f.initrd_phys, f.initrd_fnv, f.config_bytes, f.bootinfo_bytes
        )),
        None => t.push_str("unknown: the kernel did not hand its BootInfo to kexec\n"),
    }
    t.push_str(&format!("TSC now {} Hz\n", time::tsc_hz()));
    t.push_str(&format!("boot confirmation (RB-6): {}\n", crate::install::confirm::describe(&crate::install::confirm::status())));
    t.push_str(&format!("boot stick: {}\n", crate::install::stick_summary()));
    if let Some((bi, phys)) = boot::boot_info() {
        let stub = bi.reload_stub().map(|s| s.phys).unwrap_or(0);
        let ready = match stage::stub_ready(stub) {
            Ok(()) => String::from("ready"),
            Err(e) => format!("not ready: {e}"),
        };
        t.push_str(&format!(
            "BootInfo at {phys:#x}, KEEP block {} bytes; stub page {stub:#x}: {ready}\nfree for a Reload now: {} MiB (beyond the {} MiB the running kernel keeps)\n",
            crate::handover::capacity(&bi),
            stage::budget() >> 20,
            stage::RESERVE >> 20
        ));
    }
    Ok(vec![ToolOutput::Text(t)])
}

mcp_tool!(
    name: "reload_info",
    owner: "RB",
    safety: Read,
    description: "What this kernel was handed by the loader or a Reload: the Reload counter, the TSC rate, the initramfs (size and FNV-1a hash), and whether a Reload is possible now.",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: reload_info,
);
