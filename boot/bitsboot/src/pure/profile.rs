//! Boot profiles (BT-5): which machine this is and what that means for the boot. Two machines run BitsOS: the
//! Mini-PC (AMD, the normal profile) and Georg's Gaming-PC (Intel), which boots the stick only in **safe mode**:
//! no network driver, no stick, NVMe, USB-disk or WLAN writes, the firmware's picture, USB input and the console.
//!
//! The choice: CONFIG `profile=normal|gaming-pc` decides when present (`auto` or a value bitsboot does not know
//! leaves it to the machine). Otherwise the DMI identity of the Gaming-PC, then the CPU: an Intel CPU on bare
//! metal is the Gaming-PC. Under a hypervisor only DMI counts, because every QEMU test runs on the Gaming-PC's
//! own Intel CPU (`-cpu host`) and must keep the normal profile.
//!
//! The kernel learns the profile from the CONFIG record: [`apply`] adds the profile's lines to the end of the
//! file, so they win over the stick's own lines (the stick's CONFIG.TXT is shared with the Mini-PC, where Georg may
//! have set `nvme_write=on`). Lines a user may want to change (`panic=`) go before the file, so his lines win.
//! The normal profile hands the file over unchanged, byte for byte.

use crate::config;
use crate::dmi::Ident;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const KEY: &str = "profile";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Normal,
    GamingPc,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Profile::Normal => "normal",
            Profile::GamingPc => "gaming-pc",
        }
    }
    pub fn safe(self) -> bool {
        self == Profile::GamingPc
    }
}

/// The Gaming-PC's DMI names (vendor, product), matched against the system and the board structure
/// (`/sys/class/dmi/id` on CachyOS, 20.10.2026: ASRock Z890 Taichi OCF, Intel Core Ultra 7 265K).
pub const GAMING_PC_DMI: &[(&str, &str)] = &[("ASRock", "Z890 Taichi OCF")];

/// Lines added after the stick's CONFIG.TXT (they win): what safe mode means.
pub const SAFE_FORCED: &[(&str, &str)] = &[
    ("net_drivers", "off"),
    ("nvme_write", "off"),
    ("stick_write", "off"),
    ("usb_msd_write", "off"),
    ("wlan", "off"),
    ("screen", "firmware"),
];

/// Lines put before the stick's CONFIG.TXT (its own lines win): safe-mode defaults. `panic=halt` keeps a
/// crash report on the screen; a reboot would only start the same kernel again (safe mode writes no marker).
pub const SAFE_DEFAULTS: &[(&str, &str)] = &[("panic", "halt")];

/// The CPU as CPUID leaf 0 and leaf 1 ECX bit 31 tell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub vendor: [u8; 12],
    pub hypervisor: bool,
}

impl Cpu {
    pub fn intel(&self) -> bool {
        &self.vendor == b"GenuineIntel"
    }
    pub fn vendor_text(&self) -> &str {
        core::str::from_utf8(&self.vendor).unwrap_or("?")
    }
}

/// What decided the profile (short name for the log and the `profile_reason=` line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Config,
    Dmi,
    Cpuid,
    Hypervisor,
    OtherCpu,
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::Config => "config",
            Reason::Dmi => "dmi",
            Reason::Cpuid => "cpuid",
            Reason::Hypervisor => "hypervisor",
            Reason::OtherCpu => "cpu",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub profile: Profile,
    pub reason: Reason,
    /// One sentence for the loader log.
    pub detail: String,
    /// A CONFIG value bitsboot did not understand (then the machine decided).
    pub warning: Option<String>,
}

fn same(a: &Option<String>, b: &str) -> bool {
    a.as_deref().is_some_and(|a| a.trim().eq_ignore_ascii_case(b))
}

/// The Gaming-PC entry the DMI strings match, if any (system or board structure).
pub fn dmi_match(id: &Ident) -> Option<(&'static str, &'static str)> {
    GAMING_PC_DMI.iter().copied().find(|(v, p)| {
        (same(&id.sys_vendor, v) && same(&id.product, p)) || (same(&id.board_vendor, v) && same(&id.board_name, p))
    })
}

pub fn decide(config_text: &[u8], cpu: &Cpu, dmi: Option<&Ident>) -> Decision {
    let d = |profile, reason, detail: String, warning| Decision { profile, reason, detail, warning };
    let mut warning = None;
    match config::get(config_text, KEY).map(|v| v.to_ascii_lowercase()) {
        Some(v) if v == "normal" => return d(Profile::Normal, Reason::Config, String::from("CONFIG profile=normal"), None),
        Some(v) if v == "gaming-pc" => {
            return d(Profile::GamingPc, Reason::Config, String::from("CONFIG profile=gaming-pc"), None);
        }
        Some(v) if v == "auto" || v.is_empty() => {}
        Some(v) => warning = Some(format!("profile={v} is not auto, normal or gaming-pc: the machine decides")),
        None => {}
    }
    if let Some((v, p)) = dmi.and_then(dmi_match) {
        return d(Profile::GamingPc, Reason::Dmi, format!("DMI names the Gaming-PC ({v} {p})"), warning);
    }
    if cpu.hypervisor {
        let detail = format!("{} under a hypervisor and no Gaming-PC DMI names (QEMU tests keep the normal profile)", cpu.vendor_text());
        return d(Profile::Normal, Reason::Hypervisor, detail, warning);
    }
    if cpu.intel() {
        let detail = String::from("GenuineIntel on bare metal (the Mini-PC is AMD): the Gaming-PC");
        return d(Profile::GamingPc, Reason::Cpuid, detail, warning);
    }
    d(Profile::Normal, Reason::OtherCpu, format!("{} on bare metal", cpu.vendor_text()), warning)
}

/// The CONFIG text handed to the kernel. Normal: `text` itself. Safe mode: the defaults, `text`, then the
/// forced lines with `profile=` and `profile_reason=` (comment lines say where they come from).
pub fn apply(text: &[u8], d: &Decision) -> Vec<u8> {
    if !d.profile.safe() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 512);
    out.extend_from_slice(b"# bitsboot: safe-mode defaults (the lines of CONFIG.TXT below win)\n");
    for (k, v) in SAFE_DEFAULTS {
        out.extend_from_slice(format!("{k}={v}\n").as_bytes());
    }
    out.extend_from_slice(text);
    if !text.is_empty() && !text.ends_with(b"\n") {
        out.push(b'\n');
    }
    let head = format!(
        "# bitsboot: Gaming-PC safe mode ({}); profile=normal in CONFIG.TXT turns it off\n{KEY}={}\nprofile_reason={}\n",
        d.detail,
        d.profile.name(),
        d.reason.name()
    );
    out.extend_from_slice(head.as_bytes());
    for (k, v) in SAFE_FORCED {
        out.extend_from_slice(format!("{k}={v}\n").as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD: Cpu = Cpu { vendor: *b"AuthenticAMD", hypervisor: false };
    const INTEL: Cpu = Cpu { vendor: *b"GenuineIntel", hypervisor: false };
    const QEMU_INTEL: Cpu = Cpu { vendor: *b"GenuineIntel", hypervisor: true };

    fn ident(sv: &str, p: &str, bv: &str, bn: &str) -> Ident {
        let o = |s: &str| if s.is_empty() { None } else { Some(String::from(s)) };
        Ident { sys_vendor: o(sv), product: o(p), board_vendor: o(bv), board_name: o(bn) }
    }

    #[test]
    fn machines() {
        let gaming = ident("ASRock", "Z890 Taichi OCF", "ASRock", "Z890 Taichi OCF");
        let qemu = ident("QEMU", "Standard PC (Q35 + ICH9, 2009)", "", "");
        let mini = ident("AZW", "SER8", "AZW", "SER8");
        // The Gaming-PC on bare metal: DMI first.
        let r = decide(b"", &INTEL, Some(&gaming));
        assert_eq!((r.profile, r.reason), (Profile::GamingPc, Reason::Dmi));
        // Without readable DMI an Intel CPU on bare metal is enough.
        assert_eq!(decide(b"", &INTEL, None).reason, Reason::Cpuid);
        assert_eq!(decide(b"", &INTEL, Some(&qemu)).profile, Profile::GamingPc);
        // The Mini-PC.
        let r = decide(b"", &AMD, Some(&mini));
        assert_eq!((r.profile, r.reason), (Profile::Normal, Reason::OtherCpu));
        // QEMU on the Gaming-PC's CPU: normal unless the DMI names are injected.
        let r = decide(b"", &QEMU_INTEL, Some(&qemu));
        assert_eq!((r.profile, r.reason), (Profile::Normal, Reason::Hypervisor));
        assert_eq!(decide(b"", &QEMU_INTEL, None).profile, Profile::Normal);
        assert_eq!(decide(b"", &QEMU_INTEL, Some(&gaming)).profile, Profile::GamingPc);
        // Only the board structure, other case and padding.
        let board = ident("To Be Filled By O.E.M.", "To Be Filled By O.E.M.", " asrock ", "z890 taichi ocf");
        assert_eq!(decide(b"", &QEMU_INTEL, Some(&board)).reason, Reason::Dmi);
        // A vendor alone, or the product of another vendor, is not the Gaming-PC.
        assert_eq!(decide(b"", &QEMU_INTEL, Some(&ident("ASRock", "B650", "", ""))).profile, Profile::Normal);
        assert_eq!(decide(b"", &QEMU_INTEL, Some(&ident("MSI", "Z890 Taichi OCF", "", ""))).profile, Profile::Normal);
    }

    #[test]
    fn config_decides() {
        let gaming = ident("ASRock", "Z890 Taichi OCF", "", "");
        let r = decide(b"profile=normal\n", &INTEL, Some(&gaming));
        assert_eq!((r.profile, r.reason), (Profile::Normal, Reason::Config));
        let r = decide(b"profile = Gaming-PC # test\n", &AMD, None);
        assert_eq!((r.profile, r.reason), (Profile::GamingPc, Reason::Config));
        // Last line wins; auto and an empty value leave it to the machine.
        assert_eq!(decide(b"profile=gaming-pc\nprofile=auto\n", &AMD, None).profile, Profile::Normal);
        assert_eq!(decide(b"profile=\n", &INTEL, None).profile, Profile::GamingPc);
        // An unknown value: the machine decides, with a warning.
        let r = decide(b"profile=nomral\n", &INTEL, None);
        assert_eq!((r.profile, r.reason), (Profile::GamingPc, Reason::Cpuid));
        assert!(r.warning.unwrap().contains("profile=nomral"));
        let r = decide(b"profile=safe\n", &AMD, None);
        assert_eq!(r.profile, Profile::Normal);
        assert!(r.warning.is_some());
    }

    #[test]
    fn normal_is_byte_for_byte() {
        let d = decide(b"", &AMD, None);
        for t in [&b""[..], b"a=1", b"x=1\r\ny=2\n", b"\xff\xfe"] {
            assert_eq!(apply(t, &d), t.to_vec());
        }
    }

    #[test]
    fn safe_lines_win_where_they_must() {
        let user = b"nvme_write=on\nstick_write=on\nwlan=on\nscreen=2560x1440\npanic=reboot\ntoken=abc\nprofile=gaming-pc";
        let d = decide(user, &AMD, None);
        let out = apply(user, &d);
        let get = |k| config::get(&out, k);
        for (k, v) in SAFE_FORCED {
            assert_eq!(get(k), Some(*v), "{k}");
        }
        assert_eq!(get("panic"), Some("reboot"), "a user's own panic= line wins over the default");
        assert_eq!(get("token"), Some("abc"));
        assert_eq!(get("profile"), Some("gaming-pc"));
        assert_eq!(get("profile_reason"), Some("config"));
        // The user's file is inside, unchanged, with a newline added after its last line.
        let text = core::str::from_utf8(&out).unwrap();
        assert!(text.contains("panic=halt\nnvme_write=on\nstick_write=on"));
        assert!(text.contains("profile=gaming-pc\n# bitsboot: Gaming-PC safe mode (CONFIG profile=gaming-pc)"));
        // Without user lines the defaults hold.
        let out = apply(b"", &decide(b"", &INTEL, None));
        assert_eq!(config::get(&out, "panic"), Some("halt"));
        assert_eq!(config::get(&out, "profile_reason"), Some("cpuid"));
        assert_eq!(config::get(&out, "nvme_write"), Some("off"));
    }

    #[test]
    fn safe_mode_never_turns_a_switch_on() {
        // R8a/R8d: a profile may only turn things off.
        for (k, v) in SAFE_FORCED.iter().chain(SAFE_DEFAULTS) {
            assert_ne!(*v, "on", "{k}");
        }
    }

    #[test]
    fn huge_and_odd_files() {
        let mut t = Vec::new();
        for _ in 0..100_000 {
            t.extend_from_slice(b"# padding\n");
        }
        t.extend_from_slice(b"\xff\xfe=1\nnvme_write=on");
        let out = apply(&t, &decide(&t, &INTEL, None));
        assert_eq!(config::get(&out, "nvme_write"), Some("off"));
        assert!(out.len() > t.len());
    }
}
