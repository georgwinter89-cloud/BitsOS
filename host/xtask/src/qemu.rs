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
    /// `--boot nvme`: the stick stays plugged in without a boot index, an NVMe device among the extra arguments
    /// carries `bootindex=0` (bitsboot from an NVMe ESP, NV's boot scenario).
    pub boot_nvme: bool,
    /// `--boot nvme-only` (GP-R3): no stick plugged in at all, as the Mini-PC since its move to the Lexar.
    pub no_stick: bool,
}

impl QemuOpts {
    pub fn new(name: &str) -> QemuOpts {
        QemuOpts {
            name: name.into(), smp: 2, mem: "2G".into(), net: "none".into(), hostfwd: vec![], res: None,
            display: "none".into(), timeout: 60, serial_log: None, extra: vec![], echo: true, boot_retries: 3,
            boot_nvme: false, no_stick: false,
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
        match a.value("--boot")?.as_deref() {
            None | Some("stick") => {}
            Some("nvme") => o.boot_nvme = true,
            Some("nvme-only") => (o.boot_nvme, o.no_stick) = (true, true),
            Some(other) => return Err(format!("--boot {other}: want stick|nvme|nvme-only")),
        }
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
    if o.boot_nvme && !o.extra.iter().any(|x| x.starts_with("nvme,") && x.contains("bootindex=0")) {
        return Err("--boot nvme needs an NVMe -device with bootindex=0 among the extra arguments".into());
    }
    if !o.no_stick {
        add(&["-drive", &format!("if=none,id=stick,format=raw,file={}", img.display())]);
        let index = if o.boot_nvme { "" } else { ",bootindex=0" };
        add(&["-device", &format!("usb-storage,bus=xhci.0,drive=stick,removable=on{index}")]);
    }
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
        if !r.stalled || attempt >= retries(o.boot_retries, r.gave_up) {
            if r.stalled {
                eprintln!("xtask: firmware stall on every try ({} boots): no `bitsboot:` line in {}", attempt + 1, log.display());
            }
            // TS-1: whoever started this boot, the gate's ledger gets to see which tests it ran.
            crate::ledger::record_boot(root, &o.name, &String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()));
            return Ok(RunResult { exit: r.exit, timed_out: r.timed_out, log });
        }
        attempt += 1;
        if r.gave_up {
            eprintln!("xtask: OVMF found nothing to boot (no `bitsboot:` line); boot retry {attempt} of {}", retries(o.boot_retries, true));
        } else {
            eprintln!("xtask: OVMF stalled before bitsboot (no `bitsboot:` line); boot retry {attempt} of {}", o.boot_retries);
        }
    }
}

/// LO-1 review: OVMF's "No bootable option" (its USB stick "Not Found") comes within seconds and is a coin toss at
/// many vCPUs (measured 30.10.: about 1 in 3 boots loads bitsboot at -smp 16, the image does not matter; OV 17.11.:
/// QEMU's io_uring main loop, 0 of 20 under `qemu_command`'s filter, so this stays a rare safety net), so such a
/// boot is retried more often than a slow stall: it costs seconds, not the stall bound. A bound for one run, not a
/// count of things (R1). `--boot-retries 0` stays 0.
const FIRMWARE_MISS_RETRIES: u32 = 11;

fn retries(boot_retries: u32, gave_up: bool) -> u32 {
    if gave_up && boot_retries > 0 { boot_retries.max(FIRMWARE_MISS_RETRIES) } else { boot_retries }
}

/// OVMF variables saved from a boot that reached bitsboot (see `run_once`).
const SEED_VARS: &str = "ovmf-vars-seed.fd";

/// The firmware never reached bitsboot within `stall_secs`: a stall, not a kernel problem. OVMF brings every vCPU
/// up before bitsboot runs, slower with more of them and slower still on a busy host (W28: -smp 16 under the gate's
/// load was judged a stall), so the bound grows with both: 25 s + 3 s per vCPU, times the host's load per CPU
/// (1-minute average, counted from 1, at most 3).
fn stall_secs(smp: u32, load_per_cpu: f64) -> u64 {
    let base = 25 + 3 * u64::from(smp);
    let factor = if load_per_cpu.is_finite() { load_per_cpu.clamp(1.0, 3.0) } else { 1.0 };
    (base as f64 * factor).ceil() as u64
}

/// The host's 1-minute load average per CPU (0 when /proc/loadavg cannot be read).
fn host_load_per_cpu() -> f64 {
    let load = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()));
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    load.map_or(0.0, |l| l / cpus)
}

/// Whether this boot's variables become the seed every later boot copies: only from a boot that reached bitsboot
/// and only once, so a stalled boot's state never reaches a retry (each retry copies the seed afresh).
fn keep_as_seed(stalled: bool, seed_exists: bool, reached: bool) -> bool {
    !stalled && !seed_exists && reached
}

struct Once {
    exit: Option<i32>,
    timed_out: bool,
    stalled: bool,
    /// The stall was OVMF saying it found nothing to boot.
    gave_up: bool,
}

/// OVMF's last word when no boot option loaded (BdsDxe), with no `bitsboot:` before it: bitsboot never ran. (After
/// bitsboot ran and returned, as with an image without a system, the same line follows a `bitsboot:` line.)
fn firmware_gave_up(log: &Path) -> bool {
    std::fs::read(log).is_ok_and(|b| gave_up(&b))
}

fn gave_up(b: &[u8]) -> bool {
    let find = |n: &[u8]| b.windows(n.len()).position(|w| w == n);
    match (find(b"No bootable option or device was found"), find(b"bitsboot:")) {
        (Some(_), None) => true,
        (Some(nb), Some(bb)) => nb < bb,
        _ => false,
    }
}

/// The vCPUs QEMU gets: QemuOpts::smp, or a later `-smp N` among the extra arguments (the gate's NMI steps pass it
/// there), which QEMU takes over the first.
fn vcpus(o: &QemuOpts) -> u32 {
    let mut n = o.smp;
    for w in o.extra.windows(2) {
        if w[0] == "-smp"
            && let Some(v) = w[1].split(',').next().and_then(|x| x.trim_start_matches("cpus=").parse().ok())
        {
            n = v;
        }
    }
    n
}

/// OV (17.11.): QEMU 11.1 watches its main loop's file descriptors through io_uring, and on the host's kernel 7.3.4
/// that loop sleeps through wakeups: an NVMe doorbell or a finished disk read waits until something else wakes the
/// loop, so OVMF's BDS waits for its NVMe (or USB) read forever, or gives up with "Not Found" (nv/43's image: 21 of
/// 40 boots stalled, 0 of 40 with `io_uring_setup` refused; plan/hw/ovmf-stall-2026-11-17.txt). QEMU has no switch
/// for it; when `io_uring_setup` fails (ENOSYS) it falls back to ppoll by itself. So every QEMU runs under
/// util-linux's setpriv with a seccomp filter that refuses only that call (the same PID, no namespace).
const IO_URING_SETUP: u32 = 425;

/// The classic BPF program: on x86_64 `io_uring_setup` -> ENOSYS, every other call allowed.
fn no_io_uring_filter() -> Vec<u8> {
    const LD_ABS_W: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const RET_ERRNO_ENOSYS: u32 = 0x0005_0000 | 38;
    const RET_ALLOW: u32 = 0x7fff_0000;
    // struct seccomp_data: nr at offset 0, arch at 4.
    let prog: [(u16, u8, u8, u32); 6] = [
        (LD_ABS_W, 0, 0, 4),
        (JEQ_K, 0, 3, AUDIT_ARCH_X86_64),
        (LD_ABS_W, 0, 0, 0),
        (JEQ_K, 0, 1, IO_URING_SETUP),
        (RET_K, 0, 0, RET_ERRNO_ENOSYS),
        (RET_K, 0, 0, RET_ALLOW),
    ];
    let mut v = Vec::with_capacity(prog.len() * 8);
    for (code, jt, jf, k) in prog {
        v.extend_from_slice(&code.to_le_bytes());
        v.extend_from_slice(&[jt, jf]);
        v.extend_from_slice(&k.to_le_bytes());
    }
    v
}

/// `setpriv --nnp --seccomp-filter <filter> qemu-system-x86_64`, the filter file in `x` (written atomically:
/// several xtasks may start at once). A setpriv without `--seccomp-filter` (util-linux before 2.39) is an error with
/// its reason, never a QEMU that stalls.
fn qemu_command(x: &Path) -> Result<Command, String> {
    let help = Command::new("setpriv").arg("--help").output()
        .map_err(|e| format!("cannot start setpriv (util-linux), which every QEMU runs under (OV): {e}"))?;
    if !String::from_utf8_lossy(&help.stdout).contains("--seccomp-filter") {
        return Err("setpriv has no --seccomp-filter (util-linux 2.39 or newer needed), and QEMU's io_uring main loop stalls OVMF on this host (OV)".into());
    }
    let filter = x.join("qemu-no-io-uring.bpf");
    let want = no_io_uring_filter();
    if std::fs::read(&filter).ok().as_deref() != Some(&want[..]) {
        let tmp = x.join(format!("qemu-no-io-uring.bpf.{}", std::process::id()));
        std::fs::write(&tmp, &want).and_then(|_| std::fs::rename(&tmp, &filter)).map_err(|e| format!("write {}: {e}", filter.display()))?;
    }
    let mut c = Command::new("setpriv");
    c.args(["--nnp", "--seccomp-filter"]).arg(&filter).arg("qemu-system-x86_64");
    Ok(c)
}

fn reached_bitsboot(log: &Path) -> bool {
    std::fs::read(log).map(|b| b.windows(9).any(|w| w == b"bitsboot:")).unwrap_or(false)
}

fn run_once(root: &Path, o: &QemuOpts, lane: &str, x: &Path, img: &Path, log: &Path) -> Result<Once, String> {
    let vars = x.join(format!("{}-vars.fd", o.name));
    // OVMF often hangs on its first boot with fresh variables (it builds its boot entries then); variables
    // saved after a boot that reached bitsboot avoid that (measured 15.10.: 8 of 8 boots instead of ~2 of 3).
    // OV 17.11.: most likely QEMU's io_uring main loop as well (`qemu_command`): under its filter fresh variables
    // and the seed both boot 20 of 20; the seed stays, it costs nothing.
    let seed = x.join(SEED_VARS);
    let from = if seed.is_file() { seed.clone() } else { PathBuf::from(OVMF_VARS) };
    std::fs::copy(&from, &vars).map_err(|e| format!("copy {}: {e}", from.display()))?;
    let _ = std::fs::remove_file(log);
    let args = assemble(root, lane, o, &vars, img, log)?;
    if let Err(r) = refuse::check_qemu_args(lane, root, &args) {
        return Err(format!("{r}\nxtask: QEMU was not started."));
    }
    let mut child = qemu_command(x)?
        .args(&args)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start qemu-system-x86_64 under setpriv: {e}"))?;
    let pid = child.id();
    let pidfile = x.join(format!("qemu-{}.pid", o.name));
    let _ = std::fs::write(&pidfile, format!("{pid}\n"));
    eprintln!("xtask: qemu pid {pid} (lane {lane}, name v2-{lane}-{}), timeout {} s, log {}", o.name, o.timeout, log.display());

    let stall = stall_secs(vcpus(o), host_load_per_cpu());
    let start = Instant::now();
    let mut echoed: u64 = 0;
    let mut timed_out = false;
    let mut seen_loader = false;
    let mut stalled = false;
    let mut gave_up = false;
    let status = loop {
        if o.echo {
            echo_new(log, &mut echoed);
        }
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            stalled = !reached_bitsboot(log);
            break Some(st);
        }
        // LO-1 review: OVMF said it found nothing to boot (its "Not Found" for the USB stick at many vCPUs): it never
        // tries again by itself, so the boot is a stall now, not after the whole bound.
        if !seen_loader && firmware_gave_up(log) {
            stalled = true;
            gave_up = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if !seen_loader && start.elapsed() >= Duration::from_secs(stall.min(o.timeout)) {
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
    if keep_as_seed(stalled, seed.is_file(), reached_bitsboot(log)) {
        let tmp = x.join(format!("{SEED_VARS}.{}", std::process::id()));
        if std::fs::copy(&vars, &tmp).is_ok() {
            let _ = std::fs::rename(&tmp, &seed);
        }
    }
    Ok(Once { exit: status.and_then(|s| s.code()), timed_out: timed_out || (stalled && status.is_none()), stalled, gave_up })
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
    fn stall_bound_grows_with_vcpus_and_load() {
        assert_eq!(stall_secs(2, 0.0), 31, "an idle host, 2 vCPUs");
        assert_eq!(stall_secs(16, 0.5), 73, "16 vCPUs: 25 + 48");
        assert_eq!(stall_secs(16, 2.0), 146, "twice as many runnable tasks as CPUs");
        assert_eq!(stall_secs(16, 50.0), 219, "at most three times");
        assert_eq!(stall_secs(4, f64::NAN), 37);
    }

    #[test]
    fn boot_from_nvme_keeps_the_stick_without_a_boot_index() {
        let (root, vars, img, log) = (Path::new("/w"), Path::new("/w/v.fd"), Path::new("/w/s.img"), Path::new("/w/l"));
        let stick = |o: &QemuOpts| assemble(root, "nv", o, vars, img, log).map(|a| a.into_iter().find(|x| x.starts_with("usb-storage")));
        let mut o = QemuOpts::new("t");
        assert_eq!(stick(&o).unwrap().as_deref(), Some("usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0"));
        o.boot_nvme = true;
        assert!(stick(&o).is_err(), "no NVMe device to boot");
        o.extra = ["-device", "nvme,id=n0,serial=x,drive=nv0,bootindex=0"].iter().map(|s| s.to_string()).collect();
        assert_eq!(stick(&o).unwrap().as_deref(), Some("usb-storage,bus=xhci.0,drive=stick,removable=on"));
        // nvme-only: neither the stick's device nor its drive.
        o.no_stick = true;
        let a = assemble(root, "nv", &o, vars, img, log).unwrap();
        assert!(!a.iter().any(|x| x.starts_with("usb-storage") || x.contains("id=stick")), "{a:?}");
        o.extra.clear();
        assert!(assemble(root, "nv", &o, vars, img, log).is_err(), "nvme-only still needs the NVMe to boot");
    }

    #[test]
    fn firmware_gave_up_only_without_bitsboot_first() {
        assert!(gave_up(b"BdsDxe: failed to load Boot0002 ...: Not Found\r\nBdsDxe: No bootable option or device was found.\r\n"));
        assert!(!gave_up(b"bitsboot: cannot start BitsOS\r\nBdsDxe: No bootable option or device was found.\r\n"), "bitsboot ran");
        assert!(!gave_up(b"BdsDxe: loading Boot0002\r\nbitsboot: v2\r\n"));
        assert!(!gave_up(b""));
        let mut o = QemuOpts::new("t");
        assert_eq!(vcpus(&o), o.smp);
        o.extra = ["-device", "x", "-smp", "16"].iter().map(|s| s.to_string()).collect();
        assert_eq!(vcpus(&o), 16, "the gate's -smp among the extra arguments");
        o.extra = ["-smp", "cpus=8,sockets=1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(vcpus(&o), 8);
        assert_eq!((retries(3, false), retries(3, true), retries(0, true)), (3, FIRMWARE_MISS_RETRIES, 0));
    }

    #[test]
    fn three_boot_retries_from_a_seed_no_stall_wrote() {
        assert_eq!(QemuOpts::new("t").boot_retries, 3, "W28: two retries were not enough at -smp 16");
        assert!(keep_as_seed(false, false, true));
        assert!(!keep_as_seed(true, false, false), "a stalled boot's variables never become the seed");
        assert!(!keep_as_seed(true, false, true));
        assert!(!keep_as_seed(false, true, true), "the seed is written once");
        assert!(!keep_as_seed(false, false, false));
    }

    #[test]
    fn qemu_starts_with_io_uring_setup_refused() {
        let f = no_io_uring_filter();
        assert_eq!(f.len(), 6 * 8, "six sock_filter instructions");
        assert_eq!(u32::from_le_bytes(f[28..32].try_into().unwrap()), IO_URING_SETUP);
        assert_eq!(u32::from_le_bytes(f[36..40].try_into().unwrap()), 0x0005_0026, "SECCOMP_RET_ERRNO | ENOSYS");
        let x = std::env::temp_dir().join(format!("xtask-ov-{}", std::process::id()));
        std::fs::create_dir_all(&x).unwrap();
        let c = qemu_command(&x).unwrap();
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(c.get_program(), "setpriv");
        assert_eq!(args, ["--nnp", "--seccomp-filter", &x.join("qemu-no-io-uring.bpf").to_string_lossy(), "qemu-system-x86_64"]);
        assert_eq!(std::fs::read(x.join("qemu-no-io-uring.bpf")).unwrap(), f);
        // The kernel takes the filter, and under it io_uring_setup fails with ENOSYS while other calls work.
        let probe = "import ctypes,os\nl=ctypes.CDLL(None,use_errno=True)\nr=l.syscall(425,1,ctypes.create_string_buffer(120))\nprint(r,ctypes.get_errno(),os.getpid()>0)";
        let out = |filtered: bool| {
            let mut c = Command::new(if filtered { "setpriv" } else { "python3" });
            if filtered {
                c.args(["--nnp", "--seccomp-filter"]).arg(x.join("qemu-no-io-uring.bpf")).arg("python3");
            }
            let o = c.args(["-I", "-c", probe]).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert_eq!(out(true), "-1 38 True", "io_uring_setup refused with ENOSYS under the filter");
        assert!(!out(false).starts_with("-1 38"), "without the filter the host has io_uring (else this test proves nothing)");
        let _ = std::fs::remove_dir_all(&x);
    }

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
