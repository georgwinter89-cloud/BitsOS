//! What a Linux computer has for QEMU: the distribution (for the install command), `qemu-system-x86_64`, its UEFI
//! firmware, KVM and a window. Every lookup takes the root folder, so the tests hand it a fake tree.

use crate::json::{self, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Distro {
    pub id: String,
    pub like: Vec<String>,
    pub name: String,
}

/// `/etc/os-release` (or `/usr/lib/os-release`): `KEY=value`, the value maybe quoted.
pub fn parse_os_release(text: &str) -> Distro {
    let mut d = Distro::default();
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match k.trim() {
            "ID" => d.id = v.to_ascii_lowercase(),
            "ID_LIKE" => d.like = v.split_whitespace().map(|s| s.to_ascii_lowercase()).collect(),
            "PRETTY_NAME" => d.name = v,
            "NAME" if d.name.is_empty() => d.name = v,
            _ => {}
        }
    }
    d
}

pub fn distro(root: &Path) -> Distro {
    ["etc/os-release", "usr/lib/os-release"].iter().find_map(|p| std::fs::read_to_string(root.join(p)).ok()).map(|t| parse_os_release(&t)).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pm {
    Pacman,
    Apt,
    Dnf,
    Zypper,
}

/// The package manager of `d` or of the family it names itself like (CachyOS is like Arch, Mint like Ubuntu).
pub fn package_manager(d: &Distro) -> Option<Pm> {
    let names = std::iter::once(d.id.as_str()).chain(d.like.iter().map(String::as_str));
    for n in names {
        let pm = match n {
            "arch" | "archarm" | "manjaro" | "endeavouros" | "cachyos" | "garuda" => Pm::Pacman,
            "debian" | "ubuntu" | "linuxmint" | "pop" | "raspbian" => Pm::Apt,
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => Pm::Dnf,
            "opensuse" | "suse" | "opensuse-leap" | "opensuse-tumbleweed" | "sles" => Pm::Zypper,
            _ => continue,
        };
        return Some(pm);
    }
    None
}

impl Pm {
    /// What runs as root (after the person's yes): QEMU for x86-64 with its window, and OVMF.
    pub fn install_args(self) -> Vec<&'static str> {
        match self {
            Pm::Pacman => vec!["/usr/bin/pacman", "-S", "--needed", "--noconfirm", "qemu-desktop", "edk2-ovmf"],
            Pm::Apt => vec!["/usr/bin/apt-get", "install", "-y", "qemu-system-x86", "qemu-system-gui", "ovmf"],
            Pm::Dnf => vec!["/usr/bin/dnf", "install", "-y", "qemu-system-x86", "qemu-ui-gtk", "edk2-ovmf"],
            Pm::Zypper => vec!["/usr/bin/zypper", "--non-interactive", "install", "qemu-x86", "qemu-ui-gtk", "qemu-ovmf-x86_64"],
        }
    }

    /// Run first: apt installs from its package lists, which a fresh system may not have yet.
    pub fn refresh_args(self) -> Option<Vec<&'static str>> {
        (self == Pm::Apt).then(|| vec!["/usr/bin/apt-get", "update"])
    }

    pub fn command_line(self) -> String {
        let install = format!("sudo {}", self.install_args().join(" "));
        match self.refresh_args() {
            Some(r) => format!("sudo {} && {install}", r.join(" ")),
            None => install,
        }
    }

    /// Said when the installation failed: the usual cause, and its cure.
    pub fn failure_hint(self) -> &'static str {
        match self {
            // pacman -Sy alone would be a partial upgrade, which Arch does not support.
            Pm::Pacman => "an outdated package database is the usual cause: update the system (sudo pacman -Syu), then run the installer again",
            Pm::Apt => "check that the package sources work (sudo apt-get update), then run the installer again",
            Pm::Dnf | Pm::Zypper => "check that the package sources work, then run the installer again",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firmware {
    pub code: PathBuf,
    /// The template every image copies once for its own variables.
    pub vars: PathBuf,
}

/// Firmware that cannot boot an unsigned loader on a plain q35. The confidential-computing features (`amd-sev`,
/// `amd-sev-es`, `amd-sev-snp`, `intel-tdx`) say what a build supports, not what it needs; builds that only run that
/// way are mapped to memory, not flash, and fail the mapping check.
const UNFIT: &[&str] = &["secure-boot", "enrolled-keys", "requires-smm"];

fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

/// One QEMU firmware descriptor (docs/interop/firmware.json): UEFI for x86-64 q35 in split flash with a variables
/// template, without Secure Boot.
fn descriptor(root: &Path, text: &str) -> Option<Firmware> {
    let v = json::parse(text).ok()?;
    let has = |k: &str, want: &str| matches!(v.get(k), Some(Value::Arr(a)) if a.iter().any(|x| matches!(x, Value::Str(s) if s == want)));
    if !has("interface-types", "uefi") {
        return None;
    }
    if let Some(Value::Arr(f)) = v.get("features")
        && f.iter().any(|x| matches!(x, Value::Str(s) if UNFIT.contains(&s.as_str())))
    {
        return None;
    }
    let Some(Value::Arr(targets)) = v.get("targets") else { return None };
    let q35 = targets.iter().any(|t| {
        t.str("architecture").ok() == Some("x86_64") && matches!(t.get("machines"), Some(Value::Arr(m)) if m.iter().any(|x| matches!(x, Value::Str(s) if s.starts_with("pc-q35") || s == "*")))
    });
    let m = v.get("mapping")?;
    if !q35 || m.str("device").ok() != Some("flash") || m.str("mode").is_ok_and(|x| x != "split") {
        return None;
    }
    let file = |k: &str| m.get(k).filter(|e| e.str("format").ok().is_none_or(|f| f == "raw")).and_then(|e| e.str("filename").ok()).map(|f| under(root, f));
    let fw = Firmware { code: file("executable")?, vars: file("nvram-template")? };
    (fw.code.is_file() && fw.vars.is_file()).then_some(fw)
}

/// Where distributions put OVMF when they ship no descriptor for it.
const KNOWN: &[(&str, &str)] = &[
    ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
    ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
    ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
    ("/usr/share/qemu/ovmf-x86_64-code.bin", "/usr/share/qemu/ovmf-x86_64-vars.bin"),
];

/// QEMU's own descriptors first (in their priority order: by file name, /etc over /usr/share), then the known paths.
pub fn find_firmware(root: &Path) -> Option<Firmware> {
    let mut seen: Vec<(String, PathBuf)> = Vec::new();
    for d in ["etc/qemu/firmware", "usr/share/qemu/firmware"] {
        for e in std::fs::read_dir(root.join(d)).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".json") && !seen.iter().any(|(s, _)| *s == n) {
                seen.push((n, e.path()));
            }
        }
    }
    seen.sort();
    seen.iter().find_map(|(_, p)| std::fs::read_to_string(p).ok().and_then(|t| descriptor(root, &t))).or_else(|| {
        KNOWN.iter().map(|(c, v)| Firmware { code: under(root, c), vars: under(root, v) }).find(|f| f.code.is_file() && f.vars.is_file())
    })
}

/// `qemu-system-x86_64` on PATH, else in the usual places.
pub fn find_qemu(root: &Path, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let from_path: Vec<PathBuf> = path_env.map(|p| std::env::split_paths(p).filter(|d| d.is_absolute()).map(|d| under(root, &d.to_string_lossy())).collect()).unwrap_or_default();
    from_path.into_iter().chain(["usr/bin", "usr/local/bin"].iter().map(|d| root.join(d))).map(|d| d.join("qemu-system-x86_64")).find(|p| p.is_file())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kvm {
    Usable,
    /// /dev/kvm is there, but this user may not open it.
    NoAccess,
    Missing,
}

pub fn kvm(root: &Path) -> Kvm {
    let p = root.join("dev/kvm");
    match std::fs::OpenOptions::new().read(true).write(true).open(&p) {
        Ok(_) => Kvm::Usable,
        Err(_) if p.exists() => Kvm::NoAccess,
        Err(_) => Kvm::Missing,
    }
}

impl Kvm {
    /// Why BitsOS will be slow, and what makes it fast.
    pub fn note(self) -> Option<String> {
        match self {
            Kvm::Usable => None,
            Kvm::NoAccess => Some("/dev/kvm exists, but you may not use it: add yourself to the group kvm (sudo usermod -aG kvm $USER), log out and in; BitsOS uses KVM from its next start".into()),
            Kvm::Missing => Some("there is no /dev/kvm: turn virtualization (AMD-V/SVM or Intel VT-x) on in the firmware settings, or load the kvm module".into()),
        }
    }
}

/// The window QEMU can open, from `qemu-system-x86_64 -display help`: GTK, else SDL.
pub fn pick_display(help: &str) -> Option<&'static str> {
    let has = |n: &str| help.lines().any(|l| l.trim() == n);
    ["gtk", "sdl"].into_iter().find(|n| has(n))
}

pub fn display_of(qemu: &Path) -> Option<&'static str> {
    let out = std::process::Command::new(qemu).args(["-display", "help"]).stdin(std::process::Stdio::null()).output().ok()?;
    pick_display(&String::from_utf8_lossy(&out.stdout))
}

/// The guest's memory: 4 GiB, or half of a host with less than 8 GiB (at least 2 GiB).
pub fn mem_mb(host_bytes: Option<u64>) -> u64 {
    match host_bytes {
        Some(b) if b < 8 << 30 => ((b >> 20) / 2).max(2048),
        _ => 4096,
    }
}

/// 4 vCPUs, and always one host CPU left over.
pub fn cpus(host: usize) -> u32 {
    host.saturating_sub(1).clamp(1, 4) as u32
}

pub fn meminfo_bytes(text: &str) -> Option<u64> {
    let l = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    l.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// The effective user id from /proc/self/status: QEMU mode writes only the person's own folders, never as root.
pub fn euid(status: &str) -> Option<u32> {
    status.lines().find(|l| l.starts_with("Uid:"))?.split_whitespace().nth(2)?.parse().ok()
}
