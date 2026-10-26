//! A mixer client (one per open PCM in SN-4): its ring at its own rate, volume, state and counters. Writers
//! never block the mixer: each side holds the ring lock for one bounded copy. `Playback` is the owner's
//! handle; dropping it closes the client.

use crate::mixer::Shared;
use crate::resample::{self, Resampler, Table};
use crate::ring::Ring;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use devmodel::audio::PcmFormat;
use ksync::{SpinLock, WaitQueue};
use vfs::poll::PollQueue;

/// Q15 gain: 32768 is 0 dB (unity), 0 is mute; the mixer applies `(sample * gain + 2^14) >> 15` (round to nearest).
pub const UNITY: u32 = 32768;
/// Frames copied per lock hold (a long write is split, so the mixer waits at most this long).
const CHUNK: usize = 4096;
/// A single client's ring may hold at most this many bytes (a size bound against a hostile request, as hda's
/// 32 MiB per stream; at 48 kHz stereo 16 bit about 87 s).
pub const MAX_RING_BYTES: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Open, filling, not mixed yet.
    Prepared = 0,
    Running = 1,
    /// Kept, not mixed, no underrun counted.
    Paused = 2,
    /// Plays the rest, then `Drained`.
    Draining = 3,
    Drained = 4,
    Closed = 5,
    /// Ran dry while running with `set_stop_on_underrun(true)` (ALSA XRUN): not mixed; `write` and
    /// `wait_space` answer `Xrun` (EPIPE) until `drop_pending` (ALSA prepare).
    Xrun = 6,
}

impl State {
    fn from(v: u8) -> State {
        match v {
            1 => State::Running,
            2 => State::Paused,
            3 => State::Draining,
            4 => State::Drained,
            5 => State::Closed,
            6 => State::Xrun,
            _ => State::Prepared,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            State::Prepared => "prepared",
            State::Running => "running",
            State::Paused => "paused",
            State::Draining => "draining",
            State::Drained => "drained",
            State::Closed => "closed",
            State::Xrun => "xrun",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SndError {
    /// Not 16 bit, not 1 or 2 channels, or a rate outside `resample::RATES`.
    BadFormat,
    /// Ring of 0 frames or above `MAX_RING_BYTES`.
    BadSize,
    NoMemory,
    /// The mixer thread could not start.
    NoMixer,
    Closed,
    /// The call does not fit the state (ALSA -EBADFD): write after a drain started or once closed, pause
    /// when not running, resume when not paused.
    BadState,
    /// The client ran dry with stop-on-underrun (ALSA -EPIPE); `drop_pending` makes it usable again.
    Xrun,
    /// SN-9: no microphone or line input to record from (ALSA -ENODEV).
    NoInput,
    /// SN-9: every input is being recorded by another program (ALSA -EBUSY).
    Busy,
}

pub struct Client {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) fmt: PcmFormat,
    pub(crate) table: Option<Arc<Table>>,
    /// The filter state, made at open (the caller's context) and taken by the mixer thread, which then never
    /// allocates for a client.
    pub(crate) rs: SpinLock<Option<Resampler>>,
    pub(crate) ring: SpinLock<Ring>,
    /// ALSA stop_threshold = buffer size (true) or boundary (false, the default: run dry = silence).
    pub(crate) stop_on_underrun: AtomicBool,
    pub(crate) gain: [AtomicU32; 2],
    state: AtomicU8,
    pub(crate) consumed: AtomicU64,
    pub(crate) underruns: AtomicU64,
    pub(crate) silence: AtomicU64,
    /// Bumped by `drop_pending`: the mixer then forgets the filter history.
    pub(crate) reset_gen: AtomicU64,
    pub(crate) space: WaitQueue,
    pub(crate) done: WaitQueue,
    pub(crate) shared: Arc<Shared>,
    /// SN-4: the PCM file's poll queue, woken with `space` and `done` (avail, xrun and drain end change the
    /// poll mask). It belongs to the open file (its queue must outlive a hw_free/hw_params).
    pub(crate) poll: Option<Arc<PollQueue>>,
}

impl Client {
    pub(crate) fn new(shared: Arc<Shared>, id: u64, name: &str, fmt: PcmFormat, ring_frames: u32) -> Result<Client, SndError> {
        if fmt.bits != 16 || !(1..=2).contains(&fmt.channels) || !resample::RATES.contains(&fmt.rate) {
            return Err(SndError::BadFormat);
        }
        if ring_frames == 0 || ring_frames as usize * 4 > MAX_RING_BYTES {
            return Err(SndError::BadSize);
        }
        let (table, rs) = if fmt.rate != resample::OUT_RATE {
            let t = resample::table(fmt.rate).ok_or(SndError::NoMemory)?;
            let r = Resampler::try_new(t.clone()).ok_or(SndError::NoMemory)?;
            (Some(t), Some(r))
        } else {
            (None, None)
        };
        let ring = Ring::try_new(ring_frames as usize).ok_or(SndError::NoMemory)?;
        Ok(Client {
            id,
            name: name.into(),
            fmt,
            table,
            rs: SpinLock::named("snd.client.rs", rs),
            ring: SpinLock::named("snd.client.ring", ring),
            stop_on_underrun: AtomicBool::new(false),
            gain: [AtomicU32::new(UNITY), AtomicU32::new(UNITY)],
            state: AtomicU8::new(State::Prepared as u8),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            silence: AtomicU64::new(0),
            reset_gen: AtomicU64::new(0),
            space: WaitQueue::new(),
            done: WaitQueue::new(),
            shared,
            poll: None,
        })
    }

    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }
    pub(crate) fn set_state(&self, s: State) {
        self.state.store(s as u8, Ordering::Release);
    }
    /// Wakes the owner's poll queue (SN-4), if it has one.
    pub(crate) fn kick_poll(&self) {
        if let Some(p) = &self.poll {
            p.wake_all();
        }
    }
    /// Changes the state only from `from`.
    pub(crate) fn move_state(&self, from: State, to: State) -> bool {
        self.state.compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
}

/// The owner's side of a client.
pub struct Playback(pub(crate) Arc<Client>);

impl Playback {
    pub fn id(&self) -> u64 {
        self.0.id
    }
    pub fn format(&self) -> PcmFormat {
        self.0.fmt
    }
    pub fn state(&self) -> State {
        self.0.state()
    }
    pub fn ring_frames(&self) -> usize {
        self.0.ring.lock().capacity()
    }
    /// Free frames in the ring.
    pub fn avail(&self) -> usize {
        self.0.ring.lock().free()
    }
    /// Frames in the ring not yet taken by the mixer.
    pub fn queued(&self) -> usize {
        self.0.ring.lock().len()
    }

    /// Copies whole frames (16-bit little-endian, the client's channels) into the ring; returns the bytes taken
    /// (less than given when the ring is full). Allowed as ALSA's pcm_accessible_state: prepared, running,
    /// paused; `Xrun` after a stop-on-underrun, `BadState` once a drain started or closed.
    pub fn write(&self, data: &[u8]) -> Result<usize, SndError> {
        self.writable()?;
        let ch = self.0.fmt.channels as usize;
        let mut done = 0;
        loop {
            let n = self.0.ring.lock().push_bytes(&data[done * 2 * ch..], ch, CHUNK);
            done += n;
            if n < CHUNK {
                break;
            }
        }
        Ok(done * 2 * ch)
    }

    /// `write` for stereo frames; returns the frames taken.
    pub fn write_frames(&self, frames: &[[i16; 2]]) -> Result<usize, SndError> {
        self.writable()?;
        let mut done = 0;
        while done < frames.len() {
            let end = (done + CHUNK).min(frames.len());
            let n = self.0.ring.lock().push_frames(&frames[done..end]);
            let want = end - done;
            done += n;
            if n < want {
                break;
            }
        }
        Ok(done)
    }

    fn writable(&self) -> Result<(), SndError> {
        match self.state() {
            State::Prepared | State::Running | State::Paused => Ok(()),
            State::Xrun => Err(SndError::Xrun),
            State::Draining | State::Drained | State::Closed => Err(SndError::BadState),
        }
    }

    /// Waits until at least `frames` (capped at the ring size) are free: Ok(true), or Ok(false) at the
    /// deadline (B4). A paused client waits too (it frees nothing until resumed, as ALSA's poll). Ends at once
    /// with the error `write` would give (`Xrun`, `BadState`) when writing is no longer possible.
    pub fn wait_space(&self, frames: usize, timeout_ns: u64) -> Result<bool, SndError> {
        let c = &self.0;
        let want = frames.min(c.ring.lock().capacity());
        let ready = || c.ring.lock().free() >= want || self.writable().is_err();
        c.space.wait_until_timeout(ready, timeout_ns);
        self.writable()?;
        Ok(c.ring.lock().free() >= want)
    }

    /// ALSA stop_threshold: true = a client that runs dry while running goes to `Xrun` (EPIPE for SN-4);
    /// false (default) = it gets silence and plays on when written again. Both count the underrun.
    pub fn set_stop_on_underrun(&self, on: bool) {
        self.0.stop_on_underrun.store(on, Ordering::Release);
    }

    /// Prepared or paused -> running (ALSA start / pause release).
    pub fn start(&self) {
        crate::mixer::start_together(&self.0.shared, &[self]);
    }

    /// Running -> paused (on) or paused -> running (off); keeps the ring, no underruns counted while paused.
    /// `BadState` from any other state (ALSA snd_pcm_pre_pause, pcm_native.c:1594-1598).
    pub fn pause(&self, on: bool) -> Result<(), SndError> {
        let ok = if on { self.0.move_state(State::Running, State::Paused) } else { self.0.move_state(State::Paused, State::Running) };
        if !ok {
            return Err(SndError::BadState);
        }
        self.0.shared.kick();
        self.0.space.wake_all();
        self.0.kick_poll();
        Ok(())
    }

    /// Drops what is queued and goes back to prepared (ALSA drop + prepare); `position` restarts at 0. The
    /// state changes by compare-exchange only from a live state, so it never overwrites `Closed` and a
    /// concurrent mixer transition (Draining -> Drained, Running -> Xrun) is either seen before or redone
    /// from. `reset_gen` is bumped first: the mixer sees it no later than the new state and forgets the
    /// filter history and a pending drain end before it mixes the client again.
    pub fn drop_pending(&self) {
        self.reset_ring();
        self.wake_after_reset();
    }

    /// `drop_pending` without its wakes: takes only the ring lock (a leaf), so SN-4 runs it under `snd.pcm`
    /// together with its own pointer reset, and a writer never sees appl_ptr reset while the old ring is
    /// still full. The caller must call `wake_after_reset` after its unlock.
    pub(crate) fn reset_ring(&self) {
        let c = &self.0;
        c.ring.lock().clear();
        c.reset_gen.fetch_add(1, Ordering::AcqRel);
        loop {
            let s = c.state();
            if matches!(s, State::Prepared | State::Closed) || c.move_state(s, State::Prepared) {
                break;
            }
        }
    }

    pub(crate) fn wake_after_reset(&self) {
        let c = &self.0;
        c.done.wake_all();
        c.space.wake_all();
        c.kick_poll();
    }

    /// Plays what is queued, then the client is drained. Returns true once drained, false on timeout (it
    /// keeps draining). A client that never started is started.
    pub fn drain(&self, timeout_ns: u64) -> bool {
        let c = &self.0;
        self.start_drain();
        c.done.wait_until_timeout(|| !matches!(c.state(), State::Draining), timeout_ns);
        c.state() == State::Drained
    }

    /// `drain` without waiting (non-blocking ALSA drain); `state()` turns `Drained` when done.
    pub fn start_drain(&self) {
        let c = &self.0;
        if c.state() == State::Prepared {
            self.start();
        }
        if c.move_state(State::Running, State::Draining) || c.move_state(State::Paused, State::Draining) {
            c.shared.kick();
        }
        // Nothing left to play after an xrun (ALSA's drain ends at once there).
        if c.move_state(State::Xrun, State::Drained) {
            c.done.wake_all();
        }
    }

    /// Frames (client rate) the mixer has taken from the ring since open or the last `drop_pending` (ALSA
    /// hw_ptr, which restarts at 0 on prepare: pcm_native.c snd_pcm_do_reset).
    pub fn position(&self) -> u64 {
        self.0.ring.lock().position()
    }

    /// Frames (client rate) between the write position and the speaker: the ring, the filter's half length,
    /// what the mixer queued at the output and the output has not played yet.
    pub fn delay(&self) -> u64 {
        let c = &self.0;
        let ring = c.ring.lock().len() as u64;
        let filt = c.table.as_ref().map_or(0, |t| t.taps as u64 / 2);
        let out = c.shared.queued_now();
        ring + filt + out * c.fmt.rate as u64 / resample::OUT_RATE as u64
    }

    /// Per-channel Q15 gain (`UNITY` = 0 dB); values above unity are clamped.
    pub fn set_volume(&self, g: [u32; 2]) {
        for (a, v) in self.0.gain.iter().zip(g) {
            a.store(v.min(UNITY), Ordering::Release);
        }
    }
    pub fn volume(&self) -> [u32; 2] {
        [self.0.gain[0].load(Ordering::Acquire), self.0.gain[1].load(Ordering::Acquire)]
    }

    /// Times the mixer found the ring empty while running (each starvation once), and the silent frames
    /// (client rate) it put in instead.
    pub fn underruns(&self) -> (u64, u64) {
        (self.0.underruns.load(Ordering::Acquire), self.0.silence.load(Ordering::Acquire))
    }

    pub(crate) fn client(&self) -> &Arc<Client> {
        &self.0
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let c = &self.0;
        c.set_state(State::Closed);
        c.done.wake_all();
        c.space.wake_all();
        c.kick_poll();
        c.shared.kick();
        klog::event!(Info, "snd", "client.close", id = c.id, name = c.name.as_str(), consumed = c.consumed.load(Ordering::Acquire),
            underruns = c.underruns.load(Ordering::Acquire), silence = c.silence.load(Ordering::Acquire));
    }
}

/// Opens a client on the mixer behind `sh` (not running until `start`).
pub fn open(sh: &Arc<Shared>, fmt: PcmFormat, ring_frames: u32, name: &str) -> Result<Playback, SndError> {
    open_with(sh, fmt, ring_frames, name, None)
}

/// `open` with a poll queue the client wakes whenever its space, xrun or drain state changes (SN-4's PCM file).
pub fn open_with(sh: &Arc<Shared>, fmt: PcmFormat, ring_frames: u32, name: &str, poll: Option<Arc<PollQueue>>) -> Result<Playback, SndError> {
    let id = sh.next_id.fetch_add(1, Ordering::AcqRel);
    let nomem = |what: &str| {
        klog::event!(Warn, "snd", "client.nomem", id = id, name = name, what = what, ring = ring_frames, rate = fmt.rate);
        SndError::NoMemory
    };
    let c = match Client::new(sh.clone(), id, name, fmt, ring_frames) {
        Ok(mut c) => {
            c.poll = poll;
            Arc::new(c)
        }
        Err(SndError::NoMemory) => return Err(nomem("ring")),
        Err(e) => return Err(e),
    };
    {
        let mut g = sh.clients.lock();
        if g.try_reserve(1).is_err() {
            drop(g);
            return Err(nomem("list"));
        }
        g.push(c.clone());
    }
    klog::event!(Info, "snd", "client.open", id = id, name = name, rate = fmt.rate, channels = fmt.channels, ring = ring_frames,
        resampled = c.table.is_some());
    Ok(Playback(c))
}
