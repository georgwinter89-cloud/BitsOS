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

/// BT-5 safe mode for a stick whose bitsboot is older than BT-5 (install_kernel renews only the kernel): such a
/// bitsboot never writes `profile=`, so an Intel CPU on bare metal (the Gaming-PC) would boot in full mode. The
/// kernel then applies the safety lines itself. A BT-5 bitsboot hands the normal profile over unchanged too, but
/// that is only the Mini-PC (AMD) or QEMU (hypervisor bit), which this rule never matches.
const SAFE_FORCED: &[(&str, &str)] =
    &[("net_drivers", "off"), ("nvme_write", "off"), ("stick_write", "off"), ("usb_msd_write", "off"), ("wlan", "off")];

pub fn init(text: Option<&[u8]>) {
    // RB-18: a Reload whose stick does not answer hands the token on from here.
    kexec::boot::set_live_secrets(get);
    CONFIG.get_or_init(|| {
        let mut m = parse(text.unwrap_or(b""));
        if !m.contains_key("profile") {
            let f = hal::cpu::detect();
            if f.intel && !f.hypervisor {
                for (k, v) in SAFE_FORCED {
                    m.insert(k.to_string(), v.to_string());
                }
                m.insert("profile".to_string(), "gaming-pc".to_string());
                m.insert("profile_reason".to_string(), "kernel".to_string());
                klog::event!(Warn, "kernel", "profile.missing", cpu = "intel", hypervisor = false,
                    applied = "net_drivers=off nvme_write=off stick_write=off usb_msd_write=off wlan=off");
            }
        }
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
    CONFIG.get().and_then(|m| m.get(key).cloned())
}

pub fn is_secret(key: &str) -> bool {
    // `claude_api_key`: reserved, no reader (provider keys live in CL-5's store); a stick that still has the line
    // must not show it. Any spelling counts (`WLAN_PSK=` is no passphrase for rtw89, but the summary must not show it).
    kexec::secret::is_secret(key)
}

pub fn summary() -> String {
    let mut s = String::new();
    if let Some(m) = CONFIG.get() {
        for (k, v) in m {
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
