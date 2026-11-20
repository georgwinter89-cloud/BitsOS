//! GM-6c: what the quick menu reads and asks outside bitscomp (plan/spec/sofa-mode.md §7), on a thread of its own
//! (B4: the main loop never waits for a device or a socket; the shell draws from the cached `Snap`): the volume
//! through the ALSA control client (alsa_ctl.rs; never a PCM), which outputs exist (`/sys/bitsos/snd_output_status`
//! and the card's "Headphone Jack"), the HID batteries (`/sys/class/power_supply/hid-*-battery/{capacity,status}`,
//! as Linux's hid-input names them, GM-6p; each with its `device/uevent` HID_PHYS, which the main loop matches to a
//! pad's evdev phys) and the power request to init's `/run/bitsos/power.sock` (GM-6w: one JSON line, one answer
//! line, 2 s deadline; on a thread of its own, so a stuck connect never holds up the volume). The real paths are used on BitsOS; a host test
//! takes only the `BITSCOMP_*` test paths (`Paths::of`), so it never touches the build host's sound card.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::alsa_ctl::{self as alsa, Ctl};

pub const POWER_SOCKET: &str = "/run/bitsos/power.sock";
pub const SND_STATUS: &str = "/sys/bitsos/snd_output_status";
pub const SUPPLY: &str = "/sys/class/power_supply";
pub const DEADLINE: Duration = Duration::from_secs(2);
/// A sysfs value or an answer line at most (R1: per input).
const SMALL: u64 = 4096;

/// Where the card is: a directory of `controlC<n>` or the scenario's fake file.
#[derive(Clone, Debug, PartialEq)]
pub enum Card {
    Dir(PathBuf),
    Fake(PathBuf),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Paths {
    pub card: Option<Card>,
    pub status: Option<PathBuf>,
    pub supply: Option<PathBuf>,
    pub power: Option<PathBuf>,
}

impl Paths {
    /// The `BITSCOMP_*` paths, else the real ones when `real` (bitscomp runs on BitsOS's screen).
    pub fn of(real: bool, env: impl Fn(&str) -> Option<String>) -> Paths {
        let p = |k: &str, d: &str| env(k).map(PathBuf::from).or_else(|| real.then(|| PathBuf::from(d)));
        let card = match env("BITSCOMP_ALSA_FAKE") {
            Some(f) => Some(Card::Fake(f.into())),
            None => p("BITSCOMP_ALSA_DIR", "/dev/snd").map(Card::Dir),
        };
        Paths { card, status: p("BITSCOMP_SND_STATUS", SND_STATUS), supply: p("BITSCOMP_POWER_SUPPLY", SUPPLY), power: p("BITSCOMP_POWER_SOCKET", POWER_SOCKET) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Battery {
    pub capacity: Option<u8>,
    /// Linux's word: Charging, Discharging, Full, Not charging, Unknown.
    pub status: String,
}

/// What the thread read last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snap {
    pub volume: Option<u8>,
    /// Why there is no volume (no card, no master control), for the log and `ctl sofa state`.
    pub why: Option<String>,
    /// The outputs that exist: "hdmi", "jack", "usb" (kernel.snd_output's values).
    pub outputs: Vec<&'static str>,
    /// The port the mixer plays to now ("hdmi", "jack", "usb"), when it plays.
    pub playing: Option<String>,
    /// Every HID battery with its HID_PHYS (a keyboard's or a headset's too; `pad_battery` picks the pad's).
    pub batteries: Vec<(String, Battery)>,
    /// sofa.start_volume lowered the volume: (from, to).
    pub lowered: Option<(u8, u8)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Ask,
    Volume(u8),
    /// sofa.start_volume: down to this when it is higher (never up).
    Lower(u8),
    Power { op: &'static str, physical: bool },
}

pub enum Msg {
    Snap(Snap),
    /// The power request's answer as the German line for the screen; true when init goes down.
    Power(String, bool),
}

/// The ports in `/sys/bitsos/snd_output_status`'s line ("mixer=running playing=<dev>/hdmi/... clients=1 hdmi=on"):
/// HDMI when the line's hdmi is not "absent", and the playing port.
pub fn parse_status(line: &str) -> (bool, Option<String>) {
    let word = |k: &str| line.split_whitespace().find_map(|w| w.strip_prefix(k).map(str::to_string));
    let hdmi = word("hdmi=").is_some_and(|h| h != "absent");
    let playing = word("playing=").filter(|p| p != "none").and_then(|p| p.split('/').nth(1).map(str::to_string));
    (hdmi, playing)
}

/// The outputs in kernel.snd_output's order of the board's chips: HDMI (Fernseher), the jack, USB.
pub fn outputs(hdmi: bool, jack: Option<bool>, playing: Option<&str>) -> Vec<&'static str> {
    let mut v = Vec::new();
    if hdmi || playing == Some("hdmi") {
        v.push("hdmi");
    }
    if jack == Some(true) || playing == Some("jack") {
        v.push("jack");
    }
    // No file says a USB sound device is there while nothing plays to it (change request SN, notes).
    if playing == Some("usb") {
        v.push("usb");
    }
    v
}

fn small(p: &Path) -> Option<String> {
    let mut s = String::new();
    std::fs::File::open(p).ok()?.take(SMALL).read_to_string(&mut s).ok()?;
    Some(s.trim().to_string())
}

/// Linux's HID batteries `hid-<uniq or dev_name>-battery`, each with the HID_PHYS of its `device/uevent` (the HID
/// device's line of hid-core.c `hid_uevent`; empty when unreadable, so it belongs to no pad).
pub fn batteries(dir: &Path) -> Vec<(String, Battery)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("hid-") && n.ends_with("-battery"))).collect();
    names.sort();
    names
        .iter()
        .map(|p| {
            let capacity = small(&p.join("capacity")).and_then(|c| c.parse::<u8>().ok()).filter(|c| *c <= 100);
            let status = small(&p.join("status")).unwrap_or_else(|| "Unknown".into());
            let phys = small(&p.join("device/uevent")).and_then(|u| u.lines().find_map(|l| l.strip_prefix("HID_PHYS=").map(str::to_string))).unwrap_or_default();
            (phys, Battery { capacity, status })
        })
        .collect()
}

/// The battery of a connected pad: the one whose HID_PHYS is a pad's evdev phys (GM-6p: the node name's number
/// changes at every replug, never a key). A pad without phys or without a matching battery has none ("Kabel").
pub fn pad_battery<'a>(pads: &[&str], list: &'a [(String, Battery)]) -> Option<&'a Battery> {
    list.iter().find(|(phys, _)| !phys.is_empty() && pads.contains(&phys.as_str())).map(|(_, b)| b)
}

/// The request line of GM-6w; `input` only when a real press asked for it (init and the kernel check that too).
pub fn power_line(op: &str, physical: bool) -> String {
    let mut v = vec![("op", Json::str(op))];
    if physical {
        v.push(("input", Json::str("physical")));
    }
    let mut s = String::new();
    Json::obj(v).write_to(&mut s);
    s.push('\n');
    s
}

/// The German line for init's answer ({"ok":true,...} or {"ok":false,"refused":"..."}); true: it goes down.
pub fn power_answer(op: &str, line: &str) -> (String, bool) {
    let j = Json::parse(line.trim()).ok();
    match j.as_ref().and_then(|j| j.get("ok")).and_then(Json::as_bool) {
        Some(true) => (if op == "reboot" { "BitsOS startet neu …".into() } else { "BitsOS schaltet aus …".into() }, true),
        Some(false) => {
            let why = j.as_ref().and_then(|j| j.get("refused")).and_then(Json::as_str).unwrap_or("ohne Grund");
            (format!("Abgelehnt vom Startdienst: {why}"), false)
        }
        None => ("Der Startdienst antwortete unverständlich.".into(), false),
    }
}

/// The line before the socket is there (GM-6w not on this BitsOS yet).
pub fn power_later(op: &str) -> &'static str {
    if op == "reboot" { "Neustart aus dem Sofa-Modus kommt noch" } else { "Ausschalten aus dem Sofa-Modus kommt noch" }
}

/// REWIND: the same request from the rewind view's restart (snap_client's thread).
pub fn power_request(p: &Path, op: &str, physical: bool) -> (String, bool) {
    power(p, op, physical)
}

fn power(p: &Path, op: &str, physical: bool) -> (String, bool) {
    let s = match UnixStream::connect(p) {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return (power_later(op).into(), false),
        Err(e) => return (format!("Der Startdienst ist nicht erreichbar: {e}"), false),
    };
    let end = Instant::now() + DEADLINE;
    let _ = s.set_write_timeout(Some(DEADLINE));
    if let Err(e) = (&s).write_all(power_line(op, physical).as_bytes()) {
        return (format!("Der Startdienst nahm nichts an: {e}"), false);
    }
    let mut line = Vec::new();
    let left = end.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let _ = s.set_read_timeout(Some(left));
    match BufReader::new(&s).take(SMALL).read_until(b'\n', &mut line) {
        Ok(_) if line.ends_with(b"\n") => power_answer(op, &String::from_utf8_lossy(&line)),
        Ok(_) => ("Der Startdienst antwortete nicht.".into(), false),
        Err(e) => (format!("Der Startdienst antwortete nicht: {e}"), false),
    }
}

fn open_card(c: &Card) -> Result<Box<dyn Ctl>, String> {
    match c {
        Card::Fake(f) => Ok(Box::new(alsa::FileEmu::open(f))),
        Card::Dir(d) => {
            let mut last = format!("no control device in {}", d.display());
            for p in alsa::cards(d) {
                match alsa::Dev::open(&p) {
                    Ok(mut dev) => match alsa::volume(&mut dev) {
                        Ok(_) => return Ok(Box::new(dev)),
                        Err(e) => last = format!("{}: {e}", p.display()),
                    },
                    Err(e) => last = e,
                }
            }
            Err(last)
        }
    }
}

/// One job's reading (and change) of the card and the files.
pub fn snap(paths: &Paths, job: &Job) -> Snap {
    let mut s = Snap::default();
    let mut jack = None;
    match paths.card.as_ref().ok_or_else(|| "no sound card here (not on BitsOS)".to_string()).and_then(open_card) {
        Ok(mut c) => {
            let r = match job {
                Job::Volume(p) => alsa::set_volume(c.as_mut(), *p),
                Job::Lower(to) => alsa::volume(c.as_mut()).and_then(|(_, now)| {
                    if now > *to {
                        let got = alsa::set_volume(c.as_mut(), *to)?;
                        s.lowered = Some((now, got));
                        Ok(got)
                    } else {
                        Ok(now)
                    }
                }),
                _ => alsa::volume(c.as_mut()).map(|(_, p)| p),
            };
            match r {
                Ok(p) => s.volume = Some(p),
                Err(e) => s.why = Some(e),
            }
            jack = alsa::jack(c.as_mut());
        }
        Err(e) => s.why = Some(e),
    }
    let (hdmi, playing) = paths.status.as_deref().and_then(small).map_or((false, None), |l| parse_status(&l));
    s.outputs = outputs(hdmi, jack, playing.as_deref());
    s.playing = playing;
    s.batteries = paths.supply.as_deref().map(batteries).unwrap_or_default();
    s
}

/// The thread; each job's answer goes to `came` on the main loop.
pub fn start<D: 'static>(handle: &LoopHandle<'static, D>, paths: Paths, came: impl Fn(&mut D, Msg) + 'static) -> Result<mpsc::Sender<Job>, String> {
    let (tx, rx) = channel::channel::<Msg>();
    let (jtx, jrx) = mpsc::channel::<Job>();
    handle
        .insert_source(rx, move |ev, _, d| {
            if let channel::Event::Msg(m) = ev {
                came(d, m);
            }
        })
        .map_err(|e| format!("sofa io channel: {e}"))?;
    std::thread::Builder::new().name("bitscomp-sofa-io".into()).spawn(move || worker(paths, tx, jrx)).map_err(|e| format!("sofa io thread: {e}"))?;
    Ok(jtx)
}

fn worker(paths: Paths, tx: Sender<Msg>, jobs: mpsc::Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        let m = match &job {
            Job::Power { op, physical } => {
                let (tx, p, op, physical) = (tx.clone(), paths.power.clone(), *op, *physical);
                // A connect to a socket whose peer is stuck can wait without end; on its own thread it holds up
                // only itself, never the volume and snapshot jobs behind it.
                let run = move || {
                    let (text, down) = p.as_deref().map_or_else(|| (power_later(op).into(), false), |p| power(p, op, physical));
                    let _ = tx.send(Msg::Power(text, down));
                };
                if let Err(e) = std::thread::Builder::new().name("bitscomp-sofa-power".into()).spawn(run) {
                    Msg::Power(format!("Der Startdienst ist nicht erreichbar: {e}"), false)
                } else {
                    continue;
                }
            }
            j => Msg::Snap(snap(&paths, j)),
        };
        if tx.send(m).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitscomp-sofa-io-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_real_paths_only_on_bitsos() {
        let none = |_: &str| None;
        assert_eq!(Paths::of(false, none), Paths::default());
        let real = Paths::of(true, none);
        assert_eq!(real.card, Some(Card::Dir("/dev/snd".into())));
        assert_eq!(real.power.as_deref(), Some(Path::new(POWER_SOCKET)));
        let fake = Paths::of(false, |k: &str| (k == "BITSCOMP_ALSA_FAKE").then(|| "/x/ctl".to_string()));
        assert_eq!(fake.card, Some(Card::Fake("/x/ctl".into())));
        assert_eq!(fake.status, None);
    }

    #[test]
    fn the_status_line_names_hdmi_and_the_playing_port() {
        assert_eq!(parse_status("mixer=running playing=hda0/hdmi/0x0:0x3 clients=1 hdmi=on"), (true, Some("hdmi".into())));
        assert_eq!(parse_status("mixer=idle playing=none clients=0 hdmi=absent"), (false, None));
        assert_eq!(parse_status(""), (false, None));
        assert_eq!(outputs(true, Some(true), None), vec!["hdmi", "jack"]);
        assert_eq!(outputs(false, None, Some("usb")), vec!["usb"]);
        assert!(outputs(false, Some(false), None).is_empty());
    }

    #[test]
    fn the_hid_battery_is_read_as_linux_names_it_and_only_the_pads_counts() {
        let d = tmp("bat");
        assert!(batteries(&d).is_empty());
        std::fs::create_dir_all(d.join("BAT0")).unwrap();
        std::fs::write(d.join("BAT0/capacity"), "55\n").unwrap();
        assert!(batteries(&d).is_empty(), "a laptop's battery is not a HID one");
        let pad = d.join("hid-0003:045E:028E.0002-battery");
        std::fs::create_dir_all(pad.join("device")).unwrap();
        std::fs::write(pad.join("capacity"), "80\n").unwrap();
        std::fs::write(pad.join("status"), "Discharging\n").unwrap();
        std::fs::write(pad.join("device/uevent"), "HID_ID=0003:0000045E:0000028E\nHID_PHYS=usb-0000:00:14.0-2/input0\nHID_UNIQ=\n").unwrap();
        // A wireless mouse's battery, first by name: it is never the pad's.
        let mouse = d.join("hid-0003:046D:C52B.0001-battery");
        std::fs::create_dir_all(mouse.join("device")).unwrap();
        std::fs::write(mouse.join("capacity"), "30\n").unwrap();
        std::fs::write(mouse.join("device/uevent"), "HID_PHYS=usb-0000:00:14.0-3/input2\n").unwrap();
        let list = batteries(&d);
        assert_eq!(list.len(), 2);
        let b80 = Battery { capacity: Some(80), status: "Discharging".into() };
        assert_eq!(pad_battery(&["usb-0000:00:14.0-2/input0"], &list), Some(&b80));
        assert_eq!(pad_battery(&["usb-0000:00:14.0-1/input0"], &list), None, "a wired pad beside a mouse with a battery: Kabel");
        assert_eq!(pad_battery(&[""], &list), None, "a pad without phys");
        std::fs::write(pad.join("capacity"), "garbage").unwrap();
        assert_eq!(pad_battery(&["usb-0000:00:14.0-2/input0"], &batteries(&d)).unwrap().capacity, None);
        std::fs::remove_file(pad.join("device/uevent")).unwrap();
        assert_eq!(pad_battery(&[""], &batteries(&d)), None, "no uevent: no pad's");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn power_requests_and_answers() {
        assert_eq!(power_line("reboot", true), "{\"op\":\"reboot\",\"input\":\"physical\"}\n");
        assert_eq!(power_line("poweroff", false), "{\"op\":\"poweroff\"}\n");
        assert_eq!(power_answer("poweroff", "{\"ok\":true,\"op\":\"poweroff\"}\n"), ("BitsOS schaltet aus …".into(), true));
        assert_eq!(power_answer("reboot", "{\"ok\":true,\"op\":\"reboot\"}"), ("BitsOS startet neu …".into(), true));
        let (t, down) = power_answer("reboot", "{\"ok\":false,\"refused\":\"no grant\"}");
        assert!(!down && t.ends_with("no grant"), "{t}");
        assert!(!power_answer("reboot", "nonsense").1);
        let d = tmp("power");
        assert_eq!(power(&d.join("power.sock"), "reboot", true), (power_later("reboot").into(), false));
        assert_eq!(power_later("poweroff"), "Ausschalten aus dem Sofa-Modus kommt noch");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_power_request_reaches_the_socket_and_its_answer_comes_back() {
        let d = tmp("power2");
        let p = d.join("power.sock");
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            (&s).write_all(b"{\"ok\":true,\"op\":\"poweroff\"}\n").unwrap();
            line
        });
        assert_eq!(power(&p, "poweroff", true), ("BitsOS schaltet aus …".into(), true));
        assert_eq!(t.join().unwrap(), "{\"op\":\"poweroff\",\"input\":\"physical\"}\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_snapshot_lowers_only_and_reads_the_fake_card() {
        let d = tmp("snap");
        let f = d.join("ctl.txt");
        std::fs::write(&f, "Master Playback Volume=19661,19661\nHeadphone Jack=1\n").unwrap();
        std::fs::write(d.join("status"), "mixer=running playing=hda0/hdmi/0:3 clients=1 hdmi=on\n").unwrap();
        let paths = Paths { card: Some(Card::Fake(f.clone())), status: Some(d.join("status")), supply: Some(d.clone()), power: None };
        let s = snap(&paths, &Job::Ask);
        assert_eq!((s.volume, s.outputs.clone(), s.playing.as_deref(), s.batteries.len()), (Some(60), vec!["hdmi", "jack"], Some("hdmi"), 0));
        let s = snap(&paths, &Job::Lower(20));
        assert_eq!((s.volume, s.lowered), (Some(20), Some((60, 20))));
        let before = std::fs::read_to_string(&f).unwrap();
        let s = snap(&paths, &Job::Lower(50));
        assert_eq!((s.volume, s.lowered), (Some(20), None), "never up");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), before, "a start volume above the volume writes nothing");
        assert_eq!(snap(&paths, &Job::Volume(35)).volume, Some(35));
        let none = snap(&Paths::default(), &Job::Ask);
        assert!(none.volume.is_none() && none.why.is_some());
        let none = snap(&Paths::default(), &Job::Lower(20));
        assert!(none.volume.is_none() && none.lowered.is_none(), "no card (61/62, the build host): nothing lowered, no event");
        let _ = std::fs::remove_dir_all(&d);
    }
}
