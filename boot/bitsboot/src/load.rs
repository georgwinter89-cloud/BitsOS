//! The boot sequence up to the hand-over: CPU checks, config, generation and kernel, modules, stick, screen.

use crate::cpu::{self, EFER};
use crate::handover::{self, Loaded, ModuleFile};
use crate::{Ctx, R, elf, fail, fch, files, generation, gop, machine, markers, mem, stick};
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
use zeroize::Zeroize;

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

    let mut file = match files::read(&mut root, CONFIG)? {
        Some(c) => {
            info!("{CONFIG}: {} bytes", c.len());
            c
        }
        None => {
            warn!("{CONFIG} is missing: the kernel uses its defaults");
            Vec::new()
        }
    };
    // Test hooks are read from the stick's own file, and only under a hypervisor.
    let hook = if cpu::hypervisor() { config::test_hook(&file) } else { TestHook::None };
    if hook != TestHook::None {
        warn!("test hook {}: {hook:?} (honoured only under a hypervisor)", config::TEST_KEY);
    }
    let Chosen { config, safe, tiers } = if hook == TestHook::OldLoader {
        warn!("bitsboot profile.skipped: test hook old_loader, no profile is chosen and the file goes to the kernel as it is");
        Chosen { config: file.clone(), safe: false, tiers: profile::Tiers::SAFE }
    } else {
        choose_profile(&file)
    };
    // RB-18: CONFIG.TXT holds secrets and this RAM is free once the kernel runs.
    file.zeroize();

    let gens_found = generation::scan(&mut root)?;
    let pin = generation::read_pin(&mut root);
    // The firmware blobs first: a read error there must not use up a new generation's one try (TRY is written
    // inside load_generation; after it only the stick probe, the screen and the hand-over remain).
    let fw = load_fw(&mut root)?;
    let flood = !safe && after_sync_flood();
    // GP-GUARD: with gp_stick=on the Gaming-PC's stick follows the normal marker rule (a new generation gets its
    // one try and its fallback, as on the Mini-PC); without it safe mode writes nothing.
    let read_only = safe && tiers.markers_read_only();
    // GP-GUARD: under the profile only a kernel that knows the disk guard is started.
    let rules = Rules { read_only, flood, guard: safe };
    let (generation, kernel, segments, initrd) = load_generation(&mut root, &gens_found, pin, rules)?;
    let mut modules = alloc::vec![initrd];
    modules.extend(fw);
    let mut stick = stick::probe(device, &mut root);
    stick::log(&stick, volume);
    if safe && tiers.stick_id_withheld() {
        stick::withhold_id(&mut stick);
    } else if safe {
        stick::log_id_handed(&stick);
    }
    drop(root);

    let screen = gop::choose_screen(&config);
    gop::log(&screen);
    // BT-edid: read while boot services live (hand_over sizes the BootInfo with it).
    let edid = gop::edid(hook);

    handover::hand_over(Loaded { cpu, generation, kernel, segments, modules, config, stick, screen, edid, hook })
}

/// What [`choose_profile`] decided: the CONFIG text for the kernel (the file itself in the normal profile),
/// whether this is safe mode, and what the stick's tier lines release there.
struct Chosen {
    config: Vec<u8>,
    safe: bool,
    tiers: profile::Tiers,
}

/// The boot profile (BT-5, GP-GUARD): logs the machine, the choice and the tiers.
fn choose_profile(file: &[u8]) -> Chosen {
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
        let t = d.tiers;
        warn!("bitsboot profile.tiers net={} stick={} nvme={}", if t.net { "on" } else { "off" }, if t.stick { "on" } else { "off" }, if t.nvme { "on" } else { "off" });
        if t.nvme {
            // GP-DISK: the line as the stick has it; the kernel compares place and ids with its PCI scan.
            let named = gp_platform::disk_rule::named::named(config::get(file, gp_platform::tiers::NVME_KEY)).map(|n| n.render()).unwrap_or_default();
            warn!("bitsboot safe mode: only the NVMe controller gp_nvme={named} may get a driver, with the Mini-PC's rules for it; no other internal disk, no writes to other USB disks, no WLAN, the firmware's picture");
        } else {
            warn!("bitsboot safe mode: no driver for the internal disks (NVMe, SATA), no writes to other USB disks, no WLAN, the firmware's picture");
        }
        warn!(
            "bitsboot safe mode: network {}; boot stick {}",
            if t.net { "drivers for the cable allowed (gp_net=on)" } else { "off (gp_net=on in CONFIG.TXT allows the cable)" },
            if t.stick { "writable below \\BITSOS2 (gp_stick=on)" } else { "read-only (gp_stick=on in CONFIG.TXT makes it writable)" }
        );
        if d.normal_refused {
            warn!("bitsboot profile.normal_refused: CONFIG profile=normal is not honoured on this machine: its internal disks stay without a driver");
        }
        match d.reason {
            profile::Reason::Dmi => warn!("bitsboot safe mode cannot be ended on this board; only gp_net=on, gp_stick=on and gp_nvme= release something"),
            profile::Reason::Cpuid => {
                // Not the Gaming-PC's DMI names: any Intel machine on bare metal lands here, so say how to leave.
                warn!("bitsboot profile.intel_fallback: no Gaming-PC DMI names; an Intel CPU on bare metal is taken for the Gaming-PC");
                warn!("bitsboot safe mode ends with the line profile=normal in \\BITSOS2\\CONFIG.TXT when the DMI names of another machine can be read");
            }
            _ => warn!("bitsboot safe mode ends when the line profile=gaming-pc leaves \\BITSOS2\\CONFIG.TXT"),
        }
    } else {
        info!("bitsboot {line}");
    }
    let config = profile::apply(file, &d);
    if config.len() != file.len() {
        info!("CONFIG for the kernel: {} bytes ({} from bitsboot's profile)", config.len(), config.len() - file.len());
    }
    Chosen { config, safe: d.profile.safe(), tiers: d.tiers }
}

/// RB-14 CR-3: whether the FCH says the last start ended in a sync flood (read only; logged either way).
fn after_sync_flood() -> bool {
    match fch::s5_reset_status() {
        Ok(v) if v != u32::MAX && v & fch::SYNC_FLOOD != 0 => {
            warn!("bitsboot flood_start s5_reset_status={v:#010x}: the last start ended in a sync flood; a new generation keeps its one try");
            true
        }
        Ok(v) => {
            info!("bitsboot s5_reset_status={v:#010x}");
            false
        }
        Err(why) => {
            info!("bitsboot s5_reset_status not read: {why}");
            false
        }
    }
}

type Started = (bootinfo::Generation, elf::Loaded, Vec<bitsboot_pure::elf::Segment>, ModuleFile);

/// How a generation is chosen and checked in this boot.
#[derive(Clone, Copy)]
struct Rules {
    /// Safe mode without `gp_stick=on`: `gens::choose_read_only`, no marker is ever written.
    read_only: bool,
    /// A start after a sync flood (RB-14 CR-3): `gens::choose_after_flood`, a new generation keeps its one try.
    flood: bool,
    /// GP-GUARD: the profile keeps the internal disks without a driver, and only the kernel can hold that up. A
    /// KERNEL without the guard's mark (built before `nvme_bind`) would bind the NVMe driver, so it counts as a bad
    /// file: the rule goes on to the next generation, and with none left bitsboot stops and says so.
    guard: bool,
}

/// The marker rule (`bitsboot_pure::gens::choose`, V2-PLAN §7): load what it picks; a generation whose files
/// fail the checks, or whose TRY cannot be written, goes back to the rule, which then picks the next one.
/// `rules` says which form of the rule applies and whether a kernel must carry the disk guard's mark.
fn load_generation(root: &mut Directory, gens_found: &[gens::GenDir], pin: gens::Pin, rules: Rules) -> R<Started> {
    let (mut bad, mut mark_failed, mut problems) = (Vec::new(), Vec::new(), Vec::<String>::new());
    loop {
        let choice = if rules.read_only {
            gens::choose_read_only(gens_found, pin, &bad)
        } else if rules.flood {
            gens::choose_after_flood(gens_found, pin, &bad, &mark_failed)
        } else {
            gens::choose(gens_found, pin, &bad, &mark_failed)
        };
        let (record, mark_try, why) = match choice {
            gens::Choice::Start { record, mark_try, why } => (record, mark_try, why),
            gens::Choice::Refuse(why) => {
                let seen = if problems.is_empty() { String::new() } else { format!(" ({})", problems.join("; ")) };
                return Err(fail!("{why}{seen}"));
            }
        };
        let number = record.number;
        let (kernel, segments, initrd) = match load_files(root, number, rules.guard) {
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
        let unusual = gens::flag::PIN_IGNORED | gens::flag::NO_CONFIRMED_FALLBACK | gens::flag::MARKER_WRITE_FAILED | gens::flag::OTHERS_SKIPPED | gens::flag::FLOOD_START;
        if record.skipped != 0 && record.reason != bootinfo::gen_reason::PINNED || record.flags & unusual != 0 {
            warn!("bitsboot {line}");
        } else {
            info!("bitsboot {line}");
        }
        return Ok((record, kernel, segments, initrd));
    }
}

/// A generation's KERNEL (ELF checks, placed in `BITS_KERNEL` pages) and INITRD (cpio header, `BITS_MODULE`
/// pages). `Err` names the file and its problem; nothing stays allocated then. `guard`: the KERNEL must carry the
/// disk guard's mark in a loaded segment (`gp_platform::disk_rule::marker`).
fn load_files(root: &mut Directory, number: u32, guard: bool) -> Result<(elf::Loaded, Vec<bitsboot_pure::elf::Segment>, ModuleFile), String> {
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
    if guard && !profile::kernel_guarded(&file, &parsed.segs) {
        warn!("bitsboot generation.unguarded number={number}: {kpath} is older than the disk guard; on this machine it would give the internal disks a driver");
        return Err(format!("{kpath}: {}", profile::UNGUARDED));
    }
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
