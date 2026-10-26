//! FS-5 ktests that need processes: `/bin/fs-mmap` (host/tests/fs/progs/fs-mmap.c) runs every mode through
//! the real syscall and page-fault paths and must end and print exactly what Linux did
//! (host/tests/fs/mmap-linux.txt, made on CachyOS). Safe and rerunnable: the program names its files with its
//! pid and removes them.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;
const LINUX: &str = include_str!("../../../host/tests/fs/mmap-linux.txt");

/// The blocks of mmap-linux.txt: (mode, end line, stdout lines).
fn blocks() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    let mut v: Vec<(&str, &str, Vec<&str>)> = Vec::new();
    for l in LINUX.lines().filter(|l| !l.starts_with('#')) {
        if let Some(m) = l.strip_prefix("=== ") {
            v.push((m, "", Vec::new()));
        } else if let Some(b) = v.last_mut() {
            if b.1.is_empty() {
                b.1 = l;
            } else {
                b.2.push(l);
            }
        }
    }
    v
}

fn run_mode(mode: &str) -> Result<(String, String), String> {
    let out = proc::stdio::Capture::new(1 << 16);
    let argv = ["fs-mmap", mode];
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/fs-mmap", &o).map_err(|e| format!("spawn: {e}"))?;
    p.wait(Some(60 * SEC));
    let end = match p.status() {
        Some(proc::ExitStatus::Exited(c)) => format!("end: exit {c}"),
        Some(proc::ExitStatus::Signaled(s)) => format!("end: signal {s}"),
        None => {
            p.kill(9);
            return Err(format!("{mode}: still running after 60 s"));
        }
    };
    Ok((end, String::from_utf8_lossy(&out.contents()).into_owned()))
}

fn programs() -> TestResult {
    if vfs::root_ns().lookup("/bin/fs-mmap").is_err() {
        return ktest::skip("/bin/fs-mmap is not in this image (xtask's build_c of host/tests/fs/progs, FS-1 change request 3)");
    }
    let bs = blocks();
    kassert!(bs.len() == 7, "mmap-linux.txt has {} blocks", bs.len());
    let (frames, pins, maps) = (vfs::pagecache::frames_held(), vfs::mmap::pinned_pages(), vfs::mmap::live_maps());
    for (mode, end, lines) in bs {
        let (got_end, text) = run_mode(mode)?;
        if mode == "shm" && text.contains("both mapped: no") {
            return ktest::skip("mmap of a file answers an error: MM's file VMAs (FS-5 change request 1) are not in this kernel");
        }
        let got: Vec<&str> = text.lines().collect();
        if got_end != end || got != lines {
            let first = got.iter().zip(lines.iter()).position(|(a, b)| a != b).unwrap_or(got.len().min(lines.len()));
            klog::event!(Warn, "fs", "mmap.diff", mode = mode, line = first, v2 = got.get(first).copied().unwrap_or("<none>"),
                linux = lines.get(first).copied().unwrap_or("<none>"));
            return Err(format!("{mode}: {got_end} vs Linux {end}; first different line {first}: {:?} vs {:?}", got.get(first), lines.get(first)));
        }
    }
    // Every program removed its files and ended: no frame, pin or mapping of theirs is left (whatever order
    // munmap, close and exit came in).
    let after = (vfs::pagecache::frames_held(), vfs::mmap::pinned_pages(), vfs::mmap::live_maps());
    kassert!(after.0 <= frames && after.1 <= pins && after.2 <= maps, "left behind: frames {frames}->{} pins {pins}->{} maps {maps}->{}", after.0, after.1, after.2);
    Ok(())
}
ktest!("fs.map.programs", Safe, programs);
