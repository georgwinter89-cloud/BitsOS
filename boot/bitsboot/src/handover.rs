//! The hand-over: KEEP pages, page tables, the BootInfo records, ExitBootServices (with the retry on a
//! changed map key) and the jump via the stub.
//!
//! Memory types: everything the kernel keeps using is `BITS_KEEP` / `BITS_MODULE` / `BITS_KERNEL` /
//! `BITS_PAGETABLE`. The buffer for the final memory map is `LOADER_DATA` on purpose: its content is copied
//! into the BootInfo MEMORY_MAP record (KEEP) before the jump, so the kernel may reuse those pages at once.

use crate::cpu::{self, CR4_CET, EFER, EFER_NXE};
use crate::elf;
use crate::gop::Screen;
use crate::logbuf;
use crate::mem::{self, PAGE};
use crate::paging::{self, Perm, Tables};
use crate::stick::StickInfo;
use crate::{Ctx, R, fail};
use alloc::string::ToString;
use alloc::vec::Vec;
use bitsboot_pure::config::TestHook;
use bitsboot_pure::elf::{PF_W, PF_X, Segment};
use bitsboot_pure::{pixel, records, sizing};
use bootinfo::layout::{DIRECT_MAP_BASE, KERNEL_BASE, VMAP_BASE};
use bootinfo::*;
use core::arch::{asm, global_asm};
use core::convert::Infallible;
use log::{info, warn};
use uefi::Status;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryDescriptor, MemoryMap, MemoryMapMeta, MemoryMapMut, MemoryMapRefMut};
use uefi::table::cfg::ConfigTableEntry;

/// Crash records sit at the same place on every boot when this page is free (V2 crash page, `spec/core.md` 10.3).
const CRASH_PAGE_WANTED: u64 = 0x1F00_0000;
const BELOW_4G: u64 = 0xFFFF_FFFF;
const BELOW_640K: u64 = 0x9_FFFF;
const STACK_SIZE: u64 = 16 * 1024;
/// GetMemoryMap + ExitBootServices attempts. A bound against a firmware that keeps changing its map, not a
/// count of things: Linux tries twice; each retry here costs microseconds.
const EBS_TRIES: u32 = 8;

pub struct ModuleFile {
    pub kind: u32,
    pub phys: u64,
    pub size: u64,
    pub name: Vec<u8>,
}

/// Everything the loading phase produced.
pub struct Loaded {
    pub cpu: Handover,
    pub generation: Generation,
    pub kernel: elf::Loaded,
    pub segments: Vec<Segment>,
    pub modules: Vec<ModuleFile>,
    pub config: Vec<u8>,
    pub stick: StickInfo,
    pub screen: Option<Screen>,
    /// The monitor's EDID from the firmware (`gop::edid`); empty = no EDID record.
    pub edid: Vec<u8>,
    pub hook: TestHook,
}

// Runs from the identity-mapped stub page: rdx = new CR3, rcx = stack, r8 = entry, rdi/rsi = BootInfo.
// A CR3 load keeps global TLB entries, so the firmware's global pages are flushed by turning CR4.PGE off and
// on again (V1 uefi.asm:37-43, spec core.md 2.3); harmless when PGE was off. CR4.CET is already clear here.
global_asm!(
    ".text",
    ".global bitsboot_stub_start",
    ".global bitsboot_stub_end",
    "bitsboot_stub_start:",
    "mov cr3, rdx",
    "mov rax, cr4",
    "mov r9, rax",
    "btr rax, 7",
    "mov cr4, rax",
    "mov cr4, r9",
    // Probe for machines without COM1: a yellow bar through the new tables' framebuffer mapping (r11 = its
    // virtual address, r10 = pixels; 0 = no framebuffer). Proves CR3, the tables and the VMAP mapping work.
    "test r11, r11",
    "jz 2f",
    "mov r9, rdi",
    "mov rbx, rcx",
    "mov rdi, r11",
    "mov rcx, r10",
    "mov eax, 0x00E0E000",
    "rep stosd",
    "mov rdi, r9",
    "mov rcx, rbx",
    "2:",
    "mov rsp, rcx",
    "xor ebp, ebp",
    "jmp r8",
    "bitsboot_stub_end:",
);
unsafe extern "C" {
    static bitsboot_stub_start: u8;
    static bitsboot_stub_end: u8;
}

fn keep_page(at: AllocateType, what: &str) -> R<u64> {
    mem::alloc(memtype::BITS_KEEP, at, 1, what)
}

pub(crate) fn config_table(guid: uefi::Guid) -> Option<u64> {
    uefi::system::with_config_table(|t| t.iter().find(|e| e.guid == guid).map(|e| e.address as u64))
}

fn wr(r: Result<(), WriteError>) -> R<()> {
    r.map_err(|e| fail!("the BootInfo buffer is too small ({e:?})"))
}

/// What the jump needs once the firmware is gone.
struct Jump {
    tables: Tables,
    bi: u64,
    bi_size: u64,
    stub: u64,
    rsp: u64,
    entry: u64,
    /// The CPU has NX: EFER.NXE goes on right before the jump.
    nx: bool,
    /// Framebuffer for the hand-over probe bars: physical base, pixels per line (0 = none).
    fb_phys: u64,
    fb_stride: u64,
}

pub fn hand_over(l: Loaded) -> R<Infallible> {
    let stack = mem::alloc(memtype::BITS_KERNEL, AllocateType::AnyPages, STACK_SIZE / PAGE, "the kernel stack")?;
    // Not zeroed: a crash record from the previous boot must survive.
    let crash = keep_page(AllocateType::Address(CRASH_PAGE_WANTED), "the crash page")
        .or_else(|_| keep_page(AllocateType::MaxAddress(BELOW_4G), "the crash page"))?;
    let tramp = keep_page(AllocateType::MaxAddress(BELOW_640K), "the AP trampoline page below 1 MiB")?;
    let low_pml4 = keep_page(AllocateType::MaxAddress(BELOW_4G), "the low PML4 page")?;
    let stub = keep_page(AllocateType::MaxAddress(BELOW_4G), "the hand-over stub page")?;
    for p in [tramp, low_pml4, stub] {
        mem::zero(p, PAGE);
    }
    // SAFETY: both symbols are labels in this image's .text (global_asm above), start before end.
    let code = unsafe {
        let start = &raw const bitsboot_stub_start;
        core::slice::from_raw_parts(start, (&raw const bitsboot_stub_end).offset_from(start) as usize)
    };
    mem::bytes(stub, code.len() as u64).copy_from_slice(code);

    let mut cpu = l.cpu;
    if cpu.cr4 & CR4_CET != 0 {
        cpu.flags |= handover_flags::CET_TURNED_OFF;
    }
    // EFER.NXE is set only after ExitBootServices (`finish`), on this CPU, right before the jump. Set earlier,
    // the Mini-PC's firmware (A0, 15.10.) hung inside ExitBootServices: its memory protection sees NXE on the
    // boot CPU and writes NX bits into page tables its other CPUs still use with NXE off (reserved bit there).
    let nx = cpu::has_nx();
    if nx {
        cpu.flags |= handover_flags::NXE_SET;
    }

    // Page tables.
    let mut map = boot::memory_map(MemoryType::LOADER_DATA).ctx("cannot read the memory map")?;
    map.sort();
    let mut screen = l.screen;
    if let Some(sc) = screen.as_mut().filter(|s| s.fb.format != PixelFormat::None) {
        check_fb_against_ram(sc, &map);
    }
    let fb = screen.as_ref().map(|s| s.fb).filter(|f| f.format != PixelFormat::None && f.phys != 0);
    let fb_span = fb.map_or(0, |f| ((f.phys & 0xFFF) + f.size).next_multiple_of(PAGE));
    let pool = paging::estimate(mem::ram_ranges(&map), l.kernel.size, fb_span);
    let mut t = Tables::new(pool, nx)?;
    for s in &l.segments {
        let start = s.vaddr & !(PAGE - 1);
        let span = (s.vaddr + s.memsz).next_multiple_of(PAGE) - start;
        let perm = Perm { write: s.flags & PF_W != 0, exec: s.flags & PF_X != 0, pat1: false };
        t.map_range(start, l.kernel.phys_of(start), span, perm, "the kernel")?;
    }
    let ram_top = t.map_direct(mem::ram_ranges(&map)).ok_or_else(|| fail!("page tables too small for the direct map"))?;
    drop(map);
    if let Some(f) = fb {
        t.map_range(VMAP_BASE, f.phys & !0xFFF, fb_span, Perm { write: true, exec: false, pat1: true }, "the framebuffer")?;
    }
    t.map_range(stub, stub, PAGE, Perm { write: false, exec: true, pat1: false }, "the stub")?;
    info!("page tables: {} of {} pages, direct map up to {:#x}", t.used(), pool, ram_top);

    // The buffer ExitBootServices' memory map goes into, sized with room for what the firmware adds.
    let map = boot::memory_map(MemoryType::LOADER_DATA).ctx("cannot read the memory map")?;
    let (entries, desc_size) = (map.len(), map.meta().desc_size.max(size_of::<MemoryDescriptor>()));
    drop(map);
    let map_pages = mem::pages_for(((entries + sizing::MAP_SPARE) * desc_size) as u64);
    let map_buf = mem::alloc(memtype::LOADER_DATA, AllocateType::AnyPages, map_pages, "the final memory map")?;

    // The BootInfo, sized from what was loaded (the memory map follows after ExitBootServices).
    let vendor = uefi::system::firmware_vendor().to_string();
    let names: Vec<usize> = l.modules.iter().map(|m| m.name.len()).collect();
    let bi_size = sizing::capacity(&sizing::Inputs {
        config: l.config.len(),
        log_so_far: logbuf::len(),
        module_names: &names,
        stick_path: l.stick.path.len(),
        modes: screen.as_ref().map_or(0, |s| s.modes.len()),
        edid: l.edid.len(),
        vendor: vendor.len(),
        map_entries: entries,
    }) as u64;
    let bi = mem::alloc(memtype::BITS_KEEP, AllocateType::AnyPages, bi_size / PAGE, "the BootInfo")?;
    mem::zero(bi, bi_size);

    let mut w = Writer::new(mem::bytes(bi, bi_size)).map_err(|e| fail!("BootInfo buffer: {e:?}"))?;
    wr(w.handover(cpu))?;
    wr(w.generation(l.generation))?;
    let k = &l.kernel;
    wr(w.kernel_image(KernelImage { phys_base: k.phys, phys_size: k.size, virt_base: k.virt, entry: k.entry }))?;
    for m in &l.modules {
        wr(w.module(&Module { kind: m.kind, phys: m.phys, size: m.size, name: &m.name }))?;
    }
    wr(w.config(&l.config))?;
    let s = &l.stick;
    let mut block0 = [0u8; 96];
    let n = s.block0.len().min(96);
    block0[..n].copy_from_slice(&s.block0[..n]);
    wr(w.stick(&Stick {
        flags: s.flags,
        pci_seg: s.pci.0,
        pci_bus: s.pci.1,
        pci_dev: s.pci.2,
        pci_fn: s.pci.3,
        usb_vid: s.usb.0,
        usb_pid: s.usb.1,
        stick_id: s.id,
        block0,
        device_path: &s.path,
    }))?;
    if let Some(sc) = &screen {
        wr(w.framebuffer(&sc.fb))?;
        wr(w.mode_list(sc.fb.mode_index, sc.modes.iter().copied()))?;
    }
    if !l.edid.is_empty() {
        wr(w.edid(&l.edid))?;
    }
    if let Some(phys) = config_table(ConfigTableEntry::ACPI2_GUID).or_else(|| config_table(ConfigTableEntry::ACPI_GUID)) {
        // SAFETY: the firmware's ACPI configuration table entry points to the RSDP (identity-mapped firmware
        // memory); byte 15 is its revision in both ACPI 1 and 2 layouts.
        let revision = unsafe { *((phys + 15) as *const u8) } as u32;
        wr(w.rsdp(Rsdp { phys, revision }))?;
    }
    if let Some(phys) = config_table(ConfigTableEntry::SMBIOS3_GUID) {
        wr(w.smbios(Smbios { phys, major: 3 }))?;
    } else if let Some(phys) = config_table(ConfigTableEntry::SMBIOS_GUID) {
        wr(w.smbios(Smbios { phys, major: 2 }))?;
    }
    wr(w.firmware(uefi::system::firmware_revision(), uefi::system::uefi_revision().0, vendor.as_bytes()))?;
    wr(w.boot_id(cpu::boot_id()))?;
    let hint_hz = cpu::tsc_hint_hz();
    wr(w.tsc(Tsc { hint_hz, calibrated_hz: 0, source: tsc_source::UEFI_STALL, calibrated_at_reload: 0 }))?;
    wr(w.reload(Reload { counter: 0 }))?;
    wr(w.crash_page(PhysRange { phys: crash, size: PAGE }))?;
    wr(w.trampoline(Trampoline { page_phys: tramp, low_pml4_phys: low_pml4 }))?;
    wr(w.reload_stub(PhysRange { phys: stub, size: PAGE }))?;
    wr(w.page_tables(PageTables {
        pml4_phys: t.pml4,
        direct_map_base: DIRECT_MAP_BASE,
        direct_map_size: ram_top,
        kernel_virt_base: KERNEL_BASE,
        vmap_base: VMAP_BASE,
    }))?;
    // `w` is not used past this point, so the slices below are the only views of the block.
    let written = w.len() as u64;

    info!("TSC about {} MHz; crash page {:#x}, trampoline {:#x}, stub {:#x}", hint_hz / 1_000_000, crash, tramp, stub);
    let list = records::summary(mem::bytes(bi, written)).unwrap_or_default();
    info!("BootInfo records: {list} LOADER_LOG; MEMORY_MAP after ExitBootServices");
    info!("BootInfo modules: {}", records::modules(mem::bytes(bi, written)).unwrap_or_default());
    info!("BootInfo at {:#x} ({} bytes reserved); exiting boot services, then jumping to {:#x}", bi, bi_size, k.entry);

    let mut w = Writer::resume(mem::bytes(bi, bi_size)).ok_or_else(|| fail!("the BootInfo header is damaged"))?;
    wr(w.loader_log(logbuf::text()))?;
    let room = w.free();

    let (fb_phys, fb_stride) = screen.as_ref().map(|s| s.fb).filter(|f| f.format != PixelFormat::None && f.phys != 0).map_or((0, 0), |f| (f.phys, f.stride as u64));
    let jump = Jump { tables: t, bi, bi_size, stub, rsp: DIRECT_MAP_BASE + stack + STACK_SIZE, entry: k.entry, nx, fb_phys, fb_stride };
    // Probe bars (machines without COM1): blue through the firmware's Blt = ExitBootServices comes next; a
    // white bar if the firmware's tables map the framebuffer (bitsboot can draw after ExitBootServices),
    // magenta if they do not.
    crate::gop::fill_rows(0, 16, 0x0000_40FF);
    if fb_phys != 0 {
        let mapped = mapped_in_live_tables(fb_phys) && mapped_in_live_tables(fb_phys + fb_stride * 48 * 4);
        crate::gop::fill_rows(16, 16, if mapped { 0x00FF_FFFF } else { 0x00FF_00FF });
    }
    set_probe_fb(fb_phys, fb_stride);
    // Probe: the firmware's ExitBootServices notifications reached our two events (white at TPL_NOTIFY, early;
    // light blue at TPL_CALLBACK, late): tells a hang in some driver's notification from one after them.
    // SAFETY: the callbacks only write into the framebuffer (no firmware call, no allocation).
    unsafe {
        let _ = boot::create_event(uefi::boot::EventType::SIGNAL_EXIT_BOOT_SERVICES, uefi::boot::Tpl::NOTIFY, Some(ebs_notify_early), None);
        let _ = boot::create_event(uefi::boot::EventType::SIGNAL_EXIT_BOOT_SERVICES, uefi::boot::Tpl::CALLBACK, Some(ebs_notify_late), None);
    }
    let (mm, tries) = exit_boot_services(map_buf, (map_pages * PAGE) as usize, room, l.hook)?;
    finish(jump, mm, tries)
}

/// Spec core.md 2.1: never a framebuffer inside RAM (the kernel would map that RAM again as a write-combining
/// alias): such a mode is handed over as "no framebuffer" (headless, serial + MCP). Over 64 MiB: a warning.
fn check_fb_against_ram(sc: &mut Screen, map: &impl MemoryMap) {
    let (phys, size) = (sc.fb.phys, sc.fb.size);
    if pixel::overlaps_ram(phys, size, mem::ram_ranges(map)) {
        warn!("the framebuffer {phys:#x} ({size} bytes) lies in RAM of the memory map: handing over no framebuffer");
        sc.fb = Framebuffer { phys: 0, virt: 0, size: 0, format: PixelFormat::None, masks: [0; 4], ..sc.fb };
    } else if size > pixel::FB_LARGE {
        warn!("the framebuffer is {} MiB, more than the 64 MiB BT-2 would ever choose", size >> 20);
    }
}

/// GetMemoryMap + ExitBootServices, again with a fresh map while the firmware reports a changed map key
/// (UEFI 2.10 §7.4.6). Logging is off from here; on failure the console comes back for the error screen.
/// No allocation happens between GetMemoryMap and ExitBootServices (except the test hook's on purpose).
fn exit_boot_services(mut buf: u64, mut buf_len: usize, room: usize, hook: TestHook) -> R<(MemoryMapRefMut<'static>, u32)> {
    let st = uefi::table::system_table_raw().ok_or_else(|| fail!("the UEFI system table is gone"))?;
    // SAFETY: the system table is valid until ExitBootServices succeeds; only its boot services pointer is read.
    let bs = unsafe { st.as_ref().boot_services };
    if bs.is_null() {
        return Err(fail!("the firmware has no boot services table"));
    }
    let image = boot::image_handle().as_ptr();
    logbuf::console_off();
    let (mut last, mut step) = (Status::SUCCESS, "GetMemoryMap");
    // UEFI 2.10 §7.4.6: after the first ExitBootServices call only GetMemoryMap and ExitBootServices are
    // allowed, so the map buffer can grow only before it.
    let mut exit_called = false;
    let mut needed = 0usize;
    for attempt in 1..=EBS_TRIES {
        let (mut size, mut key, mut dsize, mut dver) = (buf_len, 0usize, 0usize, 0u32);
        // SAFETY: GetMemoryMap writes at most `size` bytes into our LOADER_DATA buffer of `buf_len` bytes and
        // the four out-parameters; `bs` is the live boot services table.
        let s = unsafe { ((*bs).get_memory_map)(&mut size, buf as *mut MemoryDescriptor, &mut key, &mut dsize, &mut dver) };
        // Probe: per attempt 3 blocks: map (orange ok / yellow too small / red error), before EBS (purple), result.
        let col = (attempt as u64 - 1) * 3;
        probe_block(col, match s {
            Status::SUCCESS => 0x00FF_8000,
            Status::BUFFER_TOO_SMALL => 0x00FF_FF00,
            _ => 0x00FF_0000,
        });
        if s == Status::BUFFER_TOO_SMALL {
            // `size` is now what the firmware needs.
            needed = size;
            (last, step) = (s, "GetMemoryMap");
            if !exit_called {
                let desc = dsize.max(size_of::<MemoryDescriptor>());
                let pages = mem::pages_for(size.saturating_add(sizing::MAP_SPARE * desc) as u64);
                mem::free(buf, mem::pages_for(buf_len as u64));
                match mem::alloc(memtype::LOADER_DATA, AllocateType::AnyPages, pages, "the final memory map") {
                    Ok(b) => (buf, buf_len) = (b, (pages * PAGE) as usize),
                    Err(f) => {
                        logbuf::console_back();
                        return Err(f);
                    }
                }
            }
            continue;
        }
        if s != Status::SUCCESS || dsize < size_of::<MemoryDescriptor>() || size > buf_len {
            (last, step) = (s, "GetMemoryMap");
            continue;
        }
        if !sizing::map_fits(room, size / dsize + 2 * mem::typed_blocks()) {
            logbuf::console_back();
            return Err(fail!("the BootInfo has no room for {} memory map entries", size / dsize));
        }
        let stale = match hook {
            TestHook::EbsRetry => attempt == 1,
            TestHook::EbsFail => true,
            TestHook::None | TestHook::EdidActive | TestHook::EdidFallback => false,
        };
        if stale {
            // Changes the memory map, so the key just read is stale.
            let _ = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1);
        }
        // SAFETY: our own image handle and the key of the map just read; on success the firmware is gone
        // and nothing below calls it again.
        probe_block(col + 1, 0x00A0_00FF);
        let s = unsafe { ((*bs).exit_boot_services)(image, key) };
        probe_block(col + 2, if s == Status::SUCCESS { 0x0000_FF00 } else { 0x00FF_0000 });
        if s != Status::SUCCESS {
            probe_status(26, s);
        }
        exit_called = true;
        if s == Status::SUCCESS {
            let meta = MemoryMapMeta { map_size: size, desc_size: dsize, map_key: Default::default(), desc_version: dver };
            match MemoryMapRefMut::new(mem::bytes(buf, buf_len as u64), meta) {
                Ok(mm) => return Ok((mm, attempt)),
                Err(_) => halt_after_exit(b"the final memory map is malformed"),
            }
        }
        (last, step) = (s, "ExitBootServices");
    }
    // Probe: every try failed: a red block at the end of the row, the last status before it.
    probe_status(26, last);
    probe_block(35, 0x00FF_0000);
    logbuf::console_back();
    let need = if last == Status::BUFFER_TOO_SMALL {
        alloc::format!(": the memory map needs {needed} bytes, the buffer has {buf_len}")
    } else {
        alloc::string::String::new()
    };
    Err(fail!(
        "the firmware did not let go of the machine: {step} failed {EBS_TRIES} times (last UEFI status {last:?}){need}"
    ))
}

/// After ExitBootServices nothing can be shown on screen any more: a line on COM1, then stop.
fn halt_after_exit(why: &[u8]) -> ! {
    logbuf::serial_line(&[b"bitsboot: cannot start the kernel after ExitBootServices: ", why]);
    loop {
        // SAFETY: stopping this CPU for good; interrupts are off.
        unsafe { asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// From here on: no logging, no allocation, no firmware call.
fn finish(j: Jump, mut mm: MemoryMapRefMut<'static>, tries: u32) -> ! {
    // SAFETY: masking interrupts; the firmware's handlers must not run any more.
    unsafe { asm!("cli", options(nomem, nostack)) };
    // Probe for machines without COM1: a cyan bar (rows 32..48) straight into the framebuffer through the
    // firmware's identity mapping (only where the walk finds it mapped): ExitBootServices is done.
    if j.fb_phys != 0 && mapped_in_live_tables(j.fb_phys) && mapped_in_live_tables(j.fb_phys + j.fb_stride * 64 * 4) {
        for i in j.fb_stride * 48..j.fb_stride * 64 {
            // SAFETY: inside the GOP framebuffer (stride * 16 pixels < its size), identity-mapped by the firmware.
            unsafe { ((j.fb_phys + i * 4) as *mut u32).write_volatile(0x0000_E0E0) };
        }
    }
    let mut t = j.tables;
    mm.sort();
    // Same RAM as before ExitBootServices; this only covers what the final map might add (the pool has spare
    // pages for it). RAM left out of the direct map would fault silently in the kernel, so it stops here.
    if t.map_direct(mem::ram_ranges(&mm)).is_none() {
        halt_after_exit(b"the page tables are too small for the final memory map");
    }
    let written = Writer::resume(mem::bytes(j.bi, j.bi_size)).map(|mut w| w.memory_map(mem::retyped(&mm)));
    if !matches!(written, Some(Ok(()))) {
        halt_after_exit(b"the memory map does not fit into the BootInfo");
    }
    let mut n = [0u8; 20];
    let mut e = [0u8; 20];
    logbuf::serial_line(&[
        b"bitsboot: ExitBootServices done (try ",
        logbuf::dec(tries as u64, &mut n),
        b"), ",
        logbuf::dec(mm.len() as u64, &mut e),
        b" memory map entries; starting the kernel",
    ]);
    // SAFETY: the firmware is gone, its page tables are now ours; the stub page is identity-mapped in both
    // the firmware's and our tables, CR4 loses only CET, and the jump hands over with the documented state
    // (main.rs: kernel entry contract). CET goes first: clearing CR0.WP (in allow_exec_in_firmware_tables)
    // raises #GP while CR4.CET is set (Intel SDM vol. 3A 2.5), and nothing could report that fault any more.
    unsafe {
        let cr4 = cpu::cr4();
        if cr4 & CR4_CET != 0 {
            cpu::write_cr4(cr4 & !CR4_CET);
        }
        allow_exec_in_firmware_tables(j.stub);
        // Our tables use NX: EFER.NXE must be on before the stub loads them (see `hand_over`).
        if j.nx {
            cpu::wrmsr(EFER, cpu::rdmsr(EFER) | EFER_NXE);
        }
        asm!(
            "cld",
            "jmp {stub}",
            stub = in(reg) j.stub,
            in("rdi") DIRECT_MAP_BASE + j.bi,
            in("rsi") j.bi,
            in("rdx") t.pml4,
            in("rcx") j.rsp,
            in("r8") j.entry,
            in("r11") if j.fb_phys != 0 { VMAP_BASE + (j.fb_phys & 0xFFF) + j.fb_stride * 64 * 4 } else { 0 },
            in("r10") j.fb_stride * 16,
            options(noreturn),
        )
    }
}

/// Probe for machines without COM1: a 32x16 block at column `col` of rows 32..48, written straight into the
/// framebuffer when the live tables map it (set up by `set_probe_fb`).
static PROBE_FB: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PROBE_STRIDE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn set_probe_fb(phys: u64, stride: u64) {
    if phys != 0 && mapped_in_live_tables(phys) && mapped_in_live_tables(phys + stride * 96 * 4) {
        PROBE_FB.store(phys, core::sync::atomic::Ordering::Relaxed);
        PROBE_STRIDE.store(stride, core::sync::atomic::Ordering::Relaxed);
    }
}

unsafe extern "efiapi" fn ebs_notify_early(_e: uefi::Event, _c: Option<core::ptr::NonNull<core::ffi::c_void>>) {
    probe_block(20, 0x00FF_FFFF);
}

unsafe extern "efiapi" fn ebs_notify_late(_e: uefi::Event, _c: Option<core::ptr::NonNull<core::ffi::c_void>>) {
    probe_block(21, 0x0080_C0FF);
}

fn probe_block(col: u64, rgb: u32) {
    let (fb, stride) = (PROBE_FB.load(core::sync::atomic::Ordering::Relaxed), PROBE_STRIDE.load(core::sync::atomic::Ordering::Relaxed));
    if fb == 0 || (col + 1) * 32 > stride {
        return;
    }
    for y in 32..48 {
        for x in col * 32 + 2..col * 32 + 30 {
            // SAFETY: inside the framebuffer (x < stride, y < 96), which `set_probe_fb` found mapped.
            unsafe { ((fb + (y * stride + x) * 4) as *mut u32).write_volatile(rgb) };
        }
    }
}

/// A UEFI status as 8 blocks from column `col`: bit 7 first, white = 1, dark grey = 0 (the low byte; the
/// error bit 63 is implied by red before it).
fn probe_status(col: u64, s: Status) {
    let v = s.0 as u64;
    for bit in 0..8 {
        probe_block(col + bit, if v >> (7 - bit) & 1 == 1 { 0x00FF_FFFF } else { 0x0040_4040 });
    }
}

/// Whether `addr` is mapped (present on every level) in the live page tables: lets the probes write only
/// where no fault can happen.
fn mapped_in_live_tables(addr: u64) -> bool {
    let mut table = cpu::cr3() & 0x000F_FFFF_FFFF_F000;
    for shift in [39u32, 30, 21, 12] {
        // SAFETY: the live tables are identity-mapped firmware memory; reading present entries only.
        let v = unsafe { ((table + ((addr >> shift) & 511) * 8) as *const u64).read_volatile() };
        if v & 1 == 0 {
            return false;
        }
        if shift == 12 || (shift != 39 && v & (1 << 7) != 0) {
            return true;
        }
        table = v & 0x000F_FFFF_FFFF_F000;
    }
    true
}

/// The firmware may map BitsOS pages no-execute (NX policy for OS memory types). Its tables are ours after
/// ExitBootServices: clear NX on the walk to the stub page (with CR0.WP off, in case they are read-only).
///
/// # Safety
/// Only after ExitBootServices, with interrupts off, on the CPU's live tables, with CR4.CET clear.
unsafe fn allow_exec_in_firmware_tables(addr: u64) {
    if cpu::rdmsr(EFER) & EFER_NXE == 0 {
        return;
    }
    const NX: u64 = 1 << 63;
    const WP: u64 = 1 << 16;
    let cr0 = cpu::cr0();
    // SAFETY: the caller's contract; the walk follows present entries of the live CR3 (identity-mapped
    // firmware tables), clears only NX, and CR0.WP is restored and the TLB flushed by reloading CR3.
    unsafe {
        asm!("mov cr0, {}", in(reg) cr0 & !WP, options(nostack));
        let mut table = cpu::cr3() & 0x000F_FFFF_FFFF_F000;
        for shift in [39u32, 30, 21, 12] {
            let e = (table + ((addr >> shift) & 511) * 8) as *mut u64;
            let v = e.read_volatile();
            if v & 1 == 0 {
                break;
            }
            e.write_volatile(v & !NX);
            if shift == 12 || (shift != 39 && v & (1 << 7) != 0) {
                break;
            }
            table = v & 0x000F_FFFF_FFFF_F000;
        }
        asm!("mov cr0, {}", "mov {t}, cr3", "mov cr3, {t}", in(reg) cr0, t = out(reg) _, options(nostack));
    }
}
