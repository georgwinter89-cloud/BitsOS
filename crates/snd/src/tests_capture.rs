//! SN-9 host tests: `capture` over the fake input, the ALSA capture substream's state machine and conversions
//! (Linux semantics of pcm_native.c/pcm_lib.c for SNDRV_PCM_STREAM_CAPTURE), and the hda source on the
//! register-level fake (QEMU hda-duplex's line in).
extern crate std;

use crate::alsa::abi::*;
use crate::alsa::params::{self, HwParams};
use crate::alsa::testkit::{self, T, DATA};
use crate::alsa::umem::Arena;
use crate::alsa::CapFile;
use crate::capsrc::{self, pattern, Fake, FakeInputs};
use crate::capture::{self, Inputs};
use crate::client::{SndError, UNITY};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use devmodel::audio::PcmFormat;
use std::sync::Mutex;
use uapi::errno::*;

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
/// The capture gain is card-wide: tests that touch it or read through it run one at a time.
static GAIN: Mutex<()> = Mutex::new(());

fn frames(from: u64, n: u64) -> Vec<[i16; 2]> {
    (from..from + n).map(pattern).collect()
}

fn s16(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

#[test]
fn capture_core_reads_pushed_frames_from_the_start_on() {
    let _g = GAIN.lock().unwrap();
    let fake = Fake::new(None);
    let ins = FakeInputs(fake.clone());
    assert_eq!(capture::open_on(&ins, PcmFormat { rate: 44100, ..F48 }, 1024, 256, "t", None).err(), Some(SndError::BadFormat));
    let c = capture::open_on(&ins, F48, 1024, 256, "t", None).unwrap();
    assert_eq!(capture::open_on(&ins, F48, 1024, 256, "t2", None).err(), Some(SndError::Busy), "one open per input");
    fake.push(&frames(0, 100));
    assert_eq!(c.kicks(), 1, "a push kicks the reader");
    c.start().unwrap();
    assert_eq!(c.avail(), 0, "frames from before the start are dropped");
    fake.push(&frames(100, 50));
    let mut out = [[0i16; 2]; 64];
    assert_eq!(c.read(&mut out).unwrap(), 50);
    assert_eq!(&out[..50], &frames(100, 50)[..]);
    assert!(!c.wait(1, 1000), "nothing waiting: the slice runs out");
    // Card-wide gain: right channel switched off, left at half volume.
    capture::set_gain([true, false], [UNITY / 2, UNITY]);
    fake.push(&[[1000, 1000], [-3, 7]]);
    assert_eq!(c.read(&mut out).unwrap(), 2);
    assert_eq!(&out[..2], &[[500, 0], [-1, 0]]);
    capture::set_gain([true; 2], [UNITY; 2]);
    // A ring of twice 1024 frames: 2100 pushed lose 52.
    fake.push(&frames(0, 2100));
    assert_eq!((c.avail(), c.overruns()), (2048, 52));
    drop(c);
    assert!(!fake.is_open());
    let c = capture::open_on(&ins, F48, 1024, 256, "t", None).unwrap();
    fake.unplug();
    assert!(c.gone());
    assert_eq!(c.read(&mut out), Err(SndError::Closed));
}

#[test]
fn float_and_s32_are_exact() {
    for v in [0i16, 1, -1, 2, 3, 255, -256, 12345, 16384, -16384, 32767, -32768] {
        assert_eq!(crate::alsa::cxfer::s16_to_f32(v), (v as f32 / 32768.0).to_bits(), "{v}");
        let mut b = [0u8; 4];
        crate::alsa::cxfer::from_s16(FORMAT_S32_LE, v, &mut b);
        assert_eq!(i32::from_le_bytes(b), (v as i32) << 16);
    }
}

fn cap_file(fake: &Arc<Fake>) -> (CapFile, Arena) {
    let ins: Arc<dyn Inputs> = Arc::new(FakeInputs(fake.clone()));
    (CapFile::new(ins, vfs::abi::O_NONBLOCK, "host".into()), Arena::new(1 << 20))
}

#[test]
fn refine_offers_48k_only() {
    let fake = Fake::new(None);
    let (f, m) = cap_file(&fake);
    let t = T { f: &f, m: &m };
    let (r, out) = t.with(PCM_HW_REFINE, &HwParams::any().encode());
    r.unwrap();
    let mut raw = [0u8; HW_PARAMS_SIZE];
    raw.copy_from_slice(&out);
    let p = HwParams::decode(&raw);
    assert_eq!((p.iv(params::RATE).min, p.iv(params::RATE).max), (48000, 48000));
    assert_eq!((p.iv(params::CHANNELS).min, p.iv(params::CHANNELS).max), (1, 2));
    assert_eq!(t.hw_params(&testkit::hw(FORMAT_S16_LE, 44100, 2, 0, 0)).err(), Some(EINVAL));
    assert_eq!(fake.opens.load(Ordering::Acquire), 0, "a refused setup opens nothing");
}

#[test]
fn capture_lifecycle_as_linux() {
    let _g = GAIN.lock().unwrap();
    let fake = Fake::new(None);
    let (f, m) = cap_file(&fake);
    let t = T { f: &f, m: &m };
    assert_eq!(t.readi(4).0, Err(EBADFD));
    assert_eq!(t.call(PCM_PREPARE, 0), Err(EBADFD));
    let (r, info) = t.with(PCM_INFO, &[0u8; PCM_INFO_SIZE]);
    r.unwrap();
    assert_eq!(get_u32(&info, pcm_info::STREAM), 1);
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 480, 1920)).unwrap();
    assert_eq!(fake.opens.load(Ordering::Acquire), 1);
    assert_eq!(t.status().state, STATE_SETUP);
    assert_eq!(t.readi(4).0, Err(EBADFD), "SETUP is not accessible");
    t.call(PCM_PREPARE, 0).unwrap();
    assert_eq!(t.status().state, STATE_PREPARED);
    fake.push(&frames(0, 10));
    let s = t.status();
    assert_eq!((s.avail, s.hw), (0, 0), "PREPARED: hw_ptr does not move before the start");
    assert_eq!(f_events_in(&f), false);
    // A read of >= start_threshold (1) frames starts the stream; what came before the start is gone.
    assert_eq!(t.readi(4).0, Err(EAGAIN));
    assert_eq!(t.status().state, STATE_RUNNING);
    fake.push(&frames(10, 100));
    let s = t.status();
    assert_eq!((s.appl, s.hw, s.avail, s.delay), (0, 100, 100, 100));
    let (r, n) = t.readi(64);
    assert_eq!((r, n), (Ok(0), 64));
    let got = s16(&m.get(DATA, 64 * 4));
    let want: Vec<i16> = frames(10, 64).iter().flatten().copied().collect();
    assert_eq!(got, want);
    let s = t.status();
    assert_eq!((s.appl, s.hw, s.avail), (64, 100, 36));
    // FORWARD drops frames unread; REWIND does nothing (NO_REWINDS).
    m.set(testkit::ARG, &10u64.to_le_bytes());
    t.call(PCM_FORWARD, testkit::ARG).unwrap();
    assert_eq!(u64::from_le_bytes(m.get(testkit::ARG, 8).try_into().unwrap()), 10);
    assert_eq!(t.status().appl, 74);
    // Pause keeps what waits, release goes on with it (hw_ptr never goes back, nothing is lost).
    t.call(PCM_PAUSE, 1).unwrap();
    assert_eq!(t.status().state, STATE_PAUSED);
    t.call(PCM_PAUSE, 0).unwrap();
    let s = t.status();
    assert_eq!((s.state, s.appl, s.hw, s.avail), (STATE_RUNNING, 74, 100, 26));
    assert_eq!(t.readi(26), (Ok(0), 26));
    let want: Vec<i16> = frames(84, 26).iter().flatten().copied().collect();
    assert_eq!(s16(&m.get(DATA, 26 * 4)), want, "the frames from before the pause");
    // The waiting frames reach stop_threshold (= buffer): XRUN, reads EPIPE until prepare.
    fake.push(&frames(0, 1920));
    assert_eq!(t.readi(4).0, Err(EPIPE));
    assert_eq!(t.status().state, STATE_XRUN);
    // Drain leaves a capture XRUN alone (pcm_native.c:2052); O_NONBLOCK: EAGAIN after the init (:2115).
    assert_eq!(t.call(PCM_DRAIN, 0), Err(EAGAIN));
    assert_eq!(t.status().state, STATE_XRUN);
    t.call(PCM_PREPARE, 0).unwrap();
    t.call(PCM_START, 0).unwrap();
    assert_eq!(t.status().avail, 0, "prepare dropped what waited");
    // Drain: DRAINING while frames wait; a read is EBADFD there (pcm_accessible_state); drop -> SETUP.
    fake.push(&frames(0, 8));
    assert_eq!(t.call(PCM_DRAIN, 0), Err(EAGAIN));
    assert_eq!(t.status().state, STATE_DRAINING);
    assert_eq!(t.readi(4).0, Err(EBADFD));
    t.call(PCM_DROP, 0).unwrap();
    assert_eq!(t.status().state, STATE_SETUP);
    t.call(PCM_HW_FREE, 0).unwrap();
    assert!(!fake.is_open(), "hw_free closes the input");
}

#[test]
fn mono_float_and_unplug() {
    let _g = GAIN.lock().unwrap();
    let fake = Fake::new(None);
    let (f, m) = cap_file(&fake);
    let t = T { f: &f, m: &m };
    t.hw_params(&testkit::hw(FORMAT_FLOAT_LE, 48000, 1, 240, 960)).unwrap();
    t.call(PCM_PREPARE, 0).unwrap();
    t.call(PCM_START, 0).unwrap();
    fake.push(&[[100, 300], [-32768, -32768], [16384, 0]]);
    assert_eq!(t.readi(3), (Ok(0), 3));
    let got: Vec<f32> = m.get(DATA, 12).chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    assert_eq!(got, [200.0 / 32768.0, -1.0, 0.25]);
    // Poll: readable once avail_min (a period) waits.
    use vfs::node::File;
    assert_eq!(f.events().0 & vfs::poll::Events::IN, 0);
    fake.push(&frames(0, 240));
    assert_ne!(f.events().0 & vfs::poll::Events::IN, 0);
    fake.unplug();
    assert_eq!(t.with(PCM_STATUS, &[0u8; STATUS_SIZE]).0, Err(ENODEV), "disconnected: every ioctl ENODEV");
    assert_eq!(t.readi(240).0, Err(ENODEV));
    // read(2) on a disconnected stream is EBADFD (pcm_native.c:3474); the ioctl ENODEV.
    assert_eq!(f.read_at(0, &mut [0u8; 4]), Err(EBADFD));
    assert_eq!(t.call(PCM_PREPARE, 0), Err(ENODEV), "a gone microphone is a disconnected stream");
}

/// The hda source on the ALC269's mic jack (register-level fake): the period callback refreshes what waits,
/// frames come out in order.
#[test]
fn hda_source_reads_the_mic_jack() {
    let _g = GAIN.lock().unwrap();
    let t = hda_alc269();
    let (fake, c) = (t.0, Arc::new(t.1));
    let kicks = Arc::new(AtomicU32::new(0));
    let k = kicks.clone();
    let src = capsrc::open_hda(&c, 1024, 256, Arc::new(move || {
        k.fetch_add(1, Ordering::AcqRel);
    }))
    .unwrap();
    assert!(src.name().ends_with("0:0x18"), "{}", src.name());
    let bytes: Vec<u8> = frames(0, 2000).iter().flat_map(|f| [f[0].to_le_bytes(), f[1].to_le_bytes()]).flatten().collect();
    fake.set_source(0, bytes);
    src.start().unwrap();
    fake.advance(256 * 4);
    c.service();
    assert!(kicks.load(Ordering::Acquire) >= 1, "the period callback kicked");
    assert_eq!(src.available(), 256);
    let mut out = [[0i16; 2]; 300];
    assert_eq!(src.read(&mut out).unwrap(), 256);
    assert_eq!(&out[..256], &frames(0, 256)[..]);
    assert_eq!(src.available(), 0);
    src.close();
}

fn hda_alc269() -> (Arc<hda::fake::FakeHda>, hda::Controller<hda::fake::FakeHw>) {
    let f = hda::fake::FakeHda::new(0x4401);
    f.add_codec(0, include_str!("../../hdacodec/fixtures/alc269-synthetic.txt"));
    let c = hda::Controller::new("hda0".into(), hda::Kind::Intel, hda::fake::FakeHw(f.clone()), hda::Config::default());
    c.bring_up().unwrap();
    (f, c)
}

fn f_events_in(f: &CapFile) -> bool {
    use vfs::node::File;
    f.events().0 & vfs::poll::Events::IN != 0
}

/// Hostile READI: buffers outside the program or at the top of the address space give EFAULT, take nothing.
#[test]
fn readi_bad_pointers_are_efault() {
    let _g = GAIN.lock().unwrap();
    let fake = Fake::new(None);
    let (f, m) = cap_file(&fake);
    let t = T { f: &f, m: &m };
    assert_eq!(t.call(PCM_HW_REFINE, 0xdead_0000), Err(EFAULT));
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 480, 1920)).unwrap();
    assert_eq!(t.call(PCM_HW_PARAMS, 0xdead_0000), Err(EFAULT));
    assert_eq!(t.call(PCM_SW_PARAMS, 0xdead_0000), Err(EFAULT));
    t.call(PCM_PREPARE, 0).unwrap();
    t.call(PCM_START, 0).unwrap();
    fake.push(&frames(0, 100));
    assert_eq!(t.call(PCM_READI_FRAMES, 0xdead_0000), Err(EFAULT), "the xferi struct itself outside");
    // The last frame would cross the end of the program's memory: EFAULT before the source gives frames up.
    let end = testkit::ARG + (1 << 20) - 6;
    for (addr, n) in [(0xdead_0000, 4), (end, 2), (u64::MAX - 3, u64::MAX), (u64::MAX - 3, i64::MAX as u64)] {
        let mut x = [0u8; XFERI_SIZE];
        put_u64(&mut x, 8, addr);
        put_u64(&mut x, 16, n);
        assert_eq!(t.with(PCM_READI_FRAMES, &x).0, Err(EFAULT), "{addr:#x} {n}");
    }
    // No buffer: EINVAL (pcm_lib.c:2237).
    let mut x = [0u8; XFERI_SIZE];
    put_u64(&mut x, 16, 1 << 62);
    assert_eq!(t.with(PCM_READI_FRAMES, &x).0, Err(EINVAL));
    let s = t.status();
    assert_eq!((s.state, s.appl, s.avail), (STATE_RUNNING, 0, 100), "nothing consumed");
    assert_eq!(t.readi(100), (Ok(0), 100));
    let want: Vec<i16> = frames(0, 100).iter().flatten().copied().collect();
    assert_eq!(s16(&m.get(DATA, 400)), want, "the frames are still in order");
}
