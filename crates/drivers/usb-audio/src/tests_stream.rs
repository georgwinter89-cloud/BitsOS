//! Host tests of the stream engine over a fake isochronous pipe that completes requests when the test says so.

use crate::stream::{self, Capture, Layout, Pipe, Playback};
use std::boxed::Box;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::vec;
use std::vec::Vec;
use usb_core::{Buffer, IsoDone, IsoRequest, IsoResult, UsbError, XferMem, XferResult};

struct Mem(Mutex<Vec<u8>>);

impl XferMem for Mem {
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn read(&self, off: usize, out: &mut [u8]) -> bool {
        let m = self.0.lock().unwrap();
        match m.get(off..off + out.len()) {
            Some(s) => {
                out.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&self, off: usize, data: &[u8]) -> bool {
        let mut m = self.0.lock().unwrap();
        match m.get_mut(off..off + data.len()) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn alloc(n: usize) -> Result<Buffer, UsbError> {
    Ok(Arc::new(Mem(Mutex::new(vec![0; n]))))
}

#[derive(Default)]
struct Shared {
    window: usize,
    mps: usize,
    queued: AtomicUsize,
    pending: Mutex<VecDeque<(IsoRequest, IsoDone)>>,
    sent: Mutex<Vec<u8>>,
    refuse: Mutex<Option<UsbError>>,
    requests: AtomicUsize,
}

#[derive(Clone)]
struct FakePipe(Arc<Shared>);

impl Pipe for FakePipe {
    fn room(&self) -> usize {
        self.0.window - self.0.queued.load(Ordering::Acquire)
    }
    fn queued(&self) -> usize {
        self.0.queued.load(Ordering::Acquire)
    }
    fn window(&self) -> usize {
        self.0.window
    }
    fn max_packet(&self) -> usize {
        self.0.mps
    }
    fn interval_ns(&self) -> u64 {
        1_000_000
    }
    fn queue(&self, req: IsoRequest, done: IsoDone) -> Result<(), UsbError> {
        if let Some(e) = *self.0.refuse.lock().unwrap() {
            return Err(e);
        }
        let n = req.packets.len();
        assert!(n <= self.room(), "the stream queued past the window");
        assert!(req.packets.iter().all(|p| p.len <= self.0.mps));
        self.0.queued.fetch_add(n, Ordering::AcqRel);
        self.0.requests.fetch_add(1, Ordering::AcqRel);
        self.0.pending.lock().unwrap().push_back((req, done));
        Ok(())
    }
}

impl FakePipe {
    fn new(window: usize, mps: usize) -> FakePipe {
        FakePipe(Arc::new(Shared { window, mps, ..Default::default() }))
    }
    /// Completes the oldest `n` requests: OUT data recorded; IN packets filled by `fill` (bytes per packet).
    fn complete(&self, n: usize, error: Option<UsbError>, fill: &dyn Fn(usize) -> Vec<u8>) {
        for _ in 0..n {
            let Some((req, done)) = self.0.pending.lock().unwrap().pop_front() else { return };
            let mut packets = Vec::new();
            for (k, p) in req.packets.iter().enumerate() {
                let mut b = vec![0; p.len];
                assert!(req.buf.read(p.offset, &mut b));
                self.0.sent.lock().unwrap().extend_from_slice(&b);
                let data = fill(k);
                assert!(req.buf.write(p.offset, &data));
                packets.push(XferResult::ok(if data.is_empty() { p.len } else { data.len() }));
            }
            self.0.queued.fetch_sub(req.packets.len(), Ordering::AcqRel);
            done(IsoResult { packets, error });
        }
    }
    /// Completes the oldest request with a result made by `f` (any packet count, sizes, errors), the buffer
    /// filled with `byte` first.
    fn complete_raw(&self, byte: u8, f: &dyn Fn(usize) -> IsoResult) {
        let Some((req, done)) = self.0.pending.lock().unwrap().pop_front() else { return };
        let len = req.buf.len();
        assert!(req.buf.write(0, &vec![byte; len]));
        self.0.queued.fetch_sub(req.packets.len(), Ordering::AcqRel);
        done(f(req.packets.len()));
    }
    fn pending(&self) -> usize {
        self.0.pending.lock().unwrap().len()
    }
}

fn none(_: usize) -> Vec<u8> {
    Vec::new()
}

fn sig(i: usize) -> [i16; 2] {
    [(i as i16).wrapping_mul(3), -(i as i16)]
}

fn le(frames: impl Iterator<Item = [i16; 2]>) -> Vec<u8> {
    frames.flat_map(|f| [f[0].to_le_bytes(), f[1].to_le_bytes()]).flatten().collect()
}

fn play(window: usize, ring: usize, req: usize, lay: Layout) -> (FakePipe, Playback, Arc<AtomicUsize>) {
    let p = FakePipe::new(window, 288);
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    let s = Playback::open(Box::new(p.clone()), &alloc, lay, ring, req, Arc::new(move || {
        w.fetch_add(1, Ordering::AcqRel);
    })).unwrap();
    (p, s, wakes)
}

const STEREO: Layout = Layout { channels: 2, subframe: 2 };

#[test]
fn packets_of_48_frames_in_order_and_the_position_counts_completions() {
    let (p, s, wakes) = play(20, 1920, 4, STEREO);
    let frames: Vec<[i16; 2]> = (0..1000).map(sig).collect();
    assert_eq!(s.write(&frames), Ok(1000));
    assert_eq!(p.pending(), 0, "nothing goes out before start");
    s.start().unwrap();
    assert_eq!((p.pending(), s.queued_packets()), (5, 20), "the window is full: 5 requests of 4 packets");
    assert_eq!(s.position(), Ok(0));
    p.complete(2, None, &none);
    assert_eq!(s.position(), Ok(2 * 4 * 48));
    assert_eq!(wakes.load(Ordering::Acquire), 2);
    // 1000 - 960 = 40 frames left: less than a packet, the bus still has 3 requests: wait.
    assert_eq!(p.pending(), 3);
    assert_eq!(s.write(&(1000..1400).map(sig).collect::<Vec<_>>()), Ok(400));
    assert_eq!(p.pending(), 5, "room for two requests again");
    p.complete(5, None, &none);
    let total = s.position().unwrap() as usize;
    assert_eq!(&p.0.sent.lock().unwrap()[..], &le((0..total).map(sig))[..], "every frame once, in order");
    assert_eq!(s.stats().missed, 0);
}

#[test]
fn a_dry_bus_takes_what_there_is() {
    let (p, s, _) = play(20, 1920, 4, STEREO);
    s.write(&(0..100).map(sig).collect::<Vec<_>>()).unwrap();
    s.start().unwrap();
    assert_eq!((p.pending(), s.queued_packets()), (1, 2), "two whole packets at once; 4 frames wait");
    p.complete(1, None, &none);
    assert_eq!(s.position(), Ok(96));
    assert!(s.stats().bus_empty >= 1);
}

#[test]
fn the_ring_takes_only_its_room() {
    let (_p, s, _) = play(4, 500, 4, STEREO);
    assert_eq!(s.ring_frames(), 500);
    assert_eq!(s.write(&vec![[1, 1]; 800]), Ok(500));
    assert_eq!(s.write(&[[1, 1]]), Ok(0));
}

#[test]
fn mono_downmix_and_24_bit() {
    let (p, s, _) = play(4, 480, 1, Layout { channels: 1, subframe: 2 });
    s.write(&vec![[1000, 3000]; 48]).unwrap();
    s.start().unwrap();
    p.complete(1, None, &none);
    let b = p.0.sent.lock().unwrap().clone();
    assert_eq!(b.len(), 96);
    assert_eq!(&b[..2], &2000i16.to_le_bytes());
    let (p, s, _) = play(4, 480, 1, Layout { channels: 2, subframe: 3 });
    s.write(&vec![[-2, 0x1234]; 48]).unwrap();
    s.start().unwrap();
    p.complete(1, None, &none);
    let b = p.0.sent.lock().unwrap().clone();
    assert_eq!(b.len(), 288, "48 frames of 6 bytes fill the JBL's 288-byte packet exactly");
    assert_eq!(&b[..6], &[0x00, 0xFE, 0xFF, 0x00, 0x34, 0x12]);
}

#[test]
fn unplug_ends_the_stream_as_gone() {
    let (p, s, wakes) = play(8, 1920, 4, STEREO);
    s.write(&vec![[5, 5]; 1000]).unwrap();
    s.start().unwrap();
    p.complete(2, Some(UsbError::Gone), &none);
    assert_eq!(s.position(), Err(UsbError::Gone));
    assert_eq!(s.write(&[[0, 0]]), Err(UsbError::Gone));
    assert_eq!(p.pending(), 0, "nothing queued after gone");
    assert!(wakes.load(Ordering::Acquire) >= 1, "the mixer is woken to notice");
}

#[test]
fn close_lets_the_queue_play_out_without_queueing_more() {
    let (p, s, _) = play(8, 1920, 4, STEREO);
    s.write(&vec![[5, 5]; 1500]).unwrap();
    s.start().unwrap();
    assert_eq!(p.pending(), 2);
    s.close();
    p.complete(2, None, &none);
    assert_eq!(p.pending(), 0);
    assert_eq!(p.0.requests.load(Ordering::Acquire), 2);
}

#[test]
fn a_refused_request_loses_frames_not_time() {
    let (p, s, _) = play(8, 1920, 4, STEREO);
    *p.0.refuse.lock().unwrap() = Some(UsbError::Transaction);
    s.write(&vec![[5, 5]; 192]).unwrap();
    s.start().unwrap();
    assert_eq!(s.position(), Ok(192));
    assert_eq!(s.stats().errors, 1);
    *p.0.refuse.lock().unwrap() = Some(UsbError::Gone);
    s.write(&vec![[5, 5]; 192]).unwrap();
    assert_eq!(s.position(), Err(UsbError::Gone));
}

#[test]
fn a_packet_larger_than_the_endpoint_is_refused_at_open() {
    let p = FakePipe::new(8, 144);
    let r = Playback::open(Box::new(p), &alloc, STEREO, 960, 4, Arc::new(|| {}));
    assert!(matches!(r, Err(UsbError::Invalid(_))));
}

#[test]
fn capture_decodes_short_packets_and_drops_the_oldest_when_full() {
    let p = FakePipe::new(8, 144);
    let c = Capture::open(Box::new(p.clone()), &alloc, Layout { channels: 1, subframe: 2 }, 100, 1, None).unwrap();
    c.start().unwrap();
    assert_eq!(p.pending(), 8, "the window is kept full of IN requests");
    // The j-th packet carries 48 samples of value j + 1 (96 bytes), except the second: 10 samples (short).
    let count = AtomicUsize::new(0);
    let fill = |_: usize| -> Vec<u8> {
        let j = count.fetch_add(1, Ordering::AcqRel);
        let n = if j == 1 { 10 } else { 48 };
        (0..n).flat_map(|_| (j as i16 + 1).to_le_bytes()).collect()
    };
    p.complete(4, None, &fill);
    assert_eq!(c.counts(), (48 + 10 + 48 + 48, 154 - 100));
    assert_eq!(c.available(), 100);
    assert_eq!(p.pending(), 8, "refilled");
    let mut out = vec![[0i16; 2]; 200];
    assert_eq!(c.read(&mut out), Ok(100));
    // The newest 100: 4 of packet 2 (value 2), packets 3 and 4 whole; mono on both channels.
    assert_eq!((out[0], out[3], out[4], out[51], out[52], out[99]), ([2, 2], [2, 2], [3, 3], [3, 3], [4, 4], [4, 4]));
    p.complete(1, Some(UsbError::Gone), &fill);
    let _ = c.read(&mut out);
    assert_eq!(c.read(&mut out), Err(UsbError::Gone));
}

#[test]
fn hostile_in_completions_never_panic_or_read_past_the_buffer() {
    // Requests of 4 packets of 144 bytes, mono 16 bit: a buffer of 576 bytes per request.
    let p = FakePipe::new(8, 144);
    let c = Capture::open(Box::new(p.clone()), &alloc, Layout { channels: 1, subframe: 2 }, 1000, 4, None).unwrap();
    c.start().unwrap();
    assert_eq!(p.pending(), 2);
    let err = |e| XferResult { actual: 96, error: Some(e) };
    // A failed packet, an empty one, one claiming far more than the endpoint's size, an odd byte count, and two
    // packets more than the request had (past the buffer); the request itself reports a transaction error.
    p.complete_raw(1, &|_| IsoResult {
        packets: vec![err(UsbError::Transaction), XferResult::ok(0), XferResult::ok(usize::MAX), XferResult::ok(3), XferResult::ok(96),
            XferResult::ok(96)],
        error: Some(UsbError::Transaction),
    });
    assert_eq!(c.counts(), (72 + 1, 0), "only the oversized packet (clamped to 144 bytes) and one whole sample of the odd one");
    assert_eq!(c.stats().errors, 1);
    assert_eq!(c.stats().missed, 1);
    // No packets at all, and fewer packets than asked: nothing taken, the stream goes on.
    p.complete_raw(2, &|_| IsoResult { packets: Vec::new(), error: None });
    p.complete_raw(3, &|_| IsoResult { packets: vec![XferResult::ok(4)], error: None });
    assert_eq!(c.counts(), (75, 0));
    assert_eq!(p.pending(), 2, "refilled after every completion: a bad result does not stop the capture");
    // Cancelled is the end of a request at close, not an error.
    p.complete_raw(4, &|n| IsoResult { packets: vec![XferResult { actual: 0, error: Some(UsbError::Cancelled) }; n], error: Some(UsbError::Cancelled) });
    assert_eq!(c.stats().errors, 1);
    let mut out = vec![[0i16; 2]; 100];
    assert_eq!(c.read(&mut out), Ok(75));
    assert_eq!((out[0], out[72], out[73], out[74]), ([0x0101, 0x0101], [0x0101, 0x0101], [0x0303, 0x0303], [0x0303, 0x0303]));
    c.close();
}

#[test]
fn packet_sizes_and_windows() {
    assert_eq!(stream::frames_per_packet(1_000_000), Some(48));
    assert_eq!(stream::frames_per_packet(125_000), Some(6));
    assert_eq!(stream::frames_per_packet(8_000_000), Some(384));
    assert_eq!(stream::frames_per_packet(100_000), None);
    assert_eq!(stream::frames_per_packet(0), None);
    // The default mixer period (480 frames = 10 ms): 20 ms on the bus in requests of 4 ms.
    assert_eq!(stream::sizing(480, 1_000_000), (20_000_000, 4));
    assert_eq!(stream::sizing(60, 1_000_000), (2_500_000, 1));
    assert_eq!(stream::sizing(0, 125_000), (2_000_000, 3));
}

#[test]
fn concurrent_pumps_never_overfill_the_window() {
    let (p, s, _) = play(12, 4800, 2, STEREO);
    let s = Arc::new(s);
    s.start().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, st2) = (s.clone(), stop.clone());
    let writer = std::thread::spawn(move || {
        let mut i = 0usize;
        while !st2.load(Ordering::Acquire) {
            let f: Vec<[i16; 2]> = (i..i + 100).map(sig).collect();
            i += s2.write(&f).unwrap();
        }
        i
    });
    for _ in 0..2000 {
        p.complete(1, None, &none);
    }
    stop.store(true, Ordering::Release);
    let _ = writer.join().unwrap();
    p.complete(usize::MAX, None, &none);
    let played = s.position().unwrap() as usize;
    assert_eq!(&p.0.sent.lock().unwrap()[..played * 4], &le((0..played).map(sig))[..]);
}
