//! CONFIG.TXT (spec/core.md §2.6): `name=value` lines, last line of a name wins, spaces around name and value
//! ignored, `#` starts a comment, no size limit. Secrets (`token`, `wlan_psk`, and the redaction-only
//! `claude_api_key`) are never logged or shown.
//! WL-4 CR 4 (R8 i): the take-once secrets (`wlan_psk`) never stay in the plain map: `init` moves them into
//! `TAKEN`, and `take_secret` hands each out once and zeroes it.
//! RB-18: the BootInfo CONFIG record holds none of them after `kexec::boot::blank_config` (the next line in `main`); a
//! Reload reads them from the stick again. `claude_api_key` has no reader, so nothing keeps it.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use zeroize::Zeroize;

static CONFIG: ksync::Once<BTreeMap<String, String>> = ksync::Once::new();

/// GP-GUARD: the disk guard's mark (`gp_platform::disk_rule::marker`). bitsboot, install_kernel and Reload look for
/// these bytes in a KERNEL before they start it on a machine whose internal disks are off limits; a kernel built
/// before the guard has none. `init` reads it, so no build setting can drop it from the image.
#[used]
static GUARD_MARK: [u8; gp_platform::disk_rule::marker::KERNEL_MARK.len()] = *gp_platform::disk_rule::marker::KERNEL_MARK;

/// UPDATE: this build's version number where install_kernel and the update service read it from a KERNEL file
/// (`bitsver::find` over the loaded segments); read at run time like the guard mark, so no build drops it.
#[used]
static VERSION_MARK: [u8; bitsver::MARK_LEN] = bitsver::MARK;

/// GP-GUARD: lines `guard_board` put over the map once the DMI names were read (an empty value takes the key away).
static BOARD: ksync::Once<Vec<(&'static str, &'static str)>> = ksync::Once::new();

/// `init` imposed the profile from the record alone (then `guard_board` has nothing to add).
static IMPOSED_AT_INIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The take-once secrets, until their one reader took them (a leaf lock, held only to move a value out).
static TAKEN: ksync::SpinLock<BTreeMap<String, Vec<u8>>> = ksync::SpinLock::named("config.secret", BTreeMap::new());

/// Secrets a driver reads once (the WLAN passphrase: rtw89 derives its PMK and keeps only that).
fn take_once(key: &str) -> bool {
    key == "wlan_psk"
}

pub fn parse(text: &[u8]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    // RB-18: no copy of the text and no earlier line's value stays behind in freed heap.
    let lossy = String::from_utf8_lossy(text).into_owned();
    for line in lossy.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty()
                && let Some(old) = m.insert(k.to_string(), v.trim().to_string())
            {
                old.into_bytes().zeroize();
            }
        }
    }
    lossy.into_bytes().zeroize();
    m
}

/// The Gaming-PC profile as the kernel holds it up itself (`gp_platform::tiers`), so that it does not rest on the
/// loader alone (install_kernel renews only the kernel, never bitsboot):
/// - BT-5: a bitsboot older than the profile never writes `profile=`, so an Intel CPU on bare metal (the Gaming-PC)
///   would boot in full mode. The kernel then applies the profile without any tier. A current bitsboot hands the
///   normal profile over without the line too, but that is only the Mini-PC (AMD) or QEMU (hypervisor bit), which
///   this rule never matches.
/// - GP-GUARD: under `profile=gaming-pc` the never-relaxed lines (no driver for internal disks, no write switch,
///   no WLAN) are imposed again, whatever the record says: a loader older than `nvme_bind` forces less.
/// `kexec::boot::config` gets the same lines, because the block layer reads CONFIG there and not here.
/// This runs before the DMI names are read; what only they can tell comes in [`guard_board`].
fn impose_profile(m: &mut BTreeMap<String, String>) {
    use gp_platform::tiers;
    let from_loader = m.contains_key("profile");
    let lines: Vec<(&'static str, &'static str)> = if from_loader {
        if !tiers::is_gaming_pc(m.get("profile").map(String::as_str)) {
            outside_profile(m);
            return;
        }
        // GP-DISK: with `gp_nvme=` naming a controller and the loader's word behind it, the three NVMe lines are the
        // stick's, for that one alone (PCI keeps every other storage function untouched under `nvme_bind=off`, which
        // stays imposed). A loader older than GP-DISK passes the line through unread: then it is taken away.
        let get = |k: &str| m.get(k).map(String::as_str);
        tiers::never_relaxed(tiers::Tiers::from_loader(get))
    } else {
        let f = hal::cpu::detect();
        if !f.intel || f.hypervisor {
            outside_profile(m);
            return;
        }
        let mut l = tiers::kernel_fallback();
        l.extend([("profile", tiers::PROFILE), ("profile_reason", "kernel")]);
        l
    };
    let changed = tiers::impose(m, &lines).join(" ");
    if !from_loader {
        klog::event!(Warn, "kernel", "profile.missing", cpu = "intel", hypervisor = false, applied = changed.as_str();
            "the loader named no profile on an Intel CPU on bare metal: the Gaming-PC profile without any tier is applied");
    } else if !changed.is_empty() {
        klog::event!(Warn, "kernel", "profile.guard", changed = changed.as_str();
            "the loader forced less than the Gaming-PC profile demands (an older bitsboot): imposed by the kernel");
    }
    IMPOSED_AT_INIT.store(true, core::sync::atomic::Ordering::Release);
    kexec::boot::set_imposed(lines);
}

/// GP-DISK: `gp_nvme` is the Gaming-PC profile's tier; elsewhere the kernel takes it away. A record without the
/// line (the Mini-PC's) is left exactly as it is, and nothing is logged.
fn outside_profile(m: &mut BTreeMap<String, String>) {
    let changed = gp_platform::tiers::impose(m, gp_platform::tiers::OUTSIDE_PROFILE).join(" ");
    if !changed.is_empty() {
        klog::event!(Warn, "kernel", "gp_nvme.ignored", changed = changed.as_str(); "gp_nvme counts only in the Gaming-PC profile: taken away");
    }
}

/// GP-GUARD: the kernel recognises the Gaming-PC's board itself and does not take a `profile=` line in the record
/// for the loader's word. In the normal profile bitsboot hands the stick's file over byte for byte, so
/// `profile=normal` there may be the stick's own line under a bitsboot older than the disk guard (install_kernel
/// never renews bitsboot): such a loader honours it on this board, or passes any `profile=` value through.
/// Called once the SMBIOS names are read (`acpi::init`) and before anything reads a disk, network or stick key
/// (the initcalls, `pci::set_config`): on the board, or on an Intel CPU on bare metal with a record no current
/// loader hands over, the whole profile without any tier is put over the map, as for a loader that names none.
pub fn guard_board() {
    use gp_platform::tiers::board;
    if IMPOSED_AT_INIT.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(m) = CONFIG.get() else { return };
    let field = |name: &str| acpi::dmi::fields().iter().find(|f| f.name == name).map(|f| f.text.as_str());
    let names = board::Names { sys_vendor: field("sys_vendor"), product: field("product_name"), board_vendor: field("board_vendor"), board_name: field("board_name") };
    let record = m.get("profile").map(String::as_str);
    let f = hal::cpu::detect();
    let Some(g) = board::kernel_guard(record, f.intel, f.hypervisor, Some(&names)) else { return };
    let board::KernelGuard::Fallback(why) = g else { return };
    let lines = board::kernel_lines(g, record);
    // What the map said otherwise (for the log; the map itself is behind `BOARD` from here on).
    let changed: Vec<&str> = lines.iter().filter(|(k, v)| m.get(*k).map_or("", String::as_str) != *v).map(|(k, _)| *k).collect();
    let (said, changed) = (record.unwrap_or("-").chars().take(24).collect::<String>(), changed.join(" "));
    klog::event!(Warn, "kernel", "profile.guard", reason = why, record = said.as_str(), intel = f.intel, hypervisor = f.hypervisor, changed = changed.as_str();
        "this is the Gaming-PC (or may be) and the loader did not choose its profile: imposed by the kernel without any tier, whatever profile= says");
    kexec::boot::set_imposed(lines.clone());
    BOARD.get_or_init(|| lines);
}

pub fn init(text: Option<&[u8]>) {
    // RB-18: a Reload whose stick does not answer hands the token on from here.
    kexec::boot::set_live_secrets(get);
    // Read at run time: the mark is part of every image that can start.
    core::hint::black_box(&GUARD_MARK);
    core::hint::black_box(&VERSION_MARK);
    CONFIG.get_or_init(|| {
        let mut m = parse(text.unwrap_or(b""));
        impose_profile(&mut m);
        if let Some(v) = m.remove("claude_api_key") {
            v.into_bytes().zeroize();
        }
        let once: Vec<String> = m.keys().filter(|k| take_once(k)).cloned().collect();
        let mut t = TAKEN.lock();
        for k in once {
            if let Some(v) = m.remove(&k) {
                t.insert(k, v.into_bytes());
            }
        }
        m
    });
}

/// WL-4 CR 4: hands the take-once secret `key` to `f` once, then zeroes and forgets it. False when it is not
/// configured or was taken already (a second bring-up in the same boot; the next Reload reads CONFIG again).
pub fn take_secret(key: &str, f: &mut dyn FnMut(&[u8])) -> bool {
    let Some(mut v) = TAKEN.lock().remove(key) else { return false };
    f(&v);
    v.zeroize();
    true
}

pub fn get(key: &str) -> Option<String> {
    if let Some(v) = BOARD.get().and_then(|l| kexec::boot::imposed_value(l, key)) {
        return (!v.is_empty()).then(|| v.to_string());
    }
    CONFIG.get().and_then(|m| m.get(key).cloned())
}

pub fn is_secret(key: &str) -> bool {
    // `claude_api_key`: reserved, no reader (provider keys live in CL-5's store); a stick that still has the line
    // must not show it. Any spelling counts (`WLAN_PSK=` is no passphrase for rtw89, but the summary must not show it).
    kexec::secret::is_secret(key)
}

pub fn summary() -> String {
    let mut s = String::new();
    let board = BOARD.get().map_or(&[][..], |l| l.as_slice());
    if let Some(m) = CONFIG.get() {
        // The map's keys the board guard did not replace, then the guard's own lines, in the map's order.
        let mut all: Vec<(&str, &str)> = m.iter().filter(|(k, _)| kexec::boot::imposed_value(board, k).is_none()).map(|(k, v)| (k.as_str(), v.as_str())).collect();
        all.extend(board.iter().filter(|(_, v)| !v.is_empty()).copied());
        all.sort_unstable_by_key(|(k, _)| *k);
        for (k, v) in all {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(k);
            s.push('=');
            s.push_str(if is_secret(k) { "***" } else { v });
        }
    }
    for k in TAKEN.lock().keys() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(k);
        s.push_str("=***");
    }
    s
}

/// WL-4 CR 4: the plain map never holds a take-once secret, whether or not CONFIG names one (rerunnable: it
/// takes nothing). RB-18: the BootInfo CONFIG record holds no secret value, read from RAM as it is now, and
/// `kexec::boot::config` refuses each secret name.
fn secret_never_plain() -> ktest::TestResult {
    ktest::kassert!(get("wlan_psk").is_none(), "wlan_psk is in the plain CONFIG map");
    ktest::kassert!(get("claude_api_key").is_none(), "claude_api_key is in the plain CONFIG map");
    ktest::kassert!(!summary().split(' ').any(|kv| kv.split_once('=').is_some_and(|(k, v)| is_secret(k) && v != "***")), "the summary shows a secret");
    ktest::kassert!(kexec::boot::record_holds_secret() == Some(false), "the BootInfo CONFIG record holds a secret value");
    for k in ["token", "wlan_psk", "claude_api_key", "WLAN_PSK"] {
        ktest::kassert!(kexec::boot::config(k).is_none(), "kexec::boot::config hands out a secret");
    }
    // A token in the map came from the record, so blanking had something to do.
    ktest::kassert!(get("token").is_none_or(|t| t.is_empty() || kexec::boot::secrets_blanked() > 0), "the token was read but no value was blanked");
    // Why `init` comes first: the same text blanked gives the parser nothing.
    let fake: &[u8] = b"token=fake-rb18-t\nwlan_psk=fake-rb18-p\na=1\n";
    ktest::kassert!(parse(fake).get("token").is_some_and(|t| t == "fake-rb18-t"), "the parser reads the value");
    let blank = parse(&kexec::secret::blanked(fake));
    ktest::kassert!(blank.get("token").is_some_and(String::is_empty) && blank.get("wlan_psk").is_some_and(String::is_empty), "a blanked value parses as empty");
    ktest::kassert!(blank.get("a").is_some_and(|v| v == "1"), "other lines stay");
    // rb/18-config-secret.sh boots with made-up secrets: they arrived before the record lost them. A boot with none
    // has nothing to show here; the line makes that run say what it expects, so it cannot pass with nothing to do.
    if let Some(want) = get("rb18_expect_blanked") {
        let n = kexec::boot::secrets_blanked();
        ktest::kassert!(want.parse::<u32>().ok() == Some(n), "{} values blanked, the run expects {}", n, want);
        ktest::kassert!(get("token").is_some_and(|t| !t.is_empty()), "the token did not reach the map: blanked before config::init read it?");
        ktest::kassert!(summary().split(' ').any(|kv| kv == "wlan_psk=***"), "the passphrase did not reach the take-once store");
    }
    Ok(())
}
ktest::ktest!("kernel.config.secret", Safe, secret_never_plain);
