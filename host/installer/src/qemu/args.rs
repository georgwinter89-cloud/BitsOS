//! The QEMU command and the files that start it: a shell script on Linux, a `.cmd` on Windows, and the desktop
//! entry. The device layout is xtask's (`host/xtask/src/qemu.rs`): q35, split UEFI flash, the image as a USB stick
//! on an xHCI controller with boot index 0, plus a USB keyboard and tablet on that bus (BitsOS has no PS/2 driver).

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accel {
    Kvm,
    /// The Windows Hypervisor Platform, with TCG behind it should it refuse at start.
    Whpx,
    Tcg,
}

/// The MCP service's port inside BitsOS.
pub const GUEST_MCP_PORT: u16 = 8765;

#[derive(Clone, Debug)]
pub struct Vm {
    pub os: Os,
    pub qemu: PathBuf,
    pub code: PathBuf,
    pub vars: PathBuf,
    pub image: PathBuf,
    pub pidfile: PathBuf,
    pub mem_mb: u64,
    pub cpus: u32,
    pub accel: Accel,
    pub display: String,
    /// The host port on 127.0.0.1 forwarded to the MCP service (remote control on).
    pub mcp_port: Option<u16>,
}

/// QEMU's option syntax ends a value at a comma; a doubled comma is a literal one.
fn opt_path(p: &Path) -> String {
    p.display().to_string().replace(',', ",,")
}

/// The accelerator's words. The Windows script always names WHPX with TCG behind it: QEMU falls back by itself, so
/// a Hypervisor Platform turned on later is used from the next start.
pub fn accel_words(a: Accel) -> &'static [&'static str] {
    match a {
        Accel::Kvm => &["-accel", "kvm", "-cpu", "host"],
        // WHPX passes the CPU model through unfiltered, so "max" would claim AVX-512 and more the host may lack; Haswell
        // without TSX is x86-64-v3, all BitsOS needs, and TCG emulates it as well.
        Accel::Whpx => &["-accel", "whpx,kernel-irqchip=off", "-accel", "tcg", "-cpu", "Haswell-v2"],
        Accel::Tcg => &["-accel", "tcg", "-cpu", "max"],
    }
}

/// The machine and its devices: everything but the accelerator, the serial port and the display.
fn devices(vm: &Vm) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut add = |xs: &[&str]| v.extend(xs.iter().map(|x| x.to_string()));
    add(&["-m", &format!("{}M", vm.mem_mb), "-smp", &vm.cpus.to_string()]);
    add(&["-global", "ICH9-LPC.disable_s3=1", "-rtc", "base=localtime"]);
    add(&["-drive", &format!("if=pflash,format=raw,readonly=on,file={}", opt_path(&vm.code))]);
    add(&["-drive", &format!("if=pflash,format=raw,file={}", opt_path(&vm.vars))]);
    add(&["-device", "qemu-xhci,id=xhci"]);
    add(&["-drive", &format!("if=none,id=stick,format=raw,file={}", opt_path(&vm.image))]);
    add(&["-device", "usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0"]);
    add(&["-device", "usb-kbd,bus=xhci.0", "-device", "usb-tablet,bus=xhci.0"]);
    let mut nd = "user,id=n0".to_string();
    if let Some(p) = vm.mcp_port {
        nd.push_str(&format!(",hostfwd=tcp:127.0.0.1:{p}-:{GUEST_MCP_PORT}"));
    }
    add(&["-netdev", &nd, "-device", "virtio-net-pci,netdev=n0"]);
    add(&["-vga", "std", "-monitor", "none"]);
    add(&["-pidfile", &vm.pidfile.display().to_string()]);
    v
}

const MACHINE: [&str; 4] = ["-name", "BitsOS", "-machine", "q35"];

/// The argument list without the serial port and the display (the launcher lets a test choose both).
pub fn args(vm: &Vm) -> Vec<String> {
    MACHINE.iter().chain(accel_words(vm.accel)).map(|s| s.to_string()).chain(devices(vm)).collect()
}

/// The whole command as the launcher runs it by default (for --dry-run and the person reading along).
pub fn full(vm: &Vm, log: &Path) -> Vec<String> {
    let mut v = vec![vm.qemu.display().to_string()];
    v.extend(args(vm));
    v.extend(["-display".to_string(), vm.display.clone(), "-serial".to_string(), format!("file:{}", log.display())]);
    v
}

/// A word for `sh`: single-quoted unless it is plain.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Read by the launcher: a test (or a person filing a bug) puts the serial log elsewhere or runs without a window.
pub const ENV_LOG: &str = "BITSOS_SERIAL_LOG";
pub const ENV_DISPLAY: &str = "BITSOS_QEMU_DISPLAY";

/// The words with one option and its value per line.
fn lines(words: Vec<String>, cont: &str) -> String {
    let mut s = String::new();
    for (i, w) in words.into_iter().enumerate() {
        if i > 0 {
            s.push_str(if w.starts_with('-') { cont } else { " " });
        }
        s.push_str(&w);
    }
    s
}

/// The Linux launcher. KVM is chosen at each start, so joining the group kvm (or a lost /dev/kvm) needs no new run
/// of the installer.
pub fn shell_script(vm: &Vm, log: &Path, version: &str) -> String {
    let words: Vec<String> = std::iter::once(vm.qemu.display().to_string()).chain(MACHINE.iter().map(|s| s.to_string())).chain(devices(vm)).map(|w| sh_quote(&w)).collect();
    format!(
        "#!/bin/sh\n\
         # BitsOS in a window (QEMU), written by bitsos-install {version} --qemu. Running bitsos-install --qemu again\n\
         # adds a newer release to the image and rewrites this file.\n\
         # {ENV_LOG}=<file> writes the serial console to <file> (default: next to the image);\n\
         # {ENV_DISPLAY}=none runs without a window. Arguments are passed on to QEMU.\n\
         log=\"${{{ENV_LOG}:-}}\"\n\
         [ -n \"$log\" ] || log={}\n\
         display=\"${{{ENV_DISPLAY}:-}}\"\n\
         [ -n \"$display\" ] || display={}\n\
         if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then\n\
         \x20   set -- {} \"$@\"\n\
         else\n\
         \x20   echo \"start-bitsos: /dev/kvm is missing or not yours (group kvm): QEMU emulates the processor, BitsOS is slow\" >&2\n\
         \x20   set -- {} \"$@\"\n\
         fi\n\
         exec {} \\\n    -display \"$display\" -serial \"file:$log\" \"$@\"\n",
        sh_quote(&log.display().to_string()),
        sh_quote(&vm.display),
        accel_words(Accel::Kvm).join(" "),
        accel_words(Accel::Tcg).join(" "),
        lines(words, " \\\n    ")
    )
}

/// `w` for cmd.exe with the image folder (`dir` and a backslash) as `%~dp0`: cmd fills that in itself, so a folder
/// with letters outside the console's code page (`C:\\Users\\Jörg`) reaches QEMU unchanged. Returns the text and
/// whether it uses `%~dp0`.
fn cmd_text(w: &str, dir: &str) -> Result<(String, bool), String> {
    if w.contains('"') {
        return Err(format!("{w}: a double quote cannot be passed through a .cmd file"));
    }
    let esc = |s: &str| s.replace('%', "%%");
    Ok(match w.find(dir) {
        Some(i) if !dir.is_empty() => (format!("{}%~dp0{}", esc(&w[..i]), esc(&w[i + dir.len()..])), true),
        _ => (esc(w), false),
    })
}

/// A word for `cmd.exe`: `%` doubled, quoted when it holds a space or one of cmd's operators, or the folder.
pub fn cmd_word(w: &str, dir: &str) -> Result<String, String> {
    let (t, folder) = cmd_text(w, dir)?;
    // A folder cmd fills in may hold `&` or `^`: inside quotes they stay letters.
    Ok(if folder || t.is_empty() || t.contains([' ', '\t', '&', '|', '<', '>', '^', '(', ')']) { format!("\"{t}\"") } else { t })
}

/// The Windows launcher. QEMU's messages go to `qemu-output.log` beside it; the shortcut starts it minimised, so a
/// failure opens that file in Notepad instead of waiting in a console nobody sees.
pub fn cmd_script(vm: &Vm, log: &Path, version: &str) -> Result<String, String> {
    // A comma in the folder would have to be doubled inside QEMU's options, which `%~dp0` cannot do.
    // Split as text: the tests build Windows paths on Linux too.
    let image = vm.image.display().to_string();
    let dir = image.rfind(['\\', '/']).map(|i| image[..=i].to_string()).filter(|d| !d.contains(',')).unwrap_or_default();
    let words: Vec<String> = std::iter::once(vm.qemu.display().to_string())
        .chain(MACHINE.iter().chain(accel_words(Accel::Whpx)).map(|s| s.to_string()))
        .chain(devices(vm))
        .map(|w| cmd_word(&w, &dir))
        .collect::<Result<_, _>>()?;
    let (log, _) = cmd_text(&log.display().to_string(), &dir)?;
    let text = format!(
        "@echo off\r\n\
         rem BitsOS in a window (QEMU), written by bitsos-install {version} --qemu. Running bitsos-install --qemu again\r\n\
         rem adds a newer release to the image and rewrites this file.\r\n\
         rem Set {ENV_LOG} to a file name to have the serial console there; {ENV_DISPLAY}=none runs without a window.\r\n\
         setlocal\r\n\
         if not defined {ENV_LOG} set \"{ENV_LOG}={log}\"\r\n\
         if not defined {ENV_DISPLAY} set \"{ENV_DISPLAY}={}\"\r\n\
         {} ^\r\n  -display %{ENV_DISPLAY}% -serial \"file:%{ENV_LOG}%\" %* 1>\"%~dp0qemu-output.log\" 2>&1\r\n\
         if errorlevel 1 start \"\" \"%SystemRoot%\\System32\\notepad.exe\" \"%~dp0qemu-output.log\"\r\n",
        vm.display,
        lines(words, " ^\r\n  ")
    );
    Ok(utf8_cmd(text))
}

/// cmd.exe reads a batch file in the console's code page; one that holds other letters switches to UTF-8 first.
pub fn utf8_cmd(text: String) -> String {
    match text.strip_prefix("@echo off\r\n") {
        Some(rest) if !text.is_ascii() => format!("@echo off\r\nchcp 65001 >nul\r\n{rest}"),
        _ => text,
    }
}

/// A `.cmd` that hands over to the start script (the shortcut when PowerShell could not make one). Without `call`:
/// that would expand `%` a second time, and nothing follows the script anyway.
pub fn forward_cmd(script: &str) -> String {
    utf8_cmd(format!("@echo off\r\n\"{}\" %*\r\n", script.replace('%', "%%")))
}

/// A desktop entry's Exec value: the program in double quotes with `"`, `` ` ``, `$` and `\` escaped, `%` doubled.
pub fn desktop_exec(p: &Path) -> String {
    let mut s = String::from("\"");
    for c in p.display().to_string().chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                s.push('\\');
                s.push(c);
            }
            '%' => s.push_str("%%"),
            _ => s.push(c),
        }
    }
    s.push('"');
    s
}

pub fn desktop_entry(script: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=BitsOS\n\
         Comment=BitsOS in a window (QEMU)\n\
         Exec={}\n\
         Icon=computer\n\
         Terminal=false\n\
         Categories=System;Emulator;\n",
        desktop_exec(script)
    )
}

/// The host port of a launcher's MCP forward (an update keeps the port the person paired with).
pub fn port_in(script: &str) -> Option<u16> {
    let i = script.find("hostfwd=tcp:127.0.0.1:")? + "hostfwd=tcp:127.0.0.1:".len();
    script[i..].split('-').next()?.parse().ok()
}
