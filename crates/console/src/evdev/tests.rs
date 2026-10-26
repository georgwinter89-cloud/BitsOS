//! Host tests of the input-core part: filtering as Linux input.c, the held-key union, lock keys and LED
//! listeners, unplug releases, device numbers.

use super::*;
use alloc::string::ToString;
use alloc::vec;
use core::sync::atomic::AtomicUsize;

fn keyboard(sink: &'static EvdevSink, name: &str) -> Arc<InputDevice> {
    let mut caps = Caps::default();
    caps.keys.extend([1u16, 30, 42, KEY_CAPSLOCK, KEY_NUMLOCK, KEY_SCROLLLOCK]);
    caps.msc.insert(4);
    caps.leds.extend([LED_NUML, LED_CAPSL, LED_SCROLLL]);
    caps.repeat = true;
    register(sink, DeviceInfo { name: name.to_string(), bus: BUS_USB, caps, ..DeviceInfo::default() })
}

fn drain(sink: &EvdevSink) -> Vec<(u32, u16, u16, i32)> {
    let mut v = Vec::new();
    while let Some(e) = sink.pop() {
        assert!(!e.injected || e.device >= 0xFFFF_0000);
        v.push((e.device, e.kind, e.code, e.value));
    }
    v
}

#[test]
fn keys_filter_like_the_input_core() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard(&S, "kbd filter");
    let n = k.number;
    // Press A (with its scan code), press again (dropped), repeat (passes), unknown key (dropped), release.
    assert_eq!(k.emit(1, &[(EV_MSC, 4, 0x70004), (EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]), 3);
    assert_eq!(k.emit(2, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]), 0, "unchanged key and its empty frame");
    assert_eq!(k.emit(3, &[(EV_KEY, 30, 2), (EV_KEY, 99, 1), (EV_REL, 0, 5), (EV_SYN, SYN_REPORT, 0)]), 2);
    assert_eq!(k.emit(4, &[(EV_KEY, 30, 0), (EV_SYN, SYN_REPORT, 0)]), 2);
    assert_eq!(
        drain(&S),
        vec![
            (n, EV_MSC, 4, 0x70004),
            (n, EV_KEY, 30, 1),
            (n, EV_SYN, SYN_REPORT, 0),
            (n, EV_KEY, 30, 2),
            (n, EV_SYN, SYN_REPORT, 0),
            (n, EV_KEY, 30, 0),
            (n, EV_SYN, SYN_REPORT, 0),
        ]
    );
    k.unregister(5);
}

#[test]
fn pointer_moves_and_absolute_axes() {
    static S: EvdevSink = EvdevSink::new();
    let mut caps = Caps::default();
    caps.keys.insert(0x110);
    caps.rel.extend([0u16, 1, 8]);
    caps.abs.insert(0, AbsInfo { min: 0, max: 32767, ..AbsInfo::default() });
    caps.abs.insert(1, AbsInfo { min: 0, max: 32767, fuzz: 16, ..AbsInfo::default() });
    assert!(caps.is_pointer() && !caps.is_keyboard());
    let m = register(&S, DeviceInfo { name: "pointer".to_string(), caps, ..DeviceInfo::default() });
    let n = m.number;
    m.emit(0, &[(EV_REL, 0, 0), (EV_REL, 1, -3), (EV_SYN, SYN_REPORT, 0)]);
    m.emit(0, &[(EV_ABS, 0, 100), (EV_ABS, 1, 1000), (EV_SYN, SYN_REPORT, 0)]);
    // Y within half the fuzz of 1000: dropped; X unchanged: dropped; nothing else: no frame.
    m.emit(0, &[(EV_ABS, 0, 100), (EV_ABS, 1, 1005), (EV_SYN, SYN_REPORT, 0)]);
    // Y within the fuzz: smoothed (old * 3 + new) / 4.
    m.emit(0, &[(EV_ABS, 1, 1012), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(
        drain(&S),
        vec![
            (n, EV_REL, 1, -3),
            (n, EV_SYN, SYN_REPORT, 0),
            (n, EV_ABS, 0, 100),
            (n, EV_ABS, 1, 1000),
            (n, EV_SYN, SYN_REPORT, 0),
            (n, EV_ABS, 1, 1003),
            (n, EV_SYN, SYN_REPORT, 0),
        ]
    );
    assert_eq!(m.abs_value(1), Some(1003));
    m.unregister(0);
    assert_eq!(defuzz(10, 0, 0), 10);
    assert_eq!(defuzz(i32::MIN, i32::MAX, i32::MAX), i32::MIN, "no overflow");
}

#[test]
fn unplug_releases_held_keys_and_frees_the_number() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard(&S, "kbd unplug");
    let n = k.number;
    k.emit(0, &[(EV_KEY, 42, 1), (EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(k.held_keys(), vec![30, 42]);
    drain(&S);
    k.unregister(9);
    assert_eq!(drain(&S), vec![(n, EV_KEY, 30, 0), (n, EV_KEY, 42, 0), (n, EV_SYN, SYN_REPORT, 0)]);
    assert!(S.held_keys().is_empty(), "nothing stays held");
    assert_eq!(k.emit(10, &[(EV_KEY, 30, 1)]), 0, "a late report after the unplug passes nothing");
    assert!(drain(&S).is_empty());
    assert!(!devices().iter().any(|d| core::ptr::eq(&**d, &*k)));
}

#[test]
fn several_keyboards_hold_a_key_together() {
    static S: EvdevSink = EvdevSink::new();
    let (a, b) = (keyboard(&S, "kbd a"), keyboard(&S, "kbd b"));
    assert_ne!(a.number, b.number);
    a.emit(0, &[(EV_KEY, 42, 1), (EV_SYN, SYN_REPORT, 0)]);
    b.emit(0, &[(EV_KEY, 42, 1), (EV_SYN, SYN_REPORT, 0)]);
    a.emit(0, &[(EV_KEY, 42, 0), (EV_SYN, SYN_REPORT, 0)]);
    assert!(S.key_held(42) && S.key_held_physical(42), "b still holds shift");
    // A reader's own union sees one press and one release for the two keyboards.
    let mut u = KeyUnion::new();
    let edges: Vec<bool> = core::iter::from_fn(|| S.pop()).filter_map(|e| u.apply(&e)).collect();
    assert_eq!(edges, vec![true]);
    b.unregister(0);
    assert!(!S.key_held(42));
    let edges: Vec<bool> = core::iter::from_fn(|| S.pop()).filter_map(|e| u.apply(&e)).collect();
    assert_eq!(edges, vec![false]);
    a.unregister(0);
    // Injected keys count for the union, but not as physical.
    S.push(InputEvent { time_ns: 0, kind: EV_KEY, code: 29, value: 1, device: 0xFFFF_FF00, injected: true });
    assert!(S.key_held(29) && !S.key_held_physical(29));
    S.push(InputEvent { time_ns: 0, kind: EV_KEY, code: 29, value: 0, device: 0xFFFF_FF00, injected: true });
    assert!(!S.key_held(29));
    // An injected press under a real device's number is a holder of its own (contract.evdev_injected pushes
    // device 0): the real device's press still counts as physical, its release does not end the injected one.
    let k = keyboard(&S, "kbd shared number");
    S.push(InputEvent { time_ns: 0, kind: EV_KEY, code: 30, value: 1, device: k.number, injected: true });
    k.emit(0, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert!(S.key_held_physical(30));
    k.unregister(0);
    assert!(!S.key_held_physical(30) && S.key_held(30));
    S.push(InputEvent { time_ns: 0, kind: EV_KEY, code: 30, value: 0, device: k.number, injected: true });
    assert!(!S.key_held(30));
}

#[test]
fn lock_keys_toggle_once_per_union_press_and_listeners_hear_it() {
    static S: EvdevSink = EvdevSink::new();
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST: AtomicU32 = AtomicU32::new(0xFF);
    let id = S.add_led_listener(Arc::new(|bits| {
        CALLS.fetch_add(1, Ordering::SeqCst);
        LAST.store(bits, Ordering::SeqCst);
    }));
    let (a, b) = (keyboard(&S, "kbd caps a"), keyboard(&S, "kbd caps b"));
    a.emit(0, &[(EV_KEY, KEY_CAPSLOCK, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!((S.locks(), LAST.load(Ordering::SeqCst)), (1 << LED_CAPSL, 1 << LED_CAPSL));
    // b presses Caps Lock while a holds it: the union does not change, no toggle; a repeat neither.
    b.emit(0, &[(EV_KEY, KEY_CAPSLOCK, 1), (EV_KEY, KEY_CAPSLOCK, 2), (EV_SYN, SYN_REPORT, 0)]);
    a.emit(0, &[(EV_KEY, KEY_CAPSLOCK, 0), (EV_SYN, SYN_REPORT, 0)]);
    b.emit(0, &[(EV_KEY, KEY_CAPSLOCK, 0), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    a.emit(0, &[(EV_KEY, KEY_NUMLOCK, 1), (EV_KEY, KEY_CAPSLOCK, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(S.locks(), 1 << LED_NUML, "num on, caps off again");
    S.set_locks(1 << LED_SCROLLL | 0x100);
    assert_eq!(LAST.load(Ordering::SeqCst), 1 << LED_SCROLLL, "unknown bits ignored");
    let calls = CALLS.load(Ordering::SeqCst);
    S.set_locks(1 << LED_SCROLLL);
    assert_eq!(CALLS.load(Ordering::SeqCst), calls, "no change, no call");
    S.remove_led_listener(id);
    S.set_locks(0);
    assert_eq!(CALLS.load(Ordering::SeqCst), calls);
    a.unregister(0);
    b.unregister(0);
}

#[test]
fn recent_events_are_kept_for_observers() {
    static S: EvdevSink = EvdevSink::new();
    let start = S.next_seq();
    for i in 0..(RECENT as i32 + 10) {
        S.push(InputEvent { time_ns: 0, kind: EV_REL, code: 0, value: i, device: 7, injected: false });
    }
    let r = S.recent_since(start);
    assert_eq!(r.len(), RECENT, "a window, oldest dropped");
    assert_eq!(r[0].1.value, 10);
    assert_eq!(S.recent_since(S.next_seq() - 2).len(), 2);
    assert_eq!(S.pop().map(|e| e.value), Some(0), "the reader's ring is separate");
    let n = S.dropped();
    for _ in 0..MAX_PENDING {
        S.push(InputEvent { time_ns: 0, kind: EV_SYN, code: 0, value: 0, device: 7, injected: false });
    }
    assert!(S.dropped() > n, "a full ring drops the oldest and counts it");
}

#[test]
fn overflow_drops_oldest_and_asks_for_resync() {
    static S: EvdevSink = EvdevSink::new();
    assert!(S.pop().is_none(), "a reader of the sink's ring from here on");
    assert!(!S.take_resync());
    S.push(InputEvent { time_ns: 0, kind: EV_KEY, code: 30, value: 1, device: 3, injected: false });
    for i in 0..MAX_PENDING as i32 {
        S.push(InputEvent { time_ns: 0, kind: EV_REL, code: 0, value: i, device: 3, injected: false });
    }
    // The key-up is the one that matters: the press was dropped, the union still knows the key is held.
    S.push(InputEvent { time_ns: 0, kind: EV_KEY, code: 30, value: 0, device: 3, injected: false });
    assert_eq!(S.dropped(), 2);
    assert!(S.take_resync(), "resync raised");
    assert!(!S.take_resync(), "raised once");
    assert!(!S.key_held(30), "the union is exact although the reader lost events");
    let v = drain(&S);
    assert_eq!(v.len(), MAX_PENDING);
    assert_eq!(v.last(), Some(&(3, EV_KEY, 30, 0)));
}

#[test]
fn release_keys_keeps_the_device_open() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard(&S, "kbd pipe restart");
    let n = k.number;
    k.emit(0, &[(EV_KEY, 30, 1), (EV_KEY, 42, 1), (EV_SYN, SYN_REPORT, 0)]);
    drain(&S);
    assert_eq!(k.release_keys(1), 2);
    assert_eq!(drain(&S), vec![(n, EV_KEY, 30, 0), (n, EV_KEY, 42, 0), (n, EV_SYN, SYN_REPORT, 0)]);
    assert!(!k.is_gone() && S.held_keys().is_empty());
    // Reports that come after the restart pass again; the key pressed before counts as a new press.
    assert_eq!(k.emit(2, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]), 2);
    assert_eq!(k.release_all(3), 1);
    assert_eq!(k.emit(4, &[(EV_KEY, 42, 1), (EV_SYN, SYN_REPORT, 0)]), 0, "nothing passes after release_all");
    assert_eq!(k.release_keys(5), 0);
    k.unregister(6);
}

#[test]
fn listeners_get_the_state_at_call_time() {
    static S: EvdevSink = EvdevSink::new();
    static SEEN: AtomicU32 = AtomicU32::new(0xFF);
    // A listener that changes the state again (as a second CPU would between the change and the call): the next
    // listener call carries the newer state, never the one the first change computed.
    let id = S.add_led_listener(Arc::new(|bits| {
        SEEN.store(bits, Ordering::SeqCst);
    }));
    let k = keyboard(&S, "kbd call time");
    k.emit(0, &[(EV_KEY, KEY_CAPSLOCK, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(SEEN.load(Ordering::SeqCst), 1 << LED_CAPSL);
    S.locks.fetch_or(1 << LED_NUML, Ordering::SeqCst);
    S.set_locks(1 << LED_NUML | 1 << LED_CAPSL | 1 << LED_SCROLLL);
    assert_eq!(SEEN.load(Ordering::SeqCst), 7);
    S.remove_led_listener(id);
    k.unregister(0);
}

/// A /dev/input-like reader for the pacing tests: Linux evdev's 64-event ring for a keyboard (`Ring`).
struct RingReader {
    ring: SpinLock<crate::evdevfs::Ring>,
}

impl Listener for RingReader {
    fn push(&self, ev: InputEvent) {
        self.ring.lock().pass(ev);
    }
    fn device(&self) -> Option<u32> {
        Some(0xFFFF_FF00)
    }
    fn room(&self) -> usize {
        self.ring.lock().room()
    }
    fn lost(&self) -> u64 {
        self.ring.lock().losses().1
    }
}

/// `chars` key strokes as MCP builds them: press, SYN_REPORT, release, SYN_REPORT (4 events each).
fn strokes(chars: usize) -> Vec<InputEvent> {
    let mut v = Vec::new();
    for i in 0..chars {
        let code = 16 + (i % 10) as u16;
        for (kind, code, value) in [(EV_KEY, code, 1), (EV_SYN, SYN_REPORT, 0), (EV_KEY, code, 0), (EV_SYN, SYN_REPORT, 0)] {
            v.push(InputEvent { time_ns: 1, kind, code, value, device: 0xFFFF_FF00, injected: true });
        }
    }
    v
}

#[test]
fn an_unpaced_burst_overflows_a_keyboard_ring_by_one_chunk() {
    // The typeloss cause: 100 strokes pushed at once into a reader that reads nothing meanwhile.
    static S: EvdevSink = EvdevSink::new();
    let r = Arc::new(RingReader { ring: SpinLock::new(crate::evdevfs::Ring::new(64)) });
    let id = S.attach(r.clone());
    for e in strokes(100) {
        S.push(e);
    }
    let (overflows, lost) = r.ring.lock().losses();
    assert_eq!((overflows, lost), (6, 6 * 63), "each overflow throws 63 events (15-16 strokes) away");
    S.detach(id);
}

#[test]
fn paced_injection_never_overflows_a_slow_reader() {
    extern crate std;
    static S: EvdevSink = EvdevSink::new();
    let r = Arc::new(RingReader { ring: SpinLock::new(crate::evdevfs::Ring::new(64)) });
    let id = S.attach(r.clone());
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (r, done) = (r.clone(), done.clone());
        std::thread::spawn(move || {
            let mut got = Vec::new();
            loop {
                let last = done.load(Ordering::Acquire);
                // A few events per look, as a busy compositor between frames.
                for _ in 0..12 {
                    match r.ring.lock().fetch() {
                        Some(e) => got.push(e),
                        None => break,
                    }
                }
                S.reader_took();
                if last && !r.ring.lock().readable() {
                    return got;
                }
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        })
    };
    let sent = strokes(2500);
    assert_eq!(S.push_paced(&sent), 0, "no reader lost anything");
    done.store(true, Ordering::Release);
    let got = reader.join().unwrap();
    S.detach(id);
    assert_eq!(r.ring.lock().losses(), (0, 0));
    let keys = |v: &[InputEvent]| v.iter().filter(|e| e.kind == EV_KEY).map(|e| (e.code, e.value)).collect::<Vec<_>>();
    assert_eq!(keys(&got), keys(&sent), "every press and release of 2500 strokes arrived, in order");
}

#[test]
fn a_reader_that_never_reads_does_not_hold_the_call_forever() {
    static S: EvdevSink = EvdevSink::new();
    let r = Arc::new(RingReader { ring: SpinLock::new(crate::evdevfs::Ring::new(64)) });
    let id = S.attach(r.clone());
    let lost = S.push_paced(&strokes(50));
    assert!(lost > 0, "the stalled reader overflowed as on Linux, and the call says so");
    assert_eq!(S.readers_lost(0xFFFF_FF00), lost);
    S.detach(id);
}

#[test]
fn a_grabbed_device_counts_only_the_grabber() {
    static S: EvdevSink = EvdevSink::new();
    let grabber = Arc::new(RingReader { ring: SpinLock::new(crate::evdevfs::Ring::new(64)) });
    let other = Arc::new(RingReader { ring: SpinLock::new(crate::evdevfs::Ring::new(64)) });
    let g = S.attach(grabber.clone());
    let o = S.attach(other.clone());
    S.grab(0xFFFF_FF00, g).unwrap();
    for e in strokes(30) {
        S.push(e);
    }
    assert_eq!(other.ring.lock().room(), 63, "the other reader got nothing while the device is grabbed");
    assert_eq!(S.readers_room(0xFFFF_FF00), grabber.ring.lock().room());
    assert_eq!(S.readers_lost(0xFFFF_FF00), grabber.ring.lock().losses().1);
    S.detach(o);
    S.detach(g);
}

#[test]
fn the_sinks_own_ring_counts_no_drop_without_a_reader() {
    // Nothing pops the kernel's sink: its ring is full all the time and must not look like lost input.
    static S: EvdevSink = EvdevSink::new();
    for e in strokes(MAX_PENDING) {
        S.push(e);
    }
    assert_eq!(S.dropped(), 0);
    assert!(!S.take_resync());
}
