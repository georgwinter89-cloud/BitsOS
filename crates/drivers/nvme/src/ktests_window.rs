//! In-kernel tests `nv.window.*` (NV-2p), only in the boots of host/tests/nv/4x-window*.sh: an NVMe image built like
//! Georg's Lexar SSD. Those scripts hash every byte outside the window before and after. `nv_expect=` says what the
//! boot was given.

use crate::ns::NsDevice;
use crate::{CmdError, Nvme, controllers, settle};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use block::gpt::window::{self, Boot};
use block::{Disk, DiskDevice};
use core::sync::atomic::Ordering;
use devmodel::DevError;
use devmodel::block::{BlockDevice, IoPriority};
use fs_ext4::data::{self, Phase};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;
const SERIAL: &str = "LEXARQEMUZY1234";

fn lexar() -> Result<(Arc<Nvme>, Arc<Disk>), String> {
    kassert_settled()?;
    let c = controllers().into_iter().find(|c| c.info().ident.is_some_and(|i| i.serial == SERIAL)).ok_or("no controller with the test serial")?;
    let d = block::find(&format!("{}n1", c.name)).ok_or("no namespace disk")?;
    Ok((c, d))
}

fn kassert_settled() -> Result<(), String> {
    if settle(30 * SEC) { Ok(()) } else { Err(String::from("the controllers did not settle")) }
}

fn every_write_refused(c: &Arc<Nvme>, d: &Arc<Disk>) -> TestResult {
    let bs = d.block_size() as usize;
    let b = vec![0x5Au8; bs];
    let before = d.stats().refused;
    kassert!(d.write(0, &b, false, IoPriority::Normal).is_err(), "whole disk, LBA 0");
    kassert!(d.write_policy().is_err(), "whole disk, policy");
    for e in d.scan().gpt.iter().flat_map(|g| g.entries.iter()) {
        let p = block::find(&block::gpt::part_name(d.name(), e.number)).ok_or("partition missing")?;
        kassert!(p.write(0, &b, false, IoPriority::Normal).is_err(), "partition {} took a write", e.number);
        kassert!(p.discard(0, 1, IoPriority::Normal).is_err(), "partition {} took a discard", e.number);
        kassert!(p.write_policy().is_err(), "partition {} says writable", e.number);
    }
    kassert!(d.stats().refused > before);
    let dev = NsDevice::new(c.clone(), 1).ok_or("no device")?;
    kassert!(dev.info().read_only);
    kassert!(dev.write_blocks(0, &b, false, IoPriority::Normal) == Err(DevError::Refused));
    kassert!(c.write(1, 0, &b, false) == Err(CmdError::OutsideWindow));
    kassert!(c.deallocate(1, 0, 1) == Err(CmdError::OutsideWindow));
    kassert!(c.stats.media_writes() == 0, "{} media-changing commands", c.stats.media_writes());
    Ok(())
}

fn data_on_the_stick() -> TestResult {
    let p = data::wait_done(120_000);
    kassert!(matches!(p, Phase::Mounted { pool: true, writable: true, .. }), "phase {p:?}");
    let m = data::mounted().ok_or("no /data")?;
    kassert!(m.disk.as_deref().is_some_and(|d| d.starts_with("data.")), "disk {:?}", m.disk);
    let ns = vfs::root_ns();
    let a = ns.read_file("/run/bitsos/data-state", 256).map_err(|e| format!("data-state: {e}"))?;
    kassert!(a.starts_with(b"mounted pool rw"), "data-state: {}", String::from_utf8_lossy(&a));
    ns.create_file_bytes(b"/data/nv-window-fallback", 0o644, b"up").map_err(|e| format!("a file in /data: {e}"))?;
    Ok(())
}

/// A boot whose keys are refused: `nv_expect=<word>` is what the state must say. Nothing writes, /data on data.img.
fn refused() -> TestResult {
    let want = pci::registry::config_value("nv_expect").ok_or("no nv_expect=")?;
    let (c, d) = lexar()?;
    let got = window_word();
    kassert!(got == want, "the window says {got}, want {want}");
    every_write_refused(&c, &d)?;
    kassert!(d.window().is_none() && c.window().is_none());
    data_on_the_stick()
}
ktest!("nv.window.refused", Run("nv/40-window"), refused);

fn window_word() -> String {
    match window::boot_state() {
        Boot::Armed { .. } => String::from("armed"),
        Boot::Refused { word, .. } => String::from(word),
        Boot::Looking if !window::config_has_key() => String::from("no_nvme_data"),
        Boot::Looking if !window::write_key_on() => String::from("no_nvme_write"),
        Boot::Looking => String::from("looking"),
    }
}

/// Inside: the bytes are written back as they are, so this also runs over a pool in use.
fn rewrite(p: &Arc<Disk>, lba: u64, n: u64, fua: bool) -> Result<(), String> {
    let bs = p.block_size() as usize;
    let v = p.read_vec(lba, n as usize * bs, IoPriority::Normal).map_err(|e| format!("read {lba}: {}", e.as_str()))?;
    p.write_vec(lba, v, fua, IoPriority::Normal).map(|_| ()).map_err(|e| format!("write {lba}: {}", e.as_str()))
}

/// The refused edges and the whole-disk and other-partition devices, at every layer.
fn edges(c: &Arc<Nvme>, d: &Arc<Disk>, part: &Arc<Disk>, first: u64, blocks: u64) -> TestResult {
    let bs = d.block_size() as usize;
    let (one, two) = (vec![0xEEu8; bs], vec![0xEEu8; 2 * bs]);
    let last = first + blocks - 1;
    let n0 = c.stats.window_refused.load(Ordering::Relaxed);
    let m0 = c.stats.media_writes();
    let dev = NsDevice::new(c.clone(), 1).ok_or("no device")?;
    kassert!(!dev.info().read_only);
    for (lba, b, what) in [(first - 1, &one, "first-1"), (last + 1, &one, "last+1"), (first - 1, &two, "across the start"), (last, &two, "across the end"), (0, &one, "LBA 0"), (1, &one, "the GPT")] {
        kassert!(dev.write_blocks(lba, b, false, IoPriority::Normal) == Err(DevError::Refused), "the driver took a write at {what}");
    }
    kassert!(dev.discard(first - 1, 1) == Err(DevError::Refused) && dev.discard(last, 2) == Err(DevError::Refused));
    kassert!(c.stats.window_refused.load(Ordering::Relaxed) == n0 + 8, "the refusals are counted");
    kassert!(c.stats.media_writes() == m0, "a refused request reached the queue");
    // The block layer: the whole disk, inside the range too; every other partition.
    kassert!(d.write(first, &one, false, IoPriority::Normal).is_err(), "whole disk, inside the window");
    kassert!(d.write(0, &one, false, IoPriority::Normal).is_err(), "whole disk");
    kassert!(d.discard(first, 1, IoPriority::Normal).is_err(), "whole disk, discard");
    kassert!(d.write_policy().is_err());
    let port = d.part_port(first, blocks);
    kassert!(port.write_blocks(first - 1, &one, false, IoPriority::Normal) == Err(DevError::Refused), "port, first-1");
    kassert!(port.write_blocks(last, &two, false, IoPriority::Normal) == Err(DevError::Refused), "port, across the end");
    let scan = d.scan();
    for e in scan.gpt.iter().flat_map(|g| g.entries.iter()).filter(|e| e.first != first) {
        let p = block::find(&block::gpt::part_name(d.name(), e.number)).ok_or("partition missing")?;
        kassert!(p.write(0, &one, false, IoPriority::Normal).is_err(), "partition {} took a write", e.number);
        kassert!(p.discard(0, 1, IoPriority::Normal).is_err(), "partition {} took a discard", e.number);
        kassert!(p.write_policy().is_err());
        let o = d.part_port(e.first, e.last - e.first + 1);
        kassert!(o.write_blocks(first, &one, false, IoPriority::Normal) == Err(DevError::Refused), "another partition's port, inside the window");
    }
    // The partition: its range ends where the window ends.
    kassert!(part.write(blocks, &one, false, IoPriority::Normal).is_err(), "past the partition");
    kassert!(part.write(blocks - 1, &two, false, IoPriority::Normal).is_err(), "across the partition's end");
    kassert!(c.stats.media_writes() == m0, "a refused request reached the queue");
    Ok(())
}

/// The raw window (no pool yet): edges, first and last block, a burst, discards, then pool and ext4. /data stays on
/// the stick for lack of a pool. The script compares every byte outside.
fn raw() -> TestResult {
    let (c, d) = lexar()?;
    let (first, blocks) = d.window().ok_or("no window armed")?;
    let Boot::Armed { part, .. } = window::boot_state() else { return Err(format!("window: {}", window_word())) };
    let p = block::find(&part).ok_or("the partition is not registered")?;
    kassert!(p.write_policy().is_ok(), "the window's partition refuses");
    edges(&c, &d, &p, first, blocks)?;
    let bs = p.block_size() as usize;
    rewrite(&p, 0, 1, false)?;
    rewrite(&p, blocks - 1, 1, true)?;
    rewrite(&p, 0, blocks.min(16), true)?;
    // The window's two ends, written with real bytes and read back from the device.
    let mark = vec![0xC3u8; bs];
    p.write(blocks - 1, &mark, true, IoPriority::Normal).map_err(|e| e.as_str())?;
    p.flush(IoPriority::Normal).map_err(|e| e.as_str())?;
    kassert!(p.read_vec(blocks - 1, bs, IoPriority::Normal).map_err(|e| e.as_str())? == mark);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let m0 = c.stats.media_writes();
    for _ in 0..2000 {
        let n = 1 + next() % 256;
        let at = next() % (blocks - n);
        let b = vec![next() as u8; n as usize * bs];
        p.write(at, &b, next() % 8 == 0, IoPriority::Normal).map_err(|e| format!("burst at {at}+{n}: {}", e.as_str()))?;
    }
    kassert!(p.dev_info().supports_discard, "QEMU's namespace has Dataset Management");
    for _ in 0..200 {
        let n = 1 + next() % 65536;
        let at = next() % (blocks - n);
        p.discard(at, n, IoPriority::Normal).map_err(|e| format!("discard at {at}+{n}: {}", e.as_str()))?;
    }
    p.flush(IoPriority::Normal).map_err(|e| e.as_str())?;
    kassert!(c.stats.media_writes() > m0 + 2000, "{} commands", c.stats.media_writes() - m0);
    // The window's partition is written now, the pool made like bitsdisk makes it, and read back.
    kassert!(data::wait_done(120_000) != Phase::Waiting);
    fs_ext4::data::format_pool(Arc::new(DiskDevice(p.clone())), b"bitsdata").map_err(|e| format!("format_pool: {e}"))?;
    kassert!(fs_ext4::data::look_at_pool(Arc::new(DiskDevice(p.clone()))) == fs_ext4::data::PoolLook::Data, "no pool after the format");
    edges(&c, &d, &p, first, blocks)?;
    data_on_the_stick()
}
ktest!("nv.window.raw", Run("nv/40-window"), raw);
ktest!("nv.window.pool_first", Run("nv/41-window-reload"), raw);

/// A valid window with its pool: /data lies on the partition, the same stack as on the stick; a file on it; the
/// refused edges still refused; rewrites at both ends are harmless. Run in the boots with the pool.
fn armed() -> TestResult {
    let (c, d) = lexar()?;
    let (first, blocks) = d.window().ok_or("no window armed")?;
    let Boot::Armed { part, .. } = window::boot_state() else { return Err(format!("window: {}", window_word())) };
    let p = block::find(&part).ok_or("the partition is not registered")?;
    let ph = data::wait_done(120_000);
    kassert!(ph == Phase::Mounted { pool: true, writable: true, created: false }, "phase {ph:?}");
    let m = data::mounted().ok_or("no /data")?;
    kassert!(m.pool && m.handle.is_some(), "not a pool");
    edges(&c, &d, &p, first, blocks)?;
    rewrite(&p, blocks - 1, 1, true)?;
    let ns = vfs::root_ns();
    ns.create_file_bytes(b"/data/nv-window-file", 0o644, b"on the partition").map_err(|e| format!("create: {e}"))?;
    kassert!(ns.read_file("/data/nv-window-file", 64).map_err(|e| format!("read: {e}"))? == b"on the partition");
    let a = ns.read_file("/run/bitsos/data-state", 256).map_err(|e| format!("data-state: {e}"))?;
    kassert!(a == b"mounted pool rw\n", "data-state: {}", String::from_utf8_lossy(&a));
    Ok(())
}
ktest!("nv.window.armed", Run("nv/42-window-setup"), armed);

/// A boot with its window armed and a second raw partition: bitsdisk may not take the window for that one. Without
/// this the setup's arm and disarm would leave /data's partition without writes until the next boot.
fn busy() -> TestResult {
    let (c, d) = lexar()?;
    let (first, blocks) = d.window().ok_or("no window armed")?;
    let Boot::Armed { part, .. } = window::boot_state() else { return Err(format!("window: {}", window_word())) };
    let p = block::find(&part).ok_or("the partition is not registered")?;
    let list = crate::setup::command("list\n");
    kassert!(list.lines().any(|l| l.starts_with("part\t4\t") && l.contains("window_busy")), "{list}");
    let a = crate::setup::command(&format!("prepare {} 4\n", d.name()));
    kassert!(a.starts_with("refused window_busy "), "{a}");
    let a = crate::setup::command("confirm\n");
    kassert!(a.starts_with("refused no_prepare "), "{a}");
    kassert!(d.window() == Some((first, blocks)), "the disk's window changed");
    kassert!(c.window().map(|w| w.range()) == Some((first, blocks)), "the controller's window changed");
    rewrite(&p, 0, 1, true)?;
    edges(&c, &d, &p, first, blocks)
}
ktest!("nv.window.busy", Run("nv/40-window"), busy);

/// Keys set and no NVMe controller at all (the SSD taken out): the driver's verdict comes without any of its code
/// having run by itself, and /data goes on without waiting out its bound.
fn no_controller() -> TestResult {
    kassert!(controllers().is_empty(), "this boot has no NVMe controller");
    kassert!(window::config_has_key() && window::write_key_on(), "both keys are set");
    data_on_the_stick()?;
    let got = window_word();
    kassert!(got == "serial", "the window says {got}");
    kassert!(window::boot_state() != Boot::Looking);
    Ok(())
}
ktest!("nv.window.no_controller", Run("nv/40-window"), no_controller);

const BIG: u64 = 5 << 30;
const STEP: usize = 1 << 20;
const PACE: u64 = 64 << 20;

/// A byte pattern that differs per MiB.
fn big_pattern(buf: &mut [u8], off: u64) {
    let seed = (off / STEP as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for (i, c) in buf.chunks_exact_mut(8).enumerate() {
        c.copy_from_slice(&seed.wrapping_mul(i as u64 + 1).to_le_bytes());
    }
}

/// Files over 4 GiB: 5 GiB written through /data, synced, read back and compared, the size above 32 bits.
fn big() -> TestResult {
    let (_, d) = lexar()?;
    kassert!(d.window().is_some(), "no window armed");
    let ph = data::wait_done(120_000);
    kassert!(matches!(ph, Phase::Mounted { pool: true, writable: true, .. }), "phase {ph:?}");
    const PATH: &[u8] = b"/data/nv-big-file";
    let ns = vfs::root_ns();
    let c = vfs::Ctx::kernel(&ns);
    let f = c.open(None, PATH, vfs::abi::O_CREAT | vfs::abi::O_RDWR | vfs::abi::O_TRUNC, 0o644).map_err(|e| format!("create: {e}"))?;
    let mut buf: Vec<u8> = vec![0u8; STEP];
    let mut at = 0;
    while at < BIG {
        big_pattern(&mut buf, at);
        // A write may stop early, as on Linux: the rest goes on.
        let mut done = 0;
        while done < STEP {
            let n = f.write(&buf[done..]).map_err(|e| format!("write at {}: {e}", at + done as u64))?;
            kassert!(n != 0, "no progress at {}", at + done as u64);
            done += n;
        }
        at += STEP as u64;
        // Paced: nothing in the kernel holds a writer back yet (NV-2p change request 2: a writer faster than the disk
        // fills memory with dirty pages until write() answers ENOMEM), and this test is about the file's size.
        if at.is_multiple_of(PACE) {
            f.inode().ok_or("no inode")?.sync(vfs::pagecache::SyncKind::Fsync).map_err(|e| format!("fsync at {at}: {e}"))?;
        }
    }
    let inode = f.inode().ok_or("no inode")?;
    inode.sync(vfs::pagecache::SyncKind::Fsync).map_err(|e| format!("fsync: {e}"))?;
    kassert!(inode.size() == BIG, "size {}", inode.size());
    let attr = c.stat(None, PATH, false).map_err(|e| format!("stat: {e}"))?;
    kassert!(attr.size as u64 == BIG, "stat size {}", attr.size);
    let r = c.open(None, PATH, vfs::abi::O_RDONLY, 0).map_err(|e| format!("open: {e}"))?;
    let mut want = vec![0u8; STEP];
    let mut at = 0;
    while at < BIG {
        big_pattern(&mut want, at);
        let mut got = 0;
        while got < STEP {
            let n = r.read(&mut buf[got..]).map_err(|e| format!("read at {at}: {e}"))?;
            kassert!(n != 0, "end of file at {}", at + got as u64);
            got += n;
        }
        kassert!(buf == want, "the bytes differ in the MiB at {at}");
        at += STEP as u64;
    }
    // The file from before the power-off (42-window-setup.sh) is still there.
    kassert!(ns.read_file("/data/home/1000/nv-before-poweroff", 64).map_err(|e| format!("the file from before the power-off: {e}"))? == b"kept over the power-off\n");
    // Past the end nothing.
    let mut one = [0u8; 1];
    kassert!(r.read(&mut one).map_err(|e| format!("read at the end: {e}"))? == 0);
    Ok(())
}
ktest!("nv.window.big", Run("nv/42-window-setup"), big);

/// The file tools' guard refuses the window key on the real stick (the Mini-PC's way in over MCP).
fn no_key_over_files() -> TestResult {
    kassert!(fs_fat::stick::wait_mounted(120_000).is_some(), "no stick");
    let ns = vfs::root_ns();
    let c = vfs::Ctx::kernel(&ns);
    let f = c.open(None, b"/boot/BITSOS2/CONFIG.TXT", vfs::abi::O_WRONLY | vfs::abi::O_APPEND, 0).map_err(|e| format!("open: {e}"))?;
    kassert!(f.write(b"\nnvme_data=X:00000000-0000-0000-0000-000000000000:1:2\n").err() == Some(EPERM), "CONFIG.TXT took a nvme_data line");
    Ok(())
}
ktest!("nv.window.no_key_over_files", Run("nv/40-window"), no_key_over_files);

/// MCP's `run` is uid 1000 and the node 0600; even a root process carrying MCP's program id does not get in.
fn mcp_root_refused() -> TestResult {
    let before = crate::setup::OPEN_REFUSED.load(Ordering::Relaxed);
    let argv = ["bitsdisk", "list"];
    let mut o = proc::SpawnOptions::new(&argv, &[], proc::MCP_PROGRAM);
    o.uid = 0;
    proc::spawn_with("/bin/bitsdisk", &o).map_err(|e| format!("/bin/bitsdisk: {e}"))?;
    for _ in 0..2000 {
        if crate::setup::OPEN_REFUSED.load(Ordering::Relaxed) > before {
            return Ok(());
        }
        sched::sleep_ns(10_000_000);
    }
    Err(String::from("bitsdisk under MCP's program id was never refused"))
}
ktest!("nv.window.mcp_root_refused", Run("nv/40-window"), mcp_root_refused);
