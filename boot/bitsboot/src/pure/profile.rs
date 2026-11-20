//! Boot profiles (BT-5, GP-GUARD): which machine this is and what that means for the boot. Two machines run
//! BitsOS: the Mini-PC (AMD, the normal profile) and Georg's Gaming-PC (Intel), which boots the stick only in
//! **safe mode**: no driver for any internal disk, no USB-disk or WLAN writes, the firmware's picture, USB input
//! and the console; the network and the boot stick only as far as a tier line releases them
//! (`gp_platform::tiers`: `gp_net=on`, `gp_stick=on`).
//!
//! The choice: the DMI identity of the Gaming-PC decides first and nothing on the stick ends it (Georg 5.11.: the
//! internal SSDs are not even read; a stick line `profile=normal` relaxes nothing on that board). The names count
//! exactly or by the board family's name in any of the four strings (`gp_platform::tiers::board`, the kernel reads
//! them the same way), so a firmware that writes them a little differently changes nothing. Otherwise CONFIG
//! `profile=normal|gaming-pc` decides when present (`auto` or a value bitsboot does not know leaves it to the
//! machine), then the CPU: an Intel CPU on bare metal is taken for the Gaming-PC. `profile=normal` ends that
//! guess only when the DMI names were read and are another vendor's. Under a hypervisor only DMI counts,
//! because every QEMU test runs on the Gaming-PC's own Intel CPU (`-cpu host`) and must keep the normal profile.
//!
//! The kernel learns the profile from the CONFIG record: [`apply`] adds the profile's lines to the end of the
//! file, so they win over the stick's own lines (the stick's CONFIG.TXT may come from the Mini-PC, where Georg may
//! have set `nvme_write=on`). Lines a user may want to change (`panic=`) go before the file, so his lines win.
//! The normal profile hands the file over unchanged, byte for byte.

use crate::config;
use crate::dmi::Ident;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use gp_platform::tiers::board::{self, Likeness, Names};
pub use gp_platform::tiers::Tiers;

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

/// Lines added after the stick's CONFIG.TXT (they win): what safe mode means with the tiers `t`.
pub fn forced(t: Tiers) -> Vec<(&'static str, &'static str)> {
    gp_platform::tiers::forced(t)
}

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
    /// What the stick's tier lines release (all off in the normal profile, where nothing is forced anyway).
    pub tiers: Tiers,
    /// The stick says `profile=normal` and this machine does not honour it (said on the screen).
    pub normal_refused: bool,
}

/// The loader's DMI strings as the shared board rule reads them.
pub fn names(id: &Ident) -> Names<'_> {
    Names { sys_vendor: id.sys_vendor.as_deref(), product: id.product.as_deref(), board_vendor: id.board_vendor.as_deref(), board_name: id.board_name.as_deref() }
}

pub fn decide(config_text: &[u8], cpu: &Cpu, dmi: Option<&Ident>) -> Decision {
    let tiers = Tiers::read(|k| config::get(config_text, k));
    let d = |profile: Profile, reason, detail: String, warning, normal_refused| {
        let tiers = if profile.safe() { tiers } else { Tiers::SAFE };
        Decision { profile, reason, detail, warning, tiers, normal_refused }
    };
    let wanted = config::get(config_text, KEY).map(|v| v.to_ascii_lowercase());
    let wants_normal = wanted.as_deref() == Some("normal");
    let warning = match wanted.as_deref() {
        None | Some("" | "auto" | "normal" | "gaming-pc") => None,
        Some(v) => Some(format!("profile={v} is not auto, normal or gaming-pc: the machine decides")),
    };
    // The board itself first: nothing on the stick ends safe mode there.
    let names = dmi.map(names);
    let like = names.as_ref().map_or(Likeness::None, board::likeness);
    match like {
        Likeness::Exact => {
            let detail = format!("DMI names the Gaming-PC ({} {})", board::VENDOR, board::PRODUCT);
            return d(Profile::GamingPc, Reason::Dmi, detail, warning, wants_normal);
        }
        Likeness::Family => {
            let detail = format!("DMI names resemble the Gaming-PC ({})", dmi.map(Ident::text).unwrap_or_default());
            return d(Profile::GamingPc, Reason::Dmi, detail, warning, wants_normal);
        }
        Likeness::Vendor | Likeness::None => {}
    }
    match wanted.as_deref() {
        // An Intel machine on bare metal whose names could not be read, or carry the board's vendor, may be the
        // Gaming-PC: its disks come first.
        Some("normal") if cpu.intel() && !cpu.hypervisor && board::normal_refused_on_intel(names.as_ref()) => {
            if like == Likeness::Vendor {
                let detail = format!("DMI names resemble the Gaming-PC (vendor {} on an Intel CPU)", board::VENDOR);
                return d(Profile::GamingPc, Reason::Dmi, detail, None, true);
            }
            let detail = String::from("GenuineIntel on bare metal and no readable DMI names: this may be the Gaming-PC");
            return d(Profile::GamingPc, Reason::Cpuid, detail, None, true);
        }
        Some("normal") => return d(Profile::Normal, Reason::Config, String::from("CONFIG profile=normal"), None, false),
        Some("gaming-pc") => return d(Profile::GamingPc, Reason::Config, String::from("CONFIG profile=gaming-pc"), None, false),
        _ => {}
    }
    if cpu.hypervisor {
        let detail = format!("{} under a hypervisor and no Gaming-PC DMI names (QEMU tests keep the normal profile)", cpu.vendor_text());
        return d(Profile::Normal, Reason::Hypervisor, detail, warning, false);
    }
    if cpu.intel() {
        let detail = String::from("GenuineIntel on bare metal (the Mini-PC is AMD): the Gaming-PC");
        return d(Profile::GamingPc, Reason::Cpuid, detail, warning, false);
    }
    d(Profile::Normal, Reason::OtherCpu, format!("{} on bare metal", cpu.vendor_text()), warning, false)
}

/// What the error screen says about a KERNEL without the disk guard's mark (Georg reads it; then English).
pub const UNGUARDED: &str = "zu alt für diesen PC: diese BitsOS-Version schützt die eingebauten Platten noch nicht und wird hier nicht gestartet / older than the disk guard, not started on this machine";

/// GP-GUARD: the KERNEL file carries the disk guard's mark in one of its loaded segments, so it knows
/// `nvme_bind=off` and the Gaming-PC's board (`gp_platform::disk_rule::marker`).
pub fn kernel_guarded(file: &[u8], segs: &[crate::elf::Segment]) -> bool {
    segs.iter().any(|s| {
        let end = s.offset.saturating_add(s.filesz);
        file.get(s.offset as usize..end as usize).is_some_and(gp_platform::disk_rule::marker::has_mark)
    })
}

/// The CONFIG text handed to the kernel. Normal: `text` itself. Safe mode: the defaults, `text`, then the
/// forced lines of the decision's tiers with `profile=` and `profile_reason=` (comment lines say where they
/// come from).
pub fn apply(text: &[u8], d: &Decision) -> Vec<u8> {
    if !d.profile.safe() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 640);
    out.extend_from_slice(b"# bitsboot: safe-mode defaults (the lines of CONFIG.TXT below win)\n");
    for (k, v) in SAFE_DEFAULTS {
        out.extend_from_slice(format!("{k}={v}\n").as_bytes());
    }
    out.extend_from_slice(text);
    if !text.is_empty() && !text.ends_with(b"\n") {
        out.push(b'\n');
    }
    let head = format!(
        "# bitsboot: Gaming-PC safe mode ({}); only the tier lines gp_net, gp_stick and gp_nvme in CONFIG.TXT release anything\n{KEY}={}\nprofile_reason={}\n",
        d.detail,
        d.profile.name(),
        d.reason.name()
    );
    out.extend_from_slice(head.as_bytes());
    if d.normal_refused {
        // For the report page: the stick asked to leave safe mode and this machine does not let it.
        out.extend_from_slice(format!("{}=normal\n", gp_platform::tiers::REFUSED_KEY).as_bytes());
    }
    for (k, v) in forced(d.tiers) {
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
        // A vendor alone is not the Gaming-PC; the board's name is, whatever stands in the vendor string.
        assert_eq!(decide(b"", &QEMU_INTEL, Some(&ident("ASRock", "B650", "", ""))).profile, Profile::Normal);
        assert_eq!(decide(b"", &AMD, Some(&ident("ASRock", "B650", "ASRock", "B650"))).profile, Profile::Normal);
        let r = decide(b"", &QEMU_INTEL, Some(&ident("To Be Filled By O.E.M.", "Z890 Taichi OCF", "", "")));
        assert_eq!((r.profile, r.reason), (Profile::GamingPc, Reason::Dmi));
        assert!(r.detail.contains("resemble"), "{}", r.detail);
    }

    #[test]
    fn config_decides() {
        let other = ident("Micro-Star", "MS-7E06", "", "");
        // Another Intel machine on bare metal whose DMI names were read: profile=normal ends the guess.
        let r = decide(b"profile=normal\n", &INTEL, Some(&other));
        assert_eq!((r.profile, r.reason, r.normal_refused), (Profile::Normal, Reason::Config, false));
        assert_eq!(decide(b"profile=normal\n", &AMD, None).profile, Profile::Normal);
        assert_eq!(decide(b"profile=normal\n", &QEMU_INTEL, None).reason, Reason::Config);
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

    /// B3c: the board's DMI names beat a stick line `profile=normal`, in every spelling and position, and so does
    /// an Intel CPU on bare metal whose DMI names could not be read.
    #[test]
    fn profile_normal_never_ends_safe_mode_on_the_gaming_pc() {
        let gaming = ident("ASRock", "Z890 Taichi OCF", "ASRock", "Z890 Taichi OCF");
        let board_only = ident("To Be Filled By O.E.M.", "To Be Filled By O.E.M.", "ASRock", "Z890 Taichi OCF");
        let texts: [&[u8]; 6] = [
            b"profile=normal\n",
            b"profile = NORMAL \r\n",
            b"nvme_write=on\nprofile=normal",
            b"profile=gaming-pc\nprofile=normal\n",
            b"profile=normal\ngp_net=on\ngp_stick=on\n",
            b"\xff\xfe\nprofile=normal\n",
        ];
        for t in texts {
            for cpu in [INTEL, QEMU_INTEL, AMD] {
                for id in [&gaming, &board_only] {
                    let r = decide(t, &cpu, Some(id));
                    assert_eq!((r.profile, r.reason, r.normal_refused), (Profile::GamingPc, Reason::Dmi, true));
                    let out = apply(t, &r);
                    assert_eq!(config::get(&out, "profile_refused"), Some("normal"));
                    assert_eq!(config::get(&out, "profile"), Some("gaming-pc"));
                    for (k, v) in gp_platform::tiers::NEVER_RELAXED {
                        assert_eq!(config::get(&out, k), Some(*v), "{k}");
                    }
                }
            }
            // DMI not readable (no table, or a table without names): Intel on bare metal stays safe.
            for dmi in [None, Some(ident("", "", "", ""))] {
                let r = decide(t, &INTEL, dmi.as_ref());
                assert_eq!((r.profile, r.reason, r.normal_refused), (Profile::GamingPc, Reason::Cpuid, true));
                assert_eq!(config::get(&apply(t, &r), "nvme_bind"), Some("off"));
            }
            // The names as a firmware update or a cut table could leave them: still this board, on any CPU.
            for id in [
                ident("ASRock Inc.", "Z890 Taichi OCF", "ASRock Inc.", "Z890 Taichi OCF"),
                ident("ASRock", "Z890 Taichi OCF/BIOS 3.10", "", ""),
                ident("", "", "ASRock", "Z890-Taichi-OCF"),
                ident("", "Z890 TAICHI OCF", "", ""),
                ident("To Be Filled By O.E.M.", "To Be Filled By O.E.M.", "", "Z890 Taichi"),
            ] {
                for cpu in [INTEL, QEMU_INTEL, AMD] {
                    let r = decide(t, &cpu, Some(&id));
                    assert_eq!((r.profile, r.reason, r.normal_refused), (Profile::GamingPc, Reason::Dmi, true), "{}", id.text());
                    assert!(r.detail.contains("resemble"));
                    let out = apply(t, &r);
                    for (k, v) in gp_platform::tiers::NEVER_RELAXED {
                        assert_eq!(config::get(&out, k), Some(*v), "{k}");
                    }
                }
            }
            // The board's vendor alone with an Intel CPU on bare metal: `profile=normal` is not enough either.
            for id in [ident("ASRock", "To Be Filled By O.E.M.", "", ""), ident("", "", "ASRock Inc.", "Z790 Nova")] {
                let r = decide(t, &INTEL, Some(&id));
                assert_eq!((r.profile, r.reason, r.normal_refused), (Profile::GamingPc, Reason::Dmi, true), "{}", id.text());
                assert_eq!(config::get(&apply(t, &r), "nvme_bind"), Some("off"));
                // Not on AMD and not under a hypervisor: there the vendor alone says nothing.
                assert_eq!(decide(t, &AMD, Some(&id)).profile, Profile::Normal);
                assert_eq!(decide(t, &QEMU_INTEL, Some(&id)).profile, Profile::Normal);
            }
        }
        // Without the line nothing is refused.
        assert!(!decide(b"", &INTEL, Some(&gaming)).normal_refused);
        assert_eq!(config::get(&apply(b"", &decide(b"", &INTEL, Some(&gaming))), "profile_refused"), None);
        assert!(!decide(b"profile=auto\n", &INTEL, None).normal_refused);
        // An unknown value on the board still warns.
        assert!(decide(b"profile=nomral\n", &INTEL, Some(&gaming)).warning.is_some());
    }

    /// N0 and gp_stick: a tier line releases its own switch and nothing else; the normal profile has no tiers.
    #[test]
    fn tiers_release_only_their_switch() {
        let gaming = ident("ASRock", "Z890 Taichi OCF", "", "");
        let hostile = "nvme_write=on\nnvme_bind=on\nnvme_data=S:G:1:2\nnvme_boot=S:G:1:2\nusb_msd_write=on\nstick_write=on\nscreen=2560x1440\n";
        for (net, stick) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut text = String::from(hostile);
            if net {
                text.push_str("gp_net=on\n");
            }
            if stick {
                text.push_str("gp_stick=on\n");
            }
            let d = decide(text.as_bytes(), &INTEL, Some(&gaming));
            assert_eq!(d.tiers, Tiers { net, stick, nvme: false });
            let out = apply(text.as_bytes(), &d);
            let get = |k| config::get(&out, k);
            assert_eq!(get("gp_net"), Some(if net { "on" } else { "off" }));
            assert_eq!(get("gp_stick"), Some(if stick { "on" } else { "off" }));
            assert_eq!(get("nvme_bind"), Some("off"));
            assert_eq!(get("nvme_write"), Some("off"));
            assert_eq!(get("nvme_data"), Some(""));
            assert_eq!(get("nvme_boot"), Some(""));
            assert_eq!(get("usb_msd_write"), Some("off"));
            assert_eq!(get("stick_write"), Some("off"), "the raw stick switch stays off under gp_stick too");
            assert_eq!(get("screen"), Some("firmware"));
            assert_eq!(d.tiers.stick_id_withheld(), !stick);
        }
        // A tier line that is not exactly `on` releases nothing; a later line wins.
        for text in ["gp_net=ON\n", "gp_net=1\n", "gp_net=on\ngp_net=off\n", "#gp_net=on\n", "gp_net =\n"] {
            let d = decide(text.as_bytes(), &INTEL, Some(&gaming));
            assert_eq!(d.tiers, Tiers::SAFE, "{text:?}");
            assert_eq!(config::get(&apply(text.as_bytes(), &d), "gp_net"), Some("off"));
        }
        // The Mini-PC and QEMU: the lines are just lines of the file.
        let d = decide(b"gp_net=on\ngp_stick=on\n", &AMD, None);
        assert_eq!((d.profile, d.tiers), (Profile::Normal, Tiers::SAFE));
        assert_eq!(apply(b"gp_net=on\ngp_stick=on\n", &d), b"gp_net=on\ngp_stick=on\n".to_vec());
    }

    /// GP-DISK: `gp_nvme=` naming a controller leaves the Mini-PC's three NVMe lines as the stick says them and the
    /// line itself in place; `nvme_bind=off` and everything else stay forced. A line naming Georg's Windows or
    /// CachyOS drive, or one that cannot be read, is taken away and releases nothing.
    #[test]
    fn gp_nvme_releases_the_three_nvme_lines_only() {
        let gaming = ident("ASRock", "Z890 Taichi OCF", "", "");
        let stick = "gp_net=on\ngp_stick=on\nnvme_write=on\nnvme_data=S1:G:2048:1000\nnvme_boot=S1:G:1:2\nusb_msd_write=on\nstick_write=on\nnvme_bind=on\n";
        let text = format!("{stick}gp_nvme=81:00.0/1d97:5236\n");
        let d = decide(text.as_bytes(), &INTEL, Some(&gaming));
        assert_eq!(d.tiers, Tiers { net: true, stick: true, nvme: true });
        let out = apply(text.as_bytes(), &d);
        let get = |k| config::get(&out, k);
        assert_eq!(get("gp_nvme"), Some("81:00.0/1d97:5236"));
        assert_eq!((get("nvme_write"), get("nvme_data"), get("nvme_boot")), (Some("on"), Some("S1:G:2048:1000"), Some("S1:G:1:2")));
        assert_eq!((get("nvme_bind"), get("usb_msd_write"), get("stick_write")), (Some("off"), Some("off"), Some("off")));
        for bad in ["01:00.0/144d:a80d", "86:00.0/144d:a80a", "81:00.0", "on", "81:00.0/1d97:5236x"] {
            let text = format!("{stick}gp_nvme={bad}\n");
            let d = decide(text.as_bytes(), &INTEL, Some(&gaming));
            assert!(!d.tiers.nvme, "{bad}");
            let out = apply(text.as_bytes(), &d);
            let get = |k| config::get(&out, k);
            assert_eq!((get("gp_nvme"), get("nvme_write"), get("nvme_data"), get("nvme_boot")), (Some(""), Some("off"), Some(""), Some("")), "{bad}");
        }
        // The Mini-PC: just a line of the file, the file byte for byte.
        let d = decide(text.as_bytes(), &AMD, None);
        assert_eq!((d.profile, d.tiers), (Profile::Normal, Tiers::SAFE));
        assert_eq!(apply(text.as_bytes(), &d), text.into_bytes());
    }

    /// GP-GUARD: only a kernel with the mark inside a loaded segment counts as guarded.
    #[test]
    fn a_kernel_without_the_mark_is_not_guarded() {
        use crate::elf::Segment;
        use gp_platform::disk_rule::marker::KERNEL_MARK;
        let seg = |offset: u64, filesz: u64| Segment { vaddr: 0, offset, filesz, memsz: filesz, flags: 4 };
        let mut file = alloc::vec![0x41u8; 4096];
        let segs = [seg(0x400, 0x400), seg(0x800, 0x400)];
        assert!(!kernel_guarded(&file, &segs), "a kernel built before the guard");
        // The mark outside every loaded segment (a debug section, a note): not the kernel's own data.
        file[0x100..0x100 + KERNEL_MARK.len()].copy_from_slice(KERNEL_MARK);
        file[0xE00..0xE00 + KERNEL_MARK.len()].copy_from_slice(KERNEL_MARK);
        assert!(!kernel_guarded(&file, &segs));
        // In the second segment: guarded.
        file[0x900..0x900 + KERNEL_MARK.len()].copy_from_slice(KERNEL_MARK);
        assert!(kernel_guarded(&file, &segs));
        // Hostile segment tables never read past the file.
        assert!(!kernel_guarded(&file, &[seg(4000, 1000), seg(u64::MAX - 4, 100), seg(1 << 40, 16)]));
        assert!(!kernel_guarded(&[], &segs));
        assert!(UNGUARDED.contains("eingebauten Platten"));
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
        let user = b"nvme_write=on\nstick_write=on\nscreen=2560x1440\npanic=reboot\ntoken=abc\nprofile=gaming-pc";
        let d = decide(user, &AMD, None);
        let out = apply(user, &d);
        let get = |k| config::get(&out, k);
        for (k, v) in forced(Tiers::SAFE) {
            assert_eq!(get(k), Some(v), "{k}");
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
        for (k, v) in forced(Tiers::SAFE).iter().chain(SAFE_DEFAULTS) {
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
