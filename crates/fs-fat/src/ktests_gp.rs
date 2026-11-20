//! GP-GUARD: the Gaming-PC profile's tier lines are guarded keys on the real volume. Only the `gp_stick=on` boot
//! of `host/tests/bt/65-gp-tiers.sh` runs this (QEMU, the image of that run): there the boot stick is writable,
//! its CONFIG.TXT says `gp_stick=on` and has no `gp_net` line, which is the stick the rule is for: nothing but
//! Georg's own edit may give it the network.

use crate::stick;
use alloc::format;
use alloc::string::String;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::EPERM;
use vfs::abi::{O_APPEND, O_WRONLY};
use vfs::ops::Ctx;

const CONFIG: &str = "/boot/BITSOS2/CONFIG.TXT";

fn tier_keys() -> TestResult {
    let Some(vol) = stick::wait_mounted(40_000) else { return ktest::fail("no boot stick mounted") };
    kassert!(vol.writable(), "the stick is read-only: this test belongs to the gp_stick=on boot");
    let ns = vfs::root_ns();
    let read = || ns.read_file(CONFIG, 1 << 20).map_err(|e| format!("reading CONFIG.TXT: {e:?}"));
    let before = read()?;
    let tier = |text: &[u8], key: &str| {
        String::from_utf8_lossy(text).lines().filter_map(|l| l.split('#').next().unwrap_or("").split_once('=')).filter(|(k, _)| k.trim() == key).map(|(_, v)| String::from(v.trim())).last()
    };
    kassert!(tier(&before, "gp_stick").as_deref() == Some("on"), "this boot's stick does not say gp_stick=on");
    kassert!(tier(&before, "gp_net").is_none(), "this boot's stick already has a gp_net line");
    let c = Ctx::kernel(&ns);
    let cfg = c.open(None, CONFIG.as_bytes(), O_WRONLY | O_APPEND, 0).map_err(|e| format!("opening CONFIG.TXT: {e:?}"))?;
    // Adding the network tier, changing the stick tier, in the spellings the parsers and the guard read.
    for line in ["\ngp_net=on\n", "\nGP_NET = on \r\n", "\ngp_net=off\n", "\ngp_stick=off\n", "\ngp_stick=on # again\n"] {
        kassert!(cfg.write(line.as_bytes()).err() == Some(EPERM), "CONFIG.TXT took the tier line {line:?}");
    }
    kassert!(read()? == before, "a refused line changed CONFIG.TXT");
    // The file is not closed to everything: a free line is written.
    let free = b"\n# GP-GUARD ktest: a free line\n";
    kassert!(cfg.write(free).ok() == Some(free.len()), "a free line was refused");
    let after = read()?;
    kassert!(after.len() == before.len() + free.len() && after.starts_with(&before), "the free line is not at the end");
    kassert!(tier(&after, "gp_stick").as_deref() == Some("on") && tier(&after, "gp_net").is_none(), "the tiers changed");
    klog::event!(Info, "fat", "ktest.tier_keys", refused = 5u64, config_bytes = after.len() as u64);
    Ok(())
}
ktest!("fs.fat.tier_keys", Run("bt/65-gp-tiers"), tier_keys);
