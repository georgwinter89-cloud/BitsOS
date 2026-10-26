//! BT-edid: the monitor's EDID the loader read from the firmware (BootInfo tag 21, carried on by every Reload)
//! arrives whole and is what bitsboot's log line says, and the kernel's readers accept it. Safe, rerunnable.

use alloc::string::String;
use ktest::{TestResult, kassert};

/// `Some(Some(n))` for `edid.found ... bytes=n`, `Some(None)` for `edid.none`, `None` without a line (a
/// loader from before BT-edid).
fn logged(log: &[u8]) -> Option<Option<usize>> {
    let text = String::from_utf8_lossy(log);
    let line = text.lines().find(|l| l.contains("bitsboot edid."))?;
    if line.contains("edid.none") {
        return Some(None);
    }
    let n = line.split_whitespace().find_map(|w| w.strip_prefix("bytes="))?;
    Some(n.parse().ok())
}

fn arrived() -> TestResult {
    let (bi, _) = kexec::boot::boot_info().ok_or("no BootInfo")?;
    let edid = bi.edid();
    let said = bi.loader_log().and_then(logged);
    klog::event!(Info, "ktest", "bt.edid", bytes = edid.map_or(0, |e| e.len()), logged = alloc::format!("{said:?}"));
    match (said, edid) {
        (Some(Some(n)), Some(e)) => kassert!(e.len() == n, "EDID record {} B, the loader logged {n} B", e.len()),
        (Some(Some(n)), None) => return Err(alloc::format!("the loader logged an EDID of {n} B, but no EDID record arrived")),
        (Some(None), Some(e)) => return Err(alloc::format!("the loader logged edid.none, but an EDID record of {} B arrived", e.len())),
        _ => {}
    }
    let Some(e) = edid else {
        return ktest::skip("no EDID from the firmware");
    };
    kassert!(e.len() >= 128 && e.len() % 128 == 0, "EDID of {} B is not whole blocks", e.len());
    kassert!(e[..8] == [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0], "EDID header {:02x?}", &e[..8]);
    // display::edid is strict (base checksum and version 1, like Linux drm_edid.c:1891-1895), while the loader
    // hands a bad checksum on: only a clean base block must give timings, otherwise the refusal is just logged.
    let base_ok = e[..128].iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0 && e[18] == 1;
    let timings = display::edid::timings(e);
    klog::event!(Info, "ktest", "bt.edid.timings", base_ok = base_ok, ok = timings.is_ok());
    kassert!(!base_ok || timings.is_ok(), "display::edid refuses a clean EDID: {:?}", timings.err());
    // The kernel glue set the DP audio sink from these bytes at boot; setting it again is the same sink
    // (hdacodec tolerates a bad checksum, so this holds for every EDID with a header).
    let sink = hda::hdmi::set_sink_edid(e);
    kassert!(sink.is_ok(), "hda refuses the EDID: {sink:?}");
    Ok(())
}
ktest::ktest!("bt.edid.arrived", Safe, arrived);
