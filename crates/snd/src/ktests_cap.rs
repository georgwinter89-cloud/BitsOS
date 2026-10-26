//! SN-9 in-kernel tests, all `Safe` (rerunnable; nothing is played). `sn.cap.fake`: the capture substream on
//! a clocked fake input (pattern frames at 48 kHz): blocking READI with the automatic start, frames in order,
//! XRUN after a reader that sleeps past the buffer, mono FLOAT, a blocked reader ended by DROP.
//! `sn.cap.node`: /dev/snd/pcmC0D0c (116:24, one open at a time), /proc/asound, the capture controls.
//! `sn.cap.system`: 1 s from the system's input (USB headset microphone, else an hda mic/line input; QEMU:
//! hda-duplex's line in).

use crate::alsa::abi::*;
use crate::alsa::testkit::{self, T, ARG, DATA};
use crate::alsa::umem::Arena;
use crate::alsa::CapFile;
use crate::capsrc::{pattern, Fake, FakeInputs};
use crate::capture;
use crate::ktests::{err, SLACK_NS};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::*;

fn ok(what: &str, r: Result<i64, vfs::Errno>) -> TestResult {
    kassert!(r == Ok(0), "{what}: {r:?}");
    Ok(())
}

fn now() -> u64 {
    time::now_ns()
}

fn s16(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

/// Frames `[l, r]` read are consecutive pattern frames (none lost, none twice).
fn in_order(v: &[i16]) -> Result<(), String> {
    let first = v[0] as u16 as u64;
    for (k, f) in v.chunks_exact(2).enumerate() {
        let want = pattern(first + k as u64);
        if f != want {
            return Err(format!("frame {k}: {f:?}, want {want:?}"));
        }
    }
    Ok(())
}

ktest!("sn.cap.fake", Safe, fake);
fn fake() -> TestResult {
    let src = Fake::new(Some(now));
    let f = Arc::new(CapFile::new(Arc::new(FakeInputs(src.clone())), 0, "ktest.cap".into()));
    let mem = Arena::new(256 << 10);
    let t = T { f: &f, m: &mem };
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 480, 4800)).map_err(|e| format!("hw_params: {e}"))?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    // 9600 frames (0.2 s) into a 0.1 s buffer: the read starts the stream, waits, and reads in pieces.
    let (r, n) = t.readi(9600);
    kassert!(r == Ok(0) && n == 9600, "blocking READI: {r:?} {n}");
    in_order(&s16(&mem.get(DATA, 9600 * 4)))?;
    kassert!(t.status().state == STATE_RUNNING && src.is_running());
    // A reader that sleeps longer than the buffer: XRUN (stop_threshold = buffer), EPIPE, prepare recovers.
    sched::sleep_ms(300);
    let (r, _) = t.readi(480);
    kassert!(r == Err(EPIPE), "after 0.3 s asleep: {r:?} {:?}", t.status());
    kassert!(t.status().state == STATE_XRUN);
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    ok("start", t.call(PCM_START, 0))?;
    let (r, n) = t.readi(480);
    kassert!(r == Ok(0) && n == 480, "after recovery: {r:?} {n}");
    in_order(&s16(&mem.get(DATA, 480 * 4)))?;
    // Mono FLOAT: (L + R) / 2 of the pattern is -1/2 -> -1 (arithmetic shift): -1/32768 every frame. A new
    // setup needs SETUP/PREPARED (EBADFD while running, pcm_native.c:712).
    kassert!(t.hw_params(&testkit::hw(FORMAT_FLOAT_LE, 48000, 1, 480, 4800)).err() == Some(EBADFD));
    ok("drop", t.call(PCM_DROP, 0))?;
    t.hw_params(&testkit::hw(FORMAT_FLOAT_LE, 48000, 1, 480, 4800)).map_err(|e| format!("hw_params float: {e}"))?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    let (r, n) = t.readi(960);
    kassert!(r == Ok(0) && n == 960, "mono float: {r:?} {n}");
    let want = crate::alsa::cxfer::s16_to_f32(-1).to_le_bytes();
    kassert!(mem.get(DATA, 960 * 4).chunks_exact(4).all(|c| c == want), "mono float values");
    // A reader blocked on 10 s is ended by DROP from here.
    let out = Arc::new(ksync::SpinLock::named("snd.ktest_out", None::<(Result<i64, vfs::Errno>, i64)>));
    let (f2, o) = (f.clone(), out.clone());
    let h = sched::spawn("ktest/cap-r", sched::Class::Normal, move || {
        let mem = Arena::new(2 << 20);
        let r = T { f: &f2, m: &mem }.readi(480_000);
        *o.lock() = Some(r);
    })
    .map_err(err("spawn"))?;
    kassert!(crate::ktests::wait(|| t.status().appl >= 4800, SLACK_NS), "reader did not read: {:?}", t.status());
    ok("drop", t.call(PCM_DROP, 0))?;
    kassert!(h.join_timeout(SLACK_NS), "READI still blocked after DROP");
    let r = out.lock().take();
    kassert!(matches!(r, Some((Ok(0), n)) if n > 0 && n < 480_000) || matches!(r, Some((Err(EBADFD), _))), "reader after DROP: {r:?}");
    kassert!(!src.is_running(), "DROP stopped the input");
    drop(t);
    drop(f);
    kassert!(!src.is_open(), "the input closed with the file");
    Ok(())
}

/// The source is an input of QEMU's hda codec (vendor 0x1af4).
fn qemu_codec(source: &str) -> bool {
    let ctrl = source.split('/').next().unwrap_or("");
    hda::controllers().iter().filter(|d| d.ctrl.name() == ctrl).flat_map(|d| d.ctrl.codecs()).any(|c| c.model.vendor_id >> 16 == 0x1af4)
}

fn read(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 4096).map_err(|e| format!("{path}: {e}"))?;
    Ok(String::from_utf8_lossy(&b).into())
}

ktest!("sn.cap.node", Safe, node);
fn node() -> TestResult {
    let ns = vfs::root_ns();
    let a = ns.lookup("/dev/snd/pcmC0D0c").map_err(|e| format!("pcmC0D0c: {e}"))?.attr();
    kassert!(a.mode & 0o170000 == uapi::S_IFCHR && a.rdev == vfs::abi::encode_dev(116, 24), "mode {:o} rdev {:#x}", a.mode, a.rdev);
    let mem = Arena::new(4096);
    let f = ns.open_flags(b"/dev/snd/pcmC0D0c", 2 | vfs::abi::O_NONBLOCK, 0).map_err(|e| format!("open: {e}"))?;
    // One substream: a second open is EBUSY (O_NONBLOCK; a blocking one would wait).
    let second = ns.open_flags(b"/dev/snd/pcmC0D0c", 2 | vfs::abi::O_NONBLOCK, 0);
    kassert!(matches!(second, Err(EBUSY)), "second open: {:?}", second.as_ref().err());
    mem.set(ARG, &[0; PCM_INFO_SIZE]);
    let r = f.file.ioctl_user(PCM_INFO, ARG, &mem);
    let v = mem.get(ARG, PCM_INFO_SIZE);
    kassert!(r == Some(Ok(0)) && get_u32(&v, pcm_info::STREAM) == 1, "INFO {r:?}");
    let ctl = ns.open_flags(b"/dev/snd/controlC0", 2, 0).map_err(|e| format!("open ctl: {e}"))?;
    let mut pi = [0u8; PCM_INFO_SIZE];
    put_u32(&mut pi, pcm_info::STREAM, 1);
    mem.set(ARG, &pi);
    let r = ctl.file.ioctl_user(CTL_PCM_INFO, ARG, &mem);
    let v = mem.get(ARG, PCM_INFO_SIZE);
    kassert!(r == Some(Ok(0)) && get_u32(&v, pcm_info::AVAIL) == 0, "CTL_PCM_INFO capture while open: {r:?} avail {}", get_u32(&v, pcm_info::AVAIL));
    drop(f);
    let again = ns.open_flags(b"/dev/snd/pcmC0D0c", 2 | vfs::abi::O_NONBLOCK, 0).map_err(|e| format!("open after close: {e}"))?;
    drop(again);
    let devices = read("/proc/asound/devices")?;
    kassert!(devices.ends_with(" 24: [ 0- 0]: digital audio capture\n"), "devices:\n{devices}");
    let pcm = read("/proc/asound/pcm")?;
    kassert!(pcm == "00-00: BitsOS PCM : BitsOS Mixer : playback 1 : capture 1\n", "pcm: {pcm}");
    // Capture Volume (numid 5): half on the left, read back, restored.
    let before = capture::gain();
    let mut w = [0u8; ELEM_VALUE_SIZE];
    put_u32(&mut w, elem::NUMID, 5);
    put_u64(&mut w, elem::VALUE, 16384);
    put_u64(&mut w, elem::VALUE + 8, before.1[1] as u64);
    mem.set(ARG, &w);
    let r = ctl.file.ioctl_user(CTL_ELEM_WRITE, ARG, &mem);
    let now = capture::gain();
    capture::set_gain(before.0, before.1);
    kassert!(r == Some(Ok(0)) && now.1[0] == 16384, "Capture Volume write: {r:?} {now:?}");
    let mut rd = [0u8; ELEM_VALUE_SIZE];
    put_u32(&mut rd, elem::NUMID, 4);
    mem.set(ARG, &rd);
    let r = ctl.file.ioctl_user(CTL_ELEM_READ, ARG, &mem);
    let v = mem.get(ARG, ELEM_VALUE_SIZE);
    kassert!(r == Some(Ok(0)) && &v[elem::NAME..elem::NAME + 14] == b"Capture Switch", "Capture Switch read {r:?}");
    Ok(())
}

ktest!("sn.cap.system", Safe, system);
fn system() -> TestResult {
    usb_audio::ktest_settle();
    let c = match capture::open(devmodel::audio::PcmFormat { rate: 48000, channels: 2, bits: 16 }, 9600) {
        Ok(c) => c,
        Err(crate::SndError::NoInput) => return ktest::skip("no microphone or line input (QEMU: intel-hda + hda-duplex)"),
        Err(e) => return Err(format!("open: {e:?}")),
    };
    let name = c.source_name();
    c.start().map_err(err("start"))?;
    let mut buf = alloc::vec![[0i16; 2]; 4800];
    let (mut got, mut peak, end) = (0u64, 0i32, now() + 1_000_000_000);
    while now() < end {
        c.wait(480, 50_000_000);
        let n = c.read(&mut buf).map_err(err("read"))?;
        got += n as u64;
        peak = peak.max(buf[..n].iter().map(|f| (f[0] as i32).abs().max((f[1] as i32).abs())).max().unwrap_or(0));
    }
    let lost = c.overruns();
    c.stop();
    let kicks = c.kicks();
    drop(c);
    klog::event!(Info, "ktest", "cap.system", source = name.as_str(), frames = got, peak = peak as u64, overruns = lost, kicks = kicks);
    if got == 0 && qemu_codec(&name) {
        return ktest::skip("QEMU's wav audiodev has no input side (record with -audiodev none, host/tests/sn/60-capture.sh)");
    }
    kassert!(got > 0, "no frames from {name} in 1 s");
    Ok(())
}
