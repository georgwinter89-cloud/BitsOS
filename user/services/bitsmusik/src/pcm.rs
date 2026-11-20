//! The kernel's ALSA PCM `/dev/snd/pcmC0D0p` (hw:0, card 0 = BitsOS's mixer: it always exists and plays into
//! the mixer's null output when nothing is connected; crates/snd/src/alsa). Raw ioctls as libasound's hw plugin
//! does (HW_PARAMS, PREPARE, DELAY, DROP; layouts of linux-7.3.1/include/uapi/sound/asound.h, x86-64), samples by
//! write(2) (`snd_pcm_write`). FLOAT_LE interleaved, 1 or 2 channels, the source's rate when hw:0 takes it, else
//! 48 kHz. Non-blocking: a full device makes `write` wait in short sleeps with a deadline (B4), an XRUN (pause,
//! a stall) is prepared away. Which output plays (HDMI, jack, USB) is the kernel's choice (`kernel.snd_output`);
//! the player never opens a control device and never sets a volume.

use crate::events::{Events, Level};
use crate::sink::Sink;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

pub const NODE: &str = "/dev/snd/pcmC0D0p";
pub const OUTPUT_STATUS: &str = "/sys/bitsos/snd_output_status";
/// The kernel's output choice (`kernel.snd_output`: auto, jack, hdmi, usb or fixed:DEV/PORT).
pub const OUTPUT_TARGET: &str = "/sys/bitsos/snd_output";

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | ((b'A' as u32) << 8) | nr
}
const HW_PARAMS_SIZE: usize = 608;
const HW_PARAMS: u32 = ioc(3, 0x11, HW_PARAMS_SIZE as u32);
const DELAY: u32 = ioc(2, 0x21, 8);
const PREPARE: u32 = 0x4140;
const DROP: u32 = 0x4143;

const ACCESS_RW_INTERLEAVED: u32 = 3;
const FORMAT_FLOAT_LE: u32 = 14;
// hw_params parameters (asound.h SNDRV_PCM_HW_PARAM_*): masks 0..2 at byte 4 (32 bytes each), intervals 8..19
// at byte 260 (min, max, flags: 12 bytes each).
const P_ACCESS: usize = 0;
const P_FORMAT: usize = 1;
const P_CHANNELS: usize = 10;
const P_RATE: usize = 11;
const P_PERIOD_SIZE: usize = 13;
const P_BUFFER_SIZE: usize = 17;
const EAGAIN: i32 = 11;
const EPIPE: i32 = 32;
const EBADFD: i32 = 77;
const ESTRPIPE: i32 = 86;

/// How long a write may wait for room before the output counts as hung (B4).
const WRITE_DEADLINE: Duration = Duration::from_secs(3);

/// `_snd_pcm_hw_params_any` with access, format, channels, rate, period and buffer fixed.
fn hw_params(rate: u32, channels: u32) -> [u8; HW_PARAMS_SIZE] {
    let mut b = [0u8; HW_PARAMS_SIZE];
    let put = |b: &mut [u8; HW_PARAMS_SIZE], at: usize, v: u32| {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes())
    };
    for k in 0..3 {
        for w in 0..8 {
            put(&mut b, 4 + k * 32 + w * 4, !0);
        }
    }
    for k in 8..=19 {
        put(&mut b, 260 + (k - 8) * 12 + 4, !0);
    }
    let mask1 = |b: &mut [u8; HW_PARAMS_SIZE], k: usize, v: u32| {
        for w in 0..8 {
            put(
                b,
                4 + k * 32 + w * 4,
                if w == (v / 32) as usize {
                    1 << (v % 32)
                } else {
                    0
                },
            );
        }
    };
    let fix = |b: &mut [u8; HW_PARAMS_SIZE], k: usize, v: u32| {
        let at = 260 + (k - 8) * 12;
        put(b, at, v);
        put(b, at + 4, v);
        put(b, at + 8, 4); // integer
    };
    mask1(&mut b, P_ACCESS, ACCESS_RW_INTERLEAVED);
    mask1(&mut b, P_FORMAT, FORMAT_FLOAT_LE);
    fix(&mut b, P_CHANNELS, channels);
    fix(&mut b, P_RATE, rate);
    // 20 ms periods, 8 of them (160 ms queued): short enough for pause and next to answer at once.
    let period = (rate / 50).max(64);
    fix(&mut b, P_PERIOD_SIZE, period);
    fix(&mut b, P_BUFFER_SIZE, period * 8);
    put(&mut b, 512, !0); // rmask
    put(&mut b, 520, !0); // info
    b
}

pub struct Alsa {
    ev: Events,
    file: Option<File>,
    channels: u32,
    written: u64,
}

impl Alsa {
    pub fn new(ev: Events) -> Alsa {
        Alsa {
            ev,
            file: None,
            channels: 2,
            written: 0,
        }
    }

    fn fd(&self) -> Option<usize> {
        self.file.as_ref().map(|f| f.as_raw_fd() as usize)
    }

    fn delay(&self) -> Option<u64> {
        let fd = self.fd()?;
        let mut d = [0u8; 8];
        usys::dev::ioctl_buf(fd, DELAY, &mut d).ok()?;
        Some(i64::from_le_bytes(d).max(0) as u64)
    }

    fn prepare(&mut self) -> Result<(), String> {
        let fd = self.fd().ok_or("not open")?;
        usys::dev::ioctl_none(fd, PREPARE)
            .map(|_| ())
            .map_err(|e| format!("prepare {e}"))
    }
}

impl Sink for Alsa {
    fn open(&mut self, rate: u32, channels: u32) -> Result<u32, String> {
        self.close();
        let channels = channels.clamp(1, 2);
        let f = OpenOptions::new()
            .write(true)
            .custom_flags(0o4000) // O_NONBLOCK
            .open(NODE)
            .map_err(|e| format!("{NODE}: {e}"))?;
        let fd = f.as_raw_fd() as usize;
        let rate = negotiate(rate, &self.ev, |r| {
            let mut p = hw_params(r, channels);
            usys::dev::ioctl_buf(fd, HW_PARAMS, &mut p)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .map_err(|e| format!("{e} (channels={channels})"))?;
        self.file = Some(f);
        self.channels = channels;
        self.written = 0;
        self.prepare()?;
        Ok(rate)
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let frame = 4 * self.channels as usize;
        let ev = self.ev.clone();
        let mut written = self.written;
        let r = write_all(self, &bytes, frame, &mut written, WRITE_DEADLINE, &ev);
        self.written = written;
        r
    }

    fn played(&mut self) -> u64 {
        self.written.saturating_sub(self.delay().unwrap_or(0))
    }

    fn drop_queued(&mut self) {
        self.written = self.played();
        if let Some(fd) = self.fd() {
            let _ = usys::dev::ioctl_none(fd, DROP);
        }
    }

    fn drain(&mut self, max: Duration) {
        let end = Instant::now() + max;
        while Instant::now() < end && self.delay().is_some_and(|d| d > 0) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn close(&mut self) {
        if let Some(fd) = self.fd() {
            let _ = usys::dev::ioctl_none(fd, DROP);
        }
        self.file = None;
        self.written = 0;
    }

    fn output(&self) -> String {
        output_word(&std::fs::read_to_string(OUTPUT_STATUS).unwrap_or_default())
    }

    fn target(&self) -> String {
        target_word(&std::fs::read_to_string(OUTPUT_TARGET).unwrap_or_default())
    }
}

/// The port of the kernel's output choice (`hdmi`, `jack`, `usb`; `fixed:DEV/PORT` gives PORT), else `auto`.
pub fn target_word(target: &str) -> String {
    let t = target.trim();
    let port = t
        .strip_prefix("fixed:")
        .map_or(t, |f| f.rsplit('/').next().unwrap_or(""));
    match port {
        "hdmi" | "jack" | "usb" => port.to_string(),
        _ => "auto".to_string(),
    }
}

/// The device side of [`write_all`]: the PCM fd (a fake in the tests).
pub trait Dev {
    fn put(&mut self, b: &[u8]) -> std::io::Result<usize>;
    fn queued(&self) -> Option<u64>;
    fn recover(&mut self) -> Result<(), String>;
}

impl Dev for Alsa {
    fn put(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self.file.as_mut() {
            Some(f) => f.write(b),
            None => Err(std::io::Error::from_raw_os_error(EBADFD)),
        }
    }
    fn queued(&self) -> Option<u64> {
        self.delay()
    }
    fn recover(&mut self) -> Result<(), String> {
        self.prepare()
    }
}

/// The file's rate when hw:0 takes it, else 48 kHz (the player resamples to the rate answered).
pub fn negotiate(
    rate: u32,
    ev: &Events,
    mut try_rate: impl FnMut(u32) -> Result<(), String>,
) -> Result<u32, String> {
    for r in [rate, 48_000] {
        match try_rate(r) {
            Ok(()) => return Ok(r),
            Err(e) => ev.emit(
                Level::Warn,
                "pcm.params_refused",
                &format!("rate={r} error={}", crate::events::q(&e)),
            ),
        }
    }
    Err("hw_params refused".into())
}

/// Writes all of `bytes` (whole frames of `frame` bytes): a full device is waited on in short sleeps until
/// `deadline` passes without progress (B4), an XRUN is prepared away and the write goes on.
pub fn write_all(
    dev: &mut impl Dev,
    bytes: &[u8],
    frame: usize,
    written: &mut u64,
    deadline: Duration,
    ev: &Events,
) -> Result<(), String> {
    let mut at = 0usize;
    let mut since = Instant::now();
    while at < bytes.len() {
        match dev.put(&bytes[at..]) {
            Ok(0) => {
                if since.elapsed() > deadline {
                    return Err(format!("no room for {} ms", deadline.as_millis()));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(n) => {
                at += n;
                *written += (n / frame.max(1)) as u64;
                since = Instant::now();
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => match e.raw_os_error() {
                Some(EAGAIN) => {
                    if since.elapsed() > deadline {
                        return Err(format!("no room for {} ms", deadline.as_millis()));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Some(EPIPE | ESTRPIPE | EBADFD) => {
                    // A device that runs over again and again without taking data counts as hung too.
                    if since.elapsed() > deadline {
                        return Err(format!("xrun for {} ms", deadline.as_millis()));
                    }
                    ev.emit(Level::Info, "pcm.xrun", &format!("error={e}"));
                    *written = written.saturating_sub(dev.queued().unwrap_or(0));
                    dev.recover()?;
                }
                _ => return Err(format!("write: {e}")),
            },
        }
    }
    Ok(())
}

/// `playing=` of the kernel's output status line (`mixer=.. playing=hdmi clients=.. hdmi=..`).
pub fn output_word(status: &str) -> String {
    status
        .split_whitespace()
        .find_map(|w| w.strip_prefix("playing="))
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn request_numbers_and_layout() {
        // asound.h on x86-64: _IOWR('A', 0x11, struct snd_pcm_hw_params) and _IOR('A', 0x21, snd_pcm_sframes_t).
        assert_eq!((HW_PARAMS, DELAY), (0xc260_4111, 0x8008_4121));
        let b = hw_params(44100, 2);
        assert_eq!(u32_at(&b, 4), 1 << ACCESS_RW_INTERLEAVED);
        assert_eq!(u32_at(&b, 4 + 32), 1 << FORMAT_FLOAT_LE);
        let iv = |k: usize| {
            (
                u32_at(&b, 260 + (k - 8) * 12),
                u32_at(&b, 264 + (k - 8) * 12),
                u32_at(&b, 268 + (k - 8) * 12),
            )
        };
        assert_eq!(iv(P_RATE), (44100, 44100, 4));
        assert_eq!(iv(P_CHANNELS), (2, 2, 4));
        assert_eq!(iv(P_PERIOD_SIZE), (882, 882, 4));
        assert_eq!(iv(P_BUFFER_SIZE), (882 * 8, 882 * 8, 4));
        assert_eq!(iv(8), (0, !0, 0));
    }

    /// A device by script: each `put` takes the next answer (`Some(n)` bytes taken, `None` an error number).
    /// After the script, every `put` answers `rest`.
    struct Fake {
        answers: std::collections::VecDeque<Result<usize, i32>>,
        rest: Result<usize, i32>,
        recovered: usize,
        taken: usize,
    }
    fn fake(answers: Vec<Result<usize, i32>>, rest: Result<usize, i32>) -> Fake {
        Fake {
            answers: answers.into(),
            rest,
            recovered: 0,
            taken: 0,
        }
    }
    impl Dev for Fake {
        fn put(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let a = self.answers.pop_front().unwrap_or(self.rest);
            match a {
                Ok(n) => {
                    let n = n.min(b.len());
                    self.taken += n;
                    Ok(n)
                }
                Err(e) => Err(std::io::Error::from_raw_os_error(e)),
            }
        }
        fn queued(&self) -> Option<u64> {
            Some(0)
        }
        fn recover(&mut self) -> Result<(), String> {
            self.recovered += 1;
            Ok(())
        }
    }

    #[test]
    fn the_rate_falls_back_to_48k() {
        let (ev, log) = Events::capture();
        let mut tried = Vec::new();
        let r = negotiate(22050, &ev, |r| {
            tried.push(r);
            if r == 48_000 {
                Ok(())
            } else {
                Err("EINVAL".into())
            }
        });
        assert_eq!((r, tried), (Ok(48_000), vec![22050, 48_000]));
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("pcm.params_refused"))
        );
        assert_eq!(negotiate(44100, &ev, |_| Ok(())), Ok(44100));
        assert!(negotiate(44100, &ev, |_| Err("no".into())).is_err());
    }

    #[test]
    fn an_xrun_is_prepared_away_and_the_write_goes_on() {
        let (ev, log) = Events::capture();
        let mut d = fake(vec![Ok(16), Err(EPIPE), Err(EAGAIN), Ok(1000)], Err(EAGAIN));
        let mut written = 0;
        let r = write_all(
            &mut d,
            &[0u8; 64],
            8,
            &mut written,
            Duration::from_secs(1),
            &ev,
        );
        assert_eq!((r, d.recovered, d.taken, written), (Ok(()), 1, 64, 8));
        assert!(log.lock().unwrap().iter().any(|l| l.contains("pcm.xrun")));
    }

    #[test]
    fn a_device_that_takes_nothing_ends_after_the_deadline() {
        let (ev, _) = Events::capture();
        let t = Instant::now();
        for rest in [Err(EAGAIN), Ok(0), Err(EPIPE)] {
            let mut d = fake(vec![Ok(8)], rest);
            let mut written = 0;
            let r = write_all(
                &mut d,
                &[0u8; 64],
                8,
                &mut written,
                Duration::from_millis(50),
                &ev,
            );
            assert!(r.is_err(), "{r:?}");
        }
        assert!(t.elapsed() < Duration::from_secs(5));
        let mut d = fake(vec![], Err(5)); // EIO
        let mut w = 0;
        assert!(write_all(&mut d, &[0u8; 8], 8, &mut w, Duration::from_secs(1), &ev).is_err());
    }

    #[test]
    fn target_words() {
        for (t, w) in [
            ("hdmi\n", "hdmi"),
            ("jack", "jack"),
            ("usb", "usb"),
            ("fixed:1/jack", "jack"),
            ("auto", "auto"),
            ("", "auto"),
            ("fixed:kaputt", "auto"),
        ] {
            assert_eq!(target_word(t), w, "{t:?}");
        }
    }

    #[test]
    fn output_words() {
        assert_eq!(
            output_word("mixer=running playing=hdmi clients=1 hdmi=on"),
            "hdmi"
        );
        assert_eq!(
            output_word("mixer=running playing=none clients=0 hdmi=absent"),
            "none"
        );
        assert_eq!(output_word(""), "unknown");
    }
}
