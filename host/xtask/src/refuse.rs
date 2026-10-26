//! The QEMU refusals (V2-PLAN 9.1.1). One function, `check_qemu_args`, looks at the FINAL argument list.
//! Whatever it cannot classify with confidence is refused: a false refusal costs a minute, a false pass can
//! cost Georg's disk or a port he uses.
use crate::util::lane_range;
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub rule: &'static str,
    pub detail: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "refused [{}]: {}", self.rule, self.detail)
    }
}

fn refuse<T>(rule: &'static str, detail: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal { rule, detail: detail.into() })
}

/// Ports that are never allowed: Georg's real mirror listener and the V1 MCP ports.
const ALWAYS_REFUSED: [u32; 3] = [6666, 8765, 8770];
const EPHEMERAL: (u32, u32) = (32768, 60999);

struct Ctx<'a> {
    lane: &'a str,
    lo: u32,
    hi: u32,
    target: PathBuf,
}

/// Port rule shared with `serve`. `what` names where the number came from.
pub fn check_port(lane: &str, port: u32, what: &str) -> Result<(), Refusal> {
    let Some((lo, hi)) = lane_range(lane) else {
        return refuse("lane", format!("unknown lane '{lane}'"));
    };
    port_rule(lane, lo as u32, hi as u32, port, what)
}

fn port_rule(lane: &str, lo: u32, hi: u32, port: u32, what: &str) -> Result<(), Refusal> {
    if ALWAYS_REFUSED.contains(&port) {
        return refuse("forbidden-port", format!("{what}: port {port} is never allowed"));
    }
    if (EPHEMERAL.0..=EPHEMERAL.1).contains(&port) {
        return refuse("ephemeral-port", format!("{what}: port {port} is in the host's ephemeral range 32768-60999"));
    }
    if port < lo || port > hi {
        return refuse("lane-port", format!("{what}: port {port} is outside the range {lo}-{hi} of lane '{lane}'"));
    }
    Ok(())
}

fn port_str(cx: &Ctx, s: &str, what: &str) -> Result<(), Refusal> {
    let t = s.trim();
    match t.parse::<u32>() {
        Ok(p) => port_rule(cx.lane, cx.lo, cx.hi, p, what),
        Err(_) => refuse("port-syntax", format!("{what}: cannot read a port number from '{s}'")),
    }
}

/// Port of a `host:port` / `:port` / `port` / `[::1]:port` string.
fn tail_port(cx: &Ctx, s: &str, what: &str) -> Result<(), Refusal> {
    port_str(cx, s.rsplit(':').next().unwrap_or(s), what)
}

/// Lexical normalisation, then symlink resolution of the longest existing ancestor.
fn resolve(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = norm.as_path();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(par), Some(name)) => {
                rest.push(name.to_os_string());
                cur = par;
            }
            _ => return norm,
        }
    }
}

fn lexical(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    norm
}

fn strip_fat(mut v: &str) -> &str {
    if let Some(r) = v.strip_prefix("fat:") {
        v = r;
        loop {
            let mut changed = false;
            for p in ["rw:", "floppy:", "16:", "32:"] {
                if let Some(r) = v.strip_prefix(p) {
                    v = r;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    v
}

/// `ovmf_ro`: the token declared the file read-only, which is the one exception for OVMF_CODE.
fn path_rule(cx: &Ctx, raw: &str, what: &str, ovmf_ro: bool) -> Result<(), Refusal> {
    let v = strip_fat(raw);
    if v.is_empty() {
        return refuse("path", format!("{what}: empty path"));
    }
    if v.contains("://") || ["nbd:", "iscsi:", "ssh:", "json:", "nfs:", "gluster:"].iter().any(|p| v.starts_with(p)) {
        return refuse("remote-file", format!("{what}: '{raw}' is not a local file"));
    }
    if v == "/dev" || v.starts_with("/dev/") {
        return refuse("dev-path", format!("{what}: '{raw}' is under /dev"));
    }
    let lex = lexical(Path::new(v));
    if ovmf_ro
        && lex.starts_with("/usr/share/edk2")
        && lex.file_name().map(|n| n.to_string_lossy().starts_with("OVMF_CODE")).unwrap_or(false)
    {
        return Ok(());
    }
    let res = resolve(Path::new(v));
    if res.starts_with("/dev") {
        return refuse("dev-path", format!("{what}: '{raw}' resolves into /dev"));
    }
    if !res.starts_with(&cx.target) {
        return refuse(
            "outside-worktree",
            format!("{what}: '{raw}' resolves to {} which is outside {}", res.display(), cx.target.display()),
        );
    }
    Ok(())
}

/// Split on ',' where ",," is a literal comma (QEMU's escape).
fn split_opts(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == ',' {
            if i + 1 < b.len() && b[i + 1] == ',' {
                out.last_mut().unwrap().push(',');
                i += 2;
                continue;
            }
            out.push(String::new());
        } else {
            out.last_mut().unwrap().push(b[i]);
        }
        i += 1;
    }
    out
}

fn kv(seg: &str) -> (&str, Option<&str>) {
    match seg.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (seg, None),
    }
}

/// Forbidden words anywhere, `/dev/` as a value, and `hostfwd=` wherever it shows up.
fn scan_token(cx: &Ctx, tok: &str) -> Result<(), Refusal> {
    let low = tok.to_lowercase();
    for (w, rule) in [("usb-host", "usb-host"), ("vfio", "vfio"), ("host_device", "host-device"), ("host_cdrom", "host-device")] {
        if low.contains(w) {
            return refuse(rule, format!("'{tok}' mentions {w}: host devices are never passed through"));
        }
    }
    let mut from = 0;
    while let Some(i) = tok[from..].find("/dev/") {
        let at = from + i;
        let before = tok[..at].chars().last();
        if matches!(before, None | Some('=') | Some(':') | Some(',') | Some('"') | Some('\'') | Some(' ')) {
            return refuse("dev-path", format!("'{tok}' names a path under /dev"));
        }
        from = at + 5;
    }
    let mut from = 0;
    while let Some(i) = tok[from..].find("hostfwd=") {
        let rest = &tok[from + i + 8..];
        let spec = rest.split(',').next().unwrap_or("");
        // [tcp|udp]:[hostaddr]:hostport-[guestaddr]:guestport
        let host_side = spec.split('-').next().unwrap_or("");
        let host_side = host_side.strip_prefix("tcp:").or(host_side.strip_prefix("udp:")).unwrap_or(host_side);
        tail_port(cx, host_side, "hostfwd")?;
        from += i + 8;
    }
    Ok(())
}

/// Generic `key=value` scan: file-like keys are paths, wherever they occur (but not inside `-append`).
fn scan_keys(cx: &Ctx, tok: &str) -> Result<(), Refusal> {
    let segs = split_opts(tok);
    let ro = segs.iter().any(|s| matches!(s.as_str(), "readonly=on" | "readonly=true" | "read-only=on" | "read-only=true"));
    for s in &segs {
        let (k, v) = kv(s);
        let Some(v) = v else { continue };
        if k == "driver" && ["host_device", "host_cdrom", "nbd", "iscsi", "ssh", "http", "https", "ftp", "ftps", "nfs", "gluster", "rbd"].contains(&v) {
            return refuse("remote-block", format!("driver={v} is not allowed"));
        }
        let pathish = k == "file" || k.ends_with("path") || k.ends_with("file") || k.ends_with("filename") || k == "backing" || k == "smb" || k == "tftp";
        if pathish {
            path_rule(cx, v, k, ro)?;
        }
    }
    Ok(())
}

fn check_char_spec(cx: &Ctx, v: &str, what: &str) -> Result<(), Refusal> {
    let v = v.strip_prefix("mon:").unwrap_or(v);
    let head = split_opts(v).remove(0);
    let h = head.as_str();
    if ["stdio", "null", "none", "pty", "msmouse", "braille", "defer"].contains(&h) || h.starts_with("vc") || h.starts_with("chardev:") {
        return Ok(());
    }
    for p in ["file:", "pipe:", "unix:"] {
        if let Some(rest) = h.strip_prefix(p) {
            return path_rule(cx, rest, what, false);
        }
    }
    for p in ["tcp:", "udp:", "telnet:", "telnets:", "websocket:"] {
        if let Some(rest) = h.strip_prefix(p) {
            return tail_port(cx, rest, what);
        }
    }
    if h.starts_with('/') {
        return path_rule(cx, h, what, false);
    }
    refuse("char-spec", format!("{what}: cannot classify '{v}'"))
}

fn check_chardev(cx: &Ctx, v: &str) -> Result<(), Refusal> {
    let segs = split_opts(v);
    let backend = segs[0].as_str();
    let ok = ["socket", "udp", "file", "pipe", "null", "pty", "stdio", "vc", "ringbuf", "memory", "mux", "hub", "msmouse", "braille"];
    if !ok.contains(&backend) {
        return refuse("chardev-backend", format!("chardev backend '{backend}' is not allowed (host device)"));
    }
    for s in &segs[1..] {
        let (k, val) = kv(s);
        let Some(val) = val else { continue };
        match k {
            "port" | "localport" | "to" => port_str(cx, val, &format!("chardev {k}"))?,
            "fd" => return refuse("chardev-fd", "chardev fd= inherits a descriptor"),
            _ => {}
        }
    }
    Ok(())
}

fn check_net(cx: &Ctx, v: &str) -> Result<(), Refusal> {
    let segs = split_opts(v);
    let first = segs[0].as_str();
    if !first.contains('=') {
        let ok = ["user", "none", "nic", "socket", "dgram", "stream", "hubport", "hub"];
        if !ok.contains(&first) {
            return refuse("net-backend", format!("network backend '{first}' touches the host network; ask the desk"));
        }
    }
    for s in &segs {
        let (k, val) = kv(s);
        let Some(val) = val else { continue };
        match k {
            "listen" | "connect" | "mcast" | "udp" | "localaddr" => tail_port(cx, val, &format!("netdev {k}"))?,
            "addr.port" | "local.port" | "remote.port" | "port" | "to" => port_str(cx, val, &format!("netdev {k}"))?,
            "fd" => return refuse("net-fd", "netdev fd= inherits a descriptor"),
            "guestfwd" if val.contains("cmd:") => return refuse("guestfwd-cmd", "guestfwd with cmd: runs a host command"),
            _ => {}
        }
    }
    Ok(())
}

/// Sound front ends that play through an audiodev; without `audiodev=` QEMU picks its default back end
/// (PipeWire/Pulse on this host), so they are refused without one (SN-2: never Georg's sound devices).
const AUDIO_FRONTENDS: &[&str] = &[
    "hda-duplex", "hda-output", "hda-micro", "usb-audio", "AC97", "ac97", "sb16", "es1370", "adlib", "gus", "cs4231a",
    "virtio-sound-pci", "virtio-sound", "virtio-sound-device",
];
/// The only audio back ends allowed: a WAV file under the worktree's target, or none.
const AUDIO_BACKENDS: &[&str] = &["wav", "none"];

/// `-audiodev X,...` or `-audio [driver=]X,...`: only `wav` or `none`.
fn check_audiodev(v: &str, what: &str) -> Result<(), Refusal> {
    let segs = split_opts(v);
    let first = segs.first().map(String::as_str).unwrap_or("");
    let driver = match kv(first) {
        ("driver", Some(d)) => d.to_string(),
        (k, None) => k.to_string(),
        _ => segs.iter().find_map(|s| match kv(s) {
            ("driver", Some(d)) => Some(d.to_string()),
            _ => None,
        }).unwrap_or_default(),
    };
    if AUDIO_BACKENDS.contains(&driver.as_str()) {
        Ok(())
    } else {
        refuse("audio-backend", format!("{what}: audio back end '{driver}' would reach the host's sound devices; only wav or none"))
    }
}

/// `-device MODEL,...`: a sound front end needs an explicit `audiodev=`.
fn check_audio_device(v: &str) -> Result<(), Refusal> {
    let segs = split_opts(v);
    let model = match segs.first().map(|s| kv(s)) {
        Some(("driver", Some(d))) => d,
        Some((k, None)) => k,
        _ => segs.iter().find_map(|s| match kv(s) {
            ("driver", Some(d)) => Some(d),
            _ => None,
        }).unwrap_or(""),
    };
    if AUDIO_FRONTENDS.contains(&model) && !segs.iter().any(|s| matches!(kv(s), ("audiodev", Some(_)))) {
        return refuse("audio-default", format!("-device {model} without audiodev= would use QEMU's default (host) sound back end"));
    }
    Ok(())
}

/// Options that take a path as the whole value.
const PATH_OPTS: &[&str] = &[
    "hda", "hdb", "hdc", "hdd", "cdrom", "fda", "fdb", "kernel", "initrd", "bios", "pflash", "dtb", "mtdblock", "sd", "pidfile",
    "readconfig", "loadvm", "D", "L", "dumpdir",
];
const CHAR_OPTS: &[&str] = &["serial", "monitor", "qmp", "qmp-pretty", "parallel", "debugcon", "gdb", "incoming"];
/// Options without a value (so the next token is not their value).
const FLAG_OPTS: &[&str] = &[
    "enable-kvm", "nographic", "no-reboot", "no-shutdown", "S", "snapshot", "nodefaults", "usb", "daemonize", "no-user-config",
    "no-hpet", "no-acpi", "full-screen", "k", "show-cursor", "no-fd-bootchk", "overcommit", "sandbox",
];

pub fn check_qemu_args(lane: &str, root: &Path, args: &[String]) -> Result<(), Refusal> {
    let Some((lo, hi)) = lane_range(lane) else {
        return refuse("lane", format!("unknown lane '{lane}' (no port range)"));
    };
    let cx = Ctx { lane, lo: lo as u32, hi: hi as u32, target: resolve(&root.join("target")) };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        scan_token(&cx, a)?;
        let is_opt = a.starts_with('-') && a.len() > 1;
        if !is_opt {
            scan_keys(&cx, a)?;
            i += 1;
            continue;
        }
        let name = a.trim_start_matches('-');
        match name {
            "s" => return refuse("gdb-default", "-s opens gdb on tcp::1234; use -gdb with a port of the lane"),
            "usbdevice" => return refuse("usb-host", "-usbdevice is not allowed"),
            _ => {}
        }
        if FLAG_OPTS.contains(&name) {
            i += 1;
            continue;
        }
        let Some(val) = args.get(i + 1) else { break };
        scan_token(&cx, val)?;
        if name == "chardev" {
            check_chardev(&cx, val)?;
        }
        if name != "append" {
            scan_keys(&cx, val)?;
        }
        if PATH_OPTS.contains(&name) {
            path_rule(&cx, val, &format!("-{name}"), false)?;
        } else if CHAR_OPTS.contains(&name) {
            check_char_spec(&cx, val, &format!("-{name}"))?;
        } else {
            match name {
                "netdev" | "net" | "nic" => check_net(&cx, val)?,
                "audiodev" | "audio" => check_audiodev(val, &format!("-{name}"))?,
                "device" => check_audio_device(val)?,
                "soundhw" => return refuse("audio-default", "-soundhw uses QEMU's default (host) sound back end"),
                "vnc" => {
                    let head = val.split(',').next().unwrap_or("");
                    if head == "none" {
                    } else if let Some(p) = head.strip_prefix("unix:") {
                        path_rule(&cx, p, "-vnc", false)?;
                    } else {
                        return refuse("vnc", "-vnc listens on 5900+N, outside the lane range; use -display");
                    }
                }
                "spice" => {
                    for s in split_opts(val) {
                        if let ("port" | "tls-port", Some(p)) = kv(&s) {
                            port_str(&cx, p, "-spice")?;
                        }
                    }
                }
                _ => {}
            }
        }
        i += 2;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/v2";

    fn v(s: &str) -> Vec<String> {
        // Whitespace-separated, so test lines stay readable.
        s.split_whitespace().map(String::from).collect()
    }

    fn check(lane: &str, s: &str) -> Result<(), Refusal> {
        check_qemu_args(lane, Path::new(ROOT), &v(s))
    }

    fn ok(s: &str) {
        if let Err(e) = check("desk", s) {
            panic!("expected allowed: {s}\n  got {e}");
        }
    }

    fn bad(s: &str, rule: &str) {
        match check("desk", s) {
            Ok(()) => panic!("expected refusal [{rule}]: {s}"),
            Err(e) => assert_eq!(e.rule, rule, "wrong rule for: {s} ({e})"),
        }
    }

    #[test]
    fn typical_xtask_command_line_passes() {
        ok("-name v2-desk-t -machine q35,accel=kvm -cpu host -m 2G -smp 2 -global ICH9-LPC.disable_s3=1 -no-reboot \
            -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
            -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
            -drive if=pflash,format=raw,file=/work/v2/target/xtask/t-vars.fd \
            -device qemu-xhci,id=xhci -drive if=none,id=stick,format=raw,file=/work/v2/target/xtask/t.img \
            -device usb-storage,bus=xhci.0,drive=stick,removable=on \
            -serial file:/work/v2/target/xtask/t.log -display none -monitor none -vga std \
            -netdev user,id=n0,hostfwd=tcp:127.0.0.1:20001-:7777 -device virtio-net-pci,netdev=n0");
    }

    #[test]
    fn disk_images_inside_target_pass() {
        ok("-drive file=/work/v2/target/xtask/disk.img,format=raw,if=none,id=d0 -device nvme,drive=d0,serial=x");
        ok("-drive file=fat:rw:/work/v2/target/xtask/esp-t,format=raw");
        ok("-kernel /work/v2/target/x86_64-unknown-none/debug/kernel -initrd /work/v2/target/xtask/initrd.cpio");
    }

    #[test]
    fn ports_inside_the_lane_pass() {
        ok("-gdb tcp::20010");
        ok("-serial tcp:127.0.0.1:20011,server,nowait");
        ok("-monitor tcp:127.0.0.1:20012,server,nowait");
        ok("-chardev socket,id=c0,host=127.0.0.1,port=20013,server=on,wait=off");
        ok("-netdev socket,id=s0,listen=:20014");
        ok("-nic user,hostfwd=tcp::20015-:22,hostfwd=udp::20016-:53");
        ok("-qmp unix:/work/v2/target/xtask/q.sock,server,nowait");
    }

    #[test]
    fn other_lanes_ports_follow_the_lane() {
        assert!(check("bt", "-gdb tcp::20510").is_ok());
        assert_eq!(check("bt", "-gdb tcp::20010").unwrap_err().rule, "lane-port");
        assert_eq!(check("desk", "-gdb tcp::20510").unwrap_err().rule, "lane-port");
        assert_eq!(check("nobody", "-m 1G").unwrap_err().rule, "lane");
    }

    #[test]
    fn dev_paths_are_refused() {
        bad("-drive file=/dev/sda,format=raw", "dev-path");
        bad("-drive if=none,id=x,file=/dev/nvme0n1", "dev-path");
        bad("-blockdev driver=file,node-name=a,filename=/dev/sdb", "dev-path");
        bad("-hda /dev/sda", "dev-path");
        bad("-cdrom /dev/sr0", "dev-path");
        bad("-serial /dev/ttyS0", "dev-path");
        bad("-drive file=fat:rw:/dev/shm/x", "dev-path");
    }

    #[test]
    fn host_device_passthrough_is_refused() {
        bad("-device usb-host,vendorid=0x1234", "usb-host");
        bad("-usbdevice host:1234:5678", "usb-host");
        bad("-device vfio-pci,host=01:00.0", "vfio");
        bad("-object iommufd,id=io0 -device vfio-pci-nohotplug,host=0000:01:00.0", "vfio");
        bad("-blockdev driver=host_device,node-name=a,filename=x", "host-device");
        bad("-drive driver=host_cdrom,file=x", "host-device");
    }

    #[test]
    fn paths_outside_target_are_refused() {
        bad("-drive file=/home/georg/disk.img,format=raw", "outside-worktree");
        bad("-drive file=/work/v2/other/disk.img", "outside-worktree");
        bad("-drive file=/work/v2/target/../stick.img", "outside-worktree");
        bad("-kernel /tmp/kernel", "outside-worktree");
        bad("-initrd /work/v2/targets/x", "outside-worktree");
        bad("-serial file:/tmp/log", "outside-worktree");
        bad("-drive file=relative-that-is-not-under-target.img", "outside-worktree");
        bad("-drive file=nbd:127.0.0.1:20000", "remote-file");
    }

    #[test]
    fn only_ovmf_code_read_only_may_leave_target() {
        ok("-drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd");
        bad("-drive if=pflash,format=raw,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd", "outside-worktree");
        bad("-drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_VARS.4m.fd", "outside-worktree");
        bad("-drive if=pflash,readonly=on,file=/usr/share/edk2/../../../etc/OVMF_CODE.fd", "outside-worktree");
    }

    #[test]
    fn symlinks_cannot_leave_target() {
        let base = std::env::temp_dir().join(format!("xt-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("target")).unwrap();
        std::os::unix::fs::symlink("/etc", base.join("target/link")).unwrap();
        let args = v(&format!("-drive file={}/target/link/passwd", base.display()));
        let r = check_qemu_args("desk", &base, &args);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(r.unwrap_err().rule, "outside-worktree");
    }

    #[test]
    fn ports_outside_the_range_are_refused() {
        bad("-nic user,hostfwd=tcp::2222-:22", "lane-port");
        bad("-netdev user,id=n,hostfwd=tcp:127.0.0.1:19999-:22", "lane-port");
        bad("-gdb tcp::1234", "lane-port");
        bad("-s", "gdb-default");
        bad("-serial tcp:127.0.0.1:5555,server,nowait", "lane-port");
        bad("-monitor tcp::20600,server", "lane-port");
        bad("-chardev socket,id=c,host=0.0.0.0,port=21000", "lane-port");
        bad("-netdev socket,id=s,connect=127.0.0.1:20600", "lane-port");
        bad("-netdev socket,id=s,udp=127.0.0.1:20000,localaddr=127.0.0.1", "port-syntax");
        bad("-qmp tcp:localhost:4444,server", "lane-port");
    }

    #[test]
    fn ephemeral_and_forbidden_ports_are_refused() {
        bad("-nic user,hostfwd=tcp::40000-:22", "ephemeral-port");
        bad("-gdb tcp::32768", "ephemeral-port");
        bad("-gdb tcp::60999", "ephemeral-port");
        bad("-nic user,hostfwd=tcp::6666-:22", "forbidden-port");
        bad("-serial tcp:127.0.0.1:8765,server", "forbidden-port");
        bad("-chardev socket,id=c,port=8770", "forbidden-port");
        assert_eq!(check_port("sx", 32767, "t"), Ok(()));
        assert_eq!(check_port("sx", 32768, "t").unwrap_err().rule, "ephemeral-port");
    }

    #[test]
    fn host_network_and_unclassified_things_are_refused() {
        bad("-netdev tap,id=t,ifname=tap0", "net-backend");
        bad("-netdev bridge,id=b,br=br0", "net-backend");
        bad("-netdev user,id=n,guestfwd=tcp:10.0.2.1:80-cmd:nc host 80", "guestfwd-cmd");
        bad("-vnc :1", "vnc");
        bad("-chardev serial,id=s,path=x", "chardev-backend");
        bad("-chardev socket,id=c,fd=3", "chardev-fd");
        bad("-serial something-odd", "char-spec");
        bad("-gdb dev", "char-spec");
    }

    #[test]
    fn extra_args_are_checked_like_the_rest() {
        let mut all = v("-machine q35 -m 1G");
        all.extend(v("-drive file=/dev/sda"));
        assert!(check_qemu_args("desk", Path::new(ROOT), &all).is_err());
        let mut fine = v("-machine q35 -m 1G");
        fine.extend(v("-d guest_errors"));
        assert!(check_qemu_args("desk", Path::new(ROOT), &fine).is_ok());
    }

    #[test]
    fn comma_escape_is_understood() {
        assert_eq!(split_opts("a=1,,2,b=3"), vec!["a=1,2", "b=3"]);
        // A ",," cannot be used to hide the key from the scan.
        bad("-drive file=/work/v2/target/a,,b,file=/dev/sda", "dev-path");
    }

    #[test]
    fn sound_only_into_wav_or_nowhere() {
        ok("-audiodev wav,id=a0,path=/work/v2/target/xtask/runs/x/a.wav -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0");
        ok("-audiodev none,id=a1 -device hda-output,audiodev=a1");
        ok("-audiodev driver=wav,id=a0,path=/work/v2/target/a.wav");
        bad("-audiodev pipewire,id=a0", "audio-backend");
        bad("-audiodev pa,id=a0", "audio-backend");
        bad("-audiodev id=a0,driver=alsa", "audio-backend");
        bad("-audio pipewire,model=hda", "audio-backend");
        bad("-device hda-duplex,bus=h0.0", "audio-default");
        bad("-device driver=usb-audio", "audio-default");
        bad("-soundhw hda", "audio-default");
        bad("-audiodev wav,id=a0,path=/home/georg/a.wav", "outside-worktree");
    }
}
