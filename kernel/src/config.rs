//! CONFIG.TXT (spec/core.md §2.6): `name=value` lines, last line of a name wins, spaces around name and value
//! ignored, `#` starts a comment, no size limit. Secrets (`token`, `wlan_psk`) are never logged.
//! WL-4 CR 4 (R8 i): the take-once secrets (`wlan_psk`) never stay in the plain map: `init` moves them into
//! `TAKEN`, and `take_secret` hands each out once and zeroes it.

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
    for line in String::from_utf8_lossy(text).lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                m.insert(k.to_string(), v.trim().to_string());
            }
        }
    }
    m
}

/// BT-5 safe mode for a stick whose bitsboot is older than BT-5 (install_kernel renews only the kernel): such a
/// bitsboot never writes `profile=`, so an Intel CPU on bare metal (the Gaming-PC) would boot in full mode. The
/// kernel then applies the safety lines itself. A BT-5 bitsboot hands the normal profile over unchanged too, but
/// that is only the Mini-PC (AMD) or QEMU (hypervisor bit), which this rule never matches.
const SAFE_FORCED: &[(&str, &str)] =
    &[("net_drivers", "off"), ("nvme_write", "off"), ("stick_write", "off"), ("usb_msd_write", "off"), ("wlan", "off")];

pub fn init(text: Option<&[u8]>) {
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
    matches!(key, "token" | "wlan_psk" | "claude_api_key")
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
/// takes nothing).
fn secret_never_plain() -> ktest::TestResult {
    ktest::kassert!(get("wlan_psk").is_none(), "wlan_psk is in the plain CONFIG map");
    ktest::kassert!(!summary().split(' ').any(|kv| kv.starts_with("wlan_psk=") && kv != "wlan_psk=***"), "the summary shows wlan_psk");
    Ok(())
}
ktest::ktest!("kernel.config.secret", Safe, secret_never_plain);
