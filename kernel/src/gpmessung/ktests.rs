//! The command's kernel side: an injected line never starts it; a machine without the card (QEMU, the Mini-PC)
//! gets the one German line and nothing else; the protocol's file on the boot volume takes a head and many
//! appends, each on the device when `put` returns (read behind the page cache, from the volume's own sectors),
//! and the next run gets the next name.

use super::disk::{DIR, FileDisk};
use alloc::format;
use alloc::string::String;
use amdgpu::rdna4::messung::Disk;
use console::monitor;
use ktest::{TestResult, kassert, ktest};

fn injected_refused() -> TestResult {
    let cmd = monitor::find("gpu-messung").ok_or("the command gpu-messung is not registered")?;
    kassert!(cmd.physical_only, "gpu-messung is not keyboard-only");
    let since = klog::next_seq();
    let (ok, out) = monitor::execute("gpu-messung", true, false);
    kassert!(!ok && out.contains("refused: this line was typed by injected input"), "an injected gpu-messung was not refused: {out}");
    let page = klog::read(since, 200, &klog::Filter { level: None, subsystem: Some("console"), name: Some("monitor.refused") });
    kassert!(page.records.iter().any(|r| r.text.contains("gpu-messung")), "no monitor.refused record for gpu-messung");
    let page = klog::read(since, 200, &klog::Filter { level: None, subsystem: Some("gx"), name: Some("rdna4.messung") });
    kassert!(page.records.is_empty(), "the refused line reached the run: {:?}", page.records.first().map(|r| r.render()));
    Ok(())
}
ktest!("gx.messung.injected_refused", Safe, injected_refused);

/// QEMU has no RX 9070 XT, like the Mini-PC: the command says so in one line and asks nothing.
fn no_card_here() -> TestResult {
    let since = klog::next_seq();
    let (ok, out) = monitor::execute("gpu-messung", false, false);
    kassert!(ok && out == format!("{}\n", amdgpu::rdna4::messlive::NONE_HERE), "without the card: {out:?}");
    let page = klog::read(since, 200, &klog::Filter { level: None, subsystem: Some("gx"), name: Some("rdna4.") });
    kassert!(page.records.is_empty(), "a machine without the card logged the card's events: {:?}", page.records.first().map(|r| r.render()));
    Ok(())
}
ktest!("gx.messung.no_card_here", QemuOnly, no_card_here);

fn file_on_the_stick() -> TestResult {
    let vol = fs_fat::stick::wait_mounted(40_000).ok_or("no boot stick mounted")?;
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let ns = vfs::root_ns();
    let mut a = FileDisk::on(vol.clone());
    let head = String::from("## ktest\nKOPF format=1\n");
    let first = a.open(&head)?;
    let mut want = head.clone();
    for i in 0..24 {
        let part = format!("SPUR ktest {i} {}\n", "x".repeat(i * 701 % 5000));
        a.put(&part)?;
        want.push_str(&part);
        let back = ns.read_file(&format!("{DIR}/{first}"), 1 << 20).map_err(|e| format!("reading back: {e}"))?;
        kassert!(back == want.as_bytes(), "after part {i} the file reads {} bytes, {} were written", back.len(), want.len());
        // What a hard power-off right now would keep: the directory entry and the clusters on the device.
        let raw = on_device(&vol, &first)?;
        kassert!(raw == want.as_bytes(), "after part {i} the device holds {} bytes of the file, {} were put: put returned before the text was on the disk", raw.len(), want.len());
    }
    kassert!(on_device(&vol, &first)? == want.as_bytes(), "the device does not hold the whole file");
    let mut b = FileDisk::on(vol.clone());
    let second = b.open("## ktest 2\n")?;
    let n = |s: &str| gp_report::file::messung_number(s).unwrap_or(0);
    let r = if n(&second) > n(&first) || (n(&first) == 9999 && second != first) { Ok(()) } else { Err(format!("the second run's file {second} does not follow {first}")) };
    let c = vfs::Ctx::kernel(&ns);
    for name in [&first, &second] {
        let _ = c.unlink(None, format!("{DIR}/{name}").as_bytes());
    }
    let _ = vol.sync(true);
    r
}
ktest!("gx.messung.file", QemuOnly, file_on_the_stick);

/// The file as the volume's sectors hold it (`Volume::peek`: the directory entry and the clusters read from the
/// device, past the page cache).
fn on_device(vol: &fs_fat::Volume, name: &str) -> Result<alloc::vec::Vec<u8>, String> {
    let rel = DIR.strip_prefix("/boot/").ok_or("the protocol's folder is not below /boot")?;
    let mut path: alloc::vec::Vec<&[u8]> = rel.split('/').map(str::as_bytes).collect();
    path.push(name.as_bytes());
    let e = vol.peek(&path).map_err(|e| format!("peek: {e}"))?.ok_or_else(|| format!("{name} has no entry on the device"))?;
    vol.peek_read(&e, 1 << 20).map_err(|e| format!("reading the device: {e}"))
}
