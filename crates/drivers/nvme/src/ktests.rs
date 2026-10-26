//! In-kernel tests `nv.*`. Safe ones run on any machine and only read (the Mini-PC: identify, partition
//! tables, SMART, flush, no write command); they skip without a controller and can run several times per boot.
//! QemuOnly ones need host/tests/nv's images: controller `NVQ0` (ns1 512 B pattern seed 1 behind blkdebug, a
//! read error at sector `BAD_SECTOR`; ns2 4 KiB pattern seed 2; ns3 the fake Windows GPT) and `NVQ1` (ns1 4 KiB
//! GPT with two Linux partitions).

use crate::cmd::{self, MEDIA_WRITE_IO_OPS};
use crate::{CmdError, Nvme, State, controllers, settle, test_pattern};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::block::{BlockDevice, IoPriority};
use ktest::{TestResult, kassert, ktest, skip};

const SEC: u64 = 1_000_000_000;
/// blkdebug's read error on NVQ0 ns1 (host/tests/nv/10-nvme.sh `eio.conf`).
const BAD_SECTOR: u64 = 20000;

fn ready() -> Result<Vec<Arc<Nvme>>, TestResult> {
    if !settle(30 * SEC) {
        return Err(Err(String::from("a controller's bring-up did not finish in 30 s")));
    }
    let all = controllers();
    if all.is_empty() {
        return Err(skip("no NVMe controller"));
    }
    Ok(all)
}

fn by_serial(serial: &str) -> Result<Arc<Nvme>, TestResult> {
    let all = ready()?;
    all.into_iter()
        .find(|c| c.info().ident.is_some_and(|i| i.serial == serial))
        .ok_or_else(|| skip(&format!("no controller with serial {serial} (host/tests/nv images)")))
}

macro_rules! need {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

fn no_write_opcodes() -> TestResult {
    for op in cmd::IO_OPS {
        kassert!(!MEDIA_WRITE_IO_OPS.contains(&(op as u8)), "I/O opcode {:?} writes media", op);
    }
    for op in cmd::ADMIN_OPS {
        kassert!(!cmd::MEDIA_WRITE_ADMIN_OPS.contains(&(op as u8)), "admin opcode {:?} changes media", op);
    }
    Ok(())
}
ktest!("nv.cmd.no_write_opcodes", Safe, no_write_opcodes);

fn ident() -> TestResult {
    let all = need!(ready());
    for c in &all {
        let i = c.info();
        kassert!(c.state() == State::Ready, "{} is {} ({:?})", c.name, c.state().name(), i.last_error);
        let Some(id) = &i.ident else { return Err(format!("{} has no Identify data", c.name)) };
        kassert!(!id.model.is_empty() && !id.serial.is_empty(), "{}: model {:?} serial {:?}", c.name, id.model, id.serial);
        kassert!(!i.namespaces.is_empty(), "{}: no namespace", c.name);
        kassert!(i.max_transfer >= 4096 && i.io_depth >= 2, "{}: transfer {} depth {}", c.name, i.max_transfer, i.io_depth);
        klog::event!(Info, "nvme", "ktest.ident", ctrl = c.name.as_str(), model = id.model.as_str(), serial = id.serial.as_str(),
            firmware = id.firmware.as_str(), namespaces = i.namespaces.len(), irq = i.irq.map_or("-", |m| m.name()));
    }
    Ok(())
}
ktest!("nv.ident", Safe, ident);

/// Every usable namespace: up to 1 MiB from block 0 read twice (same bytes), and the start again through the
/// block layer's queue.
fn read() -> TestResult {
    let all = need!(ready());
    for c in &all {
        for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
            let bs = ns.block_size() as u64;
            let n = (1u64 << 20).min(ns.id.nsze * bs) as usize;
            let mut a = vec![0u8; n];
            let mut b = vec![0u8; n];
            c.read(ns.nsid, 0, &mut a, true).map_err(|e| format!("{} ns{}: {}", c.name, ns.nsid, e.text()))?;
            c.read(ns.nsid, 0, &mut b, true).map_err(|e| format!("{} ns{} again: {}", c.name, ns.nsid, e.text()))?;
            kassert!(a == b, "{} ns{}: two reads differ", c.name, ns.nsid);
            let disk = format!("{}n{}", c.name, ns.nsid);
            let Some(d) = block::find(&disk) else { return Err(format!("{disk} not registered with the block layer")) };
            let len = (bs * 8.min(ns.id.nsze)) as usize;
            let v = d.read_vec(0, len, IoPriority::Normal).map_err(|e| format!("{disk} via block: {}", e.as_str()))?;
            kassert!(v[..] == a[..v.len()], "{disk}: the block layer's bytes differ");
        }
    }
    Ok(())
}
ktest!("nv.read", Safe, read);

/// R8a: no media-writing command was ever placed in a queue, and a write through the block layer or straight
/// at the namespace device is refused before it reaches the controller.
fn zero_writes() -> TestResult {
    let all = need!(ready());
    for c in &all {
        kassert!(c.stats.media_writes() == 0, "{}: {} media-writing commands issued", c.name, c.stats.media_writes());
        // The device's own word too (not only the driver's counters): SMART Host Write Commands and Data Units
        // Written must not move over reads, a flush and the refused writes below.
        let smart_before = c.smart().map_err(|e| format!("{}: SMART: {e}", c.name))?;
        let (_, io) = c.stats.used();
        kassert!(io.iter().all(|(op, _)| cmd::IO_OPS.iter().any(|o| *o as u8 == *op)), "{}: I/O opcodes {:?}", c.name, io);
        let before: u64 = io.iter().map(|x| x.1).sum();
        for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
            let disk = format!("{}n{}", c.name, ns.nsid);
            let Some(d) = block::find(&disk) else { continue };
            let buf = vec![0u8; ns.block_size() as usize];
            kassert!(d.write(0, &buf, true, IoPriority::Normal).is_err(), "{disk}: a block-layer write was accepted");
            kassert!(d.write_policy().is_err(), "{disk}: write policy allows writes");
            let dev = crate::ns::NsDevice::new(c.clone(), ns.nsid).ok_or_else(|| format!("{disk}: no device"))?;
            kassert!(dev.write_blocks(0, &buf, false, IoPriority::Normal) == Err(DevError::Refused), "{disk}: device write not refused");
            kassert!(dev.discard(0, 1) == Err(DevError::Refused), "{disk}: discard not refused");
            kassert!(dev.info().read_only, "{disk}: not read-only");
        }
        let after: u64 = c.stats.used().1.iter().map(|x| x.1).sum();
        kassert!(after == before, "{}: refused writes still sent {} commands", c.name, after - before);
        for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
            let mut buf = vec![0u8; (ns.block_size() as usize * 16).min(c.info().max_transfer.max(ns.block_size() as usize))];
            c.read(ns.nsid, 0, &mut buf, true).map_err(|e| format!("{}n{}: read: {}", c.name, ns.nsid, e.text()))?;
            c.flush(ns.nsid).map_err(|e| format!("{}n{}: flush: {}", c.name, ns.nsid, e.text()))?;
        }
        let smart_after = c.smart().map_err(|e| format!("{}: SMART: {e}", c.name))?;
        kassert!(smart_after.host_writes == smart_before.host_writes, "{}: SMART host write commands {} -> {}", c.name,
            smart_before.host_writes, smart_after.host_writes);
        kassert!(smart_after.data_units_written == smart_before.data_units_written, "{}: SMART data units written {} -> {}", c.name,
            smart_before.data_units_written, smart_after.data_units_written);
        kassert!(c.stats.media_writes() == 0, "{}: media writes", c.name);
        klog::event!(Info, "nvme", "ktest.zero_writes", ctrl = c.name.as_str(), media_writes = c.stats.media_writes(), io_commands = after,
            smart_host_writes = smart_after.host_writes, smart_units_written = smart_after.data_units_written);
    }
    Ok(())
}
ktest!("nv.zero_writes", Safe, zero_writes);

fn smart() -> TestResult {
    let all = need!(ready());
    for c in &all {
        let s = c.smart().map_err(|e| format!("{}: {e}", c.name))?;
        kassert!(s.temperature_k > 0, "{}: {:?}", c.name, s);
        klog::event!(Info, "nvme", "ktest.smart", ctrl = c.name.as_str(), temperature_k = s.temperature_k, used = s.percent_used,
            power_on_hours = s.power_on_hours, unsafe_shutdowns = s.unsafe_shutdowns, media_errors = s.media_errors, units_written = s.data_units_written);
    }
    Ok(())
}
ktest!("nv.smart", Safe, smart);

fn flush() -> TestResult {
    let all = need!(ready());
    for c in &all {
        for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
            c.flush(ns.nsid).map_err(|e| format!("{} ns{}: {}", c.name, ns.nsid, e.text()))?;
        }
    }
    Ok(())
}
ktest!("nv.flush", Safe, flush);

/// With vectors, reads complete by interrupt on the I/O vector.
fn irq() -> TestResult {
    let all = need!(ready());
    let mut seen = 0;
    for c in &all {
        let i = c.info();
        let Some(v) = i.io_vector else { continue };
        let Some(ns) = i.namespaces.iter().find(|n| n.unusable.is_none()) else { continue };
        let before = crate::hw::Hw::irq_count(&c.hw, v);
        let mut b = vec![0u8; i.max_transfer.min(ns.block_size() as usize * 64)];
        for _ in 0..8 {
            c.read(ns.nsid, 0, &mut b, true).map_err(|e| e.text())?;
        }
        let after = crate::hw::Hw::irq_count(&c.hw, v);
        kassert!(after > before, "{}: no interrupt on vector {} in 8 reads ({:?})", c.name, v, i.irq);
        seen += 1;
    }
    if seen == 0 {
        return skip("every controller is polled");
    }
    Ok(())
}
ktest!("nv.irq", Safe, irq);

fn pattern_ok(c: &Nvme, nsid: u32, seed: u8, lba: u64, len: usize) -> TestResult {
    let mut b = vec![0u8; len];
    c.read(nsid, lba, &mut b, true).map_err(|e| format!("{} ns{nsid} lba {lba}: {}", c.name, e.text()))?;
    let bs = c.info().namespaces.iter().find(|n| n.nsid == nsid).map_or(512, |n| n.block_size()) as u64;
    let base = lba * bs;
    if let Some(k) = (0..len).find(|&k| b[k] != test_pattern(seed, base + k as u64)) {
        return Err(format!("{} ns{nsid} lba {lba} len {len}: byte {k} is {:#x}, want {:#x}", c.name, b[k], test_pattern(seed, base + k as u64)));
    }
    Ok(())
}

fn qemu_pattern() -> TestResult {
    let c = need!(by_serial("NVQ0"));
    let x = c.info().max_transfer;
    for (nsid, seed) in [(1u32, 1u8), (2, 2)] {
        for (lba, len) in [(0u64, 4096usize), (7, 8192), (33, 12 * 4096), (100, x), (200, 3 * x + 4096)] {
            pattern_ok(&c, nsid, seed, lba, len)?;
        }
    }
    Ok(())
}
ktest!("nv.qemu.pattern", QemuOnly, qemu_pattern);

/// The fake Windows namespace (NVQ0 ns3) and the Linux GPT (NVQ1 ns1, 4 KiB blocks): verdicts, partitions as
/// Stacked disks, reads at their offsets, every write refused.
fn gpt() -> TestResult {
    let q0 = need!(by_serial("NVQ0"));
    let q1 = need!(by_serial("NVQ1"));
    let win = format!("{}n3", q0.name);
    let d = block::find(&win).ok_or_else(|| format!("{win} not registered"))?;
    let scan = d.scan();
    kassert!(matches!(scan.verdict(), block::table::Verdict::Windows(_) | block::table::Verdict::Efi(_)), "{win}: verdict {:?}", scan.verdict());
    kassert!(scan.parts.len() == 3, "{win}: {} partitions", scan.parts.len());
    let disks = crate::disks_of(&q0.name);
    for n in 1..=3 {
        let p = crate::gpt::part_name(&win, n);
        kassert!(disks.contains(&p), "{p} not registered ({disks:?})");
        let pd = block::find(&p).ok_or_else(|| format!("{p} missing in the block layer"))?;
        kassert!(pd.kind() == block::DiskKind::Stacked, "{p}: kind {:?}", pd.kind());
        kassert!(pd.write(0, &[0u8; 512], true, IoPriority::Normal).is_err(), "{p}: write accepted");
        kassert!(pd.write_policy().is_err(), "{p}: write policy open");
    }
    // Partition 3 (basic data) starts at 8192: its block 10 is the namespace's 8202 (pattern seed 3).
    let p3 = block::find(&crate::gpt::part_name(&win, 3)).ok_or("p3")?;
    let v = p3.read_vec(10, 4096, IoPriority::Normal).map_err(|e| e.as_str())?;
    let want: Vec<u8> = (0..4096u64).map(|k| test_pattern(3, 8202 * 512 + k)).collect();
    kassert!(v == want, "{win}p3 block 10 is not the namespace's block 8202");
    // NVQ1: Linux partitions on 4 KiB blocks: p1 256..=1023, p2 1024..=3800.
    let lin = format!("{}n1", q1.name);
    let ld = block::find(&lin).ok_or_else(|| format!("{lin} not registered"))?;
    kassert!(ld.block_size() == 4096, "{lin}: block size {}", ld.block_size());
    kassert!(ld.scan().verdict() == block::table::Verdict::Clean, "{lin}: {:?}", ld.scan().verdict());
    let p1 = block::find(&crate::gpt::part_name(&lin, 1)).ok_or("NVQ1 p1 missing")?;
    let p2 = block::find(&crate::gpt::part_name(&lin, 2)).ok_or("NVQ1 p2 missing")?;
    kassert!(p1.dev_info().blocks == 768 && p2.dev_info().blocks == 2777, "sizes {} {}", p1.dev_info().blocks, p2.dev_info().blocks);
    let v = p2.read_vec(5, 4096, IoPriority::Normal).map_err(|e| e.as_str())?;
    let want: Vec<u8> = (0..4096u64).map(|k| test_pattern(4, 1029 * 4096 + k)).collect();
    kassert!(v == want, "{lin}p2 block 5 is not the namespace's block 1029");
    kassert!(p2.write(0, &[0u8; 4096], false, IoPriority::Normal).is_err(), "{lin}p2: write accepted without nvme_write");
    kassert!(q0.stats.media_writes() == 0 && q1.stats.media_writes() == 0, "media writes");
    Ok(())
}
ktest!("nv.gpt", QemuOnly, gpt);

/// Error answers: past the end (the device's LBA Out of Range), an unknown namespace, blkdebug's read error;
/// the controller stays ready and reads go on.
fn errors() -> TestResult {
    let c = need!(by_serial("NVQ0"));
    let ns1 = c.info().namespaces.iter().find(|n| n.nsid == 1).map(|n| n.id.nsze).ok_or("NVQ0 ns1")?;
    let mut b = vec![0u8; 4096];
    match c.read(1, ns1 - 2, &mut b, false) {
        Err(e @ CmdError::Status { sct: 0, sc: 0x80, .. }) => kassert!(e.to_dev() == DevError::InvalidArgument),
        r => return Err(format!("past the end: {r:?}")),
    }
    kassert!(c.read(1, ns1 - 2, &mut b, true) == Err(CmdError::Invalid), "the driver's own bounds check");
    // Invalid Namespace (0Bh) on real controllers; QEMU answers Invalid Field (02h) for an unattached NSID.
    match c.read(99, 0, &mut b[..512], false) {
        Err(e @ CmdError::Status { sct: 0, sc: 0x0B | 0x02, .. }) => kassert!(e.to_dev() == DevError::InvalidArgument),
        r => return Err(format!("unknown namespace: {r:?}")),
    }
    match c.read(1, BAD_SECTOR - 3, &mut b, true) {
        Err(e @ CmdError::Status { .. }) => kassert!(e.to_dev() == DevError::Io, "{:?}", e),
        r => return Err(format!("blkdebug sector {BAD_SECTOR}: {r:?}")),
    }
    kassert!(c.state() == State::Ready, "{} is {}", c.name, c.state().name());
    pattern_ok(&c, 1, 1, BAD_SECTOR + 8, 4096)?;
    Ok(())
}
ktest!("nv.errors", QemuOnly, errors);

/// Quiesce (shutdown, EN=0, bus master off) and bring-up again, three times, with the disks staying usable.
fn reset() -> TestResult {
    let c = need!(by_serial("NVQ1"));
    for round in 0..3 {
        kassert!(devmodel::quiesce_matching(&c.name) == 1, "quiesce {}", c.name);
        kassert!(c.state() == State::Quiesced);
        kassert!(c.quiesce_note() & 0x8000_0003 == 0x8000_0003, "round {}: note {:#x}", round, c.quiesce_note());
        kassert!(c.hw.pci().command() & 4 == 0, "bus master still on after Quiesce");
        kassert!(c.read(1, 0, &mut [0u8; 4096], true) == Err(CmdError::Gone));
        c.restart().map_err(|e| format!("round {round}: {e}"))?;
        pattern_ok(&c, 1, 4, 300, 64 * 1024)?;
    }
    let d = block::find(&format!("{}n1p2", c.name)).ok_or("p2")?;
    d.read_vec(0, 4096, IoPriority::Normal).map_err(|e| e.as_str())?;
    Ok(())
}
ktest!("nv.reset", QemuOnly, reset);

/// The `nvme` MCP tool as a client calls it: the summary (media_writes=0), SMART, a short read speed, and a
/// bad argument.
fn tool() -> TestResult {
    let all = need!(ready());
    let t = mcp::find("nvme").ok_or("the nvme tool is not registered")?;
    kassert!(t.safety == mcp::Safety::Read, "nvme tool is not Read");
    let ctx = mcp::ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let text = |r: mcp::ToolResult| -> Result<String, String> {
        match r.map_err(|e| format!("{e:?}"))?.first() {
            Some(mcp::ToolOutput::Text(s)) => Ok(s.clone()),
            other => Err(format!("unexpected answer {other:?}")),
        }
    };
    let s = text((t.handler)(&ctx, &mcp::Json::parse(r#"{"smart":true}"#).map_err(|_| "json")?))?;
    for c in &all {
        kassert!(s.contains(&format!("{} at ", c.name)), "no line for {}: {s}", c.name);
    }
    kassert!(s.matches("media_writes=0 ").count() == all.len(), "media_writes not 0 everywhere: {s}");
    kassert!(s.matches("  smart warning=").count() == all.len(), "no SMART line: {s}");
    let first = &all[0];
    let nsid = first.info().namespaces.iter().find(|n| n.unusable.is_none()).map(|n| n.nsid).ok_or("no usable namespace")?;
    let args = format!(r#"{{"controller":"{}","speed_mb":8,"nsid":{nsid}}}"#, first.name);
    let s = text((t.handler)(&ctx, &mcp::Json::parse(&args).map_err(|_| "json")?))?;
    kassert!(s.contains(" MB/s"), "no rate: {s}");
    let bad = (t.handler)(&ctx, &mcp::Json::parse(r#"{"speed_mb":0}"#).map_err(|_| "json")?);
    kassert!(matches!(bad, Err(mcp::ToolError::BadArgument(_))), "speed_mb=0 accepted: {bad:?}");
    let none = (t.handler)(&ctx, &mcp::Json::parse(r#"{"controller":"nvme99"}"#).map_err(|_| "json")?);
    kassert!(matches!(none, Err(mcp::ToolError::BadArgument(_))), "unknown controller accepted");
    kassert!(all.iter().all(|c| c.stats.media_writes() == 0), "media writes after the tool");
    Ok(())
}
ktest!("nv.tool", Safe, tool);
