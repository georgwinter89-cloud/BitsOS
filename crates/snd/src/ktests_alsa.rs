//! SN-4 in-kernel tests, all `Safe` (no sound; rerunnable): `sn.alsa.null` drives the PCM ioctls with blocking
//! waits on a private mixer's null output; `sn.alsa.nodes` opens /dev/snd/* and reads /proc/asound by name;
//! `sn.alsa.ctl` reads the card and its elements and round-trips the master switch with an event;
//! `sn.alsa.lifecycle` ends blocked WRITEI/DRAIN from other threads and churns opens.

use crate::alsa::abi::*;
use crate::alsa::testkit::{self, T, ARG, DATA};
use crate::alsa::umem::Arena;
use crate::alsa::PcmFile;
use crate::ktests::{err, wait, NoOutputs, SLACK_NS};
use crate::thread::Mixer;
use crate::Config;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::*;
use vfs::node::File;

fn ok(what: &str, r: Result<i64, vfs::Errno>) -> TestResult {
    kassert!(r == Ok(0), "{what}: {r:?}");
    Ok(())
}

ktest!("sn.alsa.null", Safe, null);
/// A whole libasound-style session on the null output (by the clock): hw_params S16 48 kHz, sw_params with
/// start_threshold = buffer, a blocking WRITEI of 1 s that has to wait for space, poll's mask, delay, DRAIN
/// that waits until played, then FLOAT_LE at 44.1 kHz, XRUN and its recovery, O_NONBLOCK's EAGAIN.
fn null() -> TestResult {
    let m = Mixer::spawn("snd/mix-alsa", Config::latency_ms(40), |_| NoOutputs).map_err(err("spawn"))?;
    let r = session(m.clone());
    m.stop();
    r
}

fn session(m: Arc<Mixer>) -> TestResult {
    let mem = Arena::new(1 << 20);
    let f = PcmFile::new(m.clone(), 0, "ktest.alsa".into());
    let t = T { f: &f, m: &mem };
    let p = t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 1200, 4800)).map_err(|e| format!("hw_params: {e}"))?;
    kassert!(p.get(crate::alsa::params::BUFFER_SIZE) == 4800);
    ok("sw_params", t.with(PCM_SW_PARAMS, &testkit::sw(4800, 4800, 1200)).0)?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    kassert!(f.events().0 & vfs::poll::Events::OUT != 0, "prepared, empty: writable");
    // 1 s = 48000 frames into a 4800-frame ring: WRITEI blocks until the mixer took most of it.
    mem.set(DATA, &[0x10; 48000 * 4]);
    let t0 = time::now_ns();
    let (r, n) = t.writei(48000);
    kassert!(r == Ok(0) && n == 48000, "writei: {r:?} {n}");
    kassert!(time::now_ns() - t0 > 100_000_000, "a 1 s write into 0.1 s of ring returned at once");
    let st = t.status();
    kassert!(st.state == STATE_RUNNING && st.appl == 48000 && st.hw <= 48000, "{st:?}");
    kassert!(f.events().0 & vfs::poll::Events::OUT == 0 || st.avail >= 1200, "poll says writable with avail {}", st.avail);
    let mut d = [0u8; 8];
    d.copy_from_slice(&t.with(PCM_DELAY, &[0; 8]).1);
    kassert!(i64::from_le_bytes(d) > 0, "delay while running");
    ok("drain", t.call(PCM_DRAIN, 0))?;
    let st = t.status();
    kassert!(st.state == STATE_SETUP && st.hw == 48000 && st.avail == 4800, "after drain (pointers stay until prepare) {st:?}");
    // FLOAT_LE 44.1 kHz mono: the resampled path; then an XRUN (stop_threshold = buffer by default).
    ok("hw_free", t.call(PCM_HW_FREE, 0))?;
    t.hw_params(&testkit::hw(FORMAT_FLOAT_LE, 44100, 1, 441, 4410)).map_err(|e| format!("hw_params f32: {e}"))?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    let half = 0.25f32.to_bits().to_le_bytes();
    let data: alloc::vec::Vec<u8> = (0..4410).flat_map(|_| half).collect();
    mem.set(DATA, &data);
    kassert!(t.writei(4410).1 == 4410);
    kassert!(t.status().state == STATE_RUNNING, "start_threshold 1 (default) started it");
    kassert!(wait(|| t.status().state == STATE_XRUN, SLACK_NS), "no XRUN: {:?}", t.status());
    kassert!(f.events().0 & vfs::poll::Events::ERR != 0, "XRUN: poll reports ERR");
    kassert!(t.writei(10).0 == Err(EPIPE));
    ok("prepare after xrun", t.call(PCM_PREPARE, 0))?;
    kassert!(t.writei(441).1 == 441);
    ok("drop", t.call(PCM_DROP, 0))?;
    drop(f);
    // O_NONBLOCK from open (kernel callers have no fd table): a full ring answers EAGAIN, DRAIN too.
    let f = PcmFile::new(m, vfs::abi::O_NONBLOCK, "ktest.alsa.nb".into());
    let t = T { f: &f, m: &mem };
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 480, 1920)).map_err(|e| format!("hw_params nb: {e}"))?;
    ok("sw_params", t.with(PCM_SW_PARAMS, &testkit::sw(1 << 30, 1920, 480)).0)?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    kassert!(t.writei(4000) == (Ok(0), 1920), "{:?}", t.writei(0));
    kassert!(t.writei(1) == (Err(EAGAIN), -EAGAIN));
    kassert!(t.status().state == STATE_PREPARED, "start_threshold above the buffer: not started");
    ok("start", t.call(PCM_START, 0))?;
    kassert!(t.call(PCM_DRAIN, 0) == Err(EAGAIN));
    kassert!(wait(|| t.status().state == STATE_SETUP, SLACK_NS), "nonblocking drain did not end: {:?}", t.status());
    klog::event!(Info, "ktest", "alsa.null", ok = true);
    Ok(())
}

fn read(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 4096).map_err(|e| format!("{path}: {e}"))?;
    Ok(String::from_utf8_lossy(&b).into())
}

ktest!("sn.alsa.nodes", Safe, nodes);
/// /dev/snd/pcmC0D0p and controlC0 by name (char devices 116:16 and 116:0, one object per open), the PCM
/// version and info through the node, /proc/asound/cards and pcm as Linux prints them. No client is opened.
fn nodes() -> TestResult {
    let ns = vfs::root_ns();
    for (path, minor) in ["/dev/snd/pcmC0D0p", "/dev/snd/controlC0"].iter().zip([16u32, 0]) {
        let a = ns.lookup(path).map_err(|e| format!("{path}: {e}"))?.attr();
        kassert!(a.mode & 0o170000 == uapi::S_IFCHR && a.rdev == vfs::abi::encode_dev(116, minor), "{path}: mode {:o} rdev {:#x}", a.mode, a.rdev);
    }
    let mem = Arena::new(4096);
    let f = ns.open_flags(b"/dev/snd/pcmC0D0p", 2, 0).map_err(|e| format!("open pcm: {e}"))?;
    let call = |cmd, len| {
        mem.set(ARG, &[0; 1024][..len]);
        let r = f.file.ioctl_user(cmd, ARG, &mem);
        (r, mem.get(ARG, len))
    };
    let (r, v) = call(PCM_PVERSION, 4);
    kassert!(r == Some(Ok(0)) && get_u32(&v, 0) == SNDRV_PCM_VERSION, "PVERSION {r:?}");
    let (r, v) = call(PCM_INFO, PCM_INFO_SIZE);
    kassert!(r == Some(Ok(0)) && &v[pcm_info::NAME..pcm_info::NAME + 12] == b"BitsOS Mixer", "INFO {r:?}");
    let (r, v) = call(PCM_STATUS, STATUS_SIZE);
    kassert!(r == Some(Ok(0)) && get_u32(&v, 0) == STATE_OPEN, "STATUS {r:?}");
    kassert!(call(PCM_START, 0).0 == Some(Err(EBADFD)));
    kassert!(call(0x5400, 0).0 == Some(Err(ENOTTY)), "a terminal ioctl on a PCM");
    drop(f);
    let cards = read("/proc/asound/cards")?;
    kassert!(cards == " 0 [BitsOS         ]: BitsOS - BitsOS\n                      BitsOS Mixer (48 kHz, 16 bit)\n", "cards:\n{cards}");
    let pcm = read("/proc/asound/pcm")?;
    kassert!(pcm == "00-00: BitsOS PCM : BitsOS Mixer : playback 1 : capture 1\n", "pcm: {pcm}");
    Ok(())
}

ktest!("sn.alsa.ctl", Safe, ctl);
/// The control device as libasound opens a card: PVERSION, CARD_INFO, ELEM_LIST/INFO/READ, PCM_NEXT_DEVICE
/// and PCM_INFO; ELEM_WRITE of the master switch (off, then back on) queues one event for a subscriber.
fn ctl() -> TestResult {
    let mem = Arena::new(8192);
    let f = vfs::root_ns().open_flags(b"/dev/snd/controlC0", 2 | vfs::abi::O_NONBLOCK, 0).map_err(|e| format!("open ctl: {e}"))?;
    let call = |cmd, data: &[u8]| {
        mem.set(ARG, data);
        let r = f.file.ioctl_user(cmd, ARG, &mem).unwrap_or(Err(ENOTTY));
        (r, mem.get(ARG, data.len()))
    };
    kassert!(call(CTL_PVERSION, &[0; 4]).1 == SNDRV_CTL_VERSION.to_le_bytes());
    let (r, c) = call(CTL_CARD_INFO, &[0; CARD_INFO_SIZE]);
    kassert!(r == Ok(0) && &c[card::DRIVER..card::DRIVER + 7] == b"BitsOS\0", "CARD_INFO {r:?}");
    let mut l = [0u8; ELEM_LIST_SIZE];
    put_u32(&mut l, elem::LIST_SPACE, 8);
    put_u64(&mut l, elem::LIST_PIDS, DATA);
    let (r, l) = call(CTL_ELEM_LIST, &l);
    kassert!(r == Ok(0) && get_u32(&l, elem::LIST_COUNT) == 5 && get_u32(&l, elem::LIST_USED) == 5, "ELEM_LIST {r:?}");
    let id2 = mem.get(DATA + ELEM_ID_SIZE as u64, ELEM_ID_SIZE);
    kassert!(&id2[elem::NAME..elem::NAME + 22] == b"Master Playback Volume");
    let mut info = [0u8; ELEM_INFO_SIZE];
    info[..ELEM_ID_SIZE].copy_from_slice(&id2);
    let (r, i) = call(CTL_ELEM_INFO, &info);
    kassert!(r == Ok(0) && get_u32(&i, elem::INFO_TYPE) == 2 && get_u32(&i, elem::INFO_COUNT) == 2 && get_u64(&i, elem::INFO_MAX) == 32768, "ELEM_INFO {r:?}");
    // By name (numid 0): the switch; subscribe, turn it off and on, two events (merged while queued: one).
    let mut v = [0u8; ELEM_VALUE_SIZE];
    put_u32(&mut v, elem::IFACE, 2);
    put_str(&mut v, elem::NAME, elem::NAME_LEN, "Master Playback Switch");
    let (r, cur) = call(CTL_ELEM_READ, &v);
    kassert!(r == Ok(0) && get_u32(&cur, elem::NUMID) == 1, "ELEM_READ {r:?}");
    let was = [get_u64(&cur, elem::VALUE), get_u64(&cur, elem::VALUE + 8)];
    kassert!(call(CTL_SUBSCRIBE_EVENTS, &1i32.to_le_bytes()).0 == Ok(0));
    let mut w = cur.clone();
    put_u64(&mut w, elem::VALUE, 0);
    kassert!(call(CTL_ELEM_WRITE, &w).0 == Ok(0));
    put_u64(&mut w, elem::VALUE, was[0]);
    kassert!(call(CTL_ELEM_WRITE, &w).0 == Ok(0));
    put_u64(&mut w, elem::VALUE, 7);
    kassert!(call(CTL_ELEM_WRITE, &w).0 == Err(EINVAL), "out of range");
    let mut ev = [0u8; 2 * CTL_EVENT_SIZE];
    let n = f.file.read_stream(&mut ev, true);
    kassert!(n == Ok(CTL_EVENT_SIZE) && get_u32(&ev, 4) == 1 && get_u32(&ev, 8) == 1, "events {n:?}");
    kassert!(f.file.read_stream(&mut ev, true) == Err(EAGAIN));
    let (r, nd) = call(CTL_PCM_NEXT_DEVICE, &(-1i32).to_le_bytes());
    kassert!(r == Ok(0) && nd == 0i32.to_le_bytes());
    kassert!(call(CTL_PCM_NEXT_DEVICE, &0i32.to_le_bytes()).1 == (-1i32).to_le_bytes());
    let mut pi = [0u8; PCM_INFO_SIZE];
    put_u32(&mut pi, pcm_info::STREAM, 1);
    // SN-9: the capture substream exists (pcmC0D0c).
    let (r, ci) = call(CTL_PCM_INFO, &pi);
    kassert!(r == Ok(0) && get_u32(&ci, pcm_info::STREAM) == 1, "capture stream {r:?}");
    put_u32(&mut pi, pcm_info::STREAM, 0);
    kassert!(call(CTL_PCM_INFO, &pi).0 == Ok(0));
    Ok(())
}

ktest!("sn.alsa.lifecycle", Safe, lifecycle);
/// Blocked callers are ended by another thread, never left hanging: a WRITEI blocked on a full ring by DROP,
/// a blocking DRAIN by DROP, a WRITEI by the mixer stopping (what Quiesce does before Reload); 100 opens
/// and closes leave no client behind.
fn lifecycle() -> TestResult {
    let m = Mixer::spawn("snd/mix-life", Config::latency_ms(40), |_| NoOutputs).map_err(err("spawn"))?;
    let r = lifecycle_on(&m);
    m.stop();
    r
}

type Outcome = Arc<ksync::SpinLock<Option<(Result<i64, vfs::Errno>, i64)>>>;

/// Runs `call` on its own thread (class Normal, like a program's); the outcome is filled in at its end. The
/// thread gets its own memory, as a program's thread has its own stack: `T::with` puts each ioctl struct at
/// ARG, and a shared arena let the test thread's STATUS overwrite the writer's xferi between its `set` and the
/// ioctl, so WRITEI saw 0 frames and the ring was never filled (the SN-7 flake, appl 0 after 20 s).
fn behind(name: &str, f: &Arc<PcmFile>, call: fn(&T<'_>) -> (Result<i64, vfs::Errno>, i64)) -> Result<(sched::JoinHandle, Outcome), String> {
    let out: Outcome = Arc::new(ksync::SpinLock::named("snd.ktest_out", None));
    let mem = Arena::new(2 << 20);
    mem.set(DATA, &alloc::vec![0x10; LONG as usize * 4]);
    let (f, o) = (f.clone(), out.clone());
    let h = sched::spawn(name, sched::Class::Normal, move || {
        let r = call(&T { f: &f, m: &mem });
        *o.lock() = Some(r);
    })
    .map_err(err("spawn"))?;
    Ok((h, out))
}

const LONG: u64 = 480_000;

fn lifecycle_on(m: &Arc<Mixer>) -> TestResult {
    let mem = Arena::new(64 << 10);
    mem.set(DATA, &alloc::vec![0x10; 4800 * 4]);
    let f = Arc::new(PcmFile::new(m.clone(), 0, "ktest.life".into()));
    let t = T { f: &f, m: &mem };
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 1200, 4800)).map_err(|e| format!("hw_params: {e}"))?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    // 1. 10 s into a 0.1 s ring: the writer blocks; DROP from here ends it with what it wrote.
    let (h, out) = behind("ktest/alsa-w", &f, |t| t.writei(LONG))?;
    kassert!(wait(|| t.status().state == STATE_RUNNING && t.status().appl >= 4800, SLACK_NS), "writer did not fill the ring: {:?}, writer {:?}", t.status(), *out.lock());
    ok("drop", t.call(PCM_DROP, 0))?;
    kassert!(h.join_timeout(SLACK_NS), "WRITEI still blocked after DROP");
    let r = out.lock().take();
    kassert!(matches!(r, Some((Ok(0), n)) if n >= 4800 && (n as u64) < LONG) || matches!(r, Some((Err(EBADFD), _))), "writer after DROP: {r:?}");
    // 2. A blocking DRAIN ended by DROP: DRAIN returns 0 once the state left DRAINING (pcm_native.c:2090).
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    kassert!(t.writei(4800).1 == 4800);
    let (h, out) = behind("ktest/alsa-d", &f, |t| (t.call(PCM_DRAIN, 0), 0))?;
    kassert!(wait(|| t.status().state != STATE_RUNNING, SLACK_NS), "drain did not start: {:?}", t.status());
    let _ = t.call(PCM_DROP, 0);
    kassert!(h.join_timeout(SLACK_NS), "DRAIN still blocked after DROP");
    let r = out.lock().take();
    kassert!(matches!(r, Some((Ok(0), _))), "drain after DROP: {r:?}");
    kassert!(t.status().state == STATE_SETUP, "{:?}", t.status());
    drop(f);
    // 3. Opens and closes: each close gives its client back.
    for i in 0..100u32 {
        let g = PcmFile::new(m.clone(), 0, "ktest.life.churn".into());
        let t = T { f: &g, m: &mem };
        t.hw_params(&testkit::hw(FORMAT_S16_LE, if i % 2 == 0 { 48000 } else { 44100 }, 2, 480, 1920)).map_err(|e| format!("hw_params {i}: {e}"))?;
        if i % 3 != 0 {
            ok("prepare", t.call(PCM_PREPARE, 0))?;
            kassert!(t.writei(960).1 == 960);
        }
    }
    kassert!(wait(|| m.status().clients == 0, SLACK_NS), "clients left: {}", m.status().clients);
    // 4. The mixer stops under a blocked writer (Quiesce): the writer returns, short or with an error.
    let f = Arc::new(PcmFile::new(m.clone(), 0, "ktest.life.stop".into()));
    let t = T { f: &f, m: &mem };
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 1200, 4800)).map_err(|e| format!("hw_params: {e}"))?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    let (h, out) = behind("ktest/alsa-q", &f, |t| t.writei(LONG))?;
    kassert!(wait(|| t.status().appl >= 4800, SLACK_NS), "writer did not fill the ring: {:?}, writer {:?}", t.status(), *out.lock());
    m.stop();
    kassert!(h.join_timeout(SLACK_NS), "WRITEI still blocked after the mixer stopped");
    let r = out.lock().take();
    kassert!(matches!(r, Some((Ok(0), n)) if (n as u64) < LONG) || matches!(r, Some((Err(EBADFD | EIO), _))), "writer after stop: {r:?}");
    klog::event!(Info, "ktest", "alsa.lifecycle", writer = format!("{r:?}").as_str());
    Ok(())
}

ktest!("sn.alsa.threads", Safe, threads);
/// Guards the SN-7 flake's cause: a program's threads never share the memory their ioctl structs live in.
/// A writer started by `behind` writes 100 x 480 frames while this thread issues STATUS (whose struct sits
/// at the same ARG) as fast as it can; with one arena for both, STATUS zeroes the writer's xferi and a
/// WRITEI comes back short. Every WRITEI here must move exactly its 480 frames.
fn threads() -> TestResult {
    let m = Mixer::spawn("snd/mix-thr", Config::latency_ms(40), |_| NoOutputs).map_err(err("spawn"))?;
    let r = threads_on(&m);
    m.stop();
    r
}

const ROUNDS: i64 = 100;
const CHUNK: u64 = 480;

fn threads_on(m: &Arc<Mixer>) -> TestResult {
    let mem = Arena::new(64 << 10);
    let f = Arc::new(PcmFile::new(m.clone(), 0, "ktest.threads".into()));
    let t = T { f: &f, m: &mem };
    t.hw_params(&testkit::hw(FORMAT_S16_LE, 48000, 2, 1200, 4800)).map_err(|e| format!("hw_params: {e}"))?;
    ok("prepare", t.call(PCM_PREPARE, 0))?;
    // INT-W7 (YT-5 CR to SN): under full-suite load the writer thread can be starved past the ring's end and
    // see a real XRUN (EPIPE). That is not the flake this test guards (a short Ok write); the writer recovers as
    // snd_pcm_recover does (PREPARE, write again) and reports the XRUN count in place of 0. Bounded: at most
    // ROUNDS recoveries.
    let (h, out) = behind("ktest/alsa-thr", &f, |t| {
        let (mut total, mut done, mut xruns, mut left) = (0, 0, 0i64, CHUNK);
        while done < ROUNDS {
            match t.writei(left) {
                (Ok(0), n) if n == left as i64 => {
                    total += n;
                    done += 1;
                    left = CHUNK;
                }
                // Linux semantics: frames moved before the XRUN are reported as a short write. Only with the
                // stream in XRUN is a short write excused (the shared-arena flake comes back short while running).
                (Ok(0), n) if n > 0 && n < left as i64 && t.status().state == STATE_XRUN && xruns < ROUNDS => {
                    total += n;
                    left -= n as u64;
                    xruns += 1;
                    if t.call(PCM_PREPARE, 0).is_err() {
                        return (Err(EPIPE), total);
                    }
                }
                (Err(EPIPE), 0) if xruns < ROUNDS => {
                    xruns += 1;
                    if t.call(PCM_PREPARE, 0).is_err() {
                        return (Err(EPIPE), total);
                    }
                }
                (r, n) => return (r, total + n),
            }
        }
        (Ok(xruns), total)
    })?;
    let end = time::now_ns() + SLACK_NS;
    let mut polls = 0u64;
    while out.lock().is_none() && time::now_ns() < end {
        let _ = t.status();
        polls += 1;
        sched::yield_now();
    }
    let joined = h.join_timeout(SLACK_NS);
    if !joined {
        let _ = t.call(PCM_DROP, 0);
        let _ = h.join_timeout(SLACK_NS);
    }
    let r = out.lock().take();
    let _ = t.call(PCM_DROP, 0);
    kassert!(joined, "writer still running after {} s: {:?}", SLACK_NS / 1_000_000_000, t.status());
    kassert!(matches!(r, Some((Ok(_), n)) if n == ROUNDS * CHUNK as i64), "writer: {r:?} after {polls} STATUS calls (want Ok(xruns), {})", ROUNDS * CHUNK as i64);
    let xruns = r.map_or(0, |(x, _)| x.unwrap_or(0));
    klog::event!(Info, "ktest", "alsa.threads", polls = polls, xruns = xruns);
    Ok(())
}
