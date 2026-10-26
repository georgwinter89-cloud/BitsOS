//! SN-9: `/dev/snd/pcmC0D0c`, the capture substream (Linux `pcm_native.c` with SNDRV_PCM_STREAM_CAPTURE).
//! One open at a time (one substream, as a hw device: a second open waits, or EBUSY with O_NONBLOCK,
//! pcm_native.c:2754 `snd_pcm_open`). HW_PARAMS opens a `capture::Capture` on the system's inputs (USB headset
//! microphone, else an hda mic or line input); the frames go to the program as S16/S32/FLOAT, mono (L+R)/2 or
//! stereo, 48 kHz (other rates through libasound's plug). hw_ptr = appl_ptr + frames waiting; XRUN when they
//! reach stop_threshold or the source lost frames (pcm_lib.c:206 `snd_pcm_update_state`).
//!
//! Locks: `snd.pcm.c` (spin, per file) is never held across a source call that may sleep: `start`, `stop`,
//! `read` and `discard` of the source run after the unlock (hda takes `hda.slots`, a mutex). What is read under
//! it (`avail`, `overruns`, `gone`) are atomics or a leaf spin lock (`usb_audio.cring`).

use super::abi::*;
use super::cxfer;
use super::params::{self, Constraints, HwParams};
use super::pcm::Sw;
use crate::capture::{self, Capture, Inputs};
use crate::client::SndError;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::audio::PcmFormat;
use ksync::{Mutex, SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::node::File;
use vfs::poll::{Events, PollQueue};
use vfs::sys::UserMem;
use vfs::Errno;

/// What the capture side gives: 48 kHz (the USB microphone's and the hda ADC's rate), 1-2 channels,
/// S16/S32/FLOAT (16 significant bits).
pub const HW_CAPTURE: params::Hw = params::Hw {
    info: INFO_INTERLEAVED | INFO_BLOCK_TRANSFER | INFO_BATCH | INFO_PAUSE | INFO_NO_REWINDS,
    formats: (1 << FORMAT_S16_LE) | (1 << FORMAT_S32_LE) | (1 << FORMAT_FLOAT_LE),
    rates: 1 << 7,
    rate_min: 48_000,
    rate_max: 48_000,
    channels_min: 1,
    channels_max: 2,
    buffer_bytes_max: 4 << 20,
    period_bytes_min: 64,
    period_bytes_max: 2 << 20,
    periods_min: 2,
    periods_max: 1024,
    msbits32: 16,
};

/// The one capture substream is open (`substream_opened`).
static OPENED: AtomicBool = AtomicBool::new(false);
static CLOSED: WaitQueue = WaitQueue::new();

pub(crate) fn opened() -> bool {
    OPENED.load(Ordering::Acquire)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Phase {
    Open,
    Setup,
    Prepared,
    Running,
    Paused,
    Xrun,
    Draining,
}

pub(super) struct Run {
    pub cap: Arc<Capture>,
    pub format: u32,
    pub channels: u32,
    pub rate: u32,
    pub period: u64,
    pub buffer: u64,
    pub boundary: u64,
    pub frame_bytes: usize,
}

pub(super) struct St {
    pub phase: Phase,
    pub run: Option<Run>,
    pub sw: Sw,
    /// Frames read (or forwarded) since the last prepare.
    pub appl: u64,
    pub epoch: u64,
    pub busy: u32,
    pub reconf: bool,
    pub trigger_ns: u64,
    pub tstamp_type: u32,
    pub tstamp_ns: u64,
    pub audio_ns: u64,
    pub avail_max: u64,
}

impl St {
    fn new() -> St {
        St {
            phase: Phase::Open,
            run: None,
            sw: Sw { tstamp_mode: 0, avail_min: 1, start_threshold: 1, stop_threshold: 0, silence_threshold: 0, silence_size: 0 },
            appl: 0,
            epoch: 0,
            busy: 0,
            reconf: false,
            trigger_ns: 0,
            tstamp_type: TSTAMP_TYPE_GETTIMEOFDAY,
            tstamp_ns: 0,
            audio_ns: 0,
            avail_max: 0,
        }
    }

    pub fn state(&self) -> u32 {
        let Some(r) = &self.run else { return STATE_OPEN };
        if self.phase != Phase::Open && self.phase != Phase::Setup && r.cap.gone() {
            return STATE_DISCONNECTED;
        }
        match self.phase {
            Phase::Open => STATE_OPEN,
            Phase::Setup => STATE_SETUP,
            Phase::Prepared => STATE_PREPARED,
            Phase::Running => STATE_RUNNING,
            Phase::Paused => STATE_PAUSED,
            Phase::Xrun => STATE_XRUN,
            Phase::Draining => STATE_DRAINING,
        }
    }
    /// `snd_pcm_capture_avail`: frames waiting, at most a buffer (more were overwritten on Linux).
    pub fn avail(&mut self) -> u64 {
        let a = match &self.run {
            // PREPARED is 0 as on Linux (hw_ptr = appl_ptr until the start drops what came in before).
            Some(r) if matches!(self.phase, Phase::Running | Phase::Paused | Phase::Draining | Phase::Xrun) => (r.cap.avail() as u64).min(r.buffer),
            _ => 0,
        };
        self.avail_max = self.avail_max.max(a);
        a
    }
    pub fn hw_ptr(&mut self) -> u64 {
        let a = self.avail();
        self.run.as_ref().map_or(0, |r| (self.appl + a) % r.boundary)
    }
    pub fn appl_ptr(&self) -> u64 {
        self.run.as_ref().map_or(0, |r| self.appl % r.boundary)
    }
    pub fn now(&self) -> u64 {
        if self.tstamp_type == TSTAMP_TYPE_GETTIMEOFDAY && let Some(t) = time::wall::realtime_ns() {
            return t;
        }
        time::now_ns()
    }
    /// `snd_pcm_update_state` for capture: XRUN when the frames waiting reach stop_threshold, or the source
    /// lost frames (with stop_threshold < boundary; else the program gets what is left).
    pub fn update(&mut self) -> Result<(), Errno> {
        if self.phase != Phase::Running {
            return Ok(());
        }
        let a = self.avail();
        let Some(r) = &self.run else { return Ok(()) };
        if self.sw.stop_threshold < r.boundary && (a >= self.sw.stop_threshold || r.cap.overruns() > 0) {
            klog::event!(Info, "snd", "pcm.capture_xrun", avail = a, stop_threshold = self.sw.stop_threshold, lost = r.cap.overruns());
            self.phase = Phase::Xrun;
            self.trigger_ns = self.now();
            return Err(EPIPE);
        }
        Ok(())
    }
    /// `do_pcm_hwsync` (pcm_native.c:2910) for capture.
    pub fn hwsync(&mut self) -> Result<(), Errno> {
        match self.state() {
            STATE_RUNNING => {
                self.update()?;
                if self.sw.tstamp_mode == TSTAMP_ENABLE
                    && let Some(r) = &self.run
                {
                    let pos = self.appl + r.cap.avail() as u64;
                    self.tstamp_ns = self.now();
                    self.audio_ns = pos.saturating_mul(1_000_000_000) / r.rate as u64;
                }
                Ok(())
            }
            STATE_PREPARED | STATE_PAUSED => Ok(()),
            STATE_XRUN => Err(EPIPE),
            _ => Err(EBADFD),
        }
    }
    /// `snd_pcm_calc_delay`: frames waiting plus the device's own.
    pub fn delay(&mut self) -> i64 {
        let a = self.avail();
        a as i64 + self.run.as_ref().map_or(0, |r| r.cap.latency()) as i64
    }
    fn cap(&self) -> Option<Arc<Capture>> {
        self.run.as_ref().map(|r| r.cap.clone())
    }
}

/// Source calls that must wait for the unlock of `snd.pcm.c`.
#[must_use]
pub(super) enum Later {
    None,
    Stop(Arc<Capture>),
    /// Prepare/reset: drop what is waiting (appl_ptr = hw_ptr).
    Discard(Arc<Capture>, bool),
    Start(Arc<Capture>),
    /// Pause release: the source runs on, the frames that waited stay.
    Resume(Arc<Capture>),
}

impl Later {
    pub fn run(self) -> Result<(), SndError> {
        match self {
            Later::None => Ok(()),
            Later::Stop(c) => {
                c.stop();
                Ok(())
            }
            Later::Discard(c, stop) => {
                if stop {
                    c.stop();
                }
                c.discard(c.avail());
                Ok(())
            }
            Later::Start(c) => c.start(),
            Later::Resume(c) => c.resume(),
        }
    }
}

pub struct CapFile {
    inputs: Option<Arc<dyn Inputs>>,
    pub(super) st: SpinLock<St>,
    /// Serialises the state change plus its source call (`Later::run`), so two threads of one program cannot
    /// leave the source running in SETUP. Taken before `snd.pcm.c`, never while it is held.
    pub(super) trans: Mutex<()>,
    pub(super) poll: Arc<PollQueue>,
    open_nonblock: bool,
    /// Made by the devnode's open: holds the one substream (`OPENED`) until dropped. ktest files on a fake
    /// do not.
    claimed: bool,
    name: String,
    cons: Constraints,
}

impl CapFile {
    /// An open capture on `inputs` (the devnode's open, which holds the one substream; ktests pass a fake).
    pub fn new(inputs: Arc<dyn Inputs>, flags: u64, name: String) -> CapFile {
        CapFile {
            inputs: Some(inputs),
            st: SpinLock::named("snd.pcm.c", St::new()),
            trans: Mutex::named("snd.pcm.ctrans", ()),
            poll: Arc::new(PollQueue::new()),
            open_nonblock: flags & vfs::abi::O_NONBLOCK != 0,
            claimed: false,
            name,
            cons: Constraints::new(HW_CAPTURE),
        }
    }
    fn node() -> CapFile {
        CapFile { inputs: None, st: SpinLock::named("snd.pcm.c", St::new()), trans: Mutex::named("snd.pcm.ctrans", ()), poll: Arc::new(PollQueue::new()), open_nonblock: false, claimed: false, name: String::new(),
            cons: Constraints::new(HW_CAPTURE) }
    }

    /// As `PcmFile::nonblock_in` / `nonblock`: the open file's O_NONBLOCK now.
    pub(super) fn nonblock_in(&self, mem: &dyn UserMem) -> bool {
        match mem.file_status() {
            Some(s) => s & vfs::abi::O_NONBLOCK != 0,
            None => self.nonblock(),
        }
    }
    pub(super) fn nonblock(&self) -> bool {
        let me = self as *const CapFile as *const u8;
        if let Some(p) = proc::current() {
            for (_, f) in p.fds.open_fds() {
                if Arc::as_ptr(&f.file) as *const u8 == me {
                    return f.status_flags() & vfs::abi::O_NONBLOCK != 0;
                }
            }
        }
        self.open_nonblock
    }

    fn hw_params(&self, p: &mut HwParams) -> Result<(), Errno> {
        let _t = self.trans.lock();
        {
            let mut st = self.st.lock();
            if st.busy > 0 || st.reconf {
                return Err(EBUSY);
            }
            if !matches!(st.state(), STATE_OPEN | STATE_SETUP | STATE_PREPARED) {
                return Err(EBADFD);
            }
            st.reconf = true;
        }
        let r = self.hw_params_marked(p);
        self.st.lock().reconf = false;
        r
    }

    fn hw_params_marked(&self, p: &mut HwParams) -> Result<(), Errno> {
        // The old source closes after the unlock (USB: a control transfer, hda: `hda.slots`).
        let fail = |e: Errno| {
            let old = {
                let mut st = self.st.lock();
                st.phase = Phase::Open;
                st.run.take()
            };
            drop(old);
            e
        };
        if let Err(e) = self.cons.hw_params(p) {
            return Err(fail(e));
        }
        let (format, channels, rate) = (p.get(params::FORMAT), p.get(params::CHANNELS), p.get(params::RATE));
        let (period, buffer) = (p.get(params::PERIOD_SIZE) as u64, p.get(params::BUFFER_SIZE) as u64);
        // The old source goes first (a USB microphone is claimed once).
        let old = self.st.lock().run.take();
        drop(old);
        let inputs = self.inputs.as_ref().ok_or(ENODEV)?;
        let fmt = PcmFormat { rate, channels: channels as u8, bits: 16 };
        let cap = match capture::open_on(&**inputs, fmt, buffer as u32, period as u32, &self.name, Some(self.poll.clone())) {
            Ok(c) => c,
            Err(e) => {
                klog::event!(Warn, "snd", "pcm.hw_params_failed", stream = "capture", rate = rate, channels = channels, buffer = buffer,
                    err = format!("{e:?}").as_str());
                return Err(fail(match e {
                    SndError::NoMemory => ENOMEM,
                    SndError::Busy => EBUSY,
                    SndError::NoInput | SndError::Closed => ENODEV,
                    _ => EINVAL,
                }));
            }
        };
        let mut boundary = buffer;
        while boundary * 2 <= i64::MAX as u64 - buffer {
            boundary *= 2;
        }
        let mut st = self.st.lock();
        st.run = Some(Run { cap: Arc::new(cap), format, channels, rate, period, buffer, boundary, frame_bytes: (params::phys_width(format) / 8 * channels) as usize });
        st.sw = Sw { tstamp_mode: 0, avail_min: period, start_threshold: 1, stop_threshold: buffer, silence_threshold: 0, silence_size: 0 };
        st.appl = 0;
        st.epoch += 1;
        st.phase = Phase::Setup;
        drop(st);
        klog::event!(Info, "snd", "pcm.hw_params", name = self.name.as_str(), stream = "capture", format = format, rate = rate, channels = channels,
            period = period, buffer = buffer);
        Ok(())
    }

    fn hw_free(&self) -> Result<(), Errno> {
        let _t = self.trans.lock();
        let mut st = self.st.lock();
        if st.busy > 0 || st.reconf {
            return Err(EBUSY);
        }
        if !matches!(st.state(), STATE_SETUP | STATE_PREPARED) {
            return Err(EBADFD);
        }
        let old = st.run.take();
        st.phase = Phase::Open;
        drop(st);
        drop(old);
        Ok(())
    }

    fn sw_params(&self, b: &mut [u8]) -> Result<(), Errno> {
        let mut st = self.st.lock();
        if st.state() == STATE_OPEN {
            return Err(EBADFD);
        }
        let Some(r) = &st.run else { return Err(EBADFD) };
        let (buffer, boundary) = (r.buffer, r.boundary);
        let mode = get_i32(b, sw::TSTAMP_MODE);
        let proto = get_u32(b, sw::PROTO);
        let ttype = get_u32(b, sw::TSTAMP_TYPE);
        let avail_min = get_u64(b, sw::AVAIL_MIN);
        let (sil_thr, sil_size) = (get_u64(b, sw::SILENCE_THRESHOLD), get_u64(b, sw::SILENCE_SIZE));
        if !(0..=TSTAMP_ENABLE).contains(&mode) {
            return Err(EINVAL);
        }
        let new_proto = proto >= (2 << 16) | 12;
        if new_proto && ttype > TSTAMP_TYPE_LAST {
            return Err(EINVAL);
        }
        if avail_min == 0 {
            return Err(EINVAL);
        }
        if sil_size >= boundary {
            if sil_thr != 0 {
                return Err(EINVAL);
            }
        } else if sil_size > sil_thr || sil_thr > buffer {
            return Err(EINVAL);
        }
        st.sw = Sw { tstamp_mode: mode, avail_min, start_threshold: get_u64(b, sw::START_THRESHOLD), stop_threshold: get_u64(b, sw::STOP_THRESHOLD),
            silence_threshold: sil_thr, silence_size: sil_size };
        if new_proto {
            st.tstamp_type = ttype;
        }
        put_u64(b, sw::BOUNDARY, boundary);
        let r = st.update();
        drop(st);
        self.poll.wake_all();
        r
    }

    fn status(&self, b: &mut [u8], ext: bool) {
        let data = get_u32(b, status::AUDIO_TSTAMP_DATA);
        b.fill(0);
        if ext {
            put_u32(b, status::AUDIO_TSTAMP_DATA, data);
        }
        let mut st = self.st.lock();
        let running = st.state() == STATE_RUNNING;
        if running {
            let _ = st.hwsync();
        }
        let state = st.state();
        put_u32(b, status::STATE, state);
        if state == STATE_OPEN {
            return;
        }
        put_ts(b, status::TRIGGER_TSTAMP, st.trigger_ns);
        if running && st.sw.tstamp_mode == TSTAMP_ENABLE {
            put_ts(b, status::TSTAMP, st.tstamp_ns);
            put_ts(b, status::DRIVER_TSTAMP, st.tstamp_ns);
            put_ts(b, status::AUDIO_TSTAMP, st.audio_ns);
        } else if st.sw.tstamp_mode == TSTAMP_ENABLE {
            put_ts(b, status::TSTAMP, st.now());
        }
        put_u64(b, status::APPL_PTR, st.appl_ptr());
        let hw = st.hw_ptr();
        put_u64(b, status::HW_PTR, hw);
        put_u64(b, status::AVAIL, st.avail());
        put_u64(b, status::DELAY, if state == STATE_RUNNING { st.delay() as u64 } else { 0 });
        put_u64(b, status::AVAIL_MAX, st.avail_max);
        st.avail_max = 0;
    }

    fn sync_ptr(&self, b: &mut [u8]) -> Result<(), Errno> {
        let flags = get_u32(b, sync::FLAGS);
        let (appl, avail_min) = (get_u64(b, sync::APPL_PTR), get_u64(b, sync::AVAIL_MIN));
        b.fill(0);
        put_u32(b, sync::FLAGS, flags);
        if flags & SYNC_PTR_HWSYNC != 0 {
            self.st.lock().hwsync()?;
        }
        if flags & SYNC_PTR_APPL == 0 {
            cxfer::apply_appl(self, appl)?;
        }
        let mut st = self.st.lock();
        if flags & SYNC_PTR_AVAIL_MIN == 0 {
            st.sw.avail_min = avail_min;
        }
        put_u64(b, sync::APPL_PTR, st.appl_ptr());
        put_u64(b, sync::AVAIL_MIN, st.sw.avail_min);
        put_u32(b, sync::STATE, st.state());
        let hw = st.hw_ptr();
        put_u64(b, sync::HW_PTR, hw);
        put_ts(b, sync::TSTAMP, st.tstamp_ns);
        put_ts(b, sync::AUDIO_TSTAMP, st.audio_ns);
        Ok(())
    }

    /// Runs `f` on the state, then the source call it asked for after the unlock, then wakes poll.
    pub(super) fn act(&self, f: impl FnOnce(&mut St, &CapFile) -> Result<Later, Errno>) -> Result<(), Errno> {
        let _t = self.trans.lock();
        let later = f(&mut self.st.lock(), self)?;
        let r = later.run();
        self.poll.wake_all();
        r.map_err(|_| {
            // The source is gone or refused to start.
            let mut st = self.st.lock();
            if st.phase == Phase::Running {
                st.phase = Phase::Xrun;
            }
            EPIPE
        })
    }

    fn prepare(&self) -> Result<(), Errno> {
        self.act(|st, _| {
            match st.state() {
                STATE_OPEN | STATE_DISCONNECTED => return Err(EBADFD),
                STATE_RUNNING | STATE_DRAINING => return Err(EBUSY),
                _ => {}
            }
            st.appl = 0;
            st.epoch += 1;
            st.phase = Phase::Prepared;
            Ok(st.cap().map_or(Later::None, |c| Later::Discard(c, true)))
        })
    }

    pub(super) fn start_locked(st: &mut St) -> Result<Later, Errno> {
        if st.state() != STATE_PREPARED {
            return Err(EBADFD);
        }
        st.phase = Phase::Running;
        st.trigger_ns = st.now();
        Ok(st.cap().map_or(Later::None, Later::Start))
    }

    /// `snd_pcm_drop`: SETUP, the source stops, what waits is dropped at the next prepare.
    pub(super) fn drop_stream(&self) -> Result<(), Errno> {
        self.act(|st, _| {
            if matches!(st.state(), STATE_OPEN | STATE_DISCONNECTED) {
                return Err(EBADFD);
            }
            let was = st.phase;
            st.phase = Phase::Setup;
            st.trigger_ns = st.now();
            Ok(match (was, st.cap()) {
                (Phase::Running | Phase::Paused | Phase::Draining | Phase::Xrun, Some(c)) => Later::Stop(c),
                _ => Later::None,
            })
        })
    }

    /// `snd_pcm_drain` for capture (pcm_native.c:2031, :2097): a running (or paused) stream stops, DRAINING
    /// while frames wait, else SETUP; XRUN stays; nothing is waited for; O_NONBLOCK gives EAGAIN (:2115).
    fn drain(&self, mem: &dyn UserMem) -> Result<(), Errno> {
        self.act(|st, _| {
            match st.state() {
                STATE_OPEN | STATE_DISCONNECTED => return Err(EBADFD),
                STATE_PAUSED | STATE_RUNNING => {
                    let a = st.avail();
                    st.phase = if a > 0 { Phase::Draining } else { Phase::Setup };
                    st.trigger_ns = st.now();
                    Ok(st.cap().map_or(Later::None, Later::Stop))
                }
                _ => Ok(Later::None),
            }
        })?;
        if self.nonblock_in(mem) { Err(EAGAIN) } else { Ok(()) }
    }

    fn pause(&self, push: bool) -> Result<(), Errno> {
        self.act(|st, _| {
            let want = if push { STATE_RUNNING } else { STATE_PAUSED };
            if st.state() != want {
                return Err(EBADFD);
            }
            st.phase = if push { Phase::Paused } else { Phase::Running };
            st.trigger_ns = st.now();
            let Some(c) = st.cap() else { return Ok(Later::None) };
            // Release continues where it stopped (the frames that waited stay).
            Ok(if push { Later::Stop(c) } else { Later::Resume(c) })
        })
    }

    /// `snd_pcm_reset`: appl_ptr = hw_ptr (what waits is dropped), the state stays.
    fn reset_ioctl(&self) -> Result<(), Errno> {
        self.act(|st, _| {
            if !matches!(st.state(), STATE_RUNNING | STATE_PREPARED | STATE_PAUSED) {
                return Err(EBADFD);
            }
            let a = st.avail();
            st.appl += a;
            st.epoch += 1;
            Ok(st.cap().map_or(Later::None, |c| Later::Discard(c, false)))
        })
    }

    fn xrun(&self) -> Result<(), Errno> {
        self.act(|st, _| match st.state() {
            STATE_XRUN => Ok(Later::None),
            STATE_RUNNING => {
                st.phase = Phase::Xrun;
                st.trigger_ns = st.now();
                klog::event!(Info, "snd", "pcm.xrun_forced", stream = "capture");
                Ok(st.cap().map_or(Later::None, Later::Stop))
            }
            _ => Err(EBADFD),
        })
    }

    fn read_arg<const N: usize>(mem: &dyn UserMem, arg: u64) -> Result<[u8; N], Errno> {
        let mut b = [0u8; N];
        if mem.read(arg, &mut b) { Ok(b) } else { Err(EFAULT) }
    }
    fn put(mem: &dyn UserMem, arg: u64, b: &[u8]) -> Result<i64, Errno> {
        if mem.write(arg, b) { Ok(0) } else { Err(EFAULT) }
    }

    fn rewind_forward(&self, arg: u64, mem: &dyn UserMem, forward: bool) -> Result<i64, Errno> {
        let mut b = [0u8; 8];
        if !mem.read(arg, &mut b) || !mem.write(arg, &[0; 8]) {
            return Err(EFAULT);
        }
        let frames = u64::from_le_bytes(b);
        let sync = if frames == 0 { Ok(()) } else { self.st.lock().hwsync() };
        // NO_REWINDS: a backward move is refused (pcm_lib.c:2179), so rewind does 0.
        let r = sync.and_then(|_| if forward && frames > 0 { cxfer::forward(self, frames) } else { Ok(0) });
        let v = match r {
            Ok(n) => n as i64,
            Err(e) => -e,
        };
        if !mem.write(arg, &v.to_le_bytes()) {
            return Err(EFAULT);
        }
        r.map(|_| 0)
    }

    /// `snd_pcm_common_ioctl` (pcm_native.c:3288) for the capture substream.
    pub fn ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        if (cmd >> 8) & 0xff != b'A' as u64 {
            return Err(ENOTTY);
        }
        if self.inputs.is_none() {
            return Err(ENXIO);
        }
        if self.st.lock().state() == STATE_DISCONNECTED {
            // The microphone went away (Linux: the USB card's disconnect, snd_disconnect_ioctl).
            return Err(ENODEV);
        }
        match cmd {
            PCM_PVERSION => Self::put(mem, arg, &SNDRV_PCM_VERSION.to_le_bytes()),
            PCM_INFO => {
                let mut b = [0u8; PCM_INFO_SIZE];
                super::pcm_info(&mut b, 1);
                Self::put(mem, arg, &b)
            }
            PCM_TSTAMP => Ok(0),
            PCM_TTSTAMP => {
                let v = i32::from_le_bytes(Self::read_arg::<4>(mem, arg)?);
                if v < 0 || v as u32 > TSTAMP_TYPE_LAST {
                    return Err(EINVAL);
                }
                self.st.lock().tstamp_type = v as u32;
                Ok(0)
            }
            PCM_USER_PVERSION => {
                Self::read_arg::<4>(mem, arg)?;
                Ok(0)
            }
            PCM_HW_REFINE | PCM_HW_PARAMS => {
                let raw = Self::read_arg::<HW_PARAMS_SIZE>(mem, arg)?;
                let mut p = HwParams::decode(&raw);
                if cmd == PCM_HW_REFINE {
                    self.cons.refine(&mut p)?;
                    self.cons.fixup(&mut p);
                } else {
                    self.hw_params(&mut p)?;
                }
                Self::put(mem, arg, &p.encode())
            }
            PCM_HW_FREE => self.hw_free().map(|_| 0),
            PCM_SW_PARAMS => {
                let mut b = Self::read_arg::<SW_PARAMS_SIZE>(mem, arg)?;
                let r = self.sw_params(&mut b);
                Self::put(mem, arg, &b)?;
                r.map(|_| 0)
            }
            PCM_STATUS | PCM_STATUS_EXT => {
                let mut b = [0u8; STATUS_SIZE];
                if cmd == PCM_STATUS_EXT && !mem.read(arg.wrapping_add(status::AUDIO_TSTAMP_DATA as u64), &mut b[status::AUDIO_TSTAMP_DATA..status::AUDIO_TSTAMP_DATA + 4]) {
                    return Err(EFAULT);
                }
                self.status(&mut b, cmd == PCM_STATUS_EXT);
                Self::put(mem, arg, &b)
            }
            PCM_CHANNEL_INFO => {
                let mut b = Self::read_arg::<CHANNEL_INFO_SIZE>(mem, arg)?;
                let ch = get_u32(&b, 0);
                let st = self.st.lock();
                if st.state() == STATE_OPEN {
                    return Err(EBADFD);
                }
                if ch >= st.run.as_ref().map_or(0, |r| r.channels) {
                    return Err(EINVAL);
                }
                drop(st);
                b.fill(0);
                put_u32(&mut b, 0, ch);
                put_u64(&mut b, 8, u64::MAX);
                Self::put(mem, arg, &b)
            }
            PCM_PREPARE => self.prepare().map(|_| 0),
            PCM_RESET => self.reset_ioctl().map(|_| 0),
            PCM_START => self.act(|st, _| Self::start_locked(st)).map(|_| 0),
            PCM_DROP => self.drop_stream().map(|_| 0),
            PCM_DRAIN => self.drain(mem).map(|_| 0),
            PCM_PAUSE => self.pause(arg != 0).map(|_| 0),
            PCM_RESUME => Err(ENOSYS),
            PCM_XRUN => self.xrun().map(|_| 0),
            PCM_HWSYNC => self.st.lock().hwsync().map(|_| 0),
            PCM_DELAY => {
                let d = {
                    let mut st = self.st.lock();
                    st.hwsync()?;
                    st.delay()
                };
                Self::put(mem, arg, &d.to_le_bytes())
            }
            PCM_SYNC_PTR => {
                let mut b = Self::read_arg::<SYNC_PTR_SIZE>(mem, arg)?;
                self.sync_ptr(&mut b)?;
                Self::put(mem, arg, &b)
            }
            PCM_REWIND => self.rewind_forward(arg, mem, false),
            PCM_FORWARD => self.rewind_forward(arg, mem, true),
            // A capture substream reads for both (`snd_pcm_xferi_frames_ioctl` looks at the stream only).
            PCM_WRITEI_FRAMES | PCM_READI_FRAMES => cxfer::xferi(self, arg, mem),
            PCM_WRITEN_FRAMES | PCM_READN_FRAMES => {
                if self.st.lock().state() == STATE_OPEN {
                    return Err(EBADFD);
                }
                if !mem.write(arg, &[0; 8]) {
                    return Err(EFAULT);
                }
                Err(EINVAL)
            }
            _ => Err(ENOTTY),
        }
    }
}

impl File for CapFile {
    fn open_node(&self, flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        if self.inputs.is_some() {
            return None;
        }
        Some(open_capture(flags))
    }
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, self.nonblock())
    }
    /// `snd_pcm_read` (pcm_native.c:3472): whole frames, counted in bytes.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        let fb = {
            let st = self.st.lock();
            // pcm_native.c:3474: OPEN and DISCONNECTED are EBADFD for read(2) (the ioctl gives ENODEV).
            if matches!(st.state(), STATE_OPEN | STATE_DISCONNECTED) {
                return Err(EBADFD);
            }
            st.run.as_ref().map_or(1, |r| r.frame_bytes)
        };
        if buf.len() % fb != 0 {
            return Err(EINVAL);
        }
        let frames = (buf.len() / fb) as u64;
        cxfer::read(self, cxfer::Dst::Kernel(buf), frames, Some(nonblock), None).map(|n| n as usize * fb)
    }
    /// The capture file has no write (Linux's capture f_ops have none: EINVAL).
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(super::ALSA_MAJOR, super::PCM_CAPTURE_MINOR)
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        // No per-ioctl counter as the playback file keeps (a debug trace: open question in sn-SN-9.md).
        let r = self.ioctl(cmd, arg, mem);
        if let Err(e) = r
            && cmd != PCM_READI_FRAMES
        {
            klog::event!(Debug, "snd", "pcm.ioctl", stream = "capture", cmd = format!("{cmd:#x}").as_str(), errno = e);
        }
        Some(r)
    }
    /// `snd_pcm_poll` (pcm_native.c:3590) for capture.
    fn events(&self) -> Events {
        let ok = Events::IN | Events::RDNORM;
        if self.inputs.is_none() {
            return Events(ok | Events::ERR);
        }
        let mut st = self.st.lock();
        let _ = st.update();
        let avail = st.avail();
        Events(match st.state() {
            STATE_RUNNING | STATE_PREPARED | STATE_PAUSED if avail >= st.sw.avail_min => ok,
            STATE_RUNNING | STATE_PREPARED | STATE_PAUSED => 0,
            STATE_DRAINING if avail > 0 => ok,
            _ => ok | Events::ERR,
        })
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.poll)
    }
    fn show_fdinfo(&self, out: &mut String) {
        let st = self.st.lock();
        out.push_str(&format!("alsa-state:\t{}\nalsa-appl:\t{}\n", st.state(), st.appl));
        if let Some(r) = &st.run {
            out.push_str(&format!("alsa-setup:\t{} Hz {} ch format {} period {} buffer {}\nalsa-source:\t{}\n", r.rate, r.channels, r.format, r.period,
                r.buffer, r.cap.source_name()));
        }
    }
}

impl Drop for CapFile {
    fn drop(&mut self) {
        if self.inputs.is_none() {
            return;
        }
        let run = {
            let mut st = self.st.lock();
            klog::event!(Info, "snd", "pcm.close", name = self.name.as_str(), stream = "capture", state = st.state(), appl = st.appl);
            st.run.take()
        };
        // The source closes here, outside the lock (USB: alternate setting 0, a control transfer).
        drop(run);
        if self.claimed {
            OPENED.store(false, Ordering::Release);
            CLOSED.wake_all();
        }
    }
}

pub(super) fn node() -> Arc<dyn File> {
    Arc::new(CapFile::node())
}

/// The devnode's open: the one capture substream (blocking opens wait for it in bounded slices, B4).
fn open_capture(flags: u64) -> Result<Arc<dyn File>, Errno> {
    super::allowed_class(super::PCM_CAPTURE_NODE, policy::DeviceClass::Microphone)?;
    loop {
        if OPENED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            break;
        }
        if flags & vfs::abi::O_NONBLOCK != 0 {
            return Err(EBUSY);
        }
        if vfs::dentry::interrupted() {
            return Err(uapi::ab::siginfo::ERESTARTSYS);
        }
        CLOSED.wait_until_timeout(|| !OPENED.load(Ordering::Acquire), 50_000_000);
    }
    let name = match proc::current() {
        Some(p) => format!("pcmc:{}", p.pid),
        None => "pcmc:kernel".into(),
    };
    klog::event!(Debug, "snd", "pcm.open", name = name.as_str(), stream = "capture", flags = flags);
    let mut f = CapFile::new(Arc::new(crate::capsrc::SysInputs), flags, name);
    f.claimed = true;
    Ok(Arc::new(f))
}
