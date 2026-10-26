//! SN-4 host tests: the ABI numbers (checked against CachyOS's <sound/asound.h> by a one-off C program, values
//! below), the refine engine as libasound drives it, the PCM state machine on a mixer core stepped by hand
//! (null output), the sample conversions, the control device's read side.
extern crate std;

use crate::alsa::abi::*;
use crate::alsa::params::{self, HwParams, Interval, Mask};
use crate::alsa::testkit::{self, Status, T, DATA};
use crate::alsa::umem::Arena;
use crate::alsa::{constraints, f32_to_s16, Card, PcmFile};
use crate::backend::{Backend, Endpoint, OutError, Stream, Target, WakeFn};
use crate::client::{self, Playback, SndError};
use crate::mixer::{Config, Core, Shared};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::audio::PcmFormat;
use std::sync::Mutex;
use core::sync::atomic::Ordering;
use uapi::errno::*;
use vfs::poll::PollQueue;

#[test]
fn ioctl_numbers_match_linux() {
    for (v, want) in [
        (PCM_PVERSION, 0x80044100u64),
        (PCM_INFO, 0x81204101),
        (PCM_HW_REFINE, 0xc2604110),
        (PCM_SW_PARAMS, 0xc0884113),
        (PCM_STATUS, 0x80984120),
        (PCM_STATUS_EXT, 0xc0984124),
        (PCM_SYNC_PTR, 0xc0884123),
        (PCM_WRITEI_FRAMES, 0x40184150),
        (PCM_READI_FRAMES, 0x80184151),
        (PCM_FORWARD, 0x40084149),
        (PCM_CHANNEL_INFO, 0x80184132),
        (PCM_DELAY, 0x80084121),
        (PCM_REWIND, 0x40084146),
        (PCM_LINK, 0x40044160),
        (PCM_PAUSE, 0x40044145),
        (PCM_TTSTAMP, 0x40044103),
        (PCM_USER_PVERSION, 0x40044104),
        (CTL_CARD_INFO, 0x81785501),
        (CTL_ELEM_LIST, 0xc0505510),
        (CTL_ELEM_INFO, 0xc1105511),
        (CTL_ELEM_READ, 0xc4c85512),
        (CTL_ELEM_WRITE, 0xc4c85513),
        (CTL_SUBSCRIBE_EVENTS, 0xc0045516),
        (CTL_TLV_READ, 0xc008551a),
        (CTL_PCM_NEXT_DEVICE, 0x80045530),
        (CTL_PCM_INFO, 0xc1205531),
        (CTL_PCM_PREFER_SUBDEVICE, 0x40045532),
        (CTL_POWER_STATE, 0x800455d1),
    ] {
        assert_eq!(v, want, "{v:#x} != {want:#x}");
    }
    assert_eq!(CTL_EVENT_SIZE, 0x48);
}

fn refined(p: &mut HwParams) {
    constraints().refine(p).unwrap();
    constraints().fixup(p);
}

#[test]
fn refine_any_shows_the_device() {
    let mut p = HwParams::any();
    refined(&mut p);
    assert_eq!(p.masks[params::ACCESS].0[0], 1 << ACCESS_RW_INTERLEAVED, "no mmap access");
    assert_eq!(p.masks[params::FORMAT].0[0], (1 << FORMAT_S16_LE) | (1 << FORMAT_S32_LE) | (1 << FORMAT_FLOAT_LE));
    let iv = |k| *p.iv(k);
    assert_eq!((iv(params::CHANNELS).min, iv(params::CHANNELS).max), (1, 2));
    assert_eq!((iv(params::RATE).min, iv(params::RATE).max), (8000, 192000));
    assert_eq!((iv(params::SAMPLE_BITS).min, iv(params::SAMPLE_BITS).max), (16, 32));
    assert_eq!((iv(params::FRAME_BITS).min, iv(params::FRAME_BITS).max), (16, 64));
    assert_eq!(iv(params::BUFFER_BYTES).max, 4 << 20);
    assert_eq!(p.info & (INFO_MMAP | INFO_MMAP_VALID), 0);
    assert_ne!(p.info & INFO_PAUSE, 0);
    assert_eq!(p.rmask, 0);
    assert!(p.cmask & (1 << params::FORMAT) != 0);
}

#[test]
fn refine_rate_list_and_bad_requests() {
    // 44.1 kHz is a rate; 44.2 kHz is not in the list: EINVAL as Linux's snd_interval_list.
    let mut p = HwParams::decode(&testkit::hw(FORMAT_S16_LE, 44100, 2, 0, 0));
    refined(&mut p);
    assert!(p.iv(params::RATE).single() && p.rate_num == 44100 && p.rate_den == 1);
    let mut p = HwParams::decode(&testkit::hw(FORMAT_S16_LE, 44200, 2, 0, 0));
    assert_eq!(constraints().refine(&mut p), Err(EINVAL));
    let mut p = HwParams::decode(&testkit::hw(FORMAT_S16_LE, 48000, 6, 0, 0));
    assert_eq!(constraints().refine(&mut p), Err(EINVAL));
    // U8 is not offered.
    let mut p = HwParams::decode(&testkit::hw(1, 48000, 2, 0, 0));
    assert_eq!(constraints().refine(&mut p), Err(EINVAL));
    // An empty mask in the request is EINVAL before anything is refined.
    let mut p = HwParams::any();
    p.masks[params::SUBFORMAT] = Mask([0; 8]);
    assert_eq!(constraints().refine(&mut p), Err(EINVAL));
}

#[test]
fn hw_params_choose_as_linux() {
    // Format, rate, channels set only: min period time, then the largest buffer (periods_max periods).
    let mut p = HwParams::decode(&testkit::hw(FORMAT_S16_LE, 48000, 2, 0, 0));
    constraints().hw_params(&mut p).unwrap();
    let (per, buf, pers) = (p.get(params::PERIOD_SIZE), p.get(params::BUFFER_SIZE), p.get(params::PERIODS));
    assert_eq!(per, 16, "period_bytes_min 64 / 4 bytes per frame");
    assert_eq!(pers, 1024);
    assert_eq!(buf, 16 * 1024);
    assert_eq!(p.get(params::PERIOD_BYTES), 64);
    assert_eq!(p.get(params::BUFFER_BYTES), 65536);
    for k in [params::FORMAT, params::ACCESS, params::SUBFORMAT] {
        assert!(p.masks[k].single());
    }
    assert!(p.iv(params::BUFFER_SIZE).single() && p.iv(params::PERIOD_SIZE).single());
    assert_eq!(p.msbits, 16);
    // Cobalt's 512-frame request on FLOAT_LE: buffer and period as set; msbits 16 (the mixer is 16-bit).
    let mut p = HwParams::decode(&testkit::hw(FORMAT_FLOAT_LE, 48000, 2, 0, 4096));
    constraints().hw_params(&mut p).unwrap();
    assert_eq!(p.get(params::BUFFER_SIZE), 4096);
    assert_eq!(p.get(params::BUFFER_SIZE) % p.get(params::PERIOD_SIZE), 0);
    assert_eq!(p.msbits, 16);
}

#[test]
fn buffer_time_near_like_aplay() {
    // aplay: buffer_time <= 500 ms, period_time = buffer_time / 4 (set_*_near: refine with a range).
    let mut p = HwParams::decode(&testkit::hw(FORMAT_S16_LE, 44100, 2, 0, 0));
    *p.iv_mut(params::BUFFER_TIME) = Interval { openmax: false, ..Interval::range(0, 500_000) };
    p.rmask = 1 << params::BUFFER_TIME;
    refined(&mut p);
    assert_eq!(p.iv(params::BUFFER_SIZE).max, 22050);
    // 22050 / 4 is not whole: period 5512 x 4 periods = 22048 frames (PERIODS is integer, as on hda).
    *p.iv_mut(params::PERIOD_SIZE) = Interval { integer: true, ..Interval::range(5512, 5512) };
    *p.iv_mut(params::PERIODS) = Interval { integer: true, ..Interval::range(4, 4) };
    p.rmask = (1 << params::PERIOD_SIZE) | (1 << params::PERIODS);
    constraints().hw_params(&mut p).unwrap();
    assert_eq!((p.get(params::PERIOD_SIZE), p.get(params::BUFFER_SIZE)), (5512, 22048));
    assert_eq!(p.get(params::BUFFER_TIME), 499954);
    // An exact 125 ms period at 44.1 kHz is 5512.5 frames: no whole period size fits, EINVAL as on Linux.
    let mut p = HwParams::decode(&testkit::hw(FORMAT_S16_LE, 44100, 2, 0, 0));
    *p.iv_mut(params::PERIOD_TIME) = Interval { integer: true, ..Interval::range(125_000, 125_000) };
    assert_eq!(constraints().hw_params(&mut p), Err(EINVAL));
}

#[test]
fn float_and_s32_conversion() {
    assert_eq!(f32_to_s16(0.5f32.to_bits()), 16384);
    assert_eq!(f32_to_s16((-0.5f32).to_bits()), -16384);
    assert_eq!(f32_to_s16(1.0f32.to_bits()), 32767);
    assert_eq!(f32_to_s16((-1.0f32).to_bits()), -32768);
    assert_eq!(f32_to_s16(2.5f32.to_bits()), 32767);
    assert_eq!(f32_to_s16(0.0f32.to_bits()), 0);
    assert_eq!(f32_to_s16(f32::NAN.to_bits()), 0);
    assert_eq!(f32_to_s16(f32::INFINITY.to_bits()), 32767);
    for i in -32768i32..=32767 {
        let f = i as f32 / 32768.0;
        assert_eq!(f32_to_s16(f.to_bits()) as i32, i, "{f}");
        let g = (i as f32 + 0.25) / 32768.0;
        assert_eq!(f32_to_s16(g.to_bits()) as i32, i, "{g} rounds to {i}");
    }
    use crate::alsa::xfer::to_s16;
    for (v, want) in [(0x7fff_ffffi32, 32767i16), (i32::MIN, -32768), (0x1234_7fff, 0x1234), (0x1234_8000, 0x1235), (-0x0001_8000, -1), (0, 0)] {
        assert_eq!(to_s16(FORMAT_S32_LE, &v.to_le_bytes()), want, "{v:#x}");
    }
    assert_eq!(to_s16(FORMAT_S16_LE, &(-5i16).to_le_bytes()), -5);
}

/// The null back end; the test steps the core by hand.
struct NoOut;
impl Backend for NoOut {
    fn resolve(&mut self, _: &Target) -> Option<Endpoint> {
        None
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        Vec::new()
    }
    fn open(&mut self, _: &Endpoint, _: u32, _: u32, _: WakeFn, _: u64) -> Result<Box<dyn Stream>, OutError> {
        Err(OutError::NoSuch)
    }
    fn take_jack_events(&mut self) -> bool {
        false
    }
}

struct HostCard(Arc<Shared>);
impl Card for HostCard {
    fn open(&self, fmt: PcmFormat, ring: u32, name: &str, poll: Option<Arc<PollQueue>>) -> Result<Playback, SndError> {
        client::open_with(&self.0, fmt, ring, name, poll)
    }
}

struct Rig {
    core: Mutex<Core<NoOut>>,
    now: Mutex<u64>,
    sh: Arc<Shared>,
}

fn rig() -> (Arc<Shared>, Rig) {
    let sh = Arc::new(Shared::new());
    let core = Core::new(NoOut, sh.clone(), Config::latency_ms(40));
    (sh.clone(), Rig { core: Mutex::new(core), now: Mutex::new(0), sh })
}

impl Rig {
    /// Lets `ms` of time pass in 1 ms steps (the null output plays 48 frames each).
    fn run(&self, ms: u64) {
        for _ in 0..ms {
            let mut n = self.now.lock().unwrap();
            *n += 1_000_000;
            self.sh.test_now.store(*n, Ordering::Release);
            self.core.lock().unwrap().step(*n);
        }
    }
}

fn stream(card: Arc<dyn Card>, flags: u64) -> (PcmFile, Arena) {
    (PcmFile::new(card, flags, "host".into()), Arena::new(1 << 20))
}

#[test]
fn pcm_state_machine() {
    let (sh, rig) = rig();
    let (f, m) = stream(Arc::new(HostCard(sh)), 0);
    let t = T { f: &f, m: &m };
    let (_, v) = t.with(PCM_PVERSION, &[0; 4]);
    assert_eq!(get_u32(&v, 0), SNDRV_PCM_VERSION);
    assert_eq!(t.status().state, STATE_OPEN);
    assert_eq!(t.writei(10).0, Err(EBADFD));
    assert_eq!(t.call(PCM_PREPARE, 0), Err(EBADFD));
    assert_eq!(t.with(PCM_SW_PARAMS, &testkit::sw(1, 4800, 960)).0, Err(EBADFD));
    let p = t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 960, 4800)).unwrap();
    assert_eq!((p.get(params::PERIOD_SIZE), p.get(params::BUFFER_SIZE)), (960, 4800));
    assert_eq!(t.status().state, STATE_SETUP);
    assert_eq!(t.writei(10).0, Err(EBADFD), "SETUP: not accessible");
    let (r, b) = t.with(PCM_SW_PARAMS, &testkit::sw(4800, 4800, 960));
    assert_eq!(r, Ok(0));
    assert!(get_u64(&b, sw::BOUNDARY) >= 4800 << 40 && get_u64(&b, sw::BOUNDARY) % 4800 == 0);
    assert_eq!(t.with(PCM_SW_PARAMS, &testkit::sw(1, 4800, 0)).0, Err(EINVAL), "avail_min 0");
    assert_eq!(t.with(PCM_SW_PARAMS, &testkit::sw(4800, 4800, 960)).0, Ok(0));
    assert_eq!(t.call(PCM_START, 0), Err(EBADFD));
    assert_eq!(t.call(PCM_PREPARE, 0), Ok(0));
    assert_eq!(t.call(PCM_START, 0), Err(EPIPE), "nothing queued");
    m.set(DATA, &[0x11; 4800 * 4]);
    assert_eq!(t.writei(960), (Ok(0), 960));
    assert_eq!(t.status(), Status { state: STATE_PREPARED, appl: 960, hw: 0, avail: 3840, delay: 0 });
    let d = [0u8; 8];
    assert_eq!(t.with(PCM_DELAY, &d).1, 960i64.to_le_bytes(), "prepared: delay = queued");
    // More than the mixer's lead (40 ms) before it starts, or the first round finds the ring short (XRUN).
    // start_threshold = buffer: the write that fills it starts the stream; START is then EBADFD.
    assert_eq!(t.writei(3840), (Ok(0), 3840));
    assert_eq!(t.status().state, STATE_RUNNING);
    assert_eq!(t.call(PCM_START, 0), Err(EBADFD));
    rig.run(5);
    let (r, state, hw, appl) = t.sync_ptr(SYNC_PTR_HWSYNC | SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN);
    assert_eq!((r, state, appl), (Ok(0), STATE_RUNNING, 4800));
    assert!(hw > 0 && hw <= 4800, "hw_ptr {hw}");
    let st = t.status();
    assert!(st.delay > (4800 - hw) as i64, "running: delay {} counts the mixer's queue too", st.delay);
    assert_eq!(t.call(PCM_PAUSE, 1), Ok(0));
    assert_eq!(t.status().state, STATE_PAUSED);
    assert_eq!(t.call(PCM_PAUSE, 1), Err(EBADFD));
    assert_eq!(t.call(PCM_PREPARE, 0), Ok(0), "prepare from PAUSED");
    assert_eq!(t.status(), Status { state: STATE_PREPARED, appl: 0, hw: 0, avail: 4800, delay: 0 });
    assert_eq!(t.writei(960).1, 960);
    t.call(PCM_START, 0).unwrap();
    assert_eq!(t.call(PCM_PREPARE, 0), Err(EBUSY), "running");
    assert_eq!(t.call(PCM_DROP, 0), Ok(0));
    assert_eq!(t.status().state, STATE_SETUP);
    assert_eq!(t.call(PCM_DRAIN, 0), Ok(0), "drain in SETUP: nothing to do");
    assert_eq!(t.call(PCM_RESUME, 0), Err(ENOSYS));
    assert_eq!(t.call(0x4100 | 0x7f, 0), Err(ENOTTY));
    assert_eq!(t.call(PCM_HW_FREE, 0), Ok(0));
    assert_eq!(t.status().state, STATE_OPEN);
    assert_eq!(t.call(PCM_HW_FREE, 0), Err(EBADFD));
}

#[test]
fn pcm_xrun_autostart_nonblock() {
    let (sh, rig) = rig();
    let (f, m) = stream(Arc::new(HostCard(sh)), vfs::abi::O_NONBLOCK);
    let t = T { f: &f, m: &m };
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 480, 1920)).unwrap();
    // libasound's defaults after hw_params: start_threshold 1, stop_threshold = buffer.
    t.call(PCM_PREPARE, 0).unwrap();
    m.set(DATA, &[0; 4096 * 4]);
    assert_eq!(t.writei(480).1, 480);
    assert_eq!(t.status().state, STATE_RUNNING, "start_threshold 1 started it");
    // Full ring, O_NONBLOCK: the rest is refused with EAGAIN, the part that fit counts.
    assert_eq!(t.writei(4096), (Ok(0), 1440));
    assert_eq!(t.writei(10), (Err(EAGAIN), -EAGAIN));
    // Nothing more written: the ring runs dry, stop_threshold = buffer is an XRUN.
    rig.run(200);
    assert_eq!(t.status().state, STATE_XRUN);
    assert_eq!(t.writei(10).0, Err(EPIPE));
    assert_eq!(t.sync_ptr(SYNC_PTR_HWSYNC).0, Err(EPIPE));
    assert_eq!(t.call(PCM_DRAIN, 0), Err(EAGAIN), "nonblocking drain");
    assert_eq!(t.status().state, STATE_SETUP, "drain of an XRUN ends in SETUP");
    t.call(PCM_PREPARE, 0).unwrap();
    // stop_threshold = boundary: running dry is silence, the stream stays RUNNING.
    let (_, b) = t.with(PCM_SW_PARAMS, &testkit::sw(1, u64::MAX, 480));
    let boundary = get_u64(&b, sw::BOUNDARY);
    assert_eq!(t.with(PCM_SW_PARAMS, &testkit::sw(1, boundary, 480)).0, Ok(0));
    assert_eq!(t.writei(480).1, 480);
    rig.run(200);
    assert_eq!(t.status().state, STATE_RUNNING);
    // XRUN forced, then rewind does nothing (NO_REWINDS) and forward fills silence.
    assert_eq!(t.call(PCM_XRUN, 0), Ok(0));
    assert_eq!(t.status().state, STATE_XRUN);
    t.call(PCM_PREPARE, 0).unwrap();
    let (r, out) = t.with(PCM_REWIND, &100u64.to_le_bytes());
    assert_eq!((r, out), (Ok(0), 0u64.to_le_bytes().to_vec()));
    let (r, out) = t.with(PCM_FORWARD, &100u64.to_le_bytes());
    assert_eq!((r, out), (Ok(0), 100u64.to_le_bytes().to_vec()));
    assert_eq!(t.status().appl, 100);
    // SYNC_PTR moving appl_ptr forward = silence; backward is refused.
    let mut s = [0u8; SYNC_PTR_SIZE];
    put_u64(&mut s, sync::APPL_PTR, 150);
    put_u64(&mut s, sync::AVAIL_MIN, 480);
    assert_eq!(t.with(PCM_SYNC_PTR, &s).0, Ok(0));
    assert_eq!(t.status().appl, 150);
    put_u64(&mut s, sync::APPL_PTR, 10);
    assert_eq!(t.with(PCM_SYNC_PTR, &s).0, Err(EINVAL));
}

#[test]
fn pcm_formats_and_faults() {
    let (sh, rig) = rig();
    let (f, m) = stream(Arc::new(HostCard(sh.clone())), 0);
    let t = T { f: &f, m: &m };
    t.hw_params(&testkit::hw(FORMAT_FLOAT_LE, 48000, 2, 480, 1920)).unwrap();
    t.call(PCM_PREPARE, 0).unwrap();
    let mut data = Vec::new();
    for v in [0.5f32, -0.5] {
        data.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    m.set(DATA, &data);
    assert_eq!(t.writei(1).1, 1);
    // A buffer outside the program's memory: EFAULT, nothing counted.
    let mut x = [0u8; XFERI_SIZE];
    put_u64(&mut x, 8, 0xdead_0000);
    put_u64(&mut x, 16, 4);
    assert_eq!(t.with(PCM_WRITEI_FRAMES, &x).0, Err(EFAULT));
    assert_eq!(t.status().appl, 1);
    assert_eq!(t.call(PCM_HW_REFINE, 0xdead_0000), Err(EFAULT));
    let _ = rig;
    // CHANNEL_INFO: no mmap, offset -1.
    let mut ci = [0u8; CHANNEL_INFO_SIZE];
    put_u32(&mut ci, 0, 1);
    let (r, o) = t.with(PCM_CHANNEL_INFO, &ci);
    assert_eq!((r, get_u64(&o, 8)), (Ok(0), u64::MAX));
    put_u32(&mut ci, 0, 2);
    assert_eq!(t.with(PCM_CHANNEL_INFO, &ci).0, Err(EINVAL));
    // write(2): whole frames only.
    use vfs::node::File;
    assert_eq!(f.write_stream(&[0; 7], false), Err(EINVAL));
    assert_eq!(f.write_stream(&[0; 16], false), Ok(16));
    assert_eq!(t.status().appl, 3);
}

/// Cobalt's ALSA sink (starboard/shared/alsa: FLOAT_LE, buffer near 8192, avail_min 512, start 1024, silence
/// threshold 256; it writes 512 frames whenever snd_pcm_delay < `min_delay` (2048), else sleeps 5333 us) on the
/// mixer, both woken late by up to `jitter_us` as threads under load are: the XRUNs in `total_ms` of playback.
fn cobalt_xruns(latency_ms: u32, min_delay: i64, total_ms: u64, jitter_us: u64) -> u32 {
    let sh = Arc::new(Shared::new());
    let mut core = Core::new(NoOut, sh.clone(), Config::latency_ms(latency_ms));
    let (f, m) = stream(Arc::new(HostCard(sh.clone())), 0);
    let t = T { f: &f, m: &m };
    t.hw_params(&testkit::hw(FORMAT_FLOAT_LE, 48000, 2, 0, 8192)).unwrap();
    let mut b = testkit::sw(1024, 8192, 512);
    put_u64(&mut b, sw::SILENCE_THRESHOLD, 256);
    assert_eq!(t.with(PCM_SW_PARAMS, &b).0, Ok(0));
    t.call(PCM_PREPARE, 0).unwrap();
    m.set(DATA, &[0; 512 * 8]);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut late = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        if jitter_us == 0 { 0 } else { seed % (jitter_us + 1) }
    };
    let (mut xruns, mut mix_at, mut look_at) = (0, 0u64, 0u64);
    while mix_at.min(look_at) < total_ms * 1000 {
        if mix_at <= look_at {
            sh.test_now.store(mix_at * 1000 + 1, Ordering::Release);
            let sleep = core.step(mix_at * 1000 + 1) / 1000;
            mix_at += sleep + late();
            continue;
        }
        sh.test_now.store(look_at * 1000 + 1, Ordering::Release);
        loop {
            let (r, d) = t.with(PCM_DELAY, &[0; 8]);
            if r == Err(EPIPE) {
                xruns += 1;
                t.call(PCM_PREPARE, 0).unwrap();
                continue;
            }
            if i64::from_le_bytes(d.try_into().unwrap()) >= min_delay {
                break;
            }
            assert_eq!(t.writei(512).1, 512);
        }
        look_at += 5333 + late();
    }
    xruns
}

#[test]
fn cobalt_sink_policy() {
    // Two minutes of playback each, as SN-7 asks. Before SN-7 the mixer took a whole lead from the ring at once
    // while Cobalt's delay counted the mixer's queue: 1,324 XRUNs in a QEMU session at 40 ms.
    let x = |lat, keep, jit| cobalt_xruns(lat, keep, 120_000, jit);
    let r = [x(40, 2048, 0), x(40, 2048, 3000), x(20, 2048, 3000), x(40, 4096, 3000), x(40, 1536, 3000), x(5, 2048, 0)];
    std::println!("cobalt policy: XRUNs in 120 s: 40 ms {} / jitter 3 ms {}, 20 ms {}, 40 ms 4096 kept {}, 40 ms 1536 kept {}, 5 ms {}", r[0], r[1], r[2], r[3], r[4], r[5]);
    assert_eq!(r, [0; 6]);
}

/// Sizes a program can make up (R1): every one is refused with Linux's errno, nothing panics or wraps, and
/// a refused sw_params keeps the old ones. A failed HW_PARAMS leaves the stream OPEN, as Linux
/// (pcm_native.c:843 `snd_pcm_set_state(substream, SNDRV_PCM_STATE_OPEN)`).
#[test]
fn hostile_sizes() {
    let (sh, _rig) = rig();
    let (f, m) = stream(Arc::new(HostCard(sh)), vfs::abi::O_NONBLOCK);
    let t = T { f: &f, m: &m };
    for (period, buffer) in [(0, u32::MAX), (u32::MAX, 0), (u32::MAX, u32::MAX), (480, 1 << 23)] {
        assert_eq!(t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, period, buffer)).err(), Some(EINVAL), "{period} {buffer}");
        assert_eq!(t.status().state, STATE_OPEN);
    }
    for k in [params::BUFFER_BYTES, params::PERIOD_BYTES, params::PERIODS] {
        let mut p = HwParams::decode(&testkit::hw(FORMAT_S32_LE, 48000, 2, 0, 0));
        *p.iv_mut(k) = Interval { integer: true, ..Interval::range(u32::MAX - 1, u32::MAX) };
        assert_eq!(t.hw_params(&p.encode()).err(), Some(EINVAL), "interval {k}");
        assert_eq!(constraints().refine(&mut p), Err(EINVAL), "refine {k}");
    }
    // An ordinary setup afterwards still works.
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 480, 1920)).unwrap();
    assert_eq!(t.status().state, STATE_SETUP);
    // sw_params: silence beyond the buffer is EINVAL and changes nothing; absurd thresholds are legal
    // (Linux checks only those fields) and must not make a transfer hang or wrap.
    let mut b = testkit::sw(1, 1920, 480);
    put_u64(&mut b, sw::SILENCE_THRESHOLD, u64::MAX);
    assert_eq!(t.with(PCM_SW_PARAMS, &b).0, Err(EINVAL));
    t.call(PCM_PREPARE, 0).unwrap();
    m.set(DATA, &[0; 480 * 4]);
    assert_eq!(t.writei(480).1, 480);
    assert_eq!(t.status().state, STATE_RUNNING, "start_threshold 1 still in force");
    t.call(PCM_DROP, 0).unwrap();
    assert_eq!(t.with(PCM_SW_PARAMS, &testkit::sw(u64::MAX, u64::MAX, u64::MAX)).0, Ok(0));
    t.call(PCM_PREPARE, 0).unwrap();
    assert_eq!(t.writei(480).1, 480);
    assert_eq!(t.status().state, STATE_PREPARED, "start_threshold u64::MAX: not started");
    // The ring is full after 1920 frames: a nonblocking write of u64::MAX frames returns what fit.
    m.set(DATA, &[0; 1920 * 4]);
    assert_eq!(t.writei(u64::MAX).1, 1440);
    assert_eq!(t.status().appl, 1920);
    // A buffer at the top of the address space with u64::MAX frames: EFAULT, nothing counted, no wrap.
    t.call(PCM_DROP, 0).unwrap();
    t.call(PCM_PREPARE, 0).unwrap();
    for (addr, frames) in [(u64::MAX - 3, u64::MAX), (u64::MAX - 3, i64::MAX as u64), (0, 1 << 62)] {
        let mut x = [0u8; XFERI_SIZE];
        put_u64(&mut x, 8, addr);
        put_u64(&mut x, 16, frames);
        assert_eq!(t.with(PCM_WRITEI_FRAMES, &x).0, Err(EFAULT), "{addr:#x} {frames}");
    }
    assert_eq!(t.status().appl, 0);
    // FORWARD and SYNC_PTR past the buffer or the boundary are EINVAL.
    assert_eq!(t.with(PCM_FORWARD, &u64::MAX.to_le_bytes()).1, 1920u64.to_le_bytes().to_vec(), "forward is capped at avail");
    let mut s = [0u8; SYNC_PTR_SIZE];
    put_u64(&mut s, sync::APPL_PTR, u64::MAX);
    assert_eq!(t.with(PCM_SYNC_PTR, &s).0, Err(EINVAL));
}
