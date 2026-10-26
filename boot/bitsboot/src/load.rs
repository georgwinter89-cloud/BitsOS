//! The boot sequence up to the hand-over: CPU checks, config, generation and kernel, modules, stick, screen.

use crate::cpu::{self, EFER};
use crate::handover::{self, Loaded, ModuleFile};
use crate::{Ctx, R, elf, fail, files, generation, gop, machine, markers, mem, stick};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bitsboot_pure::config::{self, TestHook};
use bitsboot_pure::{gens, profile};
use bootinfo::{Handover, memtype, module_kind};
use core::convert::Infallible;
use log::{info, warn};
use uefi::boot;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::Directory;

const FW_DIR: &str = "\\BITSOS2\\fw";
const CONFIG: &str = "\\BITSOS2\\CONFIG.TXT";

pub fn run() -> R<Infallible> {
    info!("bitsboot: BitsOS V2 loader (BT-2)");
    // Before anything changes: the state the firmware handed us.
    let cpu = Handover { cr0: cpu::cr0(), cr3: cpu::cr3(), cr4: cpu::cr4(), efer: cpu::rdmsr(EFER), flags: 0 };
    if let Some(why) = bitsboot_pure::cpu::paging_refusal(cpu.cr4) {
        return Err(fail!("{why}"));
    }
    let missing = cpu::missing_v3_features();
    if !missing.is_empty() {
        let list = missing.join(", ");
        return Err(fail!(
            "Prozessor zu alt für BitsOS: fehlt {list}. / CPU too old for BitsOS (needs x86-64-v3): missing {list}."
        ));
    }
    let device = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .ctx("cannot query the loaded image")?
        .device()
        .ok_or_else(|| fail!("the firmware does not say which volume bitsboot came from"))?;
    let mut root = files::open_volume(device)?;
    let volume = files::volume_info(&mut root);

    let file = match files::read(&mut root, CONFIG)? {
        Some(c) => {
            info!("{CONFIG}: {} bytes", c.len());
            c
        }
        None => {
            warn!("{CONFIG} is missing: the kernel uses its defaults");
            Vec::new()
        }
    };
    let (config, safe) = choose_profile(&file);
    let hook = if cpu::hypervisor() { config::test_hook(&config) } else { TestHook::None };
    if hook != TestHook::None {
        warn!("test hook {}: {hook:?} (honoured only under a hypervisor)", config::TEST_KEY);
    }

    let gens_found = generation::scan(&mut root)?;
    let pin = generation::read_pin(&mut root);
    // The firmware blobs first: a read error there must not use up a new generation's one try (TRY is written
    // inside load_generation; after it only the stick probe, the screen and the hand-over remain).
    let fw = load_fw(&mut root)?;
    let (generation, kernel, segments, initrd) = load_generation(&mut root, &gens_found, pin, safe)?;
    let mut modules = alloc::vec![initrd];
    modules.extend(fw);
    let mut stick = stick::probe(device, &mut root);
    stick::log(&stick, volume);
    if safe {
        stick::withhold_id(&mut stick);
    }
    drop(root);

    let screen = gop::choose_screen(&config);
    gop::log(&screen);
    // BT-edid: read while boot services live (hand_over sizes the BootInfo with it).
    let edid = gop::edid(hook);

    handover::hand_over(Loaded { cpu, generation, kernel, segments, modules, config, stick, screen, edid, hook })
}

/// The boot profile (BT-5): logs the machine and the choice; returns the CONFIG text for the kernel (the file
/// itself in the normal profile) and whether this is safe mode.
fn choose_profile(file: &[u8]) -> (Vec<u8>, bool) {
    let cpu = machine::cpu();
    machine::log_cpu(&cpu);
    let dmi = machine::dmi();
    let d = profile::decide(file, &cpu, dmi.as_ref());
    if let Some(w) = &d.warning {
        warn!("bitsboot profile.config_invalid: {w}");
    }
    let line = format!("profile.chosen name={} reason={}: {}", d.profile.name(), d.reason.name(), d.detail);
    if d.profile.safe() {
        // Another colour on the screen: Georg should see at once that this boot is the safe one.
        warn!("bitsboot {line}");
        warn!("bitsboot safe mode: no network driver, no writes to the stick, NVMe, USB disks or WLAN, the firmware's picture");
        if d.reason == profile::Reason::Cpuid {
            // Not the Gaming-PC's DMI names: any Intel machine on bare metal lands here, so say how to leave.
            warn!("bitsboot profile.intel_fallback: no Gaming-PC DMI names; an Intel CPU on bare metal is taken for the Gaming-PC");
        }
        warn!("bitsboot safe mode ends with the line profile=normal in \\BITSOS2\\CONFIG.TXT");
    } else {
        info!("bitsboot {line}");
    }
    let config = profile::apply(file, &d);
    if config.len() != file.len() {
        info!("CONFIG for the kernel: {} bytes ({} from bitsboot's profile)", config.len(), config.len() - file.len());
    }
    (config, d.profile.safe())
}

type Chosen = (bootinfo::Generation, elf::Loaded, Vec<bitsboot_pure::elf::Segment>, ModuleFile);

/// The marker rule (`bitsboot_pure::gens::choose`, V2-PLAN §7): load what it picks; a generation whose files
/// fail the checks, or whose TRY cannot be written, goes back to the rule, which then picks the next one.
/// Safe mode (`safe`) chooses by `gens::choose_read_only`: no marker is ever written.
fn load_generation(root: &mut Directory, gens_found: &[gens::GenDir], pin: gens::Pin, safe: bool) -> R<Chosen> {
    let (mut bad, mut mark_failed, mut problems) = (Vec::new(), Vec::new(), Vec::<String>::new());
    loop {
        let choice = if safe { gens::choose_read_only(gens_found, pin, &bad) } else { gens::choose(gens_found, pin, &bad, &mark_failed) };
        let (record, mark_try, why) = match choice {
            gens::Choice::Start { record, mark_try, why } => (record, mark_try, why),
            gens::Choice::Refuse(why) => {
                let seen = if problems.is_empty() { String::new() } else { format!(" ({})", problems.join("; ")) };
                return Err(fail!("{why}{seen}"));
            }
        };
        let number = record.number;
        let (kernel, segments, initrd) = match load_files(root, number) {
            Ok(f) => f,
            Err(p) => {
                warn!("bitsboot generation.bad_files number={number}: {p}");
                problems.push(p);
                bad.push(number);
                continue;
            }
        };
        if mark_try && let Err(f) = markers::mark_try(root, number) {
            warn!("bitsboot marker.failed number={number}: {}", f.0);
            mem::free_block(kernel.phys);
            mem::free_block(initrd.phys);
            mark_failed.push(number);
            continue;
        }
        let line = format!(
            "generation.chosen number={number} reason={} skipped={} flags={}: {why}",
            gens::reason_name(record.reason),
            record.skipped,
            gens::flag_names(record.flags)
        );
        // A fallback or anything unusual is a warning (it shows on the screen in another colour).
        let unusual = gens::flag::PIN_IGNORED | gens::flag::NO_CONFIRMED_FALLBACK | gens::flag::MARKER_WRITE_FAILED | gens::flag::OTHERS_SKIPPED;
        if record.skipped != 0 && record.reason != bootinfo::gen_reason::PINNED || record.flags & unusual != 0 {
            warn!("bitsboot {line}");
        } else {
            info!("bitsboot {line}");
        }
        return Ok((record, kernel, segments, initrd));
    }
}

/// A generation's KERNEL (ELF checks, placed in `BITS_KERNEL` pages) and INITRD (cpio header, `BITS_MODULE`
/// pages). `Err` names the file and its problem; nothing stays allocated then.
fn load_files(root: &mut Directory, number: u32) -> Result<(elf::Loaded, Vec<bitsboot_pure::elf::Segment>, ModuleFile), String> {
    let kpath = generation::path(number, "KERNEL");
    let file = match files::read(root, &kpath) {
        Ok(Some(f)) => f,
        Ok(None) => return Err(format!("{kpath}: missing")),
        Err(f) => return Err(format!("{kpath}: {}", f.0)),
    };
    let parsed = bitsboot_pure::elf::parse(&file).map_err(|why| {
        warn!("{kpath} ({} bytes) is not a valid BitsOS kernel: {why}", file.len());
        format!("{kpath}: {why}")
    })?;
    let ipath = generation::path(number, "INITRD");
    let (iphys, isize) = match files::read_to_pages(root, &ipath, memtype::BITS_MODULE) {
        Ok(Some(p)) => p,
        Ok(None) => return Err(format!("{ipath}: missing")),
        Err(f) => return Err(format!("{ipath}: {}", f.0)),
    };
    if let Err(why) = gens::check_initrd(mem::bytes(iphys, isize.min(110))) {
        mem::free_block(iphys);
        return Err(format!("{ipath} ({isize} bytes): {why}"));
    }
    let kernel = elf::load(&file, &parsed, &kpath).map_err(|f| {
        mem::free_block(iphys);
        format!("{kpath}: {}", f.0)
    })?;
    info!(
        "{kpath}: {} bytes, image {:#x} ({} bytes) at {:#x}, entry {:#x}",
        file.len(),
        kernel.phys,
        kernel.size,
        kernel.virt,
        kernel.entry
    );
    info!("{ipath}: {isize} bytes at {iphys:#x}");
    let initrd = ModuleFile { kind: module_kind::INITRAMFS, phys: iphys, size: isize, name: b"INITRD".to_vec() };
    Ok((kernel, parsed.segs, initrd))
}

/// Every file in `\BITSOS2\fw\`, each in its own `BITS_MODULE` pages (the modules after the INITRD).
fn load_fw(root: &mut Directory) -> R<Vec<ModuleFile>> {
    let mut out = Vec::new();
    let mut fw = files::list(root, FW_DIR)?.unwrap_or_default();
    fw.retain(|e| !e.is_dir);
    fw.sort_by(|a, b| a.name.cmp(&b.name));
    for e in fw {
        let path = format!("{FW_DIR}\\{}", e.name);
        if let Some((phys, size)) = files::read_to_pages(root, &path, memtype::BITS_MODULE)? {
            info!("{path}: {size} bytes at {phys:#x}");
            out.push(ModuleFile { kind: module_kind::FIRMWARE, phys, size, name: e.name.into_bytes() });
        }
    }
    Ok(out)
}
