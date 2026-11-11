//! QEMU mode on fake trees: the distribution and its install command, the firmware, the QEMU command for Linux and
//! Windows, the launchers (the shell script run against a stand-in QEMU), and what `--uninstall` would remove.

use super::args::{self, Accel, Os, Vm};
use super::files::{self, Layout};
use super::host::{self, Kvm, Pm};
use super::win;
use crate::tests::Scratch;
use std::path::{Path, PathBuf};

fn put(root: &Path, rel: &str, text: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

#[test]
fn the_distribution_names_its_install_command() {
    let pm = |t: &str| host::package_manager(&host::parse_os_release(t));
    assert_eq!(pm("NAME=\"CachyOS Linux\"\nID=cachyos\nID_LIKE=arch\n"), Some(Pm::Pacman));
    assert_eq!(pm("ID=arch\n"), Some(Pm::Pacman));
    assert_eq!(pm("ID=ubuntu\nID_LIKE=debian\n"), Some(Pm::Apt));
    assert_eq!(pm("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"), Some(Pm::Apt));
    assert_eq!(pm("ID=debian\n"), Some(Pm::Apt));
    assert_eq!(pm("ID=fedora\n"), Some(Pm::Dnf));
    assert_eq!(pm("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"), Some(Pm::Dnf));
    assert_eq!(pm("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"), Some(Pm::Zypper));
    assert_eq!(pm("ID=nixos\n"), None, "no guess for a system it does not know");
    assert_eq!(pm(""), None);
    assert_eq!(Pm::Pacman.command_line(), "sudo /usr/bin/pacman -S --needed --noconfirm qemu-desktop edk2-ovmf");
    assert_eq!(Pm::Apt.command_line(), "sudo /usr/bin/apt-get update && sudo /usr/bin/apt-get install -y qemu-system-x86 qemu-system-gui ovmf", "a fresh system's package lists first");
    assert!(Pm::Pacman.refresh_args().is_none() && Pm::Pacman.failure_hint().contains("pacman -Syu"), "no partial upgrade, but the hint");
    assert_eq!(Pm::Dnf.command_line(), "sudo /usr/bin/dnf install -y qemu-system-x86 qemu-ui-gtk edk2-ovmf");
    assert_eq!(Pm::Zypper.command_line(), "sudo /usr/bin/zypper --non-interactive install qemu-x86 qemu-ui-gtk qemu-ovmf-x86_64");
    let s = Scratch::new("qemu-osr");
    put(&s.0, "usr/lib/os-release", "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nID=debian\n");
    let d = host::distro(&s.0);
    assert_eq!((d.id.as_str(), d.name.as_str()), ("debian", "Debian GNU/Linux 13 (trixie)"), "/usr/lib/os-release when /etc has none");
}

const DESC: &str = r#"{
  "description": "x64 UEFI", "interface-types": ["uefi"],
  "mapping": {"device": "flash", "executable": {"filename": "CODE", "format": "raw"}, "nvram-template": {"filename": "VARS", "format": "raw"}},
  "targets": [{"architecture": "x86_64", "machines": ["pc-i440fx-*", "pc-q35-*"]}],
  "features": FEATURES, "tags": []
}"#;

fn desc(code: &str, vars: &str, features: &str) -> String {
    DESC.replace("CODE", code).replace("VARS", vars).replace("FEATURES", features)
}

#[test]
fn firmware_comes_from_qemus_descriptors_without_secure_boot() {
    let s = Scratch::new("qemu-fw-arch");
    let r = &s.0;
    for f in ["OVMF_CODE.4m.fd", "OVMF_VARS.4m.fd", "OVMF_CODE.secboot.4m.fd", "MICROVM.4m.fd"] {
        put(r, &format!("usr/share/edk2/x64/{f}"), "fw");
    }
    let d = "usr/share/qemu/firmware";
    put(r, &format!("{d}/50-edk2-ovmf-x86_64-secure-4m.json"), &desc("/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd", r#"["requires-smm", "secure-boot"]"#));
    put(r, &format!("{d}/55-microvm.json"), &desc("/usr/share/edk2/x64/MICROVM.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd", "[]").replace(r#"["pc-i440fx-*", "pc-q35-*"]"#, r#"["microvm"]"#));
    // Fedora's and Debian's plain builds name every confidential-computing mode they support.
    put(r, &format!("{d}/60-edk2-ovmf-x86_64-4m.json"), &desc("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd", r#"["acpi-s3", "amd-sev", "amd-sev-es", "amd-sev-snp", "intel-tdx", "verbose-dynamic"]"#));
    let fw = host::find_firmware(r).unwrap();
    assert_eq!(fw.code, r.join("usr/share/edk2/x64/OVMF_CODE.4m.fd"), "neither Secure Boot nor microvm, and SEV support is no reason to skip");
    assert_eq!(fw.vars, r.join("usr/share/edk2/x64/OVMF_VARS.4m.fd"));
    // The administrator's descriptor of the same name wins over the distribution's.
    put(r, "opt/fw/CODE.fd", "fw");
    put(r, "opt/fw/VARS.fd", "fw");
    put(r, "etc/qemu/firmware/60-edk2-ovmf-x86_64-4m.json", &desc("/opt/fw/CODE.fd", "/opt/fw/VARS.fd", "[]"));
    assert_eq!(host::find_firmware(r).unwrap().code, r.join("opt/fw/CODE.fd"));
}

#[test]
fn firmware_without_descriptors_is_found_where_distributions_put_it() {
    let s = Scratch::new("qemu-fw-known");
    let r = &s.0;
    assert_eq!(host::find_firmware(r), None, "nothing there, nothing guessed");
    // A descriptor whose files are missing does not count.
    put(r, "usr/share/qemu/firmware/60-gone.json", &desc("/usr/share/gone/CODE.fd", "/usr/share/gone/VARS.fd", "[]"));
    put(r, "usr/share/OVMF/OVMF_CODE_4M.fd", "fw");
    assert_eq!(host::find_firmware(r), None, "code without its variables template");
    put(r, "usr/share/OVMF/OVMF_VARS_4M.fd", "fw");
    let fw = host::find_firmware(r).unwrap();
    assert_eq!((fw.code, fw.vars), (r.join("usr/share/OVMF/OVMF_CODE_4M.fd"), r.join("usr/share/OVMF/OVMF_VARS_4M.fd")), "Debian and Ubuntu");
    let f = Scratch::new("qemu-fw-fedora");
    put(&f.0, "usr/share/edk2/ovmf/OVMF_CODE.fd", "fw");
    put(&f.0, "usr/share/edk2/ovmf/OVMF_VARS.fd", "fw");
    assert_eq!(host::find_firmware(&f.0).unwrap().code, f.0.join("usr/share/edk2/ovmf/OVMF_CODE.fd"), "Fedora");
}

#[test]
fn qemu_kvm_window_and_sizes() {
    let s = Scratch::new("qemu-bin");
    let r = &s.0;
    assert_eq!(host::find_qemu(r, Some("/opt/q/bin:/usr/bin".as_ref())), None);
    put(r, "usr/bin/qemu-system-x86_64", "");
    assert_eq!(host::find_qemu(r, None), Some(r.join("usr/bin/qemu-system-x86_64")));
    put(r, "opt/q/bin/qemu-system-x86_64", "");
    assert_eq!(host::find_qemu(r, Some("/opt/q/bin:/usr/bin".as_ref())), Some(r.join("opt/q/bin/qemu-system-x86_64")), "PATH first");
    assert_eq!(host::kvm(r), Kvm::Missing);
    put(r, "dev/kvm", "");
    assert_eq!(host::kvm(r), Kvm::Usable);
    assert!(Kvm::NoAccess.note().unwrap().contains("group kvm") && Kvm::Missing.note().is_some() && Kvm::Usable.note().is_none());
    assert_eq!(host::pick_display("Available display backend types:\nnone\ngtk\nsdl\n"), Some("gtk"));
    assert_eq!(host::pick_display("Available display backend types:\nnone\nsdl\negl-headless\n"), Some("sdl"));
    assert_eq!(host::pick_display("Available display backend types:\nnone\nvnc\n"), None);
    assert_eq!(host::mem_mb(Some(46 << 30)), 4096);
    assert_eq!(host::mem_mb(Some(6 << 30)), 3072, "half of a host under 8 GiB");
    assert_eq!(host::mem_mb(Some(3 << 30)), 2048, "never below 2 GiB");
    assert_eq!(host::mem_mb(None), 4096);
    assert_eq!((host::cpus(20), host::cpus(4), host::cpus(2), host::cpus(1)), (4, 3, 1, 1));
    assert_eq!(host::meminfo_bytes("MemTotal:       48000000 kB\nMemFree: 1 kB\n"), Some(48_000_000 * 1024));
    assert_eq!(host::euid("Name:\tx\nUid:\t1000\t0\t0\t0\n"), Some(0), "the effective id is the second");
}

fn vm(os: Os, dir: &str, accel: Accel, port: Option<u16>) -> Vm {
    let p = |n: &str| PathBuf::from(if os == Os::Windows { format!("{dir}\\{n}") } else { format!("{dir}/{n}") });
    Vm {
        os,
        qemu: if os == Os::Windows { "C:\\Program Files\\qemu\\qemu-system-x86_64.exe".into() } else { "/usr/bin/qemu-system-x86_64".into() },
        code: if os == Os::Windows { "C:\\Program Files\\qemu\\share\\edk2-x86_64-code.fd".into() } else { "/usr/share/edk2/x64/OVMF_CODE.4m.fd".into() },
        vars: p("bitsos-vars.fd"),
        image: p("bitsos.img"),
        pidfile: p("qemu.pid"),
        mem_mb: 4096,
        cpus: 4,
        accel,
        display: "gtk,zoom-to-fit=on".into(),
        mcp_port: port,
    }
}

#[test]
fn the_qemu_command_on_linux_has_xtasks_devices() {
    let a = args::args(&vm(Os::Linux, "/home/u/.local/share/bitsos", Accel::Kvm, None));
    let want = "-name BitsOS -machine q35 -accel kvm -cpu host -m 4096M -smp 4 -global ICH9-LPC.disable_s3=1 -rtc base=localtime \
        -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
        -drive if=pflash,format=raw,file=/home/u/.local/share/bitsos/bitsos-vars.fd -device qemu-xhci,id=xhci \
        -drive if=none,id=stick,format=raw,file=/home/u/.local/share/bitsos/bitsos.img \
        -device usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0 -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 \
        -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -vga std -monitor none -pidfile /home/u/.local/share/bitsos/qemu.pid";
    assert_eq!(a.join(" "), want.split_whitespace().collect::<Vec<_>>().join(" "));
    let t = args::args(&vm(Os::Linux, "/d", Accel::Tcg, Some(18765)));
    assert!(t.join(" ").contains("-accel tcg -cpu max") && t.contains(&"user,id=n0,hostfwd=tcp:127.0.0.1:18765-:8765".to_string()));
    // A comma in a path is QEMU's option separator: doubled, it stays part of the file name.
    let c = args::args(&vm(Os::Linux, "/d/a,b", Accel::Kvm, None));
    assert!(c.contains(&"if=none,id=stick,format=raw,file=/d/a,,b/bitsos.img".to_string()));
    let f = args::full(&vm(Os::Linux, "/d", Accel::Kvm, None), Path::new("/d/log"));
    assert_eq!(f[0], "/usr/bin/qemu-system-x86_64");
    assert_eq!(&f[f.len() - 4..], ["-display", "gtk,zoom-to-fit=on", "-serial", "file:/d/log"]);
}

#[test]
fn the_qemu_command_on_windows() {
    let a = args::args(&vm(Os::Windows, "C:\\Users\\Ann\\AppData\\Local\\BitsOS", Accel::Whpx, Some(18765))).join(" ");
    assert!(a.contains("-machine q35 -accel whpx,kernel-irqchip=off -accel tcg -cpu Haswell-v2 -m 4096M"), "{a}");
    assert!(a.contains("-drive if=pflash,format=raw,readonly=on,file=C:\\Program Files\\qemu\\share\\edk2-x86_64-code.fd"));
    assert!(a.contains("-drive if=none,id=stick,format=raw,file=C:\\Users\\Ann\\AppData\\Local\\BitsOS\\bitsos.img -device usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0"));
    assert!(a.contains("hostfwd=tcp:127.0.0.1:18765-:8765"));
    let t = args::args(&vm(Os::Windows, "C:\\x", Accel::Tcg, None)).join(" ");
    assert!(t.contains("-accel tcg -cpu max") && !t.contains("whpx") && !t.contains("hostfwd"));
}

#[cfg(unix)]
#[test]
fn the_shell_launcher_runs_exactly_that_command() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("qemu-sh");
    let dir = s.0.join("it's a dir");
    std::fs::create_dir_all(&dir).unwrap();
    // A stand-in QEMU that writes the arguments it got, one per line.
    let fake = put(&dir, "qemu $x", "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$(dirname \"$0\")/argv\"\n");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut v = vm(Os::Linux, &dir.display().to_string(), Accel::Kvm, Some(18766));
    v.qemu = fake;
    let log = dir.join("bitsos-serial.log");
    let text = args::shell_script(&v, &log, "0.9.0");
    assert!(text.starts_with("#!/bin/sh\n") && text.contains("bitsos-install 0.9.0 --qemu"));
    assert!(text.contains("if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then\n    set -- -accel kvm -cpu host \"$@\"\nelse\n") && text.contains("set -- -accel tcg -cpu max \"$@\"\nfi\n"), "{text}");
    assert_eq!(args::port_in(&text), Some(18766), "an update reads the port back");
    let script = put(&dir, "start-bitsos.sh", &text);
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run = |envs: &[(&str, &str)], extra: &[&str]| {
        let mut c = std::process::Command::new(&script);
        c.args(extra).env_remove(args::ENV_LOG).env_remove(args::ENV_DISPLAY);
        for (k, val) in envs {
            c.env(k, val);
        }
        assert!(c.status().unwrap().success());
        std::fs::read_to_string(dir.join("argv")).unwrap().lines().map(String::from).collect::<Vec<_>>()
    };
    // The accelerator is chosen when the script starts, by whether this user may open /dev/kvm now.
    let accel = if host::kvm(Path::new("/")) == Kvm::Usable { Accel::Kvm } else { Accel::Tcg };
    v.accel = Accel::Tcg;
    let a = args::args(&v);
    let mut want: Vec<String> = a[..4].iter().chain(&a[8..]).cloned().collect();
    want.extend(["-display", "gtk,zoom-to-fit=on", "-serial", &format!("file:{}", log.display())].map(String::from));
    let n = want.len();
    want.extend(args::accel_words(accel).iter().map(|s| s.to_string()));
    assert_eq!(run(&[], &[]), want);
    want[n - 3] = "none".into();
    want[n - 1] = "file:/tmp/test serial.log".into();
    want.push("-S".into());
    assert_eq!(run(&[(args::ENV_DISPLAY, "none"), (args::ENV_LOG, "/tmp/test serial.log")], &["-S"]), want, "the test's log and display, extra arguments passed on");
}

#[test]
fn the_cmd_launcher_quotes_for_cmd() {
    let dir = "C:\\Users\\Ann 100%\\AppData\\Local\\BitsOS";
    let v = vm(Os::Windows, dir, Accel::Tcg, None);
    let t = args::cmd_script(&v, Path::new(&format!("{dir}\\bitsos-serial.log")), "0.9.0").unwrap();
    assert!(t.starts_with("@echo off\r\nrem ") && t.is_ascii() && !t.replace("\r\n", "").contains('\n'), "CRLF only, no code page switch for ASCII");
    assert!(t.contains("\r\n\"C:\\Program Files\\qemu\\qemu-system-x86_64.exe\" ^\r\n  -name BitsOS ^\r\n  -machine q35 ^\r\n  -accel whpx,kernel-irqchip=off ^\r\n  -accel tcg ^\r\n  -cpu Haswell-v2 ^\r\n"), "WHPX whatever the install found: {t}");
    assert!(t.contains("\"if=none,id=stick,format=raw,file=%~dp0bitsos.img\""), "the folder is cmd's own %~dp0: {t}");
    assert!(t.contains("\"if=pflash,format=raw,file=%~dp0bitsos-vars.fd\"") && t.contains("-pidfile \"%~dp0qemu.pid\""));
    assert!(!t.contains("Ann"), "the folder's name is nowhere in the file");
    assert!(t.contains("set \"BITSOS_SERIAL_LOG=%~dp0bitsos-serial.log\""));
    assert!(!t.lines().any(|l| l.starts_with("rem") && l.contains(['<', '>', '|', '&'])), "no operator in a rem line");
    assert!(t.contains("-display %BITSOS_QEMU_DISPLAY% -serial \"file:%BITSOS_SERIAL_LOG%\" %* 1>\"%~dp0qemu-output.log\" 2>&1\r\n"));
    assert!(t.contains("if errorlevel 1 start \"\" \"%SystemRoot%\\System32\\notepad.exe\" \"%~dp0qemu-output.log\"\r\n") && !t.contains("pause"), "an error is shown, not waited on unseen");
    // A QEMU outside the folder with letters beyond ASCII: the file switches cmd to UTF-8 before it.
    let mut u = vm(Os::Windows, "C:\\Users\\Jörg\\AppData\\Local\\BitsOS", Accel::Whpx, None);
    u.qemu = "D:\\Werkzeuge für QEMU\\qemu-system-x86_64.exe".into();
    let t = args::cmd_script(&u, Path::new("C:\\Users\\Jörg\\AppData\\Local\\BitsOS\\bitsos-serial.log"), "0.9.0").unwrap();
    assert!(t.starts_with("@echo off\r\nchcp 65001 >nul\r\nrem ") && !t.contains("Jörg") && t.contains("\"D:\\Werkzeuge für QEMU\\qemu-system-x86_64.exe\""), "{t}");
    // A comma in the folder stays written out, doubled for QEMU.
    let c = args::cmd_script(&vm(Os::Windows, "C:\\a,b", Accel::Whpx, None), Path::new("C:\\a,b\\bitsos-serial.log"), "0.9.0").unwrap();
    assert!(c.contains("file=C:\\a,,b\\bitsos.img") && !c.contains("%~dp0bitsos.img"), "{c}");
    assert_eq!(args::cmd_word("a&b", "").unwrap(), "\"a&b\"");
    assert_eq!(args::cmd_word("x=C:\\d&e\\f", "C:\\d&e\\").unwrap(), "\"x=%~dp0f\"");
    assert!(args::cmd_word("a\"b", "").is_err());
    assert_eq!(args::forward_cmd("C:\\Jörg 100%\\s.cmd"), "@echo off\r\nchcp 65001 >nul\r\n\"C:\\Jörg 100%%\\s.cmd\" %*\r\n", "UTF-8, % doubled once, no call");
    assert_eq!(args::forward_cmd("C:\\x\\s.cmd"), "@echo off\r\n\"C:\\x\\s.cmd\" %*\r\n");
}

#[test]
fn desktop_entry_and_windows_texts() {
    let e = args::desktop_entry(Path::new("/home/a b/$x/100%/start-bitsos.sh"));
    assert!(e.starts_with("[Desktop Entry]\nType=Application\nName=BitsOS\n"));
    assert!(e.contains("\nExec=\"/home/a b/\\$x/100%%/start-bitsos.sh\"\n"), "{e}");
    let ps = win::shortcut_script("C:\\Users\\O'Neil\\BitsOS\\start-bitsos.cmd", "C:\\Users\\O'Neil\\BitsOS", "C:\\Program Files\\qemu\\qemu-system-x86_64.exe");
    assert!(ps.contains("$s.TargetPath = 'C:\\Users\\O''Neil\\BitsOS\\start-bitsos.cmd'"), "{ps}");
    assert!(ps.contains("$s.Description = 'BitsOS in a window (QEMU): C:\\Users\\O''Neil\\BitsOS\\start-bitsos.cmd'"), "{ps}");
    // PowerShell ends a single-quoted string at typographic quotes too.
    assert_eq!(win::ps_quote("O\u{2019}Neil \u{2018}a\u{201A}b\u{201B}"), "'O\u{2019}\u{2019}Neil \u{2018}\u{2018}a\u{201A}\u{201A}b\u{201B}\u{201B}'");
    assert!(win::installer_script("C:\\Users\\O\u{2019}Neil\\x.exe").contains("-FilePath 'C:\\Users\\O\u{2019}\u{2019}Neil\\x.exe'"));
    assert!(ps.contains("GetFolderPath('Desktop')") && ps.contains("GetFolderPath('Programs')") && ps.contains("'BitsOS.lnk'"));
    assert!(ps.contains("$s.IconLocation = 'C:\\Program Files\\qemu\\qemu-system-x86_64.exe,0'"));
    assert!(win::installer_script("C:\\c\\qemu-w64-setup-1.exe").contains("-FilePath 'C:\\c\\qemu-w64-setup-1.exe' -ArgumentList '/S' -Verb RunAs -Wait"));
    let reg = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\QEMU\r\n    Install_Dir    REG_SZ    D:\\Tools\\qemu\r\n\r\n";
    assert_eq!(win::parse_reg_install_dir(reg).as_deref(), Some("D:\\Tools\\qemu"));
    assert_eq!(win::parse_reg_install_dir("ERROR: The system was unable to find the specified registry key or value."), None);
    let c = win::candidates(Some("C:\\Program Files\\qemu".into()), &[Some("C:\\Program Files".into()), Some("C:\\Program Files (x86)".into()), None]);
    assert_eq!(c, ["C:\\Program Files\\qemu", "C:\\Program Files (x86)\\qemu"]);
    let i = win::install_in("C:\\Program Files\\qemu\\");
    assert_eq!(i.vars, PathBuf::from("C:\\Program Files\\qemu\\share\\edk2-i386-vars.fd"));
    let p = &win::QEMU_FOR_WINDOWS;
    assert!(p.url.starts_with("https://qemu.weilnetz.de/w64/") && p.file_name() == format!("qemu-w64-setup-{}.exe", p.version) && p.url.contains(&format!("/{}/", &p.version[..4])));
    assert!(p.sha512.len() == 128 && p.sha512.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && p.size > 100 << 20);
}

#[test]
fn uninstall_removes_only_its_own_files() {
    let s = Scratch::new("qemu-un");
    let l = Layout { dir: s.0.join("bitsos"), os: Os::Linux };
    std::fs::create_dir_all(&l.dir).unwrap();
    for p in [l.image(), l.vars(), l.script(), l.token(), l.log()] {
        std::fs::write(&p, "x").unwrap();
        l.remember(&p).unwrap();
    }
    let entry = put(&s.0, "apps/bitsos.desktop", &args::desktop_entry(&l.script()));
    let taken_over = put(&s.0, "desk2/bitsos.desktop", &args::desktop_entry(Path::new("/other/start-bitsos.sh")));
    let foreign = put(&s.0, "home/notes.txt", "keep");
    let dir_named = s.0.join("desk/bitsos.desktop");
    std::fs::create_dir_all(&dir_named).unwrap();
    let mine_elsewhere = put(&s.0, "other/start-bitsos.sh", "not recorded");
    // Shortcuts: Windows keeps the description (naming the script) as UTF-16.
    let lnk = |target: &str| [b"L\0\0\0".to_vec(), win::shortcut_description(target).encode_utf16().flat_map(u16::to_le_bytes).collect()].concat();
    let my_lnk = s.0.join("desk/BitsOS.lnk");
    std::fs::write(&my_lnk, lnk(&l.script().display().to_string())).unwrap();
    let other_lnk = s.0.join("start/BitsOS.lnk");
    std::fs::create_dir_all(other_lnk.parent().unwrap()).unwrap();
    std::fs::write(&other_lnk, lnk(&format!("{}2/start-bitsos.sh", l.dir.display()))).unwrap();
    for p in [&entry, &taken_over, &foreign, &dir_named, &s.0.join("gone/BitsOS.lnk"), &my_lnk, &other_lnk] {
        l.remember(p).unwrap();
    }
    let r = l.to_remove();
    assert_eq!(r, [l.image(), l.vars(), l.script(), l.log(), l.token(), entry, my_lnk.clone(), l.record()], "own files, the recorded entry and shortcut, the record last");
    assert!(!r.contains(&other_lnk), "a shortcut that now starts another image folder's script");
    assert!(win::shortcut_names(b"..C:\\x\\start-bitsos.cmd..", "C:\\x\\start-bitsos.cmd") && !win::shortcut_names(b"..C:\\x2\\start-bitsos.cmd..", "C:\\x\\start-bitsos.cmd"));
    // A dry run lists and removes nothing, whatever --yes says.
    super::place::uninstall(&l, true, true).unwrap();
    assert!(r.iter().all(|p| p.exists()), "--uninstall --yes --dry-run removed something");
    assert!(!r.contains(&foreign) && !r.contains(&dir_named) && !r.contains(&mine_elsewhere), "not its name, a folder, or never recorded");
    assert!(!r.contains(&taken_over), "a launcher that now starts another image");
    let w = Layout { dir: "C:\\x".into(), os: Os::Windows };
    assert!(w.script().ends_with("start-bitsos.cmd"));
}

#[test]
fn folders_follow_the_desktops_conventions() {
    let s = Scratch::new("qemu-dirs");
    let home = s.0.join("home");
    let env = |m: Vec<(&'static str, String)>| move |k: &str| m.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let h = home.display().to_string();
    assert_eq!(files::default_dir(Os::Linux, &env(vec![("HOME", h.clone())])), Some(home.join(".local/share/bitsos")));
    assert_eq!(files::default_dir(Os::Linux, &env(vec![("HOME", h.clone()), ("XDG_DATA_HOME", "/x/data".into())])), Some(PathBuf::from("/x/data/bitsos")));
    assert_eq!(files::default_dir(Os::Linux, &env(vec![("HOME", h.clone()), ("XDG_DATA_HOME", "rel".into())])), Some(home.join(".local/share/bitsos")), "a relative XDG path is ignored");
    assert_eq!(files::default_dir(Os::Windows, &env(vec![("LOCALAPPDATA", "C:\\Users\\A\\AppData\\Local".into())])), Some(PathBuf::from("C:\\Users\\A\\AppData\\Local").join("BitsOS")));
    assert_eq!(files::default_dir(Os::Windows, &env(vec![])), None);
    // The desktop: user-dirs.dirs names it (German: Schreibtisch); only one that exists.
    put(&home, ".config/user-dirs.dirs", "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\n");
    assert_eq!(files::desktop_dir(&env(vec![("HOME", h.clone())])), None, "not created yet");
    std::fs::create_dir_all(home.join("Schreibtisch")).unwrap();
    std::fs::create_dir_all(home.join("Desktop")).unwrap();
    assert_eq!(files::desktop_dir(&env(vec![("HOME", h.clone())])), Some(home.join("Schreibtisch")));
    let elsewhere = s.0.join("d");
    std::fs::create_dir_all(&elsewhere).unwrap();
    assert_eq!(files::desktop_dir(&env(vec![("HOME", h.clone()), ("XDG_DESKTOP_DIR", elsewhere.display().to_string())])), Some(elsewhere));
    assert_eq!(files::desktop_dir(&env(vec![("HOME", h.clone()), ("XDG_CONFIG_HOME", s.0.join("none").display().to_string())])), Some(home.join("Desktop")));
    assert_eq!(files::desktop_dir(&env(vec![("HOME", h.clone()), ("XDG_DESKTOP_DIR", h.clone())])), None, "the home folder is no desktop");
}

#[test]
fn qemu_flags() {
    let p = |a: &str| crate::cli::Opts::parse(a.split_whitespace().map(String::from).collect()).map(|o| o.unwrap());
    let o = p("--qemu --remote no --start yes --size 20G --image-dir /x").unwrap();
    let q = o.qemu.clone().unwrap();
    assert!(o.batch() && q.start == Some(true) && q.size == Some(20 << 30) && q.image_dir == Some("/x".into()) && !q.install_qemu);
    assert!(!p("--qemu --remote no").unwrap().batch(), "--start missing: asked");
    assert!(p("--qemu --dry-run").unwrap().batch());
    assert!(p("--qemu --uninstall --yes").unwrap().batch() && !p("--qemu --uninstall").unwrap().batch());
    assert!(p("--qemu --install-qemu --remote yes --start no").unwrap().qemu.unwrap().install_qemu);
    for bad in ["--start yes", "--install-qemu", "--uninstall", "--qemu --erase sdb-1G", "--qemu --cable-only", "--qemu --image a.img", "--qemu --yes", "--qemu --size 4G", "--qemu --start maybe", "--qemu --list"] {
        assert!(p(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_qemu_config_turns_the_desktop_on_and_nothing_guarded() {
    use crate::config::{Answers, guarded_in, render};
    let s = render(&Answers { qemu: true, screen: Some("1600x900".into()), ..Answers::default() }, "0.9.0").unwrap();
    assert!(s.contains("\ndesktop=on\n") && s.contains("\nscreen=1600x900\n") && s.contains("QEMU's virtio card"), "{s}");
    assert!(guarded_in(&s).is_empty());
    let t = render(&Answers { qemu: true, token: Some("0123456789abcdef".into()), ..Answers::default() }, "0.9.0").unwrap();
    assert_eq!(guarded_in(&t), ["mcp"]);
}

#[test]
fn a_full_disk_says_what_to_do() {
    let d = Path::new("/home/u/.local/share/bitsos");
    let e = super::image::full_disk("write BITSOS2/G1/boot.efi: No space left on device (os error 28)".into(), d);
    assert!(e.ends_with("The disk that holds /home/u/.local/share/bitsos is full: free some space there, or give another --image-dir."), "{e}");
    assert!(super::image::full_disk("x: There is not enough space on the disk. (os error 112)".into(), d).contains("is full"));
    assert_eq!(super::image::full_disk("x: permission denied (os error 13)".into(), d), "x: permission denied (os error 13)");
}
