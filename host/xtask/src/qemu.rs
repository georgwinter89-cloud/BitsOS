//! `xtask qemu`: assemble the command line, check it (refusals), run it with a timeout, map the exit code.
use crate::image::{self, ImageOpts};
use crate::refuse;
use crate::util::*;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OVMF_CODE: &str = "/usr/share/edk2/x64/OVMF_CODE.4m.fd";
const OVMF_VARS: &str = "/usr/share/edk2/x64/OVMF_VARS.4m.fd";

pub const EXIT_PASS: i32 = 33;
pub const EXIT_FAIL: i32 = 35;
pub const EXIT_PANIC: i32 = 37;

#[derive(Clone)]
pub struct QemuOpts {
    pub name: String,
    pub smp: u32,
    pub mem: String,
    pub net: String,
    pub hostfwd: Vec<(u16, u16)>,
    pub res: Option<(u32, u32)>,
    pub display: String,
    pub timeout: u64,
    pub serial_log: Option<PathBuf>,
    pub extra: Vec<String>,
    /// Echo the serial log to stdout while the guest runs.
    pub echo: bool,
    /// Extra boots when OVMF stalls before bitsboot runs (the usb-storage flake on the Gaming-PC).
    pub boot_retries: u32,
}

impl QemuOpts {
    pub fn new(name: &str) -> QemuOpts {
        QemuOpts {
            name: name.into(), smp: 2, mem: "2G".into(), net: "none".into(), hostfwd: vec![], res: None,
            display: "none".into(), timeout: 60, serial_log: None, extra: vec![], echo: true, boot_retries: 2,
        }
    }

    pub fn parse(a: &mut Args) -> Result<QemuOpts, String> {
        let mut o = QemuOpts::new(&a.value("--name")?.unwrap_or_else(|| "test".into()));
        if let Some(v) = a.num("--smp")? { o.smp = v; }
        if let Some(v) = a.value("--mem")? { o.mem = v; }
        if let Some(v) = a.value("--net")? { o.net = v; }
        for h in a.values("--hostfwd")? {
            let (hp, gp) = h.split_once(':').ok_or("--hostfwd wants HOSTPORT:GUESTPORT")?;
            o.hostfwd.push((hp.parse().map_err(|_| "bad host port")?, gp.parse().map_err(|_| "bad guest port")?));
        }
        if let Some(r) = a.value("--res")? {
            let (w, h) = r.split_once('x').ok_or("--res wants WxH")?;
            o.res = Some((w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?));
        }
        if let Some(v) = a.value("--display")? { o.display = v; }
        if let Some(v) = a.num("--timeout")? { o.timeout = v; }
        if let Some(v) = a.num("--boot-retries")? { o.boot_retries = v; }
        o.serial_log = a.value("--serial-log")?.map(PathBuf::from);
        o.extra = a.extra.clone();
        Ok(o)
    }
}

/// The complete argument list (without the program name).
pub fn assemble(root: &Path, lane: &str, o: &QemuOpts, vars: &Path, img: &Path, log: &Path) -> Result<Vec<String>, String> {
    let s = |x: &str| x.to_string();
    let mut v: Vec<String> = Vec::new();
    let mut add = |xs: &[&str]| v.extend(xs.iter().map(|x| s(x)));
    let kvm = Path::new("/dev/kvm").exists();
    add(&["-name", &format!("v2-{lane}-{}", o.name)]);
    if kvm {
        add(&["-machine", "q35,accel=kvm", "-cpu", "host"]);
    } else {
        eprintln!("xtask: warning: /dev/kvm missing, falling back to TCG (slow, -cpu max)");
        add(&["-machine", "q35,accel=tcg", "-cpu", "max"]);
    }
    add(&["-m", &o.mem, "-smp", &o.smp.to_string()]);
    add(&["-global", "ICH9-LPC.disable_s3=1", "-no-reboot"]);
    // V2 reads the RTC as local time (as Windows on Georg's machines sets it); QEMU's default is UTC (YT-4a CR 5).
    add(&["-rtc", "base=localtime"]);
    add(&["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    add(&["-drive", &format!("if=pflash,format=raw,readonly=on,file={OVMF_CODE}")]);
    add(&["-drive", &format!("if=pflash,format=raw,file={}", vars.display())]);
    add(&["-device", "qemu-xhci,id=xhci"]);
    add(&["-drive", &format!("if=none,id=stick,format=raw,file={}", img.display())]);
    add(&["-device", "usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0"]);
    add(&["-serial", &format!("file:{}", log.display())]);
    add(&["-monitor", "none"]);
    match o.display.as_str() {
        "none" => add(&["-display", "none"]),
        d => add(&["-display", d]),
    }
    match o.res {
        None => add(&["-vga", "std"]),
        Some((w, h)) => add(&["-vga", "none", "-device", &format!("VGA,xres={w},yres={h},vgamem_mb=64")]),
    }
    match o.net.as_str() {
        "none" => {
            if !o.hostfwd.is_empty() {
                return Err("--hostfwd needs --net user|igb|virtio".into());
            }
            add(&["-net", "none"]);
        }
        "user" | "igb" | "virtio" => {
            let mut nd = "user,id=n0".to_string();
            for (h, g) in &o.hostfwd {
                nd.push_str(&format!(",hostfwd=tcp:127.0.0.1:{h}-:{g}"));
            }
            add(&["-netdev", &nd]);
            add(&["-device", if o.net == "igb" { "igb,netdev=n0" } else { "virtio-net-pci,netdev=n0" }]);
        }
        other => return Err(format!("--net {other}: want none|user|igb|virtio")),
    }
    v.extend(o.extra.iter().cloned());
    let _ = root;
    Ok(v)
}

pub struct RunResult {
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub log: PathBuf,
}

/// Runs QEMU with `o`; the image `target/xtask/<name>.img` must exist.
pub fn run(root: &Path, o: &QemuOpts) -> Result<RunResult, String> {
    image::safe_name(&o.name)?;
    let lane = lane(root);
    let x = xdir(root);
    std::fs::create_dir_all(&x).map_err(|e| e.to_string())?;
    let img = x.join(format!("{}.img", o.name));
    if !img.is_file() {
        return Err(format!("{} does not exist; run `cargo xtask image --name {}` first", img.display(), o.name));
    }
    let log = match &o.serial_log {
        Some(p) => if p.is_absolute() { p.clone() } else { std::env::current_dir().unwrap_or_default().join(p) },
        None => x.join(format!("{}.log", o.name)),
    };
    let mut attempt = 0;
    loop {
        let r = run_once(root, o, &lane, &x, &img, &log)?;
        if !r.stalled || attempt >= o.boot_retries {
            if r.stalled {
                eprintln!("xtask: firmware stall on every try ({} boots): no `bitsboot:` line in {}", attempt + 1, log.display());
            }
            return Ok(RunResult { exit: r.exit, timed_out: r.timed_out, log });
        }
        attempt += 1;
        eprintln!("xtask: OVMF stalled before bitsboot (no `bitsboot:` line); boot retry {attempt} of {}", o.boot_retries);
    }
}

/// OVMF variables saved from a boot that reached bitsboot (see `run_once`).
const SEED_VARS: &str = "ovmf-vars-seed.fd";

/// The firmware never reached bitsboot within this time: a stall, not a kernel problem.
const STALL_SECS: u64 = 25;

struct Once {
    exit: Option<i32>,
    timed_out: bool,
    stalled: bool,
}

fn reached_bitsboot(log: &Path) -> bool {
    std::fs::read(log).map(|b| b.windows(9).any(|w| w == b"bitsboot:")).unwrap_or(false)
}

fn run_once(root: &Path, o: &QemuOpts, lane: &str, x: &Path, img: &Path, log: &Path) -> Result<Once, String> {
    let vars = x.join(format!("{}-vars.fd", o.name));
    // OVMF often hangs on its first boot with fresh variables (it builds its boot entries then); variables
    // saved after a boot that reached bitsboot avoid that (measured 15.10.: 8 of 8 boots instead of ~2 of 3).
    let seed = x.join(SEED_VARS);
    let from = if seed.is_file() { seed.clone() } else { PathBuf::from(OVMF_VARS) };
    std::fs::copy(&from, &vars).map_err(|e| format!("copy {}: {e}", from.display()))?;
    let _ = std::fs::remove_file(log);
    let args = assemble(root, lane, o, &vars, img, log)?;
    if let Err(r) = refuse::check_qemu_args(lane, root, &args) {
        return Err(format!("{r}\nxtask: QEMU was not started."));
    }
    let mut child = Command::new("qemu-system-x86_64")
        .args(&args)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start qemu-system-x86_64: {e}"))?;
    let pid = child.id();
    let pidfile = x.join(format!("qemu-{}.pid", o.name));
    let _ = std::fs::write(&pidfile, format!("{pid}\n"));
    eprintln!("xtask: qemu pid {pid} (lane {lane}, name v2-{lane}-{}), timeout {} s, log {}", o.name, o.timeout, log.display());

    let start = Instant::now();
    let mut echoed: u64 = 0;
    let mut timed_out = false;
    let mut seen_loader = false;
    let mut stalled = false;
    let status = loop {
        if o.echo {
            echo_new(log, &mut echoed);
        }
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            stalled = !reached_bitsboot(log);
            break Some(st);
        }
        if !seen_loader && start.elapsed() >= Duration::from_secs(STALL_SECS.min(o.timeout)) {
            seen_loader = reached_bitsboot(log);
            if !seen_loader {
                stalled = true;
                // Exactly the process we started.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
        if start.elapsed() >= Duration::from_secs(o.timeout) {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if o.echo {
        echo_new(log, &mut echoed);
    }
    let _ = std::fs::remove_file(&pidfile);
    if !stalled && !seed.is_file() && reached_bitsboot(log) {
        let tmp = x.join(format!("{SEED_VARS}.{}", std::process::id()));
        if std::fs::copy(&vars, &tmp).is_ok() {
            let _ = std::fs::rename(&tmp, &seed);
        }
    }
    Ok(Once { exit: status.and_then(|s| s.code()), timed_out: timed_out || (stalled && status.is_none()), stalled })
}

fn echo_new(log: &Path, pos: &mut u64) {
    let Ok(mut f) = std::fs::File::open(log) else { return };
    if f.seek(SeekFrom::Start(*pos)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
        *pos += buf.len() as u64;
        let _ = std::io::stdout().write_all(&buf);
        let _ = std::io::stdout().flush();
    }
}

/// isa-debug-exit: QEMU exits with (value << 1) | 1.
pub fn describe_exit(code: Option<i32>, timed_out: bool) -> (bool, String) {
    if timed_out {
        return (false, "timeout: QEMU was stopped by xtask".into());
    }
    match code {
        Some(EXIT_PASS) => (true, "guest reported PASS (isa-debug-exit 0x10)".into()),
        Some(EXIT_FAIL) => (false, "guest reported test FAILURE (isa-debug-exit 0x11)".into()),
        Some(EXIT_PANIC) => (false, "guest reported KERNEL PANIC (isa-debug-exit 0x12)".into()),
        Some(0) => (false, "QEMU exited with 0 (guest powered off without isa-debug-exit)".into()),
        Some(c) => (false, format!("QEMU exited with code {c}")),
        None => (false, "QEMU was killed by a signal".into()),
    }
}

pub fn cmd_qemu(root: &Path, mut a: Args) -> Result<(), String> {
    let o = QemuOpts::parse(&mut a)?;
    a.positionals()?;
    let img = xdir(root).join(format!("{}.img", o.name));
    if !img.is_file() {
        let io = ImageOpts { name: o.name.clone(), generation: 1, config_extra: vec![], build: Default::default() };
        image::make_image(root, &io)?;
    }
    let r = run(root, &o)?;
    let (ok, msg) = describe_exit(r.exit, r.timed_out);
    println!("xtask: {msg}");
    if ok { Ok(()) } else { Err(msg) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_map() {
        assert!(describe_exit(Some(33), false).0);
        assert!(!describe_exit(Some(35), false).0);
        assert!(describe_exit(Some(37), false).1.contains("PANIC"));
        assert!(describe_exit(None, true).1.contains("timeout"));
        assert!(!describe_exit(Some(1), false).0);
    }

    #[test]
    fn assembled_line_passes_the_refusals_and_has_the_fixed_parts() {
        let root = Path::new("/work/v2");
        let mut o = QemuOpts::new("t");
        o.net = "user".into();
        o.hostfwd = vec![(20001, 7777)];
        o.res = Some((2560, 1440));
        let x = root.join("target/xtask");
        let v = assemble(root, "desk", &o, &x.join("t-vars.fd"), &x.join("t.img"), &x.join("t.log")).unwrap();
        let j = v.join(" ");
        for need in ["-name v2-desk-t", "isa-debug-exit,iobase=0xf4,iosize=0x04", "disable_s3=1", "-no-reboot", "qemu-xhci,id=xhci",
                     "usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0", "-vga none", "VGA,xres=2560,yres=1440,vgamem_mb=64",
                     "hostfwd=tcp:127.0.0.1:20001-:7777", "virtio-net-pci,netdev=n0"] {
            assert!(j.contains(need), "missing {need} in {j}");
        }
        assert_eq!(refuse::check_qemu_args("desk", root, &v), Ok(()));
    }

    #[test]
    fn bad_extra_args_and_ports_are_caught_on_the_final_list() {
        let root = Path::new("/work/v2");
        let mut o = QemuOpts::new("t");
        o.extra = vec!["-drive".into(), "file=/dev/sda,format=raw".into()];
        let x = root.join("target/xtask");
        let v = assemble(root, "desk", &o, &x.join("v.fd"), &x.join("t.img"), &x.join("t.log")).unwrap();
        assert!(refuse::check_qemu_args("desk", root, &v).is_err());
        let mut o2 = QemuOpts::new("t");
        o2.net = "user".into();
        o2.hostfwd = vec![(2222, 22)];
        let v2 = assemble(root, "desk", &o2, &x.join("v.fd"), &x.join("t.img"), &x.join("t.log")).unwrap();
        assert!(refuse::check_qemu_args("desk", root, &v2).is_err());
    }

    #[test]
    fn hostfwd_without_net_is_an_error() {
        let root = Path::new("/work/v2");
        let mut o = QemuOpts::new("t");
        o.hostfwd = vec![(20001, 1)];
        let x = root.join("target/xtask");
        assert!(assemble(root, "desk", &o, &x.join("v.fd"), &x.join("t.img"), &x.join("t.log")).is_err());
    }
}
