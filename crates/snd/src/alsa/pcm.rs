//! `/dev/snd/pcmC0D0p`: one mixer client per open (Linux's substream + runtime, `linux-6.6/sound/core/
//! pcm_native.c`). The data ring is the client's ring (RW_INTERLEAVED only, no mmap; libasound then uses
//! SYNC_PTR, pcm_native.c:3034). States are Linux's; while a stream is set up, PREPARED/RUNNING/PAUSED/
//! DRAINING/XRUN come from the client (`Playback::state`), SETUP and OPEN are this file's own.

use super::abi::*;
use super::params::{self, Constraints, HwParams};
use super::xfer::{self, Src};
use crate::client::{Playback, SndError, State};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use devmodel::audio::PcmFormat;
use ksync::SpinLock;
use uapi::errno::*;
use vfs::node::File;
use vfs::poll::{Events, PollQueue};
use vfs::sys::UserMem;
use vfs::Errno;

/// Where clients are opened: the kernel mixer, or a private one (ktests on the null output, host tests).
pub trait Card: Send + Sync {
    fn open(&self, fmt: PcmFormat, ring_frames: u32, name: &str, poll: Option<Arc<PollQueue>>) -> Result<Playback, SndError>;
}

impl Card for crate::Mixer {
    fn open(&self, fmt: PcmFormat, ring_frames: u32, name: &str, poll: Option<Arc<PollQueue>>) -> Result<Playback, SndError> {
        self.open_with(fmt, ring_frames, name, poll)
    }
}

/// The device's `snd_pcm_hardware`: what the mixer takes (SN-3: 16-bit, 1-2 channels, ALSA's standard rates up
/// to 192 kHz); S32_LE and FLOAT_LE are taken and converted to 16 bit here (msbits 16).
pub const HW: params::Hw = params::Hw {
    info: INFO_INTERLEAVED | INFO_BLOCK_TRANSFER | INFO_BATCH | INFO_PAUSE | INFO_NO_REWINDS,
    formats: (1 << FORMAT_S16_LE) | (1 << FORMAT_S32_LE) | (1 << FORMAT_FLOAT_LE),
    // 8000 11025 16000 22050 32000 44100 48000 (bits 1-7), 88200 96000 176400 192000 (bits 9-12).
    rates: 0b1_1110_1111_1110,
    rate_min: 8000,
    rate_max: 192_000,
    channels_min: 1,
    channels_max: 2,
    buffer_bytes_max: 4 << 20,
    period_bytes_min: 64,
    period_bytes_max: 2 << 20,
    periods_min: 2,
    periods_max: 1024,
    msbits32: 16,
};

pub fn constraints() -> Constraints {
    Constraints::new(HW)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Phase {
    Open,
    Setup,
    /// The client's state is the stream's.
    Active,
    /// SNDRV_PCM_IOCTL_XRUN forced it (the client was dropped).
    Xrun,
}

/// `runtime` after HW_PARAMS.
pub(super) struct Run {
    pub pb: Arc<Playback>,
    pub format: u32,
    pub channels: u32,
    pub rate: u32,
    pub period: u64,
    pub buffer: u64,
    pub boundary: u64,
    /// Bytes per frame in the program's format.
    pub frame_bytes: usize,
}

pub(super) struct Sw {
    pub tstamp_mode: i32,
    pub avail_min: u64,
    pub start_threshold: u64,
    pub stop_threshold: u64,
    pub silence_threshold: u64,
    pub silence_size: u64,
}

pub(super) struct St {
    pub phase: Phase,
    pub run: Option<Run>,
    pub sw: Sw,
    /// Frames written since the last prepare (appl_ptr before the wrap at `boundary`).
    pub appl: u64,
    /// Bumped by prepare/drop/hw_params: a write that started before does not count its frames after.
    pub epoch: u64,
    /// Transfers in progress (`buffer_accessing`): HW_PARAMS/HW_FREE answer EBUSY meanwhile.
    pub busy: u32,
    /// HW_PARAMS is replacing the client (`buffer_accessing < 0`): transfers answer EBUSY meanwhile.
    pub reconf: bool,
    pub trigger_ns: u64,
    pub tstamp_type: u32,
    pub tstamp_ns: u64,
    pub audio_ns: u64,
    pub avail_max: u64,
    pub user_pversion: u32,
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
            user_pversion: 0,
        }
    }

    /// The stream's state (`runtime->state`).
    pub fn state(&self) -> u32 {
        match (self.phase, &self.run) {
            (Phase::Open, _) | (_, None) => STATE_OPEN,
            (Phase::Setup, _) => STATE_SETUP,
            (Phase::Xrun, _) => STATE_XRUN,
            (Phase::Active, Some(r)) => match r.pb.state() {
                State::Prepared => STATE_PREPARED,
                State::Running => STATE_RUNNING,
                State::Paused => STATE_PAUSED,
                State::Draining => STATE_DRAINING,
                State::Drained => STATE_SETUP,
                State::Xrun => STATE_XRUN,
                State::Closed => STATE_DISCONNECTED,
            },
        }
    }
    pub fn hw_ptr(&self) -> u64 {
        match &self.run {
            Some(r) if self.phase == Phase::Active => r.pb.position() % r.boundary,
            _ => 0,
        }
    }
    pub fn appl_ptr(&self) -> u64 {
        self.run.as_ref().map_or(0, |r| self.appl % r.boundary)
    }
    /// Frames queued (`snd_pcm_playback_hw_avail`).
    pub fn hw_avail(&self) -> u64 {
        match &self.run {
            Some(r) if self.phase == Phase::Active => self.appl.saturating_sub(r.pb.position()).min(r.buffer),
            _ => 0,
        }
    }
    /// `snd_pcm_playback_avail`; also tracks `avail_max` as `snd_pcm_update_state` does.
    pub fn avail(&mut self) -> u64 {
        let a = self.run.as_ref().map_or(0, |r| r.buffer - self.hw_avail());
        self.avail_max = self.avail_max.max(a);
        a
    }
    /// `snd_pcm_running`: RUNNING, or DRAINING for playback.
    pub fn running(&self) -> bool {
        matches!(self.state(), STATE_RUNNING | STATE_DRAINING)
    }
    /// `snd_pcm_gettime` by the stream's tstamp type.
    pub fn now(&self) -> u64 {
        if self.tstamp_type == TSTAMP_TYPE_GETTIMEOFDAY && let Some(t) = time::wall::realtime_ns() {
            return t;
        }
        time::now_ns()
    }
    /// `do_pcm_hwsync` (pcm_native.c:2910): the state check, and the pointer's timestamps when enabled.
    pub fn hwsync(&mut self) -> Result<(), Errno> {
        match self.state() {
            STATE_RUNNING | STATE_DRAINING => {
                if self.sw.tstamp_mode == TSTAMP_ENABLE
                    && let Some(r) = &self.run
                {
                    self.tstamp_ns = self.now();
                    self.audio_ns = r.pb.position().saturating_mul(1_000_000_000) / r.rate as u64;
                }
                self.avail();
                Ok(())
            }
            STATE_PREPARED | STATE_PAUSED => Ok(()),
            STATE_XRUN => Err(EPIPE),
            _ => Err(EBADFD),
        }
    }
    /// `snd_pcm_calc_delay`: queued frames, plus (running) the mixer's path to the speaker.
    pub fn delay(&self) -> i64 {
        match &self.run {
            Some(r) if self.running() => r.pb.delay() as i64,
            _ => self.hw_avail() as i64,
        }
    }
    /// Prepare's reset (appl = hw = 0): the pointers now, the client's drop after the unlock (`Later`).
    /// The ring is cleared here, under `snd.pcm` (`snd.pcm` -> ring lock is the order `hw_avail` already
    /// uses), so appl_ptr and the ring restart together; only the wakes wait for the unlock (`Later`).
    pub fn reset(&mut self) -> Later {
        self.appl = 0;
        self.epoch += 1;
        Later(self.run.as_ref().map(|r| {
            r.pb.reset_ring();
            r.pb.set_stop_on_underrun(self.sw.stop_threshold < r.boundary);
            r.pb.clone()
        }))
    }
}

/// The client's wakes after a reset, run once `snd.pcm` is unlocked: they reach the poll queue, whose epoll
/// subscribers take their own locks (and an epoll harvest calls `events()`, which takes `snd.pcm`).
#[must_use]
pub(super) struct Later(Option<Arc<Playback>>);

impl Later {
    pub fn run(self) {
        if let Some(pb) = self.0 {
            pb.wake_after_reset();
        }
    }
}

/// The node's object (`open == false`) and one per open (`open_node`): Linux's `snd_pcm_file`.
pub struct PcmFile {
    card: Option<Arc<dyn Card>>,
    pub(super) st: SpinLock<St>,
    pub(super) poll: Arc<PollQueue>,
    /// O_NONBLOCK at open (see `nonblock`).
    open_nonblock: bool,
    name: String,
    cons: Constraints,
    /// Calls per ioctl number ('A' nr 0..=0x7f), logged at close: the trace that is compared with what
    /// libasound does on Linux (plan/notes/sn-SN-4.md).
    calls: [AtomicU32; 128],
}

impl PcmFile {
    /// An open stream on `card` (the devnode's open; ktests pass a private mixer).
    pub fn new(card: Arc<dyn Card>, flags: u64, name: String) -> PcmFile {
        PcmFile { card: Some(card), st: SpinLock::named("snd.pcm", St::new()), poll: Arc::new(PollQueue::new()), open_nonblock: flags & vfs::abi::O_NONBLOCK != 0, name, cons: constraints(), calls: [const { AtomicU32::new(0) }; 128] }
    }
    fn node() -> PcmFile {
        PcmFile { card: None, st: SpinLock::named("snd.pcm", St::new()), poll: Arc::new(PollQueue::new()), open_nonblock: false, name: String::new(), cons: constraints(), calls: [const { AtomicU32::new(0) }; 128] }
    }

    /// The open file's O_NONBLOCK now. `ioctl_user` gets no file flags and fcntl(F_SETFL) is not passed to the
    /// object (change request in plan/notes/sn-SN-4.md), while libasound opens with O_NONBLOCK and clears it
    /// with F_SETFL (`defaults.pcm.nonblock 1`): the caller's fd table is searched for this object. Kernel
    /// callers (ktests) have none: the open flag.
    /// O_NONBLOCK of the ioctl's open file as the syscall layer hands it over; `nonblock` where it does not.
    pub(super) fn nonblock_in(&self, mem: &dyn vfs::sys::UserMem) -> bool {
        match mem.file_status() {
            Some(s) => s & vfs::abi::O_NONBLOCK != 0,
            None => self.nonblock(),
        }
    }

    /// The fallback without the open file's flags (write(2) through `write_at`, host tests): the caller's fd
    /// table is searched for this file.
    pub(super) fn nonblock(&self) -> bool {
        let me = self as *const PcmFile as *const u8;
        if let Some(p) = proc::current() {
            for (_, f) in p.fds.open_fds() {
                if Arc::as_ptr(&f.file) as *const u8 == me {
                    return f.status_flags() & vfs::abi::O_NONBLOCK != 0;
                }
            }
        }
        self.open_nonblock
    }

    /// `trigger_tstamp` of a start/stop/pause; the caller wakes `poll` after unlocking `st`.
    pub(super) fn trigger(&self, st: &mut St) {
        st.trigger_ns = st.now();
    }

    /// `snd_pcm_hw_params` (pcm_native.c:712): the busy check and the mark that keeps transfers out
    /// (`snd_pcm_buffer_access_lock`, :691) are one critical section, the mark lasts to the end.
    fn hw_params(&self, p: &mut HwParams) -> Result<(), Errno> {
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
        let fail = |st: &mut St, e: Errno| {
            st.run = None;
            st.phase = Phase::Open;
            e
        };
        if let Err(e) = self.cons.hw_params(p) {
            return Err(fail(&mut self.st.lock(), e));
        }
        let (format, channels, rate) = (p.get(params::FORMAT), p.get(params::CHANNELS), p.get(params::RATE));
        let (period, buffer) = (p.get(params::PERIOD_SIZE) as u64, p.get(params::BUFFER_SIZE) as u64);
        // The old client goes first (its ring's memory), as Linux frees the old buffer before allocating.
        let old = self.st.lock().run.take();
        drop(old);
        let card = self.card.as_ref().ok_or(ENODEV)?;
        let fmt = PcmFormat { rate, channels: channels as u8, bits: 16 };
        let pb = match card.open(fmt, buffer as u32, &self.name, Some(self.poll.clone())) {
            Ok(pb) => pb,
            Err(e) => {
                klog::event!(Warn, "snd", "pcm.hw_params_failed", rate = rate, channels = channels, buffer = buffer, err = alloc::format!("{e:?}").as_str());
                let errno = match e {
                    SndError::NoMemory => ENOMEM,
                    SndError::NoMixer | SndError::Closed => ENODEV,
                    _ => EINVAL,
                };
                return Err(fail(&mut self.st.lock(), errno));
            }
        };
        let mut boundary = buffer;
        while boundary * 2 <= i64::MAX as u64 - buffer {
            boundary *= 2;
        }
        let mut st = self.st.lock();
        st.run = Some(Run { pb: Arc::new(pb), format, channels, rate, period, buffer, boundary, frame_bytes: (params::phys_width(format) / 8 * channels) as usize });
        st.sw = Sw { tstamp_mode: 0, avail_min: period, start_threshold: 1, stop_threshold: buffer, silence_threshold: 0, silence_size: 0 };
        let later = st.reset();
        st.phase = Phase::Setup;
        drop(st);
        later.run();
        klog::event!(Info, "snd", "pcm.hw_params", name = self.name.as_str(), format = format, rate = rate, channels = channels, period = period, buffer = buffer);
        Ok(())
    }

    fn hw_free(&self) -> Result<(), Errno> {
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

    /// `snd_pcm_sw_params` (pcm_native.c:913); `b` is copied back whatever the result (boundary filled in).
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
        st.sw = Sw { tstamp_mode: mode, avail_min, start_threshold: get_u64(b, sw::START_THRESHOLD), stop_threshold: get_u64(b, sw::STOP_THRESHOLD), silence_threshold: sil_thr, silence_size: sil_size };
        if new_proto {
            st.tstamp_type = ttype;
        }
        put_u64(b, sw::BOUNDARY, boundary);
        // stop_threshold < boundary: an empty ring stops the stream (XRUN); >= boundary: silence, plays on.
        if let Some(r) = &st.run {
            r.pb.set_stop_on_underrun(st.sw.stop_threshold < boundary);
        }
        drop(st);
        self.poll.wake_all();
        Ok(())
    }

    /// `snd_pcm_status64` (pcm_native.c:993) into `b` (zeroed; `ext` keeps the program's audio_tstamp_data).
    fn status(&self, b: &mut [u8], ext: bool) {
        let data = get_u32(b, status::AUDIO_TSTAMP_DATA);
        b.fill(0);
        if ext {
            put_u32(b, status::AUDIO_TSTAMP_DATA, data);
        }
        let mut st = self.st.lock();
        let state = st.state();
        put_u32(b, status::STATE, state);
        if state == STATE_OPEN {
            return;
        }
        put_ts(b, status::TRIGGER_TSTAMP, st.trigger_ns);
        if st.running() {
            let _ = st.hwsync();
            if st.sw.tstamp_mode == TSTAMP_ENABLE {
                put_ts(b, status::TSTAMP, st.tstamp_ns);
                put_ts(b, status::DRIVER_TSTAMP, st.tstamp_ns);
                put_ts(b, status::AUDIO_TSTAMP, st.audio_ns);
            }
        } else if st.sw.tstamp_mode == TSTAMP_ENABLE {
            put_ts(b, status::TSTAMP, st.now());
        }
        put_u64(b, status::APPL_PTR, st.appl_ptr());
        put_u64(b, status::HW_PTR, st.hw_ptr());
        put_u64(b, status::AVAIL, st.avail());
        put_u64(b, status::DELAY, if st.running() { st.delay() as u64 } else { 0 });
        put_u64(b, status::AVAIL_MAX, st.avail_max);
        st.avail_max = 0;
    }

    /// `snd_pcm_sync_ptr` (pcm_native.c:3034).
    fn sync_ptr(&self, b: &mut [u8]) -> Result<(), Errno> {
        let flags = get_u32(b, sync::FLAGS);
        let (appl, avail_min) = (get_u64(b, sync::APPL_PTR), get_u64(b, sync::AVAIL_MIN));
        b.fill(0);
        put_u32(b, sync::FLAGS, flags);
        let mut st = self.st.lock();
        if flags & SYNC_PTR_HWSYNC != 0 {
            st.hwsync()?;
        }
        if flags & SYNC_PTR_APPL == 0 {
            xfer::apply_appl(&mut st, appl)?;
        }
        if flags & SYNC_PTR_AVAIL_MIN == 0 {
            st.sw.avail_min = avail_min;
        }
        put_u64(b, sync::APPL_PTR, st.appl_ptr());
        put_u64(b, sync::AVAIL_MIN, st.sw.avail_min);
        put_u32(b, sync::STATE, st.state());
        put_u64(b, sync::HW_PTR, st.hw_ptr());
        put_ts(b, sync::TSTAMP, st.tstamp_ns);
        put_ts(b, sync::AUDIO_TSTAMP, st.audio_ns);
        Ok(())
    }

    fn prepare(&self) -> Result<(), Errno> {
        let mut st = self.st.lock();
        match st.state() {
            STATE_OPEN | STATE_DISCONNECTED => return Err(EBADFD),
            STATE_RUNNING | STATE_DRAINING => return Err(EBUSY),
            _ => {}
        }
        let later = st.reset();
        st.phase = Phase::Active;
        drop(st);
        later.run();
        self.poll.wake_all();
        Ok(())
    }

    fn start(&self) -> Result<(), Errno> {
        let mut st = self.st.lock();
        if st.state() != STATE_PREPARED {
            return Err(EBADFD);
        }
        if st.hw_avail() == 0 {
            return Err(EPIPE);
        }
        if let Some(r) = &st.run {
            r.pb.start();
        }
        self.trigger(&mut st);
        drop(st);
        self.poll.wake_all();
        Ok(())
    }

    /// `snd_pcm_drop` (pcm_native.c:2205): everything queued is dropped, SETUP.
    pub(super) fn drop_stream(&self) -> Result<(), Errno> {
        let mut st = self.st.lock();
        if matches!(st.state(), STATE_OPEN | STATE_DISCONNECTED) {
            return Err(EBADFD);
        }
        let later = st.reset();
        st.phase = Phase::Setup;
        self.trigger(&mut st);
        drop(st);
        later.run();
        self.poll.wake_all();
        Ok(())
    }

    fn pause(&self, push: bool) -> Result<(), Errno> {
        let mut st = self.st.lock();
        let want = if push { STATE_RUNNING } else { STATE_PAUSED };
        if st.state() != want {
            return Err(EBADFD);
        }
        let pb = st.run.as_ref().map(|r| r.pb.clone());
        self.trigger(&mut st);
        drop(st);
        if let Some(pb) = pb {
            pb.pause(push).map_err(|_| EBADFD)?;
        }
        self.poll.wake_all();
        Ok(())
    }

    /// `snd_pcm_reset` (pcm_native.c:1925): what is queued goes, the state stays.
    fn reset_ioctl(&self) -> Result<(), Errno> {
        let mut st = self.st.lock();
        let s = st.state();
        if !matches!(s, STATE_RUNNING | STATE_PREPARED | STATE_PAUSED) {
            return Err(EBADFD);
        }
        let later = st.reset();
        let pb = st.run.as_ref().map(|r| r.pb.clone());
        drop(st);
        later.run();
        if let Some(pb) = pb
            && s != STATE_PREPARED
        {
            pb.start();
            if s == STATE_PAUSED {
                let _ = pb.pause(true);
            }
        }
        self.poll.wake_all();
        Ok(())
    }

    /// `snd_pcm_xrun` (pcm_native.c:1850).
    fn xrun(&self) -> Result<(), Errno> {
        let mut st = self.st.lock();
        match st.state() {
            STATE_XRUN => Ok(()),
            STATE_RUNNING => {
                let later = st.reset();
                st.phase = Phase::Xrun;
                self.trigger(&mut st);
                drop(st);
                later.run();
                self.poll.wake_all();
                klog::event!(Info, "snd", "pcm.xrun_forced", name = self.name.as_str());
                Ok(())
            }
            _ => Err(EBADFD),
        }
    }

    fn rewind_forward(&self, arg: u64, mem: &dyn UserMem, forward: bool) -> Result<i64, Errno> {
        let mut b = [0u8; 8];
        if !mem.read(arg, &mut b) || !mem.write(arg, &[0; 8]) {
            return Err(EFAULT);
        }
        let frames = u64::from_le_bytes(b);
        let r: Result<u64, Errno> = if frames == 0 {
            Ok(0)
        } else {
            let mut st = self.st.lock();
            match st.hwsync() {
                // NO_REWINDS: a backward move of appl_ptr is refused (pcm_lib.c:2179), so rewind does 0.
                Ok(()) if !forward => Ok(0),
                Ok(()) => xfer::forward(&mut st, frames),
                Err(e) => Err(e),
            }
        };
        let v = match r {
            Ok(n) => n as i64,
            Err(e) => -e,
        };
        if !mem.write(arg, &v.to_le_bytes()) {
            return Err(EFAULT);
        }
        r.map(|_| 0)
    }

    fn read_arg<const N: usize>(mem: &dyn UserMem, arg: u64) -> Result<[u8; N], Errno> {
        let mut b = [0u8; N];
        if mem.read(arg, &mut b) { Ok(b) } else { Err(EFAULT) }
    }
    fn put(mem: &dyn UserMem, arg: u64, b: &[u8]) -> Result<i64, Errno> {
        if mem.write(arg, b) { Ok(0) } else { Err(EFAULT) }
    }

    /// `snd_pcm_common_ioctl` (pcm_native.c:3288).
    pub fn ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        if (cmd >> 8) & 0xff != b'A' as u64 {
            return Err(ENOTTY);
        }
        if self.card.is_none() {
            return Err(ENXIO);
        }
        if self.st.lock().state() == STATE_DISCONNECTED {
            return Err(EBADFD);
        }
        match cmd {
            PCM_PVERSION => Self::put(mem, arg, &SNDRV_PCM_VERSION.to_le_bytes()),
            PCM_INFO => {
                let mut b = [0u8; PCM_INFO_SIZE];
                super::pcm_info(&mut b, 0);
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
                self.st.lock().user_pversion = u32::from_le_bytes(Self::read_arg::<4>(mem, arg)?);
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
                // No MMAP in the setup's info: offset -1 (pcm_lib.c:1732).
                b.fill(0);
                put_u32(&mut b, 0, ch);
                put_u64(&mut b, 8, u64::MAX);
                Self::put(mem, arg, &b)
            }
            PCM_PREPARE => self.prepare().map(|_| 0),
            PCM_RESET => self.reset_ioctl().map(|_| 0),
            PCM_START => self.start().map(|_| 0),
            PCM_DROP => self.drop_stream().map(|_| 0),
            PCM_DRAIN => xfer::drain(self, mem).map(|_| 0),
            // The argument is the value (push 1 / release 0), not a pointer.
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
            // A playback substream writes for both (`snd_pcm_xferi_frames_ioctl` looks at the stream only).
            PCM_WRITEI_FRAMES | PCM_READI_FRAMES => xfer::xferi(self, arg, mem),
            PCM_WRITEN_FRAMES | PCM_READN_FRAMES => {
                if self.st.lock().state() == STATE_OPEN {
                    return Err(EBADFD);
                }
                if !mem.write(arg, &[0; 8]) {
                    return Err(EFAULT);
                }
                // RW_INTERLEAVED only: the non-interleaved transfer is refused (pcm_lib.c:2230).
                Err(EINVAL)
            }
            _ => Err(ENOTTY),
        }
    }
}

impl File for PcmFile {
    fn open_node(&self, flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        if self.card.is_some() {
            return None;
        }
        Some(super::open_pcm(flags))
    }
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_stream(buf, self.nonblock())
    }
    /// `snd_pcm_write` (pcm_native.c): whole frames, counted in bytes.
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        let fb = {
            let st = self.st.lock();
            if st.state() == STATE_OPEN {
                return Err(EBADFD);
            }
            st.run.as_ref().map_or(1, |r| r.frame_bytes)
        };
        if buf.len() % fb != 0 {
            return Err(EINVAL);
        }
        xfer::write(self, Src::Kernel(buf), (buf.len() / fb) as u64, Some(nonblock), None).map(|n| n as usize * fb)
    }
    /// 0666: Linux has root:audio 0660 plus the seat's ACL (uaccess); V2 has neither, B3's policy check on
    /// open is the gate.
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(super::ALSA_MAJOR, super::PCM_MINOR)
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        if (cmd >> 8) & 0xff == b'A' as u64 {
            self.calls[(cmd & 0x7f) as usize].fetch_add(1, Ordering::Relaxed);
        }
        let r = self.ioctl(cmd, arg, mem);
        if let Err(e) = r
            && cmd != PCM_WRITEI_FRAMES
        {
            klog::event!(Debug, "snd", "pcm.ioctl", cmd = format!("{cmd:#x}").as_str(), errno = e);
        }
        Some(r)
    }
    /// `snd_pcm_poll` (pcm_native.c:3590).
    fn events(&self) -> Events {
        let ok = Events::OUT | Events::WRNORM;
        if self.card.is_none() {
            return Events(ok | Events::ERR);
        }
        let mut st = self.st.lock();
        let avail = st.avail();
        Events(match st.state() {
            STATE_RUNNING | STATE_PREPARED | STATE_PAUSED if avail >= st.sw.avail_min => ok,
            STATE_RUNNING | STATE_PREPARED | STATE_PAUSED | STATE_DRAINING => 0,
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
            out.push_str(&format!("alsa-setup:\t{} Hz {} ch format {} period {} buffer {}\nalsa-silence:\t{} {}\n", r.rate, r.channels, r.format, r.period, r.buffer, st.sw.silence_threshold, st.sw.silence_size));
        }
    }
}

impl Drop for PcmFile {
    fn drop(&mut self) {
        if self.card.is_some() {
            let st = self.st.lock();
            let mut calls = String::new();
            for (nr, c) in self.calls.iter().enumerate() {
                let c = c.load(Ordering::Relaxed);
                if c > 0 {
                    calls.push_str(&format!("{}{nr:02x}:{c}", if calls.is_empty() { "" } else { "," }));
                }
            }
            klog::event!(Info, "snd", "pcm.close", name = self.name.as_str(), state = st.state(), appl = st.appl, ioctls = calls.as_str());
        }
    }
}

pub(super) fn node() -> Arc<dyn File> {
    Arc::new(PcmFile::node())
}

/// A zeroed kernel buffer, fallibly (sizes come from the program).
pub(super) fn try_buf(n: usize) -> Result<Vec<u8>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    v.resize(n, 0);
    Ok(v)
}
