//! The BitsOS V2 kernel binary (desk): entry, start order, panic. Everything else lives in the crates.
//!
//! Entry contract with bitsboot (BootInfo crate, V2-PLAN §7): long mode, interrupts off, CR3 = bitsboot's
//! tables (kernel image, direct map of RAM, framebuffer at VMAP_BASE, stub page), rdi = BootInfo virtual
//! address (direct map), rsi = its physical address.
#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod earlycon;
mod bt_edid;
mod follow;
mod contracts;
mod smoke;

// Every crate is linked so its registrations (ktests, syscalls, tools, devnodes, drivers) are present.
use acpi as _;
use bitsvol as _;
use crypt as _;
use fch_wdt as _;
use fs_ext4 as _;
use fs_fat as _;
use hda as _;
use igc as _;
use kexec as _;
use nvme as _;
use platform as _;
use rtw89 as _;
use seccomp as _;
use snapshots as _;
use snd as _;
use tpm as _;
use usb_hid as _;
use usb_msd as _;
use virtio_net as _;
use wifi as _;
use xhci as _;
use amdgpu as _;
use block as _;
use klogd as _;
use sched_mcp as _;
use policy_mcp as _;
// FS-1: the file syscalls, devfs/proc/sys content (vfs-sys sits above proc and syscall).
use vfs_sys as _;
// IN-3: the console monitor's `ps`/`run` (above proc).
use console_cmds as _;

use alloc::boxed::Box;
use alloc::vec::Vec;
use bootinfo::{BootInfo, memtype};
use console::Console;

#[global_allocator]
static ALLOC: mm_core::GlobalHeap = mm_core::GlobalHeap;

core::arch::global_asm!(
    r#"
    .section .text._start, "ax"
    .global _start
_start:
    cli
    cld
    lea boot_stack_top(%rip), %rsp
    xor %ebp, %ebp
    call kernel_entry
    ud2

    .section .bss.boot_stack, "aw", @nobits
    .p2align 4
boot_stack_bottom:
    .skip 131072
boot_stack_top:
"#,
    options(att_syntax)
);

/// Bytes of the first heap chunk, taken before the frame allocator exists.
const SEED_HEAP: u64 = 8 << 20;

#[unsafe(no_mangle)]
extern "C" fn kernel_entry(bootinfo_virt: u64, bootinfo_phys: u64) -> ! {
    // Our own GDT/IDT first: bitsboot's tables map neither the firmware's IDT nor its handlers, so any fault
    // before this point would triple-fault.
    hal::gdt::init();
    hal::idt::init();
    hal::serial::init();

    // SAFETY: bitsboot passes the direct-map address of the KEEP pages holding BootInfo; we read the header
    // first, then exactly `total_size` bytes. KEEP pages are never freed, so the slice lives forever.
    let bi: BootInfo<'static> = unsafe {
        let head = core::slice::from_raw_parts(bootinfo_virt as *const u8, bootinfo::HEADER_SIZE);
        let total = BootInfo::peek_total_size(head).unwrap_or_else(|| early_fail("BootInfo: bad header"));
        let all = core::slice::from_raw_parts(bootinfo_virt as *const u8, total);
        BootInfo::parse(all).unwrap_or_else(|_| early_fail("BootInfo: parse error"))
    };
    // Everything COM1 gets also goes to the screen until the console is up (on bare metal COM1 is not used).
    earlycon::init(&bi);
    hal::serial::write_bytes(b"\nBitsOS V2 kernel: entry\n");
    stage(if hal::serial::present() { "BootInfo read; early screen console on; COM1 in use" } else { "BootInfo read; early screen console on; COM1 not used" });

    // CPU basics on the BSP.
    hal::percpu::init_bsp();
    let features = hal::cpu::detect();
    hal::cpu::init_bsp(&features);
    hal::pat::init_this_cpu();
    hal::entry::init_syscall();
    stage("CPU set-up on the boot CPU done");
    hal::apic::quiet_legacy_pic();

    // Memory: seed the heap from the largest free range, then the frame allocator over the rest.
    let mm = bi.memory_map().unwrap_or_else(|| early_fail("BootInfo: no memory map"));
    let mut largest = (0u64, 0u64);
    for d in mm.iter().filter(|d| memtype::is_free_after_handover(d.ty)) {
        let (s, e) = (d.phys.max(0x10_0000), d.end());
        if e > s && e - s > largest.1 - largest.0 {
            largest = (s, e);
        }
    }
    if largest.1 - largest.0 < 4 * SEED_HEAP {
        early_fail("not enough free memory");
    }
    let seed = (largest.0 + 0x1F_FFFF) & !0x1F_FFFF;
    mm_core::HEAP.seed(hal::phys_to_virt(seed), SEED_HEAP);
    // Locks taken before this point (the seed's own) track without allocating.
    ksync::heap_ready();
    // SAFETY: `mm` is this boot's firmware map from bitsboot, used once before any frame is handed out; its free
    // ranges minus the seed are unused, direct-mapped RAM.
    let frames: &'static mm_core::BuddyFrames =
        Box::leak(Box::new(unsafe { mm_core::BuddyFrames::from_memory_map(&mm, &[(seed, seed + SEED_HEAP)]) }));
    mm_core::set_frame_allocator(frames);
    stage("frame allocator ready");
    klog::heap_ready();
    config::init(bi.config());
    // RB-5a: Reload hands this BootInfo on (its KEEP block is rewritten in place for the next kernel). Before
    // MM-2's switch below, so kexec records the boot page-table root with the reload stub page.
    kexec::set_boot_info(bi, bootinfo_phys);
    klog::configure(&config::get);
    configure_serial();

    // MM-2: the kernel's own page tables (direct map by MTRR type, W^X per section), then CR3 switch.
    unsafe extern "C" {
        static __text_start: u8;
        static __text_end: u8;
        static __rodata_start: u8;
        static __rodata_end: u8;
        static __data_start: u8;
        static __kernel_end: u8;
    }
    let sections = ::mm::kmap::KernelSections {
        text: (&raw const __text_start as u64, &raw const __text_end as u64),
        rodata: (&raw const __rodata_start as u64, &raw const __rodata_end as u64),
        data: (&raw const __data_start as u64, &raw const __kernel_end as u64),
    };
    if let Err(e) = ::mm::kernel_tables::switch_to_own(&bi, Some(sections)) {
        klog::event!(Error, "kernel", "paging.own_tables_failed", error = alloc::format!("{e:?}"));
    }

    let ram: Vec<(u64, u64)> = mm.iter().filter(|d| memtype::is_ram(d.ty)).map(|d| (d.phys, d.end())).collect();
    let vmap_start = bi.framebuffer().map(|f| f.virt + f.size).unwrap_or(bootinfo::layout::VMAP_BASE);
    hal::mmio::init(vmap_start, ram, mm_core::alloc_table_frame);
    if hal::paging::populate_kernel_half(hal::paging::current_root(), &mut mm_core::alloc_table_frame).is_err() {
        early_fail("no memory for kernel page tables");
    }

    stage("own page tables done");
    time::init(bi.tsc().map(|t| t.calibrated_hz).unwrap_or(0));
    klog::event!(Info, "kernel", "start", version = env!("CARGO_PKG_VERSION"), generation = bi.generation().map(|g| g.number).unwrap_or(0), reload = bi.reload().map(|r| r.counter).unwrap_or(0), gen_reason = bi.generation().map(|g| g.reason).unwrap_or(0), gen_skipped = bi.generation().map(|g| g.skipped).unwrap_or(0), gen_flags = klog::Hex(bi.generation().map(|g| g.flags as u64).unwrap_or(0)));
    // The Mini-PC has no COM1: bitsboot's own log lines reach the mirror only through klog.
    if let Some(log) = bi.loader_log() {
        for line in log.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            klog::event!(Info, "boot", "loader.line", text = alloc::string::String::from_utf8_lossy(line).into_owned());
        }
    }
    // ACPI tables (CP-2): the RSDP and the memory map come from BootInfo; CP-3's IRQ set-up needs them.
    stage("clock calibrated");
    acpi::init(&bi);
    stage("ACPI tables read");

    // R10: x86-64-v3 is the minimum (bitsboot checks too; this is the kernel's own line of defence).
    let missing = hal::cpu::missing_v3_features();
    if !missing.is_empty() {
        klog::event!(Error, "kernel", "cpu.too_old", missing = alloc::format!("{missing:?}"));
        panic!("CPU does not meet x86-64-v3 (R10): missing {:?}", missing);
    }
    // LAPIC, I/O APICs, 8259s masked, then sti for the boot thread (CP-3); logs its own errors.
    let _ = acpi::irq::init(&config::get);
    stage("interrupt controllers set up");
    let st = frames_stats();
    klog::event!(Info, "mm", "frames", total_mb = st.0 / (1 << 20), free_mb = st.1 / (1 << 20), nx = features.nx, smep = features.smep, smap = features.smap, avx512 = features.avx512f);

    stage("clock, ACPI and interrupts done");
    sched::init();
    stage("scheduler ready");

    // Picture: the GOP framebuffer as the first display (DP-1 replaces the G0 back end).
    if let Some(fb) = bi.framebuffer()
        && let Some(dev) = gopfb::GopFb::from_bootinfo(&fb)
    {
        let dev: &'static gopfb::GopFb = Box::leak(Box::new(dev));
        // DP-3 CR2: the monitor's EDID rate first (when the loader handed one on), then CONFIG display_refresh=,
        // which wins (apply_edid keeps a CONFIG rate either way).
        if let Some(edid) = bi.edid() {
            dev.apply_edid(edid);
        }
        dev.apply_config(&config::get);
        display::set_device(dev);
        stage("display device ready; console takes over the screen");
        earlycon::stop();
        // D1: without a port in use nothing reads klog's serial lines any more (the console has the screen).
        klog::set_serial(hal::serial::has_output());
        console::init();
        // GX-4 CR 1: the console, fbdev and the GopFb follow a display mode switch.
        follow::install();
        // IN-3/IN-quiet: CONFIG console_font, console_scrollback, console_log; attaches the console's log sink
        // (boot lines replayed at that level).
        console::configure(&config::get);
        console::console().write_str("BitsOS V2 (G0 scaffold)\n", console::FG);
    }

    for level in [kreg::Level::Core, kreg::Level::Subsys, kreg::Level::Device, kreg::Level::Service, kreg::Level::Late] {
        kreg::run_level(level, |name, r| {
            if let Err(e) = r {
                klog::event!(Error, "kernel", "initcall.failed", name = name, error = e);
            }
        });
    }

    // FS-1 change request 5 (CONTRACTS 6a): vfs::init copies every byte of the initramfs into tmpfs, so the module's
    // frames go back to the allocator right after it. kexec's hand-over facts and RB-2's symbol table were read by
    // their initcalls above; a Reload then must bring its own initramfs (kexec refuses "keep the running one").
    // File times from CP-5's wall clock (the same clock as CLOCK_REALTIME).
    vfs::inode::set_clock(wall_ns);
    let initramfs_mod = bi.modules().find(|m| m.kind == bootinfo::module_kind::INITRAMFS);
    let unpacked = {
        let initramfs = initramfs_mod.map(|m| {
            // SAFETY: MODULE pages from bitsboot, direct-mapped RAM; the slice lives only inside this block,
            // released below after vfs::init copied it into tmpfs.
            unsafe { core::slice::from_raw_parts(hal::phys_to_virt(m.phys) as *const u8, m.size as usize) }
        });
        vfs::init(initramfs)
    };
    match unpacked {
        Ok(_) => {
            if let Some(m) = initramfs_mod
                && let Some(buddy) = mm_core::boot_buddy()
            {
                // The flag first: from it on kexec no longer reads the module (`kexec::boot::module_readable`).
                kexec::boot::note_initrd_released();
                match buddy.release_module(m.phys, m.phys + m.size) {
                    Ok(bytes) => {
                        klog::event!(Info, "kernel", "initramfs.released", bytes = bytes);
                        // The tmpfs pool was measured while vfs::init unpacked: RAM minus POOL_KEEP again (D4).
                        vfs::pool::frames_grew();
                    }
                    Err(e) => klog::event!(Warn, "kernel", "initramfs.kept", error = alloc::format!("{e:?}"); "kept, but treated as released: a Reload needs an initrd URL"),
                }
            }
        }
        // A bad archive keeps the module (its bytes stay readable for diagnosis); what unpacked before the
        // error is in tmpfs.
        Err(e) => klog::event!(Error, "kernel", "vfs.failed", error = e),
    }
    syscall::init();
    static CFG_FN: fn(&str) -> Option<alloc::string::String> = config::get;
    pci::set_config(&CFG_FN);
    // WL-4 CR 4: the WLAN passphrase only through the take-once store (before the drivers bind).
    static SECRET_FN: fn(&str, &mut dyn FnMut(&[u8])) -> bool = config::take_secret;
    net::set_secret_source(&SECRET_FN);
    pci::scan();
    let (bound, skipped) = pci::bind_drivers();
    klog::event!(Info, "pci", "drivers", bound = bound, skipped_opt_in = skipped);
    // BT-edid (SN-5 CR3): the monitor's EDID from the loader (every Reload carries the record on) names the DP
    // audio sink; without one hda announces a generic stereo LPCM sink. Independent of the framebuffer.
    if let Some(edid) = bi.edid() {
        let _ = hda::hdmi::set_sink_edid(edid);
    }
    // NW-3: smoltcp on every card plus 127.0.0.1 (falls back to the loopback alone if its thread cannot start).
    net::init(&CFG_FN);
    klog::event!(Info, "kernel", "config", values = config::summary());

    if cfg!(feature = "ktest") {
        run_ktests();
    }
    // Not an initcall: the init levels run before the network stack is installed above.
    mcp::set_boot_info(&bi, env!("CARGO_PKG_VERSION"));
    match mcp::start_kernel(&config::get) {
        Ok(_running) => {} // the handle may drop; the server keeps running
        Err(e) => klog::event!(Error, "kernel", "mcp.not_started", error = alloc::format!("{e:?}")),
    }
    klog::event!(Info, "kernel", "ready");
    // UL-2: the first user program (CONFIG init=, default /bin/init; init=off starts nothing), watched and
    // restarted by userland's supervisor thread (`ul init.died` with code and fault message, back-off restarts,
    // `ul init.given_up`). The initramfs is in tmpfs since vfs::init, MCP and the network are up.
    if let Err(e) = userland::start_init(&config::get) {
        klog::event!(Error, "kernel", "init.not_started", error = e);
    }
    // The everyday stick kernel skips the G0 hello programs (desk 24.10., Georg).
    #[cfg(not(bitsos_no_ktests))]
    smoke::run_at_boot();
    sched::exit();
}

/// CONFIG `serial=auto|on|off` (D1): `auto` (the entry's choice) = COM1 only under a hypervisor; the Mini-PC's
/// FCH UART answers at 0x3F8 but has no connector, and its 87 us per byte stalled whole CPUs.
fn configure_serial() {
    let value = config::get("serial");
    let mode = match value.as_deref().map(hal::serial::Mode::parse) {
        None => hal::serial::Mode::Auto,
        Some(Some(m)) => m,
        Some(None) => {
            klog::event!(Warn, "kernel", "config.bad", key = "serial", value = value.unwrap_or_default(); "want auto, on or off; keeping auto");
            hal::serial::Mode::Auto
        }
    };
    let port = hal::serial::configure(mode);
    // klog's serial sink only while something reads it (the port, or the early screen console until the
    // console takes over).
    klog::set_serial(hal::serial::has_output());
    klog::event!(Info, "kernel", "serial", mode = mode.name(), hypervisor = hal::serial::under_hypervisor(), port = port);
}

fn run_ktests() -> ! {
    let filter = config::get("ktest").unwrap_or_default();
    // CONFIG `ktest_hw=on`: only Safe and Display tests, as on the Mini-PC over MCP (QemuOnly ones skip there).
    let hw = config::get("ktest_hw").as_deref() == Some("on");
    let allowed = |c| matches!(c, ktest::Class::Safe | ktest::Class::Display) || (hal::cpu::detect().hypervisor && !hw);
    // CONFIG `ktest_nmi_check=on`: after each test, every other CPU must answer an NMI sample (desk fix/nmi).
    let sum = if config::get("ktest_nmi_check").as_deref() == Some("on") {
        ktest::run_checked(&filter, allowed, time::now_ns, nmi_check)
    } else {
        ktest::run(&filter, allowed, time::now_ns)
    };
    // RB-klog CR2: serial lines a storm left to klogd reach the log before QEMU exits (diagnostics only).
    klog::flush_serial(usize::MAX);
    hal::panic::qemu_exit(if sum.failed == 0 { hal::panic::EXIT_PASS } else { hal::panic::EXIT_FAIL });
    hal::panic::halt_forever()
}

/// An NMI sample of every other CPU after a test (`ktest_nmi_check=on`): `None` when all answered.
fn nmi_check(_: &str) -> Option<alloc::string::String> {
    if !hal::nmi::prepared() {
        return None;
    }
    let end = time::now_ns() + 3_000_000_000;
    let r = loop {
        match hal::nmi::sample_others(time::now_ns, 1_000_000_000) {
            Ok(r) => break r,
            Err(hal::nmi::NmiError::Busy) if time::now_ns() < end => sched::sleep_ns(1_000_000),
            Err(e) => return Some(alloc::format!("nmi sample: {e:?}")),
        }
    };
    if r.answered == r.asked && r.send_failed == 0 {
        return None;
    }
    let silent: alloc::vec::Vec<usize> =
        (0..hal::percpu::cpu_count()).filter(|&c| hal::nmi::answer(c) == hal::nmi::Answer::Silent).collect();
    Some(alloc::format!("nmi {r:?} silent {silent:?} late {}", hal::nmi::late_answers()))
}

/// ns since the Unix epoch: CP-5's wall clock, or the monotonic clock without a readable RTC.
fn wall_ns() -> u64 {
    time::wall::realtime_ns().unwrap_or_else(time::now_ns)
}

fn frames_stats() -> (u64, u64) {
    mm_core::frames().map(|f| f.stats()).map(|s| (s.total_bytes, s.free_bytes)).unwrap_or((0, 0))
}

/// A progress line for the first boot steps (COM1 and the early screen console).
fn stage(what: &str) {
    hal::serial::write_bytes(b"boot: ");
    hal::serial::write_bytes(what.as_bytes());
    hal::serial::write_bytes(b"\n");
}

fn early_fail(msg: &str) -> ! {
    hal::serial::write_bytes_unlocked(b"BitsOS V2 early failure: ");
    hal::serial::write_bytes_unlocked(msg.as_bytes());
    hal::serial::write_bytes_unlocked(b"\n");
    hal::panic::qemu_exit(hal::panic::EXIT_PANIC);
    stop()
}

/// After a fatal report: under QEMU stop here; on real hardware the message stays 30 s on screen, then the
/// machine resets (a silent halt looks exactly like a hang on a machine without COM1).
fn stop() -> ! {
    if hal::cpu::detect().hypervisor {
        hal::panic::halt_forever()
    }
    hal::serial::write_bytes_unlocked(b"BitsOS V2: restarting the machine in 30 seconds\n");
    hal::panic::reset_after(30)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    hal::cpu::disable_interrupts();
    // One CPU reports. The first panic wins; a panic on another CPU meanwhile (often an allocation that fails
    // because the reporting CPU put the heap into emergency mode) stops that CPU quietly, so the first message
    // reaches serial and the mirror whole. A panic inside the report path itself prints only to serial.
    use core::sync::atomic::{AtomicUsize, Ordering};
    static PANIC_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
    let me = hal::percpu::this_cpu();
    use core::fmt::Write;
    let mut w = hal::serial::Writer;
    match PANIC_CPU.compare_exchange(usize::MAX, me, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(c) if c == me => {
            // The screen itself may be what faulted: no drawing here, only the reset (the reboot is the sign).
            earlycon::stop();
            let _ = write!(w, "\nBitsOS V2 kernel panic inside the panic path: {}\n", info);
            hal::panic::qemu_exit(hal::panic::EXIT_PANIC);
            stop()
        }
        Err(_) => hal::panic::halt_forever(),
    }
    // RB-2: the other CPUs stop by NMI first, each saving its registers and backtrace, while the heap still
    // works for them (in emergency mode their next allocation would fail and panic instead).
    kexec::crash::stop_others();
    // From here allocations return null and frees leak, without taking a heap lock (core.md 10.3).
    mm_core::HEAP.enter_emergency();
    earlycon::resume();
    // The report's lines reach the early screen console through klog's serial path, also on bare metal.
    klog::set_serial(hal::serial::has_output());
    let _ = write!(w, "\nKTEST PANIC {}\n", info);
    // RB-2: panic line, backtraces with names, the other CPUs' registers, the crash record, then
    // panic=halt|reboot; every line through klog::try_event! + klog::flush_emergency() (RB-4's mirror path).
    kexec::crash::report(info)
}
