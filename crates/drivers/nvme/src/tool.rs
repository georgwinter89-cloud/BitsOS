//! MCP tool `nvme` (Read): controllers (PCI address, model, serial, firmware, state, interrupts, command
//! counts with `media_writes`, which stays 0 without a window, R8a), namespaces with their disks, partition
//! tables and write verdicts, the I/O queues (BL-1); `smart=true` adds the SMART / Health log; `speed_mb=n` reads n MiB
//! of one namespace into scratch pages with many commands in flight and reports the rate (reads only).

use crate::{Nvme, controllers};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

/// Largest `speed_mb` (a single call's bound, about two seconds on a fast SSD).
const SPEED_MAX_MB: u64 = 4096;

fn ops(list: &[(u8, u64)]) -> String {
    if list.is_empty() {
        return String::from("-");
    }
    list.iter().map(|(o, n)| format!("{o:02x}h={n}")).collect::<Vec<_>>().join(" ")
}

pub fn lines(c: &Arc<Nvme>, smart: bool) -> Vec<String> {
    let mut out = Vec::new();
    let i = c.info();
    let pci = c.hw.pci();
    let mut l = format!("{} at {} {:04x}:{:04x} state={}", c.name, pci.loc(), pci.vendor, pci.device, c.state().name());
    if let Some(id) = &i.ident {
        let _ = write!(l, " model=\"{}\" serial=\"{}\" firmware=\"{}\" cntlid={} mdts={} cache={}", id.model, id.serial, id.firmware, id.cntlid, id.mdts, id.volatile_cache());
    }
    let _ = write!(
        l,
        " version={} irq={} vectors={} queues={} io_depth={} tags={} max_transfer={} starts={}",
        crate::regs::version_text(i.version),
        i.irq.map_or("-", |m| m.name()),
        i.irq.map_or(0, |m| m.vectors()),
        i.queues.len(),
        i.io_depth,
        i.io_depth.saturating_sub(1),
        i.max_transfer,
        i.starts
    );
    if let Some(e) = &i.last_error {
        let _ = write!(l, " last_error=\"{e}\"");
    }
    out.push(l);
    let s = &c.stats;
    let (adm, io) = s.used();
    let ld = |a: &core::sync::atomic::AtomicU64| a.load(core::sync::atomic::Ordering::Relaxed);
    out.push(format!(
        "  commands admin: {} | io: {} | media_writes={} errors={} timeouts={} aborts={} resets={} stale={} bytes_read={} inflight={} inflight_max={}",
        ops(&adm),
        ops(&io),
        s.media_writes(),
        ld(&s.errors),
        ld(&s.timeouts),
        ld(&s.aborts),
        ld(&s.resets),
        ld(&s.stale),
        ld(&s.bytes_read),
        ld(&s.inflight),
        ld(&s.inflight_max)
    ));
    // BL-1: each I/O queue (its vector and that vector's CPU, tags, commands placed and completed, in flight now).
    for q in c.queue_stats() {
        out.push(format!(
            "  queue {} vector={} cpu={} tags={} free={} placed={} completed={} inflight={} pool_pages={}",
            q.qid,
            q.vector.map_or_else(|| String::from("polled"), |v| format!("{v}")),
            q.cpu.map_or_else(|| String::from("-"), |c| format!("{c}")),
            q.tags,
            q.free,
            q.placed,
            q.completed,
            q.inflight,
            q.pool
        ));
    }
    if let Some(w) = c.window() {
        let (first, blocks) = w.range();
        let boot = w.boot().map_or_else(String::new, |(f, b)| format!(" boot {f}..{}", f + b));
        out.push(format!("  write window: namespace {} blocks {first}..{}{boot} | bytes_written={} refused={}", w.nsid(), first + blocks, ld(&s.bytes_written), ld(&s.window_refused)));
    }
    if let Some((mode, counts, released)) = c.hw.irq_stats() {
        out.push(format!("  vectors {:?} interrupts={:?} released={released}", mode, counts));
    }
    for ns in &i.namespaces {
        let mut l = format!("  ns{} blocks={} block_size={} formats={} flbas={:#x}", ns.nsid, ns.id.nsze, ns.block_size(), ns.id.formats.len(), ns.id.flbas);
        if let Some(why) = &ns.unusable {
            let _ = write!(l, " unusable=\"{why}\"");
        }
        let disk = format!("{}n{}", c.name, ns.nsid);
        match block::find(&disk) {
            Some(d) => {
                let scan = d.scan();
                let _ = write!(l, " disk={} table={} verdict={:?}", disk, scan.scheme_str(), scan.verdict());
                match d.write_policy() {
                    Ok(()) => l.push_str(" writes=allowed"),
                    Err(r) => {
                        let _ = write!(l, " writes=refused ({})", r.as_str());
                    }
                }
                out.push(l);
                for (first, blocks, kind, number) in &scan.parts {
                    out.push(format!("    {} first={first} blocks={blocks} type=\"{kind}\"", crate::gpt::part_name(&disk, *number)));
                }
            }
            None => {
                l.push_str(" disk=-");
                out.push(l);
            }
        }
    }
    if smart {
        match c.smart() {
            Ok(m) => out.push(format!(
                "  smart warning={:#x} temperature={}K ({}C) spare={}% threshold={}% used={}% units_read={} units_written={} host_reads={} host_writes={} power_cycles={} power_on_hours={} unsafe_shutdowns={} media_errors={} error_log_entries={}",
                m.critical_warning,
                m.temperature_k,
                m.temperature_k as i32 - 273,
                m.available_spare,
                m.spare_threshold,
                m.percent_used,
                m.data_units_read,
                m.data_units_written,
                m.host_reads,
                m.host_writes,
                m.power_cycles,
                m.power_on_hours,
                m.unsafe_shutdowns,
                m.media_errors,
                m.error_log_entries
            )),
            Err(e) => out.push(format!("  smart failed: {e}")),
        }
    }
    out
}

fn num(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| ToolError::BadArgument(format!("{key} must be a whole number"))),
    }
}

fn nvme(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let smart = match args.get("smart") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err(ToolError::BadArgument(String::from("smart must be true or false"))),
    };
    let want = args.get("controller").and_then(|v| v.as_str());
    let speed = num(args, "speed_mb")?;
    let nsid = num(args, "nsid")?.unwrap_or(1);
    let lba = num(args, "lba")?.unwrap_or(0);
    let _ = crate::settle(2_000_000_000);
    let all = controllers();
    let picked: Vec<Arc<Nvme>> = all.iter().filter(|c| want.is_none_or(|w| c.name == w)).cloned().collect();
    if picked.is_empty() {
        return match want {
            Some(w) => Err(ToolError::BadArgument(format!("no controller {w} (have: {})", all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")))),
            None => Ok(vec![ToolOutput::Text(String::from("no NVMe controller"))]),
        };
    }
    let mut out = Vec::new();
    if let Some(mb) = speed {
        if mb == 0 || mb > SPEED_MAX_MB {
            return Err(ToolError::BadArgument(format!("speed_mb must be 1..={SPEED_MAX_MB}")));
        }
        let c = &picked[0];
        let nsid = u32::try_from(nsid).map_err(|_| ToolError::BadArgument(String::from("nsid too large")))?;
        match c.read_speed(nsid, lba, mb << 20) {
            Ok((bytes, ns, most)) => out.push(format!(
                "{} ns{nsid} read {bytes} bytes from LBA {lba} in {} us = {} MB/s, up to {most} commands in flight on one queue",
                c.name,
                ns / 1000,
                if ns == 0 { 0 } else { bytes * 1000 / ns }
            )),
            Err(e) => return Err(ToolError::Failed(format!("{} ns{nsid}: {}", c.name, e.text()))),
        }
    }
    let guard = matches!(args.get("guard_hash"), Some(Json::Bool(true)));
    for c in &picked {
        out.extend(lines(c, smart));
        if guard {
            out.extend(guard_hashes(c));
        }
    }
    Ok(vec![ToolOutput::Text(out.join("\n"))])
}

/// What `guard_hashes` reads: the whole disk and every partition but the two that may be written, nvme_data's window
/// and the boot ESP nvme_boot= names (pure: host-tested).
fn guarded(total: u64, parts: &[(u64, u64, String, u32)], skip: &[Option<(u64, u64)>]) -> Vec<(u64, u64)> {
    let mut ranges = alloc::vec![(0, total)];
    ranges.extend(parts.iter().map(|p| (p.0, p.1)).filter(|r| !skip.contains(&Some(*r))));
    ranges
}

/// NV-2p's check for the desk: a hash of what must never change on a disk with a window: the first and last MiB of
/// the disk (both GPTs) and of every partition but the window and the boot ESP. Only reads.
fn guard_hashes(c: &Arc<Nvme>) -> Vec<String> {
    const SPAN: u64 = 1 << 20;
    let mut out = Vec::new();
    for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
        let Some(d) = block::find(&format!("{}n{}", c.name, ns.nsid)) else { continue };
        let bs = d.block_size() as u64;
        let total = d.dev_info().blocks;
        let lower = c.window().filter(|w| w.nsid() == ns.nsid);
        let window = d.window().or_else(|| lower.map(|w| w.range()));
        let esp = d.named_esp().or_else(|| lower.and_then(|w| w.boot()));
        let ranges = guarded(total, &d.scan().parts, &[window, esp]);
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        let mut bad = false;
        for (first, blocks) in ranges {
            let n = (SPAN / bs).min(blocks);
            for at in [first, first + blocks - n] {
                match d.read_vec(at, (n * bs) as usize, devmodel::block::IoPriority::Normal) {
                    Ok(v) => v.iter().for_each(|b| h = (h ^ *b as u64).wrapping_mul(0x100_0000_01B3)),
                    Err(_) => bad = true,
                }
            }
        }
        out.push(format!("  guard_hash ns{} {h:#018x}{}", ns.nsid, if bad { " UNREADABLE PARTS" } else { "" }));
    }
    out
}

mcp_tool!(
    name: "nvme",
    owner: "NV",
    safety: Read,
    description: "NVMe controllers (NV-1, NV-2p): PCI address, model, serial, firmware, state, interrupt mode, commands issued per opcode with media_writes (0 unless a window is armed by nvme_write=on and nvme_data=) and the write window, each namespace with its block disk, partition table, verdict (Windows/EFI = never written) and write policy, and its partitions. smart=true adds the SMART/Health log (temperature, wear, power-on hours, unsafe shutdowns, media errors). speed_mb=n (1..4096) reads n MiB from namespace nsid (default 1) at lba (default 0) of the first selected controller into scratch pages with many commands in flight (up to the queue's tags, BL-1) and reports MB/s and the most in flight. Each I/O queue has a line: its vector, the vector's CPU, tags, commands placed/completed/in flight. guard_hash=true hashes (only reads) the first and last MiB of each disk and of every partition but the two that may be written (the nvme_data= window and the nvme_boot= ESP): it must not change over a BitsOS round.",
    schema: r#"{"type":"object","properties":{"controller":{"type":"string","description":"nvme0, nvme1, ... (default: all)"},"smart":{"type":"boolean","description":"read the SMART/Health log"},"guard_hash":{"type":"boolean","description":"hash what must never change (NV-2p)"},"speed_mb":{"type":"integer","description":"read this many MiB and report the rate"},"nsid":{"type":"integer","description":"namespace for speed_mb (default 1)"},"lba":{"type":"integer","description":"start block for speed_mb (default 0)"}}}"#,
    handler: nvme,
);

/// NV-2p: what the root process of `data_move` runs: its lists go to `MOVE_DIR` for the desk's script to compare.
/// `mkdir` first and the log after it: a start that lost the race leaves the running copy's log alone. A copy that
/// failed or was stopped takes its folder away, so it can be started again without a restart.
pub(crate) const MOVE_SCRIPT: &str = r#"D=/run/bitsos/data-move
mkdir "$D" 2>/dev/null || exit 4
exec > /run/bitsos/data-move.log 2>&1
trap 'rm -rf "$D"; echo "done status=stopped"; exit 5' HUP INT TERM
list() { (cd "$1" && find . -exec stat -c '%f|%u|%g|%s|%Y|%h|%i|%N' {} + > "$2.stat" && find . -type f -exec sha256sum {} + > "$2.sums"); }
echo "start=$(date +%s)"
list /stickdata "$D/stick-before" && cp -a /stickdata/. /data/ && sync && list /stickdata "$D/stick-after" && list /data "$D/data"
st=$?
[ "$st" = 0 ] || rm -rf "$D"
echo "done status=$st"
exit "$st"
"#;
const MOVE_DIR: &str = "/run/bitsos/data-move";
const MOVE_LOG: &str = "/run/bitsos/data-move.log";
/// Longer than this no log of a finished copy gets (its lists go to `MOVE_DIR`): cp's complaints would have failed it.
const MOVE_LOG_MAX: usize = 1 << 20;
/// The marker `data_move {"compared":true}` writes after the desk's script compared the lists.
const MOVE_MARKER: &str = "/data/.stickdata-moved";

/// The log's last line says the copy ended well.
pub(crate) fn copy_finished(log: &[u8]) -> bool {
    log.split(|b| *b == b'\n').rfind(|l| !l.is_empty()) == Some(b"done status=0")
}

/// `data_move`: `run` is uid 1000 and cannot keep owners nor read root's files, so this starts the one fixed root
/// command; nothing of the call goes into it. Only with a valid window, /stickdata mounted, and only once (the marker).
fn data_move(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let refuse = |w: &str| Err(ToolError::Failed(String::from(w)));
    if !matches!(block::gpt::window::boot_state(), block::gpt::window::Boot::Armed { .. }) {
        return refuse("no valid write window at this boot: /data is not on the SSD");
    }
    if !fs_ext4::data::stick_data_mounted() {
        return refuse("/stickdata is not mounted");
    }
    let ns = vfs::root_ns();
    if ns.lookup(MOVE_MARKER).is_ok() {
        return refuse("already moved: /data/.stickdata-moved stands");
    }
    // The script compared the lists: the marker, written here because the file tools never write on ext4.
    if matches!(args.get("compared"), Some(Json::Bool(true))) {
        let log = ns.read_file(MOVE_LOG, MOVE_LOG_MAX).unwrap_or_default();
        if !copy_finished(&log) {
            return refuse("no finished copy at this boot");
        }
        ns.create_file_bytes(MOVE_MARKER.as_bytes(), 0o644, b"moved from /stickdata\n").map_err(|e| ToolError::Failed(format!("{MOVE_MARKER}: errno {e}")))?;
        ns.lookup_path(MOVE_MARKER.as_bytes(), true).map_err(|e| ToolError::Failed(format!("{MOVE_MARKER}: errno {e}")))?.dentry.inode.sync(vfs::pagecache::SyncKind::Fsync).map_err(|e| ToolError::Failed(format!("{MOVE_MARKER}: errno {e}")))?;
        return Ok(vec![ToolOutput::Text(format!("{MOVE_MARKER} written"))]);
    }
    if ns.lookup(MOVE_DIR).is_ok() {
        return refuse("a copy runs or has finished at this boot: the last line of /run/bitsos/data-move.log says which");
    }
    let argv = ["sh", "-c", MOVE_SCRIPT];
    let envp = ["PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &envp, proc::MCP_PROGRAM);
    o.uid = 0;
    let p = proc::spawn_with("/bin/sh", &o).map_err(|e| ToolError::Failed(format!("/bin/sh: {e}")))?;
    klog::event!(Info, "nvme", "data_move.started", pid = p.pid; "cp -a /stickdata/. /data/ as root, once");
    Ok(vec![ToolOutput::Text(format!("started as pid {}; the last line of /run/bitsos/data-move.log says when it is done", p.pid))])
}

mcp_tool!(
    name: "data_move",
    owner: "NV",
    safety: Change,
    description: "NV-2p: starts the one copy of the stick's data.img contents (read-only at /stickdata) to /data on the SSD as root, keeping hard links, symlinks, modes, owners and times, and leaves lists of both trees in /run/bitsos/data-move/. host/tests/minipc/data-move.sh runs it and compares; compared=true then writes the marker. Refused without a valid window and once the marker stands.",
    schema: r#"{"type":"object","properties":{"compared":{"type":"boolean","description":"the lists were compared and are the same: write the marker"}}}"#,
    handler: data_move,
);

#[cfg(test)]
mod tests {
    use super::guarded;
    use alloc::string::String;

    #[test]
    fn the_guard_hash_skips_both_windows_only() {
        let p = |first, blocks, n| (first, blocks, String::from("GPT"), n);
        let parts = [p(34, 2014, 1), p(2048, 2048, 2), p(4096, 1024, 3), p(5120, 2881, 4)];
        let (data, esp) = (Some((5120, 2881)), Some((4096, 1024)));
        assert_eq!(guarded(8192, &parts, &[data, esp]), [(0, 8192), (34, 2014), (2048, 2048)]);
        assert_eq!(guarded(8192, &parts, &[data, None]), [(0, 8192), (34, 2014), (2048, 2048), (4096, 1024)], "no ESP named: it is hashed");
        assert_eq!(guarded(8192, &parts, &[None, None]).len(), 5, "nothing armed: every partition");
        assert_eq!(guarded(8192, &parts, &[data, Some((4096, 1023))]).len(), 4, "only exactly the ESP's range");
    }
}
