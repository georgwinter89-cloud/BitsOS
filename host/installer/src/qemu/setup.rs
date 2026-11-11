//! Finding QEMU, its firmware and its window on this computer, and installing what is missing once the person said
//! yes: the distribution's packages on Linux, the pinned QEMU for Windows installer on Windows.

use super::args::Os;
use super::host::{self, Pm};
use super::win;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Found {
    pub qemu: PathBuf,
    pub code: PathBuf,
    pub vars: PathBuf,
    pub display: String,
}

pub const OS: Os = if cfg!(windows) { Os::Windows } else { Os::Linux };

/// What is there, or what is missing (in words for the person).
pub fn detect() -> Result<Found, Vec<&'static str>> {
    match OS {
        Os::Linux => linux(),
        Os::Windows => windows().ok_or_else(|| vec!["QEMU for Windows"]),
    }
}

fn linux() -> Result<Found, Vec<&'static str>> {
    let root = Path::new("/");
    let qemu = host::find_qemu(root, std::env::var_os("PATH").as_deref());
    let fw = host::find_firmware(root);
    let display = qemu.as_deref().and_then(host::display_of);
    let mut lack = Vec::new();
    match (&qemu, display) {
        (None, _) => lack.push("QEMU (qemu-system-x86_64)"),
        (Some(_), None) => lack.push("QEMU's window (its GTK or SDL part)"),
        _ => {}
    }
    if fw.is_none() {
        lack.push("the UEFI firmware for QEMU (OVMF)");
    }
    match (qemu, fw, display) {
        (Some(qemu), Some(fw), Some(d)) => Ok(Found { qemu, code: fw.code, vars: fw.vars, display: window(d) }),
        _ => Err(lack),
    }
}

/// GTK scales the picture to the window, so a window the screen cannot hold whole still shows all of BitsOS.
pub fn window(backend: &str) -> String {
    if backend == "gtk" { "gtk,zoom-to-fit=on".into() } else { backend.into() }
}

fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    root.join("System32").join(exe)
}

fn program_files() -> Vec<Option<String>> {
    ["ProgramW6432", "ProgramFiles"].iter().map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).collect()
}

/// Where QEMU for Windows lands when this installer installs it (its installer's default).
pub fn windows_default_dir() -> String {
    let pf = program_files().into_iter().flatten().next().unwrap_or_else(|| "C:\\Program Files".into());
    win::join(&pf, "qemu")
}

fn windows() -> Option<Found> {
    let reg = Command::new(system32("reg.exe")).args(["query", "HKLM\\SOFTWARE\\QEMU", "/v", "Install_Dir"]).output().ok().and_then(|o| win::parse_reg_install_dir(&String::from_utf8_lossy(&o.stdout)));
    let i = win::candidates(reg, &program_files()).iter().map(|d| win::install_in(d)).find(|i| i.exe.is_file() && i.code.is_file() && i.vars.is_file())?;
    let display = host::display_of(&i.exe).unwrap_or("sdl");
    Some(Found { qemu: i.exe, code: i.code, vars: i.vars, display: window(display) })
}

/// How the missing parts get installed, in words, and the line the person could type themselves.
pub fn install_text() -> Result<(String, String), String> {
    match OS {
        Os::Linux => {
            let d = host::distro(Path::new("/"));
            let pm = host::package_manager(&d).ok_or_else(|| {
                format!(
                    "this installer does not know the package manager of {}: install QEMU for x86-64 with its GTK window and the OVMF UEFI firmware yourself, then run it again",
                    if d.name.is_empty() { "this Linux" } else { &d.name }
                )
            })?;
            Ok((format!("The packages of {} can be installed (root rights; sudo asks for your password):", if d.name.is_empty() { "your Linux" } else { &d.name }), pm.command_line()))
        }
        Os::Windows => {
            let p = &win::QEMU_FOR_WINDOWS;
            Ok((
                format!(
                    "QEMU for Windows (build {}, {} MB) can be downloaded from qemu.weilnetz.de (the build qemu.org links to), checked\n(SHA-512) and installed into {}; Windows asks for permission once:",
                    p.version,
                    p.size / 1_000_000,
                    windows_default_dir()
                ),
                p.url.to_string(),
            ))
        }
    }
}

/// Installs what is missing (the person said yes, or --install-qemu).
pub fn install(cache: &Path, offline: bool, terminal: bool) -> Result<(), String> {
    match OS {
        Os::Linux => {
            let pm = host::package_manager(&host::distro(Path::new("/"))).ok_or("no known package manager")?;
            linux_install(pm, terminal)
        }
        Os::Windows => windows_install(cache, offline),
    }
}

fn linux_install(pm: Pm, terminal: bool) -> Result<(), String> {
    let (runner, why) = if terminal { ("sudo", "sudo") } else { ("pkexec", "pkexec (no terminal for sudo)") };
    for a in pm.refresh_args().into_iter().chain([pm.install_args()]) {
        println!("running: {runner} {}", a.join(" "));
        let st = Command::new(runner).args(&a).status().map_err(|e| format!("cannot start {why}: {e}"))?;
        if !st.success() {
            return Err(format!("the package installation failed (exit {}); nothing else was done. {}", st.code().map_or("?".into(), |c| c.to_string()), pm.failure_hint()));
        }
    }
    Ok(())
}

/// Opens the installer so that nobody can change, replace or delete it while the handle is open, and keeps it only
/// when the bytes read through that very handle are the pinned ones.
fn locked_if_pinned(f: &Path, p: &win::Pinned) -> Option<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ only: no writer and no rename or delete (which would let another file take the name).
        o.share_mode(0x1);
    }
    let h = o.open(f).ok()?;
    let ok = h.metadata().is_ok_and(|m| m.len() == p.size) && crate::sha512::of_reader(std::io::BufReader::with_capacity(1 << 20, &h)).is_ok_and(|(sum, _)| sum == p.sha512);
    ok.then_some(h)
}

fn windows_install(cache: &Path, offline: bool) -> Result<(), String> {
    let p = &win::QEMU_FOR_WINDOWS;
    let file = cache.join("qemu").join(p.file_name());
    let held = match locked_if_pinned(&file, p) {
        Some(h) => h,
        None => {
            if offline {
                return Err(format!("QEMU for Windows is not in {} and the run is offline", file.display()));
            }
            crate::fetch::curl_ready()?;
            crate::fetch::download(p.url, &file)?;
            locked_if_pinned(&file, p).ok_or_else(|| {
                let _ = std::fs::remove_file(&file);
                format!("refused: the downloaded {} is not the file this installer pins (size or SHA-512 differ); it was deleted and not started", p.file_name())
            })?
        }
    };
    println!("{}: size and SHA-512 match; starting its installer (Windows asks for permission)", p.file_name());
    let ps = system32("WindowsPowerShell\\v1.0\\powershell.exe");
    let st = Command::new(&ps).args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]).arg(win::installer_script(&file.display().to_string())).status().map_err(|e| format!("cannot start {}: {e}", ps.display()))?;
    // Held until the elevated installer has ended: the file Windows elevates is the file that was checked.
    drop(held);
    if !st.success() {
        return Err(format!("the QEMU installer did not finish (exit {}; was the permission prompt declined?)", st.code().map_or("?".into(), |c| c.to_string())));
    }
    Ok(())
}

/// Creates the shortcuts on Windows (Desktop and Start menu), or `.cmd` files there should PowerShell fail.
pub fn windows_shortcuts(script: &Path, dir: &Path, icon: &Path) -> Vec<PathBuf> {
    let ps = system32("WindowsPowerShell\\v1.0\\powershell.exe");
    let text = win::shortcut_script(&script.display().to_string(), &dir.display().to_string(), &icon.display().to_string());
    let out = Command::new(&ps).args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]).arg(text).output();
    if let Some(o) = out.ok().filter(|o| o.status.success()) {
        let made: Vec<PathBuf> = String::from_utf8_lossy(&o.stdout).lines().map(str::trim).filter(|l| l.ends_with("BitsOS.lnk")).map(PathBuf::from).filter(|p| p.is_file()).collect();
        if !made.is_empty() {
            return made;
        }
    }
    println!("note: PowerShell could not make the shortcuts; BitsOS.cmd files start BitsOS instead");
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    let dirs = [env("USERPROFILE").map(|u| u.join("Desktop")), env("APPDATA").map(|a| a.join("Microsoft\\Windows\\Start Menu\\Programs"))];
    let body = super::args::forward_cmd(&script.display().to_string());
    dirs.into_iter().flatten().filter(|d| d.is_dir()).map(|d| d.join("BitsOS.cmd")).filter(|p| std::fs::write(p, &body).is_ok()).collect()
}

/// Starts the launcher on its own, so it outlives this program and its terminal. On Linux QEMU's messages go to
/// `output`; the Windows script writes its own `qemu-output.log` (the shortcut starts it the same way).
pub fn start(script: &Path, output: &Path) -> Result<u32, String> {
    let mut c;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = output;
        c = Command::new(system32("cmd.exe"));
        c.raw_arg(format!("/c \"\"{}\"\"", script.display()));
        // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW: no console window beside QEMU's.
        c.creation_flags(0x0000_0200 | 0x0800_0000);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let out = std::fs::File::create(output).map_err(|e| format!("{}: {e}", output.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        c = Command::new(script);
        // Its own process group: closing the terminal this ran in does not take QEMU with it.
        c.process_group(0);
        c.stdout(out).stderr(err);
    }
    let child = c.stdin(std::process::Stdio::null()).spawn().map_err(|e| format!("cannot start {}: {e}", script.display()))?;
    Ok(child.id())
}

/// QEMU still runs this image: its pid file names a live QEMU (Linux), or QEMU still holds the file open (Windows).
pub fn running(pidfile: &Path, image: &Path) -> bool {
    if OS == Os::Windows {
        return pidfile.is_file() && std::fs::OpenOptions::new().write(true).open(pidfile).is_err();
    }
    let Some(pid) = std::fs::read_to_string(pidfile).ok().and_then(|t| t.trim().parse::<u32>().ok()) else { return false };
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(&image.display().to_string()))
}
