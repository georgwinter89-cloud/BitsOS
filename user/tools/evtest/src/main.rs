//! evtest: input devices and their events (item UL-4b), in the format of the freedesktop `evtest`.
//!
//! `evtest [--dir DIR]` lists the evdev nodes (`DIR/event*`, default /dev/input) with their names.
//! `evtest [--grab] [--describe] [--monotonic] [--count N] [--timeout SECS] DEVICE` prints the device's id,
//! name, supported events (with ABS ranges and the EV_REP settings) and properties, then every event until N
//! non-SYN events came (`--count`) or SECS passed (`--timeout`, default 10; 0 = until killed). `--grab` takes the
//! device for itself (EVIOCGRAB: the console sees no keys meanwhile) and gives it back before the end.
//! `--describe`: the description only. `--monotonic`: EVIOCSCLOCKID(CLOCK_MONOTONIC) first, as libinput does,
//! and every event time checked against the monotonic clock. After a SYN_DROPPED the key/LED/switch state is
//! read again (EVIOCGKEY/GLED/GSW), as Linux asks a reader to resync, and printed.
//! `evtest --query DEVICE TYPE CODE` (freedesktop evtest's): TYPE EV_KEY/EV_LED/EV_SND/EV_SW, CODE a name or
//! number; exit 10 when that key/LED/sound/switch is set now, 0 when not.
//!
//! Exit codes: 0 ok, 1 open/read error (and an event time off the monotonic clock), 2 usage, 3 `--count` not
//! reached within the timeout, 4 not an evdev device (ENOTTY), 10 `--query`: set. Linux semantics:
//! linux-6.6/drivers/input/evdev.c (`evdev_do_ioctl` :1029, `evdev_read` :558, `evdev_handle_get_val` :908).
#![forbid(unsafe_code)]

mod codes;

use std::process::exit;
use usys::dev::{ioctl_buf, ioctl_val};

const O_NONBLOCK: usize = 0o4000;
const O_CLOEXEC: usize = 0o2000000;
const EVENT_SIZE: usize = 24;
const EV_SYN: u16 = 0;
const EV_ABS: u16 = 3;
const EV_REP: u16 = 0x14;
const EV_MAX: u16 = 0x1f;
/// The highest code per type (input-event-codes.h *_MAX), for the EVIOCGBIT buffer sizes.
const KEY_MAX: usize = 0x2ff;

/// _IOC(dir, 'E', nr, size) (linux-6.6/include/uapi/linux/input.h).
fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (0x45 << 8) | nr
}
const READ: u32 = 2;
const EVIOCGRAB: u32 = 0x4004_4590;

const USAGE: &str = "usage: evtest [--dir DIR]\n       evtest [--grab] [--describe] [--monotonic] [--count N] [--timeout SECS] DEVICE\n       evtest --query DEVICE TYPE CODE\n";

fn help() -> ! {
    print!(
        "{USAGE}\nWithout DEVICE: lists DIR/event* (default /dev/input) with their names.\n\
         With DEVICE: id, name, supported events, properties, then each event as\n\
         'Event: time S.US, type T (EV_..), code C (..), value V'.\n\
         --count N      stop after N events that are not EV_SYN\n\
         --timeout SECS stop after SECS seconds (default 10; 0 = until killed)\n\
         --grab         EVIOCGRAB the device while reading (released at the end)\n\
         --describe     print the description only\n\
         --monotonic    EVIOCSCLOCKID(CLOCK_MONOTONIC), check each event time against that clock\n\
         --query DEVICE TYPE CODE  exit 10 when the key/led/snd/sw is set, 0 when not\n\
         exit codes: 0 ok, 1 open/read error, 2 usage, 3 --count not reached in time, 4 not an evdev device,\n\
         10 --query: set\n"
    );
    exit(0)
}

fn usage(msg: &str) -> ! {
    eprint!("evtest: {msg}\n{USAGE}");
    exit(2)
}

struct Args {
    dir: String,
    device: Option<String>,
    grab: bool,
    describe: bool,
    monotonic: bool,
    /// `--query`: (type, code).
    query: Option<(u16, u16)>,
    count: Option<u64>,
    /// The deadline's length in ns; None = until killed.
    timeout_ns: Option<u64>,
}

/// A code by name (from the tables) or number.
fn code_of(t: &[(u16, &'static str)], s: &str) -> Option<u16> {
    s.parse().ok().or_else(|| t.iter().find(|e| e.1 == s).map(|e| e.0))
}

fn args() -> Args {
    let mut a = Args { dir: "/dev/input".into(), device: None, grab: false, describe: false, monotonic: false, query: None, count: None, timeout_ns: Some(10_000_000_000) };
    let (mut query, mut pos) = (false, Vec::new());
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        let mut val = |what: &str| it.next().unwrap_or_else(|| usage(&format!("{what} needs a value")));
        match s.as_str() {
            "--help" | "-h" => help(),
            "--dir" => a.dir = val("--dir"),
            "--grab" => a.grab = true,
            "--describe" => a.describe = true,
            "--monotonic" => a.monotonic = true,
            "--query" => query = true,
            "--count" => a.count = Some(val("--count").parse().unwrap_or_else(|_| usage("--count wants a number"))),
            "--timeout" => {
                let secs: u64 = val("--timeout").parse().unwrap_or_else(|_| usage("--timeout wants seconds"));
                // A deadline that does not fit would wrap into the past and end evtest at once.
                a.timeout_ns = match secs {
                    0 => None,
                    _ => Some(secs.checked_mul(1_000_000_000).filter(|ns| usys::mono_ns().checked_add(*ns).is_some()).unwrap_or_else(|| usage("--timeout too large"))),
                };
            }
            _ if s.starts_with('-') => usage(&format!("unknown option {s}")),
            _ => pos.push(s),
        }
    }
    if query {
        let [dev, ty, code] = <[String; 3]>::try_from(pos).unwrap_or_else(|_| usage("--query wants DEVICE TYPE CODE"));
        let ty = code_of(codes::EV, &ty).filter(|t| [1, 5, 0x11, 0x12].contains(t)).unwrap_or_else(|| usage("--query TYPE: EV_KEY, EV_LED, EV_SND or EV_SW"));
        let code = code_of(table(ty), &code).filter(|c| (*c as usize) <= type_max(ty)).unwrap_or_else(|| usage(&format!("--query: no such code {code}")));
        a.query = Some((ty, code));
        a.device = Some(dev);
        return a;
    }
    match pos.len() {
        0 => {}
        1 => a.device = pos.pop(),
        _ => usage("only one DEVICE"),
    }
    a
}

/// The state ioctl of a type: EVIOCGKEY 0x18, EVIOCGLED 0x19, EVIOCGSND 0x1a, EVIOCGSW 0x1b (input.h).
fn state(fd: usize, ty: u16) -> Result<Vec<u16>, usys::Errno> {
    let nr = match ty {
        1 => 0x18,
        0x11 => 0x19,
        0x12 => 0x1a,
        5 => 0x1b,
        _ => return Ok(Vec::new()),
    };
    bits(fd, nr, type_max(ty))
}

/// `--query`: 10 when the code is set, 0 when not (evtest.c `query_device`).
fn query(dev: &str, ty: u16, code: u16) -> i32 {
    let fd = match open(dev) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("evtest: {dev}: {}", e.name());
            return 1;
        }
    };
    let r = state(fd, ty);
    let _ = usys::close(fd);
    match r {
        Ok(set) => {
            if set.contains(&code) {
                10
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("evtest: query {dev}: {}", e.name());
            if e.0 == 25 { 4 } else { 1 }
        }
    }
}

/// After SYN_DROPPED: the keys, LEDs and switches set now (what a reader resyncs to).
fn resync_line(fd: usize) -> String {
    let part = |ty: u16, what: &str| match state(fd, ty) {
        Ok(v) if v.is_empty() => format!("{what} -"),
        Ok(v) => format!("{what} {}", v.iter().map(|c| name_in(table(ty), *c)).collect::<Vec<_>>().join(" ")),
        Err(e) => format!("{what} ({})", e.name()),
    };
    format!("  resync: {}; {}; {}", part(1, "keys down"), part(0x11, "leds on"), part(5, "switches on"))
}

fn open(path: &str) -> Result<usize, usys::Errno> {
    usys::open(path, O_NONBLOCK | O_CLOEXEC, 0)
}

/// A string ioctl (EVIOCGNAME/GPHYS/GUNIQ): the bytes up to the first NUL.
fn string(fd: usize, nr: u32) -> Result<String, usys::Errno> {
    let mut b = vec![0u8; 256];
    let n = ioctl_buf(fd, ioc(READ, nr, b.len()), &mut b)?;
    let n = n.min(b.len());
    let end = b[..n].iter().position(|&c| c == 0).unwrap_or(n);
    Ok(String::from_utf8_lossy(&b[..end]).into_owned())
}

fn bits(fd: usize, nr: u32, max: usize) -> Result<Vec<u16>, usys::Errno> {
    let mut b = vec![0u8; max / 8 + 1];
    ioctl_buf(fd, ioc(READ, nr, b.len()), &mut b)?;
    Ok((0..=max).filter(|i| b[i / 8] & (1 << (i % 8)) != 0).map(|i| i as u16).collect())
}

fn name_in(t: &[(u16, &'static str)], c: u16) -> &'static str {
    t.binary_search_by_key(&c, |e| e.0).map(|i| t[i].1).unwrap_or("?")
}

fn table(ty: u16) -> &'static [(u16, &'static str)] {
    match ty {
        0 => codes::SYN,
        1 => codes::KEY,
        2 => codes::REL,
        3 => codes::ABS,
        4 => codes::MSC,
        5 => codes::SW,
        0x11 => codes::LED,
        0x12 => codes::SND,
        0x14 => codes::REP,
        _ => &[],
    }
}

/// The highest code of a type (input-event-codes.h), for EVIOCGBIT.
fn type_max(ty: u16) -> usize {
    match ty {
        0 => 0xf,
        1 => KEY_MAX,
        2 => 0xf,
        3 => 0x3f,
        4 => 0x07,
        5 => 0x10,
        0x11 => 0x0f,
        0x12 => 0x07,
        0x14 => 0x01,
        0x15 => 0x7f,
        _ => 0,
    }
}

fn list(dir: &str) -> i32 {
    let mut names: Vec<(u32, String)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.strip_prefix("event").and_then(|d| d.parse::<u32>().ok()).map(|i| (i, n))
            })
            .collect(),
        Err(e) => {
            eprintln!("evtest: {dir}: {e}");
            return 1;
        }
    };
    names.sort();
    println!("Available devices:");
    for (_, n) in names {
        let path = format!("{dir}/{n}");
        match open(&path).and_then(|fd| {
            let r = string(fd, 0x06);
            let _ = usys::close(fd);
            r
        }) {
            Ok(name) => println!("{path}:\t{name}"),
            Err(e) => println!("{path}:\t({})", e.name()),
        }
    }
    0
}

fn describe(fd: usize) -> Result<(), usys::Errno> {
    let mut v = [0u8; 4];
    ioctl_buf(fd, ioc(READ, 0x01, 4), &mut v)?;
    let v = u32::from_le_bytes(v);
    println!("Input driver version is {}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff);
    let mut id = [0u8; 8];
    ioctl_buf(fd, ioc(READ, 0x02, 8), &mut id)?;
    let w = |i: usize| u16::from_le_bytes([id[i], id[i + 1]]);
    println!("Input device ID: bus {:#x} vendor {:#x} product {:#x} version {:#x}", w(0), w(2), w(4), w(6));
    println!("Input device name: \"{}\"", string(fd, 0x06)?);
    println!("Supported events:");
    for ty in bits(fd, 0x20, EV_MAX as usize)? {
        println!("  Event type {ty} ({})", name_in(codes::EV, ty));
        if ty == EV_SYN {
            continue;
        }
        if ty == EV_REP {
            // evtest.c `print_repdata`; a device without soft repeat answers ENOSYS (V2) and shows nothing.
            let mut r = [0u8; 8];
            if ioctl_buf(fd, ioc(READ, 0x03, 8), &mut r).is_ok() {
                for (i, v) in r.chunks_exact(4).enumerate() {
                    println!("    Repeat code {i} ({})", name_in(codes::REP, i as u16));
                    println!("      Value {:6}", u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
                }
            }
            continue;
        }
        for code in bits(fd, 0x20 + ty as u32, type_max(ty))? {
            println!("    Event code {code} ({})", name_in(table(ty), code));
            if ty == EV_ABS {
                let mut a = [0u8; 24];
                ioctl_buf(fd, ioc(READ, 0x40 + code as u32, 24), &mut a)?;
                let f = |i: usize| i32::from_le_bytes([a[4 * i], a[4 * i + 1], a[4 * i + 2], a[4 * i + 3]]);
                for (i, label) in ["Value", "Min", "Max", "Fuzz", "Flat", "Resolution"].iter().enumerate() {
                    if f(i) != 0 || i < 3 {
                        println!("      {label:<10} {:>6}", f(i));
                    }
                }
            }
        }
    }
    println!("Properties:");
    for p in bits(fd, 0x09, 0x1f)? {
        println!("  Property type {p} ({})", name_in(codes::PROP, p));
    }
    Ok(())
}

fn event_line(e: &[u8]) -> (String, bool) {
    let i64at = |o: usize| i64::from_le_bytes(e[o..o + 8].try_into().unwrap_or([0; 8]));
    let (sec, usec) = (i64at(0), i64at(8));
    let ty = u16::from_le_bytes([e[16], e[17]]);
    let code = u16::from_le_bytes([e[18], e[19]]);
    let value = i32::from_le_bytes([e[20], e[21], e[22], e[23]]);
    if ty == EV_SYN {
        let what = name_in(codes::SYN, code);
        return (format!("Event: time {sec}.{usec:06}, -------------- {what} ------------"), true);
    }
    let l = format!(
        "Event: time {sec}.{usec:06}, type {ty} ({}), code {code} ({}), value {value}",
        name_in(codes::EV, ty),
        name_in(table(ty), code)
    );
    (l, false)
}

const SYN_DROPPED: u16 = 3;

/// Reads events until the count or the deadline; Ok(true) when the count was reached (or none was asked).
/// `clock_bad` counts events whose time lies after the monotonic clock's now (with `--monotonic`).
fn read_events(fd: usize, count: Option<u64>, timeout_ns: Option<u64>, monotonic: bool, clock_bad: &mut u64) -> Result<bool, usys::Errno> {
    let end = timeout_ns.map(|ns| usys::mono_ns().saturating_add(ns));
    let mut seen = 0u64;
    let mut buf = vec![0u8; EVENT_SIZE * 64];
    let mut out = std::io::stdout().lock();
    use std::io::Write;
    loop {
        if count.is_some_and(|c| seen >= c) {
            return Ok(true);
        }
        let wait_ms = match end {
            Some(e) => {
                let now = usys::mono_ns();
                if now >= e {
                    return Ok(count.is_none());
                }
                ((e - now) / 1_000_000).clamp(1, 1000) as i32
            }
            None => 1000,
        };
        let mut p = [usys::PollFd { fd: fd as i32, events: usys::POLLIN, revents: 0 }];
        match usys::poll(&mut p, wait_ms) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(usys::EINTR) => continue,
            Err(e) => return Err(e),
        }
        if p[0].revents & (usys::POLLERR | usys::POLLHUP | usys::POLLNVAL) != 0 && p[0].revents & usys::POLLIN == 0 {
            return Err(usys::Errno(19)); // ENODEV: the device went away (EVIOCREVOKE or unplug)
        }
        let n = match usys::read(fd, &mut buf) {
            Ok(n) => n,
            Err(usys::EAGAIN) | Err(usys::EINTR) => continue,
            Err(e) => return Err(e),
        };
        let now = usys::mono_ns();
        for e in buf[..n].chunks_exact(EVENT_SIZE) {
            let (l, syn) = event_line(e);
            let _ = writeln!(out, "{l}");
            if monotonic {
                let i64at = |o: usize| i64::from_le_bytes(e[o..o + 8].try_into().unwrap_or([0; 8]));
                let t = (i64at(0) as u64).saturating_mul(1_000_000_000).saturating_add(i64at(8) as u64 * 1000);
                if t > now {
                    *clock_bad += 1;
                    let _ = writeln!(out, "  (event time after the monotonic clock's now)");
                }
            }
            if syn && u16::from_le_bytes([e[18], e[19]]) == SYN_DROPPED {
                let _ = writeln!(out, "{}", resync_line(fd));
            }
            if !syn {
                seen += 1;
                if count.is_some_and(|c| seen >= c) {
                    break;
                }
            }
        }
        let _ = out.flush();
    }
}

fn main() {
    // `evtest ... | head` ends quietly, as the C evtest does (Rust's runtime ignores SIGPIPE).
    let _ = usys::rt_sigaction_simple(13, false);
    let a = args();
    let Some(dev) = a.device.clone() else { exit(list(&a.dir)) };
    if let Some((ty, code)) = a.query {
        exit(query(&dev, ty, code));
    }
    let fd = match open(&dev) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("evtest: {dev}: {}", e.name());
            exit(1)
        }
    };
    if let Err(e) = describe(fd) {
        eprintln!("evtest: {dev}: {}", e.name());
        exit(if e.0 == 25 { 4 } else { 1 });
    }
    if a.describe {
        exit(0);
    }
    if a.monotonic {
        // EVIOCSCLOCKID = _IOW('E', 0xa0, int): the kernel reads the clock id from the buffer.
        let mut id = 1i32.to_le_bytes();
        if let Err(e) = ioctl_buf(fd, ioc(1, 0xa0, 4), &mut id) {
            eprintln!("evtest: EVIOCSCLOCKID {dev}: {}", e.name());
            exit(1);
        }
    }
    if a.grab {
        if let Err(e) = ioctl_val(fd, EVIOCGRAB, 1) {
            eprintln!("evtest: grab {dev}: {}", e.name());
            exit(1);
        }
        println!("Grabbed.");
    }
    println!("Testing ... (interrupt to exit)");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut clock_bad = 0;
    let r = read_events(fd, a.count, a.timeout_ns, a.monotonic, &mut clock_bad);
    if a.grab {
        let _ = ioctl_val(fd, EVIOCGRAB, 0);
    }
    let _ = usys::close(fd);
    if clock_bad > 0 {
        eprintln!("evtest: {clock_bad} event times after the monotonic clock's now");
        exit(1);
    }
    match r {
        Ok(true) => exit(0),
        Ok(false) => {
            eprintln!("evtest: timeout before {} events", a.count.unwrap_or(0));
            exit(3)
        }
        Err(e) => {
            eprintln!("evtest: read {dev}: {}", e.name());
            exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(ioc(READ, 0x01, 4), 0x8004_4501); // EVIOCGVERSION
        assert_eq!(ioc(READ, 0x06, 256), 0x8100_4506); // EVIOCGNAME(256)
        assert_eq!(ioc(READ, 0x40, 24), 0x8018_4540); // EVIOCGABS(ABS_X)
        assert_eq!(ioc(1, 0x90, 4), EVIOCGRAB);
    }

    #[test]
    fn lines() {
        let mut e = [0u8; 24];
        e[0] = 5;
        e[8] = 7;
        e[16] = 1;
        e[18] = 30;
        e[20] = 1;
        assert_eq!(event_line(&e).0, "Event: time 5.000007, type 1 (EV_KEY), code 30 (KEY_A), value 1");
        let s = [0u8; 24];
        assert_eq!(event_line(&s).0, "Event: time 0.000000, -------------- SYN_REPORT ------------");
        assert_eq!(name_in(codes::KEY, 0x110), "BTN_LEFT");
        assert_eq!(name_in(codes::ABS, 0x35), "ABS_MT_POSITION_X");
    }
}
