//! SYS-K in-kernel tests of the measurement files AB's procfs makes (all `Safe`, rerunnable): `/proc/diskstats` and
//! `/sys/block` against the block layer (a read moves the numbers), `/proc/<pid>/io` of a held process, the DMI
//! identity against what boot read, the USB devices against the registry, `/proc/net/wireless`, and the ABI program
//! `/bin/abi-sysk` read as uid 1000 (every line "yes").

use crate::ab7_ktests::text;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;

fn mode(path: &str) -> Result<u32, String> {
    Ok(vfs::root_ns().lookup(path).map_err(|e| format!("{path}: errno {e}"))?.inode.meta.mode() & 0o7777)
}

fn missing(path: &str) -> bool {
    vfs::root_ns().lookup(path).err() == Some(ENOENT)
}

/// The numbers of `name`'s /proc/diskstats line.
fn line_of(name: &str) -> Result<Vec<u64>, String> {
    let t = text("/proc/diskstats")?;
    let l = t.lines().find(|l| l.split_whitespace().nth(2) == Some(name)).ok_or(format!("/proc/diskstats has no {name}: {t}"))?;
    let w: Vec<&str> = l.split_whitespace().collect();
    kassert!(w.len() == 20, "{l}");
    w.iter().enumerate().filter(|(i, _)| *i != 2).map(|(_, x)| x.parse::<u64>().map_err(|_| format!("{l}"))).collect()
}

/// Waits (at most 30 s: the boot stick is found a few seconds into the boot) until `ready` holds.
fn wait_for(what: &str, ready: impl Fn() -> bool) -> TestResult {
    for _ in 0..150 {
        if ready() {
            return Ok(());
        }
        sched::sleep_ms(200);
    }
    Err(format!("{what} did not appear in 30 s"))
}

fn diskstats() -> TestResult {
    kassert!(mode("/proc/diskstats")? == 0o444);
    wait_for("a disk", || block::disks().iter().any(|d| d.kind() != block::DiskKind::Stacked))?;
    let all = block::disks();
    let whole: Vec<_> = all.iter().filter(|d| d.kind() != block::DiskKind::Stacked).collect();
    kassert!(!whole.is_empty(), "no disk at all (the boot stick is one)");
    for d in &whole {
        line_of(d.name())?;
    }
    // GP-R2: a disk without a medium (a card reader's slot, the Gaming-PC's Brother device: blocks=0) is listed as
    // Linux lists it (block/genhd.c:1378-1380 drops only empty partitions) and has its size 0, but no block to read.
    let empty: Vec<_> = whole.iter().filter(|d| d.dev_info().blocks == 0).collect();
    for d in &empty {
        kassert!(text(&format!("/sys/block/{}/size", d.name()))? == "0\n", "{}: an empty disk's size", d.name());
    }
    let readable: Vec<_> = whole.iter().filter(|d| d.dev_info().blocks > 0).collect();
    kassert!(!readable.is_empty(), "no disk with a medium (the boot stick is one)");
    // A read of block 0 moves the disk's read numbers: the boot stick's (never the Mini-PC's NVMe, though a read
    // would be harmless).
    let d = **readable.iter().find(|d| d.kind() == block::DiskKind::Usb).or_else(|| readable.iter().find(|d| d.kind() == block::DiskKind::Ram)).unwrap_or(&readable[0]);
    let bs = d.block_size() as usize;
    let before = line_of(d.name())?;
    let mut b = alloc::vec![0u8; bs];
    d.read(0, &mut b, Default::default()).map_err(|e| format!("read {}: {e:?}", d.name()))?;
    let after = line_of(d.name())?;
    // Columns after major, minor: reads, merges, sectors, ms (index 2.. of the numbers).
    kassert!(after[2] > before[2] && after[4] >= before[4] + (bs / 512) as u64, "{}: {before:?} -> {after:?}", d.name());
    let dir = format!("/sys/block/{}", d.name());
    let info = d.dev_info();
    kassert!(text(&format!("{dir}/size"))? == format!("{}\n", info.blocks * bs as u64 / 512));
    let stat = text(&format!("{dir}/stat"))?;
    kassert!(stat.split_whitespace().count() == 17 && stat.split_whitespace().all(|x| x.parse::<u64>().is_ok()), "stat {stat:?}");
    kassert!(matches!(text(&format!("{dir}/queue/rotational"))?.as_str(), "0\n" | "1\n"));
    kassert!(text(&format!("{dir}/dev"))? == format!("{}:{}\n", after[0], after[1]), "dev and diskstats agree");
    for f in ["queue/hw_sector_size", "queue/logical_block_size", "queue/physical_block_size"] {
        kassert!(text(&format!("{dir}/{f}"))? == format!("{bs}\n"), "{dir}/{f}");
    }
    kassert!(text(&format!("{dir}/removable"))? == format!("{}\n", u8::from(d.removable())), "{dir}/removable");
    for f in ["size", "stat", "dev", "ro", "removable", "queue/rotational", "queue/hw_sector_size", "queue/logical_block_size", "queue/physical_block_size"] {
        kassert!(mode(&format!("{dir}/{f}"))? == 0o444, "{dir}/{f}");
    }
    let per = bs as u64 / 512;
    for (first, blocks, _, n) in d.scan().parts {
        let p = block::gpt::part_name(d.name(), n);
        kassert!(text(&format!("{dir}/{p}/start"))? == format!("{}\n", first * per), "{p} start");
        kassert!(text(&format!("{dir}/{p}/size"))? == format!("{}\n", blocks * per), "{p} size");
        kassert!(text(&format!("{dir}/{p}/partition"))? == format!("{n}\n"), "{p} number");
    }
    Ok(())
}
ktest!("so.sysk.diskstats", Safe, diskstats);

fn pid_io() -> TestResult {
    let mut o = proc::SpawnOptions::new(&["hello"], &["HOME=/"], policy::ProgramId(1));
    o.held = true;
    let p = proc::spawn_with("/bin/hello", &o).map_err(|e| format!("spawn /bin/hello: {e}"))?;
    let r = (|| -> TestResult {
        let path = format!("/proc/{}/io", p.pid);
        let n = vfs::root_ns().lookup(&path).map_err(|e| format!("{path}: errno {e}"))?;
        kassert!(n.inode.meta.mode() & 0o7777 == 0o400 && n.inode.meta.uid() == 1000, "{path}: mode {:o} uid {}", n.inode.meta.mode(), n.inode.meta.uid());
        let t = text(&path)?;
        let keys: Vec<&str> = t.lines().map(|l| l.split(": ").next().unwrap_or("")).collect();
        kassert!(keys == ["rchar", "wchar", "syscr", "syscw", "read_bytes", "write_bytes", "cancelled_write_bytes"], "{t}");
        kassert!(t.lines().all(|l| l.split(": ").nth(1).is_some_and(|v| v.parse::<u64>().is_ok())), "{t}");
        let tid = text(&format!("/proc/{}/task/{}/io", p.pid, p.pid))?;
        kassert!(tid.lines().count() == 7, "the thread's io: {tid}");
        Ok(())
    })();
    p.kill(uapi::ab::signal::SIGKILL);
    kassert!(p.wait(Some(10 * SEC)).is_some(), "the held process did not end");
    r
}
ktest!("so.sysk.pid_io", Safe, pid_io);

fn dmi() -> TestResult {
    let f = acpi::dmi::fields();
    let handed = kexec::boot_info().is_some_and(|(bi, _)| bi.smbios().is_some());
    kassert!(!handed || !f.is_empty(), "bitsboot handed over an SMBIOS table but nothing was read from it");
    if f.is_empty() {
        kassert!(missing("/sys/class/dmi"), "dmi files without an SMBIOS table");
        return Ok(());
    }
    for x in f {
        let p = format!("/sys/class/dmi/id/{}", x.name);
        kassert!(text(&p)? == format!("{}\n", x.text), "{p}");
        kassert!(mode(&p)? == x.mode, "{p} mode {:o}", mode(&p)?);
    }
    for secret in ["product_serial", "product_uuid", "board_serial", "chassis_serial"] {
        kassert!(f.iter().filter(|x| x.name == secret).all(|x| x.mode == 0o400), "{secret} is not root's");
    }
    kassert!(f.iter().any(|x| x.name == "sys_vendor") && f.iter().any(|x| x.name == "bios_vendor"), "no vendor in the SMBIOS table: {:?}", f.iter().map(|x| x.name).collect::<Vec<_>>());
    Ok(())
}
ktest!("so.sysk.dmi", Safe, dmi);

fn usb() -> TestResult {
    // QEMU and the Mini-PC both boot from a USB stick.
    wait_for("a USB device", || usb_core::registry::devices().iter().any(|d| !d.is_gone()))?;
    let devs: Vec<_> = usb_core::registry::devices().into_iter().filter(|d| !d.is_gone()).collect();
    let listed = vfs::root_ns().list_dir("/sys/bus/usb/devices").map_err(|e| format!("/sys/bus/usb/devices: errno {e}"))?;
    kassert!(listed.len() <= devs.len(), "{} directories for {} devices", listed.len(), devs.len());
    for d in &devs {
        let s = d.speed();
        let dir = format!("/sys/bus/usb/devices/{}-{}", crate::procfs_usb_busnum(d.host_name(), s), d.place.path());
        kassert!(text(&format!("{dir}/idVendor"))? == format!("{:04x}\n", d.vendor()), "{dir}");
        kassert!(text(&format!("{dir}/idProduct"))? == format!("{:04x}\n", d.product()), "{dir}");
        kassert!(matches!(text(&format!("{dir}/speed"))?.as_str(), "1.5\n" | "12\n" | "480\n" | "5000\n" | "10000\n"), "{dir}/speed");
        kassert!(mode(&format!("{dir}/bDeviceClass"))? == 0o444 && missing(&format!("{dir}/manufacturer")), "{dir}");
    }
    Ok(())
}
ktest!("so.sysk.usb", Safe, usb);

fn wireless() -> TestResult {
    const P: &str = "/proc/net/wireless";
    kassert!(mode(P)? == 0o444);
    let t = text(P)?;
    let lines: Vec<&str> = t.lines().collect();
    kassert!(lines.len() >= 2 && lines[0].starts_with("Inter-| sta-|") && lines[1].ends_with("| beacon | 22"), "{t}");
    let cards = net::netdev::devices().iter().filter(|d| d.wireless().is_some()).count();
    kassert!(lines.len() - 2 <= cards, "{} lines for {cards} wireless cards: {t}", lines.len() - 2);
    for l in &lines[2..] {
        kassert!(l.split_whitespace().count() == 11, "{l}");
    }
    Ok(())
}
ktest!("so.sysk.wireless", Safe, wireless);

const PROG: &str = "/bin/abi-sysk";

/// The ABI program as uid 1000 (SpawnOptions' default): every relation holds. `strict` (the Mini-PC): the Ryzen's
/// and 780M's sources must be there too (five more lines).
fn abi_program(strict: bool) -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::fail("the ABI programs are not in this image (no musl sysroot when it was built)");
    }
    // The program wants a block device: the boot stick is found a few seconds into the boot.
    wait_for("a disk", || block::disks().iter().any(|d| d.kind() != block::DiskKind::Stacked))?;
    let out = proc::stdio::Capture::new(1 << 16);
    let argv: &[&str] = if strict { &["abi-sysk", "strict"] } else { &["abi-sysk"] };
    let mut o = proc::SpawnOptions::new(argv, &["HOME=/", "PATH=/bin"], policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {PROG}: {e}"))?;
    let ended = p.wait(Some(60 * SEC)).is_some();
    if !ended {
        p.kill(uapi::ab::signal::SIGKILL);
        p.wait(Some(5 * SEC));
    }
    let t = String::from_utf8_lossy(&out.contents()).into_owned();
    kassert!(ended && p.status() == Some(proc::ExitStatus::Exited(0)), "{PROG} ended {:?}: {t}", p.status());
    let no: Vec<&str> = t.lines().filter(|l| !l.ends_with(": yes")).collect();
    let lines = if strict { 24 } else { 19 };
    kassert!(no.is_empty() && t.lines().count() == lines, "{PROG}: {no:?}\n{t}");
    Ok(())
}
fn abi_any() -> TestResult {
    abi_program(false)
}
ktest!("so.sysk.abi", Safe, abi_any);
fn abi_strict() -> TestResult {
    abi_program(true)
}
ktest!("so.sysk.abi_strict", Hardware, abi_strict);
