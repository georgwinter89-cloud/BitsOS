//! SN-9: recording. A `Capture` reads 48 kHz 16-bit stereo frames from one `Source` (a mono microphone on both
//! channels): the USB headset's microphone (`usb_audio::open_capture`, SN-8b) or an hda input (`Controller::
//! open_input`: the ALC269's mic jack, QEMU's line in), see `capsrc`. No mixer thread is involved: the source's
//! completion (USB) or period callback (hda) only kicks the reader's wait queue and poll queue.
//!
//! The card-wide capture switch and volume (ALSA "Capture Switch"/"Capture Volume") are applied in software
//! as the frames are read (Q15 like the master volume).

use crate::capsrc;
use crate::client::{SndError, UNITY};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use devmodel::audio::PcmFormat;
use ksync::WaitQueue;
use vfs::poll::PollQueue;

/// Called by a source when frames arrived (or it went away): only kicks.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Where frames come from. Every call is short and never sleeps on the device (`available`/`overruns` may be
/// called under the ALSA file's spin lock: they read atomics or a leaf spin lock).
pub trait Source: Send + Sync {
    /// For logs: `usb:xhci0-1/3`, `hda0/0:0x18`.
    fn name(&self) -> String;
    fn start(&self) -> Result<(), SndError>;
    fn stop(&self);
    /// Takes up to `out.len()` frames; never blocks. `Closed` once the device is gone and nothing is left.
    fn read(&self, out: &mut [[i16; 2]]) -> Result<usize, SndError>;
    fn available(&self) -> usize;
    /// Frames lost since open because the reader fell behind the source's ring.
    fn overruns(&self) -> u64;
    /// The device is gone (unplugged, controller quiesced).
    fn gone(&self) -> bool;
    /// Frames between the microphone and `available` (a controller FIFO).
    fn latency(&self) -> u32 {
        0
    }
    fn close(self: Box<Self>);
}

/// Opens sources: the system's inputs, or a fake in tests.
pub trait Inputs: Send + Sync {
    fn open(&self, ring: u32, period: u32, wake: Wake) -> Result<Box<dyn Source>, SndError>;
}

/// Capture switch per channel and volume (Q15) for every capture (ALSA card controls).
static SWITCH: [AtomicBool; 2] = [AtomicBool::new(true), AtomicBool::new(true)];
static VOLUME: [AtomicU32; 2] = [AtomicU32::new(UNITY), AtomicU32::new(UNITY)];

pub fn gain() -> ([bool; 2], [u32; 2]) {
    let sw = [SWITCH[0].load(Ordering::Acquire), SWITCH[1].load(Ordering::Acquire)];
    let vol = [VOLUME[0].load(Ordering::Acquire), VOLUME[1].load(Ordering::Acquire)];
    (sw, vol)
}

pub fn set_gain(sw: [bool; 2], vol: [u32; 2]) {
    for c in 0..2 {
        SWITCH[c].store(sw[c], Ordering::Release);
        VOLUME[c].store(vol[c].min(UNITY), Ordering::Release);
    }
}

/// The reader's wake-ups.
pub struct Waker {
    wq: WaitQueue,
    poll: Option<Arc<PollQueue>>,
    kicks: AtomicU64,
}

impl Waker {
    pub fn kick(&self) {
        self.kicks.fetch_add(1, Ordering::AcqRel);
        self.wq.wake_all();
        if let Some(p) = &self.poll {
            p.wake_all();
        }
    }
}

pub struct Capture {
    src: Option<Box<dyn Source>>,
    waker: Arc<Waker>,
    name: String,
    running: AtomicBool,
    /// Source overruns before the last start (ALSA counts only those of the running stream).
    base_overruns: AtomicU64,
    frames: AtomicU64,
}

/// Opens a capture on the system's inputs (USB headset first, then hda): `ring` frames, not running yet.
/// Thread context (USB control transfers).
pub fn open(fmt: PcmFormat, ring: u32) -> Result<Capture, SndError> {
    open_on(&capsrc::SysInputs, fmt, ring, (ring / 4).max(1), "kernel", None)
}

/// `open` on `inputs`, waking `poll` too (the ALSA file's poll queue).
pub fn open_on(inputs: &dyn Inputs, fmt: PcmFormat, ring: u32, period: u32, name: &str, poll: Option<Arc<PollQueue>>) -> Result<Capture, SndError> {
    if fmt.rate != 48000 || fmt.bits != 16 || !(1..=2).contains(&fmt.channels) {
        return Err(SndError::BadFormat);
    }
    if ring == 0 || ring as usize * 4 > crate::client::MAX_RING_BYTES {
        return Err(SndError::BadSize);
    }
    let waker = Arc::new(Waker { wq: WaitQueue::new(), poll, kicks: AtomicU64::new(0) });
    let w = waker.clone();
    let src = inputs.open(ring, period.clamp(1, ring), Arc::new(move || w.kick()))?;
    klog::event!(Info, "snd", "cap.open", name = name, source = src.name().as_str(), ring = ring, period = period);
    Ok(Capture { src: Some(src), waker, name: name.into(), running: AtomicBool::new(false), base_overruns: AtomicU64::new(0), frames: AtomicU64::new(0) })
}

impl Capture {
    fn src(&self) -> &dyn Source {
        // Present until drop.
        self.src.as_deref().expect("capture source")
    }

    pub fn source_name(&self) -> String {
        self.src().name()
    }

    /// Starts recording: what came in before is dropped (ALSA: the ring holds only frames since the start).
    pub fn start(&self) -> Result<(), SndError> {
        self.discard(self.avail());
        self.base_overruns.store(self.src().overruns(), Ordering::Release);
        self.running.store(true, Ordering::Release);
        self.src().start().inspect_err(|_| self.running.store(false, Ordering::Release))
    }

    /// Continues after a pause: what waited stays and the overrun count runs on (ALSA's hw_ptr never goes back).
    pub fn resume(&self) -> Result<(), SndError> {
        self.running.store(true, Ordering::Release);
        self.src().start().inspect_err(|_| self.running.store(false, Ordering::Release))
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.src().stop();
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Takes recorded frames with the capture switch and volume applied; never blocks.
    pub fn read(&self, out: &mut [[i16; 2]]) -> Result<usize, SndError> {
        let n = self.src().read(out)?;
        let (sw, vol) = gain();
        let g = [vol[0] * sw[0] as u32, vol[1] * sw[1] as u32];
        if g != [UNITY; 2] {
            for f in &mut out[..n] {
                for c in 0..2 {
                    f[c] = ((f[c] as i32 * g[c] as i32 + (1 << 14)) >> 15) as i16;
                }
            }
        }
        self.frames.fetch_add(n as u64, Ordering::AcqRel);
        Ok(n)
    }

    /// Drops up to `frames` recorded frames; returns how many.
    pub fn discard(&self, frames: usize) -> usize {
        let mut buf = [[0i16; 2]; 256];
        let mut done = 0;
        while done < frames {
            let want = (frames - done).min(buf.len());
            match self.src().read(&mut buf[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => done += n,
            }
        }
        done
    }

    /// Frames waiting to be read.
    pub fn avail(&self) -> usize {
        self.src().available()
    }

    /// Frames between the microphone and the reader: waiting ones plus the device's own.
    pub fn delay(&self) -> u64 {
        self.avail() as u64 + self.src().latency() as u64
    }

    /// Frames the device holds before they are waiting (a controller FIFO).
    pub fn latency(&self) -> u32 {
        self.src().latency()
    }

    /// Frames lost since the last start.
    pub fn overruns(&self) -> u64 {
        self.src().overruns().saturating_sub(self.base_overruns.load(Ordering::Acquire))
    }

    pub fn gone(&self) -> bool {
        self.src().gone()
    }

    /// Frames read since open.
    pub fn frames_read(&self) -> u64 {
        self.frames.load(Ordering::Acquire)
    }

    /// Waits up to `timeout_ns` (one bounded slice, B4) until `frames` are waiting or the source is gone.
    pub fn wait(&self, frames: usize, timeout_ns: u64) -> bool {
        let src = self.src();
        self.waker.wq.wait_until_timeout(|| src.available() >= frames || src.gone(), timeout_ns);
        src.available() >= frames
    }

    /// Wake-ups the source sent (tests).
    pub fn kicks(&self) -> u64 {
        self.waker.kicks.load(Ordering::Acquire)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(s) = self.src.take() {
            let (name, overruns) = (s.name(), s.overruns());
            s.close();
            klog::event!(Info, "snd", "cap.close", name = self.name.as_str(), source = name.as_str(), frames = self.frames.load(Ordering::Acquire),
                overruns = overruns);
        }
        self.waker.kick();
    }
}
