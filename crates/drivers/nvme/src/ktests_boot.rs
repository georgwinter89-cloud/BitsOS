//! In-kernel tests `nv.boot.*` (Georg 6.11.2026): bitsboot started from the ESP `nvme_boot=` names on an NVMe image
//! built like the Lexar (MSR, Windows, that ESP, the bitsvol window), the USB stick plugged in beside it. Only in the
//! boots of host/tests/nv/43-boot-nvme.sh, which hashes every byte outside the two windows before and after. The
//! `refused_*` ones run in its boots with a broken rule: /boot on the ESP read-only, its window shut at both layers.

use crate::ns::NsDevice;
use crate::{CmdError, Nvme, controllers, settle};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use block::gpt::window::{self, Boot};
use block::{Disk, DiskDevice, DiskKind, StickMatch};
use devmodel::DevError;
use devmodel::block::{BlockDevice, IoPriority};
use fs_ext4::data::{self, Phase};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_WRONLY};

const SEC: u64 = 1_000_000_000;
const SERIAL: &str = "LEXARQEMUZY1234";

fn lexar() -> Result<(Arc<Nvme>, Arc<Disk>), String> {
    if !settle(30 * SEC) {
        return Err(String::from("the controllers did not settle"));
    }
    let c = controllers().into_iter().find(|c| c.info().ident.is_some_and(|i| i.serial == SERIAL)).ok_or("no controller with the test serial")?;
    let d = block::find(&format!("{}n1", c.name)).ok_or("no namespace disk")?;
    Ok((c, d))
}

fn part(d: &Disk, number: u32) -> Result<Arc<Disk>, String> {
    block::find(&block::gpt::part_name(d.name(), number)).ok_or_else(|| format!("partition {number} is not registered"))
}

/// /boot is the named ESP, writable through its own window at both layers, and only below `\BITSOS2\`.
fn boot_esp() -> TestResult {
    let (c, d) = lexar()?;
    kassert!(fs_fat::stick::wait_mounted(120_000).is_some(), "no /boot");
    kassert!(fs_fat::stick::on_nvme(), "/boot is not on the NVMe");
    kassert!(fs_fat::stick::last_reason().is_none(), "/boot is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let esp = d.named_esp().ok_or("the window was armed without nvme_boot=")?;
    let win = d.window().ok_or("no nvme_data= window")?;
    kassert!(d.boot_window() == Some(esp), "the block layer's boot window is {:?}", d.boot_window());
    kassert!(c.window().is_some_and(|w| w.range() == win && w.boot() == Some(esp)), "the controller's windows are {:?}", c.window());
    let want = block::gpt::part_name(d.name(), 3);
    kassert!(fs_fat::stick::disk_name().as_deref() == Some(want.as_str()), "/boot is on {:?}, want {want}", fs_fat::stick::disk_name());
    let ns = vfs::root_ns();
    // Files below \BITSOS2\ only; the ESP's own \EFI\ and its root stay as they are.
    ns.create_file_bytes(b"/boot/BITSOS2/NVBOOT.TXT", 0o644, b"on the Lexar's ESP\r\n").map_err(|e| format!("a file below BITSOS2: {e}"))?;
    kassert!(ns.read_file("/boot/BITSOS2/NVBOOT.TXT", 64).map_err(|e| format!("read back: {e}"))? == b"on the Lexar's ESP\r\n");
    ns.remove("/boot/BITSOS2/NVBOOT.TXT").map_err(|e| format!("remove: {e}"))?;
    let k = vfs::Ctx::kernel(&ns);
    let open = |p: &[u8]| k.open(None, p, O_WRONLY | O_CREAT, 0o644).map(|_| ()).err();
    kassert!(open(b"/boot/NVBOOT.TXT") == Some(EROFS), "the ESP's root took a file");
    kassert!(open(b"/boot/EFI/BOOT/BOOTX64.EFI") == Some(EROFS), "the loader took a write");
    kassert!(k.mkdir(None, b"/boot/EFI/nv-refused", 0o755).err() == Some(EROFS));
    raw_refusals(&c, &d, esp, win)
}
ktest!("nv.boot.esp", Run("nv/43-boot-nvme"), boot_esp);

/// Everything the FAT code did not announce, and everything outside the two windows, at every layer.
fn raw_refusals(c: &Arc<Nvme>, d: &Arc<Disk>, esp: (u64, u64), win: (u64, u64)) -> TestResult {
    let bs = d.block_size() as usize;
    let one = vec![0xE5u8; bs];
    let m0 = c.stats.media_writes();
    let p3 = part(d, 3)?;
    for lba in [0u64, 1, 6, 32, esp.1 - 1] {
        kassert!(p3.write(lba, &one, false, IoPriority::Normal).is_err(), "the ESP took a raw write at {lba}");
    }
    kassert!(p3.discard(0, 1, IoPriority::Normal).is_err(), "the ESP took a discard");
    for n in [1u32, 2] {
        let p = part(d, n)?;
        kassert!(p.write(0, &one, false, IoPriority::Normal).is_err() && p.write_policy().is_err(), "Windows partition {n} takes writes");
    }
    for lba in [0u64, 1, esp.0, win.0] {
        kassert!(d.write(lba, &one, false, IoPriority::Normal).is_err(), "the whole disk took a write at {lba}");
    }
    let (eport, dport) = (d.part_port(esp.0, esp.1), d.part_port(win.0, win.1));
    kassert!(eport.write_blocks(esp.0 - 1, &one, false, IoPriority::Normal) == Err(DevError::Refused), "ESP port, first-1");
    kassert!(eport.write_blocks(esp.0 + esp.1, &one, false, IoPriority::Normal) == Err(DevError::Refused), "ESP port, last+1");
    kassert!(dport.write_blocks(esp.0, &one, false, IoPriority::Normal) == Err(DevError::Refused), "the data port into the ESP");
    let dev = NsDevice::new(c.clone(), 1).ok_or("no device")?;
    let windows = d.scan().gpt.ok_or("no GPT")?.entries.iter().filter(|e| e.number <= 2).map(|e| e.first).collect::<alloc::vec::Vec<_>>();
    for lba in [0u64, 1, esp.0 - 1].into_iter().chain(windows) {
        kassert!(dev.write_blocks(lba, &one, false, IoPriority::Normal) == Err(DevError::Refused), "the driver took a write at {lba}");
        kassert!(c.write(1, lba, &one, false) == Err(CmdError::OutsideWindow), "the controller took a write at {lba}");
    }
    kassert!(c.stats.media_writes() == m0, "a refused request reached the queue");
    Ok(())
}

/// The USB stick plugged in beside the NVMe boot holds the same STICK.ID but another block 0: an ordinary disk.
fn stick_beside() -> TestResult {
    for _ in 0..1500 {
        if let Some(u) = block::disks().into_iter().find(|x| x.kind() == DiskKind::Usb) {
            kassert!(u.stick() == StickMatch::No, "the stick matched bitsboot's block 0");
            kassert!(u.write(0, &vec![0u8; u.block_size() as usize], false, IoPriority::Normal).is_err(), "the stick took a write");
            kassert!(u.write_policy().is_err());
            // Its usb-msd unit refuses by itself too: /boot on the NVMe is no USB stick (`usb_writes_possible`).
            let sw = block::policy::switches();
            kassert!(!sw.stick_write && !sw.usb_msd_write, "the scenario's CONFIG turned a USB switch on");
            kassert!(u.dev_info().read_only, "the stick's unit takes writes with both USB switches off");
            kassert!(fs_fat::stick::on_nvme(), "/boot moved to the stick");
            return Ok(());
        }
        sched::sleep_ns(20_000_000);
    }
    Err(String::from("no USB disk appeared"))
}
ktest!("nv.boot.stick_beside", Run("nv/43-boot-nvme"), stick_beside);

/// The window's pool, made like bitsdisk makes it, for the next boot's /data; no data.img on the NVMe's /boot.
fn pool() -> TestResult {
    let (_, d) = lexar()?;
    let Boot::Armed { part, .. } = window::boot_state() else { return Err(String::from("the window is not armed")) };
    let p = block::find(&part).ok_or("the window's partition is not registered")?;
    let ph = data::wait_done(120_000);
    kassert!(ph == Phase::Skipped("boot volume on the NVMe, no window"), "phase {ph:?}");
    kassert!(vfs::root_ns().lookup_path(b"/boot/BITSOS2/data.img", true).is_err(), "a data.img on the NVMe's /boot");
    fs_ext4::data::format_pool(Arc::new(DiskDevice(p.clone())), b"bitsdata").map_err(|e| format!("format_pool: {e}"))?;
    kassert!(fs_ext4::data::look_at_pool(Arc::new(DiskDevice(p))) == fs_ext4::data::PoolLook::Data, "no pool after the format");
    kassert!(d.boot_window().is_some(), "making the pool closed /boot's window");
    Ok(())
}
ktest!("nv.boot.pool", Run("nv/43-boot-nvme"), pool);

/// /boot is the ESP, mounted read-only for `want`: no file below `\BITSOS2\`, the boot window shut at both layers.
fn boot_refused(c: &Arc<Nvme>, d: &Arc<Disk>, want: &str) -> TestResult {
    kassert!(fs_fat::stick::wait_mounted(120_000).is_some(), "no /boot");
    kassert!(fs_fat::stick::on_nvme(), "/boot is not on the NVMe");
    let why = fs_fat::stick::last_reason().ok_or("/boot is writable")?;
    kassert!(why.contains(want), "/boot is read-only for \"{why}\", want \"{want}\"");
    let want_disk = block::gpt::part_name(d.name(), 3);
    kassert!(fs_fat::stick::disk_name().as_deref() == Some(want_disk.as_str()), "/boot is on {:?}", fs_fat::stick::disk_name());
    kassert!(d.boot_window().is_none() && c.window().is_none_or(|w| w.boot().is_none()), "a boot window is open: {:?} {:?}", d.boot_window(), c.window());
    let ns = vfs::root_ns();
    let k = vfs::Ctx::kernel(&ns);
    kassert!(k.open(None, b"/boot/BITSOS2/NVREFUSED.TXT", O_WRONLY | O_CREAT, 0o644).err() == Some(EROFS), "a file below BITSOS2 was made");
    let one = vec![0xE5u8; d.block_size() as usize];
    let m0 = c.stats.media_writes();
    let p3 = part(d, 3)?;
    for lba in [0u64, 32, 100] {
        kassert!(p3.write(lba, &one, false, IoPriority::Normal).is_err(), "the ESP took a raw write at {lba}");
    }
    kassert!(c.stats.media_writes() == m0, "a refused request reached the queue");
    Ok(())
}

/// nvme_boot= names the ESP with a wrong size: the disk's window is refused as a whole (an EFI System partition
/// nobody named), so the namespace and /boot stay read-only and /data has nothing.
fn refused_key() -> TestResult {
    let (c, d) = lexar()?;
    kassert!(matches!(window::boot_state(), Boot::Refused { word: "efi", .. }), "the window: {:?}", window::boot_state());
    kassert!(d.window().is_none() && c.window().is_none(), "a window is armed");
    boot_refused(&c, &d, "write-protected or refuses writes")?;
    let ph = data::wait_done(120_000);
    kassert!(ph == Phase::Skipped("boot volume on the NVMe, no window"), "phase {ph:?}");
    Ok(())
}
ktest!("nv.boot.refused_key", Run("nv/43-boot-nvme"), refused_key);

/// The keys are right but the ESP's label is not BITSOS: nvme_data's window and /data on its pool as always, /boot
/// read-only, its window never opened.
fn refused_label() -> TestResult {
    let (c, d) = lexar()?;
    kassert!(matches!(window::boot_state(), Boot::Armed { .. }), "the window: {:?}", window::boot_state());
    kassert!(d.window().is_some() && d.named_esp().is_some(), "the window is not armed with nvme_boot=");
    boot_refused(&c, &d, "label is not BITSOS")?;
    let ph = data::wait_done(120_000);
    kassert!(matches!(ph, Phase::Mounted { pool: true, writable: true, .. }), "phase {ph:?}");
    Ok(())
}
ktest!("nv.boot.refused_label", Run("nv/43-boot-nvme"), refused_label);
